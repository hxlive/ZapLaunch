// zaplaunch-sys::tray —— 呼出子系统：托盘图标 + 全局热键 + 鼠标跟随定位 + 托盘菜单。
//
// 设计要点（三条，都是为了让「常驻」这件事不付出代价）：
//
// 1. **独立线程 + 消息窗口**：托盘和热键都需要一个 Win32 消息队列。Slint/winit 的事件循环
//    不对外暴露原生消息（不像原始 Win32 程序能自己在 WndProc 里收 WM_HOTKEY），所以这里
//    单独起一个线程、创建一个 **message-only 窗口**（父窗口 = HWND_MESSAGE 的不可见窗口，
//    天生不出现在任务栏和 Alt-Tab 里），专门收托盘通知和热键消息。
//    收到后通过 Rust 回调把事件抛给宿主，宿主再转进 Slint 事件循环。
//
// 2. **零轮询**：没有定时检查任何东西。托盘菜单、图标点击、热键全部是系统回调；
//    线程平时阻塞在 GetMessageW 上（OS 挂起线程），空闲 CPU = 0%。
//
// 3. **零依赖**：和 lib.rs 一样手工 extern，不引入 windows crate。
//
// 2026-10-22 建立（落 P0-4 面板跟随鼠标呼出 + 托盘常驻，见设计规格 7.10 / 7.12）。

#![allow(non_snake_case, non_camel_case_types)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, Ordering};

type HWND = *mut c_void;
type HICON = *mut c_void;
type HMENU = *mut c_void;
type HMONITOR = *mut c_void;
type HINSTANCE = *mut c_void;
type LPARAM = isize;
type WPARAM = usize;
type LRESULT = isize;
type BOOL = i32;
type DWORD = u32;
type UINT = u32;
type ATOM = u16;

const GWLP_USERDATA: i32 = -21;
/// CreateWindowEx 的「只收消息窗口」父句柄
const HWND_MESSAGE: isize = -3;

// ---- 消息 ----
const WM_NULL: u32 = 0x0000;
const WM_CLOSE: u32 = 0x0010;
const WM_DESTROY: u32 = 0x0002;
const WM_COMMAND: u32 = 0x0111;
const WM_HOTKEY: u32 = 0x0312;
const WM_TIMER: u32 = 0x0113;
const WM_MOUSEMOVE: u32 = 0x0200;
const WM_LBUTTONDOWN: u32 = 0x0201;
const TRAY_HOVER_TIMER: usize = 41;
const WH_MOUSE_LL: i32 = 14;
const VK_LBUTTON: i32 = 0x01;
/// 托盘通知走自定义消息号（WM_USER + 1）
const WM_TRAY: u32 = 0x0400 + 1;
/// 改热键（WM_USER + 2）：wParam = 请求编号，lParam = HotkeyRequest 所有权指针
const WM_REHOTKEY: u32 = 0x0400 + 2;
const WM_BUTTON_HOTKEYS: u32 = 0x0400 + 3;

// ---- 托盘事件（鼠标）----
const WM_LBUTTONUP: u32 = 0x0202;
const WM_RBUTTONUP: u32 = 0x0205;
const WM_CONTEXTMENU_MSG: u32 = 0x007B;

// ---- 托盘标志 ----
const NIM_ADD: u32 = 0x0000_0000;
const NIM_DELETE: u32 = 0x0000_0002;
const NIF_MESSAGE: u32 = 0x0000_0001;
const NIF_ICON: u32 = 0x0000_0002;
const NIF_TIP: u32 = 0x0000_0004;

// ---- 热键修饰键 ----
const MOD_ALT: u32 = 0x0001;
const MOD_CONTROL: u32 = 0x0002;
const MOD_SHIFT: u32 = 0x0004;
const MOD_WIN: u32 = 0x0008;
/// 禁止长按重复触发（否则按住热键会连续弹）
const MOD_NOREPEAT: u32 = 0x4000;
const HOTKEY_ID: i32 = 1;
const BUTTON_HOTKEY_FIRST: i32 = 100;

// ---- 资源 / 图标 ----
const IMAGE_ICON: u32 = 1;
const LR_DEFAULTSIZE: u32 = 0x0000_0040;

// ---- 窗口位置/显示 ----
const MONITOR_DEFAULTTONEAREST: u32 = 2;
const SPI_GETWORKAREA: u32 = 0x0030;
const SW_HIDE: i32 = 0;
const SW_SHOWNA: i32 = 8;
const HWND_TOPMOST: isize = -1;
const SWP_NOSIZE: u32 = 0x0001;
const SWP_NOMOVE: u32 = 0x0002;
const SWP_NOZORDER: u32 = 0x0004;
const SWP_NOACTIVATE: u32 = 0x0010;
const SWP_SHOWWINDOW: u32 = 0x0040;

// ---- 托盘菜单 ----
const MF_STRING: u32 = 0x0000_0000;
const MF_SEPARATOR: u32 = 0x0000_0800;
const TPM_RIGHTALIGN: u32 = 0x0008;
const TPM_BOTTOMALIGN: u32 = 0x0020;
const TPM_RIGHTBUTTON: u32 = 0x0002;

/// 托盘菜单项 id（WM_COMMAND 的 wParam 低字）
const IDM_SHOW: usize = 1;
const IDM_NEW_BUTTON: usize = 2;
const IDM_SETTINGS: usize = 3;
const IDM_ABOUT: usize = 4;
const IDM_QUIT: usize = 5;

const TRAY_CLASS_NAME: &str = "ZapLaunchTrayWindowClass";
const TRAY_WINDOW_NAME: &str = "ZapLaunch";

// ---------------------------------------------------------------------------
// 对外事件
// ---------------------------------------------------------------------------

/// 呼出子系统抛给宿主的事件（宿主负责转进 Slint 事件循环）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayEvent {
    /// 全局热键按下（呼出面板）
    Hotkey,
    ButtonHotkey(usize, usize, Hotkey),
    LaunchCompleted(u64, bool),
    LaunchTimedOut(u64),
    StartupLaunchesCompleted(Vec<String>),
    AssetReady,
    PanelActivated,
    PanelTabFallback(bool, usize),
    StartupHotkeyFailed,
    StartupTrayFailed,
    StartupButtonHotkeysFailed(Vec<(usize, usize)>),
    ConfigSaved(u64, Result<Vec<(usize, usize)>, String>),
    /// 托盘图标左键单击（与热键同一条呼出路径）
    TrayClick,
    /// 托盘菜单：显示面板
    ShowPanel,
    /// 托盘菜单：新建按钮
    NewButton,
    /// 文件拖动过程中，指针在托盘图标上停留：呼出面板供直接拖入。
    TrayDropHover,
    /// 托盘菜单：设置
    Settings,
    /// 托盘菜单：关于
    About,
    /// 托盘菜单：退出（真正退进程）
    Quit,
    /// 热键录制完成：定长 UTF-8 缓冲（保证 Copy；尾部补 0）。
    /// 全 0 = 用户按 Esc 取消，宿主应保持原热键不变。
    HotkeyRecorded([u8; 32]),
    /// 全局热键注册成功；参数为请求编号，宿主只提交仍是最新的请求。
    HotkeyRegisterSucceeded(u32),
    /// 全局热键注册失败；参数为请求编号，过期回包不得覆盖新请求状态。
    HotkeyRegisterFailed(u32),
}

/// 全局热键：修饰键位掩码 + 虚拟键码。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hotkey {
    pub mods: u32,
    pub vk: u32,
}

impl Hotkey {
    /// 默认热键：Ctrl+Alt+Space（设计规格 7.13 默认值）。
    pub fn default_hotkey() -> Self {
        Hotkey {
            mods: MOD_CONTROL | MOD_ALT | MOD_NOREPEAT,
            vk: 0x20, // VK_SPACE
        }
    }
}

