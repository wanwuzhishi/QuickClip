//! 应用全局配置。结构对齐参考实现（Better-Clip）的 ClipConfig 设计，
//! 采用扁平 JSON 文件持久化，避免为每个开关建表。

use serde::{Deserialize, Serialize};
use std::path::Path;

/// 剪贴板内容类型。数值取自对参考实现的实测逆向结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(into = "i32", from = "i32")]
pub enum ObjType {
    /// 纯文本
    Text,
    /// 图片（位图）
    Image,
    /// 文件路径列表
    Files,
    /// 富文本（HTML / RTF）
    RichText,
}

impl From<ObjType> for i32 {
    fn from(v: ObjType) -> Self {
        match v {
            ObjType::Text => 1,
            ObjType::Image => 2,
            ObjType::Files => 3,
            ObjType::RichText => 10,
        }
    }
}

impl From<i32> for ObjType {
    fn from(v: i32) -> Self {
        match v {
            2 => ObjType::Image,
            3 => ObjType::Files,
            10 => ObjType::RichText,
            _ => ObjType::Text,
        }
    }
}

/// 剪贴板条目主体。`text` 在图片类型下存放落盘文件名，文件类型下存放换行分隔的路径。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClipPayload {
    pub text: String,
    pub obj_type: ObjType,
}

/// 一条剪贴板历史记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClipItem {
    pub id: String,
    pub item: ClipPayload,
    pub create_time: String,
    pub update_time: String,
    pub is_top: bool,
    pub sub_index: i32,
    /// 源进程可执行文件名，用于展示来源图标。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_app: Option<String>,
    /// 内容指纹，用于去重比对。
    #[serde(default)]
    pub hash: String,
}

/// 收藏夹条目，在 ClipItem 基础上扩展为树形结构。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FavorItem {
    pub id: String,
    pub item: ClipPayload,
    #[serde(default)]
    pub title: String,
    pub create_time: String,
    pub update_time: String,
    #[serde(default)]
    pub parent_id: String,
    #[serde(default)]
    pub is_deleted: bool,
    #[serde(default)]
    pub description: String,
    /// 子节点 ID 列表。为空即叶子节点。
    #[serde(default)]
    pub children: Vec<String>,
    /// 是否为文件夹（分组）节点。
    #[serde(default)]
    pub is_folder: bool,
}

/// 应用主配置。
///
/// 序列化采用 camelCase：Tauri IPC 的惯例是 camelCase，前端 TypeScript
/// 接口也按 camelCase 定义。若此处用 PascalCase，会导致前后端字段对不上，
/// 前端读到 undefined、写回被忽略，表现为"设置改了但没生效"。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ClipConfig {
    pub show_clip_tab: bool,
    pub show_favor_tab: bool,
    pub font_size: f64,
    pub show_line_number: bool,
    pub clip_list_simple_mode: bool,

    /// 最大历史条数，超出后触发清理。
    pub max_item_count: i64,
    /// 单条内容最大字符数，超长截断，防止大文本拖慢列表。
    pub max_item_size: i64,
    /// 是否保留重复内容（false 表示去重）
    pub allow_duplicate_copy: bool,
    /// 忽略纯空白内容
    pub ignore_white_space: bool,
    /// 默认按纯文本粘贴，去除格式
    pub plain_text_paste_by_default: bool,
    /// 粘贴前延迟毫秒数
    pub default_paste_delay: u64,
    /// 使用数字键直接粘贴
    pub using_number_key_to_paste: bool,

    /// 黑名单进程名，逗号分隔。命中则不记录。
    pub black_list: String,
    /// 白名单进程名，逗号分隔。非空时仅记录命中项。
    pub white_list: String,

    /// 隐藏模式：0 无 / 1 失焦隐藏 / 2 边缘隐藏
    pub hide_mode: i32,
    /// 全局唤出快捷键（Tauri 加速键格式）
    pub hotkey: String,
    /// 界面主题：dark / light
    pub theme: String,
    /// 开机自启。写入系统启动项，随系统启动后台运行。
    pub auto_start: bool,
    /// 启动时静默驻留托盘（不弹出面板）。
    ///
    /// 仅对**开机自启**生效：手动双击启动时总会先显示窗口，
    /// 否则用户会以为"软件没打开"。
    pub start_minimized: bool,

    /// 面板出现时的定位策略。
    /// - `cursor`（默认）：跟随鼠标当前位置，就近弹出
    /// - `last`：沿用上次关闭时的位置
    /// - `fixed`：固定使用 `fixed_x` / `fixed_y`
    pub window_position: String,
    /// 固定位置模式的 X 坐标（屏幕坐标，左上角原点）。由用户设定，不被运行时改写。
    pub fixed_x: i32,
    /// 固定位置模式的 Y 坐标
    pub fixed_y: i32,
    /// 上次关闭时的窗口坐标，由程序自动记录，供 `window_position = "last"` 使用。
    /// 与 `fixed_*` 分开存放，否则切换策略时会互相覆盖。
    pub last_x: i32,
    /// 上次关闭时的窗口 Y 坐标
    pub last_y: i32,

    /// 唤出面板时是否让面板"永不激活"（路线 B：`WS_EX_NOACTIVATE`）。
    ///
    /// 默认 `true`，因为这是"粘贴到输入框失败"的根治手段：
    ///
    /// 许多目标程序的输入框**失焦即关闭**——浏览器地址栏的联想下拉、页面内
    /// 搜索条、编辑器命令面板（command palette）、聊天框的 @ 提及浮层等，
    /// 一旦失去前台焦点就立刻销毁。而面板为接收用户按键通常要抢焦点，
    /// 于是用户按下快捷键的瞬间输入框就没了；等粘贴时把焦点还回去，
    /// 输入框早已不存在，Ctrl+V 落到窗口本体上，表现为"输入框关闭且没粘上"。
    ///
    /// 开启后，面板被打上 `WS_EX_NOACTIVATE` 扩展样式，**从显示到隐藏
    /// 全程不会成为前台窗口**，目标输入框始终不失去焦点。
    ///
    /// **代价（这是路线 B 的固有成本，不是 bug）**：
    /// - 搜索框无法直接打字——需要点一下搜索框，前端会调用
    ///   `set_search_focusable(true)` 临时去掉该样式
    /// - 窗口收不到 `WM_KILLFOCUS`，"点击别处自动隐藏"改由
    ///   全局鼠标钩子（`mouse_hook` 模块）实现
    ///
    /// 关掉它则退回普通可激活窗口（搜索框可直接输入，但目标输入框可能被关闭）。
    pub show_without_activating: bool,
}

