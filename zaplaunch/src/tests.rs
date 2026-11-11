use super::{apply_launch_success, button_hotkeys, grid_insert_at, group_selection_after_delete, next_grid_selection, LaunchSource, PendingLaunch};
use zaplaunch_core::AppConfig;
static VIEW_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn slow_edit_leaves_ui_free_and_applies_only_confirmed_snapshot() {
    use super::*;
    let _guard = VIEW_TEST_LOCK.lock().unwrap();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let writer = config_writer::Writer::start(move |_, _, _| {
        started_tx.send(()).unwrap();
        if release_rx.recv().unwrap() { Ok(()) } else { Err(std::io::Error::other("fixture failure")) }
    }).unwrap();
    let app = Rc::new(RefCell::new(AppConfig::default()));
    let old_search = current_search_trimmed();
    for success in [true, false] {
        let previous = app.borrow().clone();
        let mut next = previous.clone(); next.grid_lines = !next.grid_lines;
        let id = NEXT_CONFIG.fetch_add(1, Ordering::Relaxed);
        let observed = Rc::new(std::cell::Cell::new(None)); let done = observed.clone();
        CONFIG_PENDING.with(|p| *p.borrow_mut() = Some(PendingConfig {
            id, app: app.clone(), next: next.clone(), done: Box::new(move |ok| done.set(Some(ok))),
        }));
        writer.edit(next.clone(), previous.clone(), None, id);
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        set_search_query("UI still responds while the disk is blocked");
        assert_eq!(app.borrow().grid_lines, previous.grid_lines);
        finish_config(id + 100, Ok(Vec::new()));
        assert_eq!(observed.get(), None, "stale replies must not commit");
        release_tx.send(success).unwrap(); wait_config();
        assert_eq!(observed.get(), Some(success));
        assert_eq!(app.borrow().grid_lines, if success { next.grid_lines } else { previous.grid_lines });
    }
    writer.flush(); set_search_query(&old_search);
}

#[test]
fn slow_site_does_not_hold_queue_lock_or_allow_same_site_parallel_write() {
    use super::*;
    let queue = WebsiteQueue::default();
    queue.send(WebsiteRequest::Icon("slow".into(), 1, false)).unwrap();
    assert!(matches!(queue.recv(), Some(WebsiteRequest::Icon(_, 1, false))));
    queue.send(WebsiteRequest::Icon("slow".into(), 2, true)).unwrap();
    queue.send(WebsiteRequest::Icon("fast".into(), 3, false)).unwrap();
    let other = queue.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || { tx.send(other.recv()).unwrap(); });
    assert!(matches!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), Some(WebsiteRequest::Icon(_, 3, false))));
    thread.join().unwrap(); queue.finished("fast"); queue.finished("slow");
    assert!(matches!(queue.recv(), Some(WebsiteRequest::Icon(_, 2, true))));
    queue.finished("slow");
}