/// 解析 "Ctrl+Alt+Space" 这样的配置串。
///
/// 解析失败返回 None —— 宿主因此拿不到热键，但托盘左键仍然可用（呼出不至于整体失效）。
pub fn parse_hotkey(s: &str) -> Option<Hotkey> {
    /// 虚拟键码表（够用即可：字母、数字、F1–F12、空格等常用键）
    fn key(name: &str) -> Option<u32> {
        let u = name.to_ascii_uppercase();
        if let Some(rest) = u.strip_prefix('F') {
            if let Ok(n) = rest.parse::<u32>() {
                if (1..=24).contains(&n) {
                    return Some(0x6F + n); // VK_F1 = 0x70
                }
            }
        }
        if u.len() == 1 {
            let c = u.as_bytes()[0];
            if c.is_ascii_alphanumeric() {
                return Some(c.to_ascii_uppercase() as u32);
            }
        }
        Some(match u.as_str() {
            "SPACE" => 0x20,
            "ENTER" | "RETURN" => 0x0D,
            "ESC" | "ESCAPE" => 0x1B,
            "TAB" => 0x09,
            "BACKSPACE" => 0x08,
            "INSERT" | "INS" => 0x2D,
            "DELETE" | "DEL" => 0x2E,
            "HOME" => 0x24,
            "END" => 0x23,
            "PAGEUP" | "PGUP" => 0x21,
            "PAGEDOWN" | "PGDN" => 0x22,
            "UP" => 0x26,
            "DOWN" => 0x28,
            "LEFT" => 0x25,
            "RIGHT" => 0x27,
            "OEM_PLUS" | "=" | "+" => 0xBB,
            "OEM_MINUS" | "-" => 0xBD,
            "OEM_1" | ";" => 0xBA,
            "OEM_2" | "/" | "?" => 0xBF,
            "OEM_3" | "`" | "~" => 0xC0,
            "OEM_COMMA" | "," | "<" => 0xBC,
            "OEM_PERIOD" | "." | ">" => 0xBE,
            _ => return None,
        })
    }

    let mut mods = 0u32;
    let mut vk = None;
    for part in s.split('+') {
        let p = part.trim();
        if p.is_empty() {
            return None;
        }
        let modifier = match p.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => MOD_CONTROL,
            "alt" => MOD_ALT,
            "shift" => MOD_SHIFT,
            "win" | "meta" => MOD_WIN,
            other => {
                if vk.is_some() {
                    return None;
                }
                vk = key(other);
                if vk.is_none() {
                    return None;
                }
                continue;
            }
        };
        if mods & modifier != 0 {
            return None;
        }
        mods |= modifier;
    }
    if mods == 0 {
        return None;
    }
    vk.map(|v| Hotkey { mods: mods | MOD_NOREPEAT, vk: v })
}

// ---------------------------------------------------------------------------
// Win32 声明
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
#[repr(C)]
struct POINT {
    x: i32,
    y: i32,
}

#[repr(C)]
struct RECT {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[repr(C)]
struct MSG {
    hwnd: HWND,
    message: UINT,
    w_param: WPARAM,
    l_param: LPARAM,
    time: DWORD,
    pt: POINT,
    l_private: DWORD,
}

#[repr(C)]
struct WNDCLASSW {
    style: UINT,
    lpfn_wnd_proc: usize,
    cb_cls_extra: i32,
    cb_wnd_extra: i32,
    h_instance: HINSTANCE,
    h_icon: HICON,
    h_cursor: *mut c_void,
    hbr_background: *mut c_void,
    lpsz_menu_name: *const u16,
    lpsz_class_name: *const u16,
}

/// NOTIFYICONDATAW **V1（到 szTip 为止）**。
///
/// 刻意用最小版本：Windows 按 cbSize 判断调用方认识多少字段，V1 全版本接受。
/// 代价是没有气泡通知（本次不需要），换来的是「不必猜 V2/V3/V4 的真实字节数」。
#[repr(C)]
struct NOTIFYICONDATAW {
    cb_size: DWORD,
    hwnd: HWND,
    u_id: UINT,
    u_flags: UINT,
    u_callback_message: UINT,
    h_icon: HICON,
    sz_tip: [u16; 128],
    dw_state: DWORD,
    dw_state_mask: DWORD,
    sz_info: [u16; 256],
    timeout_or_version: UINT,
    sz_info_title: [u16; 64],
    dw_info_flags: DWORD,
    guid_item: [u8; 16],
    h_balloon_icon: HICON,
}

#[repr(C)]
struct MONITORINFO {
    cb_size: DWORD,
    rc_monitor: RECT,
    rc_work: RECT,
    dw_flags: DWORD,
}

#[repr(C)]
struct NOTIFYICONIDENTIFIER {
    cb_size: DWORD,
    hwnd: HWND,
    u_id: UINT,
    guid_item: [u8; 16],
}

#[repr(C)]
struct MSLLHOOKSTRUCT {
    pt: POINT,
    mouse_data: DWORD,
    flags: DWORD,
    time: DWORD,
    extra_info: usize,
}

#[link(name = "user32")]
extern "system" {
    fn RegisterClassW(c: *const WNDCLASSW) -> ATOM;
    fn RegisterWindowMessageW(lpString: *const u16) -> UINT;
    fn FindWindowExW(
        hwndParent: HWND,
        hwndChildAfter: HWND,
        lpszClass: *const u16,
        lpszWindow: *const u16,
    ) -> HWND;
    fn CreateWindowExW(
        ex: DWORD,
        cls: *const u16,
        name: *const u16,
        style: DWORD,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        parent: HWND,
        menu: HMENU,
        inst: HINSTANCE,
        param: *mut c_void,
    ) -> HWND;
    fn DestroyWindow(h: HWND) -> BOOL;
    fn DefWindowProcW(h: HWND, m: UINT, w: WPARAM, l: LPARAM) -> LRESULT;
    fn GetMessageW(m: *mut MSG, h: HWND, min: UINT, max: UINT) -> BOOL;
    fn TranslateMessage(m: *const MSG) -> BOOL;
    fn DispatchMessageW(m: *const MSG) -> LRESULT;
    fn PostQuitMessage(code: i32);
    fn SetWindowLongPtrW(h: HWND, idx: i32, val: isize) -> isize;
    fn GetWindowLongPtrW(h: HWND, idx: i32) -> isize;

    fn LoadImageW(
        hinst: HINSTANCE,
        name: *const u16,
        type_: UINT,
        cx: i32,
        cy: i32,
        fu_load: UINT,
    ) -> *mut c_void;
    fn DestroyIcon(h: HICON) -> BOOL;
    fn ExtractIconW(inst: HINSTANCE, file: *const u16, idx: UINT) -> HICON;

    fn RegisterHotKey(h: HWND, id: i32, mods: UINT, vk: UINT) -> BOOL;
    fn UnregisterHotKey(h: HWND, id: i32) -> BOOL;

    fn GetCursorPos(p: *mut POINT) -> BOOL;
    fn MonitorFromPoint(pt: POINT, flags: DWORD) -> HMONITOR;
    fn GetWindowRect(h: HWND, r: *mut RECT) -> BOOL;
    fn GetMonitorInfoW(m: HMONITOR, info: *mut MONITORINFO) -> BOOL;
    fn SystemParametersInfoW(a: UINT, b: UINT, p: *mut c_void, f: UINT) -> BOOL;

    fn CreatePopupMenu() -> HMENU;
    fn AppendMenuW(m: HMENU, flags: UINT, id: usize, text: *const u16) -> BOOL;
    fn DestroyMenu(m: HMENU) -> BOOL;
    fn TrackPopupMenu(
        m: HMENU,
        flags: UINT,
        x: i32,
        y: i32,
        reserved: i32,
        h: HWND,
        rect: *const RECT,
    ) -> BOOL;

    fn ShowWindow(h: HWND, cmd: i32) -> BOOL;
    fn SetWindowPos(
        h: HWND,
        after: HWND,
        x: i32,
        y: i32,
        cx: i32,
        cy: i32,
        flags: UINT,
    ) -> BOOL;
    fn SetForegroundWindow(h: HWND) -> BOOL;
    fn GetForegroundWindow() -> HWND;
    fn SetActiveWindow(h: HWND) -> HWND;
    fn IsWindowVisible(h: HWND) -> BOOL;
    fn AttachThreadInput(a: DWORD, b: DWORD, attach: BOOL) -> BOOL;
    fn GetWindowThreadProcessId(h: HWND, pid: *mut DWORD) -> DWORD;
    fn GetCurrentThreadId() -> DWORD;
    fn GetAsyncKeyState(key: i32) -> i16;
    fn SetTimer(hwnd: HWND, id: usize, elapse: UINT, callback: isize) -> usize;
    fn KillTimer(hwnd: HWND, id: usize) -> BOOL;
    fn SetWindowsHookExW(id: i32, hook: isize, module: HINSTANCE, thread: DWORD) -> isize;
    fn UnhookWindowsHookEx(hook: isize) -> BOOL;
    fn CallNextHookEx(hook: isize, code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT;
}

#[link(name = "kernel32")]
extern "system" {
    fn GetModuleHandleW(name: *const u16) -> HINSTANCE;
    fn GetCurrentProcessId() -> DWORD;
}

/// 原生消息框（"关于"用它顶着，等批次 D 做出真窗口再换）。
///
/// 模态阻塞：调用期间本线程不再泵消息。可接受——「关于」本来就是看完就关的对话框。
pub fn message_box(title: &str, text: &str) {
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            wide(text).as_ptr(),
            wide(title).as_ptr(),
            MB_OK | MB_ICONINFORMATION,
        );
    }
}

