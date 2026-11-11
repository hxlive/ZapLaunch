// zaplaunch-sys：系统集成层。整个工程只有这一层允许直接调 Win32 API。
//
// 提供：窗口圆角 / 启动程序 / 最小化 / 还原 / 拖动 / 开机自启（注册表 Run 键）。
// 另有 tray 子模块：托盘图标 + 全局热键 + 鼠标跟随呼出（见 tray.rs 顶部注释）。
// 刻意不用 `windows` crate —— 全部手工 extern 声明，零依赖、零体积增量
// （crate 一装就是几百 KB 静态库）。
//
// 2026-10-21 加入：面板走无系统标题栏（no-frame），标题栏自绘，所以必须自己提供拖动。
// 2026-10-22 加入：开机自启（advapi32 注册表 API）。

#![allow(non_snake_case, non_camel_case_types)]

use std::ffi::c_void;
use std::sync::OnceLock;

type HWND = *mut c_void;

pub mod tray;
mod sound;
pub use sound::{EventSound,play_event_sound,warm_event_sounds};

/// 缓存调试追踪开关（避免在窗口过程和全局钩子中反复查询系统环境块）。
pub fn trace_enabled() -> bool {
    static TRACE: OnceLock<bool> = OnceLock::new();
    *TRACE.get_or_init(|| std::env::var_os("ZAPLAUNCH_TRACE").is_some())
}

/// 进程级单实例守卫。句柄随进程退出自动释放。
pub struct SingleInstance(*mut c_void);

impl SingleInstance {
    /// 获取 ZapLaunch 命名互斥锁；已有实例运行时返回 None。
    pub fn acquire() -> Option<Self> {
        Self::acquire_named("Local\\ZapLaunch.SingleInstance")
    }

    pub fn acquire_isolated(dir: &std::path::Path) -> Option<Self> {
        use std::hash::{Hash, Hasher};
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        std::fs::create_dir_all(dir).ok()?;
        dir.canonicalize().ok()?.hash(&mut hash);
        Self::acquire_named(&format!("Local\\ZapLaunch.Test.{:x}", hash.finish()))
    }

    fn acquire_named(name: &str) -> Option<Self> {
        unsafe {
            let name = wide(name);
            let handle = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
            if handle.is_null() || GetLastError() == 183 {
                if !handle.is_null() {
                    CloseHandle(handle);
                }
                None
            } else {
                Some(Self(handle))
            }
        }
    }

    /// 通知已运行的实例呼出主面板（再次双击桌面图标时唤醒已有实例）。
    pub fn notify_existing() -> bool {
        tray::notify_existing_tray_to_show()
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0); }
    }
}

/// 非阻断流程的明确错误提示；调用方仍负责保留或恢复现场。
pub fn show_error(message: &str) {
    show_message(message, 0x10);
}

pub fn show_info(message: &str) {
    show_message(message, 0x40);
}

fn show_message(message: &str, kind: u32) {
    if trace_enabled() { eprintln!("[audio] system message dialog kind=0x{kind:x}"); }
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            wide(message).as_ptr(),
            wide("ZapLaunch").as_ptr(),
            kind,
        );
    }
}

/// 开启 Per-Monitor V2 DPI 感知。
///
/// 必须在**创建任何窗口之前**调用（main 的第一句）。
///
/// 不做的两个后果，2026-10-22 都实测到了：
/// ① 界面在 125%/150% 屏上被系统位图放大 → 发糊；
/// ② `GetCursorPos` 返回虚拟化坐标、而窗口位置按物理像素落地 → 呼出定位整体偏掉
///    （实测鼠标 (500,500) 时面板跑到 (640,635)，偏了 100 逻辑像素）。
///
/// 优先用 user32 的 SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2)（Win10 1703+，
/// 连带子窗口、原生文件对话框与托盘菜单自动跨屏缩放），次选 shcore 的 SetProcessDpiAwareness(2)
/// （Win8.1+），再失败退回 user32 的 SetProcessDPIAware（Vista+）。
pub fn enable_dpi_awareness() {
    unsafe {
        let user32 = GetModuleHandleW(wide("user32.dll").as_ptr());
        if !user32.is_null() {
            if let Some(f) = GetProcAddress(user32, b"SetProcessDpiAwarenessContext\0".as_ptr()) {
                let f: unsafe extern "system" fn(isize) -> i32 = std::mem::transmute(f);
                // DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2 = -4
                if f(-4) != 0 {
                    return;
                }
            }
        }
        let shcore = LoadLibraryW(wide("shcore.dll").as_ptr());
        if !shcore.is_null() {
            let mut initialized = false;
            let f = GetProcAddress(shcore, b"SetProcessDpiAwareness\0".as_ptr());
            if let Some(f) = f {
                let f: unsafe extern "system" fn(i32) -> i32 = std::mem::transmute(f);
                initialized = f(2) == 0; // S_OK
            }
            FreeLibrary(shcore);
            if initialized {
                return;
            }
        }
        if !user32.is_null() {
            let f = GetProcAddress(user32, b"SetProcessDPIAware\0".as_ptr());
            if let Some(f) = f {
                let f: unsafe extern "system" fn() -> i32 = std::mem::transmute(f);
                f();
            }
        }
    }
}

#[link(name = "kernel32")]
extern "system" {
    fn LoadLibraryW(name: *const u16) -> *mut c_void;
    fn FreeLibrary(module: *mut c_void) -> i32;
    fn GetModuleHandleW(name: *const u16) -> *mut c_void;
    fn FindResourceW(module: *mut c_void, name: *const u16, kind: *const u16) -> *mut c_void;
    fn SizeofResource(module: *mut c_void, resource: *mut c_void) -> u32;
    fn LoadResource(module: *mut c_void, resource: *mut c_void) -> *mut c_void;
    fn LockResource(resource: *mut c_void) -> *const u8;
    fn GetProcAddress(module: *mut c_void, name: *const u8) -> Option<unsafe extern "system" fn() -> isize>;
    fn CreateMutexW(attr: *const c_void, initial_owner: i32, name: *const u16) -> *mut c_void;
    fn GetLastError() -> u32;
    fn CloseHandle(handle: *mut c_void) -> i32;
}

/// 读取自身 exe 的图标资源组，返回各档尺寸的单图 ICO；不再嵌入第二份 ICO 文件。
pub fn app_icon_images() -> Option<Vec<Vec<u8>>> {
    fn resource(kind: usize, id: usize) -> Option<Vec<u8>> {
        unsafe {
            let module = GetModuleHandleW(std::ptr::null());
            let found = FindResourceW(module, id as *const u16, kind as *const u16);
            if found.is_null() { return None; }
            let len = SizeofResource(module, found) as usize;
            if len == 0 || len > 16 * 1024 * 1024 { return None; }
            let loaded = LoadResource(module, found);
            if loaded.is_null() { return None; }
            let bytes = LockResource(loaded);
            if bytes.is_null() { return None; }
            Some(std::slice::from_raw_parts(bytes, len).to_vec())
        }
    }
    let group = resource(14, 1)?;
    if group.len() < 6 || group[..4] != [0, 0, 1, 0] { return None; }
    let count = u16::from_le_bytes([group[4], group[5]]) as usize;
    if count > 256 || group.len() < 6 + count * 14 { return None; }
    let mut images = Vec::with_capacity(count);
    for entry in group[6..6 + count * 14].chunks_exact(14) {
        let id = u16::from_le_bytes([entry[12], entry[13]]) as usize;
        let Some(payload) = resource(3, id) else { continue; };
        let expected = u32::from_le_bytes(entry[8..12].try_into().ok()?) as usize;
        if expected != payload.len() { continue; }
        let mut ico = Vec::with_capacity(22 + payload.len());
        ico.extend_from_slice(&[0, 0, 1, 0, 1, 0]);
        ico.extend_from_slice(&entry[..12]);
        ico.extend_from_slice(&22u32.to_le_bytes());
        ico.extend_from_slice(&payload);
        images.push(ico);
    }
    Some(images)
}

