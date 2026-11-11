// zaplaunch-core：纯逻辑数据层——配置模型 + JSON 持久化。
// 职责边界：不碰 UI；Windows 中文首字母借助系统编码转换，其他平台使用有限回退。
//
// 配置文件：%APPDATA%\ZapLaunch\config.json（用户级，绿色单 exe 不写注册表）。
// 对应逆向档案：原版 TurboLaunch 把按钮存 HKCU\Software\TurboLaunch\ButtonConfigs
// 等六个注册表键 + IconCache.dat；本项目换成单个 JSON（PRD 第 5 节映射表）。

use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static CONFIG_SAVE_LOCK: Mutex<()> = Mutex::new(());
static BACKUP_WARNING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn take_backup_warning() -> bool { BACKUP_WARNING.swap(false, Ordering::SeqCst) }

pub const APP_DIR_NAME: &str = "ZapLaunch";
pub const CONFIG_FILE: &str = "config.json";
pub const CONFIG_VERSION: u32 = 2;
pub const CONFIG_DIR_ENV: &str = "ZAPLAUNCH_CONFIG_DIR";

/// 配置加载结果。`read_only=true` 时禁止任何自动重建或设置写回，保护原始现场。
#[derive(Debug)]
pub struct LoadOutcome {
    pub config: AppConfig,
    pub read_only: bool,
    pub warning: Option<String>,
}

impl LoadOutcome {
    fn ready(config: AppConfig) -> Self {
        Self { config, read_only: false, warning: None }
    }

    fn read_only(config: AppConfig, warning: String) -> Self {
        Self { config, read_only: true, warning: Some(warning) }
    }
}

/// 一个启动按钮。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ButtonConfig {
    /// 磁贴下显示的名称。
    pub label: String,
    /// 启动目标。ShellExecute 语义，支持：
    /// 绝对/相对 exe 路径、裸程序名（走系统搜索路径）、文件夹、文档、URL、协议（ms-screenclip: 等）。
    pub target: String,
    /// 命令行参数（原版 ParameterHistory 对应项）。
    #[serde(default)]
    pub params: String,
    /// 工作目录；空 = 继承本进程当前目录。
    #[serde(default)]
    pub work_dir: String,
    /// 图标名（对应 icons.rs 的 IconDef.label 索引键）。
    /// 空/找不到 → 装配层 fallback 图标。将来属性编辑器支持自定义图标时扩展。
    #[serde(default)]
    pub icon: String,
    /// 独立全局快捷键；空字符串表示未设置。
    #[serde(default)]
    pub hotkey: String,
    #[serde(default = "default_window_state")]
    pub window_state: String,
    #[serde(default)]
    pub launch_on_startup: bool,
    /// 被启动过的次数（「按使用次数排序」的唯一数据来源）。
    ///
    /// ⚠️ 不能从 `history` 统计：history 受 `history_limit`（默认 20）截断，
    /// 常用按钮早就跑过几百次但历史里最多留 20 条，统计出来恒等于上限、排序退化成随机序。
    #[serde(default)]
    pub run_count: u32,
}

/// 单层群组（P0 铁律：不做两级分类）。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct GroupConfig {
    pub name: String,
    pub buttons: Vec<ButtonConfig>,
}

/// 应用配置根。
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AppConfig {
    pub version: u32,
    /// 主题："light" | "dark"。
    pub theme: String,
    /// 开机自启（HKCU\...\Run 键，由 zaplaunch-sys 落注册表；此字段记录开关意图）。
    #[serde(default)]
    pub auto_start: bool,
    /// 面板是否跟随鼠标位置弹出（Quicker 式：在鼠标所在处露出来）。
    #[serde(default = "default_true")]
    pub follow_mouse: bool,
    /// 全局呼出热键，格式 "Ctrl+Alt+Space"（由 zaplaunch-sys::tray 解析注册）。
    #[serde(default = "default_hotkey")]
    pub hotkey: String,
    /// 启动按钮后是否自动收起面板（即用即走）。
    #[serde(default = "default_true")]
    pub hide_after_run: bool,
    /// 面板失焦后是否自动收起。
    #[serde(default = "default_true")]
    pub hide_on_blur: bool,
    // ---- 批次 D 新增（设计规格 7.13 配置字段表）----
    /// 磁贴上是否显示名称（关闭则只留图标，密度更高）。
    #[serde(default = "default_true")]
    pub tile_show_label: bool,
    /// 按钮超出两行时是否显示网格滚动条。
    #[serde(default = "default_true")]
    pub grid_scrollbar: bool,
    /// 显示图标格子之间的细分隔线，旧配置默认关闭。
    #[serde(default)]
    pub grid_lines: bool,
    /// 单击磁贴即启动；关闭则改为双击启动。
    #[serde(default = "default_true")]
    pub click_to_run: bool,
    /// 事件声音总开关（启动 / 拖入 / 切群组三事件，见 7.8-b）。
    #[serde(default = "default_true")]
    pub sound_enabled: bool,
    /// 托盘图标左键单击动作："panel"（显示面板）| "none"（无动作）。
    #[serde(default = "default_tray_click")]
    pub tray_click: String,
    /// 面板内 Esc 是否收起面板。
    #[serde(default = "default_true")]
    pub esc_hide: bool,
    // ---- 批次 E 新增：面板布局（设计规格 7.13 配置字段表）----
    /// 一行显示几个磁贴（6 ~ 8，默认 7）。
    ///
    /// 7 是能放下最长按钮名的最大列数：格宽 = (496 − (n−1)×2) / n，
    /// n=7 → 69px，「资源管理器」5 字需 60px 放得下；n=8 → 60px 会显示省略号。
    #[serde(default = "default_grid_columns")]
    pub grid_columns: u32,
    /// 磁贴排序方式："manual"（手工顺序）| "usage"（按使用次数降序）| "name"（按名称）。
    ///
    /// 只在显示时排序，不改写 `groups` 里的实际顺序——关掉开关手工顺序原样回来。
    #[serde(default = "default_tile_order")]
    pub tile_order: String,
    /// 是否记录按钮启动时间到运行历史。
    #[serde(default = "default_true")]
    pub history_enabled: bool,
    /// 运行历史保留条数上限。
    #[serde(default = "default_history_limit")]
    pub history_limit: u32,
    /// 运行历史（新的在前）。
    #[serde(default)]
    pub history: Vec<HistoryEntry>,

    pub groups: Vec<GroupConfig>,
}

/// 一条运行历史（原版 RunHistory 键的现代形态，见设计规格 7.11）。
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct HistoryEntry {
    #[serde(default = "default_window_state")]
    pub window_state: String,
    /// 启动目标（与按钮 target 同格式）。
    pub target: String,
    /// 按钮名称。
    pub label: String,
    /// Unix 时间戳（秒）。
    pub ts: i64,
    /// 该次启动的参数与工作目录快照，供“重开该次启动”完整复现。
    #[serde(default)]
    pub params: String,
    #[serde(default)]
    pub work_dir: String,
}

fn default_true() -> bool {
    true
}

fn default_window_state() -> String { "normal".into() }

pub fn window_show_mode(state: &str) -> Option<i32> {
    match state { "normal" => Some(1), "minimized" => Some(2), "maximized" => Some(3), "hidden" => Some(0), _ => None }
}

fn migrate_config(c: &mut AppConfig) {
    if c.version == 1 { c.version = CONFIG_VERSION; }
}

