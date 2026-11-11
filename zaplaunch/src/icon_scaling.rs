use slint::{Image, Rgba8Pixel, SharedPixelBuffer};
use std::cell::RefCell;

const CACHE_LIMIT: usize = 128;

thread_local! {
    static CACHE: RefCell<Vec<(Image, u32, Image)>> = const { RefCell::new(Vec::new()) };
    // Logo 固定为 PNG，复用已有解码器，避免通用加载入口保留额外格式代码。
    static LOGO: Image = {
        let bytes: &[u8] = include_bytes!("../../../ZapLaunch-256.png");
        let decoder = image::codecs::png::PngDecoder::new(std::io::Cursor::new(bytes)).expect("无法读取 Logo PNG");
        let rgba = image::DynamicImage::from_decoder(decoder).expect("无法解码 Logo PNG").to_rgba8();
        Image::from_rgba8(SharedPixelBuffer::clone_from_slice(rgba.as_raw(), rgba.width(), rgba.height()))
    };
    static TITLE_IMAGES: Vec<Image> = zaplaunch_sys::app_icon_images().unwrap_or_default()
        .into_iter().filter_map(|bytes| {
            let decoder = image::codecs::ico::IcoDecoder::new(std::io::Cursor::new(&bytes)).ok()?;
            let image = image::DynamicImage::from_decoder(decoder).ok()?.to_rgba8();
            Some(Image::from_rgba8(SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(image.as_raw(), image.width(), image.height())))
        }).collect();
}

pub fn bind(renderer: zaplaunch_ui::IconRenderer<'_>) {
    renderer.on_scaled(scaled);
    renderer.on_title(title);
    renderer.on_logo(|| LOGO.with(Clone::clone));
}

pub fn clear_cache() { CACHE.with(|cache| cache.borrow_mut().clear()); }

fn title_source(images: &[Image], edge: u32) -> Option<&Image> {
    images.iter().filter(|image| image.size().width >= edge)
        .min_by_key(|image| image.size().width)
        .or_else(|| images.iter().max_by_key(|image| image.size().width))
}

fn title(physical_edge: i32) -> Image {
    let source = TITLE_IMAGES.with(|images| title_source(images, physical_edge.max(1) as u32).cloned())
        .unwrap_or_else(|| LOGO.with(Clone::clone));
    scaled(source, physical_edge)
}