const WM_NCLBUTTONDOWN: u32 = 0x00A1;
const WM_LBUTTONUP: u32 = 0x0202;
/// HTCAPTION：告诉系统「这一点算标题栏」，系统就会接管拖动
const HTCAPTION: usize = 2;

#[link(name = "user32")]
extern "system" {
    fn ReleaseCapture() -> i32;
    fn SendMessageW(hwnd: HWND, msg: u32, wparam: usize, lparam: isize) -> isize;
    fn PostMessageW(hwnd: HWND, msg: u32, wparam: usize, lparam: isize) -> i32;
    fn ShowWindow(hwnd: HWND, ncmdshow: i32) -> i32;
    fn MessageBoxW(hwnd: HWND, text: *const u16, caption: *const u16, kind: u32) -> i32;
}

#[link(name = "dwmapi")]
extern "system" {
    fn DwmSetWindowAttribute(
        hwnd: HWND,
        attr: u32,
        value: *const c_void,
        size: u32,
    ) -> i32;
}

// ---- 注册表（开机自启）----
#[link(name = "advapi32")]
extern "system" {
    fn RegOpenKeyExW(hkey: usize, sub: *const u16, opt: u32, sam: u32, out: *mut usize) -> i32;
    fn RegSetValueExW(hkey: usize, name: *const u16, rsv: u32, ty: u32, data: *const u8, cb: u32) -> i32;
    fn RegDeleteValueW(hkey: usize, name: *const u16) -> i32;
    fn RegQueryValueExW(hkey: usize, name: *const u16, rsv: *mut u32, ty: *mut u32, data: *mut u8, cb: *mut u32) -> i32;
    fn RegCloseKey(hkey: usize) -> i32;
}

const HKEY_CURRENT_USER: usize = 0x8000_0001;
const KEY_SET_VALUE: u32 = 0x0002;
const KEY_QUERY_VALUE: u32 = 0x0001;
const REG_SZ: u32 = 1;
const ERROR_SUCCESS: i32 = 0;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "ZapLaunch";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 设置开机自启：写/删 HKCU\Software\Microsoft\Windows\CurrentVersion\Run 下的 ZapLaunch 值。
///
/// 用 HKCU 而不是 HKLM：不需要管理员权限（绿色单文件产品不能要求 UAC）。
/// 值为 `"<exe 完整路径>"`（带引号，路径含空格也能被正确解析）。
pub fn set_auto_start(enable: bool, exe_path: &str) -> bool {
    if enable && exe_path.trim().is_empty() {
        return false;
    }
    if std::env::var_os("ZAPLAUNCH_AUTOSTART").as_deref() == Some(std::ffi::OsStr::new("0")) {
        return true;
    }
    unsafe {
        let sub = wide(RUN_KEY);
        let mut hkey: usize = 0;
        if RegOpenKeyExW(HKEY_CURRENT_USER, sub.as_ptr(), 0, KEY_SET_VALUE, &mut hkey) != ERROR_SUCCESS {
            return false;
        }
        let name = wide(RUN_VALUE);
        let ok = if enable {
            // 带 --tray-only：开机后只留托盘图标，不弹面板（没人希望开机蹦一个窗口）。
            // 直接双击 exe 才是「呼出面板 + 托盘」。
            let cmd = if exe_path.contains("--tray-only") {
                format!("\"{exe_path}\"")
            } else {
                format!("\"{exe_path}\" --tray-only")
            };
            let data = wide(&cmd);
            let bytes = data.len() * 2;
            RegSetValueExW(hkey, name.as_ptr(), 0, REG_SZ, data.as_ptr() as *const u8, bytes as u32)
                == ERROR_SUCCESS
        } else {
            // 值不存在也算成功（幂等）
            let rc = RegDeleteValueW(hkey, name.as_ptr());
            rc == ERROR_SUCCESS || rc == 2 // 2 = ERROR_FILE_NOT_FOUND
        };
        RegCloseKey(hkey);
        ok
    }
}

