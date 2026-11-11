use std::{io::Cursor, path::Path, time::Duration};
use url::Url;

pub fn website(target: &str) -> Option<Url> {
    let target = target.trim();
    let expanded = if target.get(..4).map(|s| s.eq_ignore_ascii_case("www.")).unwrap_or(false) {
        format!("https://{target}")
    } else { target.to_owned() };
    let url = Url::parse(&expanded).ok()?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none()
        || !url.username().is_empty() || url.password().is_some() { return None; }
    Some(url)
}

pub fn fallback(target: &str, label: &str, icon: &str) -> (String, (u8, u8, u8)) {
    let Some(url) = website(target).filter(|_| icon.trim().is_empty()) else { return (String::new(), (75, 85, 99)); };
    let host = url.host_str().unwrap_or_default().trim_start_matches("www.");
    let letter = label.trim().chars().find(|c| c.is_alphanumeric())
        .or_else(|| host.chars().find(|c| c.is_alphanumeric())).unwrap_or('?');
    const COLORS: [(u8,u8,u8); 6] = [(59,99,217), (15,127,119), (109,69,201), (196,83,26), (14,124,156), (196,60,99)];
    let hash = host.bytes().fold(0usize, |h,b| h.wrapping_mul(31).wrapping_add(b as usize));
    (letter.to_uppercase().collect(), COLORS[hash % COLORS.len()])
}

pub fn decode(bytes: &[u8]) -> super::IconPixels {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(512);
    limits.max_image_height = Some(512);
    limits.max_alloc = Some(8 * 1024 * 1024);
    // 只构造支持的解码器，避免通用入口保留其他格式的解码分支。
    let mut decoder: Box<dyn image::ImageDecoder + '_> = match image::guess_format(bytes).ok()? {
        image::ImageFormat::Png => Box::new(image::codecs::png::PngDecoder::with_limits(Cursor::new(bytes),limits.clone()).ok()?),
        image::ImageFormat::Ico => Box::new(image::codecs::ico::IcoDecoder::new(Cursor::new(bytes)).ok()?),
        _ => return None,
    };
    decoder.set_limits(limits).ok()?;
    let image = image::DynamicImage::from_decoder(decoder).ok()?.to_rgba8();
    Some((image.width(), image.height(), image.into_raw()))
}

pub fn load_file(path: &Path) -> super::IconPixels {
    use std::io::Read;
    let file = std::fs::File::open(path).ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() || meta.len() > 1024 * 1024 { return None; }
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    file.take(1024 * 1024 + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() > 1024 * 1024 { return None; }
    decode(&bytes)
}

/// Cache by origin, not page/query: pages on the same site share one icon.
pub fn load(origin: &str, cache: &Path, force: bool) -> super::IconPixels {
    let url = website(origin)?;
    let file = cache.join(cache_file_name(origin));
    let cached = load_file(&file);
    if let Ok(meta) = file.metadata() {
        let ttl = if meta.len() == 0 { 3600 } else { 7 * 86400 };
        if !force && (meta.len() == 0 || cached.is_some()) && meta.modified().ok().and_then(|t| t.elapsed().ok()).map(|d| d < Duration::from_secs(ttl)).unwrap_or(false) {
            return cached;
        }
    }
    // shortcut: use the conventional /favicon.ico (ICO or PNG); add HTML link discovery if needed.
    let bytes = zaplaunch_sys::web::download(url.host_str()?, url.port_or_known_default()?, "/favicon.ico", url.scheme() == "https");
    let pixels = bytes.as_deref().and_then(decode);
    if (pixels.is_some() || cached.is_none()) && std::fs::create_dir_all(cache).is_ok() {
        // 只替换完整的新缓存，下载或写入失败不破坏已有图标。
        let temp = file.with_extension("tmp");
        if std::fs::write(&temp, if pixels.is_some() { bytes.as_deref().unwrap_or_default() } else { &[] }).is_ok() {
            let _ = std::fs::rename(&temp, &file);
        }
        let _ = std::fs::remove_file(temp);
        prune(cache, None);
    }
    // 强制刷新失败仍报告失败，调用方保留原图；普通加载可继续使用过期图标。
    if force { pixels } else { pixels.or(cached) }
}

/// 只回收本程序的普通缓存文件，不递归、不跟随符号链接。
pub fn prune(cache: &Path, keep: Option<&std::collections::HashSet<String>>) -> usize {
    const MAX_FILES: usize = super::PATH_CACHE_LIMIT;
    const MAX_BYTES: u64 = 64 * 1024 * 1024;
    let entries = match std::fs::read_dir(cache) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return 0,
        Err(_) => return 1,
    };
    let mut failed = 0;
    let mut files = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else { failed += 1; continue; };
        let path = entry.path();
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
        if path.extension().and_then(|s| s.to_str()) != Some("icon") || stem.len() != 16
            || !stem.bytes().all(|b| b.is_ascii_hexdigit()) { continue; }
        match entry.file_type() {
            Ok(kind) if kind.is_file() => {},
            Ok(_) => continue,
            Err(_) => { failed += 1; continue; },
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if keep.map(|keep| !keep.contains(&name)).unwrap_or(false) {
            if std::fs::remove_file(&path).is_err() { failed += 1; }
            continue;
        }
        match entry.metadata().and_then(|m| m.modified().map(|t| (t,m.len()))) {
            Ok((time,len)) => files.push((time,len,path)),
            Err(_) => failed += 1,
        }
    }
    files.sort_unstable_by_key(|(time,_,_)| *time);
    let mut count = files.len();
    let mut bytes: u64 = files.iter().map(|(_,len,_)| *len).sum();
    for (_,len,path) in files {
        if count <= MAX_FILES && bytes <= MAX_BYTES { break; }
        if std::fs::remove_file(path).is_ok() { count -= 1; bytes -= len; }
        else { failed += 1; }
    }
    failed
}