#[link(name = "user32")]
extern "system" {
    fn MessageBoxW(h: HWND, text: *const u16, cap: *const u16, ty: UINT) -> i32;
}

const MB_OK: u32 = 0x0000_0000;
const MB_ICONINFORMATION: u32 = 0x0000_0040;

#[link(name = "shell32")]
extern "system" {
    fn Shell_NotifyIconW(op: DWORD, data: *mut NOTIFYICONDATAW) -> BOOL;
    fn Shell_NotifyIconGetRect(identifier: *const NOTIFYICONIDENTIFIER, rect: *mut RECT) -> i32;
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 查找已运行实例的后台托盘窗口，并向其投递「显示面板」命令。
pub(crate) fn notify_existing_tray_to_show() -> bool {
    unsafe {
        let cls = wide(TRAY_CLASS_NAME);
        let name = wide(TRAY_WINDOW_NAME);
        let hwnd = FindWindowExW(
            HWND_MESSAGE as HWND,
            std::ptr::null_mut(),
            cls.as_ptr(),
            name.as_ptr(),
        );
        if hwnd.is_null() {
            return false;
        }
        PostMessageW(hwnd, WM_COMMAND, IDM_SHOW, 0) != 0
    }
}

// ---------------------------------------------------------------------------
// 窗口过程
// ---------------------------------------------------------------------------

/// 窗口过程中需要的上下文（放在 GWLP_USERDATA 里，Box 泄漏后由 WM_DESTROY 回收）。
struct WindowCtx {
    cb: Box<dyn Fn(TrayEvent) + Send>,
    icon: HICON,
    hotkey: Option<Hotkey>,
    buttons: Vec<(usize, usize, Hotkey)>,
    taskbar_created_msg: UINT,
}

/// 跨线程投递的热键请求。由消息窗口过程取回所有权，避免把多次请求的状态混在一起。
struct HotkeyRequest {
    hotkey: Option<Hotkey>,
    /// 配置中已提交的热键；新请求失败时恢复它，而不是恢复某个未提交的旧请求。
    fallback: Option<Hotkey>,
}

struct ButtonHotkeyRequest {
    buttons: Vec<(usize, usize, Hotkey)>,
    reply: std::sync::mpsc::Sender<Vec<(usize, usize)>>,
    state: std::sync::Arc<std::sync::atomic::AtomicU8>,
}

fn wait_button_hotkeys(rx: std::sync::mpsc::Receiver<Vec<(usize, usize)>>, state: &std::sync::atomic::AtomicU8, timeout: std::time::Duration) -> Option<Vec<(usize, usize)>> {
    match rx.recv_timeout(timeout) {
        Ok(failed) => Some(failed),
        // 提交已完成时只等紧随其后的回包，不再等待系统调用。
        Err(_) if state.compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst).is_err() => rx.recv().ok(),
        Err(_) => None,
    }
}

unsafe fn replace_button_hotkeys(ctx: &mut WindowCtx, h: HWND, buttons: Vec<(usize, usize, Hotkey)>) -> Vec<(usize, usize)> {
    for i in 0..ctx.buttons.len() { UnregisterHotKey(h, BUTTON_HOTKEY_FIRST + i as i32); }
    let mut failed = Vec::new();
    for (i, &(gi, bi, hk)) in buttons.iter().enumerate() {
        if RegisterHotKey(h, BUTTON_HOTKEY_FIRST + i as i32, hk.mods, hk.vk) == 0 { failed.push((gi, bi)); }
    }
    ctx.buttons = buttons;
    failed
}

// 只有「在托盘外按下左键并移动进来」才视为拖入候选；单纯长按托盘图标不会呼出。
static DRAG_ARMED: AtomicBool = AtomicBool::new(false);
static TRAY_TIMER_ACTIVE: AtomicBool = AtomicBool::new(false);

// 缓存在 WM_LBUTTONDOWN 时刻查询到的托盘图标矩形：
// 避免在高频 WM_MOUSEMOVE 中发起跨进程 Shell_NotifyIconGetRect RPC 调用导致全系统鼠标卡顿。
static CACHED_TRAY_VALID: AtomicBool = AtomicBool::new(false);
static CACHED_TRAY_LEFT: AtomicI32 = AtomicI32::new(0);
static CACHED_TRAY_TOP: AtomicI32 = AtomicI32::new(0);
static CACHED_TRAY_RIGHT: AtomicI32 = AtomicI32::new(0);
static CACHED_TRAY_BOTTOM: AtomicI32 = AtomicI32::new(0);

/// 查询托盘图标真实坐标，并同步更新内存缓存。
fn query_tray_rect() -> Option<RECT> {
    unsafe {
        let hwnd = LAST_HWND.load(Ordering::SeqCst) as HWND;
        if hwnd.is_null() {
            CACHED_TRAY_VALID.store(false, Ordering::SeqCst);
            return None;
        }
        let mut id: NOTIFYICONIDENTIFIER = std::mem::zeroed();
        id.cb_size = std::mem::size_of::<NOTIFYICONIDENTIFIER>() as DWORD;
        id.hwnd = hwnd;
        id.u_id = 1;
        let mut rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        if Shell_NotifyIconGetRect(&id, &mut rect) == 0 && rect.right > rect.left && rect.bottom > rect.top {
            CACHED_TRAY_LEFT.store(rect.left, Ordering::Relaxed);
            CACHED_TRAY_TOP.store(rect.top, Ordering::Relaxed);
            CACHED_TRAY_RIGHT.store(rect.right, Ordering::Relaxed);
            CACHED_TRAY_BOTTOM.store(rect.bottom, Ordering::Relaxed);
            CACHED_TRAY_VALID.store(true, Ordering::Release);
            Some(rect)
        } else {
            CACHED_TRAY_VALID.store(false, Ordering::Release);
            None
        }
    }
}