/// 查询当前是否已设置开机自启（只比对值是否存在，不比对路径）。
pub fn is_auto_start_enabled() -> bool {
    if std::env::var_os("ZAPLAUNCH_AUTOSTART").as_deref() == Some(std::ffi::OsStr::new("0")) {
        return false;
    }
    unsafe {
        let sub = wide(RUN_KEY);
        let mut hkey: usize = 0;
        if RegOpenKeyExW(HKEY_CURRENT_USER, sub.as_ptr(), 0, KEY_QUERY_VALUE, &mut hkey) != ERROR_SUCCESS {
            return false;
        }
        let name = wide(RUN_VALUE);
        let rc = RegQueryValueExW(
            hkey,
            name.as_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        RegCloseKey(hkey);
        rc == ERROR_SUCCESS
    }
}

/// 系统当前是否为「应用深色模式」。
///
/// 读 HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize\AppsUseLightTheme：
/// 0 = 深色，1 = 浅色。读不到（Win7 / 组策略禁个性化）一律按浅色处理——
/// 宁可浅色，也不要在用户没设置过的情况下突然变深。
pub fn is_system_dark() -> bool {
    const PERSONALIZE: &str =
        r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize";
    unsafe {
        let sub = wide(PERSONALIZE);
        let mut hkey: usize = 0;
        if RegOpenKeyExW(HKEY_CURRENT_USER, sub.as_ptr(), 0, KEY_QUERY_VALUE, &mut hkey)
            != ERROR_SUCCESS
        {
            return false;
        }
        let name = wide("AppsUseLightTheme");
        let mut ty: u32 = 0;
        let mut data: u32 = 1;
        let mut cb: u32 = 4;
        let rc = RegQueryValueExW(
            hkey,
            name.as_ptr(),
            std::ptr::null_mut(),
            &mut ty,
            &mut data as *mut u32 as *mut u8,
            &mut cb,
        );
        RegCloseKey(hkey);
        rc == ERROR_SUCCESS && data == 0
    }
}

/// 当前 exe 的完整路径（自启写注册表用；取不到返回空串）。
pub fn current_exe_path() -> String {
    std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_default()
}

const SW_MINIMIZE: i32 = 6;
const SW_RESTORE: i32 = 9;

/// DWM 窗口圆角偏好（Windows 11 22000+）。
const DWMWA_WINDOW_CORNER_PREFERENCE: u32 = 33;
const DWMWCP_ROUND: u32 = 2;

/// 请求系统把窗口四角渲染为圆角（Windows 11+）。
///
/// 为什么走 DWM 而不是自绘透明窗口：Slint 1.18 的 winit 后端在 Windows 上
/// 不向 wint 请求透明窗口（`wants_transparent` 仅 macOS 分支启用），且
/// renderer-software 的呈现路径（softbuffer → 窗口 DC）不支持逐像素 alpha
/// 混合到桌面（源码实测：sw.rs 无 `set_transparent` 实现）。硬做透明要重写
/// 呈现层（WS_EX_LAYERED + UpdateLayeredWindow 逐帧整窗提交），成本与性能
/// 都不划算。DWM 路线一行调用、零渲染开销。
///
/// Win10 上本调用返回非 0（属性不存在），窗口保持直角——无害降级，
/// 与设计规格 6.5「Win10 直角属 DWM 能力限制」一致。
pub fn round_window_corners(hwnd: isize) -> bool {
    if hwnd == 0 {
        return false;
    }
    unsafe {
        let pref: u32 = DWMWCP_ROUND;
        0 == DwmSetWindowAttribute(
            hwnd as HWND,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &pref as *const u32 as *const c_void,
            4,
        )
    }
}

// ---- 输入法（IME）----
// 只查询上下文；启用/禁用交给 Slint/winit，避免原生关联与框架状态不一致。
#[link(name = "imm32")]
extern "system" {
    fn ImmGetContext(hwnd: HWND) -> *mut c_void;
    fn ImmReleaseContext(hwnd: HWND, himc: *mut c_void) -> i32;
}

#[link(name = "user32")]
extern "system" {
    fn GetFocus() -> HWND;
    fn GetActiveWindow() -> HWND;
}

/// 当前线程（UI 线程）的键盘焦点窗口句柄；无焦点返回 0。
/// 诊断用：判断 Slint 窗口到底有没有拿到键盘焦点。
pub fn get_focus() -> isize {
    unsafe { GetFocus() as isize }
}

/// 当前活动窗口句柄。
pub fn get_active_window() -> isize {
    unsafe { GetActiveWindow() as isize }
}

/// 查询窗口是否仍有关联的 IME 上下文，不修改当前输入法或组合输入。
pub fn ime_enabled(hwnd: isize) -> bool {
    if hwnd == 0 {
        return false;
    }
    unsafe {
        let context = ImmGetContext(hwnd as HWND);
        if context.is_null() { return false; }
        ImmReleaseContext(hwnd as HWND,context);
        true
    }
}

#[link(name = "shell32")]
extern "system" {
    fn ShellExecuteW(
        hwnd: HWND,
        verb: *const u16,
        file: *const u16,
        params: *const u16,
        dir: *const u16,
        show: i32,
    ) -> isize;
}

/// ShellExecute 默认显示方式（正常大小激活窗口）。
const SW_SHOWNORMAL: i32 = 1;

/// 启动一个目标（按钮的核心动作）。
///
/// 走 ShellExecuteW "open" 动词，天然支持：exe 路径、裸程序名（系统搜索路径）、
/// 文件夹（打开资源管理器）、文档（关联程序打开）、URL（默认浏览器）、
/// 协议（ms-screenclip: 等）。对应用户手动双击的行为，权限/UAC 由系统弹窗处理。
///
/// 返回值 > 32 = 成功（Win32 ShellExecute 的历史约定，SE_ERR_* 都 ≤ 32）。
pub mod web;

pub fn is_network_path(path: &str) -> bool {
    #[link(name = "kernel32")]
    extern "system" { fn GetDriveTypeW(root: *const u16) -> u32; }
    let path = path.trim_matches('"');
    let path = path.strip_prefix(r"\\?\").unwrap_or(path);
    if path.starts_with(r"\\") || path.starts_with("//") || path.get(..4).is_some_and(|s| s.eq_ignore_ascii_case("UNC\\")) {
        return true;
    }
    if let [drive, b':', ..] = path.as_bytes() {
        if drive.is_ascii_alphabetic() {
            let root = [*drive as u16, b':' as u16, b'\\' as u16, 0];
            return unsafe { GetDriveTypeW(root.as_ptr()) } == 4; // DRIVE_REMOTE
        }
    }
    false
}

pub fn launch_program(target: &str, params: &str, work_dir: &str) -> bool {
    launch_program_with_state(target, params, work_dir, SW_SHOWNORMAL)
}

pub fn launch_program_with_state(target: &str, params: &str, work_dir: &str, show: i32) -> bool {
    let _com = ComApartment::initialize();
    let verb = wide("open");
    let file = wide(target);
    let par = wide(params);
    let dir = wide(work_dir);
    unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            if params.is_empty() { std::ptr::null() } else { par.as_ptr() },
            if work_dir.is_empty() { std::ptr::null() } else { dir.as_ptr() },
            show,
        ) > 32
    }
}

/// 最小化窗口。
///
/// 不用 Slint 的 `Window::set_minimized`：实测在 winit + software-renderer 组合下
/// 调用后窗口状态不变（2026-10-21，点击最小化按钮无效）。直接走 Win32 最稳。
pub fn minimize_window(hwnd: isize) {
    if hwnd == 0 {
        return;
    }
    unsafe {
        ShowWindow(hwnd as HWND, SW_MINIMIZE);
    }
}

/// 从最小化恢复。
pub fn restore_window(hwnd: isize) {
    if hwnd == 0 {
        return;
    }
    unsafe {
        ShowWindow(hwnd as HWND, SW_RESTORE);
    }
}

/// 让系统接管当前窗口的拖动。
///
/// 原理：释放鼠标捕获，然后给自己发一条「标题栏按下」的消息。系统收到后按标准
/// 非客户区拖动流程处理——平滑跟随、支持拖到屏幕边缘 Aero Snap、双击最大化等，
/// 全都免费获得，比自己算鼠标位移可靠得多。
pub fn drag_window(hwnd: isize) {
    if hwnd == 0 {
        return;
    }
    unsafe {
        ReleaseCapture();
        // 同步阻塞：系统会在此进入自己的模态拖动循环，直到用户松开鼠标才返回
        SendMessageW(hwnd as HWND, WM_NCLBUTTONDOWN, HTCAPTION, 0);

        // 关键补丁：上面那个模态循环把鼠标抬起事件自己吃掉了，winit 收不到
        // WM_LBUTTONUP，会一直认为左键处于按下状态 —— 表现就是「拖完窗口后
        // 第一次点按钮没反应」。补一条抬起消息让 winit 侧状态复位。
        // 2026-10-21 实测踩到：拖完再点关闭按钮无响应。
        PostMessageW(hwnd as HWND, WM_LBUTTONUP, 0, 0);
    }
}

// ---------------------------------------------------------------------------
// 文件对话框（设置页 ⑩-d 导入 / 导出）
// ---------------------------------------------------------------------------

/// OPENFILENAMEW（x64 布局：结构体 152 字节，指针对齐 8）。
///
/// 手工声明的代价就是必须把偏移算对，否则系统连对话框都不弹（返回
/// CDERR_STRUCTSIZE = 1）。按 SDK 字段顺序逐个对齐后 sizeof = 152。
#[repr(C)]
struct OPENFILENAMEW {
    l_struct_size: u32,          // 0
    hwnd_owner: HWND,            // 8
    h_instance: *mut c_void,     // 16
    lpstr_filter: *const u16,    // 24
    lpstr_custom_filter: *mut u16, // 32
    n_max_cust_filter: u32,      // 40
    n_filter_index: u32,         // 44
    lpstr_file: *mut u16,        // 48
    n_max_file: u32,             // 56
    lpstr_file_title: *mut u16,  // 64
    n_max_file_title: u32,       // 72
    lpstr_initial_dir: *const u16, // 80（72+4 → 对齐到 80）
    lpstr_title: *const u16,     // 88
    flags: u32,                  // 96
    n_file_offset: u16,          // 100
    n_file_extension: u16,       // 102
    lpstr_def_ext: *const u16,   // 104
    l_cust_data: isize,          // 112
    lpfn_hook: *const c_void,    // 120
    lp_template_name: *const u16, // 128
    pv_reserved: *mut c_void,    // 136
    dw_reserved: u32,            // 144
    flags_ex: u32,               // 148
}