pub fn cache_file_name(origin: &str) -> String {
    let hash = origin.bytes().fold(0xcbf29ce484222325u64, |h,b| (h ^ b as u64).wrapping_mul(0x100000001b3));
    format!("{hash:016x}.icon")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn expired_cache_survives_http_and_decode_failures_and_recovers() {
        use std::{io::{Read,Write}, net::TcpListener};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://127.0.0.1:{}",listener.local_addr().unwrap().port());
        let dir = std::env::temp_dir().join(format!("web-stale-{}",std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(cache_file_name(&origin));
        let mut png = Cursor::new(Vec::new());
        image::RgbaImage::new(16,16).write_to(&mut png,image::ImageFormat::Png).unwrap();
        let original = png.into_inner();
        std::fs::write(&file,&original).unwrap();
        let old = std::time::SystemTime::now() - Duration::from_secs(8 * 86400);
        std::fs::File::options().write(true).open(&file).unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(old)).unwrap();
        let mut png = Cursor::new(Vec::new());
        image::RgbaImage::new(32,32).write_to(&mut png,image::ImageFormat::Png).unwrap();
        let updated = png.into_inner();
        let body = updated.clone();
        let server = std::thread::spawn(move || {
            for (status,body) in [("404 Not Found",Vec::new()),("200 OK",b"<html>broken</html>".to_vec()),("200 OK",body.clone()),("200 OK",body)] {
                let (mut stream,_) = listener.accept().unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut request = [0;4096]; stream.read(&mut request).unwrap();
                write!(stream,"HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).unwrap();
                stream.write_all(&body).unwrap();
            }
        });
        for _ in 0..2 {
            assert_eq!(load(&origin,&dir,false).map(|(w,_,_)|w),Some(16));
            assert_eq!(std::fs::read(&file).unwrap(),original);
        }
        assert_eq!(load(&origin,&dir,false).map(|(w,_,_)|w),Some(32));
        assert_eq!(std::fs::read(&file).unwrap(),updated);
        std::fs::write(&file,b"corrupt but fresh").unwrap();
        assert_eq!(load(&origin,&dir,false).map(|(w,_,_)|w),Some(32));
        server.join().unwrap();
        assert!(!file.with_extension("tmp").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cache_pruning_removes_orphans_and_bounds_count_and_bytes() {
        use std::{fs, collections::HashSet};
        let dir = std::env::temp_dir().join(format!("web-prune-{}",std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let keep = "0000000000000001.icon".to_string();
        let orphan = dir.join("0000000000000002.icon");
        fs::write(dir.join(&keep),b"keep").unwrap(); fs::write(&orphan,b"old").unwrap();
        let note = dir.join("notes.icon"); fs::write(&note,b"user data").unwrap();
        let folder = dir.join("ffffffffffffffff.icon"); fs::create_dir(&folder).unwrap();
        assert_eq!(prune(&dir,Some(&HashSet::from([keep.clone()]))),0);
        assert!(!orphan.exists() && dir.join(&keep).exists() && note.exists() && folder.is_dir());
        fs::remove_file(dir.join(&keep)).unwrap();
        for i in 0..513 { fs::write(dir.join(format!("{i:016x}.icon")),[]).unwrap(); }
        assert_eq!(prune(&dir,None),0);
        let owned = || fs::read_dir(&dir).unwrap().flatten().filter(|e| e.file_type().unwrap().is_file()
            && e.file_name().to_string_lossy().len() == 21).map(|e|e.path()).collect::<Vec<_>>();
        assert_eq!(owned().len(),512);
        for file in owned() { fs::remove_file(file).unwrap(); }
        let old = std::time::SystemTime::now() - Duration::from_secs(1000);
        for i in 0..65 {
            let file = fs::File::create(dir.join(format!("{i:016x}.icon"))).unwrap();
            file.set_len(1024 * 1024).unwrap();
            file.set_times(fs::FileTimes::new().set_modified(old + Duration::from_secs(i))).unwrap();
        }
        assert_eq!(prune(&dir,None),0);
        assert_eq!(owned().len(),64);
        assert!(!dir.join("0000000000000000.icon").exists(),"oldest file must be evicted first");
        assert_eq!(prune(&dir,Some(&HashSet::new())),0);
        assert!(owned().is_empty() && note.exists() && folder.exists());
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(prune(&dir,None),0);
        fs::write(&dir,b"blocked").unwrap();
        assert_eq!(prune(&dir,None),1);
        fs::remove_file(dir).unwrap();
    }

    #[test]
    fn force_refresh_bypasses_cache_and_retains_good_bytes_on_failure() {
        use std::{io::{Read, Write}, net::TcpListener};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let hash = origin.bytes().fold(0xcbf29ce484222325u64, |h,b| (h ^ b as u64).wrapping_mul(0x100000001b3));
        let dir = std::env::temp_dir().join(format!("web-refresh-{}",std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let png = |edge| {
            let mut bytes = Cursor::new(Vec::new());
            image::RgbaImage::new(edge,edge).write_to(&mut bytes,image::ImageFormat::Png).unwrap(); bytes.into_inner()
        };
        let file = dir.join(format!("{hash:016x}.icon"));
        std::fs::write(&file,png(16)).unwrap();
        assert_eq!(load(&origin,&dir,false).map(|(w,_,_)| w),Some(16));
        let body = png(32);
        let server = std::thread::spawn(move || {
            for (status,body) in [("200 OK",body),("404 Not Found",Vec::new())] {
                let (mut stream,_) = listener.accept().unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut request = [0;4096]; let _ = stream.read(&mut request).unwrap();
                write!(stream,"HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).unwrap();
                stream.write_all(&body).unwrap();
            }
        });
        assert_eq!(load(&origin,&dir,true).map(|(w,_,_)| w),Some(32));
        assert!(load(&origin,&dir,true).is_none());
        assert_eq!(load(&origin,&dir,false).map(|(w,_,_)| w),Some(32));
        server.join().unwrap();
        std::fs::remove_file(file).unwrap(); std::fs::remove_dir(dir).unwrap();
    }
    #[test]
    fn websites_and_fallback_are_predictable() {
        assert_eq!(website(" www.hxlive.cn/path?q=1 ").unwrap().origin().ascii_serialization(), "https://www.hxlive.cn");
        assert!(website("HTTPS://EXAMPLE.COM/a").is_some());
        for path in ["C:\\foo.exe", "shell:AppsFolder", "calc.exe", "ftp://example.com", "https://user:pass@example.com"] { assert!(website(path).is_none()); }
        assert_eq!(fallback("www.hxlive.cn", "寒星", "").0, "寒");
        assert_eq!(fallback("https://example.com", "", "").0, "E");
        assert!(fallback("https://example.com", "例", "终端").0.is_empty());
        assert_eq!(fallback("https://example.com/a", "A", "").1, fallback("https://example.com/b", "B", "").1);
    }
    #[test]
    fn concrete_decoders_preserve_png_and_both_ico_encodings() {
        let source = image::RgbaImage::from_raw(2,2,vec![10,20,30,255,40,50,60,128,70,80,90,0,100,110,120,255]).unwrap();
        let mut png = Cursor::new(Vec::new());
        source.write_to(&mut png,image::ImageFormat::Png).unwrap();
        let mut ico_png = Cursor::new(Vec::new());
        source.write_to(&mut ico_png,image::ImageFormat::Ico).unwrap();
        let mut dib = Vec::new();
        for value in [40u32,2,4] { dib.extend_from_slice(&value.to_le_bytes()); }
        dib.extend_from_slice(&1u16.to_le_bytes()); dib.extend_from_slice(&32u16.to_le_bytes());
        for value in [0u32,16,0,0,0,0] { dib.extend_from_slice(&value.to_le_bytes()); }
        dib.extend_from_slice(&[30,20,10,255,60,50,40,128,90,80,70,255,120,110,100,255]);
        dib.extend_from_slice(&[0;8]);
        let mut ico_bmp = vec![0,0,1,0,1,0,2,2,0,0,1,0,32,0];
        ico_bmp.extend_from_slice(&(dib.len() as u32).to_le_bytes());
        ico_bmp.extend_from_slice(&22u32.to_le_bytes()); ico_bmp.extend_from_slice(&dib);
        for bytes in [png.get_ref().as_slice(),ico_png.get_ref().as_slice(),ico_bmp.as_slice(),include_bytes!("../../../ZapLaunch.ico").as_slice()] {
            let old = image::load_from_memory(bytes).unwrap().to_rgba8();
            let actual = decode(bytes).unwrap();
            assert_eq!((actual.0,actual.1),(old.width(),old.height()));
            assert!(actual.2 == old.into_raw(),"decoded RGBA must remain identical");
        }
        assert!(decode(b"\xff\xd8\xff\xe0unsupported JPEG").is_none());
        assert!(decode(&png.get_ref()[..png.get_ref().len()/2]).is_none());
    }

    #[test]
    fn rejects_html_and_oversized_images() {
        assert!(decode(b"<html>not an icon</html>").is_none());
        let image = image::RgbaImage::new(513, 1);
        let mut png = Cursor::new(Vec::new());
        image.write_to(&mut png, image::ImageFormat::Png).unwrap();
        assert!(decode(png.get_ref()).is_none());
        let image = image::RgbaImage::from_pixel(32,32,image::Rgba([10,20,30,255]));
        let mut png = Cursor::new(Vec::new());
        image.write_to(&mut png, image::ImageFormat::Png).unwrap();
        let (w,h,pixels) = decode(png.get_ref()).unwrap();
        assert_eq!((w,h,pixels.len()), (32,32,4096));
    }
    #[test]
    fn positive_and_negative_disk_cache_avoid_network() {
        let origin = "https://cache-test.invalid";
        let hash = origin.bytes().fold(0xcbf29ce484222325u64, |h,b| (h ^ b as u64).wrapping_mul(0x100000001b3));
        let root = std::env::temp_dir().canonicalize().unwrap();
        let dir = root.join(format!("web-cache-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(format!("{hash:016x}.icon"));
        let mut png = Cursor::new(Vec::new());
        image::RgbaImage::new(16,16).write_to(&mut png, image::ImageFormat::Png).unwrap();
        std::fs::write(&file, png.get_ref()).unwrap();
        assert_eq!(load(origin, &dir, false).map(|(w,h,_)| (w,h)), Some((16,16)));
        std::fs::write(&file, []).unwrap();
        assert!(load(origin, &dir, false).is_none());
        std::fs::remove_file(file).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }
}