fn point_in_tray(pt: POINT) -> bool {
    match query_tray_rect() {
        Some(rect) => {
            pt.x >= rect.left && pt.x < rect.right && pt.y >= rect.top && pt.y < rect.bottom
        }
        None => false,
    }
}

/// 纯内存判断鼠标点是否落在已缓存的托盘图标矩形内（供 WM_MOUSEMOVE 零开销使用）。
#[inline]
fn point_in_cached_tray(pt: POINT) -> bool {
    if !CACHED_TRAY_VALID.load(Ordering::Acquire) {
        return false;
    }
    let left = CACHED_TRAY_LEFT.load(Ordering::Relaxed);
    let top = CACHED_TRAY_TOP.load(Ordering::Relaxed);
    let right = CACHED_TRAY_RIGHT.load(Ordering::Relaxed);
    let bottom = CACHED_TRAY_BOTTOM.load(Ordering::Relaxed);
    pt.x >= left && pt.x < right && pt.y >= top && pt.y < bottom
}

fn cancel_tray_hover() {
    let hwnd = LAST_HWND.load(Ordering::SeqCst) as HWND;
    if !hwnd.is_null() && TRAY_TIMER_ACTIVE.swap(false, Ordering::SeqCst) {
        unsafe { KillTimer(hwnd, TRAY_HOVER_TIMER); }
    } else {
        TRAY_TIMER_ACTIVE.store(false, Ordering::SeqCst);
    }
}

unsafe extern "system" fn mouse_hook(code: i32, msg: WPARAM, data: LPARAM) -> LRESULT {
    if code >= 0 && data != 0 {
        let pt = (*(data as *const MSLLHOOKSTRUCT)).pt;
        match msg as u32 {
            WM_LBUTTONDOWN => {
                // 仅在按下左键瞬间查询一次托盘坐标并缓存；若托盘不可见（如折叠在溢出区）则不激活拖拽检测
                let inside = point_in_tray(pt);
                let rect_valid = CACHED_TRAY_VALID.load(Ordering::Acquire);
                if crate::trace_enabled() {
                    eprintln!("[trace] hook down at {},{} inside={inside} valid={rect_valid}", pt.x, pt.y);
                }
                DRAG_ARMED.store(rect_valid && !inside, Ordering::SeqCst);
                cancel_tray_hover();
            }
            WM_LBUTTONUP => {
                DRAG_ARMED.store(false, Ordering::SeqCst);
                cancel_tray_hover();
            }
            WM_MOUSEMOVE => {
                if DRAG_ARMED.load(Ordering::Relaxed) {
                    let dragging = (GetAsyncKeyState(VK_LBUTTON) as u16 & 0x8000) != 0;
                    if !dragging {
                        DRAG_ARMED.store(false, Ordering::Relaxed);
                        cancel_tray_hover();
                    } else {
                        // 纯内存坐标比对，绝不在鼠标移动钩子中发起跨进程 IPC
                        let inside = point_in_cached_tray(pt);
                        let hwnd = LAST_HWND.load(Ordering::Relaxed) as HWND;
                        if inside && !hwnd.is_null() && !TRAY_TIMER_ACTIVE.load(Ordering::Relaxed) {
                            if crate::trace_enabled() { eprintln!("[trace] hook drag entered tray"); }
                            if SetTimer(hwnd, TRAY_HOVER_TIMER, 450, 0) != 0 {
                                TRAY_TIMER_ACTIVE.store(true, Ordering::SeqCst);
                                if crate::trace_enabled() { eprintln!("[trace] tray hover timer started"); }
                            }
                        } else if !inside && TRAY_TIMER_ACTIVE.load(Ordering::Relaxed) {
                            cancel_tray_hover();
                        }
                    }
                }
            }
            _ => {}
        }
    }
    CallNextHookEx(0, code, msg, data)
}

unsafe extern "system" fn wnd_proc(h: HWND, msg: UINT, w: WPARAM, l: LPARAM) -> LRESULT {
    // 创建期间（WM_NCCREATE 等）取不到 userdata，一律走默认处理
    let ctx = GetWindowLongPtrW(h, GWLP_USERDATA) as *mut WindowCtx;
    if ctx.is_null() {
        return DefWindowProcW(h, msg, w, l);
    }
    let ctx = &mut *ctx;

    // explorer.exe 重启后任务栏重建：自动重新挂载托盘图标
    if ctx.taskbar_created_msg != 0 && msg == ctx.taskbar_created_msg {
        let mut nid = icon_data(h, ctx.icon, TRAY_WINDOW_NAME);
        Shell_NotifyIconW(NIM_ADD, &mut nid);
        return 0;
    }

    match msg {
        WM_HOTKEY if w as i32 == HOTKEY_ID => (ctx.cb)(TrayEvent::Hotkey),
        WM_HOTKEY if (w as i32) >= BUTTON_HOTKEY_FIRST => {
            if let Some(&(gi, bi, hk)) = ctx.buttons.get(w - BUTTON_HOTKEY_FIRST as usize) {
                (ctx.cb)(TrayEvent::ButtonHotkey(gi, bi, hk));
            }
        }
        WM_TIMER if w == TRAY_HOVER_TIMER => {
            if crate::trace_enabled() { eprintln!("[trace] tray hover timer fired"); }
            cancel_tray_hover();
            let mut pt = POINT { x: 0, y: 0 };
            if GetCursorPos(&mut pt) != 0
                && DRAG_ARMED.load(Ordering::SeqCst)
                && (GetAsyncKeyState(VK_LBUTTON) as u16 & 0x8000) != 0
                && point_in_tray(pt)
            {
                (ctx.cb)(TrayEvent::TrayDropHover);
            }
        }
        // 改热键：在**后台线程**里注销旧的再注册新的（RegisterHotKey 必须由
        // 创建窗口的线程调用，UI 线程直接调会失败）。
        WM_REHOTKEY => {
            if l == 0 {
                return 0;
            }
            let request = Box::from_raw(l as *mut HotkeyRequest);
            let requested = request.hotkey;
            UnregisterHotKey(h, HOTKEY_ID);
            let registered = match requested {
                None => true,
                Some(hk) => RegisterHotKey(h, HOTKEY_ID, hk.mods, hk.vk) != 0,
            };
            if registered {
                ctx.hotkey = requested;
                (ctx.cb)(TrayEvent::HotkeyRegisterSucceeded(w as u32));
            } else {
                // 恢复配置中最后提交的热键，并检查回滚本身是否成功。
                let restored = match request.fallback {
                    Some(hk) => RegisterHotKey(h, HOTKEY_ID, hk.mods, hk.vk) != 0,
                    None => true,
                };
                ctx.hotkey = if restored { request.fallback } else { None };
                if restored {
                    eprintln!("[zaplaunch-sys] 重新注册热键失败，已恢复已提交热键 request={}", w);
                } else {
                    eprintln!("[zaplaunch-sys] 新热键及旧热键回滚均失败，当前无全局热键 request={}", w);
                }
                (ctx.cb)(TrayEvent::HotkeyRegisterFailed(w as u32));
            }
        }
        WM_BUTTON_HOTKEYS => {
            if l == 0 { return 0; }
            let request = Box::from_raw(l as *mut ButtonHotkeyRequest);
            if request.state.load(Ordering::SeqCst) == 1 { return 0; }
            let previous = ctx.buttons.clone();
            let failed = replace_button_hotkeys(ctx, h, request.buttons);
            // 超时取消与提交只允许一方成功，迟到注册必须恢复原状态。
            if request.state.compare_exchange(0, 2, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
                let _ = request.reply.send(failed);
            } else {
                let failed = replace_button_hotkeys(ctx, h, previous);
                if !failed.is_empty() { (ctx.cb)(TrayEvent::StartupButtonHotkeysFailed(failed)); }
            }
        }
        WM_TRAY => match l as u32 {
            WM_LBUTTONUP => (ctx.cb)(TrayEvent::TrayClick),
            WM_RBUTTONUP | WM_CONTEXTMENU_MSG => show_tray_menu(h, ctx.hotkey),
            _ => {}
        },
        WM_COMMAND => {
            let ev = match w & 0xFFFF {
                IDM_SHOW => Some(TrayEvent::ShowPanel),
                IDM_NEW_BUTTON => Some(TrayEvent::NewButton),
                IDM_SETTINGS => Some(TrayEvent::Settings),
                IDM_ABOUT => Some(TrayEvent::About),
                IDM_QUIT => Some(TrayEvent::Quit),
                _ => None,
            };
            if let Some(e) = ev {
                (ctx.cb)(e);
            }
        }
        WM_DESTROY => {
            remove_icon(h, ctx.icon);
            PostQuitMessage(0);
            // 先摘除上下文指针，再析构回调捕获值，避免析构重入访问已释放内存。
            SetWindowLongPtrW(h, GWLP_USERDATA, 0);
            drop(Box::from_raw(ctx as *const WindowCtx as *mut WindowCtx));
            return 0;
        }
        WM_CLOSE => {
            DestroyWindow(h);
            return 0;
        }
        _ => return DefWindowProcW(h, msg, w, l),
    }
    0
}