#[link(name = "comdlg32")]
extern "system" {
    fn GetSaveFileNameW(o: *mut OPENFILENAMEW) -> i32;
    fn GetOpenFileNameW(o: *mut OPENFILENAMEW) -> i32;
    fn CommDlgExtendedError() -> u32;
}

#[link(name = "user32")]
extern "system" {
    fn GetAsyncKeyState(vk: i32) -> i16;
    fn MapVirtualKeyW(code: u32, map: u32) -> u32;
}

const VK_SHIFT: u32 = 0x10;
const VK_CONTROL: u32 = 0x11;
const VK_MENU: u32 = 0x12;
const VK_ESCAPE: u32 = 0x1B;
const VK_SPACE: u32 = 0x20;
/// MapVirtualKeyW 的「虚拟键 → 字符」映射模式
const MAPVK_VK_TO_CHAR: u32 = 2;

/// 虚拟键码 → 配置串里的键名（"K" / "5" / "Space" / "F3"）；识别不了返回空串。
fn vk_to_name(vk: u32) -> String {
    if vk == VK_SPACE {
        return "Space".into();
    }
    if vk == VK_ESCAPE {
        return "Esc".into();
    }
    if (0x30..=0x39).contains(&vk) || (0x41..=0x5A).contains(&vk) {
        return char::from_u32(vk).unwrap_or('?').to_string();
    }
    if (0x70..=0x87).contains(&vk) {
        return format!("F{}", vk - 0x6F);
    }
    // 其余可打印字符（如 ; = , ）交给系统映射表
    unsafe {
        let c = MapVirtualKeyW(vk, MAPVK_VK_TO_CHAR) & 0xFFFF;
        if c > 0x20 && c != 0xFFFF {
            return char::from_u32(c).unwrap_or('?').to_string();
        }
    }
    String::new()
}

/// Esc 当前是否处于按下状态（收起面板用；只查一次按键状态，开销可忽略）。
pub fn is_esc_pressed() -> bool {
    unsafe { (GetAsyncKeyState(VK_ESCAPE as i32) as u16) & 0x8000 != 0 }
}

/// 任意虚拟键当前是否处于按下状态（↓ 展开运行历史用）。
pub fn is_key_pressed(vk: u32) -> bool {
    unsafe { (GetAsyncKeyState(vk as i32) as u16) & 0x8000 != 0 }
}

/// 热键录制的一次采样：当前按下的是「哪些修饰键 + 哪个主键」。
///
/// 返回 None = 还没按到有效组合（没按主键，或没按任何修饰键）。
/// 输出格式与 `tray::parse_hotkey` 严格对应（如 `Ctrl+Alt+K`），录制完直接落配置。
///
/// 为什么轮询：Slint 的 FocusScope 拿不到「Ctrl/Alt + 字母」——Windows 不产生
/// 字符消息（2026-10-22 实测）。全局 WH_KEYBOARD_LL 钩子装在后台托盘线程上
/// 系统不回调（同样实测）。录制是一次性动作，用 40ms 轮询 + 用户取消，代价可接受。
pub fn polled_hotkey() -> Option<String> {
    unsafe {
        let ctrl = (GetAsyncKeyState(VK_CONTROL as i32) as u16) & 0x8000 != 0;
        let alt = (GetAsyncKeyState(VK_MENU as i32) as u16) & 0x8000 != 0;
        let shift = (GetAsyncKeyState(VK_SHIFT as i32) as u16) & 0x8000 != 0;
        if !(ctrl || alt || shift) {
            return None;
        }
        let mut key = None;
        for vk in (0x41u32..=0x5A).chain(0x30u32..=0x39).chain(0x70u32..=0x87) {
            if (GetAsyncKeyState(vk as i32) as u16) & 0x8000 != 0 {
                key = Some(vk_to_name(vk));
                break;
            }
        }
        if key.is_none() && (GetAsyncKeyState(VK_SPACE as i32) as u16) & 0x8000 != 0 {
            key = Some("Space".into());
        }
        key.map(|k| {
            let mut s = String::new();
            if ctrl {
                s += "Ctrl+";
            }
            if alt {
                s += "Alt+";
            }
            if shift {
                s += "Shift+";
            }
            s + &k
        })
    }
}

const OFN_OVERWRITEPROMPT: u32 = 0x0000_0002;
const OFN_NOCHANGEDIR: u32 = 0x0000_0008;
const OFN_PATHMUSTEXIST: u32 = 0x0000_0800;
const OFN_EXPLORER: u32 = 0x0008_0000;

/// UTF-16 缓冲 → String（遇到第一个 NUL 为止）。
fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// 系统「另存为」对话框：返回用户选中的路径，取消返回 None。
///
/// `hwnd` 传主窗口句柄（模态对话框挂在它上面）；传 0 则不挂父窗口。
pub fn save_file_dialog(hwnd: isize, title: &str, default_name: &str) -> Option<String> {
    file_dialog(true, hwnd, title, default_name, "JSON 配置\0*.json\0")
}

/// 系统「打开」对话框：返回用户选中的路径，取消返回 None。
pub fn open_file_dialog(hwnd: isize, title: &str) -> Option<String> {
    file_dialog(false, hwnd, title, "", "JSON 配置\0*.json\0")
}

pub fn open_icon_dialog(hwnd: isize) -> Option<String> {
    file_dialog(false, hwnd, "选择图标（引用原文件，移动后需要重新选择）", "", "图标与图片\0*.ico;*.png\0")
}

fn file_dialog(save: bool, hwnd: isize, title: &str, default_name: &str, patterns: &str) -> Option<String> {
    // 过滤器是「描述\0模式\0」对，以两个 NUL 收尾
    let mut filter: Vec<u16> = patterns.encode_utf16().collect();
    filter.push(0);
    filter.push(0);

    let mut file: [u16; 1024] = [0; 1024];
    if save && !default_name.is_empty() {
        let n: Vec<u16> = default_name.encode_utf16().collect();
        let k = n.len().min(file.len() - 1);
        file[..k].copy_from_slice(&n[..k]);
    }

    let w_title = wide(title);
    let w_def = wide("json");
    let w_init = wide(&default_dir());

    let mut ofn = unsafe { std::mem::zeroed::<OPENFILENAMEW>() };
    ofn.l_struct_size = std::mem::size_of::<OPENFILENAMEW>() as u32;
    ofn.hwnd_owner = hwnd as HWND;
    ofn.lpstr_filter = filter.as_ptr();
    ofn.lpstr_file = file.as_mut_ptr();
    ofn.n_max_file = file.len() as u32;
    ofn.lpstr_initial_dir = w_init.as_ptr();
    ofn.lpstr_title = w_title.as_ptr();
    ofn.lpstr_def_ext = if save { w_def.as_ptr() } else { std::ptr::null() };
    ofn.flags = OFN_EXPLORER | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR
        | if save { OFN_OVERWRITEPROMPT } else { 0 };

    unsafe {
        let ok = if save {
            GetSaveFileNameW(&mut ofn)
        } else {
            GetOpenFileNameW(&mut ofn)
        };
        if ok != 0 {
            Some(from_wide(&file))
        } else {
            let e = CommDlgExtendedError();
            // 0 = 用户取消；其余（尤其 CDERR_STRUCTSIZE=1）打印出来便于定位
            if e != 0 {
                eprintln!("[zaplaunch-sys] 文件对话框错误码 {e:#x}");
            }
            None
        }
    }
}

