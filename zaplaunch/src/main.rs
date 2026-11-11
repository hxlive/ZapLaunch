// zaplaunch：装配入口。
// 职责边界：只做「配置数据 → UI 模型」的装配和窗口/启动动作的宿主侧处理，
// 不写任何布局数值（布局数值一律在 crates/zaplaunch-ui/ui/*.slint，见 app/docs/设计规格.md）。
//
// 四个窗口：MainPanel（主面板 520×374）/ SettingsWindow（设置 480×640，四分组）
//           / PropsWindow（按钮属性 420×506，展开 420×622）/ AboutWindow（关于 380×264）。
// 子窗口按需惰性创建，关闭时在下一轮事件循环安全释放渲染缓冲。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use slint::{Color, ComponentHandle, Model, ModelRc, Timer, TimerMode, VecModel, Weak};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;
use zaplaunch_core::{AppConfig, ButtonConfig, HistoryEntry};
use zaplaunch_sys::tray::TrayEvent;
use zaplaunch_ui::{AboutWindow, Group, HistoryRow, MainPanel, Palette, PropsWindow, SettingsWindow, Tile};

mod icons;
mod icon_scaling;
mod icon_disk_cache;
mod web_icons;
mod config_writer;

/// 网格列数默认值（配置 grid_columns 缺省 / 越界时用；合法范围 6~8）。
/// 7 是能放下最长按钮名的最大列数，见 zaplaunch_core::clamp_grid_columns 注释。
const DEFAULT_COLS: usize = 7;
/// 当前生效的列数（由配置 grid_columns 推导，每次刷新网格时同步）。
///
/// ⚠️ 点击换算（row × cols + col）必须与分行用的是同一个值，否则会「点 A 开 B」。
/// 放原子量是为了让点击回调不必再借用 RefCell（那些回调里常常已经借过了）。
static CUR_COLS: AtomicUsize = AtomicUsize::new(DEFAULT_COLS);

/// 面板固定宽度（= 设计规格 520，高度随 2~6 行动态自适应）。
const PANEL_W: f32 = 520.0;

/// 面板离屏幕边缘的最小间距（呼出定位用，设计规格 7.12 的「边缘内缩」）。
const EDGE_MARGIN: i32 = 8;

/// ⑬ 运行历史浮层最多显示几条（config 里默认留 20 条，浮层只露前 5 条）。
const HISTORY_SHOWN: usize = 5;
/// 图标与快捷方式缓存设置上限，避免长期使用时无界增长。
const PATH_CACHE_LIMIT: usize = 512;


// ---------------------------------------------------------------------------
// 延迟执行与 UI 线程局部状态（Slint 定时器防呆封装）
// ---------------------------------------------------------------------------

// 一次性定时器必须**活到触发为止**：`Timer` 一 drop，定时器就取消
// （换 crontab 想一想就明白 —— 这里掉过坑，表现为「N 毫秒后什么都没发生」）。
//
// ⚠️ 2026-10-22 实测：`Timer::single_shot(..)` 是**关联函数、不返回句柄**，
// 它内部靠 `current_timers()` 注册，时机不对（事件循环未起 / 正跑在别的定时器回调里）
// 会**静默什么都不做**，连报错都没有。搜索框抢焦点、子窗口圆角两处都栽在这。
// 改用「Timer 实例 + 全局槽持有」，注册时机就与调用点无关了。
//
// `Timer` 及各类 UI 组件句柄含 `PhantomData<*mut ()>`，既非 Send 也非 Sync，
// 统一挂在 thread_local 中，仅在 UI 线程访问，无需 `unsafe impl Send` 包装。
thread_local! {
    static DEFERRED: RefCell<Vec<Timer>> = const { RefCell::new(Vec::new()) };
    static ESC_TIMER: RefCell<Option<Rc<Timer>>> = const { RefCell::new(None) };
    static BLUR_TIMER: RefCell<Option<Rc<Timer>>> = const { RefCell::new(None) };
    /// 唯一的宿主实例（仅在 UI 线程使用）。
    static HOST: RefCell<Option<Host>> = const { RefCell::new(None) };
    /// ③ 搜索词。UI 与 Rust 的唯一事实来源：输入框改词 → 写这里 → 重算网格；Rust 清词 → set_query("")。
    static SEARCH: RefCell<String> = const { RefCell::new(String::new()) };
    /// 网格第 n 格 → 真实按钮坐标 `(群组下标, 群内下标)`。
    static HIT_MAP: RefCell<Vec<(usize, usize)>> = const { RefCell::new(Vec::new()) };
    /// 文件图标与失败结果共用有界缓存；命中只更新使用次序，不重复提取。
    static ICON_CACHE: RefCell<AssetCache<Option<slint::Image>>> = RefCell::new(AssetCache::default());
    static ICON_REQUESTS: RefCell<std::collections::HashMap<String, usize>> = RefCell::new(std::collections::HashMap::new());
    static CACHE_REBUILD: RefCell<Option<CacheRebuild>> = const { RefCell::new(None) };
    static CACHE_REBUILD_STATUS: RefCell<String> = RefCell::new("按当前启动项重新获取图标".into());
    /// `.lnk` 快捷方式真实目标路径缓存：供智能搜索匹配 `.lnk` 背后的 `.exe` 路径与程序名。
    static PENDING_SHORTCUTS: RefCell<std::collections::HashSet<String>> = RefCell::new(std::collections::HashSet::new());
    static LNK_TARGET_CACHE: RefCell<AssetCache<Option<String>>> = RefCell::new(AssetCache::default());
    static PENDING_LAUNCHES: RefCell<std::collections::HashMap<u64, PendingLaunch>> =
        RefCell::new(std::collections::HashMap::new());
}

static CONFIG_READ_ONLY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static VISIBILITY_GENERATION: AtomicUsize = AtomicUsize::new(0);
static NEXT_HOTKEY_REQUEST: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);
static NEXT_LAUNCH_REQUEST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
struct PendingHotkey {
    id: u32,
    text: String,
}
static PENDING_HOTKEY: Mutex<Option<PendingHotkey>> = Mutex::new(None);

type IconPixels = Option<(u32, u32, Vec<u8>)>;
struct AssetCache<T> {
    entries: std::collections::HashMap<String, (usize, T)>,
    clock: usize,
}
impl<T> Default for AssetCache<T> {
    fn default() -> Self { Self { entries: std::collections::HashMap::new(), clock: 0 } }
}
impl<T: Clone> AssetCache<T> {
    fn get(&mut self, key: &str) -> Option<T> {
        let entry = self.entries.get_mut(key)?;
        self.clock += 1;
        entry.0 = self.clock;
        Some(entry.1.clone())
    }
    fn insert(&mut self, key: String, value: T) {
        if self.entries.len() >= PATH_CACHE_LIMIT && !self.entries.contains_key(&key) {
            if let Some(oldest) = self.entries.iter().min_by_key(|(_, (age, _))| age).map(|(key, _)| key.clone()) {
                self.entries.remove(&oldest);
            }
        }
        self.clock += 1;
        self.entries.insert(key, (self.clock, value));
    }
}
static ASSET_NOTIFY_PENDING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
fn notify_asset_ready() {
    if !ASSET_NOTIFY_PENDING.swap(true, Ordering::SeqCst) { push_tray_event(TrayEvent::AssetReady); }
}
static NEXT_ICON_REQUEST: AtomicUsize = AtomicUsize::new(1);
enum AssetRequest { Icon(String, usize, bool), Clean(std::path::PathBuf, Vec<String>), Drop(Vec<String>, Vec<String>, usize, usize), Import(String), Export(AppConfig, String) }
enum WebsiteRequest { Icon(String, usize, bool), Keep(std::collections::HashSet<String>) }

#[derive(Default)]
struct WebsiteState {
    pending: std::collections::VecDeque<WebsiteRequest>,
    keep: Option<std::collections::HashSet<String>>,
    active: std::collections::HashSet<String>,
}
#[derive(Clone, Default)]
struct WebsiteQueue(std::sync::Arc<(Mutex<WebsiteState>, std::sync::Condvar)>);
impl WebsiteQueue {
    fn send(&self, request: WebsiteRequest) -> Result<(), ()> {
        let (state, wake) = &*self.0;
        let mut state = state.lock().map_err(|_| ())?;
        if let WebsiteRequest::Keep(keys) = &request {
            if state.keep.as_ref() == Some(keys) { return Ok(()); }
            state.keep = Some(keys.clone());
        }
        enqueue_website(&mut state.pending, request);
        wake.notify_one();
        Ok(())
    }
    fn recv(&self) -> Option<WebsiteRequest> {
        let (state, wake) = &*self.0;
        let mut state = state.lock().ok()?;
        loop {
            if let Some(index) = state.pending.iter().position(|r| match r {
                WebsiteRequest::Icon(key, ..) => !state.active.contains(key),
                WebsiteRequest::Keep(_) => true,
            }) {
                let request = state.pending.remove(index)?;
                if let WebsiteRequest::Icon(key, ..) = &request { state.active.insert(key.clone()); }
                return Some(request);
            }
            state = wake.wait(state).ok()?;
        }
    }
    fn finished(&self, origin: &str) {
        if let Ok(mut state) = self.0.0.lock() { state.active.remove(origin); self.0.1.notify_all(); }
    }
    fn obsolete(&self, origin: &str) -> bool {
        self.0.0.lock().ok().and_then(|s| s.keep.as_ref().map(|keys| !keys.contains(&web_icons::cache_file_name(origin)))).unwrap_or(false)
    }
}
fn enqueue_website(queue: &mut std::collections::VecDeque<WebsiteRequest>, request: WebsiteRequest) {
    queue.retain(|old| !match (&request, old) {
        (WebsiteRequest::Icon(key, ..), WebsiteRequest::Icon(previous, ..)) => key == previous,
        (WebsiteRequest::Keep(_), WebsiteRequest::Keep(_)) => true,
        _ => false,
    });
    if matches!(request, WebsiteRequest::Keep(_)) { queue.push_front(request); }
    else { queue.push_back(request); }
}

enum AssetResult { Icon(String, usize, IconPixels, bool), Shortcut(String, Option<String>), CacheCleaned(usize), Drop(Vec<ButtonConfig>, usize, Vec<String>, usize, usize), Import(Result<AppConfig, String>), Export(Result<(), String>) }
static DROP_PENDING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
struct AssetWorker {
    requests: std::sync::mpsc::Sender<AssetRequest>,
    shortcuts: std::sync::mpsc::Sender<String>,
    websites: WebsiteQueue,
    results: Mutex<std::sync::mpsc::Receiver<AssetResult>>,
}
static ASSET_WORKER: OnceLock<Option<AssetWorker>> = OnceLock::new();

fn asset_worker() -> Option<&'static AssetWorker> {
    ASSET_WORKER.get_or_init(|| {
        let (request_tx, request_rx) = std::sync::mpsc::channel::<AssetRequest>();
        let (shortcut_tx, shortcut_rx) = std::sync::mpsc::channel::<String>();
        let (result_tx, result_rx) = std::sync::mpsc::sync_channel(32);
        let web_queue = WebsiteQueue::default();
        #[cfg(test)]
        let cache_root = std::env::temp_dir().join(format!("zaplaunch-worker-cache-{}",std::process::id()));
        #[cfg(not(test))]
        let cache_root = zaplaunch_core::config_dir().join("cache");
        let web_cache = cache_root.join("favicons");
        for index in 0..2 {
            let (queue, web_results, web_cache) = (web_queue.clone(), result_tx.clone(), web_cache.clone());
            if std::thread::Builder::new().name(format!("zaplaunch-web-{index}")).spawn(move || {
                while let Some(request) = queue.recv() {
                    let (origin, id, force) = match request {
                        WebsiteRequest::Icon(origin, id, force) => (origin, id, force),
                        WebsiteRequest::Keep(keys) => { web_icons::prune(&web_cache, Some(&keys)); continue; }
                    };
                    let mut pixels = if queue.obsolete(&origin) { None } else { web_icons::load(&origin, &web_cache, force) };
                    if queue.obsolete(&origin) {
                        pixels = None;
                        let _ = std::fs::remove_file(web_cache.join(web_icons::cache_file_name(&origin)));
                    }
                    let cache_ok = !force || (pixels.is_some() && web_icons::load_file(&web_cache.join(web_icons::cache_file_name(&origin))).as_ref() == pixels.as_ref());
                    queue.finished(&origin);
                    if web_results.send(AssetResult::Icon(origin, id, pixels, cache_ok)).is_err() { break; }
                    notify_asset_ready();
                }
            }).is_err() { return None; }
        }
        let shortcut_results = result_tx.clone();
        if std::thread::Builder::new().name("zaplaunch-shortcuts".into()).spawn(move || {
            for path in shortcut_rx {
                let target = zaplaunch_sys::resolve_shortcut_target(&path);
                if shortcut_results.send(AssetResult::Shortcut(path, target)).is_err() { break; }
                notify_asset_ready();
            }
        }).is_err() { return None; }
        let icon_cache = cache_root.join("native-icons");
        let request_rx = std::sync::Arc::new(Mutex::new(request_rx));
        // 固定两个提取线程，单个慢 Shell 图标不会阻塞所有后续图标。
        for index in 0..2 {
            let request_rx = request_rx.clone();
            let result_tx = result_tx.clone();
            let icon_cache = icon_cache.clone();
            if std::thread::Builder::new().name(format!("zaplaunch-assets-{index}")).spawn(move || loop {
                let request = request_rx.lock().ok().and_then(|rx| rx.recv().ok());
                let Some(request) = request else { break; };
                let result = match request {
                    AssetRequest::Icon(path,id,force) => {
                        let pixels = if let Some(file) = path.strip_prefix("image:") {
                            web_icons::load_file(std::path::Path::new(file))
                        } else { icon_disk_cache::load(&path,&icon_cache,force,|| zaplaunch_sys::extract_file_icon_rgba(&path)) };
                        let cache_ok = !force || path.starts_with("image:") || pixels.as_ref().map(|pixels| icon_disk_cache::matches_pixels(&path,&icon_cache,pixels)).unwrap_or(false);
                        AssetResult::Icon(path,id,pixels,cache_ok)
                    }
                    AssetRequest::Import(path) => AssetResult::Import(zaplaunch_core::import_from(std::path::Path::new(&path)).map_err(|e| e.to_string())),
                    AssetRequest::Export(config, path) => AssetResult::Export(zaplaunch_core::export_to(&config, std::path::Path::new(&path)).map_err(|e| e.to_string())),
                    AssetRequest::Clean(root,keys) => AssetResult::CacheCleaned(clean_icon_cache(&root,&keys)),
                    AssetRequest::Drop(paths, groups, gi, generation) => {
                        let buttons: Vec<_> = paths.iter().filter_map(|p| button_from_path(p)).collect();
                        let rejected = paths.len() - buttons.len();
                        AssetResult::Drop(buttons, rejected, groups, gi, generation)
                    }
                };
                if result_tx.send(result).is_err() { break; }
                notify_asset_ready();
            }).is_err() { return None; }
        }
        Some(AssetWorker { requests: request_tx, shortcuts: shortcut_tx, websites: web_queue, results: Mutex::new(result_rx) })
    }).as_ref()
}

/// 热键「存储格式」→「显示格式」：`Ctrl+Alt+Space` → `Ctrl + Alt + Space`。
///
/// 存储格式是数据契约（写进 config.json，由 `tray::parse_hotkey` 解析），保持无空格；
/// 只在喂给 UI 显示的这一步加空格，让修饰键与主键的关系一眼可辨。
fn fmt_hotkey(s: &str) -> String {
    s.replace("+", " + ")
}

/// 延迟 `ms` 毫秒在 UI 线程执行一次 `cb`；回调跑完自动把自己从槽里摘掉，不堆积。
fn defer(ms: u64, cb: impl FnOnce() + 'static) {
    let t = Timer::default();
    let mut cb = Some(cb);
    t.start(TimerMode::SingleShot, Duration::from_millis(ms), move || {
        if let Some(f) = cb.take() {
            f();
        }
        DEFERRED.with(|d| d.borrow_mut().retain(|x| x.running()));
    });
    DEFERRED.with(|d| d.borrow_mut().push(t));
}

/// 子窗口关闭后释放组件与软件渲染缓冲；不能在其自身回调栈里直接 drop，
/// 因此立即从槽位取走所有权（防止后续快速重开时发生竞态），再在下一轮 UI 事件里析构该实例。
fn close_child<W: ComponentHandle + 'static>(w: &W, slot: Rc<RefCell<Option<W>>>) {
    let _ = w.hide();
    let mut slot = slot.borrow_mut();
    let owns_window = slot.as_ref().is_some_and(|current| std::ptr::eq(current.window(), w.window()));
    if let Some(owned) = owns_window.then(|| slot.take()).flatten() {
        defer(0, move || {
            drop(owned);
        });
    }
}

fn raise_open_props(slot: &RefCell<Option<PropsWindow>>) -> bool {
    let slot = slot.borrow();
    let Some(w) = slot.as_ref().filter(|w| w.window().is_visible()) else { return false; };
    if let Some(hwnd) = hwnd_of_window(w.window()) {
        zaplaunch_sys::restore_window(hwnd);
        zaplaunch_sys::tray::force_foreground(hwnd);
    }
    true
}

fn current_search_trimmed() -> String {
    SEARCH.with(|s| s.borrow().trim().to_string())
}

fn set_search_query(val: &str) {
    SEARCH.with(|s| {
        let mut b = s.borrow_mut();
        b.clear();
        b.push_str(val);
    });
}

// ---------------------------------------------------------------------------
// 窗口句柄与几何
// ---------------------------------------------------------------------------

/// 从任意 Slint 窗口取 Win32 HWND。窗口创建完成前取不到，返回 None。
fn hwnd_of_window(w: &slint::Window) -> Option<isize> {
    // slint::Window::window_handle() → slint::WindowHandle（自身实现 raw_window_handle::HasWindowHandle）
    let handle = w.window_handle(); // 先绑定，否则临时的 WindowHandle 在取 raw 前就被释放
    let raw = handle.window_handle().ok()?;
    match raw.as_raw() {
        RawWindowHandle::Win32(x) => Some(x.hwnd.get()),
        _ => None,
    }
}

fn hwnd_of(panel: &MainPanel) -> Option<isize> {
    hwnd_of_window(panel.window())
}

fn recover_panel_input_method(p: &MainPanel) -> bool {
    if !p.get_text_input_requested() { return false; }
    let Some(hwnd) = hwnd_of(p) else { return false; };
    if !zaplaunch_sys::tray::is_foreground_window(hwnd) { return false; }
    let input_focused = if p.get_group_modal_open() { p.get_group_input_focused() } else { p.get_search_input_focused() };
    if input_focused && zaplaunch_sys::ime_enabled(hwnd) { return false; }
    // 对已聚焦控件重复 focus 是空操作；实际切换一次才能让 winit 重发 IME Enable。
    p.invoke_restore_input_focus();
    true
}

/// request_redraw 只请求一次绘制，不会清除软件渲染器的局部帧缓存。
/// Windows 隐藏/遮挡会丢弃已呈现的像素，但 softbuffer 的 buffer age 不一定反映这一点；
/// 因此显示窗口时必须把整窗标脏，再请求重绘，不能只改尺寸碰运气。
fn request_full_redraw(w: &slint::Window) {
    let inner = i_slint_core::window::WindowInner::from_pub(w);
    let size = w.size().to_logical(w.scale_factor());
    inner.window_adapter().renderer().mark_dirty_region(
        i_slint_core::lengths::LogicalRect::from_size(
            i_slint_core::lengths::logical_size_from_api(size),
        ).into(),
    );
    w.request_redraw();
}

fn centered_position(parent: slint::PhysicalPosition, parent_size: slint::PhysicalSize, child_size: slint::PhysicalSize) -> slint::PhysicalPosition {
    slint::PhysicalPosition::new(parent.x + (parent_size.width as i32 - child_size.width as i32) / 2,
        parent.y + (parent_size.height as i32 - child_size.height as i32) / 2)
}

/// 按实际物理尺寸居中，保留副屏负坐标，再夹回所在屏幕工作区。
fn center_on_parent<W: ComponentHandle + 'static>(child: &W, parent: &Weak<MainPanel>) {
    fn center(child: &slint::Window, parent: &slint::Window) {
        let size = child.size();
        if size.width == 0 || size.height == 0 { return; }
        child.set_position(centered_position(parent.position(), parent.size(), size));
        if let Some(hwnd) = hwnd_of_window(child) {
            zaplaunch_sys::tray::clamp_window_to_work_area(hwnd, (EDGE_MARGIN as f32 * child.scale_factor()).round() as i32);
        }
    }
    if let Some(p) = parent.upgrade() { center(child.window(), p.window()); }
    let weak = child.as_weak();
    let parent = parent.clone();
    // 新窗口创建及跨屏 DPI 调整完成后，再按最终尺寸落位。
    defer(30, move || {
        if let (Some(child), Some(parent)) = (weak.upgrade(), parent.upgrade()) {
            center(child.window(), parent.window());
        }
    });
}

