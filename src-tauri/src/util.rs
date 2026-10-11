//! 通用工具函数。

use sha2::{Digest, Sha256};

/// 生成短 ID（12 位 Base62），对齐参考实现的 ID 长度风格。
pub fn short_id() -> String {
    const CHARS: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let uid = uuid::Uuid::new_v4();
    let bytes = uid.as_bytes();
    let mut out = String::with_capacity(12);
    for i in 0..12 {
        out.push(CHARS[(bytes[i] as usize) % CHARS.len()] as char);
    }
    out
}

/// 当前时间，ISO 8601 带本地时区偏移。
pub fn now_iso() -> String {
    chrono::Local::now().to_rfc3339()
}

/// 内容指纹。用于去重比对，避免逐字符比较大文本。
pub fn content_hash(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    let d = h.finalize();
    format!("{:x}", d)[..16].to_string()
}