fn wait_config() {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while super::CONFIG_PENDING.with(|p| p.borrow().is_some()) {
        super::drain_tray_events();
        assert!(std::time::Instant::now() < deadline, "configuration transaction timed out");
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    super::config_writer::flush();
}

fn save_props_wait(w: &zaplaunch_ui::PropsWindow, app: &std::rc::Rc<std::cell::RefCell<AppConfig>>, ctx: &std::rc::Rc<std::cell::RefCell<super::PropsEdit>>, panel: &slint::Weak<zaplaunch_ui::MainPanel>) -> bool {
    let result = std::rc::Rc::new(std::cell::Cell::new(false));
    let done = result.clone();
    super::save_props(w, app, ctx, panel, move |ok| done.set(ok));
    wait_config();
    result.get()
}

#[test]
fn website_queue_keeps_latest_request_and_prioritizes_cleanup() {
    use super::*;
    let mut queue = std::collections::VecDeque::new();
    enqueue_website(&mut queue, WebsiteRequest::Icon("https://a.invalid".into(), 1, false));
    enqueue_website(&mut queue, WebsiteRequest::Keep(Default::default()));
    enqueue_website(&mut queue, WebsiteRequest::Icon("https://a.invalid".into(), 2, true));
    enqueue_website(&mut queue, WebsiteRequest::Keep(Default::default()));
    assert_eq!(queue.len(), 2);
    assert!(matches!(queue.pop_front(), Some(WebsiteRequest::Keep(_))));
    assert!(matches!(queue.pop_front(), Some(WebsiteRequest::Icon(_, 2, true))));
}

#[test]
fn grid_lines_are_crisp_theme_aware_and_settings_reuse_the_models() {
    use super::*;
    use slint::platform::{Platform,PlatformError,WindowAdapter,WindowEvent,PointerEventButton};
    use slint::platform::software_renderer::{MinimalSoftwareWindow,RepaintBufferType};
    let _guard = VIEW_TEST_LOCK.lock().unwrap();
    struct TestPlatform(Rc<RefCell<Vec<Rc<MinimalSoftwareWindow>>>>);
    impl Platform for TestPlatform {
        fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>,PlatformError> {
            let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
            self.0.borrow_mut().push(window.clone()); Ok(window)
        }
    }
    struct RestoreDir(Option<std::ffi::OsString>);
    impl Drop for RestoreDir {
        fn drop(&mut self) {
            if let Some(value) = self.0.take() { std::env::set_var(zaplaunch_core::CONFIG_DIR_ENV,value); }
            else { std::env::remove_var(zaplaunch_core::CONFIG_DIR_ENV); }
        }
    }
    let _restore = RestoreDir(std::env::var_os(zaplaunch_core::CONFIG_DIR_ENV));
    let dir = std::env::temp_dir().join(format!("grid-lines-ui-{}",std::process::id()));
    std::env::set_var(zaplaunch_core::CONFIG_DIR_ENV,&dir);
    let windows = Rc::new(RefCell::new(Vec::new()));
    slint::platform::set_platform(Box::new(TestPlatform(windows.clone()))).unwrap();
    let p = MainPanel::new().unwrap(); icon_scaling::bind(p.global::<zaplaunch_ui::IconRenderer>());
    let window = windows.borrow()[0].clone();
    let mut config = AppConfig::default();
    let mut buttons = config.groups.iter().flat_map(|g|g.buttons.iter().cloned()).take(18).collect::<Vec<_>>();
    for button in &mut buttons { button.target.clear(); }
    for group in &mut config.groups { group.buttons.clear(); }
    config.groups[0].buttons = buttons; config.tile_order = "manual".into();
    let render = |width:usize,height:usize| {
        let mut pixels = vec![slint::Rgb8Pixel::default();width*height];
        p.window().request_redraw(); assert!(window.draw_if_needed(|r| { r.render(&mut pixels,width); }));
        pixels
    };
    let previous = current_search_trimmed(); set_search_query("");
    p.show().unwrap();
    for scale in [1.0f32,1.25,1.5] { for dark in [false,true] { for label in [false,true] { for cols in [6,7,8] {
        config.grid_columns = cols; config.tile_show_label = label; config.grid_lines = false;
        p.global::<Palette>().set_dark(dark);
        p.window().dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor:scale });
        refresh_view(&p,&config); p.set_grid_height(170.0);
        let width = (520.0*scale).round() as usize; let height = (342.0*scale).round() as usize;
        window.set_size(slint::PhysicalSize::new(width as u32,height as u32));
        let off = render(width,height);
        let rows = p.get_rows();
        config.grid_lines = true; refresh_view(&p,&config); p.set_grid_height(170.0);
        assert!(p.get_grid_lines()); assert_eq!(p.get_rows(),rows,"grid lines must not replace tile models");
        let on = render(width,height);
        let line = if dark { slint::Rgb8Pixel::new(45,48,53) } else { slint::Rgb8Pixel::new(235,236,238) };
        let surface = if dark { slint::Rgb8Pixel::new(27,30,36) } else { slint::Rgb8Pixel::new(252,252,253) };
        let top = (128.0*scale).round() as usize; let left = (12.0*scale).round() as usize;
        let right = (508.0*scale).round() as usize - 1;
        let bottom = ((128.0 + 170.0)*scale).round() as usize - 1;
        for (x,y) in [(left,top),(right,top),(left,bottom),(right,bottom),
                      (left,(top+bottom)/2),(right,(top+bottom)/2),((left+right)/2,top),((left+right)/2,bottom)] {
            assert_eq!(on[y*width+x],line,"outer frame: DPI={scale} dark={dark} labels={label} columns={cols} x={x} y={y}");
            assert_ne!(off[y*width+x],line);
        }
        let probe_y = (130.0*scale).round() as usize;
        let first_x = left + ((p.get_tile_width()+1.0)*scale).round() as usize;
        for boundary in 1..cols {
            let x = left + ((boundary as f32*(p.get_tile_width()+2.0)-1.0)*scale).round() as usize;
            assert_eq!(on[probe_y*width+x],line,"vertical line: DPI={scale} dark={dark} labels={label} columns={cols}");
            assert_eq!(on[probe_y*width+x-1],surface,"line must be exactly one physical pixel");
            assert_eq!(on[probe_y*width+x+1],surface);
            assert_eq!(off[probe_y*width+x],surface);
        }
        let y = top + ((if label {82.0}else{tile_width(cols as usize)+2.0})*scale).round() as usize;
        if !label {
            assert!((first_x as i32-left as i32-(y-top) as i32).abs()<=2,"grid cells must be square within pixel rounding");
            let mut bounds=(usize::MAX,usize::MAX,0usize,0usize);
            for py in top+1..y { for px in left+1..first_x {
                if off[py*width+px]!=surface {
                    bounds.0=bounds.0.min(px); bounds.1=bounds.1.min(py);
                    bounds.2=bounds.2.max(px); bounds.3=bounds.3.max(py);
                }
            }}
            assert_ne!(bounds.0,usize::MAX);
            assert!(((bounds.0+bounds.2+1) as i32-(left+first_x) as i32).abs()<=2,"horizontal centering: DPI={scale} columns={cols} bounds={bounds:?}");
            assert!(((bounds.1+bounds.3+1) as i32-(top+y) as i32).abs()<=2,"vertical centering: DPI={scale} columns={cols} bounds={bounds:?}");
        }
        let probe_x = left + 1;
        assert_eq!(on[y*width+probe_x],line);
        assert_eq!(on[(y-1)*width+probe_x],surface);
        assert_eq!(on[(y+1)*width+probe_x],surface);
        assert_eq!(on[y*width+first_x],line,"intersections must not be darker");
        if scale==1.0 && cols==7 && label {
            if let Ok(folder)=std::env::var("ZAPLAUNCH_GRID_PREVIEW_DIR") {
                std::fs::create_dir_all(&folder).unwrap();
                let name=if dark {"grid-lines-dark.png"}else{"grid-lines-light.png"};
                image::RgbImage::from_raw(width as u32,height as u32,on.iter().flat_map(|p|[p.r,p.g,p.b]).collect()).unwrap().save(std::path::Path::new(&folder).join(name)).unwrap();
            }
        }
        p.invoke_reveal_tile(17);
        let scrolled=render(width,height);
        let row_height=if label {82.0}else{tile_width(cols as usize)+2.0};
        let offset=170.0-p.get_rows().row_count() as f32*row_height;
        let line_y=top+((row_height+offset)*scale).round() as usize;
        assert_eq!(scrolled[line_y*width+probe_x],line,"scrolling must retain pixel alignment at DPI {scale}");
        assert_eq!(scrolled[(line_y-1)*width+probe_x],surface);
        assert_eq!(scrolled[(line_y+1)*width+probe_x],surface);
        p.invoke_reveal_tile(0);
    }}}}
    for label in [false,true] {
        for rows in [0usize,1,2,3,6,7,72] {
            apply_dynamic_grid_height(&p,rows,label);
            let expected = if rows==0 {160.0} else {rows.min(6) as f32 * if label {82.0} else {p.get_tile_width()+2.0} + 6.0};
            assert_eq!(p.get_grid_height(),expected,"dynamic height: rows={rows} labels={label}");
        }
    }
    let full_group = config.groups[0].buttons.clone();
    config.grid_columns=7;
    for count in [1usize,8] { for label in [false,true] { for scale in [1.0f32,1.25,1.5] {
        config.groups[0].buttons=full_group[..count].to_vec(); config.tile_show_label=label;
        p.global::<Palette>().set_dark(false);
        p.window().dispatch_event(WindowEvent::ScaleFactorChanged {scale_factor:scale});
        refresh_view(&p,&config);
        let rows=count.div_ceil(7); let row_height=if label {82.0} else {p.get_tile_width()+2.0};
        assert_eq!(p.get_grid_height(),rows as f32*row_height+6.0);
        let width=(520.0*scale).round() as usize;
        let height=((172.0+p.get_grid_height())*scale).round() as usize;
        window.set_size(slint::PhysicalSize::new(width as u32,height as u32));
        let pixels=render(width,height);
        let left=(12.0*scale).round() as usize;
        let bottom=((128.0+rows as f32*row_height)*scale).round() as usize-1;
        let line=slint::Rgb8Pixel::new(235,236,238);
        assert_eq!(pixels[bottom*width+left],line,"compact frame bottom: count={count} labels={label} DPI={scale}");
        assert_eq!(pixels[bottom*width+width/2],line,"compact bottom center: count={count} labels={label} DPI={scale}");
        if scale==1.0 {
            if let Ok(folder)=std::env::var("ZAPLAUNCH_GRID_PREVIEW_DIR") {
                image::RgbImage::from_raw(width as u32,height as u32,pixels.iter().flat_map(|p|[p.r,p.g,p.b]).collect()).unwrap()
                    .save(std::path::Path::new(&folder).join(format!("compact-{count}-labels-{label}.png"))).unwrap();
            }
        }
    }}}
    config.groups[0].buttons=full_group;
    config.theme="light".into();
    let app = Rc::new(RefCell::new(config));
    let slot = Rc::new(RefCell::new(None));
    let settings = build_settings(app.clone(),p.as_weak(),slot);
    settings.set_tab(0);
    for on in [false,true,false] {
        settings.invoke_setting_changed("grid_lines".into(),if on {"1"}else{"0"}.into()); wait_config();
        assert_eq!(settings.get_tab(),0); assert_eq!(settings.get_grid_lines(),on);
        assert_eq!(p.get_grid_lines(),on); assert_eq!(app.borrow().grid_lines,on);
        assert_eq!(zaplaunch_core::load_or_init_from(&dir).config.grid_lines,on);
    }
    settings.show().unwrap();
    let settings_window=windows.borrow()[1].clone();
    settings_window.set_size(slint::PhysicalSize::new(480,640));
    let mut settings_pixels=vec![slint::Rgb8Pixel::default();480*640];
    settings.window().request_redraw();settings_window.draw_if_needed(|r|{r.render(&mut settings_pixels,480);});
    let toggle=slint::LogicalPosition::new(442.0,545.0);
    settings.window().dispatch_event(WindowEvent::PointerPressed {position:toggle,button:PointerEventButton::Left});
    settings.window().dispatch_event(WindowEvent::PointerReleased {position:toggle,button:PointerEventButton::Left}); wait_config();
    assert!(settings.get_grid_lines() && app.borrow().grid_lines && p.get_grid_lines(),"the new appearance switch must actually toggle and apply");
    assert!(zaplaunch_core::load_or_init_from(&dir).config.grid_lines);
    if let Ok(folder)=std::env::var("ZAPLAUNCH_GRID_PREVIEW_DIR") {
        std::thread::sleep(Duration::from_millis(150)); slint::platform::update_timers_and_animations();
        settings.window().request_redraw();settings_window.draw_if_needed(|r|{r.render(&mut settings_pixels,480);});
        image::RgbImage::from_raw(480,640,settings_pixels.iter().flat_map(|p|[p.r,p.g,p.b]).collect()).unwrap().save(std::path::Path::new(&folder).join("grid-lines-settings.png")).unwrap();
    }
    settings.hide().unwrap();
    set_search_query("计算"); app.borrow_mut().grid_lines=true; refresh_view(&p,&app.borrow());
    assert!(p.get_searching() && p.get_grid_lines());
    let rows=p.get_rows(); app.borrow_mut().grid_lines=false; refresh_view(&p,&app.borrow());
    assert!(!p.get_grid_lines()); assert_eq!(p.get_rows(),rows);
    set_search_query(""); app.borrow_mut().grid_lines=true; refresh_view(&p,&app.borrow());
    p.set_grid_height(170.0); p.window().dispatch_event(WindowEvent::ScaleFactorChanged {scale_factor:1.0});
    window.set_size(slint::PhysicalSize::new(520,342)); render(520,342);
    let clicked=Rc::new(std::cell::Cell::new(None));let observed=clicked.clone();
    p.on_tile_clicked(move |r,c|observed.set(Some((r,c))));
    let position=slint::LogicalPosition::new(46.0,156.0);
    p.window().dispatch_event(WindowEvent::PointerPressed {position,button:PointerEventButton::Left});
    p.window().dispatch_event(WindowEvent::PointerReleased {position,button:PointerEventButton::Left});
    assert_eq!(clicked.get(),Some((0,0)),"decorations must not intercept tile clicks");
    set_search_query(&previous); std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn group_order_preview_cancel_save_and_indexed_state_stay_consistent() {
    use super::*;
    use slint::platform::{Platform,PlatformError,WindowAdapter,WindowEvent,PointerEventButton};
    use slint::platform::software_renderer::{MinimalSoftwareWindow,RepaintBufferType};
    let _guard = VIEW_TEST_LOCK.lock().unwrap();
    struct TestPlatform(Rc<MinimalSoftwareWindow>);
    impl Platform for TestPlatform {
        fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>,PlatformError> { Ok(self.0.clone()) }
    }
    struct RestoreDir(Option<std::ffi::OsString>);
    impl Drop for RestoreDir {
        fn drop(&mut self) {
            if let Some(value) = self.0.take() { std::env::set_var(zaplaunch_core::CONFIG_DIR_ENV,value); }
            else { std::env::remove_var(zaplaunch_core::CONFIG_DIR_ENV); }
        }
    }
    let _restore = RestoreDir(std::env::var_os(zaplaunch_core::CONFIG_DIR_ENV));
    let dir = std::env::temp_dir().join(format!("group-order-{}",std::process::id()));
    std::env::set_var(zaplaunch_core::CONFIG_DIR_ENV,&dir);
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(window.clone()))).unwrap();
    let p = MainPanel::new().unwrap();
    icon_scaling::bind(p.global::<zaplaunch_ui::IconRenderer>());
    let mut config = AppConfig::default(); config.tile_order = "manual".into();
    config.groups = (0..3).map(|i| {
        let mut button = empty_button(); button.label = format!("marker {i}");
        button.target = format!("fixture-{i}.exe"); button.icon = "终端".into();
        zaplaunch_core::GroupConfig { name:format!("分组 {i}"),buttons:vec![button] }
    }).collect();
    let app = Rc::new(RefCell::new(config.clone()));
    bind_group_actions(&p,&app);
    sync_groups(&p,&config);
    p.show().unwrap(); window.set_size(slint::PhysicalSize::new(520,342));
    let render = || {
        let mut pixels = vec![slint::Rgb8Pixel::default();520*342];
        p.window().request_redraw(); assert!(window.draw_if_needed(|r| { r.render(&mut pixels,520); }));
        pixels
    };
    let save_button_preview = |name:&str| {
        if let Ok(folder)=std::env::var("ZAPLAUNCH_REUSE_PREVIEW_DIR") {
            std::fs::create_dir_all(&folder).unwrap();
            let pixels=render();
            image::RgbImage::from_raw(520,342,pixels.iter().flat_map(|p|[p.r,p.g,p.b]).collect()).unwrap()
                .save(std::path::Path::new(&folder).join(name)).unwrap();
        }
    };
    let click = |x,y| {
        let position = slint::LogicalPosition::new(x,y);
        p.window().dispatch_event(WindowEvent::PointerPressed { position,button:PointerEventButton::Left });
        p.window().dispatch_event(WindowEvent::PointerReleased { position,button:PointerEventButton::Left });
    };
    p.set_current_group(0); p.invoke_group_right_clicked(1);
    let preview = render();
    if let Ok(path) = std::env::var("ZAPLAUNCH_GROUP_ORDER_PREVIEW") {
        image::RgbImage::from_raw(520,342,preview.iter().flat_map(|p|[p.r,p.g,p.b]).collect()).unwrap().save(path).unwrap();
    }
    assert_eq!(p.get_group_modal_position(),1);
    save_button_preview("buttons-normal.png");
    click(245.0,195.0); assert_eq!(p.get_group_modal_position(),0);
    click(245.0,195.0); assert_eq!(p.get_group_modal_position(),0,"left boundary must be disabled");
    save_button_preview("buttons-disabled-left.png");
    click(349.0,195.0); assert_eq!(p.get_group_modal_position(),1);
    p.window().dispatch_event(WindowEvent::KeyPressed {text:" ".into()});
    p.window().dispatch_event(WindowEvent::KeyReleased {text:" ".into()});
    assert_eq!(p.get_group_modal_position(),2,"Space must activate a focused enabled button");
    click(349.0,195.0);
    assert_eq!(p.get_group_modal_position(),2,"right boundary must be disabled");
    save_button_preview("buttons-disabled-right.png");
    p.global::<Palette>().set_dark(true); save_button_preview("buttons-disabled-dark.png");
    p.global::<Palette>().set_dark(false);
    p.set_group_modal_name("temporary".into());
    assert_eq!(app.borrow().groups,config.groups,"preview must not change configuration");
    p.invoke_group_modal_cancel(); wait_config();
    assert_eq!(app.borrow().groups,config.groups);
    assert!(!dir.join("config.json").exists());
    p.invoke_group_right_clicked(1); render();
    assert_eq!(p.get_group_modal_name().as_str(),config.groups[1].name);
    assert_eq!(p.get_group_modal_position(),1,"reopening must discard the unsaved position");
    click(349.0,195.0); p.invoke_group_modal_save(); wait_config();
    assert!(!p.get_group_modal_open()); assert_eq!(p.get_current_group(),0);
    assert_eq!(app.borrow().groups,[config.groups[0].clone(),config.groups[2].clone(),config.groups[1].clone()]);
    let disk = zaplaunch_core::load_or_init_from(&dir).config;
    assert_eq!(disk.groups,app.borrow().groups);
    p.set_current_group(1);
    let button = app.borrow().groups[1].buttons[0].clone();
    PENDING_LAUNCHES.with(|q| q.borrow_mut().insert(u64::MAX,PendingLaunch {
        source:LaunchSource::Button(1,0),target:button.target,params:button.params,dir:button.work_dir,
        label:button.label,window_state:button.window_state,hide_generation:None,hist_on:false,hist_max:20,sound_on:false,network:false,timed_out:false,timeout:Timer::default(),
    }));
    p.invoke_group_right_clicked(2); p.set_group_modal_position(0); p.invoke_group_modal_save(); wait_config();
    assert_eq!(p.get_current_group(),2,"selected group must follow its new index");
    let request = PENDING_LAUNCHES.with(|q| q.borrow_mut().remove(&u64::MAX).unwrap());
    assert!(matches!(request.source,LaunchSource::Button(2,0)));
    apply_launch_success(&mut app.borrow_mut(),&request);
    assert_eq!(app.borrow().groups[2].buttons[0].run_count,1);
    assert_eq!(app.borrow().groups[0].buttons[0].run_count,0);
    p.set_current_group(0); p.invoke_group_right_clicked(0); p.set_group_modal_position(2);
    p.set_group_modal_name("renamed group".into());
    p.invoke_group_modal_save(); wait_config(); assert_eq!(p.get_current_group(),2);
    assert_eq!(app.borrow().groups[2].name,"renamed group");
    let previous = current_search_trimmed(); set_search_query("marker"); refresh_view(&p,&app.borrow());
    for i in 0..3 {
        let (gi,bi) = hit_at(i).unwrap();
        assert_eq!(p.get_rows().row_data(0).unwrap().row_data(i).unwrap().label.as_str(),app.borrow().groups[gi].buttons[bi].label);
    }
    set_search_query(&previous);
    p.invoke_add_group_clicked(); assert!(!p.get_group_modal_editing());
    p.invoke_group_modal_cancel(); wait_config();
    app.borrow_mut().groups.truncate(1); sync_groups(&p,&app.borrow());
    p.invoke_group_right_clicked(0); render();
    click(245.0,195.0); click(349.0,195.0);
    assert_eq!(p.get_group_modal_position(),0,"a single group cannot move in either direction");
    p.invoke_group_modal_cancel(); wait_config();
    app.borrow_mut().groups = config.groups.clone();
    sync_groups(&p, &app.borrow());
    PENDING_LAUNCHES.with(|pending| {
        for group in [0, 2] {
            let button = app.borrow().groups[group].buttons[0].clone();
            pending.borrow_mut().insert(group as u64, PendingLaunch {
                source: LaunchSource::Button(group, 0), target: button.target, params: button.params,
                dir: button.work_dir, label: button.label, window_state: button.window_state,
                hide_generation: None, hist_on: true, hist_max: 20, sound_on: false, network: false, timed_out: false, timeout: Timer::default(),
            });
        }
    });
    p.invoke_group_right_clicked(0);
    p.invoke_group_modal_delete();
    p.invoke_group_modal_cancel();
    p.invoke_group_right_clicked(2);
    p.set_group_modal_name("draft after delete".into());
    wait_config();
    assert!(p.get_group_modal_open());
    assert_eq!(p.get_group_modal_name(), "draft after delete");
    assert_eq!(p.get_group_modal_position(), 1);
    PENDING_LAUNCHES.with(|pending| {
        let removed = pending.borrow_mut().remove(&0).unwrap();
        let remaining = pending.borrow_mut().remove(&2).unwrap();
        assert!(matches!(removed.source, LaunchSource::RemovedButton));
        assert!(matches!(remaining.source, LaunchSource::Button(1, 0)));
        let mut config = app.borrow_mut();
        apply_launch_success(&mut config, &removed);
        apply_launch_success(&mut config, &remaining);
        assert_eq!(config.history.len(), 2);
        assert_eq!(config.groups[1].buttons[0].run_count, 1);
    });
    p.invoke_group_modal_save();
    wait_config();
    assert_eq!(zaplaunch_core::load_or_init_from(&dir).config.groups[1].name, "draft after delete");
    p.invoke_group_modal_cancel(); wait_config();
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn group_move_index_mapping_covers_both_directions_and_unchanged_groups() {
    use super::*;
    for from in 0..4 { for to in 0..4 {
        let mut config = AppConfig::default();
        config.groups = (0..4).map(|i|zaplaunch_core::GroupConfig { name:i.to_string(),buttons:vec![] }).collect();
        assert!(config.move_group(from,to));
        for index in 0..4 {
            let mapped = group_index_after_move(index,from,to);
            assert_eq!(config.groups[mapped].name,index.to_string());
        }
    }}
}