/// 弹出托盘菜单（宽 232px 由系统按文字排布，规格 7.10 给的是视觉宽度）。
///
/// 两个必须做的坑处理：
/// ① 弹菜单前必须 SetForegroundWindow，否则菜单可能不响应鼠标；
/// ② 弹完必须补一条 WM_NULL，否则下一次菜单弹出时会卡住（经典 TrackPopupMenu 陷阱）。
/// 菜单里的热键只是「当前全局热键」的实时展示，不是另一套菜单加速器。
/// 必须从注册状态生成，不能写死默认值，否则用户改完热键后菜单会说谎。
fn hotkey_menu_label(hk: Option<Hotkey>) -> String {
    let Some(hk) = hk else { return "显示面板".into(); };
    let mut parts = Vec::new();
    if hk.mods & MOD_CONTROL != 0 { parts.push("Ctrl".to_string()); }
    if hk.mods & MOD_ALT != 0 { parts.push("Alt".to_string()); }
    if hk.mods & MOD_SHIFT != 0 { parts.push("Shift".to_string()); }
    if hk.mods & MOD_WIN != 0 { parts.push("Win".to_string()); }
    let key = match hk.vk {
        0x20 => "Space".to_string(),
        0x0D => "Enter".to_string(),
        0x1B => "Esc".to_string(),
        0x09 => "Tab".to_string(),
        0x08 => "Backspace".to_string(),
        0x2D => "Insert".to_string(),
        0x2E => "Delete".to_string(),
        0x24 => "Home".to_string(),
        0x23 => "End".to_string(),
        0x21 => "PageUp".to_string(),
        0x22 => "PageDown".to_string(),
        0x26 => "Up".to_string(),
        0x28 => "Down".to_string(),
        0x25 => "Left".to_string(),
        0x27 => "Right".to_string(),
        0x70..=0x87 => format!("F{}", hk.vk - 0x6F),
        0x30..=0x39 | 0x41..=0x5A => char::from_u32(hk.vk).unwrap_or('?').to_string(),
        0xBB => "+".to_string(),
        0xBD => "-".to_string(),
        0xBA => ";".to_string(),
        0xBF => "/".to_string(),
        0xC0 => "`".to_string(),
        0xBC => ",".to_string(),
        0xBE => ".".to_string(),
        _ => format!("VK_{:02X}", hk.vk),
    };
    parts.push(key);
    format!("显示面板\t{}", parts.join(" + "))
}

unsafe fn show_tray_menu(h: HWND, hotkey: Option<Hotkey>) {
    let menu = CreatePopupMenu();
    if menu.is_null() {
        return;
    }
    let add = |text: &str, id: usize| {
        AppendMenuW(menu, MF_STRING, id, wide(text).as_ptr());
    };
    let sep = || {
        AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
    };

    add(&hotkey_menu_label(hotkey), IDM_SHOW);
    add("新建按钮…", IDM_NEW_BUTTON);
    sep();
    add("设置…", IDM_SETTINGS);
    add("关于 ZapLaunch", IDM_ABOUT);
    sep();
    add("退出", IDM_QUIT);

    let mut pt = POINT { x: 0, y: 0 };
    if GetCursorPos(&mut pt) == 0 {
        DestroyMenu(menu);
        return;
    }
    SetForegroundWindow(h);
    TrackPopupMenu(
        menu,
        TPM_RIGHTALIGN | TPM_BOTTOMALIGN | TPM_RIGHTBUTTON,
        pt.x,
        pt.y,
        0,
        h,
        std::ptr::null(),
    );
    DestroyMenu(menu);
    PostMessageW(h, WM_NULL, 0, 0);
}

unsafe fn remove_icon(h: HWND, icon: HICON) {
    let mut nid = icon_data(h, icon, "");
    Shell_NotifyIconW(NIM_DELETE, &mut nid);
    if !icon.is_null() {
        DestroyIcon(icon);
    }
}

unsafe fn icon_data(h: HWND, icon: HICON, tip: &str) -> NOTIFYICONDATAW {
    let mut nid = std::mem::zeroed::<NOTIFYICONDATAW>();
    nid.cb_size = std::mem::size_of::<NOTIFYICONDATAW>() as DWORD;
    nid.hwnd = h;
    nid.u_id = 1;
    nid.u_flags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    nid.u_callback_message = WM_TRAY;
    nid.h_icon = icon;
    let t = wide(tip);
    let mut n = t.len().min(nid.sz_tip.len() - 1);
    if n > 0 && (0xD800..=0xDBFF).contains(&t[n - 1]) { n -= 1; }
    nid.sz_tip[..n].copy_from_slice(&t[..n]);
    nid
}

// ---------------------------------------------------------------------------
// 对外：启动 / 停止
// ---------------------------------------------------------------------------

/// 托盘句柄：调用 `stop()` 注销托盘与热键，或 drop 时自动执行。
pub struct TrayHandle {
    hwnd: isize,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// 只投递请求，不拥有托盘线程；用于后台配置事务。
#[derive(Clone, Copy)]
pub struct ButtonHotkeyClient(isize);

impl ButtonHotkeyClient {
    pub fn update(&self, buttons: Vec<(usize, usize, Hotkey)>) -> Option<Vec<(usize, usize)>> {
        let hwnd = self.0;
        if hwnd == 0 || buttons.len() > (0xBFFF - BUTTON_HOTKEY_FIRST + 1) as usize { return None; }
        let (tx, rx) = std::sync::mpsc::channel();
        let state = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0));
        let request = Box::into_raw(Box::new(ButtonHotkeyRequest { buttons, reply: tx, state: state.clone() }));
        if unsafe { PostMessageW(hwnd as HWND, WM_BUTTON_HOTKEYS, 0, request as isize) } == 0 {
            unsafe { drop(Box::from_raw(request)); }
            return None;
        }
        wait_button_hotkeys(rx, &state, std::time::Duration::from_millis(500))
    }
}