fn preserve_v1(dir: &Path, text: &str) -> io::Result<()> {
    use std::io::Write;
    for i in 0.. {
        let path = dir.join(if i == 0 { "config.v1.bak".into() } else { format!("config.v1.{i}.bak") });
        match fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                let result = file.write_all(text.as_bytes()).and_then(|_| file.sync_all());
                if result.is_err() { let _ = fs::remove_file(path); }
                return result;
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                if read_config_text(&path).ok().as_deref() == Some(text) { return Ok(()); }
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}

fn default_hotkey() -> String {
    "Ctrl+Alt+Space".into()
}

fn default_tray_click() -> String {
    "panel".into()
}

fn default_history_limit() -> u32 {
    20
}

fn default_grid_columns() -> u32 {
    7
}

fn default_tile_order() -> String {
    "manual".into()
}

/// 一行显示数量的合法范围（设置页下拉只给这三档）。
pub const GRID_COLUMNS_MIN: u32 = 6;
pub const GRID_COLUMNS_MAX: u32 = 8;

/// 把列数夹到合法范围（读配置 / 改设置两条路都过一遍，杜绝越界导致格宽算成负数）。
pub fn clamp_grid_columns(n: u32) -> u32 {
    n.clamp(GRID_COLUMNS_MIN, GRID_COLUMNS_MAX)
}

fn validate_config(c: &AppConfig) -> io::Result<()> {
    if c.version != CONFIG_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("配置版本 {} 与当前版本 {CONFIG_VERSION} 不同", c.version),
        ));
    }
    if c.groups.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "配置至少需要一个群组"));
    }
    if c.groups.iter().any(|g| g.name.trim().is_empty()) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "群组名称不能为空"));
    }
    if c.groups.iter().flat_map(|g| &g.buttons).map(|b| b.window_state.as_str())
        .chain(c.history.iter().map(|h| h.window_state.as_str())).any(|s| window_show_mode(s).is_none()) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "未知的目标窗口状态"));
    }
    Ok(())
}

fn normalize_config(c: &mut AppConfig) {
    c.grid_columns = clamp_grid_columns(c.grid_columns);
    for b in c.groups.iter_mut().flat_map(|g| &mut g.buttons) {
        if b.label == "绘图" && b.target.eq_ignore_ascii_case("mspaint.exe") {
            b.label = "画图".into();
        }
    }
}

impl AppConfig {
    /// 在指定群组末尾新增一个按钮；索引非法返回 false（不 panic）。
    pub fn add_button(&mut self, group: usize, button: ButtonConfig) -> bool {
        match self.groups.get_mut(group) {
            Some(g) => {
                g.buttons.push(button);
                true
            }
            None => false,
        }
    }

    /// 覆盖指定群组的第 index 个按钮；索引非法返回 false。
    pub fn update_button(&mut self, group: usize, index: usize, button: ButtonConfig) -> bool {
        match self.groups.get_mut(group).and_then(|g| g.buttons.get_mut(index)) {
            Some(slot) => {
                *slot = button;
                true
            }
            None => false,
        }
    }

    /// 按最终下标移动整个分组，保留组内按钮和其他分组的相对顺序。
    pub fn move_group(&mut self, from: usize, to: usize) -> bool {
        if from >= self.groups.len() || to >= self.groups.len() { return false; }
        if from != to {
            let group = self.groups.remove(from);
            self.groups.insert(to,group);
        }
        true
    }

    /// 删除指定群组的第 index 个按钮；索引非法返回 false。
    pub fn remove_button(&mut self, group: usize, index: usize) -> bool {
        match self.groups.get_mut(group) {
            Some(g) if index < g.buttons.len() => {
                g.buttons.remove(index);
                true
            }
            _ => false,
        }
    }

    /// 在同一群组内按插入位置移动按钮；`before` 是移动前序列中的插入边界。
    pub fn move_button(&mut self, group: usize, from: usize, before: usize) -> bool {
        let Some(buttons) = self.groups.get_mut(group).map(|g| &mut g.buttons) else { return false; };
        if from >= buttons.len() || before > buttons.len() { return false; }
        let button = buttons.remove(from);
        buttons.insert(if before > from { before - 1 } else { before }, button);
        true
    }

    /// 跨组移动整个按钮，目标组末尾追加；先校验，非法坐标不修改数据。
    pub fn move_button_to_group(&mut self, from: usize, index: usize, to: usize) -> bool {
        if to >= self.groups.len() || self.groups.get(from).and_then(|g| g.buttons.get(index)).is_none() { return false; }
        if from != to {
            let button = self.groups[from].buttons.remove(index);
            self.groups[to].buttons.push(button);
        }
        true
    }

    /// 新增一个群组（末尾追加）；名称为空或已存在同名群组时返回 None，成功返回新群组下标。
    pub fn add_group(&mut self, name: String) -> Option<usize> {
        let trimmed = name.trim();
        if trimmed.is_empty() || self.groups.iter().any(|g| g.name == trimmed) {
            return None;
        }
        self.groups.push(GroupConfig {
            name: trimmed.to_string(),
            buttons: Vec::new(),
        });
        Some(self.groups.len() - 1)
    }

    /// 重命名指定群组；名称为空、越界或与其他群组重名时返回 false。
    pub fn rename_group(&mut self, group: usize, name: String) -> bool {
        let trimmed = name.trim();
        if trimmed.is_empty() || group >= self.groups.len() {
            return false;
        }
        if self
            .groups
            .iter()
            .enumerate()
            .any(|(i, g)| i != group && g.name == trimmed)
        {
            return false;
        }
        self.groups[group].name = trimmed.to_string();
        true
    }

    /// 删除指定群组（至少保留 1 个群组，防止无群组状态）。
    pub fn remove_group(&mut self, group: usize) -> bool {
        if self.groups.len() <= 1 || group >= self.groups.len() {
            return false;
        }
        self.groups.remove(group);
        true
    }

    /// 「恢复默认设置」：只重置设置项，**按钮与群组保留**（设计规格 7.8 判定）。
    ///
    /// 主题算设置项（不是用户数据），所以一并回到默认 `system`——与旧实现的
    /// `reset_defaults_keep_theme`（保留主题 + 重置按钮）语义相反，别搞混。
    pub fn reset_settings(&mut self) {
        let groups = std::mem::take(&mut self.groups);
        let history = std::mem::take(&mut self.history);
        *self = AppConfig::default();
        self.groups = groups;
        self.history = history;
    }
}

/// 子串大小写不敏感匹配：纯 ASCII 查询词走字节级零分配滑动窗口，非 ASCII 查询词走 Unicode 小写折叠。
fn contains_case_insensitive(haystack: &str, needle_lower: &str, is_ascii: bool) -> bool {
    if is_ascii {
        let nb = needle_lower.as_bytes();
        if nb.is_empty() {
            return true;
        }
        haystack
            .as_bytes()
            .windows(nb.len())
            .any(|w| w.eq_ignore_ascii_case(nb))
    } else {
        haystack.to_lowercase().contains(needle_lower)
    }
}

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn WideCharToMultiByte(
        CodePage: u32,
        dwFlags: u32,
        lpWideCharStr: *const u16,
        cchWideChar: i32,
        lpMultiByteStr: *mut u8,
        cbMultiByte: i32,
        lpDefaultChar: *const u8,
        lpUsedDefaultChar: *mut i32,
    ) -> i32;
}

/// 提取单个中文字符的汉语拼音首字母（小写 `a`..=`z`）。
///
/// 利用 GB2312 (CP936) 一级字库（3755 个常用汉字，覆盖 99.9% 软件名称用字）按拼音排序的特性，
/// 仅用 23 个 `u16` 码点边界即可实现零体积增量、微秒级转换。
fn char_pinyin_initial(ch: char) -> Option<u8> {
    if ch.is_ascii_alphanumeric() {
        return Some(ch.to_ascii_lowercase() as u8);
    }
    if ('\u{4E00}'..='\u{9FA5}').contains(&ch) {
        // 常见多音字在软件名中的高频读音优先修正
        match ch {
            '行' => return Some(b'h'), // 命令行 (hmh / mlh)
            '重' => return Some(b'c'), // 重启
            '长' => return Some(b'c'),
            '调' => return Some(b't'), // 调试
            _ => {}
        }
        #[cfg(windows)]
        unsafe {
            let wide_ch = ch as u16;
            let mut gb = [0u8; 2];
            // CP936 = GBK / GB2312
            let len = WideCharToMultiByte(
                936,
                0,
                &wide_ch,
                1,
                gb.as_mut_ptr(),
                2,
                std::ptr::null(),
                std::ptr::null_mut(),
            );
            if len == 2 {
                let code = u16::from_be_bytes(gb);
                const RANGES: &[(u16, u16, u8)] = &[
                    (0xB0A1, 0xB0C4, b'a'),
                    (0xB0C5, 0xB2C0, b'b'),
                    (0xB2C1, 0xB4ED, b'c'),
                    (0xB4EE, 0xB6E9, b'd'),
                    (0xB6EA, 0xB7A1, b'e'),
                    (0xB7A2, 0xB8C0, b'f'),
                    (0xB8C1, 0xB9FD, b'g'),
                    (0xB9FE, 0xBBF6, b'h'),
                    (0xBBF7, 0xBFA5, b'j'),
                    (0xBFA6, 0xC0AB, b'k'),
                    (0xC0AC, 0xC2E7, b'l'),
                    (0xC2E8, 0xC4C2, b'm'),
                    (0xC4C3, 0xC5B5, b'n'),
                    (0xC5B6, 0xC5BD, b'o'),
                    (0xC5BE, 0xC6D9, b'p'),
                    (0xC6DA, 0xC8BA, b'q'),
                    (0xC8BB, 0xC8F5, b'r'),
                    (0xC8F6, 0xCBF9, b's'),
                    (0xCBFA, 0xCDD9, b't'),
                    (0xCDDA, 0xCEF3, b'w'),
                    (0xCEF4, 0xD1B8, b'x'),
                    (0xD1B9, 0xD4D0, b'y'),
                    (0xD4D1, 0xD7F9, b'z'),
                ];
                for &(start, end, initial) in RANGES {
                    if code >= start && code <= end {
                        return Some(initial);
                    }
                }
            }
        }
    }
    None
}

