//! 全局快捷键字符串解析。
//!
//! 抽出为独立模块的原因：`main.rs` 的启动注册与 `commands.rs` 的运行时改键
//! 都需要解析同一格式，两处必须使用完全一致的规则，否则会出现
//! "注册用的是 A，比对的却是 B" 这类逻辑错位。

use tauri_plugin_global_shortcut::{Code, Modifiers, Shortcut};

/// 解析 "Alt+Space"、"Ctrl+Shift+V" 这类加速键字符串。
/// 返回 None 表示格式无法识别。
pub fn parse(s: &str) -> Option<Shortcut> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }

    let parts: Vec<String> = s.split('+').map(|p| p.trim().to_lowercase()).collect();
    let mut mods = Modifiers::empty();
    let mut code: Option<Code> = None;

    for p in parts.iter() {
        match p.as_str() {
            "ctrl" | "control" => mods |= Modifiers::CONTROL,
            "alt" => mods |= Modifiers::ALT,
            "shift" => mods |= Modifiers::SHIFT,
            "super" | "win" | "meta" | "cmd" => mods |= Modifiers::SUPER,
            other => {
                if let Some(c) = parse_key(other) {
                    code = Some(c);
                }
            }
        }
    }

    // 快捷键必须包含修饰键，否则会劫持普通按键输入
    if code.is_none() || mods.is_empty() {
        return None;
    }
    code.map(|c| Shortcut::new(Some(mods), c))
}

/// 解析主键部分。
fn parse_key(k: &str) -> Option<Code> {
    if k.len() == 1 {
        let ch = k.chars().next()?.to_ascii_uppercase();
        return match ch {
            'A' => Some(Code::KeyA),
            'B' => Some(Code::KeyB),
            'C' => Some(Code::KeyC),
            'D' => Some(Code::KeyD),
            'E' => Some(Code::KeyE),
            'F' => Some(Code::KeyF),
            'G' => Some(Code::KeyG),
            'H' => Some(Code::KeyH),
            'I' => Some(Code::KeyI),
            'J' => Some(Code::KeyJ),
            'K' => Some(Code::KeyK),
            'L' => Some(Code::KeyL),
            'M' => Some(Code::KeyM),
            'N' => Some(Code::KeyN),
            'O' => Some(Code::KeyO),
            'P' => Some(Code::KeyP),
            'Q' => Some(Code::KeyQ),
            'R' => Some(Code::KeyR),
            'S' => Some(Code::KeyS),
            'T' => Some(Code::KeyT),
            'U' => Some(Code::KeyU),
            'V' => Some(Code::KeyV),
            'W' => Some(Code::KeyW),
            'X' => Some(Code::KeyX),
            'Y' => Some(Code::KeyY),
            'Z' => Some(Code::KeyZ),
            '0' => Some(Code::Digit0),
            '1' => Some(Code::Digit1),
            '2' => Some(Code::Digit2),
            '3' => Some(Code::Digit3),
            '4' => Some(Code::Digit4),
            '5' => Some(Code::Digit5),
            '6' => Some(Code::Digit6),
            '7' => Some(Code::Digit7),
            '8' => Some(Code::Digit8),
            '9' => Some(Code::Digit9),
            '`' => Some(Code::Backquote),
            '-' => Some(Code::Minus),
            '=' => Some(Code::Equal),
            '[' => Some(Code::BracketLeft),
            ']' => Some(Code::BracketRight),
            '\\' => Some(Code::Backslash),
            ';' => Some(Code::Semicolon),
            '\'' => Some(Code::Quote),
            ',' => Some(Code::Comma),
            '.' => Some(Code::Period),
            '/' => Some(Code::Slash),
            _ => None,
        };
    }

    match k {
        "space" => Some(Code::Space),
        "enter" | "return" => Some(Code::Enter),
        "tab" => Some(Code::Tab),
        "backquote" | "grave" => Some(Code::Backquote),
        "escape" | "esc" => Some(Code::Escape),
        "up" => Some(Code::ArrowUp),
        "down" => Some(Code::ArrowDown),
        "left" => Some(Code::ArrowLeft),
        "right" => Some(Code::ArrowRight),
        other => other.strip_prefix('f').and_then(|n| {
            n.parse::<u8>().ok().and_then(|n| match n {
                1 => Some(Code::F1),
                2 => Some(Code::F2),
                3 => Some(Code::F3),
                4 => Some(Code::F4),
                5 => Some(Code::F5),
                6 => Some(Code::F6),
                7 => Some(Code::F7),
                8 => Some(Code::F8),
                9 => Some(Code::F9),
                10 => Some(Code::F10),
                11 => Some(Code::F11),
                12 => Some(Code::F12),
                _ => None,
            })
        }),
    }
}