fn default_dir() -> String {
    std::env::var("APPDATA")
        .map(|p| format!(r"{p}\ZapLaunch"))
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// 时区（⑬ 运行历史的「昨天 HH:MM」需要本地时间）
// ---------------------------------------------------------------------------

/// TIME_ZONE_INFORMATION。Bias 需与当前状态的 StandardBias / DaylightBias 相加。
#[repr(C)]
struct TimeZoneInformation {
    bias: i32,
    standard_name: [u16; 32],
    standard_date: [u16; 8],
    standard_bias: i32,
    daylight_name: [u16; 32],
    daylight_date: [u16; 8],
    daylight_bias: i32,
}

#[link(name = "kernel32")]
extern "system" {
    fn GetTimeZoneInformation(tz: *mut TimeZoneInformation) -> u32;
}

/// 本地时区相对 UTC 的偏移（秒），含当前生效的夏令时。取不到按 0 处理。
///
/// Windows 的 Bias 语义是 `UTC = 本地时间 + Bias`（分钟），所以本地偏移 = `-Bias`。
pub fn local_utc_offset_secs() -> i64 {
    unsafe {
        let mut tz: TimeZoneInformation = std::mem::zeroed();
        match GetTimeZoneInformation(&mut tz) {
            // 0=未知 1=标准时 2=夏令时；UTC = 本地时间 + Bias + 当前状态 Bias。
            0 => -(tz.bias as i64) * 60,
            1 => -((tz.bias + tz.standard_bias) as i64) * 60,
            2 => -((tz.bias + tz.daylight_bias) as i64) * 60,
            _ => 0,
        }
    }
}

// ---------------------------------------------------------------------------
// 文件拖放（WM_DROPFILES）
// ---------------------------------------------------------------------------
//
// Slint 当前没有把 winit 的 DroppedFile 事件暴露到组件层，因此在主面板 HWND 上
// 做一个极薄的 Win32 子类：只拦截 WM_DROPFILES，其余消息原样交回 winit。
// 不为拖放引入 OLE 运行时、后台线程或常驻轮询。

type HDROP = *mut c_void;
const WM_DROPFILES: u32 = 0x0233;
const WM_COPYGLOBALDATA: u32 = 0x0049;
const MSGFLT_ALLOW: u32 = 1;
const GWLP_WNDPROC: i32 = -4;

#[link(name = "ole32")]
extern "system" {
    fn RevokeDragDrop(hwnd: HWND) -> i32;
}

#[link(name = "shell32")]
extern "system" {
    fn DragAcceptFiles(hwnd: HWND, accept: i32);
    fn DragQueryFileW(drop: HDROP, index: u32, file: *mut u16, chars: u32) -> u32;
    fn DragFinish(drop: HDROP);
}

#[link(name = "user32")]
extern "system" {
    fn SetWindowLongPtrW(hwnd: HWND, index: i32, value: isize) -> isize;
    fn CallWindowProcW(previous: isize, hwnd: HWND, msg: u32, wparam: usize, lparam: isize) -> isize;
    fn ChangeWindowMessageFilterEx(hwnd: HWND, msg: u32, action: u32, pchange: *mut c_void) -> i32;
}

static DROP_HWND: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);
static DROP_OLD_PROC: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);
static DROP_CALLBACK: std::sync::Mutex<Option<Box<dyn Fn(Vec<String>) + Send>>> =
    std::sync::Mutex::new(None);
static DROP_ACTIVATED: std::sync::Mutex<Option<fn()>> = std::sync::Mutex::new(None);
static PANEL_TAB_FALLBACK: std::sync::Mutex<Option<fn(bool)>> = std::sync::Mutex::new(None);
static PANEL_TAB_CONSUMED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Slint 已消费本次 Tab 导航，后续翻译字符不能再进入原生默认处理。
pub fn mark_panel_tab_consumed() {
    PANEL_TAB_CONSUMED.store(true,std::sync::atomic::Ordering::SeqCst);
    if trace_enabled() { eprintln!("[input] Tab navigation consumed"); }
}

#[cfg(test)]
static WINDOW_PROC_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
const WM_KEYDOWN: u32 = 0x0100;
const WM_SYSKEYDOWN: u32 = 0x0104;
const WM_CHAR: u32 = 0x0102;
const WM_SYSCHAR: u32 = 0x0106;
const VK_TAB: usize = 9;
const WM_SETFOCUS: u32 = 0x0007;
const WM_NCACTIVATE: u32 = 0x0086;

unsafe extern "system" fn drop_wnd_proc(hwnd: HWND, msg: u32, wparam: usize, lparam: isize) -> isize {
    if msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN {
        PANEL_TAB_CONSUMED.store(false,std::sync::atomic::Ordering::SeqCst);
    }
    // 系统 Tab 字符落到无菜单的主面板时，不交给默认窗口过程。
    let system_tab = msg == WM_SYSCHAR && (lparam >> 16) & 0xff == 0x0f;
    // 不吞首个普通 WM_CHAR：winit 可能要靠它完成 KeyPressed 分发。
    if (msg == WM_CHAR || msg == WM_SYSCHAR) && wparam == VK_TAB
        && (system_tab || PANEL_TAB_CONSUMED.load(std::sync::atomic::Ordering::SeqCst)) {
        if trace_enabled() { eprintln!("[input] translated Tab suppressed after navigation"); }
        return 0;
    }
    if msg == WM_DROPFILES {
        let drop = wparam as HDROP;
        let count = DragQueryFileW(drop, 0xFFFF_FFFF, std::ptr::null_mut(), 0);
        if trace_enabled() { eprintln!("[trace] WM_DROPFILES count={count}"); }
        let mut paths = Vec::with_capacity(count as usize);
        for index in 0..count {
            let len = DragQueryFileW(drop, index, std::ptr::null_mut(), 0) as usize;
            let mut buf = vec![0u16; len + 1];
            let copied = DragQueryFileW(drop, index, buf.as_mut_ptr(), buf.len() as u32) as usize;
            if copied > 0 {
                paths.push(String::from_utf16_lossy(&buf[..copied]));
            }
        }
        DragFinish(drop);
        if !paths.is_empty() {
            if let Ok(slot) = DROP_CALLBACK.lock() {
                if let Some(cb) = slot.as_ref() {
                    cb(paths);
                }
            }
        }
        return 0;
    }
    let previous = DROP_OLD_PROC.load(std::sync::atomic::Ordering::SeqCst);
    let result = if previous == 0 {
        0
    } else {
        CallWindowProcW(previous, hwnd, msg, wparam, lparam)
    };
    // WM_SETFOCUS / WM_NCACTIVATE：先让 winit 处理，再通知宿主核对原生与界面焦点。
    if msg == WM_CHAR && wparam == VK_TAB && (lparam >> 16) & 0xff == 0x0f
        && !PANEL_TAB_CONSUMED.load(std::sync::atomic::Ordering::SeqCst)
        && ![VK_CONTROL, VK_MENU, 0x5b, 0x5c].into_iter().any(is_key_pressed)
    {
        let callback = PANEL_TAB_FALLBACK.lock().ok().and_then(|slot| *slot);
        if let Some(callback) = callback {
            mark_panel_tab_consumed();
            callback(is_key_pressed(VK_SHIFT));
            return 0;
        }
    }
    if msg == WM_SETFOCUS || (msg == WM_NCACTIVATE && wparam != 0) {
        let callback = DROP_ACTIVATED.lock().ok().and_then(|slot| *slot);
        if let Some(callback) = callback { callback(); }
    }
    result
}