/// 将字符串转换为拼音首字母与英文数字缩写串（如 `"微信图片_01"` -> `"wxtp01"`）。
pub fn pinyin_initials(s: &str) -> String {
    // 多音字不能按单个汉字全局固定读音；先处理常见词语，再退回 GB2312 首字母表。
    const PHRASES: &[(&str, &str)] = &[
        ("重量", "zl"),
        ("旅行", "lx"),
        ("调动", "dd"),
        ("重庆", "cq"),
        ("长度", "cd"),
        ("银行", "yh"),
        ("行业", "hy"),
    ];
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while !rest.is_empty() {
        if let Some((phrase, initials)) = PHRASES.iter().find(|(phrase, _)| rest.starts_with(phrase)) {
            out.push_str(initials);
            rest = &rest[phrase.len()..];
            continue;
        }
        let ch = rest.chars().next().expect("非空字符串必有字符");
        if let Some(b) = char_pinyin_initial(ch) {
            out.push(b as char);
        }
        rest = &rest[ch.len_utf8()..];
    }
    out
}

/// 常见中文软件与系统工具的全拼/英文搜索别名扩展，让输入 `wechat` / `weixin` / `notepad` 等均能命中对应中文按钮。
fn builtin_alias_match(label: &str, needle_lower: &str) -> bool {
    const ALIASES: &[(&str, &str)] = &[
        ("微信", "weixin wechat tencent wx"),
        ("企业微信", "qiyeweixin wxwork wecom qywx"),
        ("钉钉", "dingding dingtalk dd"),
        ("飞书", "feishu lark fs"),
        ("网易云", "wangyiyun cloudmusic wyy"),
        ("音乐", "yinyue music"),
        ("百度", "baidu bd"),
        ("迅雷", "xunlei thunder xl"),
        ("浏览器", "liulanqi browser"),
        ("资源管理器", "ziyuanguanliqi explorer file"),
        ("记事本", "jishiben notepad txt"),
        ("计算器", "jisuanqi calc calculator"),
        ("绘图", "huitu huatu mspaint paint"),
        ("画图", "huatu huitu mspaint paint"),
        ("命令行", "minglinghang cmd command mlh"),
        ("终端", "zhongduan terminal wt"),
        ("截图", "jietu snip screenshot"),
        ("任务管理器", "renwuguanliqi taskmgr"),
        ("控制面板", "kongzhimianban control"),
        ("回收站", "huishouzhan recycle trash"),
        ("设置", "shezhi settings"),
        ("关于系统", "guanyuxitong winver system"),
    ];
    for &(kw, aliases) in ALIASES {
        if label.contains(kw)
            && aliases
                .split_whitespace()
                .any(|alias| alias.starts_with(needle_lower))
        {
            return true;
        }
    }
    false
}

/// 判断单个按钮是否匹配搜索词（支持：原名/路径子串、中文拼音首字母缩写、高频全拼/英文名、以及 `.lnk` 真实目标路径）。
fn button_matches_query(
    b: &ButtonConfig,
    q: &str,
    is_ascii: bool,
    resolved_target: Option<&str>,
) -> bool {
    if contains_case_insensitive(&b.label, q, is_ascii)
        || contains_case_insensitive(&b.target, q, is_ascii)
    {
        return true;
    }
    if is_ascii {
        // ① 匹配中文名称的拼音首字母缩写（如 "wx" -> "微信"，"zyglq" -> "资源管理器"）
        let label_initials = pinyin_initials(&b.label);
        if !label_initials.is_empty() && label_initials.contains(q) {
            return true;
        }
        // ② 匹配内置高频软件全拼与英文别名（如 "weixin" / "wechat" -> "微信"）
        if builtin_alias_match(&b.label, q) {
            return true;
        }
    }
    // ③ 若是 .lnk 快捷方式，匹配其指向的真实目标路径与程序名（如 `...\WeChat\WeChat.exe`）
    if let Some(real_path) = resolved_target {
        if !real_path.is_empty() && contains_case_insensitive(real_path, q, is_ascii) {
            return true;
        }
    }
    false
}

/// 搜索匹配（③ 搜索态，设计规格 7.2）。
///
/// 跨全部群组匹配 `label`、`target`、中文拼音首字母缩写及高频英文别名。
/// 排序：**当前群组的命中项在最前面**（保持群内原有顺序），其余群组按配置顺序跟在后面。
pub fn search_buttons(c: &AppConfig, query: &str, current: usize) -> Vec<(usize, usize)> {
    search_buttons_smart(c, query, current, |_| None)
}

/// 带快捷方式真实目标路径解析器的智能搜索匹配。
pub fn search_buttons_smart(
    c: &AppConfig,
    query: &str,
    current: usize,
    mut resolve_extra: impl FnMut(&ButtonConfig) -> Option<String>,
) -> Vec<(usize, usize)> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    let is_ascii = q.is_ascii();
    let mut here: Vec<(usize, usize)> = Vec::new();
    let mut there: Vec<(usize, usize)> = Vec::new();
    for (gi, g) in c.groups.iter().enumerate() {
        // 当前群组的命中单独放一桶，最后拼到前面（其余群组按遍历顺序天然有序）
        let bucket = if gi == current { &mut here } else { &mut there };
        for (bi, b) in g.buttons.iter().enumerate() {
            let resolved = resolve_extra(b);
            if button_matches_query(b, &q, is_ascii, resolved.as_deref()) {
                bucket.push((gi, bi));
            }
        }
    }
    here.append(&mut there);
    here
}

/// 相对时间文案（⑬ 运行历史浮层的右端时间戳，设计规格 7.11）：
/// 刚刚 / N 分钟前 / N 小时前 / 昨天 HH:MM / N 天前。
///
/// `ts`、`now` 为 Unix 秒；`utc_offset` 为本地时区偏移秒（由 zaplaunch-sys 提供，保持本 crate 与平台无关）。
/// 时钟回拨或未来时间戳按「刚刚」处理，绝不 panic。
pub fn format_rel_time(ts: i64, now: i64, utc_offset: i64) -> String {
    let d = now.saturating_sub(ts);
    if d < 60 {
        return "刚刚".into();
    }
    if d < 3600 {
        return format!("{} 分钟前", d / 60);
    }
    if d < 86400 {
        return format!("{} 小时前", d / 3600);
    }
    if d < 172800 {
        let local = ts.saturating_add(utc_offset);
        let hh = local.div_euclid(3600).rem_euclid(24);
        let mm = local.div_euclid(60).rem_euclid(60);
        return format!("昨天 {hh:02}:{mm:02}");
    }
    format!("{} 天前", d / 86400)
}