/// 窗口显示后的 50ms 施加 DWM 圆角。
/// 必须在事件循环内：窗口真正创建前 HWND 拿不到（主面板同一坑，见 zaplaunch-sys 注释）。
fn round_corners_later<W: ComponentHandle + 'static>(w: &W) {
    let weak = w.as_weak();
    defer(50, move || {
        if let Some(x) = weak.upgrade() {
            if let Some(hwnd) = hwnd_of_window(x.window()) {
                zaplaunch_sys::round_window_corners(hwnd);
                // 子窗口也要置顶：主面板呼出时是 TOPMOST，子窗口不置顶会被它盖住
                // （2026-10-22 实测：设置页打开后被主面板遮住，pagetest 像素全错）
                zaplaunch_sys::tray::set_topmost(hwnd, true);
                // 夹回屏幕：子窗口以主面板为中心，而主面板跟随鼠标 —— 在屏幕下方呼出时
                // 子窗口会被顶出屏幕（2026-10-24 实测设置页底部 122px 在屏外，截图全黑）
                zaplaunch_sys::tray::clamp_window_to_work_area(hwnd, (EDGE_MARGIN as f32 * x.window().scale_factor()).round() as i32);
                request_full_redraw(x.window());
            }
        }
    });
}

// ---------------------------------------------------------------------------
// 呼出 / 收起（托盘与热键共用同一条路径，见设计规格 7.12 状态机）
// ---------------------------------------------------------------------------

/// 呼出面板：按当前图标行数动态计算面板高度 → 算位置（跟随鼠标并确保在任务栏上方）→ 显示 → 抢输入焦点。
fn show_panel(p: &MainPanel, app: &AppConfig) {
    p.set_keyboard_navigation(false);
    PANEL_VISIBLE.store(true, Ordering::SeqCst);
    let generation = VISIBILITY_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    // 先刷新视图以确保 grid-height 已按当前群组/搜索的实际行数（2~6 行）更新完毕
    refresh_view(p, app);

    // 呼出不制造“新按键”：若 Esc/↓ 此时已经被按住，先继承按下态，
    // 等用户松开后再按才产生下一条上升沿，避免面板刚显示就立刻收起。
    ESC_LATCH.store(zaplaunch_sys::is_esc_pressed(), Ordering::SeqCst);
    DOWN_LATCH.store(zaplaunch_sys::is_key_pressed(0x28), Ordering::SeqCst);
    ESC_TIMER.with(|slot| {
        if let Some(t) = slot.borrow().as_ref() { t.restart(); }
    });
    BLUR_TIMER.with(|slot| {
        if let Some(t) = slot.borrow().as_ref() { t.restart(); }
    });

    let sf = p.window().scale_factor();
    let panel_h = 172.0 + p.get_grid_height();
    let phys_w = (PANEL_W * sf).round() as i32;
    let phys_h = (panel_h * sf).round() as i32;
    let phys_margin = (EDGE_MARGIN as f32 * sf).round() as i32;

    // 目标位置一次性算好：物理像素，下面用 Win32 落位（不用 Slint 的 set_position，
    // 它在高 DPI 下会重复缩放，见 zaplaunch_sys::tray::move_window 注释）
    // 当用户在屏幕下方或任务栏附近呼出时，popup_position 会自动将 y 上移至 work.bottom - phys_h - phys_margin，
    // 确保整个动态高度面板 100% 完整展示在任务栏上方的安全工作区内。
    let target = if app.follow_mouse {
        Some(zaplaunch_sys::tray::popup_position(
            phys_w,
            phys_h,
            phys_margin,
        ))
    } else {
        None
    };

    // 窗口已存在（常驻模式下窗口建好但隐藏）→ 先就位并同步尺寸再显示，避免"先弹出来再跳"
    if let Some(hwnd) = hwnd_of(p) {
        if let Some((x, y)) = target {
            zaplaunch_sys::tray::move_window(hwnd, x, y);
        }
        zaplaunch_sys::tray::resize_and_clamp_window(hwnd, phys_w, phys_h, phys_margin);
    }
    let _ = p.show();
    if app.sound_enabled { zaplaunch_sys::warm_event_sounds(); }
    if !p.get_group_modal_open() { p.invoke_focus_search(); }
    if let Some(hwnd) = hwnd_of(p) {
        zaplaunch_sys::restore_window(hwnd);
    }
    // 软件渲染在反复显隐/遮挡后必须明确标记完整重绘，避免复用旧帧。
    request_full_redraw(p.window());

    // 显示是异步过程：窗口真正建好、被系统接管之后抢焦点才有意义。
    // ⚠️ 别用 Timer::single_shot —— 它是关联函数、不返回句柄，时机不对会**静默失效**
    // （2026-10-22 实测，详见 defer 的注释）。
    let weak = p.as_weak();
    defer(30, move || {
        if generation != VISIBILITY_GENERATION.load(Ordering::SeqCst)
            || !PANEL_VISIBLE.load(Ordering::SeqCst)
        {
            return;
        }
        if let Some(q) = weak.upgrade() {
            if let Some(hwnd) = hwnd_of(&q) {
                let cur_sf = q.window().scale_factor();
                let cur_w = (PANEL_W * cur_sf).round() as i32;
                let cur_h = ((172.0 + q.get_grid_height()) * cur_sf).round() as i32;
                let cur_margin = (EDGE_MARGIN as f32 * cur_sf).round() as i32;
                // 启动那一刻窗口可能还没建（hwnd 为 None），这里补一次落位与尺寸夹取
                if let Some((x, y)) = target {
                    zaplaunch_sys::tray::move_window(hwnd, x, y);
                }
                zaplaunch_sys::tray::resize_and_clamp_window(hwnd, cur_w, cur_h, cur_margin);
                // 新建/设置等子窗口可能已在这 30ms 内取得焦点，延迟置前不得再抢回来。
                let child_has_focus = zaplaunch_sys::tray::is_foreground_in_process()
                    && !zaplaunch_sys::tray::is_foreground_window(hwnd);
                if !child_has_focus {
                    zaplaunch_sys::restore_window(hwnd);
                    zaplaunch_sys::tray::force_foreground(hwnd);
                    recover_panel_input_method(&q);
                }
                // 必须在 ShowWindow/置前之后把整窗标脏再重绘，避免隐藏期间
                // 被系统丢弃的像素仍被 softbuffer 当作有效局部帧复用。
                request_full_redraw(q.window());
                // 输入法由 Slint TextInput 焦点统一管理，不能绕过 winit 单独恢复原生上下文。
                if !zaplaunch_sys::install_file_drop(hwnd, push_dropped_files, notify_panel_activated, notify_panel_tab_fallback) {
                    eprintln!("[zaplaunch] 主面板文件拖放入口安装失败");
                }
            }
        }
    });
}

fn mark_panel_hidden(p: &MainPanel) {
    PANEL_VISIBLE.store(false, Ordering::SeqCst);
    VISIBILITY_GENERATION.fetch_add(1, Ordering::SeqCst);
    ESC_TIMER.with(|slot| {
        if let Some(t) = slot.borrow().as_ref() { t.stop(); }
    });
    BLUR_TIMER.with(|slot| {
        if let Some(t) = slot.borrow().as_ref() { t.stop(); }
    });
    if let Some(hwnd) = hwnd_of(p) {
        zaplaunch_sys::tray::unset_topmost(hwnd);
    }
}

fn hide_panel(p: &MainPanel) {
    mark_panel_hidden(p);
    // 正式收起 Slint 窗口；下次呼出由 show_panel 标记整窗重绘，
    // 不再复用隐藏前可能已失效的局部帧。
    let _ = p.hide();
}

// ---------------------------------------------------------------------------
// 数据 → UI 模型
// ---------------------------------------------------------------------------

/// 图标查找：配置的 icon 名精确匹配 icons.rs 的 IconDef.label；
/// 找不到用兜底图标（终端·灰）——用户自建按钮没配图标也能正常显示。
fn icon_def(name: &str) -> &'static icons::IconDef {
    icons::ICONS
        .iter()
        .find(|d| d.label == name)
        .unwrap_or(&icons::ICONS[3]) // 终端（灰）
}

fn file_icon_key(path: &str, custom: bool) -> Option<String> {
    let key = path.trim().trim_matches('"').trim().replace('/', "\\").to_lowercase();
    if key.is_empty() || key.starts_with("http:") || key.starts_with("https:")
        || key.starts_with("ms-") || key.starts_with("shell:") { return None; }
    Some(if custom && (key.ends_with(".png") || key.ends_with(".ico")) { format!("image:{key}") } else { key })
}

fn native_icon_key(target: &str, icon: &str) -> Option<String> {
    let ic = icon.trim();
    if ic.contains('\\') || ic.contains('/') { return file_icon_key(ic, true); }
    if ic.is_empty() {
        if let Some(url) = web_icons::website(target) { return Some(url.origin().ascii_serialization()); }
    }
    if ic.is_empty() || target.contains('\\') || target.contains('/') { return file_icon_key(target, false); }
    None
}

fn request_icon(key: &str, force: bool) -> bool {
    let Some(worker) = asset_worker() else { return false; };
    let id = NEXT_ICON_REQUEST.fetch_add(1, Ordering::Relaxed);
    let sent = if key.starts_with("http:") || key.starts_with("https:") {
        worker.websites.send(WebsiteRequest::Icon(key.to_string(), id, force)).is_ok()
    } else { worker.requests.send(AssetRequest::Icon(key.to_string(), id, force)).is_ok() };
    if sent { ICON_REQUESTS.with(|r| { r.borrow_mut().insert(key.to_string(), id); }); }
    sent
}

fn resolve_native_icon(target: &str, icon: &str) -> Option<slint::Image> {
    resolve_icon_key(native_icon_key(target, icon)?)
}

fn resolve_icon_key(key: String) -> Option<slint::Image> {
    ICON_CACHE.with(|cache| {
        if let Some(image) = cache.borrow_mut().get(&key) { return image; }
        let pending = ICON_REQUESTS.with(|r| r.borrow().contains_key(&key));
        if !pending && request_icon(&key, false) { cache.borrow_mut().insert(key, None); }
        None
    })
}

fn complete_icon(key: &str, id: usize) -> bool {
    ICON_REQUESTS.with(|r| {
        let mut requests = r.borrow_mut();
        if requests.get(key) != Some(&id) { return false; }
        requests.remove(key);
        true
    })
}

fn preload_local_icons(c: &AppConfig) {
    for button in c.groups.iter().flat_map(|group| &group.buttons) {
        if let Some(key) = native_icon_key(&button.target,&button.icon) {
            if !key.starts_with("http:") && !key.starts_with("https:") {
                resolve_icon_key(key);
            }
        }
    }
}

struct CacheRebuild {
    waiting: Vec<String>,
    active: std::collections::HashMap<String,usize>,
    root: std::path::PathBuf,
    total: usize,
    failed: usize,
    cleaning: bool,
}

fn configured_icon_keys(c: &AppConfig) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    c.groups.iter().flat_map(|g| &g.buttons).filter_map(|b| native_icon_key(&b.target,&b.icon))
        .filter(|key| seen.insert(key.clone())).collect()
}

fn maintain_web_cache(c: &AppConfig) {
    if let Some(worker) = asset_worker() {
        let keys = c.groups.iter().flat_map(|g| &g.buttons)
            .filter_map(|b| native_icon_key(&b.target,&b.icon))
            .filter(|key| key.starts_with("http:") || key.starts_with("https:"))
            .map(|key| web_icons::cache_file_name(&key)).collect();
        let _ = worker.websites.send(WebsiteRequest::Keep(keys));
    }
}

fn sync_cache_rebuild_ui(w: Option<&SettingsWindow>) {
    let Some(w) = w else { return; };
    CACHE_REBUILD.with(|state| {
        let state = state.borrow();
        w.set_cache_busy(state.is_some());
        let text = match state.as_ref() {
            Some(job) if job.cleaning => "正在清理旧缓存…".into(),
            Some(job) => format!("正在重建 {} / {}",job.total-job.waiting.len()-job.active.len(),job.total),
            None => CACHE_REBUILD_STATUS.with(|text| text.borrow().clone()),
        };
        w.set_cache_status(text.into());
    });
}

fn finish_cache_rebuild(cleanup_failed: usize, w: Option<&SettingsWindow>) {
    if let Some(job) = CACHE_REBUILD.with(|state| state.borrow_mut().take()) {
        let message = if cleanup_failed > 0 { format!("图标失败 {}，清理失败 {}",job.failed,cleanup_failed) }
            else if job.failed > 0 { format!("完成 {} 项，失败 {} 项",job.total-job.failed,job.failed) }
            else { format!("已完成 {} 项图标重建",job.total) };
        CACHE_REBUILD_STATUS.with(|text| *text.borrow_mut() = message);
    }
    sync_cache_rebuild_ui(w);
}

fn advance_cache_rebuild(c: &AppConfig, w: Option<&SettingsWindow>) {
    let cleanup = CACHE_REBUILD.with(|state| {
        let mut state = state.borrow_mut();
        let job = state.as_mut()?;
        if job.cleaning { return None; }
        for key in std::mem::take(&mut job.waiting) {
            if ICON_REQUESTS.with(|requests| requests.borrow().contains_key(&key)) {
                job.waiting.push(key);
            } else if request_icon(&key,true) {
                let id = ICON_REQUESTS.with(|requests| requests.borrow()[&key]);
                job.active.insert(key,id);
            } else { job.failed += 1; }
        }
        if job.waiting.is_empty() && job.active.is_empty() && ICON_REQUESTS.with(|requests| requests.borrow().is_empty()) {
            job.cleaning = true;
            Some(AssetRequest::Clean(job.root.clone(),configured_icon_keys(c)))
        } else { None }
    });
    if let Some(request) = cleanup {
        if !asset_worker().map(|worker| worker.requests.send(request).is_ok()).unwrap_or(false) {
            finish_cache_rebuild(1,w);
        }
    }
    sync_cache_rebuild_ui(w);
}

fn begin_cache_rebuild(c: &AppConfig, root: std::path::PathBuf, w: Option<&SettingsWindow>) {
    let started = CACHE_REBUILD.with(|state| {
        let mut state = state.borrow_mut();
        if state.is_some() { return false; }
        let keys = configured_icon_keys(c);
        *state = Some(CacheRebuild { total:keys.len(),waiting:keys,active:Default::default(),root,failed:0,cleaning:false });
        true
    });
    if started { advance_cache_rebuild(c,w); }
}

fn clean_icon_cache(root: &std::path::Path, keys: &[String]) -> usize {
    let local: std::collections::HashSet<_> = keys.iter().filter(|key| !key.starts_with("http:") && !key.starts_with("https:") && !key.starts_with("image:"))
        .map(|key| icon_disk_cache::cache_file_name(key)).collect();
    let web: std::collections::HashSet<_> = keys.iter().filter(|key| key.starts_with("http:") || key.starts_with("https:"))
        .map(|key| web_icons::cache_file_name(key)).collect();
    let mut failed = web_icons::prune(&root.join("favicons"),Some(&web));
    for (folder,extension,keep) in [("native-icons","rgba",local)] {
        let files = match std::fs::read_dir(root.join(folder)) {
            Ok(files) => files,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => { failed += 1; continue; }
        };
        for entry in files {
            let Ok(entry) = entry else { failed += 1; continue; };
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
            if path.extension().and_then(|s| s.to_str()) != Some(extension) || stem.len() != 16
                || !stem.bytes().all(|b| b.is_ascii_hexdigit()) || keep.contains(&name) { continue; }
            match entry.file_type() {
                Ok(kind) if kind.is_file() => { if std::fs::remove_file(path).is_err() { failed += 1; } }
                Err(_) => failed += 1,
                _ => {}
            }
        }
    }
    failed
}

/// ButtonConfig → UI Tile（优先使用文件/快捷方式原本的系统图标，兜底查内置矢量库）。
///
/// `sel` 表示当前键盘选中项；搜索默认第一项，随后可通过 Tab 或方向键移动。
/// 常态为 `SEL_IDX`。底栏「Enter 启动」启动的就是这一格。
fn tile_of(b: &ButtonConfig, sel: bool) -> Tile {
    let def = icon_def(&b.icon);
    let native = resolve_native_icon(&b.target, &b.icon);
    let (letter, color) = web_icons::fallback(&b.target, &b.label, &b.icon);
    Tile {
        label: b.label.as_str().into(),
        icon: def.cmds.into(),
        tint: if letter.is_empty() { Color::from_rgb_u8(def.color.0, def.color.1, def.color.2) } else { Color::from_rgb_u8(color.0, color.1, color.2) },
        letter: letter.into(),
        has_native_icon: native.is_some(),
        native_icon: native.unwrap_or_default(),
        sel,
    }
}

/// 单个磁贴格宽：(496 − (cols−1)×2) / cols。
/// 496 = 面板宽 520 − 网格左右各 12 的 margin（设计规格 7.2）。
/// cols=6→81 / 7→69 / 8→60.25
fn tile_width(cols: usize) -> f32 {
    let c = cols.max(1) as f32;
    (496.0 - (c - 1.0) * 2.0) / c
}

/// 显示串「7 个」→ 数字 7（UI 带单位是给人看的，配置里只存数字；越界夹到 6~8）。
fn parse_grid_columns(v: &str) -> u32 {
    let digits: String = v.chars().filter(|c| c.is_ascii_digit()).collect();
    zaplaunch_core::clamp_grid_columns(digits.parse::<u32>().unwrap_or(DEFAULT_COLS as u32))
}

/// 显示串「按使用次数」→ 存储串 "usage"（UI 文案 vs 数据契约，边界在这一层转换）。
fn parse_tile_order(v: &str) -> &'static str {
    match v {
        "按使用次数" => "usage",
        "按名称" => "name",
        _ => "manual",
    }
}

/// 当前群组的按钮**显示顺序**（返回的是 buttons 的下标序列）。
///
/// 只作用于显示层，不改写 `groups` 里的实际顺序——所以把排序方式切回「手动」，
/// 手工排的顺序原样回来。
///   - "manual"：配置原序
///   - "usage" ：使用次数降序；次数相同的保持原序（sort_by 稳定），0 次的新按钮自然落到最后
///   - "name"  ：名称升序 ⚠️ 中文按 Unicode 码点排，不是拼音序（要拼音序得引外部表，会涨体积）
fn display_order(c: &zaplaunch_core::AppConfig, gi: usize) -> Vec<usize> {
    let Some(g) = c.groups.get(gi) else {
        return Vec::new();
    };
    let mut order: Vec<usize> = (0..g.buttons.len()).collect();
    match c.tile_order.as_str() {
        "usage" => order.sort_by(|&a, &b| g.buttons[b].run_count.cmp(&g.buttons[a].run_count)),
        "name" => order.sort_by(|&a, &b| g.buttons[a].label.cmp(&g.buttons[b].label)),
        _ => {}
    }
    order
}

fn grid_insert_at(row: usize, col: usize, x: f32, y: f32, cols: usize, count: usize, tile_width: f32, tile_height: f32) -> Option<usize> {
    if cols == 0 || count == 0 || !x.is_finite() || !y.is_finite() { return None; }
    let pitch = tile_width + 2.0;
    let px = col as f32 * pitch + x;
    let py = row as f32 * tile_height + y;
    if px < 0.0 || px > cols as f32 * pitch - 2.0 || py < 0.0 || py >= count.div_ceil(cols) as f32 * tile_height { return None; }
    let target_col = (px / pitch) as usize;
    let target_row = (py / tile_height) as usize;
    let after = px - target_col as f32 * pitch > tile_width / 2.0;
    Some((target_row * cols + target_col + usize::from(after)).min(count))
}

/// 平铺 Tile 序列 → cols 列网格的行模型（行距 0 / 列距 2px 由 TileGrid 负责）。
///
/// 常态与搜索态共用这一条路：区别只在于传进来的序列是「整组按钮」还是「跨组命中」。
fn rows_of_tiles(tiles: Vec<Tile>, cols: usize) -> Vec<ModelRc<Tile>> {
    let n = tiles.len();
    (0..n)
        .step_by(cols)
        .map(|r| {
            let end = (r + cols).min(n);
            ModelRc::new(VecModel::from(tiles[r..end].to_vec()))
        })
        .collect()
}