#[test]
fn activation_notification_follows_original_window_proc_without_swallowing_messages() {
    let _guard = WINDOW_PROC_TEST_LOCK.lock().unwrap();
    use std::sync::atomic::{AtomicUsize,Ordering};
    static ORDER: AtomicUsize = AtomicUsize::new(0);
    static NOTIFIED: AtomicUsize = AtomicUsize::new(0);
    unsafe extern "system" fn original(_:HWND,_:u32,_:usize,_:isize)->isize { ORDER.store(1,Ordering::SeqCst);1234 }
    fn activated() { if ORDER.load(Ordering::SeqCst) == 1 { ORDER.store(2,Ordering::SeqCst); } NOTIFIED.fetch_add(1,Ordering::SeqCst); }
    let old = DROP_OLD_PROC.swap(original as *const () as isize,Ordering::SeqCst);
    let callback = DROP_ACTIVATED.lock().unwrap().replace(activated);
    for (message,wparam,expected) in [(WM_SETFOCUS,0,1),(WM_NCACTIVATE,1,2),(WM_NCACTIVATE,0,2),(0x0100,0,2)] {
        ORDER.store(0,Ordering::SeqCst);
        assert_eq!(unsafe { drop_wnd_proc(std::ptr::null_mut(),message,wparam,0) },1234);
        assert_eq!(NOTIFIED.load(Ordering::SeqCst),expected);
        assert_eq!(ORDER.load(Ordering::SeqCst),if message == WM_SETFOCUS || (message == WM_NCACTIVATE && wparam != 0) {2}else{1});
    }
    DROP_OLD_PROC.store(old,Ordering::SeqCst);
    *DROP_ACTIVATED.lock().unwrap() = callback;
}

#[test]
fn consumed_tab_characters_never_reach_default_processing_but_key_dispatch_and_ime_do() {
    use std::sync::atomic::{AtomicUsize,Ordering};
    let _guard = WINDOW_PROC_TEST_LOCK.lock().unwrap();
    static CALLS: AtomicUsize = AtomicUsize::new(0);
    static HANDLE_AT: AtomicUsize = AtomicUsize::new(0);
    unsafe extern "system" fn original(_:HWND,msg:u32,key:usize,_:isize)->isize {
        CALLS.fetch_add(1,Ordering::SeqCst);
        if msg as usize == HANDLE_AT.load(Ordering::SeqCst) && key == 9 { mark_panel_tab_consumed(); }
        1234
    }
    let old = DROP_OLD_PROC.swap(original as *const () as isize,Ordering::SeqCst);
    let consumed = PANEL_TAB_CONSUMED.swap(false,Ordering::SeqCst);
    for handle_at in [0x0100,0x0102,0] {
        HANDLE_AT.store(handle_at,Ordering::SeqCst); CALLS.store(0,Ordering::SeqCst);
        assert_eq!(unsafe { drop_wnd_proc(std::ptr::null_mut(),0x0100,9,0) },1234);
        assert_eq!(unsafe { drop_wnd_proc(std::ptr::null_mut(),0x0102,9,0) },if handle_at==0x0100 {0}else{1234});
        let before = CALLS.load(Ordering::SeqCst);
        assert_eq!(unsafe { drop_wnd_proc(std::ptr::null_mut(),0x0102,9,0) },if handle_at==0 {1234}else{0});
        assert_eq!(CALLS.load(Ordering::SeqCst)-before,if handle_at==0 {1}else{0});
        for (msg,key) in [(0x0101,9),(0x0102,0x4e2d),(0x0286,0x6587)] {
            assert_eq!(unsafe { drop_wnd_proc(std::ptr::null_mut(),msg,key,0) },1234);
        }
        // Ctrl+I also translates to 0x09; a different key must clear the previous Tab marker.
        assert_eq!(unsafe { drop_wnd_proc(std::ptr::null_mut(),WM_KEYDOWN,0x49,0) },1234);
        assert_eq!(unsafe { drop_wnd_proc(std::ptr::null_mut(),WM_CHAR,9,0) },1234);
        assert_eq!(unsafe { drop_wnd_proc(std::ptr::null_mut(),WM_SYSKEYDOWN,0x12,0) },1234);
        assert_eq!(unsafe { drop_wnd_proc(std::ptr::null_mut(),WM_SYSCHAR,9,0) },1234);
        let before = CALLS.load(Ordering::SeqCst);
        assert_eq!(unsafe { drop_wnd_proc(std::ptr::null_mut(),WM_SYSCHAR,9,0x200f0001) },0);
        assert_eq!(CALLS.load(Ordering::SeqCst),before);
    }
    DROP_OLD_PROC.store(old,Ordering::SeqCst); PANEL_TAB_CONSUMED.store(consumed,Ordering::SeqCst);
}

/// 给主面板安装文件拖放入口；同一 HWND 重复调用只更新回调，不重复子类化。
///
/// 关键点：winit 创建窗口时默认会调用 `RegisterDragDrop` 注册 OLE `IDropTarget`。
/// 在 Windows OLE 拖放机制下，只要窗口挂有 `IDropTarget`，资源管理器拖入文件时就会走
/// OLE 接口而**绝不会触发 `WM_DROPFILES`**（即便调了 `DragAcceptFiles(hwnd, 1)` 也无效），
/// 且 Slint 并未向外转发 winit 的 `DroppedFile` 事件。
/// 因此这里必须先调用 `RevokeDragDrop` 撤销 winit 的 OLE 拖放目标，让系统回退到经典的
/// `WM_DROPFILES` 路径，再配合 `ChangeWindowMessageFilterEx` 放行跨权限拖放消息。
pub fn install_file_drop(hwnd: isize, callback: impl Fn(Vec<String>) + Send + 'static, activated: fn(), tab_fallback: fn(bool)) -> bool {
    if hwnd == 0 {
        return false;
    }
    if let Ok(mut slot) = DROP_CALLBACK.lock() {
        *slot = Some(Box::new(callback));
    } else {
        return false;
    }
    if let Ok(mut slot) = DROP_ACTIVATED.lock() { *slot = Some(activated); } else { return false; }
    if let Ok(mut slot) = PANEL_TAB_FALLBACK.lock() { *slot = Some(tab_fallback); } else { return false; }
    unsafe {
        let h = hwnd as HWND;
        let _ = RevokeDragDrop(h);
        DragAcceptFiles(h, 1);
        let _ = ChangeWindowMessageFilterEx(h, WM_DROPFILES, MSGFLT_ALLOW, std::ptr::null_mut());
        let _ = ChangeWindowMessageFilterEx(h, WM_COPYGLOBALDATA, MSGFLT_ALLOW, std::ptr::null_mut());
        if DROP_HWND.load(std::sync::atomic::Ordering::SeqCst) == hwnd {
            return true;
        }
        let previous = SetWindowLongPtrW(h, GWLP_WNDPROC, drop_wnd_proc as *const () as isize);
        if previous == 0 {
            DragAcceptFiles(h, 0);
            return false;
        }
        DROP_OLD_PROC.store(previous, std::sync::atomic::Ordering::SeqCst);
        DROP_HWND.store(hwnd, std::sync::atomic::Ordering::SeqCst);
        if trace_enabled() { eprintln!("[trace] drop subclass installed hwnd={hwnd}"); }
        true
    }
}