#[test]
fn input_method_focus_recovery_preserves_search_and_group_selection() {
    use super::*;
    use slint::platform::{Platform,PlatformError,WindowAdapter,WindowEvent,Key};
    use slint::platform::software_renderer::{MinimalSoftwareWindow,RepaintBufferType};
    use i_slint_core::input::{InternalKeyEvent,KeyEvent,KeyEventType};
    struct TestPlatform;
    impl Platform for TestPlatform {
        fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>,PlatformError> {
            Ok(MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer))
        }
    }
    slint::platform::set_platform(Box::new(TestPlatform)).unwrap();
    let p = MainPanel::new().unwrap(); p.show().unwrap();
    p.window().dispatch_event(WindowEvent::WindowActiveChanged(true));
    let commit = |text:&str| {
        let mut key = KeyEvent::default(); key.text = text.into();
        p.window().dispatch_event(WindowEvent::internal(InternalKeyEvent {
            event_type:KeyEventType::CommitComposition,key_event:key,..Default::default()
        }));
    };
    p.set_query("旧搜索".into()); p.invoke_focus_search();
    p.window().dispatch_event(WindowEvent::WindowActiveChanged(false));
    assert!(!p.get_search_input_focused());
    assert!(p.get_text_input_requested(),"activation loss must not erase the requested input target");
    p.invoke_restore_input_focus();
    assert!(p.get_search_input_focused(),"repair must work even when has-focus was already cleared");
    p.window().dispatch_event(WindowEvent::KeyPressed { text:Key::Control.into() });
    p.window().dispatch_event(WindowEvent::KeyPressed { text:"a".into() });
    p.window().dispatch_event(WindowEvent::KeyReleased { text:Key::Control.into() });
    p.invoke_restore_input_focus();
    commit("中文搜索");
    assert_eq!(p.get_query().as_str(),"中文搜索","recovery must preserve the selected range");
    p.set_group_modal_name("旧分组名称".into()); p.set_group_modal_open(true); p.invoke_focus_group_input();
    assert!(p.get_group_input_focused());
    p.window().dispatch_event(WindowEvent::WindowActiveChanged(false));
    assert!(!p.get_group_input_focused());
    assert!(p.get_text_input_requested());
    p.invoke_restore_input_focus();
    assert!(p.get_group_input_focused());
    commit("中文分组");
    assert_eq!(p.get_group_modal_name().as_str(),"中文分组");
    assert_eq!(p.get_query().as_str(),"中文搜索");
    p.invoke_clear_search_focus(); p.invoke_restore_input_focus();
    assert!(!p.get_text_input_requested());
    assert!(!p.get_search_input_focused() && !p.get_group_input_focused(),"grid focus must not be stolen");
    p.set_group_modal_open(false); p.invoke_focus_search();
    p.window().dispatch_event(WindowEvent::internal(InternalKeyEvent {
        event_type:KeyEventType::UpdateComposition,preedit_text:"pinyin".into(),preedit_selection:Some(6..6),..Default::default()
    }));
    assert!(!recover_panel_input_method(&p),"a software window has no native IME to repair");
    assert_eq!(p.get_query().as_str(),"中文搜索");
    commit("输入");
    assert!(p.get_query().contains("输入"));
}