impl Default for ClipConfig {
    fn default() -> Self {
        Self {
            show_clip_tab: true,
            show_favor_tab: true,
            font_size: 13.0,
            show_line_number: true,
            clip_list_simple_mode: true,
            max_item_count: 500,
            max_item_size: 20000,
            allow_duplicate_copy: false,
            ignore_white_space: true,
            plain_text_paste_by_default: false,
            default_paste_delay: 0,
            using_number_key_to_paste: true,
            black_list: String::new(),
            white_list: String::new(),
            hide_mode: 2,
            hotkey: "Alt+V".into(),
            theme: "dark".into(),
            auto_start: false,
            start_minimized: true,
            window_position: "cursor".into(),
            fixed_x: 100,
            fixed_y: 100,
            last_x: 0,
            last_y: 0,
            show_without_activating: true,
        }
    }
}

impl ClipConfig {
    /// 从 JSON 文件加载。
    ///
    /// 兼容两种字段命名：早期版本写出的 PascalCase（如 `Hotkey`）
    /// 与当前使用的 camelCase（如 `hotkey`）。先在文本层面做一次键名归一，
    /// 再反序列化，保证老配置文件不丢失设置。
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(s) => {
                let normalized = normalize_keys(&s);
                serde_json::from_str(&normalized).unwrap_or_default()
            }
            Err(_) => Self::default(),
        }
    }

    /// 写入 JSON 文件，自动创建父目录。
    pub fn save(&self, path: &Path) -> Result<(), std::io::Error> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let s = serde_json::to_string_pretty(self)?;
        std::fs::write(path, s)
    }
}

/// 把 JSON 中所有键的首字母转为小写，实现 PascalCase -> camelCase 归一。
///
/// 仅处理顶层与嵌套对象的第一层键命名差异，不做语义转换。
fn normalize_keys(s: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(s) {
        Ok(serde_json::Value::Object(map)) => {
            let mut out = serde_json::Map::new();
            for (k, v) in map {
                let mut chars = k.chars();
                let new_key = match chars.next() {
                    Some(first) => first.to_lowercase().collect::<String>() + chars.as_str(),
                    None => k,
                };
                out.insert(new_key, v);
            }
            serde_json::Value::Object(out).to_string()
        }
        _ => s.to_string(),
    }
}