impl TrayHandle {
    pub fn button_hotkey_client(&self) -> ButtonHotkeyClient { ButtonHotkeyClient(self.hwnd) }
    /// 返回未能注册的按钮坐标；其余按钮的热键仍可使用。
    pub fn update_button_hotkeys(&self, buttons: Vec<(usize, usize, Hotkey)>) -> Option<Vec<(usize, usize)>> {
        self.button_hotkey_client().update(buttons)
    }
    /// 运行时改全局热键（设置页「录制」后调用）。
    ///
    /// 只投递一条消息给后台线程，真正的注册在窗口过程里做 —— 因为
    /// `RegisterHotKey` / `UnregisterHotKey` 必须由**创建该窗口的线程**调用。
    /// 传 None 表示只注销（不注册新热键，托盘左键仍可呼出）。
    /// 回包携带 `request_id`，宿主据此丢弃快速连续修改产生的过期结果。
    pub fn update_hotkey(
        &self,
        hk: Option<Hotkey>,
        fallback: Option<Hotkey>,
        request_id: u32,
    ) -> bool {
        if self.hwnd == 0 {
            return false;
        }
        let request = Box::into_raw(Box::new(HotkeyRequest {
            hotkey: hk,
            fallback,
        }));
        let posted = unsafe {
            PostMessageW(
                self.hwnd as HWND,
                WM_REHOTKEY,
                request_id as usize,
                request as isize,
            ) != 0
        };
        if !posted {
            unsafe { drop(Box::from_raw(request)); }
        }
        posted
    }

    /// 注销托盘图标 + 热键，并结束消息线程。
    pub fn stop(&mut self) {
        if self.hwnd != 0 {
            unsafe {
                PostMessageW(self.hwnd as HWND, WM_CLOSE, 0, 0);
            }
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        self.hwnd = 0;
    }
}

impl Drop for TrayHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 启动呼出子系统（所有权交给后台线程，立即返回）。
///
/// - `hotkey`：全局热键；传 None 时不注册（托盘左键仍可用）。
/// - `cb`：**从后台线程调用**，宿主需要自己把事件搬回 UI 线程
///   （Slint 用 `slint::invoke_from_event_loop`）。闭包必须 Send + 'static。
pub fn start(hotkey: Option<Hotkey>, cb: impl Fn(TrayEvent) + Send + 'static) -> TrayHandle {
    let (tx, rx) = std::sync::mpsc::sync_channel::<isize>(1);
    let handle = std::thread::spawn(move || {
        unsafe { run_message_loop(hotkey, Box::new(cb), tx) };
    });

    // 通过同步通道零延迟等待后台线程建好消息窗口（不再走 10ms 轮询 sleep）
    let hwnd = rx.recv().unwrap_or(0);
    let mut thread = Some(handle);
    if hwnd == 0 {
        // 建窗失败：不要留下一个收不到 stop 的野线程
        if let Some(t) = thread.take() {
            let _ = t.join();
        }
    }
    TrayHandle { hwnd, thread }
}

/// 后台线程建好窗口后写到这里，供钩子回调读取。（单进程只有一个托盘，够了。）
static LAST_HWND: AtomicIsize = AtomicIsize::new(0);

unsafe fn run_message_loop(
    hotkey: Option<Hotkey>,
    cb: Box<dyn Fn(TrayEvent) + Send>,
    ready_tx: std::sync::mpsc::SyncSender<isize>,
) {
    // 1) 注册窗口类（同一个 Class 重复注册会失败，忽略返回值即可）
    let cls = wide(TRAY_CLASS_NAME);
    let wc = WNDCLASSW {
        style: 0,
        lpfn_wnd_proc: wnd_proc as *const () as usize,
        cb_cls_extra: 0,
        cb_wnd_extra: 0,
        h_instance: GetModuleHandleW(std::ptr::null()),
        h_icon: std::ptr::null_mut(),
        h_cursor: std::ptr::null_mut(),
        hbr_background: std::ptr::null_mut(),
        lpsz_menu_name: std::ptr::null(),
        lpsz_class_name: cls.as_ptr(),
    };
    RegisterClassW(&wc);

    // 2) 建 message-only 窗口
    let name = wide(TRAY_WINDOW_NAME);
    let h = CreateWindowExW(
        0,
        cls.as_ptr(),
        name.as_ptr(),
        0,
        0,
        0,
        0,
        0,
        HWND_MESSAGE as HWND,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        std::ptr::null_mut(),
    );
    if h.is_null() {
        eprintln!("[zaplaunch-sys] tray window create failed");
        cb(TrayEvent::StartupTrayFailed);
        let _ = ready_tx.send(0);
        return;
    }

    // Load the app icon from the executable's resource table.
    let h_inst = GetModuleHandleW(std::ptr::null());
    let icon = LoadImageW(
        h_inst, 1usize as *const u16, IMAGE_ICON, 0, 0, LR_DEFAULTSIZE,
    ) as HICON;
    // 兜底：若资源 ID 1 未命中，退回 ExtractIconW 取 exe 自身的第 0 个图标
    let icon = if icon.is_null() {
        let exe = crate::current_exe_path();
        ExtractIconW(h_inst, wide(&exe).as_ptr(), 0)
    } else {
        icon
    };

    // 注册 TaskbarCreated 系统广播消息，用于在 explorer.exe 重启后恢复托盘图标
    let taskbar_created_msg = RegisterWindowMessageW(wide("TaskbarCreated").as_ptr());

    // 4) 挂回调上下文
    let ctx = Box::into_raw(Box::new(WindowCtx {
        cb,
        icon,
        hotkey: None,
        buttons: Vec::new(),
        taskbar_created_msg,
    }));
    SetWindowLongPtrW(h, GWLP_USERDATA, ctx as isize);

    // 5) 加托盘图标
    let mut nid = icon_data(h, icon, TRAY_WINDOW_NAME);
    if Shell_NotifyIconW(NIM_ADD, &mut nid) == 0 {
        eprintln!("[zaplaunch-sys] Shell_NotifyIconW(NIM_ADD) failed");
        ((*ctx).cb)(TrayEvent::StartupTrayFailed);
    }

    // 6) 注册全局热键
    if let Some(hk) = hotkey {
        if RegisterHotKey(h, HOTKEY_ID, hk.mods, hk.vk) == 0 {
            eprintln!("[zaplaunch-sys] RegisterHotKey failed (mods={:#x} vk={:#x})", hk.mods, hk.vk);
            ((*ctx).cb)(TrayEvent::StartupHotkeyFailed);
        } else {
            (*ctx).hotkey = Some(hk);
        }
    }

    LAST_HWND.store(h as isize, Ordering::SeqCst);
    let _ = ready_tx.send(h as isize);

    // 低级鼠标钩子只负责识别「外部拖动进入托盘」；没有拖动时只有几次原子判断，
    // 不做定时轮询。真正呼出前还需在图标上停留 450ms。
    let mouse_hook_handle = SetWindowsHookExW(
        WH_MOUSE_LL,
        mouse_hook as *const () as isize,
        GetModuleHandleW(std::ptr::null()),
        0,
    );
    if mouse_hook_handle == 0 {
        eprintln!("[zaplaunch-sys] 托盘拖动悬停钩子安装失败，直接拖入已打开面板仍可用");
    } else if crate::trace_enabled() {
        eprintln!("[trace] mouse hook installed");
    }

    // 7) 消息循环（阻塞线程，空闲零 CPU）
    let mut msg = std::mem::zeroed::<MSG>();
    while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
        TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }

    // 收尾：先摘全局钩子与悬停定时器；热键已随窗口销毁自动注销。
    cancel_tray_hover();
    DRAG_ARMED.store(false, Ordering::SeqCst);
    CACHED_TRAY_VALID.store(false, Ordering::SeqCst);
    if mouse_hook_handle != 0 {
        UnhookWindowsHookEx(mouse_hook_handle);
    }
    if GetWindowLongPtrW(h, GWLP_USERDATA) != 0 && !icon.is_null() {
        remove_icon(h, icon);
    }
    LAST_HWND.store(0, Ordering::SeqCst);
}

// ---------------------------------------------------------------------------
// 呼出定位：鼠标跟随 + 工作区内缩
// ---------------------------------------------------------------------------

