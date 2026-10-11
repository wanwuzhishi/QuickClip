//! QuickClip 库入口。
//!
//! 模块分层：
//! - `config`    ：配置模型与过滤规则
//! - `db`        ：SQLite 持久化 + 内存缓存
//! - `clipboard` ：剪贴板采集服务
//! - `platform`  ：Windows 原生能力（仅 Windows 编译）
//! - `util`      ：通用工具

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

#[cfg(not(windows))]
pub mod platform {
    //! 非 Windows 平台的空实现，保证代码可跨平台编译（本项目仅发布 Windows 版）。
    pub fn clipboard_sequence_number() -> u32 {
        0
    }
    pub fn send_paste() {}
    pub fn send_key(_vk: u16) {}
    pub fn current_pid() -> u32 {
        0
    }
}

#[cfg(not(windows))]
pub mod mouse_hook {
    //! 非 Windows 平台的空实现。
    pub fn ensure_installed(_panel_hwnd: isize) {}
    pub fn set_active(_active: bool) {}
    pub fn take_hide_request() -> bool {
        false
    }
}