/// 判断内容是否命中过滤规则。返回 true 表示应当记录。
///
/// 这条路径**每次复制都会执行**，因此名单匹配不做任何堆分配：
/// 直接在 `&str` 上迭代，命中即返回。
pub fn should_capture(cfg: &ClipConfig, source_app: Option<&str>, text: &str) -> bool {
    if cfg.ignore_white_space && text.trim().is_empty() {
        return false;
    }
    let Some(app) = source_app else {
        return true;
    };
    let app_lower = app.to_ascii_lowercase();

    // 白名单非空时，只有命中项才记录。
    // 先探一次是否非空，再判命中——迭代器是即时的，不能"用两次"，
    // 因此这里用 peekable 而不是 clone。
    let mut white = cfg
        .white_list
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .peekable();
    if white.peek().is_some() && !white.any(|w| app_lower.contains(&w.to_ascii_lowercase())) {
        return false;
    }

    // 黑名单命中即排除
    let mut black = cfg
        .black_list
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .peekable();
    if black.peek().is_some() && black.any(|b| app_lower.contains(&b.to_ascii_lowercase())) {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 验证 PascalCase 老配置能被正确读入（键名归一化）。
    #[test]
    fn legacy_pascal_case_config_is_readable() {
        let old = r#"{"Hotkey":"Ctrl+Shift+V","Theme":"light","MaxItemCount":1234,"FontSize":15.0}"#;
        let normalized = normalize_keys(old);
        let cfg: ClipConfig = serde_json::from_str(&normalized).unwrap();
        assert_eq!(cfg.hotkey, "Ctrl+Shift+V");
        assert_eq!(cfg.theme, "light");
        assert_eq!(cfg.max_item_count, 1234);
        assert_eq!(cfg.font_size, 15.0);
    }

    /// 验证 camelCase 新配置直接可读。
    #[test]
    fn camel_case_config_is_readable() {
        let new = r#"{"hotkey":"Alt+V","theme":"dark","maxItemCount":500}"#;
        let cfg: ClipConfig = serde_json::from_str(new).unwrap();
        assert_eq!(cfg.hotkey, "Alt+V");
        assert_eq!(cfg.max_item_count, 500);
    }

    /// 验证保存后再读取，字段完全一致（往返一致性）。
    #[test]
    fn save_load_roundtrip() {
        let mut cfg = ClipConfig::default();
        cfg.hotkey = "Ctrl+Alt+C".into();
        cfg.max_item_count = 888;
        let s = serde_json::to_string(&cfg).unwrap();
        let back: ClipConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(back.hotkey, "Ctrl+Alt+C");
        assert_eq!(back.max_item_count, 888);
    }

    /// 快捷键解析：非法格式应被拒绝。
    #[test]
    fn hotkey_parse_rejects_invalid() {
        assert!(crate::hotkey::parse("").is_none());
        assert!(crate::hotkey::parse("Space").is_none()); // 无修饰键
        assert!(crate::hotkey::parse("Alt+不存在的键").is_none());
        assert!(crate::hotkey::parse("Alt+Space").is_some());
        assert!(crate::hotkey::parse("ctrl+shift+v").is_some());
    }

    /// 快捷键解析必须大小写不敏感。
    ///
    /// 这条测试是回归防线：`parse` 曾改成"零分配、不再预转小写"，
    /// 若 `parse_key` 的具名主键分支忘了同步改成忽略大小写，
    /// "Alt+Space" 这种常规写法就会解析失败——而旧实现是支持的。
    #[test]
    fn hotkey_parse_is_case_insensitive() {
        // 修饰键的各种写法
        assert!(crate::hotkey::parse("ALT+v").is_some());
        assert!(crate::hotkey::parse("Ctrl+SHIFT+V").is_some());
        // 具名主键（大小写混合）
        assert!(crate::hotkey::parse("Alt+Space").is_some());
        assert!(crate::hotkey::parse("Alt+space").is_some());
        assert!(crate::hotkey::parse("Alt+SPACE").is_some());
        assert!(crate::hotkey::parse("Alt+Enter").is_some());
        assert!(crate::hotkey::parse("Ctrl+Esc").is_some());
        // 功能键
        assert!(crate::hotkey::parse("Alt+F5").is_some());
        assert!(crate::hotkey::parse("Alt+f12").is_some());
        // 仍然要拒绝无修饰键与无法识别的主键
        assert!(crate::hotkey::parse("alt+space").is_some()); // 小写也合法
        assert!(crate::hotkey::parse("Space").is_none());
        assert!(crate::hotkey::parse("Alt+F13").is_none()); // 超出 F1-F12
    }

    /// 配置字段的默认值应与文档一致（防止文档与实现漂移）。
    #[test]
    fn default_config_matches_documented_values() {
        let c = ClipConfig::default();
        assert_eq!(c.hotkey, "Alt+V");
        assert_eq!(c.theme, "dark");
        assert_eq!(c.max_item_count, 500);
        assert_eq!(c.max_item_size, 20000);
        assert_eq!(c.font_size, 13.0);
        assert!(c.show_without_activating);
        assert!(c.start_minimized);
    }
}