pub fn scaled(source: Image, physical_edge: i32) -> Image {
    let size = source.size();
    if physical_edge <= 0 || size.width == 0 || size.height == 0 {
        return source;
    }
    let edge = (physical_edge as u32).min(512);
    let longest = size.width.max(size.height);
    if longest == edge {
        return source;
    }
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(index) = cache.iter().rposition(|(image, px, _)| *image == source && *px == edge) {
            // 命中移到最近端，避免频繁使用的图标被随后加载的冷图标淘汰。
            cache[index..].rotate_left(1);
            return cache.last().unwrap().2.clone();
        }
        let Some(pixels) = source.to_rgba8_premultiplied() else { return source; };
        let width = ((size.width as f64 * edge as f64 / longest as f64).round() as u32).max(1);
        let height = ((size.height as f64 * edge as f64 / longest as f64).round() as u32).max(1);
        let input = image::RgbaImage::from_raw(size.width, size.height, pixels.as_bytes().to_vec())
            .expect("Slint image buffer dimensions");
        // 先预乘透明度再滤波，避免透明像素里的杂色渗入图标边缘。
        let mut output = image::imageops::resize(&input, width, height, image::imageops::FilterType::Lanczos3);
        for pixel in output.pixels_mut() {
            for channel in 0..3 {
                pixel[channel] = pixel[channel].min(pixel[3]);
            }
        }
        let buffer = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(output.as_raw(), width, height);
        let result = Image::from_rgba8_premultiplied(buffer);
        if cache.len() >= CACHE_LIMIT {
            cache.remove(0);
        }
        cache.push((source, edge, result.clone()));
        result
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logo_png_decoder_preserves_every_pixel() {
        let original = Image::load_from_data(include_bytes!("../../../ZapLaunch-256.png"),Some("png")).unwrap();
        LOGO.with(|source| {
            let actual = source.to_rgba8_premultiplied().unwrap();
            let expected = original.to_rgba8_premultiplied().unwrap();
            assert!(actual.as_slice() == expected.as_slice(), "displayed premultiplied Logo pixels must remain identical");
        });
    }

    #[test]
    fn title_uses_nearest_larger_ico_frame() {
        let images: Vec<_> = [16, 24, 32, 48, 256].into_iter().map(|size|
            Image::from_rgba8(SharedPixelBuffer::<Rgba8Pixel>::new(size, size))).collect();
        for (requested, expected) in [(24, 24), (30, 32), (36, 48), (48, 48), (60, 256), (512, 256)] {
            assert_eq!(title_source(&images, requested).unwrap().size().width, expected);
        }
        assert!(title_source(&[], 24).is_none());
    }

    #[test]
    fn about_logo_downscales_from_256_for_common_dpi() {
        LOGO.with(|source| {
            assert_eq!((source.size().width, source.size().height), (256, 256));
            for edge in [64, 80, 96, 128, 160, 256] {
                let result = scaled(source.clone(), edge);
                assert_eq!((result.size().width, result.size().height), (edge as u32, edge as u32));
                assert!(result.to_rgba8().unwrap().as_slice().iter().any(|p| p.a == 255));
            }
        });
    }

    #[test]
    fn fractional_dpi_interpolates_without_transparent_color_fringes() {
        let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(48, 48);
        for (index, pixel) in buffer.make_mut_slice().iter_mut().enumerate() {
            *pixel = if index % 48 < 24 { Rgba8Pixel { r: 255, g: 0, b: 0, a: 255 } }
                else { Rgba8Pixel { r: 0, g: 255, b: 0, a: 0 } };
        }
        let source = Image::from_rgba8(buffer);
        assert_eq!(scaled(source.clone(), 48), source);
        for edge in [60, 72, 96] {
            let result = scaled(source.clone(), edge);
            assert_eq!((result.size().width, result.size().height), (edge as u32, edge as u32));
            assert_eq!(scaled(source.clone(), edge), result);
            let pixels = result.to_rgba8().unwrap();
            assert!(pixels.as_slice().iter().any(|p| p.a > 0 && p.a < 255));
            assert!(pixels.as_slice().iter().all(|p| p.g == 0 && p.b == 0));
        }
    }

    #[test]
    fn frequently_used_scaled_icon_survives_500_cold_icons() {
        CACHE.with(|cache| cache.borrow_mut().clear());
        let source = Image::from_rgba8(SharedPixelBuffer::<Rgba8Pixel>::new(16, 16));
        let hot = scaled(source.clone(), 20);
        for index in 0..500 {
            let cold = Image::from_rgba8(SharedPixelBuffer::<Rgba8Pixel>::new(16, 16));
            scaled(cold, 20);
            assert!(scaled(source.clone(), 20) == hot, "hot icon was recomputed after cold icon {index}");
        }
        CACHE.with(|cache| assert_eq!(cache.borrow().len(), CACHE_LIMIT));
        assert!(scaled(source, 24) != hot, "DPI sizes must remain separate");
    }

    #[test]
    fn scaling_preserves_aspect_ratio_and_bounds_cache() {
        let source = Image::from_rgba8(SharedPixelBuffer::<Rgba8Pixel>::new(4, 2));
        let result = scaled(source.clone(), 60);
        assert_eq!((result.size().width, result.size().height), (60, 30));
        for edge in 1..=140 {
            scaled(source.clone(), edge);
        }
        CACHE.with(|cache| assert_eq!(cache.borrow().len(), CACHE_LIMIT));
    }

    #[test]
    fn selection_frame_has_equal_physical_pixel_margins_in_every_column() {
        use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
        use slint::platform::{Platform, PlatformError, WindowAdapter, WindowEvent};
        use slint::{ComponentHandle, ModelRc, VecModel};
        use std::rc::Rc;
        struct TestPlatform(Rc<MinimalSoftwareWindow>);
        impl Platform for TestPlatform {
            fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> { Ok(self.0.clone()) }
        }
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        slint::platform::set_platform(Box::new(TestPlatform(window.clone()))).unwrap();
        let panel = zaplaunch_ui::MainPanel::new().unwrap();
        bind(panel.global::<zaplaunch_ui::IconRenderer>());
        let source = Image::from_rgba8(SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&[240, 20, 30, 255].repeat(48 * 48), 48, 48));
        panel.set_keyboard_navigation(true);
        panel.show().unwrap();
        for scale in [1.0_f32, 1.25, 1.5] {
            panel.window().dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor: scale });
            let (width,height) = ((520.0 * scale).round() as u32, (342.0 * scale).round() as u32);
            window.set_size(slint::PhysicalSize::new(width,height));
            for dark in [false, true] {
                panel.global::<zaplaunch_ui::Palette>().set_dark(dark);
                let accent: [i32;3] = if dark { [147,174,201] } else { [27,59,111] };
                for cols in 6..=8 {
                    let tile_width = (496.0 - (cols - 1) as f32 * 2.0) / cols as f32;
                    panel.set_tile_columns(cols); panel.set_tile_width(tile_width);
                    panel.set_rows(ModelRc::new(VecModel::from(vec![ModelRc::new(VecModel::from((0..cols).map(|_| zaplaunch_ui::Tile {
                        sel: true, has_native_icon: true, native_icon: source.clone(), ..Default::default()
                    }).collect::<Vec<_>>()))])));
                    let mut pixels = vec![slint::Rgb8Pixel::default(); (width * height) as usize];
                    panel.window().request_redraw();
                    assert!(window.draw_if_needed(|r| { r.render(&mut pixels,width as usize); }));
                    let pixel = |x:usize,y:usize| pixels[y * width as usize + x];
                    let is_icon = |p:slint::Rgb8Pixel| p.r > 220 && p.g < 50 && p.b < 60;
                    let is_frame = |p:slint::Rgb8Pixel| [p.r,p.g,p.b].iter().zip(accent).all(|(c,a)| (*c as i32-a).abs() < 60);
                    for col in 0..cols {
                        let start = ((12.0 + col as f32 * (tile_width + 2.0)) * scale).floor() as usize;
                        let end = ((12.0 + col as f32 * (tile_width + 2.0) + tile_width) * scale).ceil() as usize;
                        let y = (157.0 * scale).round() as usize;
                        let icon:Vec<_> = (start..end).filter(|x| is_icon(pixel(*x,y))).collect();
                        let (left,right) = (*icon.first().unwrap(),*icon.last().unwrap());
                        let frame_left = (start..left).rev().find(|x| is_frame(pixel(*x,y))).unwrap();
                        let frame_right = (right+1..end).find(|x| is_frame(pixel(*x,y))).unwrap();
                        assert_eq!(left-frame_left-1,frame_right-right-1,"horizontal DPI={scale} dark={dark} cols={cols} col={col}, icon={left}..{right}, frame={frame_left}..{frame_right}");
                        let x = (left + right) / 2;
                        let top = (128.0 * scale) as usize;
                        let bottom = (210.0 * scale).min(height as f32) as usize;
                        let icon:Vec<_> = (top..bottom).filter(|y| is_icon(pixel(x,*y))).collect();
                        let (first,last) = (*icon.first().unwrap(),*icon.last().unwrap());
                        let frame_top = (top..first).rev().find(|y| is_frame(pixel(x,*y))).unwrap();
                        let frame_bottom = (last+1..bottom).find(|y| is_frame(pixel(x,*y))).unwrap();
                        assert_eq!(first-frame_top-1,frame_bottom-last-1,"vertical DPI={scale} dark={dark} cols={cols} col={col}");
                        let inset = (3.0 * scale).round() as usize + (2.0 * scale).round() as usize;
                        let (x0,x1,y0,y1) = (left-inset,right+inset,first-inset,last+inset);
                        for y in y0..=y1 {
                            for x in x0..=x1 {
                                let a = pixel(x,y);
                                for b in [pixel(x0+x1-x,y),pixel(x,y0+y1-y)] {
                                    // 圆角抗锯齿的颜色量化允许相差1级；几何留白必须精确相等。
                                    assert!([a.r,a.g,a.b].iter().zip([b.r,b.g,b.b]).all(|(a,b)| (*a as i32-b as i32).abs() <= 1),
                                        "frame reflection DPI={scale} dark={dark} cols={cols} col={col} at {x},{y}");
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn about_logo_recalculates_pixels_when_window_dpi_changes() {
        use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
        use slint::platform::{Platform, PlatformError, WindowAdapter, WindowEvent};
        use slint::ComponentHandle;
        use std::rc::Rc;

        struct TestPlatform(Rc<MinimalSoftwareWindow>);
        impl Platform for TestPlatform {
            fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
                Ok(self.0.clone())
            }
        }
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        slint::platform::set_platform(Box::new(TestPlatform(window.clone()))).unwrap();
        let preview = zaplaunch_ui::AboutWindow::new().unwrap();
        bind(preview.global::<zaplaunch_ui::IconRenderer>());
        let observed = Rc::new(RefCell::new(Vec::new()));
        let calls = observed.clone();
        preview.global::<zaplaunch_ui::IconRenderer>().on_scaled(move |image, edge| {
            calls.borrow_mut().push(edge);
            scaled(image, edge)
        });
        preview.show().unwrap();
        for scale in [1.0, 1.25, 1.5, 2.0, 1.0] {
            observed.borrow_mut().clear();
            preview.window().dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor: scale });
            let width = (380.0 * scale) as u32;
            let height = (264.0 * scale) as u32;
            window.set_size(slint::PhysicalSize::new(width, height));
            let mut pixels = vec![slint::Rgb8Pixel::default(); (width * height) as usize];
            preview.window().request_redraw();
            window.draw_if_needed(|renderer| { renderer.render(&mut pixels, width as usize); });
            let expected = (96.0 * scale).round() as i32;
            assert!(observed.borrow().contains(&expected), "DPI {scale} must request {expected}px, got {:?}", observed.borrow());
        }
    }
}
