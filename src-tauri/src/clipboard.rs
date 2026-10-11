//! 剪贴板采集服务。
//!
//! 采用轮询剪贴板序列号的方式判断内容变化，而非挂钩子（hook）。
//! 理由：钩子方案需要注入其他进程，易被杀软拦截且稳定性差；
//! 序列号轮询为进程外读取，兼容性与安全性都更优，200ms 间隔的 CPU 开销可忽略。

use crate::config::{ClipItem, ClipConfig, ObjType};
use crate::db::Store;
use crate::util::{content_hash, now_iso, short_id};
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// 剪贴板服务运行状态。原子量避免加锁，轮询线程可无阻塞读取。
pub struct ClipService {
    pub running: Arc<AtomicBool>,
    /// 上一次处理的剪贴板序列号
    pub last_seq: Arc<AtomicU32>,
    /// 应用自身写入剪贴板时置位，用于跳过自触发
    pub suppress: Arc<AtomicBool>,
    /// 最近一次前台窗口进程名（非本进程），用于标记来源
    pub last_source: Arc<Mutex<Option<String>>>,
}

impl ClipService {
    pub fn new() -> Self {
        Self {
            running: Arc::new(AtomicBool::new(true)),
            last_seq: Arc::new(AtomicU32::new(0)),
            suppress: Arc::new(AtomicBool::new(false)),
            last_source: Arc::new(Mutex::new(None)),
        }
    }
}

/// 轮询主循环。由独立线程驱动，每 tick 检查一次剪贴板序列号。
pub fn spawn_watcher(
    svc: Arc<ClipService>,
    store: Arc<Store>,
    cfg: Arc<Mutex<ClipConfig>>,
    on_change: Arc<dyn Fn(&ClipItem) + Send + Sync>,
) {
    std::thread::spawn(move || {
        // 初始化基线序列号，避免把启动前的旧内容当作新内容采集
        let mut seq = crate::platform::clipboard_sequence_number();
        svc.last_seq.store(seq, Ordering::SeqCst);

        while svc.running.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(200));

            let cur = crate::platform::clipboard_sequence_number();
            if cur == seq {
                continue;
            }
            seq = cur;
            svc.last_seq.store(cur, Ordering::SeqCst);

            // 应用自身写回剪贴板导致的变更，跳过
            if svc.suppress.swap(false, Ordering::SeqCst) {
                continue;
            }

            let cfg_snapshot = cfg.lock().clone();
            let source = current_source_app();
            if let Some(item) = capture(&cfg_snapshot, source.as_deref(), &store) {
                let _ = store.upsert_clip(&item);
                {
                    let mut c = store.cache.write();
                    c.clips.retain(|x| x.id != item.id);
                    c.clips.insert(0, item.clone());
                }
                on_change(&item);
            }
        }
    });
}

/// 判断当前前台窗口是否属于需要记录的应用（排除本程序自身与桌面）。
fn current_source_app() -> Option<String> {
    let hwnd = crate::platform::foreground_window();
    let name = crate::platform::process_name_of_window(hwnd)?;
    let me = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|s| s.to_string_lossy().to_lowercase()))
        .unwrap_or_default();
    if name.to_lowercase() == me {
        return None;
    }
    Some(name)
}

/// 读取当前剪贴板并构造条目。按 文件 → 图片 → 富文本 → 文本 的优先级探测，
/// 与 Windows 剪贴板的格式优先级一致。
pub fn capture(cfg: &ClipConfig, source: Option<&str>, store: &Store) -> Option<ClipItem> {
    let (text, obj_type) = read_clipboard_payload(cfg, store)?;

    if !crate::config::should_capture(cfg, source, &text) {
        return None;
    }

    let hash = content_hash(&text);

    // 去重：命中已有条目的指纹时，仅提升其时间戳到最前，不再新增
    if !cfg.allow_duplicate_copy {
        let c = store.cache.read();
        if let Some(existing) = c.clips.iter().find(|x| x.hash == hash) {
            let mut updated = existing.clone();
            updated.update_time = now_iso();
            drop(c);
            let _ = store.upsert_clip(&updated);
            let mut cw = store.cache.write();
            cw.clips.retain(|x| x.id != updated.id);
            cw.clips.insert(0, updated.clone());
            return None;
        }
    }

    let id = short_id();
    Some(ClipItem {
        id,
        item: crate::config::ClipPayload { text, obj_type },
        create_time: now_iso(),
        update_time: now_iso(),
        is_top: false,
        sub_index: 0,
        source_app: source.map(|s| s.to_string()),
        hash,
    })
}

/// 探测并读取剪贴板内容，返回 (存储文本, 类型)。
fn read_clipboard_payload(cfg: &ClipConfig, store: &Store) -> Option<(String, ObjType)> {
    let mut cb = arboard::Clipboard::new().ok()?;

    // 1) 文件列表（资源管理器复制文件时优先）
    if let Ok(files) = cb.get().file_list() {
        if !files.is_empty() {
            let joined = files
                .iter()
                .map(|p| p.to_string_lossy().to_string())
                .collect::<Vec<_>>()
                .join("\n");
            return Some((truncate(&joined, cfg.max_item_size), ObjType::Files));
        }
    }

    // 2) 图片
    if let Ok(img) = cb.get().image() {
        if img.width > 0 && img.height > 0 {
            if let Some(name) = save_image(&img, store) {
                return Some((name, ObjType::Image));
            }
        }
    }

    // 3) 文本（含富文本标记）
    if let Ok(text) = cb.get().text() {
        let t = text.trim_end_matches('\u{0}').to_string();
        if !t.is_empty() {
            let kind = if looks_like_html(&t) || looks_like_rtf(&t) {
                ObjType::RichText
            } else {
                ObjType::Text
            };
            return Some((truncate(&t, cfg.max_item_size), kind));
        }
    }

    None
}

/// 按配置上限截断超长内容，附带省略提示。
fn truncate(s: &str, max: i64) -> String {
    if max <= 0 {
        return s.to_string();
    }
    if s.chars().count() <= max as usize {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max as usize).collect();
    out.push_str("\n…(内容过长已截断)");
    out
}

fn looks_like_html(s: &str) -> bool {
    let head = s.trim_start();
    head.starts_with("<!DOCTYPE") || head.starts_with("<html") || head.starts_with("<!--StartFragment")
}

fn looks_like_rtf(s: &str) -> bool {
    s.trim_start().starts_with("{\\rtf")
}

/// 将图片落盘到 Image/ 目录，返回相对文件名。
fn save_image(img: &arboard::ImageData, store: &Store) -> Option<String> {
    let dir = store
        .db_path
        .parent()
        .map(|p| p.join("Image"))
        .unwrap_or_else(|| std::path::PathBuf::from("Image"));
    std::fs::create_dir_all(&dir).ok()?;

    let buf: image::RgbaImage =
        image::ImageBuffer::from_raw(img.width as u32, img.height as u32, img.bytes.to_vec())?;

    let stamp = chrono::Local::now().format("%Y%m%d_%H%M%S_%3f");
    let name = format!("{}.png", stamp);
    let path = dir.join(&name);
    buf.save(&path).ok()?;
    Some(format!("Image/{name}"))
}