/// 面板中心对准鼠标；碰到工作区边缘就内缩。
///
/// `w / h / margin` 全部使用物理像素，由调用方按当前窗口 DPI 换算。
/// 多显示器用 `MonitorFromPoint` 取鼠标所在那块屏的工作区（不含任务栏），
/// 取不到就退回主屏工作区。
pub fn popup_position(w: i32, h: i32, margin: i32) -> (i32, i32) {
    unsafe {
        let mut pt = POINT { x: 0, y: 0 };
        if GetCursorPos(&mut pt) == 0 {
            return fallback_position(w, h);
        }
        let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let mut work = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        let ok = if mon.is_null() {
            false
        } else {
            let mut mi = std::mem::zeroed::<MONITORINFO>();
            mi.cb_size = std::mem::size_of::<MONITORINFO>() as DWORD;
            GetMonitorInfoW(mon, &mut mi) != 0 && {
                work = mi.rc_work;
                true
            }
        };
        if !ok {
            return fallback_position(w, h);
        }

        centered_popup_position(pt, &work, w, h, margin)
    }
}

fn centered_popup_position(pt: POINT, work: &RECT, w: i32, h: i32, margin: i32) -> (i32, i32) {
    let min_x = work.left + margin;
    let min_y = work.top + margin;
    let max_x = (work.right - w - margin).max(min_x);
    let max_y = (work.bottom - h - margin).max(min_y);
    ((pt.x - w / 2).clamp(min_x, max_x), (pt.y - h / 2).clamp(min_y, max_y))
}

fn fallback_position(w: i32, h: i32) -> (i32, i32) {
    unsafe {
        let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        if SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            &mut r as *mut RECT as *mut c_void,
            0,
        ) != 0
        {
            (
                r.left + ((r.right - r.left - w) / 2).max(0),
                r.top + ((r.bottom - r.top - h) / 2).max(0),
            )
        } else {
            (0, 0)
        }
    }
}

/// 隐藏窗口（Slint 的 hide 之外还要保证 WC_EX 层级干净也能收起来）。
pub fn hide_window(hwnd: isize) {
    if hwnd == 0 {
        return;
    }
    unsafe {
        ShowWindow(hwnd as HWND, SW_HIDE);
    }
}

/// 把窗口推到最前并激活。
///
/// Windows 不允许后台进程直接抢前台 (SetForegroundWindow 可能被系统拒绝，
/// 只闪任务栏图标)。这里用标准破招：**把自己的线程输入队列挂到当前前台线程上**
/// （AttachThreadInput），拿到同一输入队列后再 SetForegroundWindow 就合法了。
/// 全部失败也不致命 —— 窗口已经是 TOPMOST，只是键盘焦点可能没过来。
pub fn force_foreground(hwnd: isize) -> bool {
    if hwnd == 0 {
        return false;
    }
    unsafe {
        let h = hwnd as HWND;
        let fg = GetForegroundWindow();
        let mut attached_to = 0;
        if !fg.is_null() && fg != h {
            let fg_thread = GetWindowThreadProcessId(fg, std::ptr::null_mut());
            let my_thread = GetCurrentThreadId();
            if fg_thread != 0 && fg_thread != my_thread && AttachThreadInput(fg_thread, my_thread, 1) != 0 {
                attached_to = fg_thread;
            }
        }
        ShowWindow(h, SW_SHOWNA);
        SetWindowPos(h, HWND_TOPMOST as HWND, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW);
        SetActiveWindow(h);
        let ok = SetForegroundWindow(h) != 0;
        if attached_to != 0 {
            AttachThreadInput(attached_to, GetCurrentThreadId(), 0);
        }
        ok
    }
}