/// 搜索/刷新共用已有行，只通知真正变化的项，保留磁贴交互和滚动状态。
fn sync_grid_tiles(p: &MainPanel, tiles: Vec<Tile>, cols: usize) -> usize {
    let row_count = tiles.len().div_ceil(cols);
    let rows = p.get_rows();
    let Some(model) = rows.as_any().downcast_ref::<VecModel<ModelRc<Tile>>>() else {
        p.set_rows(ModelRc::new(VecModel::from(rows_of_tiles(tiles, cols))));
        return row_count;
    };
    for (ri, chunk) in tiles.chunks(cols).enumerate() {
        let Some(row) = model.row_data(ri) else {
            model.push(ModelRc::new(VecModel::from(chunk.to_vec())));
            continue;
        };
        if let Some(items) = row.as_any().downcast_ref::<VecModel<Tile>>() {
            for (ci, tile) in chunk.iter().enumerate() {
                if ci >= items.row_count() { items.push(tile.clone()); }
                else if items.row_data(ci).as_ref() != Some(tile) { items.set_row_data(ci, tile.clone()); }
            }
            while items.row_count() > chunk.len() { items.remove(items.row_count() - 1); }
        } else { model.set_row_data(ri, ModelRc::new(VecModel::from(chunk.to_vec()))); }
    }
    while model.row_count() > row_count { model.remove(model.row_count() - 1); }
    row_count
}

fn set_launch_hint(p: &MainPanel, label: &str) {
    p.set_run_hint(format!("启动 {label}").into());
    p.set_run_hint_w((24.0 + label.chars().count() as f32 * 12.0).min(140.0));
}

fn update_grid_selection(p: &MainPanel, selected: usize) {
    let rows = p.get_rows();
    let cols = CUR_COLS.load(Ordering::SeqCst);
    let previous = SEL_IDX.swap(selected, Ordering::SeqCst);
    for index in [previous, selected] {
        let Some(row) = rows.row_data(index / cols) else { continue };
        let Some(mut tile) = row.row_data(index % cols) else { continue };
        let selected_now = index == selected;
        if tile.sel != selected_now {
            tile.sel = selected_now;
            row.set_row_data(index % cols, tile);
        }
    }
    if p.get_searching() {
        if let Some(tile) = rows.row_data(selected / cols).and_then(|row| row.row_data(selected % cols)) {
            set_launch_hint(p, &tile.label);
        }
    }
    p.invoke_reveal_tile(selected as i32);
}

fn cycle_search_result(p: &MainPanel, backwards: bool) {
    let len = HIT_MAP.with(|map| map.borrow().len());
    if len == 0 { p.invoke_focus_search(); return; }
    let current = SEL_IDX.load(Ordering::SeqCst).min(len - 1);
    let selected = if p.get_search_input_focused() { if backwards { len - 1 } else { 0 } }
        else if backwards { (current + len - 1) % len } else { (current + 1) % len };
    p.invoke_clear_search_focus();
    p.set_keyboard_navigation(true);
    update_grid_selection(p, selected);
}

/// 缓存解析 `.lnk` 快捷方式内部指向的真实目标路径（供智能搜索匹配 `.exe` 路径与程序名）。
fn cached_shortcut_target(path: &str) -> Option<String> {
    let key = path.trim().replace('/', "\\").to_lowercase();
    if !key.ends_with(".lnk") {
        return None;
    }
    LNK_TARGET_CACHE.with(|cache| {
        if let Some(cached) = cache.borrow_mut().get(&key) { return cached; }
        PENDING_SHORTCUTS.with(|pending| {
            let mut pending = pending.borrow_mut();
            if !pending.contains(&key) && asset_worker().is_some_and(|w| w.shortcuts.send(key.clone()).is_ok()) {
                pending.insert(key);
            }
        });
        None
    })
}

/// 根据当前网格行数（1~6 行自适应）动态更新面板高度，并在窗口可见时确保整个面板完整位于任务栏上方工作区。
fn apply_dynamic_grid_height(p: &MainPanel, row_count: usize, show_label: bool) {
    let vis_rows = row_count.clamp(1, 6);
    let row_h = if show_label { 82.0 } else { p.get_tile_width() + 2.0 };
    // 空状态文案需要 160px；有按钮时只留实际行高和 6px 底部边距。
    let grid_h = if row_count == 0 { 160.0 } else { vis_rows as f32 * row_h + 6.0 };
    let old_h = p.get_grid_height();
    p.set_grid_height(grid_h);
    if (grid_h - old_h).abs() > 0.5 && PANEL_VISIBLE.load(Ordering::SeqCst) {
        if let Some(hwnd) = hwnd_of(p) {
            let sf = p.window().scale_factor();
            let phys_w = (PANEL_W * sf).round() as i32;
            let phys_h = ((172.0 + grid_h) * sf).round() as i32;
            let phys_margin = (EDGE_MARGIN as f32 * sf).round() as i32;
            zaplaunch_sys::tray::resize_and_clamp_window(hwnd, phys_w, phys_h, phys_margin);
            zaplaunch_sys::round_window_corners(hwnd);
            request_full_redraw(p.window());
        }
    }
}

/// ② 常态视图：当前群组的全部按钮 + 标签计数 + 空状态 + 底栏常态文案。
///
/// 顺带重建 HIT_MAP（常态下 = 当前群组的第 n 个按钮），
/// 让点击 / 右键 / Enter 三处的换算口径始终一致。
fn refresh_group(p: &MainPanel, c: &AppConfig, gi: usize) {
    let n = c.groups.get(gi).map(|g| g.buttons.len()).unwrap_or(0);
    let sel = if n == 0 || p.get_searching() { 0 } else { SEL_IDX.load(Ordering::SeqCst).min(n - 1) };
    SEL_IDX.store(sel, Ordering::SeqCst);
    let cols = zaplaunch_core::clamp_grid_columns(c.grid_columns) as usize;
    CUR_COLS.store(cols, Ordering::SeqCst);
    p.set_tile_width(tile_width(cols));
    p.set_tile_columns(cols as i32);
    p.set_reorder_enabled(c.tile_order == "manual" && n > 1 && PENDING_LAUNCHES.with(|q| q.borrow().values().all(|request| request.timed_out)));
    p.set_drag_insert(-1);
    p.set_tile_show_label(c.tile_show_label);
    p.set_grid_scrollbar(c.grid_scrollbar);
    p.set_grid_lines(c.grid_lines);

    // 显示顺序（manual / usage / name）：tiles 按 order 取，HIT_MAP 也按 order 建，
    // 两边必须同源，否则排序后点击会错位。
    let order = display_order(c, gi);
    let tiles: Vec<Tile> = order
        .iter()
        .enumerate()
        .map(|(i, &bi)| {
            c.groups
                .get(gi)
                .and_then(|g| g.buttons.get(bi))
                .map(|b| tile_of(b, i == sel))
                .unwrap_or_else(|| tile_of(&empty_button(), false))
        })
        .collect();
    let row_count = sync_grid_tiles(p, tiles, cols);
    apply_dynamic_grid_height(p, row_count, c.tile_show_label);
    p.set_show_empty(n == 0);
    sync_groups(p, c);
    p.set_searching(false);
    p.set_search_pending(false);
    p.set_hit_count(n as i32);
    p.set_hit_text("".into());
    p.set_run_hint("启动".into());
    p.set_run_hint_w(24.0);
    HIT_MAP.with(|m| {
        *m.borrow_mut() = order.iter().map(|&bi| (gi, bi)).collect();
    });
}

/// 搜索词改变时选中第一项；后台补全结果时保留正在浏览的匹配项。
fn refresh_search(p: &MainPanel, c: &AppConfig, gi: usize, query: &str) {
    let previous = if p.get_searching() && p.get_keyboard_navigation() { hit_at(SEL_IDX.load(Ordering::SeqCst)) } else { None };
    let hits = zaplaunch_core::search_buttons_smart(c, query, gi, |b| {
        cached_shortcut_target(&b.target)
    });
    let selected = previous.and_then(|hit| hits.iter().position(|item| *item == hit)).unwrap_or(0);
    SEL_IDX.store(selected, Ordering::SeqCst);
    let tiles: Vec<Tile> = hits
        .iter()
        .enumerate()
        .map(|(i, &(g, b))| {
            c.groups
                .get(g)
                .and_then(|grp| grp.buttons.get(b))
                .map(|btn| tile_of(btn, i == selected))
                .unwrap_or_else(|| tile_of(&empty_button(), false))
        })
        .collect();
    let cols = zaplaunch_core::clamp_grid_columns(c.grid_columns) as usize;
    CUR_COLS.store(cols, Ordering::SeqCst);
    p.set_tile_width(tile_width(cols));
    p.set_tile_columns(cols as i32);
    p.set_reorder_enabled(false);
    p.set_drag_insert(-1);
    p.set_tile_show_label(c.tile_show_label);
    p.set_grid_scrollbar(c.grid_scrollbar);
    p.set_grid_lines(c.grid_lines);
    let row_count = sync_grid_tiles(p, tiles, cols);
    apply_dynamic_grid_height(p, row_count, c.tile_show_label);
    p.set_show_empty(false);
    p.set_searching(true);
    let pending = PENDING_SHORTCUTS.with(|pending| !pending.borrow().is_empty());
    p.set_search_pending(pending);
    let n = hits.len();
    p.set_hit_count(n as i32);
    // 底栏左：搜索范围（全部群组名）；右：Enter 语义随命中数变化
    let names: Vec<String> = c.groups.iter().map(|g| g.name.clone()).collect();
    p.set_hit_text(format!("在「{}」中命中 {} 个", names.join(" / "), n).into());
    if n == 0 {
        p.set_run_hint(if pending { "解析中" } else { "新建按钮" }.into());
        p.set_run_hint_w(48.0);
    } else {
        let label = c
            .groups
            .get(hits[selected].0)
            .and_then(|grp| grp.buttons.get(hits[selected].1))
            .map(|b| b.label.clone())
            .unwrap_or_default();
        set_launch_hint(p, &label);
    }
    HIT_MAP.with(|m| {
        *m.borrow_mut() = hits;
    });
    p.invoke_reveal_tile(selected as i32);
}

/// 统一出口：按当前搜索词决定走常态还是搜索态。
///
/// 所有会改变「网格里显示什么」的入口（切群组 / 增删改按钮 / 搜索 / 清词 / 恢复默认）
/// 一律收敛到这里 —— 避免同一个网格两套刷新逻辑各写一份、状态打架。
fn refresh_view(p: &MainPanel, c: &AppConfig) {
    let gi = (p.get_current_group() as usize).min(c.groups.len().saturating_sub(1));
    let q = current_search_trimmed();
    if q.is_empty() {
        refresh_group(p, c, gi);
    } else {
        refresh_search(p, c, gi, &q);
    }
}

/// 网格第 n 格 → 真实按钮坐标 `(群组, 群内)`；越界返回 None。
///
/// 常态下它恰好等于「当前群组第 n 个按钮」，搜索态则是跨组命中序列的第 n 项 ——
/// 差一层，所以点击 / 右键 / Enter 一律经它换算。
fn hit_at(n: usize) -> Option<(usize, usize)> {
    HIT_MAP.with(|m| m.borrow().get(n).copied())
}

/// 空按钮（搜索命中落空时的占位，避免造出半初始化的 Tile）。
fn empty_button() -> ButtonConfig {
    ButtonConfig {
        label: String::new(),
        target: String::new(),
        params: String::new(),
        work_dir: String::new(),
        icon: String::new(),
        hotkey: String::new(),
        window_state: "normal".into(),
        launch_on_startup: false,
        run_count: 0,
    }
}

/// 拖入的文件/快捷方式 → 新按钮。快捷方式保持 .lnk 目标，由 ShellExecute
/// 按快捷方式自身参数启动；不擅自解析并丢掉它原有的参数、图标和工作目录。
fn button_from_path(path: &str) -> Option<ButtonConfig> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return None;
    }
    let p = std::path::Path::new(trimmed);
    // 兼容快捷方式 (.lnk)、符号链接与盘符根目录：即使目标不可访问，只要自身元数据存在即允许添加
    if !p.exists() && p.symlink_metadata().is_err() {
        return None;
    }
    let label = p
        .file_stem()
        .or_else(|| p.file_name())
        .map(|s| s.to_string_lossy().trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| trimmed.trim_end_matches(&['\\', '/'][..]).to_string());
    if label.is_empty() {
        return None;
    }
    let ext = p.extension().and_then(|x| x.to_str()).unwrap_or("").to_ascii_lowercase();
    Some(ButtonConfig {
        label,
        target: trimmed.to_string(),
        params: String::new(),
        work_dir: if ext == "lnk" {
            String::new()
        } else {
            p.parent().map(|x| x.to_string_lossy().into_owned()).unwrap_or_default()
        },
        // 留空表示直接使用该快捷方式/文件/EXE/文件夹原本的系统图标
        icon: String::new(),
        hotkey: String::new(),
        window_state: "normal".into(),
        launch_on_startup: false,
        run_count: 0,
    })
}

#[derive(Clone, Copy)]
enum LaunchSource { Button(usize, usize), RemovedButton, History }

struct PendingLaunch {
    window_state: String,
    source: LaunchSource,
    target: String,
    params: String,
    dir: String,
    label: String,
    hide_generation: Option<usize>,
    hist_on: bool,
    hist_max: u32,
    sound_on: bool,
    network: bool,
    timed_out: bool,
    timeout: Timer,
}

fn panel_hide_generation(hide_after: bool) -> Option<usize> {
    (hide_after && PANEL_VISIBLE.load(Ordering::SeqCst))
        .then(|| VISIBILITY_GENERATION.load(Ordering::SeqCst))
}

fn remap_pending_buttons(map: impl Fn(usize, usize) -> Option<(usize, usize)>) {
    PENDING_LAUNCHES.with(|pending| {
        for request in pending.borrow_mut().values_mut() {
            if let LaunchSource::Button(group, button) = request.source {
                request.source = match map(group, button) {
                    Some((group, button)) => LaunchSource::Button(group, button),
                    None => LaunchSource::RemovedButton,
                };
            }
        }
    });
}

fn apply_launch_success(c: &mut AppConfig, request: &PendingLaunch) {
    let matches = |b: &ButtonConfig| b.target == request.target && b.params == request.params
        && b.work_dir == request.dir && b.label == request.label && b.window_state == request.window_state;
    match request.source {
        LaunchSource::Button(..) | LaunchSource::RemovedButton => {
            if let LaunchSource::Button(gi, bi) = request.source {
                if let Some(b) = c.groups.get_mut(gi).and_then(|g| g.buttons.get_mut(bi)) {
                    if matches(b) { b.run_count = b.run_count.saturating_add(1); }
                }
            }
            if request.hist_on {
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::SystemTime::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64).unwrap_or(0);
                c.history.insert(0, HistoryEntry {
                    window_state: request.window_state.clone(),
                    target: request.target.clone(), label: request.label.clone(), ts,
                    params: request.params.clone(), work_dir: request.dir.clone(),
                });
                c.history.truncate(request.hist_max.max(1) as usize);
            }
        }
        LaunchSource::History => {
            for g in &mut c.groups {
                if let Some(b) = g.buttons.iter_mut().find(|b| matches(b)) {
                    b.run_count = b.run_count.saturating_add(1);
                    break;
                }
            }
            if request.hist_on {
                if let Some(index) = c.history.iter().position(|h| h.target == request.target
                    && h.params == request.params && h.work_dir == request.dir && h.label == request.label && h.window_state == request.window_state) {
                    let mut entry = c.history.remove(index);
                    entry.ts = std::time::SystemTime::now()
                        .duration_since(std::time::SystemTime::UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64).unwrap_or(entry.ts);
                    c.history.insert(0, entry);
                }
            }
        }
    }
}

fn launch_slots_available(total: usize, network_pending: usize, network: bool) -> bool {
    total < 8 && (!network || network_pending < 6)
}

fn queue_launch(p: &MainPanel, mut request: PendingLaunch) {
    request.network = zaplaunch_sys::is_network_path(&request.target) || zaplaunch_sys::is_network_path(&request.dir);
    // Reserve two of the eight slots for targets that are not known network paths.
    let can_launch = PENDING_LAUNCHES.with(|q| {
        let pending = q.borrow();
        launch_slots_available(pending.len(), pending.values().filter(|item| item.network).count(), request.network)
    });
    if !can_launch {
        zaplaunch_sys::show_error("正在启动的目标过多，请等待完成后重试。");
        return;
    }
    let id = NEXT_LAUNCH_REQUEST.fetch_add(1, Ordering::Relaxed);
    let target = request.target.clone();
    let params = request.params.clone();
    let dir = request.dir.clone();
    let state = request.window_state.clone();
    let sound_on = request.sound_on;
    request.timeout.start(TimerMode::SingleShot, Duration::from_secs(10), move || {
        push_tray_event(TrayEvent::LaunchTimedOut(id));
    });
    PENDING_LAUNCHES.with(|q| { q.borrow_mut().insert(id, request); });
    let was_reorder_enabled = p.get_reorder_enabled();
    p.set_reorder_enabled(false);
    p.set_drag_insert(-2);
    if let Err(e) = std::thread::Builder::new().spawn(move || {
        let ok = execute_launch(&target, &params, &dir, &state);
        push_tray_event(TrayEvent::LaunchCompleted(id, ok));
    }) {
        PENDING_LAUNCHES.with(|q| { q.borrow_mut().remove(&id); });
        p.set_reorder_enabled(was_reorder_enabled);
        p.set_drag_insert(-1);
        zaplaunch_sys::show_error(&format!("无法创建启动线程：{e}"));
    } else if sound_on {
        zaplaunch_sys::play_event_sound(zaplaunch_sys::EventSound::Click);
    }
}

fn execute_launch(target: &str, params: &str, dir: &str, state: &str) -> bool {
    let Some(show) = zaplaunch_core::window_show_mode(state) else { return false; };
    let url = web_icons::website(target).map(|u| u.to_string());
    zaplaunch_sys::launch_program_with_state(url.as_deref().unwrap_or(target), params, dir, show)
}

fn startup_buttons(c: &AppConfig, enabled: bool) -> Vec<ButtonConfig> {
    if !enabled { return Vec::new(); }
    c.groups.iter().flat_map(|g| &g.buttons).filter(|b| b.launch_on_startup).cloned().collect()
}

fn run_startup_buttons(buttons: &[ButtonConfig], mut launch: impl FnMut(&ButtonConfig) -> bool) -> Vec<String> {
    let mut failed = Vec::new();
    for b in buttons {
        if !launch(b) { eprintln!("[zaplaunch] 自动运行失败: {} ({})", b.label, b.target); failed.push(b.label.clone()); }
    }
    failed
}

/// 启动指定坐标的按钮；系统调用在后台进行，结果回到 UI 线程提交。
fn launch_at(p: &MainPanel, app: &Rc<RefCell<AppConfig>>, gi: usize, bi: usize) {
    let request = {
        let c = app.borrow();
        c.groups.get(gi).and_then(|g| g.buttons.get(bi)).map(|b| PendingLaunch {
            window_state: b.window_state.clone(),
            source: LaunchSource::Button(gi, bi),
            target: b.target.clone(), params: b.params.clone(), dir: b.work_dir.clone(), label: b.label.clone(),
            hide_generation: panel_hide_generation(c.hide_after_run), hist_on: c.history_enabled,
            hist_max: c.history_limit, sound_on: c.sound_enabled,
            network: false, timed_out: false, timeout: Timer::default(),
        })
    };
    if let Some(request) = request { queue_launch(p, request); }
}

/// ⑬ 运行历史 → 浮层行模型（只取前 `HISTORY_SHOWN` 条，新的在前）。
///
/// 图标按 (target, params) 优先精确反查按钮，次选 (target, label)，最后退回 target；
/// 避免「资源管理器」与「回收站」同为 explorer.exe 时图标混淆。
fn history_icon<'a>(c: &'a AppConfig, h: &HistoryEntry) -> &'a str {
    let buttons = || c.groups.iter().flat_map(|g| &g.buttons);
    buttons().find(|b| b.target == h.target && b.params == h.params)
        .or_else(|| buttons().find(|b| b.target == h.target && b.label == h.label))
        .or_else(|| buttons().find(|b| b.target == h.target)).map(|b| b.icon.as_str()).unwrap_or("")
}

fn patch_asset_icons(p: &MainPanel, c: &AppConfig, images: &std::collections::HashMap<String, slint::Image>) {
    if images.is_empty() { return; }
    let rows = p.get_rows();
    let cols = CUR_COLS.load(Ordering::SeqCst);
    HIT_MAP.with(|hits| {
        for (index, &(gi, bi)) in hits.borrow().iter().enumerate() {
            let Some(b) = c.groups.get(gi).and_then(|g| g.buttons.get(bi)) else { continue; };
            let Some(key) = native_icon_key(&b.target, &b.icon) else { continue; };
            let Some(image) = images.get(&key) else { continue; };
            let Some(row) = rows.row_data(index / cols) else { continue; };
            let Some(mut tile) = row.row_data(index % cols) else { continue; };
            if !tile.has_native_icon || tile.native_icon != *image {
                tile.native_icon = image.clone(); tile.has_native_icon = true;
                row.set_row_data(index % cols, tile);
            }
        }
    });
    let history = p.get_history();
    for (index, h) in c.history.iter().take(history.row_count()).enumerate() {
        let Some(key) = native_icon_key(&h.target, history_icon(c, h)) else { continue; };
        let Some(image) = images.get(&key) else { continue; };
        let Some(mut item) = history.row_data(index) else { continue; };
        if !item.has_native_icon || item.native_icon != *image {
            item.native_icon = image.clone(); item.has_native_icon = true;
            history.set_row_data(index, item);
        }
    }
}