#[test]
fn rebuild_cache_updates_icons_waits_for_old_requests_and_survives_settings_close() {
    let _guard = VIEW_TEST_LOCK.lock().unwrap();
    use super::*;
    use slint::platform::{Platform,PlatformError,WindowAdapter};
    use slint::platform::software_renderer::{MinimalSoftwareWindow,RepaintBufferType};
    struct TestPlatform;
    impl Platform for TestPlatform {
        fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>,PlatformError> {
            Ok(MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer))
        }
    }
    slint::platform::set_platform(Box::new(TestPlatform)).unwrap();
    let root = std::env::temp_dir().join(format!("rebuild-icons-{}",std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let a = root.join("a.png"); let b = root.join("b.png");
    image::RgbaImage::from_pixel(32,32,image::Rgba([10,20,30,255])).save(&a).unwrap();
    image::RgbaImage::from_pixel(32,32,image::Rgba([40,50,60,255])).save(&b).unwrap();
    let mut c = AppConfig::default();
    for group in &mut c.groups { group.buttons.clear(); }
    for path in [&a,&b,&a] {
        let mut button = empty_button();
        button.label = "图标".into(); button.target = "fixture.exe".into(); button.icon = path.to_string_lossy().into_owned();
        c.groups[0].buttons.push(button);
    }
    let keys = configured_icon_keys(&c);
    assert_eq!(keys.len(),2);
    let app = Rc::new(RefCell::new(c));
    let p = MainPanel::new().unwrap();
    let settings = Rc::new(RefCell::new(None));
    let host = Host { panel:p.as_weak(),app:app.clone(),settings:settings.clone(),about:Rc::new(RefCell::new(None)),
        props:Rc::new(RefCell::new(None)),props_ctx:Rc::new(RefCell::new(PropsEdit::default())) };
    refresh_group(&p,&app.borrow(),0);
    let started = std::time::Instant::now();
    while ICON_REQUESTS.with(|r| !r.borrow().is_empty()) {
        host.handle(TrayEvent::AssetReady);
        assert!(started.elapsed() < Duration::from_secs(15));
        std::thread::sleep(Duration::from_millis(1));
    }
    let rows = p.get_rows(); let row = rows.row_data(0).unwrap();
    let old_b = row.row_data(1).unwrap().native_icon;
    let config_before = app.borrow().clone();
    let w = build_settings(app.clone(),p.as_weak(),settings.clone());
    w.set_tab(3);
    *settings.borrow_mut() = Some(w.clone_strong());
    let cache_root = root.join("cache");
    let native = cache_root.join("native-icons"); let web = cache_root.join("favicons");
    std::fs::create_dir_all(&native).unwrap(); std::fs::create_dir_all(&web).unwrap();
    let orphan = native.join("0123456789abcdef.rgba"); let web_orphan = web.join("0123456789abcdef.icon");
    std::fs::write(&orphan,b"old").unwrap(); std::fs::write(&web_orphan,b"old").unwrap();
    let ww = w.as_weak(); let app2 = app.clone(); let cache2 = cache_root.clone();
    w.on_rebuild_cache(move || {
        let w = ww.upgrade(); begin_cache_rebuild(&app2.borrow(),cache2.clone(),w.as_ref());
    });
    w.show().unwrap();
    w.window().set_size(slint::PhysicalSize::new(480,640));
    if let Ok(path) = std::env::var("ZAPLAUNCH_CACHE_PREVIEW") {
        let pixels = w.window().take_snapshot().unwrap();
        image::RgbaImage::from_raw(pixels.width(),pixels.height(),pixels.as_bytes().to_vec()).unwrap().save(path).unwrap();
    }
    let click_rebuild = || {
        use slint::platform::{WindowEvent,PointerEventButton};
        let position = slint::LogicalPosition::new(422.0,425.0);
        w.window().dispatch_event(WindowEvent::PointerPressed { position,button:PointerEventButton::Left });
        w.window().dispatch_event(WindowEvent::PointerReleased { position,button:PointerEventButton::Left });
    };
    image::RgbaImage::from_pixel(32,32,image::Rgba([90,80,70,255])).save(&a).unwrap();
    std::fs::remove_file(&b).unwrap();
    assert!(request_icon(&keys[0],false));
    click_rebuild();
    assert!(w.get_cache_busy()); assert_eq!(w.get_tab(),3);
    assert!(CACHE_REBUILD.with(|state| state.borrow().as_ref().unwrap().waiting.contains(&keys[0])));
    let requested = NEXT_ICON_REQUEST.load(Ordering::Relaxed);
    click_rebuild();
    w.invoke_rebuild_cache();
    assert_eq!(requested,NEXT_ICON_REQUEST.load(Ordering::Relaxed),"duplicate action must not restart the job");
    settings.borrow_mut().take();
    w.hide().unwrap();
    let reopened = build_settings(app.clone(),p.as_weak(),settings.clone());
    assert!(reopened.get_cache_busy());
    reopened.set_tab(3);
    *settings.borrow_mut() = Some(reopened.clone_strong());
    while CACHE_REBUILD.with(|state| state.borrow().is_some()) {
        host.handle(TrayEvent::AssetReady);
        assert!(started.elapsed() < Duration::from_secs(15));
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(!reopened.get_cache_busy()); assert_eq!(reopened.get_tab(),3);
    assert!(reopened.get_cache_status().contains("失败 1 项"));
    assert!(!orphan.exists() && !web_orphan.exists());
    assert_eq!(rows,p.get_rows()); assert_eq!(row,p.get_rows().row_data(0).unwrap());
    let actual = row.row_data(0).unwrap().native_icon.to_rgba8().unwrap();
    assert!(actual.as_bytes() == [90,80,70,255].repeat(32*32));
    assert!(row.row_data(1).unwrap().native_icon == old_b,"failed refresh must retain the displayed good icon");
    assert_eq!(app.borrow().groups,config_before.groups);
    for group in &mut app.borrow_mut().groups { group.buttons.clear(); }
    begin_cache_rebuild(&app.borrow(),cache_root.clone(),Some(&reopened));
    while CACHE_REBUILD.with(|state| state.borrow().is_some()) {
        host.handle(TrayEvent::AssetReady);
        assert!(started.elapsed() < Duration::from_secs(15));
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(reopened.get_cache_status().contains("0 项"));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn automatic_web_cleanup_follows_config_changes_on_the_worker() {
    use super::*;
    let _guard = VIEW_TEST_LOCK.lock().unwrap();
    let dir = std::env::temp_dir().join(format!("zaplaunch-worker-cache-{}",std::process::id())).join("favicons");
    std::fs::create_dir_all(&dir).unwrap();
    let mut config = AppConfig::default();
    let mut button = config.groups[0].buttons[0].clone();
    for group in &mut config.groups { group.buttons.clear(); }
    button.target = "https://automatic-cleanup.invalid/page".into();
    button.icon.clear();
    config.groups[0].buttons.push(button);
    let keep = dir.join(web_icons::cache_file_name("https://automatic-cleanup.invalid"));
    let orphan = dir.join(web_icons::cache_file_name("https://removed.invalid"));
    for path in [&keep,&orphan] { std::fs::write(path,b"fixture").unwrap(); }
    let wait_removed = |path: &std::path::Path| {
        let start = std::time::Instant::now();
        while path.exists() {
            assert!(start.elapsed() < Duration::from_secs(5),"background cleanup did not finish");
            std::thread::sleep(Duration::from_millis(1));
        }
    };
    maintain_web_cache(&config);
    wait_removed(&orphan);
    assert!(keep.exists());
    // Moving or renaming a button keeps the same origin and must retain its cache.
    config.groups[0].buttons[0].label = "renamed".into();
    maintain_web_cache(&config);
    config.groups[0].buttons.clear();
    maintain_web_cache(&config);
    wait_removed(&keep);
}

#[test]
fn cache_cleanup_keeps_current_files_and_does_not_delete_unrelated_content() {
    use super::*;
    let root = std::env::temp_dir().join(format!("cache-cleanup-{}",std::process::id()));
    let native = root.join("native-icons"); let web = root.join("favicons");
    std::fs::create_dir_all(&native).unwrap(); std::fs::create_dir_all(&web).unwrap();
    let keys: Vec<String> = vec!["c:\\fixture.exe".into(),"https://example.com".into()];
    let keep_native = native.join(icon_disk_cache::cache_file_name(&keys[0]));
    let keep_web = web.join(web_icons::cache_file_name(&keys[1]));
    let old_native = native.join("0123456789abcdef.rgba"); let old_web = web.join("0123456789abcdef.icon");
    for file in [&keep_native,&keep_web,&old_native,&old_web] { std::fs::write(file,b"fixture").unwrap(); }
    let unrelated = native.join("notes.txt"); std::fs::write(&unrelated,b"user data").unwrap();
    let directory = native.join("ffffffffffffffff.rgba"); std::fs::create_dir_all(&directory).unwrap();
    assert_eq!(clean_icon_cache(&root,&keys),0);
    assert!(keep_native.exists() && keep_web.exists() && unrelated.exists() && directory.exists());
    assert!(!old_native.exists() && !old_web.exists());
    assert_eq!(clean_icon_cache(&root,&[]),0);
    assert!(!keep_native.exists() && !keep_web.exists());
    std::fs::remove_dir_all(&native).unwrap();
    std::fs::write(&native,b"blocked cache directory").unwrap();
    assert_eq!(clean_icon_cache(&root,&[]),1,"cleanup errors must be reported");
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn asset_cache_evicts_only_the_least_recent_entry_and_keeps_failures() {
    use super::*;
    let mut cache = AssetCache::default();
    for i in 0..PATH_CACHE_LIMIT { cache.insert(i.to_string(), Some(i)); }
    assert_eq!(cache.get("0"), Some(Some(0)));
    cache.insert("failure".into(), None);
    assert_eq!(cache.entries.len(), PATH_CACHE_LIMIT);
    assert_eq!(cache.get("0"), Some(Some(0)));
    assert_eq!(cache.get("1"), None);
    assert_eq!(cache.get("failure"), Some(None));
    cache.insert("0".into(), Some(99));
    assert_eq!(cache.get("0"), Some(Some(99)));
    assert_eq!(cache.entries.len(), PATH_CACHE_LIMIT);
}

#[test]
fn five_hundred_distinct_icons_load_once_and_preserve_models() {
    let _view_guard = VIEW_TEST_LOCK.lock().unwrap();
    use super::*;
    use slint::platform::{Platform, PlatformError, WindowAdapter};
    use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
    struct TestPlatform;
    impl Platform for TestPlatform {
        fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
            Ok(MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer))
        }
    }
    slint::platform::set_platform(Box::new(TestPlatform)).unwrap();
    let root = std::env::temp_dir().canonicalize().unwrap();
    let dir = root.join(format!("zaplaunch-500-icons-{}-{}",std::process::id(),NEXT_ICON_REQUEST.load(Ordering::Relaxed)));
    std::fs::create_dir(&dir).unwrap();
    let mut c = AppConfig::default();
    c.sound_enabled = false;
    for g in &mut c.groups { g.buttons.clear(); }
    for i in 0..500 {
        let path = dir.join(format!("icon-{i}.png"));
        image::RgbaImage::from_pixel(48,48,image::Rgba([i as u8,(i / 256) as u8,100,255])).save(&path).unwrap();
        let mut b = empty_button();
        b.label = format!("工具 {i}"); b.target = format!("C:\\Fixture\\tool_{i}.exe");
        b.icon = path.to_string_lossy().into_owned();
        c.groups[0].buttons.push(b);
    }
    c.history.push(HistoryEntry { target:c.groups[0].buttons[0].target.clone(),label:"工具 0".into(),
        params:String::new(),work_dir:String::new(),ts:1,window_state:"normal".into() });
    let app = Rc::new(RefCell::new(c));
    let p = MainPanel::new().unwrap();
    let ctx = Rc::new(RefCell::new(PropsEdit::default()));
    let props = PropsWindow::new().unwrap();
    let host = Host { panel:p.as_weak(),app:app.clone(),settings:Rc::new(RefCell::new(None)),
        about:Rc::new(RefCell::new(None)),props:Rc::new(RefCell::new(Some(props.clone_strong()))),props_ctx:ctx.clone() };
    let started = std::time::Instant::now();
    let before = NEXT_ICON_REQUEST.load(Ordering::Relaxed);
    let mut preload = app.borrow().clone();
    let other = preload.groups[0].buttons.split_off(250);
    preload.groups.push(zaplaunch_core::GroupConfig { name:"后台分组".into(),buttons:other });
    let duplicate = preload.groups.last().unwrap().buttons[0].clone();
    preload.groups[0].buttons.push(duplicate);
    let mut website = empty_button();
    website.target = "https://preload-must-not-download.invalid".into();
    preload.groups[0].buttons.push(website);
    preload_local_icons(&preload);
    preload_local_icons(&preload);
    assert_eq!(NEXT_ICON_REQUEST.load(Ordering::Relaxed)-before,500,"preload must cover other groups, deduplicate and skip websites");
    refresh_group(&p,&app.borrow(),0);
    let group_tabs = p.get_groups();
    p.set_history(ModelRc::new(VecModel::from(history_rows(&app.borrow()))));
    fill_props(&props,&app.borrow(),&ctx,0,0);
    let outer = p.get_rows(); let first = outer.row_data(0).unwrap();
    let requested = NEXT_ICON_REQUEST.load(Ordering::Relaxed);
    assert_eq!(requested - before,500,"each distinct source must be requested once");
    while ICON_REQUESTS.with(|r| !r.borrow().is_empty()) {
        host.handle(TrayEvent::AssetReady);
        assert!(started.elapsed() < Duration::from_secs(15), "500-icon loading failed to settle");
        std::thread::sleep(Duration::from_millis(1));
    }
    let load_ms = started.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(outer,p.get_rows(),"icon completion must not rebuild the outer model");
    assert_eq!(first,p.get_rows().row_data(0).unwrap(),"icon completion must not replace tile rows");
    let count: usize = (0..outer.row_count()).map(|r| {
        let row=outer.row_data(r).unwrap(); (0..row.row_count()).filter(|&i|row.row_data(i).unwrap().has_native_icon).count()
    }).sum();
    assert_eq!(count,500);
    assert!(p.get_history().row_data(0).unwrap().has_native_icon);
    assert!(props.get_has_native_icon());
    let warm = std::time::Instant::now();
    for query in ["工具", "tool_19", "notfound", "", "工具 2", ""] {
        set_search_query(query); refresh_view(&p,&app.borrow());
        assert_eq!(outer,p.get_rows(),"search must reuse the outer model even when row count changes");
        assert_eq!(group_tabs,p.get_groups(),"refresh must reuse group tabs");
        let expected = if query.is_empty() { (0..500).map(|bi|(0,bi)).collect() }
            else { zaplaunch_core::search_buttons(&app.borrow(),query,0) };
        assert_eq!(HIT_MAP.with(|hits|hits.borrow().clone()),expected);
        for (index,&(gi,bi)) in expected.iter().enumerate() {
            assert_eq!(p.get_rows().row_data(index/7).unwrap().row_data(index%7).unwrap().label,
                app.borrow().groups[gi].buttons[bi].label);
        }
    }
    let warm_ms = warm.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(requested,NEXT_ICON_REQUEST.load(Ordering::Relaxed),"warm searches must not re-extract any of 500 icons");
    assert!(ICON_REQUESTS.with(|r|r.borrow().is_empty()));
    let stable = p.get_rows().row_data(0).unwrap();
    refresh_view(&p,&app.borrow());
    assert_eq!(stable,p.get_rows().row_data(0).unwrap());
    assert_eq!(ICON_CACHE.with(|cache|cache.borrow().entries.len()),500);
    app.borrow_mut().grid_columns = 8;
    refresh_view(&p,&app.borrow());
    assert_eq!(outer,p.get_rows()); assert_eq!(p.get_rows().row_data(0).unwrap().row_count(),8);
    assert_eq!(p.get_rows().row_data(1).unwrap().row_data(0).unwrap().label,"工具 8");
    assert_eq!(requested,NEXT_ICON_REQUEST.load(Ordering::Relaxed));
    println!("500 distinct PNG icons: loaded in {load_ms:.1}ms; 6 warm searches/refreshes {warm_ms:.1}ms; no repeat extraction");
    // 可选长测，仅测试构建启用；外部探针按进度采样内存/句柄，不影响产品行为。
    if let Some(rounds) = std::env::var("ZAPLAUNCH_SOAK_ROUNDS").ok().and_then(|s|s.parse::<usize>().ok()) {
        let started = std::time::Instant::now();
        let mut times = Vec::new();
        for round in 1..=rounds {
            let tick = std::time::Instant::now();
            for query in ["工具", "tool_19", "notfound", "", "工具 2", ""] {
                set_search_query(query); refresh_view(&p,&app.borrow());
            }
            app.borrow_mut().grid_columns = if round % 2 == 0 { 7 } else { 8 };
            refresh_view(&p,&app.borrow());
            let slot = Rc::new(RefCell::new(None));
            let ctx = Rc::new(RefCell::new(PropsEdit::default()));
            let w = build_props(app.clone(),p.as_weak(),ctx.clone(),slot.clone());
            fill_props(&w,&app.borrow(),&ctx,0,round % 500);
            *slot.borrow_mut() = Some(w.clone_strong());
            w.show().unwrap();
            close_child(&w,slot);
            drop(w);
            slint::platform::update_timers_and_animations();
            times.push(tick.elapsed().as_secs_f64()*1000.0);
            assert_eq!(requested,NEXT_ICON_REQUEST.load(Ordering::Relaxed));
            assert_eq!(outer,p.get_rows());
            assert!(ICON_REQUESTS.with(|r|r.borrow().is_empty()));
            assert!(DEFERRED.with(|timers|timers.borrow().is_empty()),"closed windows must release their deferred ownership");
            if round % 100 == 0 {
                let block=&times[times.len()-100..];
                println!("SOAK {} {} {:.1} {:.3} {}",std::process::id(),round,started.elapsed().as_secs_f64()*1000.0,
                    block.iter().sum::<f64>()/100.0,ICON_CACHE.with(|cache|cache.borrow().entries.len()));
            }
        }
    }
    let mut changed = app.borrow().clone();
    changed.groups[0].name = "重命名".into();
    changed.groups.push(zaplaunch_core::GroupConfig { name:"重命名".into(),buttons:Vec::new() });
    sync_groups(&p,&changed);
    assert_eq!(group_tabs,p.get_groups());
    for (index, group) in changed.groups.iter().enumerate() {
        assert_eq!(group_tabs.row_data(index).unwrap().name,group.name);
    }
    changed.groups.reverse();
    changed.groups.remove(1);
    sync_groups(&p,&changed);
    assert_eq!(group_tabs,p.get_groups());
    assert_eq!(group_tabs.row_count(),changed.groups.len());
    for (index, group) in changed.groups.iter().enumerate() {
        assert_eq!(group_tabs.row_data(index).unwrap().name,group.name);
    }
    changed.groups.clear();
    sync_groups(&p,&changed);
    assert_eq!(group_tabs,p.get_groups());
    assert_eq!(group_tabs.row_count(),0);
    sync_groups(&p,&app.borrow());
    assert_eq!(group_tabs,p.get_groups());
    assert_eq!(group_tabs.row_count(),app.borrow().groups.len());
    if dir.parent() == Some(root.as_path()) { std::fs::remove_dir_all(dir).unwrap(); }
}

#[test]
fn settings_refresh_preserves_current_page_and_updates_controls() {
    use super::*;
    use slint::platform::{Platform, PlatformError, WindowAdapter};
    use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
    struct TestPlatform;
    impl Platform for TestPlatform {
        fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
            Ok(MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer))
        }
    }
    slint::platform::set_platform(Box::new(TestPlatform)).unwrap();
    let w = SettingsWindow::new().unwrap();
    let mut config = AppConfig::default();
    assert_eq!(w.get_tab(),0,"a new settings window starts on Appearance");
    for tab in 0..4 {
        w.set_tab(tab);
        for on in [false,true] {
            config.follow_mouse = on; config.click_to_run = on;
            config.hide_after_run = on; config.hide_on_blur = on;
            config.sound_enabled = on; config.esc_hide = on;
            config.history_enabled = on; config.tile_show_label = on;
            config.grid_scrollbar = on; config.grid_lines = on;
            fill_settings(&w,&config);
            assert_eq!(w.get_tab(),tab,"refreshing controls must preserve page {tab}");
            assert_eq!(w.get_follow_mouse(),on); assert_eq!(w.get_click_to_run(),on);
            assert_eq!(w.get_hide_after_run(),on); assert_eq!(w.get_hide_on_blur(),on);
            assert_eq!(w.get_sound_enabled(),on); assert_eq!(w.get_esc_hide(),on);
            assert_eq!(w.get_history_enabled(),on); assert_eq!(w.get_tile_show_label(),on);
            assert_eq!(w.get_grid_scrollbar(),on); assert_eq!(w.get_grid_lines(),on);
        }
    }
    config.reset_settings(); fill_settings(&w,&config);
    assert_eq!(w.get_tab(),3,"restoring defaults must also preserve the current page");
    assert_eq!(SettingsWindow::new().unwrap().get_tab(),0);
}

#[test]
fn child_centering_uses_actual_sizes_and_keeps_negative_monitor_coordinates() {
    use slint::{PhysicalPosition as Pos, PhysicalSize as Size};
    assert_eq!(super::centered_position(Pos::new(100,200),Size::new(520,342),Size::new(420,506)),Pos::new(150,118));
    assert_eq!(super::centered_position(Pos::new(100,200),Size::new(520,342),Size::new(420,622)),Pos::new(150,60));
    assert_eq!(super::centered_position(Pos::new(-1800,-800),Size::new(650,428),Size::new(525,778)),Pos::new(-1738,-975));
    assert_eq!(super::centered_position(Pos::new(-1800,-800),Size::new(650,428),Size::new(630,933)),Pos::new(-1790,-1052));
}

#[test]
fn default_selection_has_no_ring_until_keyboard_navigation() {
    use super::*;
    use slint::platform::{Platform, PlatformError, WindowAdapter};
    use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
    struct TestPlatform(Rc<MinimalSoftwareWindow>);
    impl Platform for TestPlatform {
        fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> { Ok(self.0.clone()) }
    }
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(window.clone()))).unwrap();
    let p = MainPanel::new().unwrap();
    icon_scaling::bind(p.global::<zaplaunch_ui::IconRenderer>());
    let row = ModelRc::new(VecModel::from(vec![Tile { sel: true, has_native_icon: true, label: "test".into(), ..Default::default() }]));
    p.set_rows(ModelRc::new(VecModel::from(vec![row])));
    p.set_hit_count(1);
    p.show().unwrap();
    window.set_size(slint::PhysicalSize::new(520,342));
    let render = || {
        let mut pixels = vec![slint::Rgb8Pixel::default(); 520 * 342];
        p.window().request_redraw();
        assert!(window.draw_if_needed(|r| { r.render(&mut pixels,520); }));
        pixels
    };
    assert!(!p.get_selection_visible());
    let normal = render();
    p.set_keyboard_navigation(true);
    let keyboard = render();
    assert!(normal.iter().zip(&keyboard).filter(|(a,b)| a != b).count() > 30, "keyboard navigation must reveal a visible focus cue");
    p.set_keyboard_navigation(false);
    assert_eq!(normal, render(), "mouse mode must restore the uniform icon appearance");
    assert!(p.get_rows().row_data(0).unwrap().row_data(0).unwrap().sel, "Enter still has a default target");
    p.set_searching(true);
    assert!(p.get_selection_visible(), "search must identify the Enter target");

    use i_slint_core::input::{InternalKeyEvent, KeyEvent, KeyEventType};
    use slint::platform::WindowEvent;
    let switched = Rc::new(RefCell::new(Vec::new()));
    let observed = switched.clone();
    p.on_group_clicked(move |index| observed.borrow_mut().push(index));
    let consumed = Rc::new(std::cell::Cell::new(0));
    let notices = consumed.clone();
    p.on_tab_consumed(move || notices.set(notices.get()+1));
    let tab = |backwards| {
        let before = consumed.get();
        if backwards { p.window().dispatch_event(WindowEvent::KeyPressed { text:slint::platform::Key::Shift.into() }); }
        p.window().dispatch_event(WindowEvent::KeyPressed { text:slint::platform::Key::Tab.into() });
        p.window().dispatch_event(WindowEvent::KeyReleased { text:slint::platform::Key::Tab.into() });
        if backwards { p.window().dispatch_event(WindowEvent::KeyReleased { text:slint::platform::Key::Shift.into() }); }
        assert_eq!(consumed.get(),before+1,"each handled Tab must notify native input exactly once");
    };
    p.set_searching(false);
    p.set_groups(ModelRc::new(VecModel::from(["常用","工具","网页"].into_iter().map(|name| Group { name:name.into() }).collect::<Vec<_>>())));
    for backwards in [false,true] {
        p.invoke_focus_search();
        assert!(!p.get_group_focus_visible());
        let expected = if backwards { [2,1,0] } else { [0,1,2] };
        for index in expected {
            tab(backwards);
            assert_eq!(p.get_current_group(),index);
            assert!(!p.get_search_input_focused(),"Tab into a group must leave the search field");
            assert!(p.get_group_focus_visible(),"even the already selected group must show keyboard focus");
        }
        let count = switched.borrow().len();
        tab(backwards);
        assert!(p.get_search_input_focused(),"the end of the group list must wrap back to search");
        assert!(!p.get_group_focus_visible());
        assert_eq!(switched.borrow().len(),count,"returning to search must not switch groups again");
    }
    assert_eq!(*switched.borrow(),vec![0,1,2,2,1,0]);
    p.set_groups(ModelRc::new(VecModel::from(vec![Group { name:"唯一分组".into() }])));
    tab(false); assert_eq!(p.get_current_group(),0); assert!(!p.get_search_input_focused());
    tab(false); assert!(p.get_search_input_focused());
    p.set_groups(ModelRc::new(VecModel::from(Vec::<Group>::new())));
    for backwards in [false,true] { tab(backwards); assert!(p.get_search_input_focused()); }
    let count = switched.borrow().len();
    p.set_group_modal_open(true); p.invoke_cycle_focus(false);
    assert_eq!(switched.borrow().len(),count,"group naming must not enter the Tab group cycle");
    p.set_group_modal_open(false); p.set_drag_insert(0); p.invoke_cycle_focus(true);
    assert_eq!(switched.borrow().len(),count,"Tab must not change the drop target during a drag");
    p.set_drag_insert(-1); p.invoke_focus_search();
    p.set_groups(ModelRc::new(VecModel::from((0..20).map(|index| Group { name:format!("分组{index:02}").into() }).collect::<Vec<_>>())));
    tab(true); assert_eq!(p.get_current_group(),19);
    render(); slint::platform::update_timers_and_animations();
    let pixels = render();
    assert!((120..126).flat_map(|y| (6..428).map(move |x| y * 520 + x))
        .filter(|index| { let c=pixels[*index]; c.r==27 && c.g==59 && c.b==111 }).count() > 12,
        "Tab to an off-screen group must scroll its active underline into view");
    tab(false); assert!(p.get_search_input_focused());
    let edits = Rc::new(RefCell::new(Vec::new()));
    let observed = edits.clone();
    p.on_search_changed(move |s| observed.borrow_mut().push(s.to_string()));
    for round in 0..20 {
        p.set_query("".into());
        p.hide().unwrap(); p.show().unwrap();
        p.window().dispatch_event(WindowEvent::WindowActiveChanged(true));
        assert!(p.get_search_input_focused(), "reopening must focus the real input, round {round}");
        let count = edits.borrow().len();
        p.window().dispatch_event(WindowEvent::internal(InternalKeyEvent {
            event_type: KeyEventType::UpdateComposition,
            preedit_text: "hanxing".into(), preedit_selection: Some(7..7), ..Default::default()
        }));
        assert!(p.get_query().is_empty(), "uncommitted pinyin must not enter the query");
        assert_eq!(edits.borrow().len(), count, "preedit must not rebuild search results");
        let mut committed = KeyEvent::default();
        committed.text = "寒星".into();
        p.window().dispatch_event(WindowEvent::internal(InternalKeyEvent {
            event_type: KeyEventType::CommitComposition,
            key_event: committed, ..Default::default()
        }));
        assert_eq!(p.get_query().as_str(), "寒星");
        assert_eq!(edits.borrow().len(), count + 1);
        assert_eq!(edits.borrow().last().unwrap(), "寒星");
    }
}