/// 配置目录：测试/便携场景可用环境变量或 `--config-dir` 显式覆盖。
/// 覆盖值就是最终目录，不再追加 ZapLaunch 子目录。
pub fn config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os(CONFIG_DIR_ENV).filter(|v| !v.is_empty()) {
        return PathBuf::from(dir);
    }
    let base = std::env::var_os("APPDATA")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join(APP_DIR_NAME)
}

pub fn config_path() -> PathBuf {
    config_dir().join(CONFIG_FILE)
}

/// 导出：把当前配置写成一个 JSON 文件（设置页 ⑩-d「导出」）。
///
/// 走和 `save()` 一样的原子写：先写 .tmp 再 rename，写一半不会留下半个文件。
/// 导出的是**同一份结构**，导出的文件可以直接当配置用（整机迁移即拷贝回去）。
pub fn export_to(config: &AppConfig, path: &Path) -> io::Result<()> {
    write_config_to(config, path, None)
}

fn unique_temp_path(target: &Path) -> PathBuf {
    let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".tmp.{}.{sequence}", std::process::id()));
    target.with_file_name(name)
}

const MAX_CONFIG_BYTES: usize = 16 * 1024 * 1024;

fn check_config_size(bytes: usize) -> io::Result<()> {
    if bytes > MAX_CONFIG_BYTES {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "配置文件超过 16 MiB"));
    }
    Ok(())
}