fn history_rows(c: &AppConfig) -> Vec<HistoryRow> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let tz = zaplaunch_sys::local_utc_offset_secs();
    c.history
        .iter()
        .take(HISTORY_SHOWN)
        .map(|h| {
            let icon = history_icon(c, h);
            let def = icon_def(icon);
            let native = resolve_native_icon(&h.target, icon);
            let (letter, color) = web_icons::fallback(&h.target, &h.label, icon);
            HistoryRow {
                label: h.label.as_str().into(),
                icon: def.cmds.into(),
                tint: if letter.is_empty() { Color::from_rgb_u8(def.color.0, def.color.1, def.color.2) } else { Color::from_rgb_u8(color.0, color.1, color.2) },
                letter: letter.into(),
                has_native_icon: native.is_some(),
                native_icon: native.unwrap_or_default(),
                time: zaplaunch_core::format_rel_time(h.ts, now, tz).into(),
            }
        })
        .collect()
}

/// 复用标签模型，避免普通网格刷新重建全部分组控件。
fn sync_groups(p: &MainPanel, c: &AppConfig) {
    let groups = p.get_groups();
    let Some(model) = groups.as_any().downcast_ref::<VecModel<Group>>() else {
        p.set_groups(ModelRc::new(VecModel::from(c.groups.iter().map(|g| Group {
            name: g.name.as_str().into(),
        }).collect::<Vec<_>>())));
        return;
    };
    for (index, group) in c.groups.iter().enumerate() {
        match model.row_data(index) {
            None => model.push(Group { name: group.name.as_str().into() }),
            Some(row) if row.name != group.name.as_str() => {
                model.set_row_data(index, Group { name: group.name.as_str().into() });
            }
            _ => {}
        }
    }
    while model.row_count() > c.groups.len() { model.remove(model.row_count() - 1); }
}

fn button_hotkey_fields(c: &AppConfig) -> Vec<(usize, usize, String)> {
    c.groups.iter().enumerate().flat_map(|(gi, g)| {
        g.buttons.iter().enumerate().filter_map(move |(bi, b)| {
            (!b.hotkey.trim().is_empty()).then(|| (gi, bi, b.hotkey.trim().to_string()))
        })
    }).collect()
}

fn button_hotkeys(c: &AppConfig) -> (Vec<(usize, usize, zaplaunch_sys::tray::Hotkey)>, Vec<(usize, usize)>) {
    let panel_key = zaplaunch_sys::tray::parse_hotkey(&c.hotkey);
    let mut result = Vec::new();
    let mut failed = Vec::new();
    for (gi, bi, text) in button_hotkey_fields(c) {
        let Some(hk) = zaplaunch_sys::tray::parse_hotkey(&text) else {
            failed.push((gi, bi));
            continue;
        };
        if Some(hk) == panel_key || result.iter().any(|&(_, _, existing)| existing == hk) {
            failed.push((gi, bi));
            continue;
        }
        result.push((gi, bi, hk));
    }
    (result, failed)
}

fn failed_hotkey_message(c: &AppConfig, failed: &[(usize, usize)]) -> String {
    let labels: Vec<String> = failed.iter().take(6).filter_map(|&(gi, bi)| {
        c.groups.get(gi).and_then(|g| g.buttons.get(bi).map(|b| format!("{} / {}", g.name, b.label)))
    }).collect();
    let extra = if failed.len() > 6 { format!("，另有 {} 项", failed.len() - 6) } else { String::new() };
    format!("以下按钮快捷键未生效：{}{}。请更换冲突或无效的组合键；其他按钮仍可使用。", labels.join("、"), extra)
}

struct PendingConfig {
    id: u64,
    app: Rc<RefCell<AppConfig>>,
    next: AppConfig,
    done: Box<dyn FnOnce(bool)>,
}
thread_local! {
    static CONFIG_PENDING: RefCell<Option<PendingConfig>> = const { RefCell::new(None) };
    static CONFIG_LAUNCHES: RefCell<Vec<(u64, bool)>> = const { RefCell::new(Vec::new()) };
}
static CONFIG_FILE_PENDING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static QUIT_AFTER_CONFIG: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static QUIT_DEADLINE_STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static QUIT_CONFIG_EXPIRED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static NEXT_CONFIG: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn commit_config(app: &Rc<RefCell<AppConfig>>, mutate: impl FnOnce(&mut AppConfig), done: impl FnOnce(bool) + 'static) {
    if CONFIG_READ_ONLY.load(Ordering::SeqCst) || CONFIG_PENDING.with(|p| p.borrow().is_some())
        || CONFIG_FILE_PENDING.load(Ordering::SeqCst) || PENDING_HOTKEY.lock().map(|p| p.is_some()).unwrap_or(true) {
        zaplaunch_sys::show_error("配置处于保护模式或正在保存，请稍后重试。草稿尚未保存。");
        done(false);
        resume_deferred_quit();
        return;
    }
    let previous = app.borrow().clone();
    let mut next = previous.clone();
    mutate(&mut next);
    let id = NEXT_CONFIG.fetch_add(1, Ordering::Relaxed);
    let client = TRAY.lock().ok().and_then(|t| t.as_ref().map(|h| h.button_hotkey_client()));
    CONFIG_PENDING.with(|p| *p.borrow_mut() = Some(PendingConfig { id, app: app.clone(), next: next.clone(), done: Box::new(done) }));
    if let Err(e) = config_writer::get().map(|w| w.edit(next, previous, client, id)) {
        finish_config(id, Err(e.to_string()));
    }
}

fn finish_config(id: u64, result: Result<Vec<(usize, usize)>, String>) {
    let pending = CONFIG_PENDING.with(|p| {
        if p.borrow().as_ref().map(|p| p.id) == Some(id) { p.borrow_mut().take() } else { None }
    });
    let Some(pending) = pending else { return; };
    let ok = result.is_ok();
    match result {
        Ok(failed) => {
            *pending.app.borrow_mut() = pending.next;
            maintain_web_cache(&pending.app.borrow());
            if !failed.is_empty() { zaplaunch_sys::show_error(&failed_hotkey_message(&pending.app.borrow(), &failed)); }
        }
        Err(e) => {
            #[cfg(not(test))]
            zaplaunch_sys::show_error(&format!("配置未保存，修改未应用：{e}"));
            #[cfg(test)]
            eprintln!("configuration rejected: {e}");
        }
    }
    (pending.done)(ok);
    for (id, ok) in CONFIG_LAUNCHES.with(|q| std::mem::take(&mut *q.borrow_mut())) {
        push_tray_event(TrayEvent::LaunchCompleted(id, ok));
    }
    resume_deferred_quit();
}

fn resume_deferred_quit() {
    if QUIT_AFTER_CONFIG.load(Ordering::SeqCst) { push_tray_event(TrayEvent::Quit); }
}