#[test]
fn search_tab_cycles_matches_preserves_query_and_launch_target() {
    use super::*;
    use slint::platform::{Platform, PlatformError, WindowAdapter, WindowEvent, Key};
    use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
    struct TestPlatform(Rc<MinimalSoftwareWindow>);
    impl Platform for TestPlatform {
        fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> { Ok(self.0.clone()) }
    }
    let _view_guard = VIEW_TEST_LOCK.lock().unwrap();
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(window.clone()))).unwrap();
    let p = MainPanel::new().unwrap();
    icon_scaling::bind(p.global::<zaplaunch_ui::IconRenderer>());
    let mut config = AppConfig::default();
    for g in &mut config.groups { g.buttons.clear(); }
    config.groups[0].buttons = (0..60).map(|index| ButtonConfig {
        label:format!("match{index}"),target:format!("fixture-{index}.exe"),icon:"文件夹".into(),..empty_button()
    }).collect();
    let weak = p.as_weak();
    p.on_cycle_results(move |backwards| { if let Some(p)=weak.upgrade() { cycle_search_result(&p,backwards); } });
    let switched = Rc::new(RefCell::new(0)); let calls=switched.clone();
    p.on_group_clicked(move |_| *calls.borrow_mut() += 1);
    let launched = Rc::new(RefCell::new(Vec::new())); let calls=launched.clone();
    p.on_search_accepted(move || { if let Some(hit)=hit_at(SEL_IDX.load(Ordering::SeqCst)) { calls.borrow_mut().push(hit); } });
    let weak = p.as_weak();
    p.on_esc_clicked(move || { weak.upgrade().unwrap().invoke_focus_search(); });
    let tab = |backwards| {
        if backwards { p.window().dispatch_event(WindowEvent::KeyPressed { text:Key::Shift.into() }); }
        p.window().dispatch_event(WindowEvent::KeyPressed { text:Key::Tab.into() });
        p.window().dispatch_event(WindowEvent::KeyReleased { text:Key::Tab.into() });
        if backwards { p.window().dispatch_event(WindowEvent::KeyReleased { text:Key::Shift.into() }); }
    };
    set_search_query("match"); p.set_query("match".into()); refresh_view(&p,&config);
    p.show().unwrap(); p.invoke_focus_search();
    let rows=p.get_rows();
    for index in 0..60 {
        tab(false);
        assert_eq!(hit_at(SEL_IDX.load(Ordering::SeqCst)),Some((0,index)));
        let selected:Vec<_> = (0..60).filter(|i| p.get_rows().row_data(i/7).unwrap().row_data(i%7).unwrap().sel).collect();
        assert_eq!(selected,vec![index],"moving selection must clear the previous tile");
        assert!(!p.get_search_input_focused());
        assert_eq!(p.get_query().as_str(),"match"); assert_eq!(current_search_trimmed(),"match");
        assert_eq!(p.get_rows(),rows,"Tab must only patch selection, not rebuild search models");
    }
    tab(false); assert_eq!(SEL_IDX.load(Ordering::SeqCst),0,"last result wraps to first");
    tab(true); assert_eq!(SEL_IDX.load(Ordering::SeqCst),59,"reverse wraps to last");
    assert_eq!(p.get_run_hint().as_str(),"启动 match59");
    window.set_size(slint::PhysicalSize::new(520,(172.0+p.get_grid_height()) as u32));
    let height=(172.0+p.get_grid_height()) as usize;
    let mut pixels=vec![slint::Rgb8Pixel::default();520*height];
    p.window().request_redraw(); window.draw_if_needed(|r| { r.render(&mut pixels,520); });
    assert!((height-44-82..height-44).flat_map(|y| (12..508).map(move |x| y*520+x))
        .any(|index| { let c=pixels[index]; c.r==27 && c.g==59 && c.b==111 }),"off-screen result must scroll into view");
    p.window().dispatch_event(WindowEvent::KeyPressed { text:Key::Return.into() });
    assert_eq!(*launched.borrow(),vec![(0,59)],"Enter must resolve the selected result instead of the first hit");
    p.window().dispatch_event(WindowEvent::KeyPressed { text:Key::Escape.into() });
    assert!(p.get_search_input_focused()); assert_eq!(p.get_query().as_str(),"match");
    set_search_query(""); p.set_query("".into()); refresh_view(&p,&config);
    assert_eq!(SEL_IDX.load(Ordering::SeqCst),0,"clearing search must not reuse a result index in the full group");
    assert!(!p.get_searching());
    set_search_query("match1"); p.set_query("match1".into()); refresh_view(&p,&config);
    assert_eq!(SEL_IDX.load(Ordering::SeqCst),0,"new query resets to first hit");
    tab(false); tab(false);
    let selected=hit_at(SEL_IDX.load(Ordering::SeqCst));
    refresh_view(&p,&config);
    assert_eq!(hit_at(SEL_IDX.load(Ordering::SeqCst)),selected,"background refresh must preserve selection");
    assert_eq!(p.get_rows(),rows);
    p.invoke_focus_search(); set_search_query("match59"); p.set_query("match59".into()); refresh_view(&p,&config);
    for backwards in [false,false,true] { tab(backwards); assert_eq!(hit_at(SEL_IDX.load(Ordering::SeqCst)),Some((0,59))); }
    p.invoke_focus_search(); set_search_query("absent"); p.set_query("absent".into()); refresh_view(&p,&config);
    assert_eq!(p.get_hit_count(),0);
    for backwards in [false,true] { tab(backwards); assert!(p.get_search_input_focused()); }
    assert_eq!(*switched.borrow(),0,"search Tab must never switch groups");
}