fn read_config_text(path: &Path) -> io::Result<String> {
    use std::io::Read;
    let mut bytes = Vec::new();
    fs::File::open(path)?.take(MAX_CONFIG_BYTES as u64 + 1).read_to_end(&mut bytes)?;
    check_config_size(bytes.len())?;
    String::from_utf8(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// 导入：从 JSON 文件读回一份配置（设置页 ⑩-d「导入」）。
///
/// 失败时**不改动内存里的配置**——先把文件读全、解析并校验，成功才交给调用方替换。
pub fn import_from(path: &Path) -> io::Result<AppConfig> {
    let text = read_config_text(path)?;
    let mut config: AppConfig = serde_json::from_str(&text)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    migrate_config(&mut config);
    validate_config(&config)?;
    for b in config.groups.iter_mut().flat_map(|g| &mut g.buttons) { b.launch_on_startup = false; }
    normalize_config(&mut config);
    Ok(config)
}

/// 读取配置。损坏文件必须先成功备份才允许重建；任一步失败都进入只读模式。
pub fn load_or_init() -> LoadOutcome {
    load_or_init_from(&config_dir())
}

/// 主配置缺失时按新到旧恢复备份。只要发现过备份但没有可用项，
/// 就进入只读保护，不能用默认配置覆盖最后的恢复线索。
fn recover_missing_from_backups(dir: &Path) -> Option<LoadOutcome> {
    let mut found_backup = false;
    let mut errors = Vec::new();
    for i in 1..=5 {
        let backup = dir.join(format!("config.bak.{i}"));
        let text = match read_config_text(&backup) {
            Ok(text) => {
                found_backup = true;
                text
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => {
                found_backup = true;
                errors.push(format!("config.bak.{i} 读取失败：{e}"));
                continue;
            }
        };
        match serde_json::from_str::<AppConfig>(&text) {
            Ok(mut c) if c.version == 1 || c.version == CONFIG_VERSION => {
                if c.version == 1 {
                    if let Err(e) = preserve_v1(dir, &text) {
                        errors.push(format!("config.bak.{i} 升级前备份失败：{e}")); continue;
                    }
                }
                migrate_config(&mut c);
                if let Err(e) = validate_config(&c) {
                    errors.push(format!("config.bak.{i} 校验失败：{e}"));
                    continue;
                }
                normalize_config(&mut c);
                return Some(match save_to(&c, dir) {
                    Ok(()) => LoadOutcome {
                        config: c,
                        read_only: false,
                        warning: Some(format!("主配置缺失，已从 config.bak.{i} 自动恢复")),
                    },
                    Err(e) => LoadOutcome::read_only(
                        c,
                        format!("主配置缺失，config.bak.{i} 可读取但恢复写盘失败：{e}"),
                    ),
                });
            }
            Ok(c) => errors.push(format!(
                "config.bak.{i} 版本 {} 与当前版本 {} 不同",
                c.version, CONFIG_VERSION
            )),
            Err(e) => errors.push(format!("config.bak.{i} 解析失败：{e}")),
        }
    }
    found_backup.then(|| {
        LoadOutcome::read_only(
            AppConfig::default(),
            format!(
                "主配置缺失，且没有可恢复的有效备份；为保护备份已按只读模式运行：{}",
                errors.join("；")
            ),
        )
    })
}

pub fn load_or_init_from(dir: &Path) -> LoadOutcome {
    let path = dir.join(CONFIG_FILE);
    let text = match read_config_text(&path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => {
            return LoadOutcome::read_only(
                AppConfig::default(),
                format!("配置读取失败，已按只读模式运行：{e}"),
            );
        }
    };
    if let Some(text) = text {
        match serde_json::from_str::<AppConfig>(&text) {
            Ok(mut c) if c.version == 1 || c.version == CONFIG_VERSION => {
                let legacy = c.version == 1;
                migrate_config(&mut c);
                if let Err(e) = validate_config(&c) {
                    return LoadOutcome::read_only(c, format!("配置校验失败，已按只读模式运行：{e}"));
                }
                normalize_config(&mut c);
                if legacy {
                    if let Err(e) = preserve_v1(dir, &text) { return LoadOutcome::read_only(c, format!("升级前备份失败，已按只读模式运行：{e}")); }
                }
                return LoadOutcome::ready(c);
            }
            Ok(c) => {
                let warning = format!("配置版本 {} 与当前版本 {} 不同，为避免覆盖数据已按只读模式运行", c.version, CONFIG_VERSION);
                return LoadOutcome::read_only(c, warning);
            }
            Err(e) => {
                let bak = (0..)
                    .map(|i| if i == 0 {
                        dir.join("config.corrupt.bak")
                    } else {
                        dir.join(format!("config.corrupt.{i}.bak"))
                    })
                    .find(|candidate| !candidate.exists())
                    .expect("损坏配置备份序号不可耗尽");
                if let Err(backup_error) = fs::rename(&path, &bak) {
                    return LoadOutcome::read_only(
                        AppConfig::default(),
                        format!("配置损坏且备份失败，原文件未改动：{backup_error}；解析错误：{e}"),
                    );
                }
                if let Some(mut recovered) = recover_missing_from_backups(dir) {
                    recovered.warning = Some(format!(
                        "损坏配置已备份到 {bak:?}；{}",
                        recovered.warning.unwrap_or_default()
                    ));
                    return recovered;
                }
                let c = AppConfig::default();
                return match save_to(&c, dir) {
                    Ok(()) => LoadOutcome {
                        config: c,
                        read_only: false,
                        warning: Some(format!("损坏配置已备份到 {bak:?}，并重建默认配置")),
                    },
                    Err(save_error) => LoadOutcome::read_only(
                        c,
                        format!("损坏配置已备份，但默认配置写入失败：{save_error}"),
                    ),
                };
            }
        }
    }
    if let Some(recovered) = recover_missing_from_backups(dir) {
        return recovered;
    }
    let c = AppConfig::default();
    match save_to(&c, dir) {
        Ok(()) => LoadOutcome::ready(c),
        Err(e) => LoadOutcome::read_only(c, format!("默认配置写入失败，已按只读模式运行：{e}")),
    }
}

/// 原子写回整个配置。
pub fn save(config: &AppConfig) -> io::Result<()> {
    save_to(config, &config_dir())
}

/// 主配置替换成功后，才用替换前的快照滚动最近 5 份备份。
fn rotate_backups(dir: &Path, previous_snapshot: &Path) -> io::Result<()> {
    for i in (1..5).rev() {
        let from = dir.join(format!("config.bak.{i}"));
        let to = dir.join(format!("config.bak.{}", i + 1));
        match fs::metadata(&from) {
            Ok(meta) if meta.is_file() => replace_file(&from, &to)?,
            Ok(_) => return Err(io::Error::new(io::ErrorKind::InvalidData,"备份路径不是文件")),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    replace_file(previous_snapshot, &dir.join("config.bak.1"))
}

#[cfg(windows)]
fn replace_file(tmp: &Path, path: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    extern "system" {
        fn MoveFileExW(existing: *const u16, replacement: *const u16, flags: u32) -> i32;
    }
    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x2;
    let from: Vec<u16> = tmp.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let ok = unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok != 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(windows))]
fn replace_file(tmp: &Path, path: &Path) -> io::Result<()> {
    fs::rename(tmp, path)
}

/// 仅保存启动统计和历史，不挤占用户编辑配置的备份。
pub fn save_usage(config: &AppConfig) -> io::Result<()> {
    save_to_with_backup(config, &config_dir(), false)
}

pub fn save_to(config: &AppConfig, dir: &Path) -> io::Result<()> {
    save_to_with_backup(config, dir, true)
}

pub fn save_to_with_backup(config: &AppConfig, dir: &Path, backup: bool) -> io::Result<()> {
    validate_config(config)?;
    let _guard = CONFIG_SAVE_LOCK.lock()
        .map_err(|_| io::Error::other("配置保存锁已中毒"))?;
    write_config_to(config, &dir.join(CONFIG_FILE), backup.then_some(dir))
}

fn write_config_to(config: &AppConfig, path: &Path, backup_dir: Option<&Path>) -> io::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let tmp = unique_temp_path(path);
    let text = serde_json::to_string_pretty(config)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    check_config_size(text.len())?;
    let result = (|| {
        let mut file = fs::File::create_new(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        let snapshot = if let Some(dir) = backup_dir { match fs::metadata(path) {
            Ok(_) => {
                let snapshot = unique_temp_path(&dir.join("config.bak.1"));
                let staged = (|| {
                    fs::copy(path, &snapshot)?;
                    fs::OpenOptions::new().write(true).open(&snapshot)?.sync_all()
                })();
                if let Err(e) = staged {
                    let _ = fs::remove_file(&snapshot);
                    return Err(e);
                }
                Some(snapshot)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        }} else { None };
        if let Err(e) = replace_file(&tmp, path) {
            if let Some(snapshot) = &snapshot {
                let _ = fs::remove_file(snapshot);
            }
            return Err(e);
        }
        if let (Some(dir), Some(snapshot)) = (backup_dir, snapshot) {
            // 主配置已经提交，备份失败不能把“主保存成功”误报成内存应回滚的失败。
            if let Err(e) = rotate_backups(dir, &snapshot) {
                BACKUP_WARNING.store(true, Ordering::SeqCst);
                eprintln!("配置已保存，但滚动备份失败：{e}");
            }
            let _ = fs::remove_file(snapshot);
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

impl Default for AppConfig {
    /// 首次运行的默认配置：全部是 Windows 系统真实可启动项（点了必有反应），
    /// 图标名精确对应 icons.rs 的 IconDef.label。
    fn default() -> Self {
        fn btn(label: &str, target: &str, params: &str, icon: &str) -> ButtonConfig {
            ButtonConfig {
                label: label.into(),
                target: target.into(),
                params: params.into(),
                work_dir: String::new(),
                icon: icon.into(),
                hotkey: String::new(),
                window_state: "normal".into(),
                launch_on_startup: false,
                run_count: 0,
            }
        }
        AppConfig {
            version: CONFIG_VERSION,
            theme: "system".into(),
            auto_start: false,
            follow_mouse: true,
            hotkey: default_hotkey(),
            hide_after_run: true,
            hide_on_blur: true,
            tile_show_label: true,
            grid_scrollbar: true,
            grid_lines: false,
            click_to_run: true,
            sound_enabled: true,
            tray_click: default_tray_click(),
            esc_hide: true,
            history_enabled: true,
            grid_columns: default_grid_columns(),
            tile_order: default_tile_order(),
            history_limit: default_history_limit(),
            history: Vec::new(),
            groups: vec![
                GroupConfig {
                    name: "常用".into(),
                    buttons: vec![
                        btn("资源管理器", "explorer.exe", "", "资源管理器"),
                        btn("记事本", "notepad.exe", "", "记事本"),
                        btn("计算器", "calc.exe", "", "计算器"),
                        btn("画图", "mspaint.exe", "", "绘图"),
                        btn("命令行", "cmd.exe", "", "命令行"),
                        btn("PowerShell", "powershell.exe", "", "PowerShell"),
                        btn("截图", "ms-screenclip:", "", "截图"),
                        btn("任务管理器", "taskmgr.exe", "", "计时器"),
                        btn("控制面板", "control.exe", "", "换算"),
                        btn("回收站", "explorer.exe", "shell:RecycleBinFolder", "数据库"),
                        btn("Edge", "msedge.exe", "", "Edge"),
                        btn("关于系统", "winver.exe", "", "播放器"),
                    ],
                },
                GroupConfig {
                    name: "工具".into(),
                    buttons: vec![
                        btn("磁盘清理", "cleanmgr.exe", "", "看图"),
                        btn("注册表", "regedit.exe", "", "数据库"),
                        btn("系统信息", "msinfo32.exe", "", "待办"),
                        btn("放大镜", "magnify.exe", "", "看图"),
                        btn("屏幕键盘", "osk.exe", "", "换算"),
                        btn("远程桌面", "mstsc.exe", "", "虚拟机"),
                    ],
                },
                GroupConfig {
                    name: "网页".into(),
                    buttons: vec![
                        btn("音乐", "https://music.163.com", "", "音乐"),
                        btn("视频", "https://www.bilibili.com", "", "视频"),
                        btn("邮箱", "https://mail.qq.com", "", "邮箱"),
                    ],
                },
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_config_read_is_rejected_without_modifying_file() {
        let path = unique_temp_path(&std::env::temp_dir().join("zaplaunch-size-test.json"));
        let size = MAX_CONFIG_BYTES as u64 + 1;
        fs::File::create_new(&path).unwrap().set_len(size).unwrap();
        assert_eq!(import_from(&path).unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert_eq!(fs::metadata(&path).unwrap().len(), size);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn paint_default_name_is_corrected_without_changing_custom_names() {
        let mut c = AppConfig::default();
        let paint = c.groups.iter().flat_map(|g| &g.buttons).find(|b| b.target == "mspaint.exe").unwrap();
        assert_eq!(paint.label,"画图");
        let mut old=paint.clone(); old.label="绘图".into();
        let mut custom=old.clone(); custom.label="我的画板".into();
        let mut other=old.clone(); other.target="other.exe".into();
        c.groups[0].buttons=vec![old,custom.clone(),other.clone()];
        normalize_config(&mut c);
        assert_eq!(c.groups[0].buttons[0].label,"画图");
        assert_eq!(c.groups[0].buttons[0].icon,"绘图");
        assert_eq!(c.groups[0].buttons[1],custom);
        assert_eq!(c.groups[0].buttons[2],other);
    }

    #[test]
    fn v1_migration_preserves_original_and_recovery() {
        let dir = test_dir("v1-upgrade"); fs::create_dir_all(&dir).unwrap();
        let text = r#"{"version":1,"theme":"dark","groups":[{"name":"旧配置","buttons":[{"label":"旧按钮","target":"notepad.exe"}]}]}"#;
        fs::write(dir.join(CONFIG_FILE), text).unwrap();
        let loaded = load_or_init_from(&dir);
        assert!(!loaded.read_only); assert_eq!(loaded.config.version, 2);
        let b = &loaded.config.groups[0].buttons[0];
        assert_eq!(b.window_state, "normal"); assert!(!b.launch_on_startup);
        assert_eq!(fs::read_to_string(dir.join("config.v1.bak")).unwrap(), text);
        save_to(&loaded.config, &dir).unwrap();
        assert_eq!(load_or_init_from(&dir).config.groups[0].buttons[0], *b);
        fs::remove_file(dir.join(CONFIG_FILE)).unwrap();
        let recovered = load_or_init_from(&dir);
        assert!(!recovered.read_only); assert_eq!(recovered.config.version, 2);
        assert_eq!(recovered.config.groups[0].buttons[0], *b);
    }

    #[test]
    fn launch_options_roundtrip_import_and_invalid_protection() {
        let dir = test_dir("launch-options"); fs::create_dir_all(&dir).unwrap();
        let mut c = AppConfig::default();
        c.groups[0].buttons[0].window_state = "maximized".into();
        c.groups[0].buttons[0].launch_on_startup = true;
        save_to(&c, &dir).unwrap();
        let loaded = load_or_init_from(&dir); assert!(!loaded.read_only);
        assert!(loaded.config.groups[0].buttons[0].launch_on_startup);
        let imported = import_from(&dir.join(CONFIG_FILE)).unwrap();
        assert!(!imported.groups[0].buttons[0].launch_on_startup);
        assert_eq!(imported.groups[0].buttons[0].window_state, "maximized");
        c.groups[0].buttons[0].window_state = "future-state".into();
        let invalid = serde_json::to_string(&c).unwrap();
        fs::write(dir.join(CONFIG_FILE), &invalid).unwrap();
        assert!(load_or_init_from(&dir).read_only);
        assert_eq!(fs::read_to_string(dir.join(CONFIG_FILE)).unwrap(), invalid);
        assert!(import_from(&dir.join(CONFIG_FILE)).is_err());
        assert_eq!(["normal", "minimized", "maximized", "hidden"].map(window_show_mode), [Some(1),Some(2),Some(3),Some(0)]);
    }

    #[test]
    fn default_roundtrip() {
        // 序列化 → 反序列化必须无损（新增字段都带 serde default，老配置可读）。
        let c = AppConfig::default();
        let text = serde_json::to_string(&c).unwrap();
        let back: AppConfig = serde_json::from_str(&text).unwrap();
        assert_eq!(back.groups.len(), 3);
        assert_eq!(back.groups[0].buttons.len(), 12);
        assert_eq!(back.groups[0].buttons[0].label, "资源管理器");
        // 默认主题是「跟随系统」（三值 light/dark/system），见设计规格 7.13
        assert_eq!(back.theme, "system");
    }

    #[test]
    fn button_crud() {
        let mut c = AppConfig::default();
        let n0 = c.groups[0].buttons.len();
        // 增
        assert!(c.add_button(0, ButtonConfig {
            label: "新按钮".into(),
            target: "notepad.exe".into(),
            params: String::new(),
            work_dir: String::new(),
            icon: "记事本".into(),
            hotkey: String::new(),
            window_state: "normal".into(),
            launch_on_startup: false,
            run_count: 0,
        }));
        assert_eq!(c.groups[0].buttons.len(), n0 + 1);
        // 改
        let mut edited = c.groups[0].buttons[0].clone();
        edited.label = "改名了".into();
        assert!(c.update_button(0, 0, edited));
        assert_eq!(c.groups[0].buttons[0].label, "改名了");
        // 删
        assert!(c.remove_button(0, 0));
        assert_eq!(c.groups[0].buttons.len(), n0);
        // 越界一律 false，不 panic
        assert!(!c.remove_button(0, 9999));
        assert!(!c.update_button(99, 0, c.groups[0].buttons[0].clone()));
        assert!(!c.add_button(99, c.groups[0].buttons[0].clone()));
        // 「恢复默认设置」：设置项回默认（含主题），按钮与群组保留
        c.theme = "dark".into();
        c.click_to_run = false;
        c.history.push(HistoryEntry { window_state: "normal".into(), target: "notepad.exe".into(), label: "记事本".into(), ts: 1, params: String::new(), work_dir: String::new() });
        let n = c.groups[0].buttons.len();
        c.reset_settings();
        assert_eq!(c.theme, "system");
        assert!(c.click_to_run);
        assert_eq!(c.groups.len(), 3);
        assert_eq!(c.groups[0].buttons.len(), n);
        assert_eq!(c.history.len(), 1, "恢复设置不得清空运行历史");
    }

    #[test]
    fn moving_button_uses_insertion_boundary() {
        let mut c = AppConfig::default();
        let original: Vec<_> = c.groups[0].buttons.iter().map(|b| b.label.clone()).collect();
        assert!(c.move_button(0, 0, 3));
        assert_eq!(c.groups[0].buttons[2].label, original[0]);
        assert_eq!(c.groups[0].buttons[0].label, original[1]);
        assert!(c.move_button(0, 2, 0));
        assert_eq!(c.groups[0].buttons.iter().map(|b| b.label.clone()).collect::<Vec<_>>(), original);
        assert!(!c.move_button(0, 0, original.len() + 1));
    }

    #[test]
    fn moving_between_groups_preserves_all_button_fields_and_rejects_bad_indices() {
        let mut c = AppConfig::default();
        let b = &mut c.groups[0].buttons[0];
        b.params = "--project demo".into(); b.work_dir = "C:\\Work".into();
        b.icon = "C:\\icon.png".into(); b.hotkey = "Ctrl+Alt+J".into();
        b.window_state = "maximized".into(); b.launch_on_startup = true; b.run_count = 42;
        let button = b.clone(); let before = c.groups.clone(); let destination_count = c.groups[1].buttons.len();
        assert!(!c.move_button_to_group(0,0,99));
        assert!(!c.move_button_to_group(99,0,1));
        assert!(!c.move_button_to_group(0,999,1));
        assert_eq!(c.groups,before);
        assert!(c.move_button_to_group(0,0,0)); assert_eq!(c.groups,before);
        assert!(c.move_button_to_group(0,0,1));
        assert_eq!(c.groups[1].buttons[destination_count],button);
        assert_eq!(c.groups[0].buttons,&before[0].buttons[1..]);
        assert_eq!(c.groups[1].buttons.len(),destination_count+1);
        let text = serde_json::to_string(&c).unwrap();
        let restored: AppConfig = serde_json::from_str(&text).unwrap();
        assert_eq!(restored.groups,c.groups);
    }

    #[test]
    fn grid_lines_default_off_roundtrip_and_reset_without_changing_groups() {
        let mut config = AppConfig::default();
        assert!(!config.grid_lines);
        let mut old = serde_json::to_value(&config).unwrap();
        old.as_object_mut().unwrap().remove("grid_lines");
        assert!(!serde_json::from_value::<AppConfig>(old).unwrap().grid_lines);
        let dir = test_dir("grid-lines");
        config.grid_lines = true;
        save_to(&config,&dir).unwrap();
        let loaded = load_or_init_from(&dir);
        assert!(!loaded.read_only); assert!(loaded.config.grid_lines);
        let groups = config.groups.clone();
        config.reset_settings();
        assert!(!config.grid_lines); assert_eq!(config.groups,groups);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn moving_groups_preserves_buttons_and_the_config_format() {
        let mut c = AppConfig::default();
        c.add_group("order fixture".into()).unwrap();
        let b = &mut c.groups[0].buttons[0];
        b.hotkey = "Ctrl+Alt+J".into(); b.params = "--demo".into();
        b.work_dir = r"C:\Work".into(); b.icon = "custom.ico".into();
        b.window_state = "maximized".into(); b.launch_on_startup = true; b.run_count = 42;
        let original = c.groups.clone(); let last = c.groups.len() - 1;
        assert!(!c.move_group(0,last+1)); assert!(!c.move_group(last+1,0));
        assert_eq!(c.groups,original);
        assert!(c.move_group(0,0)); assert_eq!(c.groups,original);
        assert!(c.move_group(0,last));
        assert_eq!(c.groups[last],original[0]);
        assert_eq!(&c.groups[..last],&original[1..]);
        assert!(c.move_group(last,0)); assert_eq!(c.groups,original);
        let restored: AppConfig = serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
        assert_eq!(restored.groups,original);
        let mut empty = c.clone(); empty.groups.clear(); assert!(!empty.move_group(0,0));
        c.groups.truncate(1); assert!(c.move_group(0,0)); assert!(!c.move_group(0,1));
    }

    #[test]
    fn search_matches_label_and_target_across_groups() {
        let mut c = AppConfig::default();
        // 工具组（下标 1）放一个只靠 target 命中的按钮：名称里没有 "note"
        c.add_button(1, ButtonConfig {
            label: "备忘录".into(),
            target: "notepad.exe".into(),
            params: String::new(),
            work_dir: String::new(),
            icon: "记事本".into(),
            hotkey: String::new(),
            window_state: "normal".into(),
            launch_on_startup: false,
            run_count: 0,
        });
        // 当前组 = 1（工具）→ 工具组的命中必须排在最前
        let hits = search_buttons(&c, "note", 1);
        assert!(!hits.is_empty());
        assert_eq!(hits[0].0, 1, "当前群组的命中项必须排在最前");
        // label 命中（记事本）+ target 命中（备忘录）都要进结果
        assert!(hits.len() >= 2);
        // 空词 / 纯空白 → 空结果（不是"全部"）
        assert!(search_buttons(&c, "", 0).is_empty());
        assert!(search_buttons(&c, "   ", 0).is_empty());
        // 大小写不敏感
        assert_eq!(search_buttons(&c, "NOTE", 1).len(), hits.len());
        // 中文词匹配
        assert!(!search_buttons(&c, "备忘", 0).is_empty());
        // 无命中
        assert!(search_buttons(&c, "zzzz-不存在", 0).is_empty());
    }

    #[test]
    fn rel_time_buckets() {
        let now = 1_700_000_000; // 任意固定基准，避免依赖当前时钟
        assert_eq!(format_rel_time(now, now, 8 * 3600), "刚刚");
        assert_eq!(format_rel_time(now - 30, now, 8 * 3600), "刚刚");
        assert_eq!(format_rel_time(now - 120, now, 8 * 3600), "2 分钟前");
        assert_eq!(format_rel_time(now - 3600 * 3, now, 8 * 3600), "3 小时前");
        assert_eq!(format_rel_time(now - 86400 * 5, now, 8 * 3600), "5 天前");
        // 「昨天」带本地时分：ts 落在东八区 21:04 → 本地时间 21:04
        // 取一个 UTC 秒数，使其 +8h 后落在 21:04
        let ts = now - 86400;
        let s = format_rel_time(ts, now, 8 * 3600);
        assert!(s.starts_with("昨天 "), "实际：{s}");
        // 时钟回拨（未来时间戳）→ 按「刚刚」，不 panic
        assert_eq!(format_rel_time(now + 99999, now, 0), "刚刚");
    }

    #[test]
    fn legacy_minimal_config_parses() {
        // 只有必要字段的极简配置也要能读（serde default 兜底）。
        let text = r#"{"version":1,"theme":"dark","groups":[{"name":"g","buttons":[{"label":"x","target":"notepad.exe"}]}]}"#;
        let c: AppConfig = serde_json::from_str(text).unwrap();
        assert_eq!(c.theme, "dark");
        assert_eq!(c.groups[0].buttons[0].params, "");
        assert_eq!(c.groups[0].buttons[0].icon, "");
        assert_eq!(c.groups[0].buttons[0].hotkey, "");
        // 老配置没有 auto_start / follow_mouse → serde default 兜底
        assert!(!c.auto_start);
        assert!(c.follow_mouse);
    }

    fn test_dir(name: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        std::env::temp_dir().join(format!("zaplaunch-{name}-{}-{nonce}", std::process::id()))
    }

    #[test]
    fn corrupt_config_is_backed_up_before_rebuild() {
        let dir = test_dir("corrupt");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(CONFIG_FILE), "{bad json").unwrap();
        let loaded = load_or_init_from(&dir);
        assert!(!loaded.read_only);
        assert!(loaded.warning.is_some());
        assert_eq!(fs::read_to_string(dir.join("config.corrupt.bak")).unwrap(), "{bad json");
        let rebuilt: AppConfig = serde_json::from_str(&fs::read_to_string(dir.join(CONFIG_FILE)).unwrap()).unwrap();
        assert_eq!(rebuilt.version, CONFIG_VERSION);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn corrupt_config_recovers_valid_rolling_backup() {
        let dir = test_dir("corrupt-recover");
        fs::create_dir_all(&dir).unwrap();
        let mut expected = AppConfig::default();
        expected.groups[0].buttons[0].label = "从备份恢复".into();
        fs::write(dir.join("config.bak.1"), serde_json::to_string(&expected).unwrap()).unwrap();
        fs::write(dir.join(CONFIG_FILE), "{bad json").unwrap();

        let loaded = load_or_init_from(&dir);
        assert!(!loaded.read_only);
        assert_eq!(loaded.config.groups[0].buttons[0].label, "从备份恢复");
        assert_eq!(fs::read_to_string(dir.join("config.corrupt.bak")).unwrap(), "{bad json");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn future_version_is_read_only_and_untouched() {
        let dir = test_dir("future");
        fs::create_dir_all(&dir).unwrap();
        let mut c = AppConfig::default();
        c.version = CONFIG_VERSION + 1;
        let text = serde_json::to_string(&c).unwrap();
        fs::write(dir.join(CONFIG_FILE), &text).unwrap();
        let loaded = load_or_init_from(&dir);
        assert!(loaded.read_only);
        assert_eq!(fs::read_to_string(dir.join(CONFIG_FILE)).unwrap(), text);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn save_failure_returns_error() {
        let path = test_dir("not-a-directory");
        fs::write(&path, "file").unwrap();
        assert!(save_to(&AppConfig::default(), &path).is_err());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn usage_saves_preserve_edit_backups_and_atomic_validation() {
        let dir=test_dir("usage-backups");
        let mut c=AppConfig::default();
        for i in 0..6 { c.groups[0].name=format!("edit {i}"); save_to(&c,&dir).unwrap(); }
        let backups=(1..=5).map(|i|fs::read(dir.join(format!("config.bak.{i}"))).unwrap()).collect::<Vec<_>>();
        for count in 1..=12 {
            c.groups[0].buttons[0].run_count=count;
            c.history=vec![HistoryEntry {label:"usage".into(),target:"notepad.exe".into(),ts:count as i64,params:String::new(),work_dir:String::new(),window_state:"normal".into()}];
            save_to_with_backup(&c,&dir,false).unwrap();
        }
        let loaded=load_or_init_from(&dir); assert!(!loaded.read_only);
        assert_eq!(loaded.config.groups[0].buttons[0].run_count,12);
        assert_eq!(loaded.config.history[0].ts,12);
        for i in 1..=5 { assert_eq!(fs::read(dir.join(format!("config.bak.{i}"))).unwrap(),backups[i-1]); }
        let good=fs::read(dir.join(CONFIG_FILE)).unwrap();
        c.version=CONFIG_VERSION+1;
        assert!(save_to_with_backup(&c,&dir,false).is_err());
        assert_eq!(fs::read(dir.join(CONFIG_FILE)).unwrap(),good);
        c.version=CONFIG_VERSION; c.groups[0].name="edited again".into(); save_to(&c,&dir).unwrap();
        assert_eq!(fs::read(dir.join("config.bak.1")).unwrap(),good);
        assert_eq!(fs::read(dir.join("config.bak.2")).unwrap(),backups[0]);
        fs::remove_file(dir.join(CONFIG_FILE)).unwrap();
        let recovered=load_or_init_from(&dir);
        assert!(!recovered.read_only); assert_eq!(recovered.config.groups[0].name,"edit 5");
        assert_eq!(recovered.config.groups[0].buttons[0].run_count,12);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn save_rotates_up_to_five_backups() {
        let dir = test_dir("backups");
        fs::create_dir_all(&dir).unwrap();
        let mut c = AppConfig::default();
        for i in 1..=7 {
            c.grid_columns = clamp_grid_columns(6 + (i % 3));
            c.groups[0].buttons[0].label = format!("版本 {i}");
            save_to(&c, &dir).unwrap();
        }
        assert!(!dir.join("config.bak.6").exists());
        for (i, expected) in [6, 8, 7, 6, 8].into_iter().enumerate() {
            let text = fs::read_to_string(dir.join(format!("config.bak.{}", i + 1))).unwrap();
            let backup: AppConfig = serde_json::from_str(&text).unwrap();
            assert_eq!(backup.grid_columns, expected);
            assert_eq!(backup.groups[0].buttons[0].label,format!("版本 {}",6-i));
        }
        assert_eq!(fs::read_dir(&dir).unwrap().count(),6,"only main config and five backups should remain");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn backup_rotation_failure_preserves_committed_config_and_existing_backups() {
        let dir = test_dir("rotation-failure");
        let mut config = AppConfig::default();
        for i in 0..7 {
            config.groups[0].buttons[0].label = format!("版本 {i}");
            save_to(&config,&dir).unwrap();
        }
        let first = fs::read(dir.join("config.bak.1")).unwrap();
        let fourth = fs::read(dir.join("config.bak.4")).unwrap();
        fs::remove_file(dir.join("config.bak.5")).unwrap();
        fs::create_dir(dir.join("config.bak.5")).unwrap();
        config.groups[0].buttons[0].label = "已提交的新版本".into();
        save_to(&config,&dir).unwrap();
        let current: AppConfig = serde_json::from_slice(&fs::read(dir.join(CONFIG_FILE)).unwrap()).unwrap();
        assert_eq!(current.groups[0].buttons[0].label,"已提交的新版本");
        assert!(take_backup_warning());
        assert_eq!(fs::read(dir.join("config.bak.1")).unwrap(),first);
        assert_eq!(fs::read(dir.join("config.bak.4")).unwrap(),fourth);
        assert!(dir.join("config.bak.5").is_dir());
        fs::remove_dir(dir.join("config.bak.5")).unwrap();
        fs::remove_file(dir.join("config.bak.4")).unwrap();
        fs::create_dir(dir.join("config.bak.4")).unwrap();
        save_to(&config,&dir).unwrap();
        assert!(dir.join("config.bak.4").is_dir(),"invalid backup directories must not be moved");
        assert!(!dir.join("config.bak.5").exists());
        assert_eq!(fs::read(dir.join("config.bak.1")).unwrap(),first);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_config_recovers_newest_valid_backup() {
        let dir = test_dir("recover-missing");
        fs::create_dir_all(&dir).unwrap();
        let mut expected = AppConfig::default();
        expected.groups[0].buttons[0].label = "备份中的按钮".into();
        fs::write(
            dir.join("config.bak.2"),
            serde_json::to_string_pretty(&expected).unwrap(),
        )
        .unwrap();
        fs::write(dir.join("config.bak.1"), "{bad json").unwrap();

        let loaded = load_or_init_from(&dir);
        assert!(!loaded.read_only);
        assert_eq!(loaded.config.groups[0].buttons[0].label, "备份中的按钮");
        assert!(loaded.warning.unwrap_or_default().contains("config.bak.2"));
        assert!(dir.join(CONFIG_FILE).exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_config_with_only_invalid_backups_is_read_only() {
        let dir = test_dir("invalid-backups");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("config.bak.1"), "{bad json").unwrap();

        let loaded = load_or_init_from(&dir);
        assert!(loaded.read_only);
        assert!(!dir.join(CONFIG_FILE).exists(), "不得用默认配置覆盖恢复现场");
        assert_eq!(fs::read_to_string(dir.join("config.bak.1")).unwrap(), "{bad json");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn import_rejects_future_version_and_empty_groups() {
        let dir = test_dir("import-validation");
        fs::create_dir_all(&dir).unwrap();
        let mut future = AppConfig::default();
        future.version = CONFIG_VERSION + 1;
        let future_path = dir.join("future.json");
        fs::write(&future_path, serde_json::to_string(&future).unwrap()).unwrap();
        assert!(import_from(&future_path).is_err());

        let mut empty = AppConfig::default();
        empty.groups.clear();
        let empty_path = dir.join("empty.json");
        fs::write(&empty_path, serde_json::to_string(&empty).unwrap()).unwrap();
        assert!(import_from(&empty_path).is_err());
        fs::write(dir.join(CONFIG_FILE), serde_json::to_string(&empty).unwrap()).unwrap();
        assert!(load_or_init_from(&dir).read_only);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn export_overwrites_existing_file() {
        let dir = test_dir("export-overwrite");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("export.json");
        fs::write(&path, "old").unwrap();
        export_to(&AppConfig::default(), &path).unwrap();
        let exported: AppConfig = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(exported.version, CONFIG_VERSION);
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn failed_main_replace_preserves_backup_history() {
        use std::os::windows::fs::OpenOptionsExt;

        let dir = test_dir("replace-failure-backups");
        fs::create_dir_all(&dir).unwrap();
        let mut config = AppConfig::default();
        save_to(&config, &dir).unwrap();
        config.grid_columns = 8;
        save_to(&config, &dir).unwrap();
        let backup_before = fs::read(dir.join("config.bak.1")).unwrap();
        let main_before = fs::read(dir.join(CONFIG_FILE)).unwrap();

        // 允许读写备份快照，但拒绝删除/替换主文件。
        let lock = fs::OpenOptions::new().read(true).write(true).share_mode(3)
            .open(dir.join(CONFIG_FILE)).unwrap();
        config.grid_columns = 6;
        assert!(save_to(&config, &dir).is_err());
        drop(lock);
        assert_eq!(fs::read(dir.join("config.bak.1")).unwrap(), backup_before);
        assert_eq!(fs::read(dir.join(CONFIG_FILE)).unwrap(), main_before);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn repeated_corrupt_configs_keep_distinct_backups() {
        let dir = test_dir("repeated-corrupt");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(CONFIG_FILE), "{first").unwrap();
        assert!(!load_or_init_from(&dir).read_only);
        fs::write(dir.join(CONFIG_FILE), "{second").unwrap();
        assert!(!load_or_init_from(&dir).read_only);
        assert_eq!(fs::read_to_string(dir.join("config.corrupt.bak")).unwrap(), "{first");
        assert_eq!(fs::read_to_string(dir.join("config.corrupt.1.bak")).unwrap(), "{second");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn pinyin_handles_common_polyphonic_phrases() {
        assert_eq!(pinyin_initials("重量"), "zl");
        assert_eq!(pinyin_initials("旅行"), "lx");
        assert_eq!(pinyin_initials("调动"), "dd");
        assert_eq!(pinyin_initials("重庆银行"), "cqyh");
    }

    #[test]
    fn search_matches_pinyin_initials_and_shortcut_target() {
        let mut c = AppConfig::default();
        c.add_button(
            0,
            ButtonConfig {
                label: "微信".into(),
                target: r"C:\Users\Public\Desktop\微信.lnk".into(),
                params: String::new(),
                work_dir: String::new(),
                icon: String::new(),
                hotkey: String::new(),
                window_state: "normal".into(),
                launch_on_startup: false,
                run_count: 0,
            },
        );
        // 拼音首字母 "wx" 命中 "微信"
        let hits_wx = search_buttons(&c, "wx", 0);
        assert!(hits_wx.iter().any(|&(g, b)| c.groups[g].buttons[b].label == "微信"));
        // 全拼 "weixin" 与英文名 "wechat" 命中 "微信"
        let hits_weixin = search_buttons(&c, "weixin", 0);
        assert!(hits_weixin.iter().any(|&(g, b)| c.groups[g].buttons[b].label == "微信"));
        // .lnk 真实目标解析器命中自定义的 exe 名
        let hits_real = search_buttons_smart(&c, "mycustomapp", 0, |btn| {
            if btn.target.ends_with(".lnk") {
                Some(r"D:\Apps\MyCustomApp\MyCustomApp.exe".into())
            } else {
                None
            }
        });
        assert!(hits_real.iter().any(|&(g, b)| c.groups[g].buttons[b].label == "微信"));

        // 别名必须按空白分词后做前缀匹配，不能把别名串任意位置的单字母当成命中。
        let mut only_wechat = AppConfig::default();
        for group in &mut only_wechat.groups {
            group.buttons.clear();
        }
        only_wechat.groups[0].buttons.push(ButtonConfig {
            label: "微信".into(),
            target: "微信.lnk".into(),
            params: String::new(),
            work_dir: String::new(),
            icon: String::new(),
            hotkey: String::new(),
            window_state: "normal".into(),
            launch_on_startup: false,
            run_count: 0,
        });
        assert!(search_buttons(&only_wechat, "e", 0).is_empty());
        assert!(search_buttons(&only_wechat, "wech", 0).len() == 1);
        assert!(search_buttons(&only_wechat, "echat", 0).is_empty());
    }
}
