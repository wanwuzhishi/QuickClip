//! QuickClip 库入口。
//!
//! 模块分层：
//! - `config`      ：配置模型与过滤规则
//! - `db`          ：SQLite 持久化 + 内存缓存
//! - `clipboard`   ：剪贴板采集服务
//! - `hotkey`      ：全局快捷键字符串解析
//! - `platform`    ：Windows 原生能力（仅 Windows 编译）
//! - `mouse_hook`  ：全局鼠标钩子，路线 B 的配套（仅 Windows 编译）
//! - `util`        ：通用工具

pub mod clipboard;
pub mod commands;
pub mod config;
pub mod db;
pub mod hotkey;
pub mod util;

#[cfg(windows)]
pub mod platform;

#[cfg(windows)]
pub mod mouse_hook;

/// 非 Windows 平台的空实现。
///
/// 本项目只发布 Windows 版，这些桩子仅用于让 `cargo check` 在其它平台上
/// 也能通过（便于 CI 或贡献者在 macOS/Linux 上做类型检查）。
/// 只提供 `commands.rs` 中非 Windows 分支实际引用的那几个函数，
/// 避免出现"从未被调用"的死代码。
#[cfg(not(windows))]
pub mod platform {
    pub fn clipboard_sequence_number() -> u32 {
        0
    }
    pub fn send_paste() {}
    pub fn current_pid() -> u32 {
        0
    }
}

#[cfg(not(windows))]
pub mod mouse_hook {
    pub fn ensure_installed(_panel_hwnd: isize) {}
    pub fn set_active(_active: bool) {}
    pub fn take_hide_request() -> bool {
        false
    }
}