#[test]
fn dropping_into_visible_panel_updates_without_reopening() {
    let _view_guard = VIEW_TEST_LOCK.lock().unwrap();
    use super::*;
    use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
    use slint::platform::{Platform, PlatformError, WindowAdapter};

    struct TestPlatform;
    impl Platform for TestPlatform {
        fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
            Ok(MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer))
        }
    }
    struct Cleanup(std::path::PathBuf, Option<std::ffi::OsString>, bool);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            if let Some(value) = self.1.take() { std::env::set_var(zaplaunch_core::CONFIG_DIR_ENV, value); }
            else { std::env::remove_var(zaplaunch_core::CONFIG_DIR_ENV); }
            PANEL_VISIBLE.store(self.2, Ordering::SeqCst);
            let base = std::env::temp_dir().canonicalize().unwrap();
            if self.0.parent() == Some(base.as_path()) { let _ = std::fs::remove_dir_all(&self.0); }
        }
    }
    let base = std::env::temp_dir().canonicalize().unwrap();
    let dir = base.join(format!("zaplaunch-drop-{}-{}", std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
    std::fs::create_dir(&dir).unwrap();
    let _cleanup = Cleanup(dir.clone(), std::env::var_os(zaplaunch_core::CONFIG_DIR_ENV),
        PANEL_VISIBLE.load(Ordering::SeqCst));
    std::env::set_var(zaplaunch_core::CONFIG_DIR_ENV, &dir);
    let sample = dir.join("drop-test.lnk");
    std::fs::write(&sample, "drop fixture").unwrap();
    let path = sample.to_string_lossy().to_string();
    ICON_CACHE.with(|cache| { cache.borrow_mut().insert(path.replace('/', "\\").to_lowercase(), Some(slint::Image::default())); });
    slint::platform::set_platform(Box::new(TestPlatform)).unwrap();
    let panel = MainPanel::new().unwrap();
    panel.show().unwrap();
    PANEL_VISIBLE.store(true, Ordering::SeqCst);
    let mut config = AppConfig::default();
    config.sound_enabled = false;
    config.follow_mouse = true;
    for group in &mut config.groups { group.buttons.clear(); }
    let app = Rc::new(RefCell::new(config));
    let host = Host {
        panel: panel.as_weak(), app: app.clone(),
        settings: Rc::new(RefCell::new(None)), about: Rc::new(RefCell::new(None)),
        props: Rc::new(RefCell::new(None)), props_ctx: Rc::new(RefCell::new(PropsEdit::default())),
    };
    let generation = VISIBILITY_GENERATION.load(Ordering::SeqCst);
    let groups = app.borrow().groups.iter().map(|g| g.name.clone()).collect();
    host.finish_drop(vec![button_from_path(&path).unwrap()], 0, groups, 0, generation); wait_config();
    assert_eq!(VISIBILITY_GENERATION.load(Ordering::SeqCst), generation, "drop must not re-enter mouse-follow popup positioning");
    assert!(PANEL_VISIBLE.load(Ordering::SeqCst));
    assert_eq!(app.borrow().groups[0].buttons[0].target, path);
    assert_eq!(panel.get_rows().row_data(0).unwrap().row_data(0).unwrap().label, "drop-test");
    let saved = zaplaunch_core::load_or_init_from(&dir);
    assert!(!saved.read_only);
    assert_eq!(saved.config.groups[0].buttons[0].target, path);

    // 元数据检查在实际资产线程执行；即使拒绝项存在也必须返回完整批次。
    let worker = asset_worker().unwrap();
    worker.requests.send(AssetRequest::Drop(vec![path.clone(), dir.join("missing").to_string_lossy().into_owned()], vec!["fixture".into()], 0, generation)).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let result = worker.results.lock().unwrap().try_recv();
        if let Ok(AssetResult::Drop(buttons, rejected, groups, gi, returned_generation)) = result {
            assert_eq!(buttons[0].target, path);
            assert_eq!((rejected, groups, gi, returned_generation), (1, vec!["fixture".into()], 0, generation));
            break;
        }
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }

    // Use the real property callbacks against an isolated config, without launching any targets.
    {
        let mut c = app.borrow_mut();
        for label in ["zulu", "alpha"] {
            let mut b = c.groups[0].buttons[0].clone();
            b.label = label.into(); b.window_state = "maximized".into();
            b.hotkey = if label == "zulu" { "Ctrl+Alt+J".into() } else { String::new() };
            b.launch_on_startup = true; b.run_count = 8;
            c.groups[0].buttons.push(b);
        }
        c.tile_order = "name".into();
    }
    let ctx = Rc::new(RefCell::new(PropsEdit::default()));
    let slot = Rc::new(RefCell::new(None));
    let props = build_props(app.clone(), panel.as_weak(), ctx.clone(), slot);
    assert!(fill_props(&props, &app.borrow(), &ctx, 0, 1));
    assert_eq!(props.get_nav_position(), 2);
    app.borrow_mut().groups[0].buttons[1].run_count += 1;
    app.borrow_mut().groups[0].buttons[2].run_count += 1;
    assert!(props_unchanged(&app.borrow(), &ctx.borrow()));
    props.set_button_label("Beta".into());
    props.invoke_navigate(-1); wait_config();
    assert_eq!(app.borrow().groups[0].buttons[1].label, "Beta");
    assert_eq!(app.borrow().groups[0].buttons[1].run_count, 9);
    assert_eq!(zaplaunch_core::load_or_init_from(&dir).config.groups[0].buttons[1].run_count, 9);
    assert_eq!(ctx.borrow().original.as_ref().unwrap().1, 0, "navigation keeps the initial display order after renaming");
    props.invoke_navigate(1); wait_config();
    props.set_button_params("draft-only".into());
    props.invoke_duplicate();
    assert!(props.get_copying()); assert!(!props.get_editing());
    assert_eq!(props.get_nav_count(), 0);
    assert_eq!(app.borrow().groups[0].buttons.len(), 3, "copy is only a draft until save");
    assert_eq!(props.get_window_state(), 2);
    assert!(props.get_button_hotkey().is_empty()); assert!(!props.get_launch_on_startup());
    assert!(save_props_wait(&props, &app, &ctx, &panel.as_weak()));
    let c = app.borrow();
    let copy = &c.groups[0].buttons[2];
    assert_eq!(copy.label, "Beta 副本"); assert_eq!(copy.params, "draft-only");
    assert_eq!(copy.run_count, 0); assert_eq!(copy.window_state, "maximized");
    assert!(c.groups[0].buttons[1].params.is_empty(), "copy does not overwrite the original draft");
    assert_eq!(zaplaunch_core::load_or_init_from(&dir).config.groups, c.groups);
    let moved_button = copy.clone();
    drop(c);
    assert!(fill_props(&props,&app.borrow(),&ctx,0,2));
    let destination = app.borrow().groups[1].name.clone();
    props.set_group_name(destination.into()); props.set_group_index(1);
    let generation = VISIBILITY_GENERATION.load(Ordering::SeqCst);
    props.invoke_navigate(-1); wait_config();
    assert_eq!(app.borrow().groups[0].buttons.len(),4,"pending move must not be autosaved by navigation");
    assert!(save_props_wait(&props,&app,&ctx,&panel.as_weak()));
    assert_eq!(app.borrow().groups[0].buttons.len(),3);
    assert_eq!(app.borrow().groups[1].buttons,vec![moved_button.clone()]);
    assert_eq!(panel.get_current_group(),1);
    assert_eq!(VISIBILITY_GENERATION.load(Ordering::SeqCst),generation,"moving a button must not reopen/reposition the panel");
    assert_eq!(ctx.borrow().group,1);
    assert!(fill_new_props(&props,&app.borrow(),&ctx,0,"新条目"));
    props.set_button_target(path.clone().into());
    props.set_group_name(app.borrow().groups[2].name.as_str().into()); props.set_group_index(2);
    assert!(save_props_wait(&props,&app,&ctx,&panel.as_weak()));
    assert_eq!(app.borrow().groups[0].buttons.len(),3);
    assert_eq!(app.borrow().groups[2].buttons[0].label,"新条目");
    assert!(fill_props(&props,&app.borrow(),&ctx,0,2));
    props.set_group_name(app.borrow().groups[1].name.as_str().into()); props.set_group_index(1);
    props.invoke_duplicate();
    assert_eq!(app.borrow().groups[1].buttons.len(),1);
    assert!(save_props_wait(&props,&app,&ctx,&panel.as_weak()));
    assert_eq!(app.borrow().groups[0].buttons.len(),3,"copy into another group must leave the source in place");
    assert_eq!(app.borrow().groups[1].buttons[1].label,"alpha 副本");
    assert!(fill_props(&props,&app.borrow(),&ctx,1,0));
    let before_cancel = app.borrow().groups.clone();
    props.set_group_name(app.borrow().groups[0].name.as_str().into()); props.set_group_index(0);
    props.invoke_cancel_clicked();
    assert_eq!(app.borrow().groups,before_cancel);
    assert_eq!(zaplaunch_core::load_or_init_from(&dir).config.groups,before_cancel);
    app.borrow_mut().groups[2].name = "其他入口改名".into();
    assert!(!props_unchanged(&app.borrow(),&ctx.borrow()),"stale group choices must be rejected");
    let duplicate_name = app.borrow().groups[0].name.clone();
    app.borrow_mut().groups[2].name = duplicate_name;
    assert!(fill_props(&props,&app.borrow(),&ctx,1,0));
    props.set_group_name(app.borrow().groups[2].name.as_str().into()); props.set_group_index(2);
    assert!(save_props_wait(&props,&app,&ctx,&panel.as_weak()));
    assert_eq!(app.borrow().groups[0].buttons.len(),3,"duplicate group names in imported configs must not select the first match");
    assert_eq!(app.borrow().groups[2].buttons[1],moved_button);
    assert!(fill_new_props(&props, &app.borrow(), &ctx, 0, "submitted"));
    props.set_button_target(path.clone().into());
    let count = app.borrow().groups[0].buttons.len();
    let closed = Rc::new(std::cell::Cell::new(None)); let done = closed.clone();
    save_props(&props, &app, &ctx, &panel.as_weak(), move |ok| done.set(Some(ok)));
    props.set_button_label("typed while saving".into());
    wait_config();
    assert_eq!(closed.get(), Some(false), "newer draft must stay open");
    assert_eq!(app.borrow().groups[0].buttons[count].label, "submitted");
    assert!(save_props_wait(&props, &app, &ctx, &panel.as_weak()));
    assert_eq!(app.borrow().groups[0].buttons.len(), count + 1, "second save updates the newly committed button");
    assert_eq!(app.borrow().groups[0].buttons[count].label, "typed while saving");

    assert!(fill_props(&props, &app.borrow(), &ctx, 0, count));
    props.set_group_index(1);
    panel.set_current_group(0);
    let moved_button = app.borrow().groups[0].buttons[count].clone();
    let destination_index = app.borrow().groups[1].buttons.len();
    PENDING_LAUNCHES.with(|pending| {
        pending.borrow_mut().insert(u64::MAX, PendingLaunch {
            source: LaunchSource::Button(0, count),
            target: moved_button.target, params: moved_button.params,
            dir: moved_button.work_dir, label: moved_button.label, window_state: moved_button.window_state,
            hide_generation: None, hist_on: false, hist_max: 20, sound_on: false, network: false, timed_out: false, timeout: Timer::default(),
        });
    });
    save_props(&props, &app, &ctx, &panel.as_weak(), |_| {});
    panel.set_current_group(2);
    set_search_query("new query during move");
    panel.set_query("new query during move".into());
    wait_config();
    assert_eq!(panel.get_current_group(), 2);
    assert_eq!(panel.get_query(), "new query during move");
    let request = PENDING_LAUNCHES.with(|pending| pending.borrow_mut().remove(&u64::MAX).unwrap());
    assert!(matches!(request.source, LaunchSource::Button(1, index) if index == destination_index));
    apply_launch_success(&mut app.borrow_mut(), &request);
    assert_eq!(app.borrow().groups[1].buttons[destination_index].run_count, 1);
    assert!(zaplaunch_core::load_or_init_from(&dir).config.groups[1].buttons.iter()
        .any(|button| button.label == "typed while saving"));
    set_search_query("");
    panel.set_query("".into());

    let group_names = || app.borrow().groups.iter().map(|g| g.name.clone()).collect();
    let count_before_drop = app.borrow().groups[0].buttons.len();
    mark_panel_hidden(&panel);
    let hidden_generation = VISIBILITY_GENERATION.load(Ordering::SeqCst);
    assert_ne!(hidden_generation, generation, "minimize must invalidate delayed show");
    host.finish_drop(vec![button_from_path(&path).unwrap()], 0, group_names(), 0, generation);
    wait_config();
    assert!(!PANEL_VISIBLE.load(Ordering::SeqCst), "late drop must not reopen the panel");
    assert_eq!(VISIBILITY_GENERATION.load(Ordering::SeqCst), hidden_generation);
    assert_eq!(app.borrow().groups[0].buttons.len(), count_before_drop + 1, "hidden drop must still be saved");

    PANEL_VISIBLE.store(true, Ordering::SeqCst);
    VISIBILITY_GENERATION.fetch_add(1, Ordering::SeqCst);
    set_search_query("new search");
    panel.set_query("new search".into());
    host.finish_drop(vec![button_from_path(&path).unwrap()], 0, group_names(), 0, generation);
    wait_config();
    assert_eq!(panel.get_query(), "new search");
    assert_eq!(current_search_trimmed(), "new search");
    assert_eq!(zaplaunch_core::load_or_init_from(&dir).config.groups[0].buttons.len(), count_before_drop + 2);
    set_search_query("");
}

