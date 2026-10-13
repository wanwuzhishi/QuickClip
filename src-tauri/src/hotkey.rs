//! 全局快捷键字符串解析。
//!
//! 抽出为独立模块的原因：`main.rs` 的启动注册与 `commands.rs` 的运行时改键
//! 都需要解析同一格式，两处必须使用完全一致的规则，否则会出现
//! "注册用的是 A，比对的却是 B" 这类逻辑错位。

use tauri_plugin_global_shortcut::{Code, Modifiers, Shortcut};

/// 解析 "Alt+Space"、"Ctrl+Shift+V" 这类加速键字符串。
/// 返回 None 表示格式无法识别。
///
/// **零分配实现**：直接在 `&str` 上迭代，不做 `to_lowercase` 的字符串分配。
/// 该函数在全局快捷键的 `with_handler` 里**每次按键都会执行一次**
/// （用来判断按下的是不是用户设的那个组合），属于热路径。
/// 大小写不敏感通过 `eq_ignore_ascii_case` 实现，语义与原先的
/// "统一转小写后比较" 完全一致。
pub fn parse(s: &str) -> Option<Shortcut> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }

    let mut mods = Modifiers::empty();
    let mut code: Option<Code> = None;

    for part in s.split('+') {
        let part = part.trim();
        if part.eq_ignore_ascii_case("ctrl") || part.eq_ignore_ascii_case("control") {
            mods |= Modifiers::CONTROL;
        } else if part.eq_ignore_ascii_case("alt") {
            mods |= Modifiers::ALT;
        } else if part.eq_ignore_ascii_case("shift") {
            mods |= Modifiers::SHIFT;
        } else if part.eq_ignore_ascii_case("super")
            || part.eq_ignore_ascii_case("win")
            || part.eq_ignore_ascii_case("meta")
            || part.eq_ignore_ascii_case("cmd")
        {
            mods |= Modifiers::SUPER;
        } else if let Some(c) = parse_key(part) {
            code = Some(c);
        }
        // 无法识别的片段直接忽略：与旧实现一致（旧实现里解析不出就跳过）
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

    // 具名主键。比较必须大小写不敏感：`parse` 现在不再预转小写，
    // 直接透传用户写的原始片段（"Space" / "space" / "SPACE" 都应识别）。
    // 若这里用小写面值匹配，"Alt+Space" 会解析失败。
    if k.eq_ignore_ascii_case("space") {
        return Some(Code::Space);
    }
    if k.eq_ignore_ascii_case("enter") || k.eq_ignore_ascii_case("return") {
        return Some(Code::Enter);
    }
    if k.eq_ignore_ascii_case("tab") {
        return Some(Code::Tab);
    }
    if k.eq_ignore_ascii_case("backquote") || k.eq_ignore_ascii_case("grave") {
        return Some(Code::Backquote);
    }
    if k.eq_ignore_ascii_case("escape") || k.eq_ignore_ascii_case("esc") {
        return Some(Code::Escape);
    }
    if k.eq_ignore_ascii_case("up") {
        return Some(Code::ArrowUp);
    }
    if k.eq_ignore_ascii_case("down") {
        return Some(Code::ArrowDown);
    }
    if k.eq_ignore_ascii_case("left") {
        return Some(Code::ArrowLeft);
    }
    if k.eq_ignore_ascii_case("right") {
        return Some(Code::ArrowRight);
    }

    // F1-F12。大小写不敏感地剥掉前缀，再解析数字。
    let stripped = if k.len() >= 2 && k.as_bytes()[0].eq_ignore_ascii_case(&b'f') {
        &k[1..]
    } else {
        return None;
    };
    stripped.parse::<u8>().ok().and_then(|n| match n {
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
}