/// 请求后台线程注册热键；只有最新请求的成功事件回来后才能写入配置。
fn request_hotkey(text: &str, fallback: Option<zaplaunch_sys::tray::Hotkey>) -> bool {
    if CONFIG_PENDING.with(|p| p.borrow().is_some()) || CONFIG_FILE_PENDING.load(Ordering::SeqCst) { return false; }
    let Some(hk) = zaplaunch_sys::tray::parse_hotkey(text) else { return false; };
    let id = NEXT_HOTKEY_REQUEST.fetch_add(1, Ordering::SeqCst);
    if let Ok(g) = TRAY.lock() {
        if let Some(t) = g.as_ref() {
            if let Ok(mut pending) = PENDING_HOTKEY.lock() {
                *pending = Some(PendingHotkey {
                    id,
                    text: text.to_string(),
                });
            } else {
                return false;
            }
            if t.update_hotkey(Some(hk), fallback, id) {
                return true;
            }
            if let Ok(mut pending) = PENDING_HOTKEY.lock() {
                if pending.as_ref().map(|request| request.id) == Some(id) {
                    *pending = None;
                }
            }
            return false;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// ⑤ 设置页
// ---------------------------------------------------------------------------

/// 用当前配置重刷主面板当前群组的网格。
///
/// 几个会影响「网格怎么排」的设置项（如磁贴标签显隐）改完要调它，
/// 否则界面上的开关动了、画面不动。
fn refresh_current(panel: &Weak<MainPanel>, app: &Rc<RefCell<AppConfig>>) {
    if let Some(p) = panel.upgrade() {
        let c = app.borrow();
        refresh_view(&p, &c);
    }
}

/// 主题解析：配置三值（light/dark/system）→ UI 二值（Palette.dark）。
///
/// 这是 theme 唯一的转换点，别在别处再写一遍 `theme == "dark"`
/// —— 否则「跟随系统」就会在某个分支里被悄悄丢掉。
fn resolve_dark(c: &AppConfig) -> bool {
    match c.theme.as_str() {
        "dark" => true,
        "light" => false,
        _ => zaplaunch_sys::is_system_dark(),
    }
}

/// 同步设置控件值，保留当前页面；新窗口由 UI 默认打开外观页。
///
/// 主题拆成两个值给 UI：卡片选中态用「解析后的明暗」，开关用「是否跟随系统」。
fn fill_settings(w: &SettingsWindow, c: &AppConfig) {
    // 关键：Slint 的 global 是**每个窗口实例各一份**，新建窗口的 Palette 会回到
    // 默认值（浅色）。不手动同步的话：深色主题下打开设置页，设置页自己仍是浅的
    // （2026-10-22 实测踩到，之前被"设置页盖住主面板"的截图假象掩盖了）。
    w.global::<Palette>().set_dark(resolve_dark(c));
    w.set_auto_start(zaplaunch_sys::is_auto_start_enabled());
    w.set_follow_mouse(c.follow_mouse);
    w.set_theme_mode(if resolve_dark(c) { "dark" } else { "light" }.into());
    w.set_follow_system(c.theme == "system");
    w.set_tile_show_label(c.tile_show_label);
    w.set_grid_scrollbar(c.grid_scrollbar);
    w.set_grid_lines(c.grid_lines);
    w.set_grid_columns(
        format!("{} 个", zaplaunch_core::clamp_grid_columns(c.grid_columns)).into(),
    );
    w.set_tile_order(
        match c.tile_order.as_str() {
            "usage" => "按使用次数",
            "name" => "按名称",
            _ => "手动",
        }
        .into(),
    );
    w.set_click_to_run(c.click_to_run);
    w.set_hide_after_run(c.hide_after_run);
    w.set_hide_on_blur(c.hide_on_blur);
    w.set_sound_enabled(c.sound_enabled);
    w.set_hotkey(fmt_hotkey(&c.hotkey).into());
    w.set_tray_click(if c.tray_click == "none" { "无动作" } else { "显示面板" }.into());
    w.set_esc_hide(c.esc_hide);
    w.set_history_count(c.history.len() as i32);
    w.set_history_enabled(c.history_enabled);
    w.set_hotkey_conflict("".into());
    // 数据页「配置路径」主标题：真实路径太长（会被 elide 到看不见），用
    // %APPDATA% 形式更短也更易读；「打开目录」按钮打开的仍是同一个目录。
    w.set_config_path(
        zaplaunch_core::config_path()
            .to_string_lossy()
            .replacen(
                &std::env::var("APPDATA").unwrap_or_default(),
                "%APPDATA%",
                1,
            )
            .into(),
    );
}

/// 创建设置页并接线。
fn build_settings(
    app: Rc<RefCell<AppConfig>>,
    panel: Weak<MainPanel>,
    slot: Rc<RefCell<Option<SettingsWindow>>>,
) -> SettingsWindow {
    let w = SettingsWindow::new().expect("settings window");
    w.window().on_close_requested({
        let weak = w.as_weak();
        move || {
            if let Some(w) = weak.upgrade() { w.invoke_close_clicked(); }
            slint::CloseRequestResponse::KeepWindowShown
        }
    });
    icon_scaling::bind(w.global::<zaplaunch_ui::IconRenderer>());
    sync_cache_rebuild_ui(Some(&w));
    {
        let app = app.clone();
        let ww = w.as_weak();
        w.on_rebuild_cache(move || {
            let w = ww.upgrade();
            begin_cache_rebuild(&app.borrow(),zaplaunch_core::config_dir().join("cache"),w.as_ref());
        });
    }

    // ---- 统一的设置变更出口：按 key 分发（配置字段见设计规格 7.13）----
    // 三类处理：① 只存盘（大多数布尔项）；② 存盘 + 系统副作用（自启写注册表、
    //    热键重注册）；③ 存盘 + 重刷界面（磁贴标签显隐要重排网格）。
    {
        let app = app.clone();
        let ww = w.as_weak();
        let panel = panel.clone();
        w.on_setting_changed(move |key, value| {
            let v = value.to_string();
            if key == "hotkey" {
                let fallback = zaplaunch_sys::tray::parse_hotkey(&app.borrow().hotkey);
                if !request_hotkey(&v, fallback) {
                    if let Some(x) = ww.upgrade() { x.set_hotkey_conflict("组合键无效或配置正在保存，请稍后重试".into()); }
                }
                return;
            }
            let mut next = app.borrow().clone();
            let on = v == "1";
            match key.as_str() {
                "theme" => next.theme = v,
                "auto_start" => next.auto_start = on,
                "tile_show_label" => next.tile_show_label = on,
                "grid_scrollbar" => next.grid_scrollbar = on,
                "grid_lines" => next.grid_lines = on,
                "tray_click" => next.tray_click = v,
                "grid_columns" => next.grid_columns = parse_grid_columns(&v),
                "tile_order" => next.tile_order = parse_tile_order(&v).into(),
                "follow_mouse" => next.follow_mouse = on,
                "click_to_run" => next.click_to_run = on,
                "hide_after_run" => next.hide_after_run = on,
                "hide_on_blur" => next.hide_on_blur = on,
                "sound_enabled" => next.sound_enabled = on,
                "esc_hide" => next.esc_hide = on,
                "history_enabled" => next.history_enabled = on,
                "history_limit" => next.history_limit = v.parse().unwrap_or(20),
                _ => { eprintln!("未识别的设置项：{key}"); return; }
            }
            let (app2, ww, panel) = (app.clone(), ww.clone(), panel.clone());
            commit_config(&app, |c| *c = next, move |ok| {
                let dark = resolve_dark(&app2.borrow());
                if let Some(w) = ww.upgrade() { fill_settings(&w, &app2.borrow()); w.global::<Palette>().set_dark(dark); }
                if let Some(p) = panel.upgrade() { p.global::<Palette>().set_dark(dark); }
                if ok && app2.borrow().sound_enabled { zaplaunch_sys::warm_event_sounds(); }
                refresh_current(&panel, &app2);
            });
        });
    }
    // 录制是短时有界轮询；关闭设置页、取消或捕获完成都会停止。
    let recording_timer = Rc::new(Timer::default());
    {
        let ww = w.as_weak();
        let timer = recording_timer.clone();
        let weak_timer = Rc::downgrade(&recording_timer);
        let app_record = app.clone();
        w.on_record_hotkey(move || {
            let Some(x) = ww.upgrade() else { return; };
            if x.get_recording() {
                if let Some(t) = weak_timer.upgrade() { t.stop(); }
                x.set_recording(false);
                return;
            }
            x.set_recording(true);
            let ww2 = ww.clone();
            let wt2 = weak_timer.clone();
            let app_tick = app_record.clone();
            timer.start(TimerMode::Repeated, Duration::from_millis(20), move || {
                let Some(w) = ww2.upgrade() else {
                    if let Some(t) = wt2.upgrade() { t.stop(); }
                    return;
                };
                let active = hwnd_of_window(w.window()).unwrap_or(0) == zaplaunch_sys::get_active_window();
                if !w.get_recording() || !active {
                    if let Some(t) = wt2.upgrade() { t.stop(); }
                    w.set_recording(false);
                    return;
                }
                if let Some(text) = zaplaunch_sys::polled_hotkey() {
                    let fallback = zaplaunch_sys::tray::parse_hotkey(&app_tick.borrow().hotkey);
                    if request_hotkey(&text, fallback) {
                        w.set_hotkey_conflict("".into());
                    } else {
                        w.set_hotkey_conflict("组合键无效，须含修饰键且只能有一个主键".into());
                    }
                    w.set_recording(false);
                    if let Some(t) = wt2.upgrade() { t.stop(); }
                }
            });
        });
    }
    // ---- 打开配置目录（ShellExecute 打开文件夹）----
    w.on_open_config_dir(|| {
        let dir = zaplaunch_core::config_dir();
        let _ = std::fs::create_dir_all(&dir);
        let _ = zaplaunch_sys::launch_program(&dir.display().to_string(), "", "");
    });
    // ---- 清除运行历史（只清历史，按钮与群组不动）----
    {
        let app = app.clone();
        let ww = w.as_weak();
        w.on_clear_history(move || {
            let ww = ww.clone();
            commit_config(&app, |c| c.history.clear(), move |ok| {
                if ok { if let Some(x) = ww.upgrade() { x.set_history_count(0); } }
            });
        });
    }
    // ---- 导出：系统「另存为」→ 写同一份结构的 JSON ----
    {
        let app = app.clone();
        let ww = w.as_weak();
        w.on_export_config(move || {
            let hwnd = ww
                .upgrade()
                .and_then(|x| hwnd_of_window(x.window()))
                .unwrap_or(0);
            let secs = std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let name = format!("zaplaunch-config-{secs}.json");
            if let Some(path) = zaplaunch_sys::save_file_dialog(hwnd, "导出 ZapLaunch 配置", &name) {
                if CONFIG_PENDING.with(|p| p.borrow().is_some()) || CONFIG_FILE_PENDING.swap(true, Ordering::SeqCst) {
                    zaplaunch_sys::show_error("配置正在保存或读写文件，请稍后重试。"); return;
                }
                if asset_worker().map(|w| w.requests.send(AssetRequest::Export(app.borrow().clone(), path)).is_ok()) != Some(true) {
                    CONFIG_FILE_PENDING.store(false, Ordering::SeqCst); zaplaunch_sys::show_error("导出任务无法启动。");
                }
            }
        });
    }
    // ---- 导入：系统「打开」→ 校验 → 只覆盖按钮与群组 ----
    {
        let ww = w.as_weak();
        w.on_import_config(move || {
            let hwnd = ww
                .upgrade()
                .and_then(|x| hwnd_of_window(x.window()))
                .unwrap_or(0);
            if let Some(path) = zaplaunch_sys::open_file_dialog(hwnd, "导入 ZapLaunch 配置") {
                if CONFIG_PENDING.with(|p| p.borrow().is_some()) || CONFIG_FILE_PENDING.swap(true, Ordering::SeqCst) {
                    zaplaunch_sys::show_error("配置正在保存或读写文件，请稍后重试。"); return;
                }
                if asset_worker().map(|w| w.requests.send(AssetRequest::Import(path)).is_ok()) != Some(true) {
                    CONFIG_FILE_PENDING.store(false, Ordering::SeqCst); zaplaunch_sys::show_error("导入任务无法启动。");
                }
            }
        });
    }
    // ---- 恢复默认设置（按钮与群组保留；自启要同步写回注册表）----
    {
        let app = app.clone();
        let ww = w.as_weak();
        let panel = panel.clone();
        w.on_reset_settings(move || {
            let default_key = zaplaunch_sys::tray::parse_hotkey("Ctrl+Alt+Space");
            if button_hotkey_fields(&app.borrow()).iter().any(|(_, _, text)| zaplaunch_sys::tray::parse_hotkey(text) == default_key) {
                zaplaunch_sys::show_error("默认面板快捷键已被按钮使用，请先更改该按钮的快捷键。"); return;
            }
            let (app2, ww, panel) = (app.clone(), ww.clone(), panel.clone());
            commit_config(&app, |c| { let old = c.hotkey.clone(); c.reset_settings(); c.hotkey = old; }, move |ok| {
                if !ok { return; }
                let fallback = zaplaunch_sys::tray::parse_hotkey(&app2.borrow().hotkey);
                request_hotkey("Ctrl+Alt+Space", fallback);
                if let Some(w) = ww.upgrade() { fill_settings(&w, &app2.borrow()); w.global::<Palette>().set_dark(resolve_dark(&app2.borrow())); }
                if let Some(p) = panel.upgrade() { p.global::<Palette>().set_dark(resolve_dark(&app2.borrow())); }
                refresh_current(&panel, &app2);
            });
        });
    }
    // 关闭 / 拖动
    {
        let ww = w.as_weak();
        let timer = recording_timer.clone();
        let slot = slot.clone();
        w.on_close_clicked(move || {
            timer.stop();
            if let Some(x) = ww.upgrade() {
                x.set_recording(false);
                close_child(&x, slot.clone());
            }
        });
    }
    {
        let ww = w.as_weak();
        w.on_drag_start(move || {
            if let Some(x) = ww.upgrade() {
                if let Some(h) = hwnd_of_window(x.window()) {
                    zaplaunch_sys::drag_window(h);
                }
            }
        });
    }
    w
}

// ---------------------------------------------------------------------------
// ④ 按钮属性页
// ---------------------------------------------------------------------------

#[derive(Default)]
struct PropsEdit {
    original: Option<(usize, usize, ButtonConfig)>,
    group: usize,
    snapshot: Option<zaplaunch_core::GroupConfig>,
    order: Vec<usize>,
    insert_after: Option<usize>,
    group_names: Vec<String>,
}

fn same_button_fields(a: &ButtonConfig, b: &ButtonConfig) -> bool {
    a.label == b.label && a.target == b.target && a.params == b.params
        && a.work_dir == b.work_dir && a.icon == b.icon && a.hotkey == b.hotkey
        && a.window_state == b.window_state && a.launch_on_startup == b.launch_on_startup
}

fn props_unchanged(c: &AppConfig, edit: &PropsEdit) -> bool {
    let (Some(current), Some(snapshot)) = (c.groups.get(edit.group), &edit.snapshot) else {
        return false;
    };
    current.name == snapshot.name
        && current.buttons.len() == snapshot.buttons.len()
        && current.buttons.iter().zip(&snapshot.buttons).all(|(a, b)| same_button_fields(a, b))
        && c.groups.iter().map(|g|g.name.as_str()).eq(edit.group_names.iter().map(String::as_str))
}

fn fill_props_groups(w: &PropsWindow, c: &AppConfig, gi: usize) {
    w.set_group_names(ModelRc::new(VecModel::from(c.groups.iter().map(|g|g.name.as_str().into()).collect::<Vec<_>>())));
    w.set_group_name(c.groups[gi].name.as_str().into());
    w.set_group_index(gi as i32);
    w.set_source_group_index(gi as i32);
}

fn copied_button(c: &AppConfig, gi: usize, mut button: ButtonConfig) -> ButtonConfig {
    let base = format!("{} 副本", button.label);
    button.label = (1..).map(|n| if n == 1 { base.clone() } else { format!("{base} {n}") })
        .find(|name| !c.groups[gi].buttons.iter().any(|b| b.label == *name)).unwrap();
    button.hotkey.clear();
    button.launch_on_startup = false;
    button.run_count = 0;
    button
}

const WINDOW_STATES: [&str; 4] = ["normal", "minimized", "maximized", "hidden"];

fn props_button(w: &PropsWindow, runs: u32) -> Option<ButtonConfig> {
    let label = w.get_button_label().trim().to_string();
    let target = w.get_button_target().trim().to_string();
    if label.is_empty() || target.is_empty() {
        zaplaunch_sys::show_error("名称和目标都不能为空。");
        return None;
    }
    Some(ButtonConfig {
        label, target, params: w.get_button_params().to_string(),
        work_dir: w.get_button_work_dir().to_string(), icon: w.get_button_icon().trim().to_string(),
        hotkey: w.get_button_hotkey().split('+').map(str::trim).collect::<Vec<_>>().join("+"),
        window_state: WINDOW_STATES.get(w.get_window_state().max(0) as usize).unwrap_or(&"normal").to_string(),
        launch_on_startup: w.get_launch_on_startup(), run_count: runs,
    })
}

fn fill_props_fields(w: &PropsWindow, b: &ButtonConfig) {
    w.set_button_label(b.label.as_str().into());
    w.set_button_target(b.target.as_str().into());
    w.set_button_params(b.params.as_str().into());
    w.set_button_work_dir(b.work_dir.as_str().into());
    w.set_button_icon(b.icon.as_str().into());
    w.set_button_hotkey(fmt_hotkey(&b.hotkey).into());
    w.set_window_state(WINDOW_STATES.iter().position(|s| *s == b.window_state).unwrap_or(0) as i32);
    w.set_launch_on_startup(b.launch_on_startup);
    w.set_recording(false);
    w.set_icon_status("".into());
    refresh_props_icon(w);
}

fn update_props_nav(w: &PropsWindow, edit: &PropsEdit) {
    let pos = edit.original.as_ref().and_then(|(_,bi,_)| edit.order.iter().position(|b| b == bi));
    w.set_nav_position(pos.map(|p| p as i32).unwrap_or(0));
    w.set_nav_count(if pos.is_some() { edit.order.len() as i32 } else { 0 });
}

fn save_props(w: &PropsWindow, app: &Rc<RefCell<AppConfig>>, ctx: &Rc<RefCell<PropsEdit>>, panel: &Weak<MainPanel>, done: impl FnOnce(bool) + 'static) {
    let edit = ctx.borrow();
    if !props_unchanged(&app.borrow(), &edit) {
        zaplaunch_sys::show_error("按钮或群组已变化，请关闭属性页后重新打开。"); done(false); return;
    }
    let runs = edit.original.as_ref().and_then(|(gi, bi, _)| {
        app.borrow().groups.get(*gi).and_then(|g| g.buttons.get(*bi)).map(|b| b.run_count)
    }).unwrap_or(0);
    let Some(button) = props_button(w, runs) else { done(false); return; };
    let gi = edit.group;
    let destination = w.get_group_index() as usize;
    if destination >= app.borrow().groups.len() { zaplaunch_sys::show_error("所选分组已不存在，请重新打开属性页。"); done(false); return; }
    let bi = edit.original.as_ref().map(|(_,bi,_)| *bi);
    let after = edit.insert_after;
    let original = edit.original.clone();
    let moved = destination != gi;
    let destination_index = if moved { app.borrow().groups[destination].buttons.len() }
        else { bi.or_else(|| after.map(|i| i + 1)).unwrap_or_else(|| app.borrow().groups[destination].buttons.len()) };
    let changed = moved || edit.original.as_ref().is_none_or(|(_,_,b)| !same_button_fields(b, &button));
    drop(edit);
    let (ww, app2, ctx, panel) = (w.as_weak(), app.clone(), ctx.clone(), panel.clone());
    let saved_button = button.clone();
    let view_at_save = moved.then(|| panel.upgrade().map(|p| (
        VISIBILITY_GENERATION.load(Ordering::SeqCst),
        p.get_current_group(),
        p.get_query(),
    ))).flatten();
    let finish = move |ok: bool| {
        if !ok { done(false); return; }
        if let Some(source_index) = bi.filter(|_| moved) {
            remap_pending_buttons(|group, index| {
                if group != gi { return Some((group, index)); }
                if index == source_index { return Some((destination, destination_index)); }
                Some((group, index - usize::from(index > source_index)))
            });
        } else if let Some(after) = after.filter(|_| bi.is_none() && !moved) {
            remap_pending_buttons(|group, index| {
                Some((group, index + usize::from(group == gi && index > after)))
            });
        }
        let mut unchanged_draft = true;
        if let Some(w) = ww.upgrade() {
            if ctx.borrow().original != original { refresh_current(&panel, &app2); done(false); return; }
            unchanged_draft = props_button(&w, saved_button.run_count).as_ref() == Some(&saved_button)
                && w.get_group_index() == destination as i32;
            {
                let mut edit = ctx.borrow_mut();
                edit.original = Some((destination, destination_index, saved_button));
                edit.group = destination;
                edit.insert_after = None;
                edit.snapshot = app2.borrow().groups.get(destination).cloned();
                edit.order = display_order(&app2.borrow(), destination);
                w.set_editing(true); w.set_copying(false);
                w.set_source_group_index(destination as i32);
                update_props_nav(&w, &edit);
            }
            if let (Some((generation, group, query)), Some(p)) = (&view_at_save, panel.upgrade()) {
                if *generation == VISIBILITY_GENERATION.load(Ordering::SeqCst)
                    && *group == p.get_current_group() && *query == p.get_query() {
                    p.set_current_group(destination as i32);
                    set_search_query("");
                    p.set_query("".into());
                }
            }
        }
        refresh_current(&panel, &app2);
        done(unchanged_draft);
    };
    if changed {
        commit_config(app, |c| {
            if let Some(bi) = bi { c.update_button(gi, bi, button.clone()); c.move_button_to_group(gi, bi, destination); }
            else if let Some(after) = after.filter(|_| !moved) { c.groups[gi].buttons.insert(after + 1, button); }
            else { c.add_button(destination, button); }
        }, finish);
    } else { finish(true); }
}

fn refresh_props_icon(w: &PropsWindow) {
    let target = w.get_button_target();
    let icon = w.get_button_icon();
    let def = icon_def(&icon);
    let native = resolve_native_icon(&target, &icon);
    let (letter, color) = web_icons::fallback(&target, &w.get_button_label(), &icon);
    let busy = native_icon_key(&target, &icon).map(|key| ICON_REQUESTS.with(|r| r.borrow().contains_key(&key))).unwrap_or(false);
    w.set_icon_busy(busy);
    w.set_icon_cmds(def.cmds.into());
    w.set_icon_tint(if letter.is_empty() { Color::from_rgb_u8(def.color.0, def.color.1, def.color.2) } else { Color::from_rgb_u8(color.0, color.1, color.2) });
    w.set_icon_letter(letter.into());
    w.set_has_native_icon(native.is_some());
    w.set_native_icon(native.unwrap_or_default());
}

/// 填充属性页字段。保存下标和原始按钮快照，结构变化后拒绝误改其他按钮。
fn fill_props(
    w: &PropsWindow,
    c: &AppConfig,
    ctx: &Rc<RefCell<PropsEdit>>,
    gi: usize,
    bi: usize,
) -> bool {
    let Some(g) = c.groups.get(gi) else { return false };
    let Some(b) = g.buttons.get(bi) else { return false };
    w.set_editing(true);
    w.set_copying(false);
    fill_props_groups(w,c,gi);
    *ctx.borrow_mut() = PropsEdit { original: Some((gi, bi, b.clone())), group: gi,
        snapshot: Some(g.clone()), order: display_order(c, gi), insert_after: None,
        group_names:c.groups.iter().map(|g|g.name.clone()).collect() };
    fill_props_fields(w, b);
    update_props_nav(w, &ctx.borrow());
    true
}

/// 新建模式：ctx=None 明确表示不是编辑第 0 项，保存时追加到当前群组。
fn fill_new_props(
    w: &PropsWindow,
    c: &AppConfig,
    ctx: &Rc<RefCell<PropsEdit>>,
    gi: usize,
    default_label: &str,
) -> bool {
    let Some(g) = c.groups.get(gi) else { return false; };
    w.set_editing(false);
    w.set_copying(false);
    fill_props_groups(w,c,gi);
    *ctx.borrow_mut() = PropsEdit { group: gi, snapshot: Some(g.clone()), group_names:c.groups.iter().map(|g|g.name.clone()).collect(), ..Default::default() };
    let mut b = empty_button();
    b.label = default_label.to_string();
    fill_props_fields(w, &b);
    update_props_nav(w, &ctx.borrow());
    true
}

/// 创建属性页并接线（编辑目标由 ctx 决定）。
fn build_props(
    app: Rc<RefCell<AppConfig>>,
    panel: Weak<MainPanel>,
    ctx: Rc<RefCell<PropsEdit>>,
    slot: Rc<RefCell<Option<PropsWindow>>>,
) -> PropsWindow {
    let w = PropsWindow::new().expect("props window");
    w.window().on_close_requested({
        let weak = w.as_weak();
        move || {
            if let Some(w) = weak.upgrade() { w.invoke_close_clicked(); }
            slint::CloseRequestResponse::KeepWindowShown
        }
    });
    icon_scaling::bind(w.global::<zaplaunch_ui::IconRenderer>());
    let recording_timer = Rc::new(Timer::default());
    {
        let ww = w.as_weak();
        w.on_layout_changed(move || {
            let ww = ww.clone();
            defer(0, move || {
                if let Some(w) = ww.upgrade() {
                    if let Some(hwnd) = hwnd_of_window(w.window()) {
                        let sf = w.window().scale_factor();
                        zaplaunch_sys::tray::resize_and_clamp_window(hwnd, (420.0 * sf).round() as i32,
                            ((if w.get_advanced() { 622.0 } else { 506.0 }) * sf).round() as i32,
                            (EDGE_MARGIN as f32 * sf).round() as i32);
                    }
                }
            });
        });
    }
    {
        let ww = w.as_weak();
        w.on_icon_action(move |action| {
            let Some(w) = ww.upgrade() else { return; };
            match action {
                0 => { w.set_button_icon("".into()); w.set_icon_status("".into()); }
                1 => {
                    if let Some(path) = zaplaunch_sys::open_icon_dialog(hwnd_of_window(w.window()).unwrap_or(0)) {
                        w.set_button_icon(path.into());
                        w.set_icon_status("图标引用原文件，移动文件后需重新选择".into());
                    }
                }
                2 => {
                    if let Some(key) = native_icon_key(&w.get_button_target(), &w.get_button_icon()) {
                        if !ICON_REQUESTS.with(|r| r.borrow().contains_key(&key)) && request_icon(&key, true) {
                            w.set_icon_status("正在重新获取…".into());
                        }
                    } else { w.set_icon_status("内置图标无需重新获取".into()); }
                }
                _ => return,
            }
            refresh_props_icon(&w);
        });
    }
    {
        let ww = w.as_weak();
        let app = app.clone();
        let panel = panel.clone();
        let ctx = ctx.clone();
        let timer = recording_timer.clone();
        w.on_navigate(move |direction| {
            let Some(w) = ww.upgrade() else { return; };
            if w.get_group_index() != w.get_source_group_index() { return; }
            timer.stop(); w.set_recording(false); w.set_icon_menu(false);
            let (gi, order, next) = {
                let edit = ctx.borrow();
                let Some((gi,bi,_)) = edit.original.as_ref() else { return; };
                let Some(pos) = edit.order.iter().position(|b| b == bi) else { return; };
                let next = pos as i32 + direction;
                if !(0..edit.order.len() as i32).contains(&next) { return; }
                (*gi, edit.order.clone(), next as usize)
            };
            let (ww, app2, ctx2) = (w.as_weak(), app.clone(), ctx.clone());
            save_props(&w, &app, &ctx, &panel, move |ok| {
                if !ok { return; }
                if let Some(w) = ww.upgrade() { if fill_props(&w, &app2.borrow(), &ctx2, gi, order[next]) {
                    ctx2.borrow_mut().order = order;
                    update_props_nav(&w, &ctx2.borrow());
                } }
            });
        });
    }
    {
        let ww = w.as_weak();
        let app = app.clone();
        let ctx = ctx.clone();
        let timer = recording_timer.clone();
        w.on_duplicate(move || {
            let Some(w) = ww.upgrade() else { return; };
            timer.stop(); w.set_recording(false); w.set_icon_menu(false);
            let mut edit = ctx.borrow_mut();
            if !props_unchanged(&app.borrow(), &edit) {
                zaplaunch_sys::show_error("群组或按钮已变化，请重新打开属性页。"); return;
            }
            let Some((_,bi,_)) = edit.original.as_ref() else { return; };
            let bi = *bi;
            let Some(button) = props_button(&w, 0) else { return; };
            let destination = w.get_group_index() as usize;
            if destination >= app.borrow().groups.len() {
                zaplaunch_sys::show_error("所选分组已不存在，请重新打开属性页。"); return;
            };
            let button = copied_button(&app.borrow(), destination, button);
            edit.original = None;
            edit.insert_after = Some(bi);
            w.set_editing(false); w.set_copying(true);
            fill_props_fields(&w, &button);
            update_props_nav(&w, &edit);
        });
    }
    {
        let ww = w.as_weak();
        let preview_timer = Rc::new(Timer::default());
        w.on_preview_icon(move || {
            let weak = ww.clone();
            preview_timer.start(TimerMode::SingleShot, Duration::from_millis(500), move || {
                if let Some(w) = weak.upgrade() { refresh_props_icon(&w); }
            });
        });
    }
    {
        let ww = w.as_weak();
        let timer = recording_timer.clone();
        let weak_timer = Rc::downgrade(&recording_timer);
        w.on_record_hotkey(move || {
            let Some(pw) = ww.upgrade() else { return; };
            if pw.get_recording() {
                timer.stop();
                pw.set_recording(false);
                return;
            }
            pw.set_recording(true);
            let ww2 = ww.clone();
            let wt2 = weak_timer.clone();
            timer.start(TimerMode::Repeated, Duration::from_millis(20), move || {
                let Some(pw) = ww2.upgrade() else {
                    if let Some(t) = wt2.upgrade() { t.stop(); }
                    return;
                };
                let active = hwnd_of_window(pw.window()).unwrap_or(0) == zaplaunch_sys::get_active_window();
                if !pw.get_recording() || !active {
                    if let Some(t) = wt2.upgrade() { t.stop(); }
                    pw.set_recording(false);
                    return;
                }
                if let Some(text) = zaplaunch_sys::polled_hotkey() {
                    pw.set_button_hotkey(fmt_hotkey(&text).into());
                    pw.set_recording(false);
                    if let Some(t) = wt2.upgrade() { t.stop(); }
                }
            });
        });
    }
    {
        let ww = w.as_weak();
        let timer = recording_timer.clone();
        w.on_clear_hotkey(move || {
            timer.stop();
            if let Some(pw) = ww.upgrade() {
                pw.set_recording(false);
                pw.set_button_hotkey("".into());
            }
        });
    }

    // 保存：读表单 → 覆盖配置 → 存盘 → 刷新网格 → 关闭
    {
        let app = app.clone();
        let panel = panel.clone();
        let ctx = ctx.clone();
        let ww = w.as_weak();
        let slot = slot.clone();
        let timer = recording_timer.clone();
        w.on_save_clicked(move || {
            timer.stop();
            let Some(pw) = ww.upgrade() else { return; };
            pw.set_recording(false);
            let (ww, slot) = (pw.as_weak(), slot.clone());
            save_props(&pw, &app, &ctx, &panel, move |ok| {
                if ok { if let Some(w) = ww.upgrade() { close_child(&w, slot); } }
            });
        });
    }
    // 删除：移除 → 存盘 → 刷新 → 关闭
    {
        let app = app.clone();
        let panel = panel.clone();
        let ctx = ctx.clone();
        let ww = w.as_weak();
        let slot = slot.clone();
        let timer = recording_timer.clone();
        w.on_delete_clicked(move || {
            timer.stop();
            let Some((gi, bi, original)) = ctx.borrow().original.clone() else { return };
            let unchanged = props_unchanged(&app.borrow(), &ctx.borrow())
                && app.borrow().groups.get(gi).and_then(|g| g.buttons.get(bi))
                    .is_some_and(|button| same_button_fields(button, &original));
            if !unchanged {
                zaplaunch_sys::show_error("按钮或群组已在其他窗口中变化，请关闭属性页后重新打开。");
                return;
            }
            let (app2, panel, ww, slot) = (app.clone(), panel.clone(), ww.clone(), slot.clone());
            commit_config(&app, |c| { c.remove_button(gi, bi); }, move |ok| {
                if !ok { return; }
                remap_pending_buttons(|group, button| {
                    if group != gi { return Some((group, button)); }
                    (button != bi).then(|| (group, button - usize::from(button > bi)))
                });
                refresh_current(&panel, &app2);
                if let Some(w) = ww.upgrade() { close_child(&w, slot); }
            });
        });
    }
    // 取消 / 关闭
    {
        let ww = w.as_weak();
        let slot = slot.clone();
        let timer = recording_timer.clone();
        w.on_cancel_clicked(move || {
            timer.stop();
            if let Some(x) = ww.upgrade() {
                close_child(&x, slot.clone());
            }
        });
    }
    {
        let ww = w.as_weak();
        let slot = slot.clone();
        let timer = recording_timer.clone();
        w.on_close_clicked(move || {
            timer.stop();
            if let Some(x) = ww.upgrade() {
                close_child(&x, slot.clone());
            }
        });
    }
    // 拖动
    {
        let ww = w.as_weak();
        w.on_drag_start(move || {
            if let Some(x) = ww.upgrade() {
                if let Some(h) = hwnd_of_window(x.window()) {
                    zaplaunch_sys::drag_window(h);
                }
            }
        });
    }
    w
}

/// 关于页按需创建：常驻但从未打开时，不提前保留一份窗口和渲染缓冲。
fn build_about(slot: Rc<RefCell<Option<AboutWindow>>>) -> AboutWindow {
    let w = AboutWindow::new().expect("about window");
    icon_scaling::bind(w.global::<zaplaunch_ui::IconRenderer>());
    w.set_version_text(env!("ZAPLAUNCH_DISPLAY_VERSION").into());
    w.window().on_close_requested({
        let weak = w.as_weak();
        move || {
            if let Some(w) = weak.upgrade() { w.invoke_close_clicked(); }
            slint::CloseRequestResponse::KeepWindowShown
        }
    });
    w.on_close_clicked({
        let weak = w.as_weak();
        move || {
            if let Some(x) = weak.upgrade() { close_child(&x, slot.clone()); }
        }
    });
    w.on_drag_start({
        let weak = w.as_weak();
        move || {
            if let Some(x) = weak.upgrade() {
                if let Some(h) = hwnd_of_window(x.window()) {
                    zaplaunch_sys::drag_window(h);
                }
            }
        }
    });
    w
}

// ---------------------------------------------------------------------------
// 托盘宿主：UI 句柄与后台消息线程之间的那道桥
// ---------------------------------------------------------------------------

/// 托盘事件的处理现场（仅在 UI 线程 thread_local 内持有与访问）。
struct Host {
    panel: Weak<MainPanel>,
    app: Rc<RefCell<AppConfig>>,
    settings: Rc<RefCell<Option<SettingsWindow>>>,
    about: Rc<RefCell<Option<AboutWindow>>>,
    props: Rc<RefCell<Option<PropsWindow>>>,
    props_ctx: Rc<RefCell<PropsEdit>>,
}

/// 面板可见态。Slint 不暴露 is-visible，而显示/收起全部由本文件发起，所以自己记账。
/// 用 AtomicBool 是因为它要同时被呼出路径、收起路径和失焦定时器读取。
static PANEL_VISIBLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 后台线程投递过来的事件队列（只装 Copy 的事件枚举，不涉及任何 UI 句柄）。
static PENDING: Mutex<Vec<TrayEvent>> = Mutex::new(Vec::new());

/// WM_DROPFILES 只搬运路径字符串；配置与 UI 仍在 UI 线程统一提交。
static PENDING_DROPS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// 上一次处理 Esc 的时刻（去抖用）。初值取「很久以前」，保证第一次一定生效。
static LAST_ESC: Mutex<Option<std::time::Instant>> = Mutex::new(None);

/// 按键上升沿锁存（轮询兜底用）：true = 上一轮采样时键是按下状态。
static ESC_LATCH: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static DOWN_LATCH: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn last_esc() -> std::time::Instant {
    let g = LAST_ESC.lock().unwrap_or_else(|e| e.into_inner());
    g.unwrap_or_else(|| std::time::Instant::now() - std::time::Duration::from_secs(60))
}

fn set_last_esc(t: std::time::Instant) {
    if let Ok(mut g) = LAST_ESC.lock() {
        *g = Some(t);
    }
}

/// 托盘句柄全局槽。
///
/// 设置页改热键时要用它：`RegisterHotKey` 必须由创建窗口的线程（＝后台托盘线程）
/// 调用，UI 线程只能投递消息，所以句柄得放在一个两边都够得着的地方。
static TRAY: Mutex<Option<zaplaunch_sys::tray::TrayHandle>> = Mutex::new(None);

/// 当前显示模型中的键盘选中下标，常态和搜索态共用。
static SEL_IDX: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// 方向键在网格内移动，不跨行回绕，也不越过最后一格。direction: 0左/1右/2上/3下。
fn next_grid_selection(current: usize, len: usize, cols: usize, direction: i32) -> Option<usize> {
    if len == 0 || cols == 0 || current >= len {
        return None;
    }
    let next = match direction {
        0 if current % cols > 0 => current - 1,
        1 if current % cols + 1 < cols && current + 1 < len => current + 1,
        2 if current >= cols => current - cols,
        3 if current + cols < len => current + cols,
        _ => return None,
    };
    Some(next)
}

fn group_index_after_move(index: usize, from: usize, to: usize) -> usize {
    if index == from { to }
    else if from < index && index <= to { index - 1 }
    else if to <= index && index < from { index + 1 }
    else { index }
}

fn show_group_modal(p: &MainPanel) {
    p.set_group_modal_open(true);
    p.invoke_focus_group_input();
    let weak = p.as_weak();
    defer(15, move || {
        if let Some(p) = weak.upgrade() {
            if !p.get_group_modal_open() || !PANEL_VISIBLE.load(Ordering::SeqCst) { return; }
            p.invoke_focus_group_input();
            recover_panel_input_method(&p);
        }
    });
}

fn bind_group_actions(panel: &MainPanel, app: &Rc<RefCell<AppConfig>>) {
    // ---- ⑭ 分组管理：新建分组（「+ 分组」按钮）与编辑/重命名/删除分组（右键分组标签） ----
    let group_edit_ctx: Rc<RefCell<Option<(usize, Vec<String>)>>> = Rc::new(RefCell::new(None));
    {
        let weak = panel.as_weak();
        let ctx = group_edit_ctx.clone();
        panel.on_add_group_clicked(move || {
            let Some(p) = weak.upgrade() else { return; };
            *ctx.borrow_mut() = None;
            p.set_history_open(false);
            p.set_group_modal_editing(false);
            p.set_group_can_delete(false);
            p.set_group_modal_name("".into());
            show_group_modal(&p);
        });
    }
    {
        let weak = panel.as_weak();
        let appc = app.clone();
        let ctx = group_edit_ctx.clone();
        panel.on_group_right_clicked(move |i| {
            let Some(p) = weak.upgrade() else { return; };
            let idx = i.max(0) as usize;
            let (name, names) = {
                let c = appc.borrow();
                let Some(g) = c.groups.get(idx) else { return; };
                (g.name.clone(), c.groups.iter().map(|g| g.name.clone()).collect::<Vec<_>>())
            };
            p.set_group_modal_position(idx as i32);
            p.set_group_can_delete(names.len() > 1);
            *ctx.borrow_mut() = Some((idx,names));
            p.set_history_open(false);
            p.set_group_modal_editing(true);
            p.set_group_modal_name(name.into());
            show_group_modal(&p);
        });
    }
    {
        let weak = panel.as_weak();
        panel.on_group_modal_cancel(move || {
            let Some(p) = weak.upgrade() else { return; };
            p.set_group_modal_open(false);
            p.invoke_clear_search_focus();
        });
    }
    {
        let weak = panel.as_weak();
        let appc = app.clone();
        let ctx = group_edit_ctx.clone();
        panel.on_group_modal_save(move || {
            let Some(p) = weak.upgrade() else { return; };
            let name = p.get_group_modal_name().trim().to_string();
            let submitted_name = name.clone();
            if name.is_empty() {
                zaplaunch_sys::show_error("分组名称不能为空。");
                return;
            }
            let edit = ctx.borrow().clone();
            let edit_idx = edit.as_ref().map(|(index,_)| *index);
            let mut next = appc.borrow().clone();
            if edit.as_ref().map(|(_,names)| !next.groups.iter().map(|g| &g.name).eq(names.iter())).unwrap_or(false) {
                zaplaunch_sys::show_error("分组已变化，请关闭后重新编辑。"); return;
            }
            let target_group = match edit_idx {
                Some(idx) => {
                    if !next.rename_group(idx, name) {
                        zaplaunch_sys::show_error("已存在同名分组，请换一个名称。");
                        return;
                    }
                    let destination = p.get_group_modal_position();
                    if destination < 0 || !next.move_group(idx,destination as usize) { return; }
                    destination as usize
                }
                None => {
                    let Some(new_idx) = next.add_group(name) else {
                        zaplaunch_sys::show_error("已存在同名分组，请换一个名称。");
                        return;
                    };
                    new_idx
                }
            };
            let (app2, pp) = (appc.clone(), p.as_weak());
            let ctx = ctx.clone();
            commit_config(&appc, |c| *c = next, move |ok| {
            if !ok { return; }
            let Some(p) = pp.upgrade() else { return; };
            let selected = if let Some(from) = edit_idx.filter(|from| *from != target_group) {
                // 启动结果仍在异步返回，同步旧坐标以免使用次数记到错误分组。
                remap_pending_buttons(|group, button| Some((group_index_after_move(group, from, target_group), button)));
                group_index_after_move(p.get_current_group().max(0) as usize,from,target_group)
            } else { target_group };
            let same_editor = *ctx.borrow() == edit;
            let keep_draft = p.get_group_modal_open() && (!same_editor
                || p.get_group_modal_name().trim() != submitted_name
                || (edit_idx.is_some() && p.get_group_modal_position() != target_group as i32));
            if keep_draft && same_editor {
                *ctx.borrow_mut() = Some((target_group, app2.borrow().groups.iter().map(|g| g.name.clone()).collect()));
                p.set_group_modal_editing(true);
            }
            p.set_group_modal_open(keep_draft);
            p.set_current_group(selected as i32);
            {
                let c = app2.borrow();
                refresh_view(&p, &c);
            }
            if !keep_draft { p.invoke_clear_search_focus(); }
            });
        });
    }
    {
        let weak = panel.as_weak();
        let appc = app.clone();
        let ctx = group_edit_ctx.clone();
        panel.on_group_modal_delete(move || {
            let Some(p) = weak.upgrade() else { return; };
            let Some((idx,names)) = ctx.borrow().clone() else { return; };
            let mut next = appc.borrow().clone();
            if !next.groups.iter().map(|g| &g.name).eq(names.iter()) {
                zaplaunch_sys::show_error("分组已变化，请关闭后重新编辑。"); return;
            }
            if !next.remove_group(idx) {
                zaplaunch_sys::show_error("至少需要保留一个分组。");
                return;
            }
            let (app2, pp, ctx) = (appc.clone(), p.as_weak(), ctx.clone());
            let generation = VISIBILITY_GENERATION.load(Ordering::SeqCst);
            commit_config(&appc, |c| { c.remove_group(idx); }, move |ok| {
            if !ok { return; }
            remap_pending_buttons(|group, button| {
                (group != idx).then(|| (group - usize::from(group > idx), button))
            });
            let Some(p) = pp.upgrade() else { return; };
            let old_cur = p.get_current_group().max(0) as usize;
            let new_cur = group_selection_after_delete(old_cur, idx, next.groups.len());
            let same_view = generation == VISIBILITY_GENERATION.load(Ordering::SeqCst);
            let open_editor = ctx.borrow().clone();
            if let Some((editing, _)) = open_editor.filter(|(_, snapshot)| *snapshot == names) {
                if editing == idx {
                    p.set_group_modal_open(false);
                    *ctx.borrow_mut() = None;
                    if same_view { p.invoke_clear_search_focus(); }
                } else {
                    let group = group_selection_after_delete(editing, idx, next.groups.len());
                    *ctx.borrow_mut() = Some((group, app2.borrow().groups.iter().map(|g| g.name.clone()).collect()));
                    let position = p.get_group_modal_position().max(0) as usize;
                    p.set_group_modal_position(group_selection_after_delete(position, idx, next.groups.len()) as i32);
                    p.set_group_can_delete(next.groups.len() > 1);
                }
            }
            p.set_current_group(new_cur as i32);
            refresh_view(&p, &app2.borrow());
            });
        });
    }

}

fn group_selection_after_delete(current: usize, deleted: usize, new_len: usize) -> usize {
    debug_assert!(new_len > 0);
    if deleted < current {
        current - 1
    } else {
        current.min(new_len - 1)
    }
}

/// 投递一个托盘事件：**后台线程和 UI 线程都可以调**，处理路径只有一条。
///
/// 后台线程先入队，再用 `invoke_from_event_loop` 唤醒 UI 线程去取 —— 这是零轮询的关键：
/// 唤醒由系统消息驱动，UI 线程平时不被定时器打扰。
fn push_tray_event(ev: TrayEvent) {
    let mut fire = false;
    if let Ok(mut q) = PENDING.lock() {
        q.push(ev);
        fire = true;
    }
    if fire {
        // 注意：闭包是 fn item，不捕获任何东西 —— 天然满足 Send + 'static，
        // 也避免「闭包在后台线程被 drop」把 UI 句柄带过去析构。
        if slint::invoke_from_event_loop(drain_tray_events).is_err() {
            eprintln!("[zaplaunch] 事件循环还没起来，托盘事件已入队等待下一轮");
        }
    }
}

fn notify_panel_activated() { push_tray_event(TrayEvent::PanelActivated); }

fn notify_panel_tab_fallback(backwards: bool) {
    push_tray_event(TrayEvent::PanelTabFallback(backwards, VISIBILITY_GENERATION.load(Ordering::SeqCst)));
}

fn push_dropped_files(paths: Vec<String>) {
    if let Ok(mut q) = PENDING_DROPS.lock() {
        q.extend(paths);
    } else {
        return;
    }
    if slint::invoke_from_event_loop(drain_dropped_files).is_err() {
        eprintln!("[zaplaunch] 文件拖入时事件循环不可用");
    }
}

fn drain_dropped_files() {
    let paths = match PENDING_DROPS.lock() {
        Ok(mut q) if !q.is_empty() => std::mem::take(&mut *q),
        _ => return,
    };
    HOST.with(|slot| {
        let host_opt = slot.borrow_mut().take();
        if let Some(host) = host_opt {
            host.add_dropped_files(paths);
            *slot.borrow_mut() = Some(host);
        }
    });
}

/// UI 线程：取走全部待办事件逐个处理。
fn drain_tray_events() {
    let events: Vec<TrayEvent> = match PENDING.lock() {
        Ok(mut q) => std::mem::take(&mut *q),
        Err(_) => return,
    };
    if events.is_empty() {
        return;
    }
    let mut remaining = Vec::new();
    for event in events {
        if let TrayEvent::ConfigSaved(id, result) = event { finish_config(id, result); }
        else { remaining.push(event); }
    }
    HOST.with(|slot| {
        let host_opt = slot.borrow_mut().take();
        if let Some(host) = host_opt {
            for ev in remaining {
                host.handle(ev);
            }
            *slot.borrow_mut() = Some(host);
        }
    });
}

impl Host {
    fn finish_launch(&self, p: &MainPanel, id: u64, ok: bool) {
        PENDING_LAUNCHES.with(|q| {
            if let Some(request) = q.borrow().get(&id) { request.timeout.stop(); }
        });
        if CONFIG_PENDING.with(|p| p.borrow().is_some()) {
            CONFIG_LAUNCHES.with(|q| q.borrow_mut().push((id, ok)));
            return;
        }
        let Some(request) = PENDING_LAUNCHES.with(|q| q.borrow_mut().remove(&id)) else { return; };
        if !ok {
            if !request.timed_out { zaplaunch_sys::show_error(&format!("无法启动「{}」。\n请检查目标、参数和工作目录。\n{}", request.label, request.target)); }
            refresh_current(&p.as_weak(), &self.app);
            return;
        }
        if !request.timed_out { p.set_history_open(false); }
        if request.hide_generation.is_some()
            && request.hide_generation == panel_hide_generation(true) {
            hide_panel(p);
        }
        let mut next = self.app.borrow().clone();
        apply_launch_success(&mut next, &request);
        if !CONFIG_READ_ONLY.load(Ordering::SeqCst) {
            match config_writer::get().and_then(|w| w.save(&next, false)) {
                Ok(()) => *self.app.borrow_mut() = next,
                Err(e) => zaplaunch_sys::show_error(&format!("启动统计未保存：{e}")),
            }
        }
        refresh_current(&p.as_weak(), &self.app);
    }

    fn open_new_button(&self) {
        if raise_open_props(&self.props) { return; }
        let Some(p) = self.panel.upgrade() else { return; };
        {
            let c = self.app.borrow();
            show_panel(&p, &c);
        }
        if self.props.borrow().is_none() {
            *self.props.borrow_mut() = Some(build_props(
                self.app.clone(), self.panel.clone(), self.props_ctx.clone(), self.props.clone(),
            ));
        }
        let gi = (p.get_current_group() as usize).min(self.app.borrow().groups.len().saturating_sub(1));
        let default_label = current_search_trimmed();
        let opened = {
            let c = self.app.borrow();
            self.props.borrow().as_ref()
                .map(|w| fill_new_props(w, &c, &self.props_ctx, gi, &default_label))
                .unwrap_or(false)
        };
        if opened {
            if let Some(w) = self.props.borrow().as_ref() {
                w.global::<Palette>().set_dark(resolve_dark(&self.app.borrow()));
                let _ = w.show();
                w.window().request_redraw();
                center_on_parent(w, &self.panel);
                round_corners_later(w);
            }
        }
    }

    fn add_dropped_files(&self, paths: Vec<String>) {
        let Some(p) = self.panel.upgrade() else { return; };
        if DROP_PENDING.swap(true, Ordering::SeqCst) {
            zaplaunch_sys::show_error("上一批文件仍在检查，请稍后重试。");
            return;
        }
        let groups = self.app.borrow().groups.iter().map(|g| g.name.clone()).collect();
        let gi = p.get_current_group().max(0) as usize;
        let generation = VISIBILITY_GENERATION.load(Ordering::SeqCst);
        if asset_worker().map(|w| w.requests.send(AssetRequest::Drop(paths, groups, gi, generation)).is_ok()) != Some(true) {
            DROP_PENDING.store(false, Ordering::SeqCst);
            zaplaunch_sys::show_error("无法检查拖入的文件，请重试。");
        }
    }

    fn finish_drop(&self, buttons: Vec<ButtonConfig>, rejected: usize, groups: Vec<String>, gi: usize, generation: usize) {
        DROP_PENDING.store(false, Ordering::SeqCst);
        let Some(p) = self.panel.upgrade() else { return; };
        if groups != self.app.borrow().groups.iter().map(|g| g.name.clone()).collect::<Vec<_>>() || gi >= groups.len() {
            zaplaunch_sys::show_error("检查文件期间群组已改变，请重新拖入。");
            return;
        }
        if buttons.is_empty() {
            zaplaunch_sys::show_error("没有可添加的文件。请拖入真实存在的文件、文件夹或快捷方式。");
            return;
        }
        let mut next = self.app.borrow().clone();
        let mut added = 0usize;
        for button in buttons {
            if next.add_button(gi, button) { added += 1; }
        }
        if added == 0 { return; }
        let sound_on = next.sound_enabled;
        let (app, pp) = (self.app.clone(), p.as_weak());
        commit_config(&self.app, |c| *c = next, move |ok| {
        if !ok { return; }
        let Some(p) = pp.upgrade() else { return; };
        if sound_on {
            zaplaunch_sys::play_event_sound(zaplaunch_sys::EventSound::Click);
        }
        if let Some(visible_generation) = panel_hide_generation(true) {
            if visible_generation == generation {
                set_search_query("");
                p.set_query("".into());
            }
            refresh_view(&p, &app.borrow());
            p.window().request_redraw();
        }
        if rejected > 0 {
            zaplaunch_sys::show_error(&format!("已添加 {added} 项；另有 {rejected} 个路径不存在或无法读取，未添加。"));
        }
        });
    }

    fn handle(&self, ev: TrayEvent) {
        let Some(p) = self.panel.upgrade() else {
            return;
        };
        match ev {
            TrayEvent::ConfigSaved(id, result) => finish_config(id, result),
            TrayEvent::PanelActivated => { recover_panel_input_method(&p); }
            TrayEvent::PanelTabFallback(backwards, generation) => {
                if generation == VISIBILITY_GENERATION.load(Ordering::SeqCst)
                    && PANEL_VISIBLE.load(Ordering::SeqCst)
                    && hwnd_of(&p).is_some_and(zaplaunch_sys::tray::is_foreground_window)
                {
                    recover_panel_input_method(&p);
                    p.invoke_cycle_focus(backwards);
                }
            }
            TrayEvent::StartupLaunchesCompleted(failed) => {
                if !failed.is_empty() { zaplaunch_sys::show_error(&format!("以下按钮自动运行失败，其余条目已继续处理：\n{}", failed.join("\n"))); }
            }
            TrayEvent::LaunchCompleted(id, ok) => self.finish_launch(&p, id, ok),
            TrayEvent::LaunchTimedOut(id) => {
                let label = PENDING_LAUNCHES.with(|q| {
                    let mut pending = q.borrow_mut();
                    let request = pending.get_mut(&id)?;
                    request.timed_out = true;
                    request.hide_generation = None;
                    Some(request.label.clone())
                });
                if let Some(label) = label {
                    refresh_current(&p.as_weak(), &self.app);
                    zaplaunch_sys::show_error(&format!("「{label}」启动超时。Windows 仍在处理该请求。"));
                }
            }
            TrayEvent::AssetReady => {
                ASSET_NOTIFY_PENDING.store(false, Ordering::SeqCst);
                let Some(worker) = asset_worker() else { return; };
                let Ok(results) = worker.results.lock() else { return; };
                let mut images = std::collections::HashMap::new();
                let mut shortcuts_changed = false;
                let mut props_changed = false;
                let mut processed = 0;
                while processed < 32 {
                    let Ok(result) = results.try_recv() else { break; };
                    processed += 1;
                    match result {
                        AssetResult::Export(result) => {
                            CONFIG_FILE_PENDING.store(false, Ordering::SeqCst);
                            if let Err(e) = result { zaplaunch_sys::show_error(&format!("导出失败：{e}")); }
                            resume_deferred_quit();
                        }
                        AssetResult::Import(result) => {
                            CONFIG_FILE_PENDING.store(false, Ordering::SeqCst);
                            match result {
                                Err(e) => {
                                    zaplaunch_sys::show_error(&format!("导入失败：{e}"));
                                    resume_deferred_quit();
                                }
                                Ok(loaded) => {
                                    let (app, panel) = (self.app.clone(), self.panel.clone());
                                    commit_config(&self.app, |c| c.groups = loaded.groups, move |ok| {
                                        if !ok { return; }
                                        remap_pending_buttons(|_, _| None);
                                        if let Some(p) = panel.upgrade() {
                                            let current = (p.get_current_group().max(0) as usize).min(app.borrow().groups.len() - 1);
                                            p.set_current_group(current as i32); p.set_history_open(false); p.set_group_modal_open(false);
                                        }
                                        refresh_current(&panel, &app);
                                        zaplaunch_sys::show_info("配置已导入。导入按钮的自动运行已关闭，需要时请在属性中重新开启。");
                                    });
                                }
                            }
                        }
                        AssetResult::Drop(buttons, rejected, groups, gi, generation) => self.finish_drop(buttons, rejected, groups, gi, generation),
                        AssetResult::Icon(key, id, pixels, cache_ok) => {
                            if !complete_icon(&key, id) { continue; }
                            let ok = pixels.is_some();
                            CACHE_REBUILD.with(|state| {
                                if let Some(job) = state.borrow_mut().as_mut() {
                                    if job.active.get(&key) == Some(&id) {
                                        job.active.remove(&key);
                                        if !ok || !cache_ok { job.failed += 1; }
                                    }
                                }
                            });
                            let image = pixels.map(|(w, h, rgba)| {
                                let buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&rgba, w, h);
                                slint::Image::from_rgba8(buf)
                            });
                            ICON_CACHE.with(|cache| {
                                let mut cache = cache.borrow_mut();
                                if let Some(image) = image {
                                    cache.insert(key.clone(), Some(image.clone()));
                                    images.insert(key.clone(), image);
                                } else if cache.get(&key).is_none() { cache.insert(key.clone(), None); }
                            });
                            if let Some(w) = self.props.borrow().as_ref() {
                                if native_icon_key(&w.get_button_target(), &w.get_button_icon()).as_deref() == Some(&key) {
                                    w.set_icon_status(if ok { "图标已更新" } else { "未能获取图标；保留已有图标或使用默认图标" }.into());
                                    props_changed = true;
                                }
                            }
                        }
                        AssetResult::Shortcut(key, target) => {
                            PENDING_SHORTCUTS.with(|pending| { pending.borrow_mut().remove(&key); });
                            LNK_TARGET_CACHE.with(|cache| { cache.borrow_mut().insert(key, target); });
                            shortcuts_changed = true;
                        }
                        AssetResult::CacheCleaned(failed) => {
                            let keep: std::collections::HashSet<_> = configured_icon_keys(&self.app.borrow()).into_iter().collect();
                            ICON_CACHE.with(|cache| cache.borrow_mut().entries.retain(|key,_| keep.contains(key)));
                            icon_scaling::clear_cache();
                            finish_cache_rebuild(failed,self.settings.borrow().as_ref());
                        }
                    }
                }
                drop(results);
                if processed == 32 { notify_asset_ready(); }
                if shortcuts_changed && p.get_searching() {
                    refresh_view(&p, &self.app.borrow());
                } else {
                    patch_asset_icons(&p, &self.app.borrow(), &images);
                }
                if props_changed { if let Some(w) = self.props.borrow().as_ref() { refresh_props_icon(w); } }
                advance_cache_rebuild(&self.app.borrow(),self.settings.borrow().as_ref());
            }
            TrayEvent::ButtonHotkey(gi, bi, hk) => {
                if self.props.borrow().as_ref().map(|w| w.get_recording()).unwrap_or(false)
                    || self.settings.borrow().as_ref().map(|w| w.get_recording()).unwrap_or(false) { return; }
                let matches = self.app.borrow().groups.get(gi).and_then(|g| g.buttons.get(bi))
                    .and_then(|b| zaplaunch_sys::tray::parse_hotkey(&b.hotkey)) == Some(hk);
                if matches { launch_at(&p, &self.app, gi, bi); }
            }
            TrayEvent::StartupHotkeyFailed => zaplaunch_sys::show_error("面板全局快捷键注册失败：组合键可能已被其他程序占用。仍可从托盘打开设置并更换。"),
            TrayEvent::StartupTrayFailed => zaplaunch_sys::show_error("托盘初始化失败。请退出 ZapLaunch 后重试。"),
            TrayEvent::StartupButtonHotkeysFailed(failed) => zaplaunch_sys::show_error(&failed_hotkey_message(&self.app.borrow(), &failed)),
            // ①→② 呼出：热键 / 菜单「显示面板」走同一条路径（规格 7.12）
            TrayEvent::Hotkey if self.props.borrow().as_ref().map(|w| w.get_recording()).unwrap_or(false) => {}
            TrayEvent::Hotkey | TrayEvent::ShowPanel | TrayEvent::TrayDropHover => {
                let c = self.app.borrow();
                show_panel(&p, &c);
                PANEL_VISIBLE.store(true, Ordering::SeqCst);
            }
            // 托盘左键：动作可配（「显示面板」/「无动作」，见 7.8-c）
            TrayEvent::TrayClick => {
                if self.app.borrow().tray_click != "none" {
                    let c = self.app.borrow();
                    show_panel(&p, &c);
                    PANEL_VISIBLE.store(true, Ordering::SeqCst);
                }
            }
            TrayEvent::NewButton => {
                self.open_new_button();
            }
            TrayEvent::Settings => {
                if self.settings.borrow().is_none() {
                    *self.settings.borrow_mut() =
                        Some(build_settings(self.app.clone(), self.panel.clone(), self.settings.clone()));
                }
                let opened = {
                    let c = self.app.borrow();
                    self.settings
                        .borrow()
                        .as_ref()
                        .map(|w| fill_settings(w, &c))
                        .is_some()
                };
                if opened {
                    if let Some(w) = self.settings.borrow().as_ref() {
                        let _ = w.show();
                        center_on_parent(w, &self.panel);
                        round_corners_later(w);
                    }
                }
            }
            // 热键录制完成（后台钩子捕获到组合键）→ 落盘 + 回写设置页
            TrayEvent::HotkeyRecorded(buf) => {
                let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
                if end > 0 {
                    let text = String::from_utf8_lossy(&buf[..end]).to_string();
                    let fallback = zaplaunch_sys::tray::parse_hotkey(&self.app.borrow().hotkey);
                    if request_hotkey(&text, fallback) {
                        if let Some(w) = self.settings.borrow().as_ref() {
                            w.set_hotkey_conflict("".into());
                        }
                    }
                }
                // 无论成功还是按 Esc 取消，录制态都要收掉
                if let Some(w) = self.settings.borrow().as_ref() {
                    w.set_recording(false);
                }
            }
            TrayEvent::HotkeyRegisterSucceeded(id) => {
                let pending = PENDING_HOTKEY.lock().ok().and_then(|mut p| {
                    if p.as_ref().map(|request| request.id) == Some(id) {
                        p.take()
                    } else {
                        None
                    }
                });
                let Some(pending) = pending else { return; };
                let text = pending.text;
                let old = self.app.borrow().hotkey.clone();
                let mut next = self.app.borrow().clone();
                next.hotkey = text.clone();
                let settings = self.settings.clone();
                commit_config(&self.app, |c| *c = next, move |ok| {
                    if ok {
                        if let Some(w) = settings.borrow().as_ref() { w.set_hotkey(fmt_hotkey(&text).into()); w.set_hotkey_conflict("".into()); }
                    } else {
                        let fallback = zaplaunch_sys::tray::parse_hotkey(&old);
                        request_hotkey(&old, fallback);
                    }
                });
            }
            // 只处理最新请求的失败；系统层恢复的是配置中已提交的热键，不是过期请求。
            TrayEvent::HotkeyRegisterFailed(id) => {
                let is_latest = PENDING_HOTKEY.lock().ok().map(|mut pending| {
                    if pending.as_ref().map(|request| request.id) == Some(id) {
                        *pending = None;
                        true
                    } else {
                        false
                    }
                }).unwrap_or(false);
                if !is_latest {
                    return;
                }
                if let Some(w) = self.settings.borrow().as_ref() {
                    w.set_hotkey(fmt_hotkey(&self.app.borrow().hotkey).into());
                    w.set_hotkey_conflict("该组合键已被其他程序占用，请换一个".into());
                }
                resume_deferred_quit();
            }
            // 关于页复用已建窗口，主题与主面板保持一致。
            TrayEvent::About => {
                if self.about.borrow().is_none() {
                    *self.about.borrow_mut() = Some(build_about(self.about.clone()));
                }
                if let Some(w) = self.about.borrow().as_ref() {
                    // 先同步主题再显示，避免首帧沿用默认浅色或旧主题。
                    w.global::<Palette>().set_dark(resolve_dark(&self.app.borrow()));
                    let _ = w.show();
                    w.window().request_redraw();
                    center_on_parent(w, &self.panel);
                    round_corners_later(w);
                }
            }
            // 只停事件循环；托盘注销放在 run() 返回之后（那里才拿得到 TrayHandle）
            TrayEvent::Quit => {
                if CONFIG_PENDING.with(|p| p.borrow().is_some()) || CONFIG_FILE_PENDING.load(Ordering::SeqCst)
                    || PENDING_HOTKEY.lock().map(|p| p.is_some()).unwrap_or(false) {
                    QUIT_AFTER_CONFIG.store(true, Ordering::SeqCst);
                    if !QUIT_DEADLINE_STARTED.swap(true, Ordering::SeqCst) {
                        defer(5000, || {
                            if QUIT_AFTER_CONFIG.swap(false, Ordering::SeqCst) {
                                QUIT_CONFIG_EXPIRED.store(true, Ordering::SeqCst);
                                zaplaunch_sys::show_error("退出时等待配置操作超时。该操作可能未完成，请重新打开后检查。");
                                let _ = slint::quit_event_loop();
                            }
                        });
                    }
                } else {
                    QUIT_AFTER_CONFIG.store(false, Ordering::SeqCst);
                    let _ = slint::quit_event_loop();
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 入口
// ---------------------------------------------------------------------------

fn main() {
    // 第一件事：声明 DPI 感知。必须在任何窗口 / Slint 初始化之前
    //（不做的话 125% 屏上界面发糊，而且呼出定位会整体偏掉，详见 zaplaunch_sys::enable_dpi_awareness）
    zaplaunch_sys::enable_dpi_awareness();

    let args: Vec<String> = std::env::args().collect();
    let isolated = args.iter().any(|a| a == "--isolated-test");
    if isolated && !args.iter().any(|a| a == "--config-dir") {
        zaplaunch_sys::show_error("--isolated-test 必须指定独立的 --config-dir。"); return;
    }
    if isolated { std::env::set_var("ZAPLAUNCH_AUTOSTART", "0"); }
    let stress = args.iter().any(|a| a == "--stress");
    // 常驻模式：只进托盘不弹面板（开机自启就是这个参数，见 zaplaunch-sys::set_auto_start）
    let tray_only = args.iter().any(|a| a == "--tray-only");
    // 验收模式：禁用自动收起。截图/单测脚本会把窗口拉前台又切走，
    // 若按正常失焦逻辑会把它收起来，导致截图拿到空白。
    let keep_alive = isolated || args.iter().any(|a| a == "--keep-alive");
    if let Some(i) = args.iter().position(|a| a == "--config-dir") {
        let Some(dir) = args.get(i + 1).filter(|v| !v.is_empty()) else {
            zaplaunch_sys::show_error("--config-dir 后必须提供配置目录。");
            return;
        };
        std::env::set_var(zaplaunch_core::CONFIG_DIR_ENV, dir);
    }
    let secs: u64 = args
        .iter()
        .position(|a| a == "--secs")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let instance = if isolated {
        zaplaunch_sys::SingleInstance::acquire_isolated(&zaplaunch_core::config_dir())
    } else { zaplaunch_sys::SingleInstance::acquire() };
    let Some(_instance) = instance else {
        if isolated { eprintln!("隔离测试实例无法启动"); return; }
        if !tray_only {
            // 第一实例可能已持有互斥体但尚未创建托盘窗口，短暂等待其完成初始化。
            let mut notified = false;
            for _ in 0..40 {
                if zaplaunch_sys::SingleInstance::notify_existing() {
                    notified = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            if !notified {
                eprintln!("ZapLaunch 已在运行，但现有实例暂未响应唤醒请求");
            }
        }
        eprintln!("ZapLaunch 已在运行，第二个实例退出");
        return;
    };

    eprintln!("[zaplaunch] boot: stderr ok");

    // 损坏或版本不兼容的配置进入只读保护，绝不自动覆盖。
    let loaded = zaplaunch_core::load_or_init();
    CONFIG_READ_ONLY.store(loaded.read_only, Ordering::SeqCst);
    if let Some(warning) = loaded.warning.as_deref() {
        eprintln!("{warning}");
        zaplaunch_sys::show_error(warning);
    }
    if loaded.config.sound_enabled { zaplaunch_sys::warm_event_sounds(); }
    let app: Rc<RefCell<AppConfig>> = Rc::new(RefCell::new(loaded.config));

    let panel = MainPanel::new().expect("failed to create window");
    panel.on_tab_consumed(zaplaunch_sys::mark_panel_tab_consumed);
    icon_scaling::bind(panel.global::<zaplaunch_ui::IconRenderer>());

    // 初始渲染：群组 0 的网格 + 标签行 + 空状态
    {
        let c = app.borrow();
        refresh_group(&panel, &c, 0);
    }
    panel.set_current_group(0);
    panel.set_phase(0.0);
    panel.set_stress_mode(stress);

    // ---- 主题：初始值来自配置（"system" 按 Windows 当前深浅色启动） ----
    {
        let dark = resolve_dark(&app.borrow());
        panel.global::<Palette>().set_dark(dark);
    }

    // ---- 窗口圆角（Win11+ 走 DWM；Win10 无害降级为直角，见 zaplaunch-sys 注释） ----
    // 必须在事件循环内调用：winit 要等 run() 启动才真正创建窗口，
    // run() 之前 hwnd_of 拿到的是 None（2026-10-21 实测：提前调用无效果）。
    let corner_timer = Timer::default();
    {
        let weak = panel.as_weak();
        corner_timer.start(TimerMode::SingleShot, Duration::from_millis(50), move || {
            if let Some(p) = weak.upgrade() {
                if let Some(hwnd) = hwnd_of(&p) {
                    zaplaunch_sys::round_window_corners(hwnd);
                    zaplaunch_sys::install_file_drop(hwnd, push_dropped_files, notify_panel_activated, notify_panel_tab_fallback);
                }
            }
        });
    }

    // ---- 窗口动作：宿主侧处理，UI 层只抛意图 ----
    {
        let weak = panel.as_weak();
        panel.on_minimize_clicked(move || {
            if let Some(p) = weak.upgrade() {
                if let Some(hwnd) = hwnd_of(&p) {
                    mark_panel_hidden(&p);
                    zaplaunch_sys::minimize_window(hwnd);
                }
            }
        });
    }
    {
        let weak = panel.as_weak();
        panel.on_close_clicked(move || {
            if let Some(p) = weak.upgrade() {
                // 关闭 = 收起到托盘（进程保留；真正退出只有托盘菜单那一条路）
                hide_panel(&p);
            }
        });
    }
    {
        let weak = panel.as_weak();
        panel.window().on_close_requested(move || {
            if let Some(p) = weak.upgrade() { mark_panel_hidden(&p); }
            slint::CloseRequestResponse::HideWindow
        });
    }
    // 标题栏拖动：从 Slint 拿到真实 HWND，再交给 Win32 系统接管
    // （不用 GetForegroundWindow —— 鼠标按下瞬间窗口未必已激活，会拖错窗口）
    {
        let weak = panel.as_weak();
        panel.on_drag_window(move || {
            if let Some(p) = weak.upgrade() {
                if let Some(hwnd) = hwnd_of(&p) {
                    zaplaunch_sys::drag_window(hwnd);
                }
            }
        });
    }

    panel.on_tab_to_search({
        let app = app.clone();
        move || {
            if app.borrow().sound_enabled {
                zaplaunch_sys::play_event_sound(zaplaunch_sys::EventSound::GroupSwitch);
            }
        }
    });

    // ---- 群组切换：UI 已改 current-group，这里按新群组重建网格数据 ----
    {
        let weak = panel.as_weak();
        let app = app.clone();
        panel.on_group_clicked(move |i| {
            if let Some(p) = weak.upgrade() {
                p.set_keyboard_navigation(false);
                set_search_query("");
                p.set_query("".into());
                p.set_history_open(false);
                let c = app.borrow();
                if c.sound_enabled {
                    zaplaunch_sys::play_event_sound(zaplaunch_sys::EventSound::GroupSwitch);
                }
                // 切组结束搜索，统一出口显示目标组内容。
                let _ = i; // 索引以 UI 侧 current-group 为准，这里只按新状态重建网格
                refresh_view(&p, &c);
            }
        });
    }

    // ---- 核心动作：点击 / 双击磁贴启动程序（受 click_to_run 设置项控制） ----
    {
        let weak = panel.as_weak();
        panel.on_tile_drag_started(move || {
            if let Some(p) = weak.upgrade() { p.set_keyboard_navigation(false); p.set_drag_insert(-1); }
        });
    }
    {
        let weak = panel.as_weak();
        let app = app.clone();
        panel.on_tile_drag_moved(move |row, col, x, y| {
            let Some(p) = weak.upgrade() else { return; };
            if p.get_drag_insert() == -2 { return; }
            let c = app.borrow();
            if !p.get_reorder_enabled() || p.get_searching() || row < 0 || col < 0 {
                p.set_drag_insert(-1);
                return;
            }
            let gi = p.get_current_group().max(0) as usize;
            let count = c.groups.get(gi).map(|g| g.buttons.len()).unwrap_or(0);
            let cols = zaplaunch_core::clamp_grid_columns(c.grid_columns) as usize;
            let source = row as usize * cols + col as usize;
            if source >= count || hit_at(source) != Some((gi, source)) {
                p.set_drag_insert(-1);
                return;
            }
            let height = if c.tile_show_label { 82.0 } else { p.get_tile_width() + 2.0 };
            let before = grid_insert_at(row as usize, col as usize, x, y, cols, count, tile_width(cols), height);
            p.set_drag_insert(before.map(|n| n as i32).unwrap_or(-1));
        });
    }
    {
        let weak = panel.as_weak();
        let app = app.clone();
        panel.on_tile_drag_finished(move |row, col| {
            let Some(p) = weak.upgrade() else { return; };
            let before = p.get_drag_insert();
            p.set_drag_insert(-1);
            if !p.get_reorder_enabled() || p.get_searching() || row < 0 || col < 0 || before < 0 { return; }
            let gi = p.get_current_group().max(0) as usize;
            let cols = zaplaunch_core::clamp_grid_columns(app.borrow().grid_columns) as usize;
            let from = row as usize * cols + col as usize;
            let before = before as usize;
            let count = app.borrow().groups.get(gi).map(|g| g.buttons.len()).unwrap_or(0);
            if from >= count || before > count || hit_at(from) != Some((gi, from)) || before == from || before == from + 1 { return; }
            let (app2, pp) = (app.clone(), p.as_weak());
            let to = if before > from { before - 1 } else { before };
            commit_config(&app, |c| { c.move_button(gi, from, before); }, move |ok| {
                if !ok { return; }
                remap_pending_buttons(|group, index| {
                    Some((group, if group == gi { group_index_after_move(index, from, to) } else { index }))
                });
                if let Some(p) = pp.upgrade() {
                    SEL_IDX.store(to, Ordering::SeqCst);
                    refresh_view(&p, &app2.borrow());
                }
            });
        });
    }
    {
        let weak = panel.as_weak();
        panel.on_tile_drag_cancelled(move || {
            if let Some(p) = weak.upgrade() { p.set_drag_insert(-1); }
        });
    }
    {
        let weak = panel.as_weak();
        let app = app.clone();
        panel.on_tile_clicked(move |row, col| {
            if let Some(p) = weak.upgrade() {
                p.set_keyboard_navigation(false);
                let idx = row as usize * CUR_COLS.load(Ordering::SeqCst) + col as usize;
                // ⚠️ 格子下标 ≠ 按钮下标：搜索态下网格是跨群组命中序列，
                //    必须经 HIT_MAP 换算（常态下它恰好是「当前群组第 idx 个」）
                let Some((g, b)) = hit_at(idx) else {
                    eprintln!("磁贴命中映射失效，已取消启动: index={idx}");
                    return;
                };
                // 历史浮层开着就顺手收掉（下拉菜单的常规行为）
                p.set_history_open(false);
                // 只更新旧、新两项，保留 TouchArea 实例以维持双击状态。
                update_grid_selection(&p, idx);
                if app.borrow().click_to_run {
                    launch_at(&p, &app, g, b);
                }
            }
        });
    }
    {
        let weak = panel.as_weak();
        let app = app.clone();
        panel.on_tile_double_clicked(move |row, col| {
            if let Some(p) = weak.upgrade() {
                p.set_keyboard_navigation(false);
                if app.borrow().click_to_run {
                    // 单击即启动模式下第一次点击已触发启动，忽略双击第二下避免重复启动
                    return;
                }
                let idx = row as usize * CUR_COLS.load(Ordering::SeqCst) + col as usize;
                let Some((g, b)) = hit_at(idx) else {
                    return;
                };
                update_grid_selection(&p, idx);
                p.set_history_open(false);
                launch_at(&p, &app, g, b);
            }
        });
    }

    // ---- ⑤ 设置页：齿轮点击。与托盘菜单「设置…」共用同一条路径（push_tray_event），
    //      避免「两个入口两份打开逻辑」导致的状态不一致。
    let settings_slot: Rc<RefCell<Option<SettingsWindow>>> = Rc::new(RefCell::new(None));
    panel.on_settings_clicked(|| push_tray_event(TrayEvent::Settings));

    // ---- ⑪ 关于页：托盘菜单第一次打开时才创建，避免白占常驻内存 ----
    let about_slot: Rc<RefCell<Option<AboutWindow>>> = Rc::new(RefCell::new(None));

    // ---- 主题切换持久化：UI 已翻转 Palette.dark，这里存盘 ----
    {
        let weak = panel.as_weak();
        let app = app.clone();
        let slot = settings_slot.clone();
        panel.on_theme_clicked(move || {
            let Some(p) = weak.upgrade() else { return; };
            let requested_dark = p.global::<Palette>().get_dark();
            let (app2, weak, slot) = (app.clone(), weak.clone(), slot.clone());
            commit_config(&app, |c| c.theme = if requested_dark { "dark" } else { "light" }.into(), move |_| {
                let dark = resolve_dark(&app2.borrow());
                if let Some(p) = weak.upgrade() { p.global::<Palette>().set_dark(dark); }
                if let Some(s) = slot.borrow().as_ref() { fill_settings(s, &app2.borrow()); s.global::<Palette>().set_dark(dark); }
            });
        });
    }

    // ---- ④ 按钮属性页：右键磁贴打开（惰性创建，编辑目标存 ctx） ----
    let props_slot: Rc<RefCell<Option<PropsWindow>>> = Rc::new(RefCell::new(None));
    let props_ctx: Rc<RefCell<PropsEdit>> = Rc::new(RefCell::new(PropsEdit::default()));
    {
        let weak = panel.as_weak();
        let app = app.clone();
        let slot = props_slot.clone();
        let ctx = props_ctx.clone();
        panel.on_tile_right_clicked(move |row, col| {
            if raise_open_props(&slot) { return; }
            let slot_idx = row as usize * CUR_COLS.load(Ordering::SeqCst) + col as usize;
            // 映射失效时绝不回退猜测目标，避免打开或编辑另一个按钮。
            let Some((g_idx, b_idx)) = hit_at(slot_idx) else {
                eprintln!("磁贴命中映射失效，已取消属性操作: index={slot_idx}");
                return;
            };
            if slot.borrow().is_none() {
                let w = build_props(app.clone(), weak.clone(), ctx.clone(), slot.clone());
                *slot.borrow_mut() = Some(w);
            }
            let ok = {
                let c = app.borrow();
                slot.borrow()
                    .as_ref()
                    .map(|w| fill_props(w, &c, &ctx, g_idx, b_idx))
                    .unwrap_or(false)
            };
            if ok {
                if let Some(w) = slot.borrow().as_ref() {
                    // 新建窗口的 Palette 是独立副本，必须在显示前同步当前主题。
                    w.global::<Palette>().set_dark(resolve_dark(&app.borrow()));
                    let _ = w.show();
                    w.window().request_redraw();
                    center_on_parent(w, &weak);
                    round_corners_later(w);
                }
            }
        });
    }

    {
        let weak = panel.as_weak();
        panel.on_cycle_results(move |backwards| {
            if let Some(p) = weak.upgrade() { cycle_search_result(&p, backwards); }
        });
    }
    // 网格方向键导航；输入框内的光标键由输入框自行处理。
    {
        let weak = panel.as_weak();
        panel.on_navigate_grid(move |direction| {
            let Some(p) = weak.upgrade() else { return; };
            if p.get_search_input_focused() || p.get_group_modal_open() {
                return;
            }
            // UI 已处理 ↓，避免轮询再把同一次按键当作打开历史。
            if direction == 3 { DOWN_LATCH.store(true, Ordering::SeqCst); }
            let len = HIT_MAP.with(|map| map.borrow().len());
            p.set_keyboard_navigation(true);
            let current = SEL_IDX.load(Ordering::SeqCst);
            let cols = CUR_COLS.load(Ordering::SeqCst);
            if let Some(next) = next_grid_selection(current, len, cols, direction) {
                p.set_history_open(false);
                update_grid_selection(&p, next);
            }
        });
    }

    // Esc：收起面板（状态机 ③→④ 的一条路径，见设计规格 7.12）。
    // 开关 esc_hide 关闭时，Esc 只留给搜索清词用（批次 E），这里不收面板。
    {
        let weak = panel.as_weak();
        let app = app.clone();
        panel.on_esc_clicked(move || {
            // 去抖：Slint 焦点链与本文件的轮询兜底可能同时命中同一个物理按键，
            // 150ms 内的第二次一律丢弃（连按 Esc 本身也没有独立语义）。
            let now = std::time::Instant::now();
            if now.duration_since(last_esc()) < std::time::Duration::from_millis(150) {
                return;
            }
            set_last_esc(now);
            if let Some(p) = weak.upgrade() {
                if p.get_drag_insert() >= 0 {
                    p.set_drag_insert(-2);
                    return;
                }
                // 先收弹窗/历史，结果焦点先回搜索栏，再清词，最后收起面板。
                if p.get_group_modal_open() {
                    p.set_group_modal_open(false);
                    p.invoke_clear_search_focus();
                    return;
                }
                if p.get_history_open() {
                    p.set_history_open(false);
                    return;
                }
                if p.get_searching() && !p.get_search_input_focused() && p.get_keyboard_navigation() {
                    p.invoke_focus_search();
                    return;
                }
                let has_q = !current_search_trimmed().is_empty();
                if has_q {
                    set_search_query("");
                    p.set_query("".into());     // 别名同步 → 输入框内容一起抹掉
                    p.invoke_focus_search();
                    let c = app.borrow();
                    refresh_view(&p, &c);
                    return;
                }
                if !app.borrow().esc_hide {
                    return;
                }
                hide_panel(&p);
            }
        });
    }
    // 面板「＋」与托盘「新建按钮…」共用同一条新建链路，不再留空回调。
    panel.on_add_clicked(|| push_tray_event(TrayEvent::NewButton));

    bind_group_actions(&panel,&app);

    // ---- ③ 搜索定位（规格 7.2）：键入即跨群组过滤，不建索引、不加定时器 ----
    {
        let weak = panel.as_weak();
        let appc = app.clone();
        panel.on_search_changed(move |s| {
            set_search_query(&s);
            if let Some(p) = weak.upgrade() {
                p.set_keyboard_navigation(false);
                // 一打字就收起历史浮层（搜索优先，规格 7.11）
                if p.get_history_open() && !s.trim().is_empty() {
                    p.set_history_open(false);
                }
                let c = appc.borrow();
                refresh_view(&p, &c);
            }
        });
    }
    // Enter 启动当前选中项；搜索无命中时保留新建按钮入口。
    {
        let weak = panel.as_weak();
        let appc = app.clone();
        panel.on_search_accepted(move || {
            let Some(p) = weak.upgrade() else {
                return;
            };
            if p.get_searching() && p.get_hit_count() == 0 {
                if p.get_search_pending() { return; }
                push_tray_event(TrayEvent::NewButton);
                return;
            }
            let idx = SEL_IDX.load(Ordering::SeqCst);
            let Some((gi, bi)) = hit_at(idx) else {
                return;
            };
            launch_at(&p, &appc, gi, bi);
        });
    }
    // ↓：搜索框为空时展开 / 收起运行历史浮层（规格 7.11）
    {
        let weak = panel.as_weak();
        let appc = app.clone();
        panel.on_search_down(move || {
            if let Some(p) = weak.upgrade() {
                // 搜索或模态弹窗优先：有词、编辑群组时 ↓ 不展开历史
                if p.get_searching() || p.get_group_modal_open() {
                    return;
                }
                let open = !p.get_history_open();
                p.set_history_open(open);
                if open {
                    let c = appc.borrow();
                    p.set_history(ModelRc::new(VecModel::from(history_rows(&c))));
                }
            }
        });
    }
    // 历史行点击 → 重开该目标，同步累加对应按钮使用次数并更新该条历史的时间戳到首位
    {
        let weak = panel.as_weak();
        let appc = app.clone();
        panel.on_history_clicked(move |i| {
            let Some(p) = weak.upgrade() else {
                return;
            };
            let idx = i as usize;
            let hit = {
                let c = appc.borrow();
                c.history.get(idx).map(|h| {
                    (
                        h.target.clone(),
                        h.params.clone(),
                        h.work_dir.clone(),
                        h.label.clone(),
                        h.window_state.clone(),
                        c.sound_enabled,
                        c.hide_after_run,
                    )
                })
            };
            let Some((target, params, work_dir, label, window_state, sound_on, hide_after)) = hit else { return; };
            queue_launch(&p, PendingLaunch {
                source: LaunchSource::History, target, params, dir: work_dir, label, window_state,
                sound_on, hide_generation: panel_hide_generation(hide_after), hist_on: appc.borrow().history_enabled,
                network: false, timed_out: false, timeout: Timer::default(),
                hist_max: appc.borrow().history_limit,
            });
        });
    }
    // 清除历史：清内存 + 落盘 + 收起浮层
    {
        let weak = panel.as_weak();
        let appc = app.clone();
        panel.on_history_clear(move || {
            let weak = weak.clone();
            commit_config(&appc, |c| c.history.clear(), move |ok| {
                if ok { if let Some(p) = weak.upgrade() {
                    p.set_history(ModelRc::new(VecModel::from(Vec::<HistoryRow>::new())));
                    p.set_history_open(false);
                } }
            });
        });
    }

    // 压力模式：16ms 一帧（约 60fps）持续改 phase，强制每帧重绘
    let stress_timer = Timer::default();
    if stress {
        let weak = panel.as_weak();
        let mut phase: f32 = -1.0;
        stress_timer.start(TimerMode::Repeated, Duration::from_millis(16), move || {
            phase += 0.02;
            if phase > 1.0 {
                phase = -1.0;
            }
            if let Some(p) = weak.upgrade() {
                p.set_phase(phase);
            }
        });
    }

    // 自动退出，便于脚本化测量
    let exit_timer = Timer::default();
    if secs > 0 {
        exit_timer.start(TimerMode::SingleShot, Duration::from_secs(secs), || {
            let _ = slint::quit_event_loop();
        });
    }

    // ---- ⑫ 托盘 + 全局热键：常驻入口（批次 A，见设计规格 7.10 / 7.12） ----
    let hk_str = app.borrow().hotkey.clone();
    let hotkey = zaplaunch_sys::tray::parse_hotkey(&hk_str).unwrap_or_else(|| {
        eprintln!(
            "[zaplaunch] 热键配置无法解析：{:?}，回退默认 Ctrl + Alt + Space",
            hk_str
        );
        zaplaunch_sys::tray::Hotkey::default_hotkey()
    });

    // 宿主放入 UI 线程 thread_local：托盘线程只投递 Copy 的事件枚举，真正的处理永远在 UI 线程
    HOST.with(|slot| {
        *slot.borrow_mut() = Some(Host {
            panel: panel.as_weak(),
            app: app.clone(),
            settings: settings_slot.clone(),
            about: about_slot.clone(),
            props: props_slot.clone(),
            props_ctx: props_ctx.clone(),
        });
    });

    // 起后台消息线程（托盘图标 + 热键注册都在这里面；空闲时线程挂在 GetMessage 上，零 CPU）
    // 句柄放进全局槽：设置页改热键时要用它投递重注册消息。
    let tray_handle = zaplaunch_sys::tray::start(if isolated { None } else { Some(hotkey) }, |ev| push_tray_event(ev));
    let (keys, mut failed) = button_hotkeys(&app.borrow());
    let client = tray_handle.button_hotkey_client();
    if !keys.is_empty() && !isolated {
        if let Err(e) = std::thread::Builder::new().name("zaplaunch-hotkeys".into()).spawn(move || {
            match client.update(keys.clone()) {
                Some(unavailable) => failed.extend(unavailable),
                None => failed.extend(keys.iter().map(|&(g, b, _)| (g, b))),
            }
            if !failed.is_empty() { push_tray_event(TrayEvent::StartupButtonHotkeysFailed(failed)); }
        }) { zaplaunch_sys::show_error(&format!("无法注册按钮快捷键：{e}")); }
    } else if !failed.is_empty() { push_tray_event(TrayEvent::StartupButtonHotkeysFailed(failed)); }
    if let Ok(mut g) = TRAY.lock() {
        *g = Some(tray_handle);
    }

    // ---- Esc / ↓ 轮询兜底（状态机 ③→④ 的第三条路径）----
    // 窗口「隐藏 → 显示」之后 winit 不总能把键盘焦点交回（见文件头注释），
    // 只靠 Slint 焦点链会表现为「敲字母没反应 / Esc 收不起来」。
    // 这里用 60ms 一次的 GetAsyncKeyState 查询兜底，上升沿触发一次。
    // 前置条件：面板可见 **且** 前台就是主面板——子窗口或别的程序按键都不能转发给面板。
    let esc_timer = Rc::new(Timer::default());
    {
        let weak = panel.as_weak();
        esc_timer.start(TimerMode::Repeated, Duration::from_millis(60), move || {
            if !PANEL_VISIBLE.load(Ordering::SeqCst) {
                ESC_LATCH.store(false, Ordering::SeqCst);
                DOWN_LATCH.store(false, Ordering::SeqCst);
                return;
            }
            let Some(p) = weak.upgrade() else { return; };
            let panel_foreground = hwnd_of(&p)
                .map(zaplaunch_sys::tray::is_foreground_window)
                .unwrap_or(false);
            if !panel_foreground {
                // 子窗口或外部程序前台时不转发按键；同步物理状态，回到面板后仍需重新按下。
                ESC_LATCH.store(zaplaunch_sys::is_esc_pressed(), Ordering::SeqCst);
                DOWN_LATCH.store(zaplaunch_sys::is_key_pressed(0x28), Ordering::SeqCst);
                return;
            }
            if p.get_group_modal_open() || p.get_search_input_focused() {
                // 输入控件自行处理 Esc/方向键；IME 组合态取消和候选导航不得泄漏成面板快捷键。
                ESC_LATCH.store(zaplaunch_sys::is_esc_pressed(), Ordering::SeqCst);
                DOWN_LATCH.store(zaplaunch_sys::is_key_pressed(0x28), Ordering::SeqCst);
                return;
            }
            let esc = zaplaunch_sys::is_esc_pressed();
            if esc && !ESC_LATCH.load(Ordering::SeqCst) {
                p.invoke_esc_clicked();
            }
            ESC_LATCH.store(esc, Ordering::SeqCst);

            let down = zaplaunch_sys::is_key_pressed(0x28); // VK_DOWN
            if down && !DOWN_LATCH.load(Ordering::SeqCst) {
                p.invoke_search_down();
            }
            DOWN_LATCH.store(down, Ordering::SeqCst);
        });
    }
    esc_timer.stop();
    ESC_TIMER.with(|slot| *slot.borrow_mut() = Some(esc_timer.clone()));

    // ---- 失焦自动收起（状态机 ③→④ 的另一条路径）----
    // Win32 没有「窗口失焦」的订阅型事件，只能查询。这里做三件事把开销压到最低：
    //   ① 面板不可见时直接返回；② 配置关闭时不查；③ 只在**焦点不在本进程**时才收
    //      （否则设置页/属性页一抢焦点，面板就跟着没了）。
    // 查询本身只是 GetForegroundWindow + GetWindowThreadProcessId 两次系统调用。
    let blur_timer = Rc::new(Timer::default());
    if !keep_alive {
        let weak = panel.as_weak();
        let app = app.clone();
        blur_timer.start(TimerMode::Repeated, Duration::from_millis(200), move || {
            if !PANEL_VISIBLE.load(Ordering::SeqCst) {
                return;
            }
            if !app.borrow().hide_on_blur {
                return;
            }
            // 文件拖动期间焦点仍在资源管理器等来源窗口，不能因此把刚呼出的
            // 拖放目标收掉；同时若鼠标指针仍在主面板窗口矩形内（例如准备释放拖放或悬停操作），
            // 也保持面板显示，移出窗口且失焦后再自动收起。
            if zaplaunch_sys::is_key_pressed(0x01) || zaplaunch_sys::is_key_pressed(0x02) {
                return;
            }
            if zaplaunch_sys::tray::is_foreground_in_process() {
                return;
            }
            if let Some(p) = weak.upgrade() {
                if hwnd_of(&p).map(zaplaunch_sys::tray::is_cursor_in_window).unwrap_or(false) {
                    return;
                }
                hide_panel(&p);
            }
        });
        blur_timer.stop();
        BLUR_TIMER.with(|slot| *slot.borrow_mut() = Some(blur_timer.clone()));
    }

    // ---- 启动姿态 ----
    // 常驻模式（--tray-only，开机自启也走这个）：只留托盘，不弹面板。
    // 普通模式：按正常呼出流程显示一次，让人第一眼看到东西。
    //
    // 注意必须**在事件循环启动之后**才能显示：winit 的窗口是在事件循环里创建的，
    // run_event_loop 之前调 show() 不会显示（2026-10-22 实测：进程起来了但 IsWindowVisible=0，
    // 之前用 panel.run() 时是它在内部 show，改成 run_event_loop_until_quit 后这个活儿归我们）。
    // 变量必须绑住：Timer 一旦被 drop，定时器就取消了（换 crontab 想一想就明白 ——
    // 这里掉坑过，表现为"10ms 后什么事都没发生"）
    let boot_timer = Timer::default();
    if !tray_only {
        let weak = panel.as_weak();
        let app2 = app.clone();
        boot_timer.start(TimerMode::SingleShot, Duration::from_millis(10), move || {
            if let Some(p) = weak.upgrade() {
                let c = app2.borrow();
                show_panel(&p, &c);
                PANEL_VISIBLE.store(true, Ordering::SeqCst);
            }
        });
    }

    // 先呈现首屏，再预载其他分组；图标读取和系统提取仍在后台执行。
    let preload_app = app.clone();
    defer(100, move || {
        let c = preload_app.borrow();
        maintain_web_cache(&c);
        preload_local_icons(&c);
    });

    // 事件循环启动前到达的托盘/拖放事件只入队，无法 invoke；首轮主动补取，避免滞留。
    let startup = startup_buttons(&app.borrow(), !CONFIG_READ_ONLY.load(Ordering::SeqCst) && !stress && !keep_alive && secs == 0);
    if !startup.is_empty() {
        defer(0, move || {
            if let Err(e) = std::thread::Builder::new().name("zaplaunch-startup".into()).spawn(move || {
                let failed = run_startup_buttons(&startup, |b| execute_launch(&b.target, &b.params, &b.work_dir, &b.window_state));
                push_tray_event(TrayEvent::StartupLaunchesCompleted(failed));
            }) { zaplaunch_sys::show_error(&format!("无法创建自动运行线程：{e}")); }
        });
    }
    defer(0, || {
        drain_tray_events();
        drain_dropped_files();
    });

    // 事件循环用 run_event_loop_until_quit()，两个坑都不能踩：
    // ① panel.run()：会无条件 show 窗口，常驻模式就藏不住了；
    // ② run_event_loop()：最后一个窗口**隐藏**时它会返回（Slint 把 hide 也算窗口关闭），
    //    表现为「Esc 收起后进程直接没了，退出码 0」——2026-10-22 实测踩到。
    //    tray-only 这类后台程序必须显式 quit 才返回。
    slint::run_event_loop_until_quit().expect("event loop failed");

    if !QUIT_CONFIG_EXPIRED.load(Ordering::SeqCst) { config_writer::flush(); }
    // 收尾：事件循环结束 → 取回宿主（释放 UI 句柄）+ 注销托盘图标与热键
    HOST.with(|slot| {
        drop(slot.borrow_mut().take());
    });
    // 托盘句柄从全局槽取回并 drop → 注销图标、结束消息线程
    if let Ok(mut g) = TRAY.lock() {
        drop(g.take());
    }
}

#[cfg(test)]
mod tests;