#[test]
fn reopening_props_preserves_draft_and_stale_close_keeps_new_window() {
    use super::*;
    use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
    use slint::platform::{Platform, PlatformError, WindowAdapter, WindowEvent};
    struct TestPlatform;
    impl Platform for TestPlatform {
        fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
            Ok(MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer))
        }
    }
    slint::platform::set_platform(Box::new(TestPlatform)).unwrap();
    let panel = MainPanel::new().unwrap();
    let app = Rc::new(RefCell::new(AppConfig::default()));
    let host = Host {
        panel: panel.as_weak(), app: app.clone(),
        settings: Rc::new(RefCell::new(None)), about: Rc::new(RefCell::new(None)),
        props: Rc::new(RefCell::new(None)), props_ctx: Rc::new(RefCell::new(PropsEdit::default())),
    };
    let old = build_props(app.clone(), panel.as_weak(), host.props_ctx.clone(), host.props.clone());
    *host.props.borrow_mut() = Some(old.clone_strong());
    old.show().unwrap();
    old.set_button_label("unsaved draft".into());
    host.open_new_button();
    assert_eq!(old.get_button_label(), "unsaved draft");
    old.window().dispatch_event(WindowEvent::CloseRequested);
    assert!(!old.window().is_visible());
    assert!(host.props.borrow().is_none());

    let new = PropsWindow::new().unwrap();
    *host.props.borrow_mut() = Some(new.clone_strong());
    new.show().unwrap();
    close_child(&old, host.props.clone());
    assert!(new.window().is_visible());
    assert!(host.props.borrow().is_some());
}