// ---------------------------------------------------------------------------
// 原生文件 / 快捷方式 (.lnk) / EXE / 文件夹高清 RGBA 图标提取
// ---------------------------------------------------------------------------

const SHGFI_ICON: u32 = 0x0000_0100;
const SHGFI_LARGEICON: u32 = 0x0000_0000;
const SHGFI_SYSICONINDEX: u32 = 0x0000_4000;
const SHIL_EXTRALARGE: i32 = 2; // 48x48 系统高清图标列表
const ILD_TRANSPARENT: u32 = 0x0000_0001;
const COINIT_APARTMENTTHREADED: u32 = 0x2;

#[repr(C)]
struct GUID {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

const IID_IIMAGELIST: GUID = GUID {
    data1: 0x46EB_5926,
    data2: 0x582E,
    data3: 0x4017,
    data4: [0x9F, 0xDF, 0xE8, 0x99, 0x8D, 0xAA, 0x09, 0x50],
};

#[repr(C)]
struct SHFILEINFOW {
    hIcon: *mut c_void,
    iIcon: i32,
    dwAttributes: u32,
    szDisplayName: [u16; 260],
    szTypeName: [u16; 80],
}

#[repr(C)]
struct ICONINFO {
    fIcon: i32,
    xHotspot: u32,
    yHotspot: u32,
    hbmMask: *mut c_void,
    hbmColor: *mut c_void,
}

#[repr(C)]
struct BITMAP {
    bmType: i32,
    bmWidth: i32,
    bmHeight: i32,
    bmWidthBytes: i32,
    bmPlanes: u16,
    bmBitsPixel: u16,
    bmBits: *mut c_void,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct BITMAPINFOHEADER {
    biSize: u32,
    biWidth: i32,
    biHeight: i32,
    biPlanes: u16,
    biBitCount: u16,
    biCompression: u32,
    biSizeImage: u32,
    biXPelsPerMeter: i32,
    biYPelsPerMeter: i32,
    biClrUsed: u32,
    biClrImportant: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct BITMAPINFO {
    bmiHeader: BITMAPINFOHEADER,
    bmiColors: [u32; 1],
}

#[link(name = "ole32")]
extern "system" {
    fn CoInitializeEx(pvReserved: *mut c_void, dwCoInit: u32) -> i32;
    fn CoUninitialize();
    fn CoCreateInstance(
        rclsid: *const GUID,
        pUnkOuter: *mut c_void,
        dwClsContext: u32,
        riid: *const GUID,
        ppv: *mut *mut c_void,
    ) -> i32;
}

struct ComApartment(bool);

impl ComApartment {
    fn initialize() -> Self {
        let result = unsafe { CoInitializeEx(std::ptr::null_mut(), COINIT_APARTMENTTHREADED) };
        // S_OK / S_FALSE 都必须配对释放；RPC_E_CHANGED_MODE 等失败不能释放。
        Self(result == 0 || result == 1)
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize(); }
        }
    }
}

#[link(name = "shell32")]
extern "system" {
    fn SHGetFileInfoW(
        pszPath: *const u16,
        dwFileAttributes: u32,
        psfi: *mut SHFILEINFOW,
        cbFileInfo: u32,
        uFlags: u32,
    ) -> usize;
    fn SHGetImageList(iImageList: i32, riid: *const GUID, ppvObj: *mut *mut c_void) -> i32;
}

#[link(name = "user32")]
extern "system" {
    fn GetIconInfo(hIcon: *mut c_void, piconinfo: *mut ICONINFO) -> i32;
    fn DestroyIcon(hIcon: *mut c_void) -> i32;
}

#[link(name = "gdi32")]
extern "system" {
    fn CreateCompatibleDC(hdc: *mut c_void) -> *mut c_void;
    fn DeleteDC(hdc: *mut c_void) -> i32;
    fn DeleteObject(ho: *mut c_void) -> i32;
    fn GetObjectW(h: *mut c_void, c: i32, pv: *mut c_void) -> i32;
    fn GetDIBits(
        hdc: *mut c_void,
        hbm: *mut c_void,
        start: u32,
        cLines: u32,
        lpvBits: *mut c_void,
        lpbmi: *mut BITMAPINFO,
        usage: u32,
    ) -> i32;
}

/// 提取任意文件、文件夹、快捷方式 (`.lnk`) 或可执行程序 (`.exe`) 的系统原生标准图标，
/// 返回 Shell 实际提供的分辨率；显示层另按窗口 DPI 平滑缩放，不能假定源图始终与目标尺寸一致。
/// 保持原始构图比例，仅对满幅直角边缘做柔和圆角抗锯齿过渡。
pub fn extract_file_icon_rgba(path: &str) -> Option<(u32, u32, Vec<u8>)> {
    let clean = path.trim().trim_matches('"').trim();
    if clean.is_empty() {
        return None;
    }
    let normalized = clean.replace('/', "\\");
    let wide_path = wide(&normalized);

    unsafe {
        let _com = ComApartment::initialize();

        // ① 优先从 SHIL_EXTRALARGE (标准 48×48 逻辑像素，系统自动按 DPI 放大) 获取无快捷方式角标的原生图标
        let mut hicon: *mut c_void = std::ptr::null_mut();
        let mut shfi_idx: SHFILEINFOW = std::mem::zeroed();
        let idx_res = SHGetFileInfoW(
            wide_path.as_ptr(),
            0,
            &mut shfi_idx,
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_SYSICONINDEX,
        );
        if idx_res != 0 {
            let mut img_list: *mut c_void = std::ptr::null_mut();
            if SHGetImageList(SHIL_EXTRALARGE, &IID_IIMAGELIST, &mut img_list) == 0
                && !img_list.is_null()
            {
                let vtbl = *(img_list as *const *const usize);
                // IImageList::GetIcon 位于 vtable 索引 10，IUnknown::Release 位于索引 2
                let get_icon: unsafe extern "system" fn(
                    *mut c_void,
                    i32,
                    u32,
                    *mut *mut c_void,
                ) -> i32 = std::mem::transmute(*vtbl.add(10));
                let release: unsafe extern "system" fn(*mut c_void) -> u32 =
                    std::mem::transmute(*vtbl.add(2));
                let _ = get_icon(img_list, shfi_idx.iIcon, ILD_TRANSPARENT, &mut hicon);
                release(img_list);
            }
        }

        // ② 若系统图像列表未命中，回退 SHGFI_ICON | SHGFI_LARGEICON
        if hicon.is_null() {
            let mut shfi: SHFILEINFOW = std::mem::zeroed();
            let res = SHGetFileInfoW(
                wide_path.as_ptr(),
                0,
                &mut shfi,
                std::mem::size_of::<SHFILEINFOW>() as u32,
                SHGFI_ICON | SHGFI_LARGEICON,
            );
            if res == 0 || shfi.hIcon.is_null() {
                return None;
            }
            hicon = shfi.hIcon;
        }

        let mut ii: ICONINFO = std::mem::zeroed();
        if GetIconInfo(hicon, &mut ii) == 0 {
            DestroyIcon(hicon);
            return None;
        }

        let result = (|| -> Option<(u32, u32, Vec<u8>)> {
            if ii.hbmColor.is_null() {
                return None;
            }
            let mut bm: BITMAP = std::mem::zeroed();
            if GetObjectW(
                ii.hbmColor,
                std::mem::size_of::<BITMAP>() as i32,
                &mut bm as *mut _ as *mut c_void,
            ) == 0
            {
                return None;
            }
            let w = bm.bmWidth;
            let h = bm.bmHeight;
            if w <= 0 || h <= 0 || w > 512 || h > 512 {
                return None;
            }

            let hdc = CreateCompatibleDC(std::ptr::null_mut());
            if hdc.is_null() {
                return None;
            }

            let mut bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h, // 负值：Top-Down 自顶向下位图
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: 0, // BI_RGB
                    biSizeImage: 0,
                    biXPelsPerMeter: 0,
                    biYPelsPerMeter: 0,
                    biClrUsed: 0,
                    biClrImportant: 0,
                },
                bmiColors: [0],
            };

            let byte_len = (w as usize) * (h as usize) * 4;
            let mut pixels = vec![0u8; byte_len];
            let lines = GetDIBits(
                hdc,
                ii.hbmColor,
                0,
                h as u32,
                pixels.as_mut_ptr() as *mut c_void,
                &mut bmi,
                0,
            );
            if lines == 0 {
                DeleteDC(hdc);
                return None;
            }

            // 检查是否自带有效 Alpha 通道（现代 32-bit ARGB 图标）
            let has_alpha = pixels.chunks_exact(4).any(|px| px[3] != 0);
            if !has_alpha && !ii.hbmMask.is_null() {
                let mut mask_bmi = bmi;
                let mut mask_pixels = vec![0u8; byte_len];
                let mask_lines = GetDIBits(
                    hdc,
                    ii.hbmMask,
                    0,
                    h as u32,
                    mask_pixels.as_mut_ptr() as *mut c_void,
                    &mut mask_bmi,
                    0,
                );
                if mask_lines != 0 {
                    for (px, m) in pixels.chunks_exact_mut(4).zip(mask_pixels.chunks_exact(4)) {
                        if m[0] == 0 {
                            px[3] = 255;
                        } else {
                            px[0] = 0;
                            px[1] = 0;
                            px[2] = 0;
                            px[3] = 0;
                        }
                    }
                } else {
                    for px in pixels.chunks_exact_mut(4) {
                        px[3] = 255;
                    }
                }
            } else if !has_alpha {
                for px in pixels.chunks_exact_mut(4) {
                    px[3] = 255;
                }
            }

            DeleteDC(hdc);

            // GDI BGRA -> RGBA，并在原生分辨率上仅对贴近画布外框的四角做柔和抗锯齿圆角（不裁剪、不拉伸原图）
            let wf = w as f32;
            let hf = h as f32;
            let half_w = wf * 0.5;
            let half_h = hf * 0.5;
            // 对应 48×48 标准图标的 10px 圆角比例（若图标自带边距则丝毫不影响内部图形）
            let radius = wf.min(hf) * (10.0 / 48.0);

            for y in 0..(h as usize) {
                for x in 0..(w as usize) {
                    let idx = (y * (w as usize) + x) * 4;
                    pixels.swap(idx, idx + 2);

                    let px = x as f32 + 0.5;
                    let py = y as f32 + 0.5;
                    let dx = (px - half_w).abs() - (half_w - radius);
                    let dy = (py - half_h).abs() - (half_h - radius);
                    let outside = dx.max(0.0).hypot(dy.max(0.0));
                    let inside = dx.max(dy).min(0.0);
                    let dist = outside + inside - radius;
                    let cov = (0.5 - dist).clamp(0.0, 1.0);
                    if cov < 1.0 {
                        pixels[idx + 3] = ((pixels[idx + 3] as f32) * cov).round() as u8;
                    }
                }
            }

            Some((w as u32, h as u32, pixels))
        })();

