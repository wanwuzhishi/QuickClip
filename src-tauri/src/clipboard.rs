//! 剪贴板采集服务。
//!
//! 采用轮询剪贴板序列号的方式判断内容变化，而非挂钩子（hook）。
//! 理由：钩子方案需要注入其他进程，易被杀软拦截且稳定性差；
//! 序列号轮询为进程外读取，兼容性与安全性都更优，200ms 间隔的 CPU 开销可忽略。

use crate::config::{ClipConfig, ClipItem, ObjType};
use crate::db::Store;
use crate::util::{content_hash, now_iso, short_id};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// 剪贴板服务运行状态。原子量避免加锁，轮询线程可无阻塞读取。
pub struct ClipService {
    /// 轮询线程的存活开关。置 false 后线程在下一次 tick 退出。
    pub running: Arc<AtomicBool>,
    /// 应用自身写入剪贴板时置位，用于跳过自触发。
    ///
    /// 用 `swap(false)` 而非 `load` + `store`：前者是单次原子操作，
    /// 不会出现"读到 true 后、清标志前又来一次写入"的漏判窗口。
    pub suppress: Arc<AtomicBool>,
}

impl ClipService {
    pub fn new() -> Self {
        Self {
            running: Arc::new(AtomicBool::new(true)),
            suppress: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// 轮询主循环。由独立线程驱动，每 tick 检查一次剪贴板序列号。
pub fn spawn_watcher(
    svc: Arc<ClipService>,
    store: Arc<Store>,
    cfg: Arc<parking_lot::Mutex<ClipConfig>>,
    on_change: Arc<dyn Fn(&ClipItem) + Send + Sync>,
) {
    std::thread::spawn(move || {
        // 初始化基线序列号，避免把启动前的旧内容当作新内容采集
        let mut seq = crate::platform::clipboard_sequence_number();

        while svc.running.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(200));

            let cur = crate::platform::clipboard_sequence_number();
            if cur == seq {
                continue;
            }
            seq = cur;

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

/// 本进程可执行文件名（小写），首次使用时缓存。
///
/// `current_exe()` 是系统调用且返回 `PathBuf`（含分配），而它在本进程
/// 生命周期内不会变。采集路径虽然只在剪贴板变化时走，仍不值得每次
/// 都重新查一次。
fn self_exe_name() -> &'static str {
    static CACHE: OnceLock<String> = OnceLock::new();
    CACHE.get_or_init(|| {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.file_name().map(|s| s.to_string_lossy().to_lowercase()))
            .unwrap_or_default()
    })
}

/// 判断当前前台窗口是否属于需要记录的应用（排除本程序自身与桌面）。
fn current_source_app() -> Option<String> {
    let hwnd = crate::platform::foreground_window();
    let name = crate::platform::process_name_of_window(hwnd)?;
    if name.to_lowercase() == self_exe_name() {
        return None;
    }
    Some(name)
}

/// 读取当前剪贴板并构造条目。按 文件 → 图片 → 富文本 → 文本 的优先级探测，
/// 与 Windows 剪贴板的格式优先级一致。
///
/// 模块私有：采集流程由 `spawn_watcher` 独占驱动，没有外部调用方。
fn capture(cfg: &ClipConfig, source: Option<&str>, store: &Store) -> Option<ClipItem> {
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
///
/// 用 `take(max + 1).count()` 判断是否超长：只需知道"是否超过 max"，
/// 不必把整串数字完。对 20000 字符的内容，超长时能省掉后面全部遍历。
fn truncate(s: &str, max: i64) -> String {
    if max <= 0 {
        return s.to_string();
    }
    let max = max as usize;
    if s.chars().take(max + 1).count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
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
///
/// 文件名 = 时间戳（毫秒）+ 短随机后缀。**随机后缀不是可选项**：
/// 只用毫秒时间戳时，快速连截两张图（截屏工具连按是常见操作）
/// 只要落在同一毫秒，后一张就会覆盖前一张——用户看到的是
/// "历史里少了一条，且剩下那条内容不对"。
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
    let name = format!("{}_{}.png", stamp, short_id());
    let path = dir.join(&name);
    // 失败必须记日志：否则"复制了图片但历史里没有"这类问题无从排查
    // （此时 read_clipboard_payload 会静默降级到文本分支，最终整条被丢弃）。
    match buf.save(&path) {
        Ok(()) => Some(format!("Image/{name}")),
        Err(e) => {
            log::warn!("图片落盘失败 {name}: {e}");
            None
        }
    }
}