/// 把窗口移到指定**物理像素**位置（不改大小、不改 Z 序、不激活）。
///
/// 为什么不用 Slint 的 `Window::set_position(LogicalPosition)`：2026-10-22 实测，
/// 在 125% 缩放下它会把位置再乘一次缩放（×1.25²），鼠标 (1213,1350) 时窗口被放到
/// (1531,1248) 甚至跑出屏幕底部，截图里下半截是黑的。Win32 的 SetWindowPos 用物理
/// 像素、绝对可预测，一步到位。
pub fn move_window(hwnd: isize, x: i32, y: i32) {
    if hwnd == 0 {
        return;
    }
    unsafe {
        SetWindowPos(
            hwnd as HWND,
            std::ptr::null_mut(),
            x,
            y,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

/// 把窗口夹回「所在显示器的工作区」（只挪位置，不改尺寸）。返回是否真的挪动了。
///
/// 为什么需要：子窗口是以主面板为中心弹出的（`center_on_parent`），而主面板是跟随鼠标的 ——
/// 在屏幕底部呼出面板时，设置页会被顶到屏幕外面，底部一截根本看不见。
/// 2026-10-24 实测：设置页 bottom=1562 > 屏幕高 1440，底部 122px 全是黑的
/// （ImageGrab 抓到屏幕外的结果），pagetest 的「设置页底栏 footBg」因此间歇性失败。
/// 居中后用显示器工作区限制四边，不把副屏的合法负坐标当成越界。
///
/// ⚠️ 用窗口中心点找所在显示器，多显示器下不能拿主显示器算。
pub fn clamp_window_to_work_area(hwnd: isize, margin: i32) -> bool {
    if hwnd == 0 {
        return false;
    }
    unsafe {
        let mut r = std::mem::zeroed::<RECT>();
        if GetWindowRect(hwnd as HWND, &mut r) == 0 {
            return false;
        }
        let w = r.right - r.left;
        let h = r.bottom - r.top;
        let pt = POINT {
            x: r.left + w / 2,
            y: r.top + h / 2,
        };
        let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        if mon.is_null() {
            return false;
        }
        let mut mi = std::mem::zeroed::<MONITORINFO>();
        mi.cb_size = std::mem::size_of::<MONITORINFO>() as DWORD;
        if GetMonitorInfoW(mon, &mut mi) == 0 {
            return false;
        }
        let work = mi.rc_work;
        let min_x = work.left + margin;
        let min_y = work.top + margin;
        let max_x = work.right - w - margin;
        let max_y = work.bottom - h - margin;
        let x = r.left.min(max_x.max(min_x)).max(min_x);
        let y = r.top.min(max_y.max(min_y)).max(min_y);
        if x != r.left || y != r.top {
            SetWindowPos(
                hwnd as HWND,
                std::ptr::null_mut(),
                x,
                y,
                0,
                0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
            return true;
        }
    }
    false
}

/// 同步调整窗口物理像素尺寸，并确保整个窗口完整处于所在显示器的任务栏上方工作区（`rc_work`）内。
///
/// 当主面板因群组切换或搜索导致行数（2~6 行）动态变化时，若窗口原本靠近屏幕底部/任务栏，
/// 直接增高会导致下半部分被任务栏遮挡。此函数在设置新尺寸 `(w, h)` 的同时将 `y` 自动上移到
/// `work.bottom - h - margin`，保证面板 100% 完整展示在任务栏上方区域。
pub fn resize_and_clamp_window(hwnd: isize, w: i32, h: i32, margin: i32) {
    if hwnd == 0 || w <= 0 || h <= 0 {
        return;
    }
    unsafe {
        let mut r = std::mem::zeroed::<RECT>();
        if GetWindowRect(hwnd as HWND, &mut r) == 0 {
            return;
        }
        let pt = POINT {
            x: r.left + (r.right - r.left) / 2,
            y: r.top + (r.bottom - r.top) / 2,
        };
        let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let mut x = r.left;
        let mut y = r.top;
        if !mon.is_null() {
            let mut mi = std::mem::zeroed::<MONITORINFO>();
            mi.cb_size = std::mem::size_of::<MONITORINFO>() as DWORD;
            if GetMonitorInfoW(mon, &mut mi) != 0 {
                let work = mi.rc_work;
                let min_x = work.left + margin;
                let min_y = work.top + margin;
                let max_x = (work.right - w - margin).max(min_x);
                let max_y = (work.bottom - h - margin).max(min_y);
                x = x.clamp(min_x, max_x);
                y = y.clamp(min_y, max_y);
            }
        }
        SetWindowPos(
            hwnd as HWND,
            std::ptr::null_mut(),
            x,
            y,
            w,
            h,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

/// 设置/取消窗口置顶。
///
/// 为什么子窗口也要置顶：主面板呼出时是 TOPMOST（不然会被别的窗口盖住，呼出就白费了），
/// 设置页/属性页若不置顶就会被主面板压在下面 —— 2026-10-22 实测到（截图里设置页被面板遮住）。
pub fn set_topmost(hwnd: isize, on: bool) {
    if hwnd == 0 {
        return;
    }
    unsafe {
        let after = if on {
            HWND_TOPMOST as HWND
        } else {
            (-2isize) as HWND // HWND_NOTOPMOST
        };
        SetWindowPos(
            hwnd as HWND,
            after,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

/// 取消 TOPMOST 置顶（面板收起前调用，避免留下一个永远压着别的窗口的隐形 TopMost）。
pub fn unset_topmost(hwnd: isize) {
    set_topmost(hwnd, false);
}

#[link(name = "user32")]
extern "system" {
    fn PostMessageW(h: HWND, m: UINT, w: WPARAM, l: LPARAM) -> BOOL;
}

/// 前台窗口是否就是指定窗口。键盘轮询用它避免把子窗口按键转发给主面板。
pub fn is_foreground_window(hwnd: isize) -> bool {
    hwnd != 0 && unsafe { GetForegroundWindow() == hwnd as HWND }
}

/// 焦点现在停留在本进程的某个窗口里吗？
///
/// 失焦收起判断要用这个而不是「前台 == 面板」：设置页/属性页是本进程的独立窗口，
/// 焦点在它们身上时不能把面板收走（否则会连子窗口的宿主一起抽掉，体验很怪）。
pub fn is_foreground_in_process() -> bool {
    unsafe {
        let fg = GetForegroundWindow();
        if fg.is_null() {
            return false;
        }
        let mut pid: DWORD = 0;
        GetWindowThreadProcessId(fg, &mut pid);
        pid != 0 && pid == GetCurrentProcessId()
    }
}

/// 从 exe 里取图标（备用路径：ExtractIconW 已在上面声明）。
pub fn is_window_visible(hwnd: isize) -> bool {
    if hwnd == 0 {
        return false;
    }
    unsafe { IsWindowVisible(hwnd as HWND) != 0 }
}

/// 判断鼠标当前是否位于指定窗口矩形内（用于失焦检测时保护拖放与悬停交互）。
pub fn is_cursor_in_window(hwnd: isize) -> bool {
    if hwnd == 0 {
        return false;
    }
    unsafe {
        let mut pt = POINT { x: 0, y: 0 };
        let mut rc = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        if GetCursorPos(&mut pt) == 0 || GetWindowRect(hwnd as HWND, &mut rc) == 0 {
            return false;
        }
        pt.x >= rc.left && pt.x < rc.right && pt.y >= rc.top && pt.y < rc.bottom
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timed_out_button_hotkeys_cannot_commit_but_completed_reply_is_kept() {
        use std::{sync::atomic::AtomicU8, time::Duration};
        let state = AtomicU8::new(0);
        let (_tx, rx) = std::sync::mpsc::channel();
        assert_eq!(wait_button_hotkeys(rx, &state, Duration::ZERO), None);
        assert!(state.compare_exchange(0, 2, Ordering::SeqCst, Ordering::SeqCst).is_err());
        let state = AtomicU8::new(2);
        let (tx, rx) = std::sync::mpsc::channel();
        let reply = std::thread::spawn(move || { tx.send(vec![(1, 2)]).unwrap(); });
        assert_eq!(wait_button_hotkeys(rx, &state, Duration::ZERO), Some(vec![(1, 2)]));
        reply.join().unwrap();
    }

    #[test]
    fn popup_centers_current_size_and_clamps_to_work_area() {
        for (w,h,margin) in [(520,342,8),(650,428,10),(780,513,12),(780,1005,12)] {
            let work = RECT {left:0,top:0,right:2560,bottom:1440};
            assert_eq!(centered_popup_position(POINT{x:1280,y:720},&work,w,h,margin), (1280-w/2,720-h/2));
            let work = RECT {left:-2560,top:-1440,right:0,bottom:0};
            assert_eq!(centered_popup_position(POINT{x:-1280,y:-720},&work,w,h,margin), (-1280-w/2,-720-h/2));
            assert_eq!(centered_popup_position(POINT{x:-1,y:-1},&work,w,h,margin), (-w-margin,-h-margin));
            assert_eq!(centered_popup_position(POINT{x:-2559,y:-1439},&work,w,h,margin), (-2560+margin,-1440+margin));
        }
        assert_eq!(centered_popup_position(POINT{x:100,y:100}, &RECT{left:0,top:0,right:400,bottom:300},520,342,8), (8,8));
    }

    #[test]
    fn parse_hotkey_ok() {
        let hk = parse_hotkey("Ctrl+Alt+Space").unwrap();
        assert_eq!(hk.vk, 0x20);
        assert_eq!(hk.mods, MOD_CONTROL | MOD_ALT | MOD_NOREPEAT);

        let hk = parse_hotkey("ctrl+shift+f1").unwrap();
        assert_eq!(hk.vk, 0x70); // VK_F1
        assert_eq!(hk.mods & MOD_SHIFT, MOD_SHIFT);
    }

    #[test]
    fn parse_hotkey_bad() {
        assert!(parse_hotkey("Ctrl+Alt+不存在的键").is_none());
        assert!(parse_hotkey("").is_none());
        assert!(parse_hotkey("A").is_none(), "禁止无修饰键的全局单键");
        assert!(parse_hotkey("Ctrl+A+B").is_none(), "只允许一个主键");
        assert!(parse_hotkey("Ctrl+Ctrl+A").is_none(), "拒绝重复修饰键");
        assert!(parse_hotkey("Ctrl+").is_none());
        assert!(parse_hotkey("Ctrl+Unknown").is_none());
    }

    #[test]
    fn tray_menu_uses_registered_hotkey() {
        let hk = parse_hotkey("Ctrl+Shift+F8").unwrap();
        assert_eq!(hotkey_menu_label(Some(hk)), "显示面板\tCtrl + Shift + F8");
        assert_eq!(hotkey_menu_label(None), "显示面板");
    }

    #[test]
    fn struct_sizes() {
        // 布局一旦写错，Shell_NotifyIconW 会静默失败，这里用 size 兜底
        assert_eq!(std::mem::size_of::<POINT>(), 8);
        assert_eq!(std::mem::size_of::<RECT>(), 16);
        assert_eq!(std::mem::size_of::<MONITORINFO>(), 40);
        // 与 Windows SDK 的完整 NOTIFYICONDATAW 对齐，不能混用 128 字符提示与 V1 前缀。
        assert_eq!(std::mem::size_of::<NOTIFYICONDATAW>(), 976);
        assert_eq!(std::mem::offset_of!(NOTIFYICONDATAW, dw_state), 296);
        assert_eq!(std::mem::offset_of!(NOTIFYICONDATAW, guid_item), 952);
        assert_eq!(std::mem::offset_of!(NOTIFYICONDATAW, h_balloon_icon), 968);
        let nid = unsafe { icon_data(std::ptr::null_mut(), std::ptr::null_mut(), &"😀".repeat(100)) };
        assert_eq!(nid.cb_size, 976);
        assert_eq!(nid.sz_tip[126..], [0, 0]);
    }
}