        if !ii.hbmColor.is_null() {
            DeleteObject(ii.hbmColor);
        }
        if !ii.hbmMask.is_null() {
            DeleteObject(ii.hbmMask);
        }
        DestroyIcon(hicon);

        result
    }
}

const CLSID_SHELL_LINK: GUID = GUID {
    data1: 0x0002_1401,
    data2: 0x0000,
    data3: 0x0000,
    data4: [0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
};

const IID_ISHELL_LINK_W: GUID = GUID {
    data1: 0x0002_14F9,
    data2: 0x0000,
    data3: 0x0000,
    data4: [0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
};

const IID_IPERSIST_FILE: GUID = GUID {
    data1: 0x0000_010B,
    data2: 0x0000,
    data3: 0x0000,
    data4: [0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
};

/// 解析 `.lnk` 快捷方式文件指向的真实目标路径（如 `C:\Program Files\Tencent\WeChat\WeChat.exe`）。
///
/// 供智能搜索使用：当用户拖入 `微信.lnk` 后，即使快捷方式文件名是中文，输入 `wechat` 或 `weixin`
/// 也能匹配到 `.lnk` 背后真实的 `.exe` 路径与程序名。
pub fn resolve_shortcut_target(path: &str) -> Option<String> {
    let trimmed = path.trim().trim_matches('"').trim();
    if !trimmed.to_ascii_lowercase().ends_with(".lnk") {
        return None;
    }
    let normalized = trimmed.replace('/', "\\");
    let wide_path = wide(&normalized);

    unsafe {
        let _com = ComApartment::initialize();
        let mut psl: *mut c_void = std::ptr::null_mut();
        // CLSCTX_INPROC_SERVER = 1
        if CoCreateInstance(
            &CLSID_SHELL_LINK,
            std::ptr::null_mut(),
            1,
            &IID_ISHELL_LINK_W,
            &mut psl,
        ) != 0
            || psl.is_null()
        {
            return None;
        }

        let psl_vtbl = *(psl as *const *const usize);
        let query_interface: unsafe extern "system" fn(
            *mut c_void,
            *const GUID,
            *mut *mut c_void,
        ) -> i32 = std::mem::transmute(*psl_vtbl.add(0));
        let release_psl: unsafe extern "system" fn(*mut c_void) -> u32 =
            std::mem::transmute(*psl_vtbl.add(2));
        let get_path: unsafe extern "system" fn(
            *mut c_void,
            *mut u16,
            i32,
            *mut c_void,
            u32,
        ) -> i32 = std::mem::transmute(*psl_vtbl.add(3));

        let mut ppf: *mut c_void = std::ptr::null_mut();
        let mut target_out = None;
        if query_interface(psl, &IID_IPERSIST_FILE, &mut ppf) == 0 && !ppf.is_null() {
            let ppf_vtbl = *(ppf as *const *const usize);
            let release_ppf: unsafe extern "system" fn(*mut c_void) -> u32 =
                std::mem::transmute(*ppf_vtbl.add(2));
            let load: unsafe extern "system" fn(*mut c_void, *const u16, u32) -> i32 =
                std::mem::transmute(*ppf_vtbl.add(5));

            // STGM_READ = 0
            if load(ppf, wide_path.as_ptr(), 0) == 0 {
                let mut buf = [0u16; 1024];
                if get_path(psl, buf.as_mut_ptr(), buf.len() as i32, std::ptr::null_mut(), 0) == 0 {
                    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
                    if len > 0 {
                        target_out = Some(String::from_utf16_lossy(&buf[..len]));
                    }
                }
            }
            release_ppf(ppf);
        }

        release_psl(psl);
        target_out
    }
}