#[test]
fn network_launches_leave_two_slots_for_other_targets() {
    use super::launch_slots_available;
    assert!(zaplaunch_sys::is_network_path(r"\\server\share\app.exe"));
    assert!(zaplaunch_sys::is_network_path(r"\\?\UNC\server\share\app.exe"));
    assert!(!zaplaunch_sys::is_network_path("notepad.exe"));
    assert!(!launch_slots_available(6, 6, true));
    assert!(launch_slots_available(6, 6, false));
    assert!(launch_slots_available(7, 6, false));
    assert!(!launch_slots_available(8, 6, false));
}

#[test]
fn stale_icons_and_startup_queue_are_bounded() {
    use super::*;
    let key = "https://test.invalid";
    ICON_REQUESTS.with(|r| { r.borrow_mut().insert(key.into(), 42); });
    assert!(!complete_icon(key, 41));
    assert!(complete_icon(key, 42)); assert!(!complete_icon(key, 42));
    assert_eq!(file_icon_key(r"C:\Test.PNG", true).as_deref(), Some(r"image:c:\test.png"));
    assert_eq!(native_icon_key("www.hxlive.cn/path", "").as_deref(), Some("https://www.hxlive.cn"));
    let mut c = AppConfig::default();
    c.groups[0].buttons[0].launch_on_startup = true;
    c.groups[1].buttons[1].launch_on_startup = true;
    let original = c.groups.clone(); let history = c.history.len();
    assert!(startup_buttons(&c, false).is_empty());
    let buttons = startup_buttons(&c, true);
    let mut seen = Vec::new();
    let failed = run_startup_buttons(&buttons, |b| { seen.push(b.label.clone()); seen.len() != 1 });
    assert_eq!(seen, buttons.iter().map(|b| b.label.clone()).collect::<Vec<_>>());
    assert_eq!(failed, vec![buttons[0].label.clone()]);
    assert_eq!(c.groups, original); assert_eq!(c.history.len(), history);
}

#[test]
fn grid_drag_chooses_insertion_boundary() {
    assert_eq!(grid_insert_at(0, 0, 10.0, 10.0, 7, 12, 69.0, 82.0), Some(0));
    assert_eq!(grid_insert_at(0, 0, 60.0, 10.0, 7, 12, 69.0, 82.0), Some(1));
    assert_eq!(grid_insert_at(0, 0, 80.0, 90.0, 7, 12, 69.0, 82.0), Some(8));
    assert_eq!(grid_insert_at(0, 0, 510.0, 10.0, 7, 12, 69.0, 82.0), None);
}

#[test]
fn button_hotkeys_disable_only_duplicates_and_panel_conflicts() {
    let mut c = AppConfig::default();
    c.groups[0].buttons[0].hotkey = "Ctrl+Alt+N".into();
    assert_eq!(button_hotkeys(&c).0.len(), 1);
    c.groups[0].buttons[1].hotkey = "Ctrl + Alt + N".into();
    assert_eq!(button_hotkeys(&c).0.len(), 1);
    assert_eq!(button_hotkeys(&c).1, vec![(0, 1)]);
    c.groups[0].buttons[1].hotkey = c.hotkey.clone();
    assert_eq!(button_hotkeys(&c).1, vec![(0, 1)]);
    c.groups[0].buttons[2].hotkey = "Shift".into();
    assert_eq!(button_hotkeys(&c).1, vec![(0, 1), (0, 2)]);
    c.groups[0].buttons[2].hotkey.clear();
    c.groups[0].buttons[1].hotkey = "Ctrl+Alt+M".into();
    c.remove_button(0, 0);
    assert_eq!(button_hotkeys(&c).0[0].0, 0);
    assert_eq!(button_hotkeys(&c).0[0].1, 0);
}

#[test]
fn late_launch_result_does_not_count_a_replaced_button() {
    let mut c = AppConfig::default();
    let b = &c.groups[0].buttons[0];
    let request = PendingLaunch {
        window_state: "normal".into(), source: LaunchSource::Button(0, 0), target: b.target.clone(), params: b.params.clone(),
        dir: b.work_dir.clone(), label: b.label.clone(), hide_generation: Some(0),
        hist_on: true, hist_max: 20, sound_on: false, network: false, timed_out: false, timeout: slint::Timer::default(),
    };
    c.groups[0].buttons[0].target = "different.exe".into();
    apply_launch_success(&mut c, &request);
    assert_eq!(c.groups[0].buttons[0].run_count, 0);
    assert_eq!(c.history[0].target, request.target);
}

#[test]
fn grid_navigation_clamps_at_edges_and_last_tile() {
    // 7 列、12 项：第二行最后一个可视格是 11，不能向右跳到不存在的 12。
    assert_eq!(next_grid_selection(0, 12, 7, 0), None);
    assert_eq!(next_grid_selection(0, 12, 7, 1), Some(1));
    assert_eq!(next_grid_selection(0, 12, 7, 2), None);
    assert_eq!(next_grid_selection(0, 12, 7, 3), Some(7));
    assert_eq!(next_grid_selection(6, 12, 7, 1), None);
    assert_eq!(next_grid_selection(7, 12, 7, 2), Some(0));
    assert_eq!(next_grid_selection(11, 12, 7, 1), None);
    assert_eq!(next_grid_selection(8, 12, 7, 3), None);
    assert_eq!(next_grid_selection(0, 0, 7, 1), None);
    assert_eq!(next_grid_selection(12, 12, 7, 0), None);
}

#[test]
fn group_deletion_preserves_current_group_identity() {
    assert_eq!(group_selection_after_delete(1, 0, 2), 0);
    assert_eq!(group_selection_after_delete(1, 1, 2), 1);
    assert_eq!(group_selection_after_delete(2, 2, 2), 1);
    assert_eq!(group_selection_after_delete(0, 1, 2), 0);
}


#[test]
fn unchanged_website_origins_do_not_enqueue_another_cleanup() {
    use super::*;
    let queue = WebsiteQueue::default();
    let keys = ["fixture".to_owned()].into_iter().collect::<std::collections::HashSet<_>>();
    queue.send(WebsiteRequest::Keep(keys.clone())).unwrap();
    assert!(matches!(queue.recv(), Some(WebsiteRequest::Keep(_))));
    queue.send(WebsiteRequest::Keep(keys)).unwrap();
    assert!(queue.0.0.lock().unwrap().pending.is_empty());
    queue.send(WebsiteRequest::Keep(Default::default())).unwrap();
    assert!(matches!(queue.recv(), Some(WebsiteRequest::Keep(keys)) if keys.is_empty()));
}

#[test]
fn in_flight_shortcut_is_not_queued_again_on_cache_miss() {
    use super::*;
    let key = "c:\\fixture.lnk";
    PENDING_SHORTCUTS.with(|pending| { pending.borrow_mut().insert(key.to_owned()); });
    assert_eq!(cached_shortcut_target(key), None);
    LNK_TARGET_CACHE.with(|cache| assert!(!cache.borrow().entries.contains_key(key)));
    PENDING_SHORTCUTS.with(|pending| assert!(pending.borrow_mut().remove(key)));
}
