//! Tauri 指令层。前端通过 `invoke` 调用这些函数，构成前后端契约。

use crate::clipboard::ClipService;
use crate::config::{ClipConfig, ClipItem, FavorItem};
use crate::db::Store;
use crate::util::{now_iso, short_id};
use parking_lot::Mutex;
use serde::Serialize;
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager, State};

/// 取主窗口的裸 HWND（仅 Windows）。
///
/// `win.hwnd()` 返回的句柄在各调用点都要做一次指针转换才能传给 `platform`
/// 的函数；集中到这里，避免同一段转换代码散落五处、改类型时漏改其一。
#[cfg(windows)]
fn panel_hwnd(app: &AppHandle) -> Option<windows::Win32::Foundation::HWND> {
    let win = app.get_webview_window("main")?;
    win.hwnd()
        .ok()
        .map(|h| windows::Win32::Foundation::HWND(h.0 as *mut std::ffi::c_void))
}

/// 应用全局状态。由 Tauri 托管，各指令按名取用。
pub struct AppState {
    pub store: Arc<Store>,
    pub config: Arc<Mutex<ClipConfig>>,
    pub clip_service: Arc<ClipService>,
    pub config_path: std::path::PathBuf,
    /// 本次启动是否由开机自启触发（命令行带 --silent）。
    pub launched_silently: bool,
    /// 窗口是否处于"固定"状态（置顶常驻）。
    ///
    /// 为纯运行时状态，不写入配置：它是本轮的交互意图，
    /// 而非需要跨会话保留的长期偏好。固定时面板常驻最上层，
    /// 且粘贴后不自动隐藏；取消固定则恢复"粘贴即关闭"。
    pub pinned: std::sync::atomic::AtomicBool,
    /// 面板当前是否处于"永不激活"（路线 B）状态。
    ///
    /// 为 `WS_EX_NOACTIVATE` 是否生效的软件侧镜像，用于：
    /// - 判断是否需要临时切回可激活（搜索框输入）
    /// - 鼠标钩子回调里决定"点窗外是否要隐藏"
    ///
    /// 之所以要镜像而不是每次读样式：钩子回调在极热的路径上，
    /// 读一次窗口样式要走内核，能省则省。
    pub no_activate: std::sync::atomic::AtomicBool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BootstrapPayload {
    pub config: ClipConfig,
    pub clips: Vec<ClipItem>,
    pub favors: Vec<FavorItem>,
    pub data_dir: String,
    pub version: String,
}

/// 启动时一次性拉取全部初始数据，避免前端多次往返。
#[tauri::command]
pub fn bootstrap(state: State<AppState>) -> BootstrapPayload {
    let cache = state.store.cache.read();
    let data_dir = state
        .store
        .db_path
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    BootstrapPayload {
        config: state.config.lock().clone(),
        clips: cache.clips.clone(),
        favors: cache.favors.clone(),
        data_dir,
        version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

// ---------- 配置 ----------

#[tauri::command]
pub fn save_config(state: State<AppState>, config: ClipConfig) -> Result<(), String> {
    config
        .save(&state.config_path)
        .map_err(|e| e.to_string())?;
    *state.config.lock() = config;
    Ok(())
}

// ---------- 剪贴板 ----------

#[tauri::command]
pub fn list_clips(state: State<AppState>, query: Option<String>) -> Vec<ClipItem> {
    let cache = state.store.cache.read();
    match query {
        Some(q) if !q.trim().is_empty() => {
            let q = q.to_lowercase();
            cache
                .clips
                .iter()
                .filter(|c| c.item.text.to_lowercase().contains(&q))
                .cloned()
                .collect()
        }
        _ => cache.clips.clone(),
    }
}

/// 将指定条目写回系统剪贴板。
///
/// `suppress` 置位是关键：写回动作本身会改变剪贴板序列号，
/// 若不屏蔽，轮询线程会把它当成一次新的复制行为而重复入库。
#[tauri::command]
pub fn copy_clip(
    app: AppHandle,
    state: State<AppState>,
    id: String,
    plain_text: Option<bool>,
) -> Result<(), String> {
    // 先在剪贴板历史里找，找不到再去收藏夹找。
    //
    // 两者的后续处理不同，必须分开：剪贴板条目"复制"后要提升到列表顶部，
    // 而收藏条目**不能**被写进 clip_item 表——否则"从收藏复制到剪贴板"
    // 会让收藏内容凭空出现在历史列表里，且每次复制都重复插入一次。
    let (item, from_clips) = {
        let cache = state.store.cache.read();
        if let Some(c) = cache.clips.iter().find(|c| c.id == id) {
            (c.clone(), true)
        } else if let Some(f) = cache.favors.iter().find(|c| c.id == id) {
            (
                ClipItem {
                    id: f.id.clone(),
                    item: f.item.clone(),
                    create_time: f.create_time.clone(),
                    update_time: f.update_time.clone(),
                    is_top: false,
                    sub_index: 0,
                    source_app: None,
                    hash: String::new(),
                },
                false,
            )
        } else {
            return Err("条目不存在".to_string());
        }
    };

    let cfg = state.config.lock().clone();
    state.clip_service.suppress.store(true, std::sync::atomic::Ordering::SeqCst);

    // 图片条目以数据目录为基准寻址（见 resolve_image_path）
    let data_dir = state.store.db_path.parent().unwrap_or(&state.store.db_path);
    write_to_clipboard(
        &item,
        plain_text.unwrap_or(cfg.plain_text_paste_by_default),
        data_dir,
    )?;

    // 仅剪贴板条目需要提升位置；收藏条目不参与历史排序
    if from_clips {
        let mut updated = item.clone();
        updated.update_time = now_iso();
        let _ = state.store.upsert_clip(&updated);
        {
            let mut c = state.store.cache.write();
            c.clips.retain(|x| x.id != updated.id);
            c.clips.insert(0, updated.clone());
        }
        let _ = app.emit("clip-updated", &updated);
    }
    Ok(())
}

fn write_to_clipboard(
    item: &ClipItem,
    plain: bool,
    data_dir: &std::path::Path,
) -> Result<(), String> {
    use crate::config::ObjType;
    let mut cb = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    match item.item.obj_type {
        ObjType::Files => {
            let paths: Vec<std::path::PathBuf> = item
                .item
                .text
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(std::path::PathBuf::from)
                .collect();
            cb.set().file_list(&paths).map_err(|e| e.to_string())
        }
        ObjType::Image => {
            let path = resolve_image_path(&item.item.text, data_dir);
            match image::open(&path) {
                Ok(img) => {
                    let rgba = img.to_rgba8();
                    let (w, h) = rgba.dimensions();
                    cb.set()
                        .image(arboard::ImageData {
                            width: w as usize,
                            height: h as usize,
                            bytes: rgba.into_raw().into(),
                        })
                        .map_err(|e| e.to_string())
                }
                Err(e) => Err(format!("图片读取失败: {e}")),
            }
        }
        _ => {
            let text = if plain {
                strip_rich(&item.item.text)
            } else {
                item.item.text.clone()
            };
            cb.set().text(text).map_err(|e| e.to_string())
        }
    }
}

/// 图片类型下 text 存的是相对路径（`Image/xxx.png`），需还原为绝对路径。
///
/// **基准必须是数据目录，不能是进程当前工作目录**：保存时用的是
/// `数据目录/Image/`，而进程的 cwd 取决于用户从哪启动（快捷方式、安装目录、
/// 甚至资源管理器双击都各不相同），两者一旦不一致，图片就读不到——
/// 表现为"历史里有图片条目，点粘贴却报读取失败"。
fn resolve_image_path(rel: &str, data_dir: &std::path::Path) -> std::path::PathBuf {
    if std::path::Path::new(rel).is_absolute() {
        return std::path::PathBuf::from(rel);
    }
    data_dir.join(rel)
}

/// 去除富文本标记，仅保留纯文本。用于"按纯文本粘贴"。
///
/// 只剥标签，**不动空白结构**：换行是用户内容的一部分（多行文本按纯文本
/// 粘贴后应当仍是多行）。早期版本在末尾做了
/// `split_whitespace().join(" ")`，会把所有换行压成单个空格，
/// 表现为"粘贴出来变成一行"。
fn strip_rich(s: &str) -> String {
    if !s.starts_with('<') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// 删除若干条目。
#[tauri::command]
pub fn delete_clips(app: AppHandle, state: State<AppState>, ids: Vec<String>) -> Result<(), String> {
    state.store.delete_clips(&ids).map_err(|e| e.to_string())?;
    {
        let mut c = state.store.cache.write();
        c.clips.retain(|x| !ids.contains(&x.id));
    }
    let _ = app.emit("clips-changed", &());
    Ok(())
}

/// 置顶 / 取消置顶。
#[tauri::command]
pub fn toggle_top(state: State<AppState>, id: String) -> Result<(), String> {
    let mut updated = {
        let mut c = state.store.cache.write();
        let idx = c.clips.iter().position(|x| x.id == id);
        match idx {
            Some(i) => {
                let mut it = c.clips.remove(i);
                it.is_top = !it.is_top;
                it.update_time = now_iso();
                if it.is_top {
                    c.clips.insert(0, it.clone());
                } else {
                    c.clips.push(it.clone());
                }
                it
            }
            None => return Err("条目不存在".into()),
        }
    };
    updated.update_time = now_iso();
    state.store.upsert_clip(&updated).map_err(|e| e.to_string())?;
    Ok(())
}

// ---------- 收藏夹 ----------

#[tauri::command]
pub fn list_favors(state: State<AppState>) -> Vec<FavorItem> {
    state.store.cache.read().favors.clone()
}

/// 将剪贴板条目加入收藏。`parent_id` 为空表示根级。
#[tauri::command]
pub fn add_favor(
    app: AppHandle,
    state: State<AppState>,
    id: String,
    parent_id: Option<String>,
    title: Option<String>,
) -> Result<FavorItem, String> {
    let src = {
        let c = state.store.cache.read();
        c.clips.iter().find(|x| x.id == id).cloned()
    }
    .ok_or_else(|| "源条目不存在".to_string())?;

    let fid = short_id();
    let favor = FavorItem {
        id: fid.clone(),
        item: src.item.clone(),
        title: title.unwrap_or_else(|| {
            src.item
                .text
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(30)
                .collect()
        }),
        create_time: now_iso(),
        update_time: now_iso(),
        parent_id: parent_id.clone().unwrap_or_default(),
        is_deleted: false,
        description: String::new(),
        children: Vec::new(),
        is_folder: false,
    };

    state.store.upsert_favor(&favor).map_err(|e| e.to_string())?;

    // 挂到父节点下（若有）。先在锁内改内存，再把受影响的父节点收集出来，
    // 出锁后统一落库——不在持锁状态下做 IO。
    let mut parent_dirty: Option<FavorItem> = None;
    {
        let mut c = state.store.cache.write();
        c.favors.insert(0, favor.clone());
        if let Some(pid) = &parent_id {
            if let Some(p) = c.favors.iter_mut().find(|f| f.id == *pid) {
                p.children.push(fid.clone());
                parent_dirty = Some(p.clone());
            }
        }
    }
    if let Some(p) = &parent_dirty {
        let _ = state.store.upsert_favor(p);
    }
    let _ = app.emit("favors-changed", &());
    Ok(favor)
}

#[tauri::command]
pub fn delete_favor(app: AppHandle, state: State<AppState>, id: String) -> Result<(), String> {
    // 递归收集自身与全部子孙节点，一并删除
    let mut to_delete = vec![id.clone()];
    {
        let c = state.store.cache.read();
        let mut i = 0;
        while i < to_delete.len() {
            let cur = to_delete[i].clone();
            if let Some(node) = c.favors.iter().find(|f| f.id == cur) {
                for ch in &node.children {
                    if !to_delete.contains(ch) {
                        to_delete.push(ch.clone());
                    }
                }
            }
            i += 1;
        }
    }

    for did in &to_delete {
        state.store.delete_favor(did).map_err(|e| e.to_string())?;
    }
    {
        // 摘除各父节点 children 里对被删节点的引用。
        //
        // 必须遍历**所有**受影响的父节点并逐个落库：早期实现在找到第一个
        // children 发生变化的节点后就 break，若同一节点被多个父节点引用
        // （数据异常或历史遗留），后面的父节点在内存里改了却没写库，
        // 重启后引用又回来了。
        //
        // 先收集再统一写库，避免在持锁状态下调用 store（虽然当前实现不会
        // 死锁，但"持锁做 IO"是应当避开的模式）。
        let mut dirty: Vec<FavorItem> = Vec::new();
        {
            let mut c = state.store.cache.write();
            for f in c.favors.iter_mut() {
                let before = f.children.len();
                f.children.retain(|x| !to_delete.contains(x));
                if f.children.len() != before {
                    dirty.push(f.clone());
                }
            }
        }
        for f in &dirty {
            let _ = state.store.upsert_favor(f);
        }
    }
    let _ = app.emit("favors-changed", &());
    Ok(())
}

/// 把指定路径交给资源管理器打开。
#[tauri::command]
pub fn open_path(path: String) -> Result<(), String> {
    #[cfg(windows)]
    {
        std::process::Command::new("explorer")
            .arg(&path)
            .spawn()
            .map_err(|e| e.to_string())?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Err("仅支持 Windows".into())
    }
}

/// 按配置策略把面板摆到合适的位置。在 `show()` **之前**调用。
///
/// 三种策略：
/// - `cursor`（默认）：跟随鼠标，就近弹出。鼠标右/下方空间不足时自动翻到左/上方，
///   而不是简单地把窗口钳进屏幕——钳制会让窗口"离开鼠标"，失去就近的意义。
/// - `last`：沿用上次关闭时的坐标（`last_x` / `last_y`）。无历史记录时退回 `cursor`。
/// - `fixed`：使用用户设定的 `fixed_x` / `fixed_y`，做边界钳制。
#[cfg(windows)]
fn apply_window_position(app: &AppHandle, win: &tauri::WebviewWindow, mode: &str) {
    use tauri::PhysicalPosition;

    let Ok(size) = win.outer_size() else { return };
    let w = size.width as i32;
    let h = size.height as i32;

    // 只有 `last` / `fixed` 两种策略需要读配置，`cursor`（默认）完全不依赖它。
    // 因此先判模式，避免每次唤出都 clone 一整份配置。
    //
    // `State` 句柄必须先绑定到变量：它是 `app.state()` 返回的临时值，
    // 直接链式 `.lock()` 会让守卫引用一个已被释放的对象。
    let pos: Option<PhysicalPosition<i32>> = match mode {
        "last" | "fixed" => {
            let state = app.state::<AppState>();
            let cfg = state.config.lock();
            let area = crate::platform::work_area_at_cursor();
            if mode == "fixed" {
                Some(clamp_to(area, cfg.fixed_x, cfg.fixed_y, w, h))
            } else if cfg.last_x == 0 && cfg.last_y == 0 {
                // 沿用上次位置；首启动（0,0）视为"无记录"，退回跟随鼠标
                cursor_anchored(w, h)
            } else {
                Some(clamp_to(area, cfg.last_x, cfg.last_y, w, h))
            }
        }
        // `cursor`，以及任何无法识别的值都按跟随鼠标处理
        _ => cursor_anchored(w, h),
    };

    if let Some(p) = pos {
        let _ = win.set_position(p);
    }
}

/// 把 (x, y) 钳进工作区，保证窗口完整可见。
#[cfg(windows)]
fn clamp_to(
    area: Option<(i32, i32, i32, i32)>,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) -> tauri::PhysicalPosition<i32> {
    let (left, top, right, bottom) = area.unwrap_or((0, 0, i32::MAX / 2, i32::MAX / 2));
    tauri::PhysicalPosition::new(
        x.clamp(left, (right - w).max(left)),
        y.clamp(top, (bottom - h).max(top)),
    )
}

/// 以鼠标为锚点计算窗口位置：优先右下方，越界则翻向左上方。
#[cfg(windows)]
fn cursor_anchored(w: i32, h: i32) -> Option<tauri::PhysicalPosition<i32>> {
    let (mx, my) = crate::platform::cursor_position()?;
    let (left, top, right, bottom) = crate::platform::work_area_at_cursor()?;

    // 8px 间隙，避免窗口边缘紧贴光标造成视觉压迫
    let gap = 8;
    let mut x = mx + gap;
    let mut y = my + gap;

    // 越界则翻转方向，而不是钳制——保持"挨着鼠标"的相对关系
    if x + w > right {
        x = mx - w - gap;
    }
    if y + h > bottom {
        y = my - h - gap;
    }

    // 翻转后仍越界（鼠标贴在最左上角等极端情况）才退化为钳制
    Some(tauri::PhysicalPosition::new(
        x.clamp(left, (right - w).max(left)),
        y.clamp(top, (bottom - h).max(top)),
    ))
}

#[cfg(not(windows))]
fn apply_window_position(_app: &AppHandle, _win: &tauri::WebviewWindow, _mode: &str) {}

/// 显示面板窗口。
///
/// 注意：**这里不再记录粘贴目标**。目标由 `platform` 模块的前台窗口事件钩子
/// 持续维护（见 `install_foreground_tracker`）。原因：本函数执行时，前台窗口
/// 可能已经变成面板自己（面板刚被鼠标点过、或上次隐藏尚未生效），此时抓到的
/// 目标是错的——这正是"粘贴时好时坏"的根源。
pub fn show_panel(app: &AppHandle) {
    let state = app.state::<AppState>();
    let pinned = state.pinned.load(std::sync::atomic::Ordering::SeqCst);

    #[cfg(windows)]
    {
        // 若钩子尚未捕获到目标（极早期或钩子安装失败），退化为此刻抓取一次。
        // 仅作为兜底，不作为常规路径。
        if crate::platform::last_foreign_foreground().0.is_null() {
            let fg = crate::platform::foreground_window();
            if crate::platform::is_window(fg)
                && !crate::platform::window_belongs_to_current_process(fg)
            {
                crate::platform::set_last_foreign_foreground(fg);
                let name = crate::platform::process_name_of_window(fg).unwrap_or_default();
                log::info!("钩子未就绪，临时记录粘贴目标: {name}");
            }
        }
    }

    if let Some(win) = app.get_webview_window("main") {
        // 配置快照只取一次，避免重复加锁（parking_lot::Mutex 不可重入，
        // 同一作用域内二次 lock 会死锁）。
        let cfg = state.config.lock().clone();

        // 先定位再显示：位置切换若发生在显示之后，用户会看到窗口"跳"一下
        apply_window_position(app, &win, &cfg.window_position);

        // 是否需要"永不激活地显示"（路线 B）。
        //
        // 默认开启：许多目标程序的输入框失焦即关闭（浏览器地址栏联想、
        // 页面内搜索条、编辑器命令面板、聊天框 @ 提及浮层等）。面板若按
        // 常规方式 set_focus() 抢焦点，输入框会在用户按下快捷键的瞬间关闭，
        // 等粘贴时焦点还回去，输入框已经没了——Ctrl+V 落到窗口本体上。
        //
        // 置顶常驻（pinned）时**必然**不夺焦点：固定的语义本就是"常驻最上层
        // 却不打断用户"，它早先就已跳过 set_focus，此处统一到同一分支。
        #[cfg(windows)]
        let no_activate = pinned || cfg.show_without_activating;
        #[cfg(not(windows))]
        let no_activate = pinned;

        #[cfg(windows)]
        if no_activate {
            // ---- 路线 B：给窗口加上 WS_EX_NOACTIVATE，使其永不参与激活 ----
            //
            // 与"显示时不激活"（SWP_NOACTIVATE）的区别：后者只作用于那一次
            // SetWindowPos，用户点一下窗口它照样会变成前台；本路线是给窗口
            // 打上持久样式，除非显式去掉，否则它永远不会成为前台窗口。
            //
            // 这样才能保证：无论面板显示、隐藏、还是用户点击它，
            // 目标程序的输入框都**始终不失去焦点**。
            match panel_hwnd(app) {
                Some(h) => {
                    // 先把样式打上，再显示——顺序很重要：
                    // 若先显示后加样式，窗口会经历一次"可激活 → 不可激活"
                    // 的翻转，期间可能短暂抢走前台焦点，正好破坏了我们要保的输入框。
                    crate::platform::set_no_activate(h, true);
                    if crate::platform::show_no_activate(h, pinned) {
                        state.no_activate.store(true, std::sync::atomic::Ordering::SeqCst);
                        log::debug!("面板已显示（路线 B：永不激活）");
                    } else {
                        let _ = win.show();
                    }
                }
                None => {
                    let _ = win.show();
                }
            }
        } else {
            // 未开启不夺焦点：恢复普通可激活窗口后再显示。
            #[cfg(windows)]
            if let Some(hwnd) = panel_hwnd(app) {
                crate::platform::set_no_activate(hwnd, false);
            }
            state.no_activate.store(false, std::sync::atomic::Ordering::SeqCst);
            let _ = win.show();
            // 允许夺焦点的常规路径：用户才能直接输入搜索、用键盘操作。
            let _ = win.set_focus();
        }

        #[cfg(not(windows))]
        {
            let _ = no_activate;
            let _ = win.show();
            if !pinned {
                let _ = win.set_focus();
            }
        }

        // ---- 安装"点窗外隐藏"的全局鼠标钩子（路线 B 的必需配套）----
        //
        // WS_EX_NOACTIVATE 的窗口不会成为前台窗口，因此收不到 WM_KILLFOCUS，
        // "点击别处就收起"必须靠全局钩子自己判断点击落点。
        // 钩子只装一次，在整个进程生命周期内有效。
        #[cfg(windows)]
        if no_activate {
            if let Some(h) = panel_hwnd(app) {
                crate::mouse_hook::ensure_installed(h.0 as isize);
            }
        } else {
            crate::mouse_hook::set_active(false);
        }

        // 若处于固定状态，重新唤出时必须恢复置顶。
        //
        // 为什么不能只依赖 `set_always_on_top(true)`：
        // hide/show 循环、外部程序改 Z 序、以及本进程内的 `show_no_activate`
        // 都可能让窗口脱离 topmost，而框架的内部状态标记不会跟着更新，
        // 于是"框架以为还置顶、系统实际已取消"。这里直接用原生
        // `is_topmost()` 读**系统真实样式**，发现掉了就补一次
        // `SetWindowPos(HWND_TOPMOST)`，保证固定语义不丢失。
        if pinned {
            #[cfg(windows)]
            {
                let _ = win.set_always_on_top(true);
                if let Some(hwnd) = panel_hwnd(app) {
                    if !crate::platform::is_topmost(hwnd) {
                        crate::platform::set_topmost(hwnd, true);
                        log::info!("唤出时补设置顶（固定状态）");
                    }
                }
            }
            #[cfg(not(windows))]
            {
                let _ = win.set_always_on_top(true);
            }
        }
        // 携带是否已激活的信息给前端：未激活时**不能**让搜索框抢焦点，
        // 否则前端一句 $search.focus() 就把刚保住的输入框又弄丢了。
        let _ = app.emit("panel-shown", serde_json::json!({ "activated": !no_activate }));
    }
}

/// 隐藏面板窗口。
pub fn hide_panel(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        // 关闭前记录位置，供"上次关闭位置"策略使用。
        // 必须在 hide() 之前读——窗口隐藏后部分后端会返回 (0,0)。
        remember_window_position(app, &win);

        let _ = win.hide();

        // 隐藏后把窗口恢复成普通可激活状态。
        //
        // 为什么不在隐藏前做：面板可见时是 WS_EX_NOACTIVATE，
        // 若在隐藏过程中去掉该样式，窗口可能短暂地被激活一次、
        // 从而抢走目标程序的焦点——正好破坏我们要保的东西。
        // 隐藏之后窗口已不可见，此刻改样式不会产生任何视觉或焦点副作用。
        //
        // 恢复的意义：下次若用户关掉了"永不激活"设置，窗口应能正常被激活；
        // 若不恢复，"关掉设置也没用"会变成一个极难排查的残留 bug。
        #[cfg(windows)]
        {
            if let Some(hwnd) = panel_hwnd(app) {
                crate::platform::set_no_activate(hwnd, false);
            }
            app.state::<AppState>()
                .no_activate
                .store(false, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

/// 临时切换面板的"可聚焦"状态，供搜索框输入使用。
///
/// `WS_EX_NOACTIVATE` 的窗口永远无法获得键盘焦点，搜索框也就无法打字。
/// 这是路线 B 的**固有代价**，唯一解法是：用户要输入时临时去掉该样式，
/// 输入结束（失焦 / 关闭搜索）再戴回去。
///
/// 调用时机由前端掌握：
/// - 用户点击搜索框 / 按下会进入输入的键 → `focusable = true`
/// - 搜索框失焦 / 面板隐藏 → `focusable = false`
#[tauri::command]
pub fn set_search_focusable(app: AppHandle, focusable: bool) {
    #[cfg(windows)]
    {
        if let Some(hwnd) = panel_hwnd(&app) {
            // 取反：要能输入 = 不带 NOACTIVATE
            crate::platform::set_no_activate(hwnd, !focusable);
            log::debug!("搜索框可聚焦: {focusable}");
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (app, focusable);
    }
}

/// 轮询"是否请求隐藏面板"。
///
/// 全局鼠标钩子（钩子线程）不能直接调 Tauri API，只能置一个原子标志；
/// 前端以较低频率调用本命令把标志取走，再真正执行隐藏。
///
/// 为什么用轮询而不是事件：钩子里做事件投递需要 `AppHandle`，
/// 而 `AppHandle` 跨线程传递要额外包装；这个动作频率极低（用户点一下才触发），
/// 轮询的开销可以忽略，换来实现上的简单可靠。
///
/// 面板不可见时直接返回 `false`，并且**不消费**标志：
/// - 前端无从可靠知道窗口到底隐藏没有（`document.hidden` 反映的是页面可见性，
///   而 Tauri 的 `hide()` 不一定触发它），所以这个判断只能放在后端做；
/// - 不消费标志是为了不丢事件——若窗口恰好在这一刻被隐藏，用户下次唤出面板时
///   那一击的隐藏请求仍然有效，不会出现"面板唤出后又莫名不隐藏"。
#[tauri::command]
pub fn poll_hide_request(app: AppHandle) -> bool {
    #[cfg(windows)]
    {
        // 面板不可见时直接返回 false，且不消费标志（理由见函数文档）
        match panel_hwnd(&app) {
            Some(hwnd) if crate::platform::is_visible(hwnd) => {}
            _ => return false,
        }
    }
    #[cfg(not(windows))]
    {
        let _ = app;
    }
    crate::mouse_hook::take_hide_request()
}

/// 把窗口当前坐标写入配置，供 `windowPosition = "last"` 使用。
///
/// 只在位置策略无关紧要时也照记：用户可能随时切换策略，
/// 提前积累数据可以让切换后的第一次弹出就落在正确位置。
///
/// 若当前策略是 `fixed`，同时更新 `fixed_x` / `fixed_y`——
/// 这样"固定位置"模式下用户手动拖走窗口后，新位置会被采纳，
/// 而不是下次弹出又跳回旧坐标。
fn remember_window_position(app: &AppHandle, win: &tauri::WebviewWindow) {
    let Ok(pos) = win.outer_position() else { return };
    let state = app.state::<AppState>();
    let mut cfg = state.config.lock();

    if cfg.last_x == pos.x && cfg.last_y == pos.y {
        return; // 无变化，跳过写盘
    }
    cfg.last_x = pos.x;
    cfg.last_y = pos.y;

    if cfg.window_position == "fixed" {
        cfg.fixed_x = pos.x;
        cfg.fixed_y = pos.y;
    }
    let _ = cfg.save(&state.config_path);
}

/// 切换面板显示状态。全局快捷键使用此函数，实现"按一次开、再按一次关"。
pub fn toggle_panel_impl(app: &AppHandle) {
    let visible = app
        .get_webview_window("main")
        .and_then(|w| w.is_visible().ok())
        .unwrap_or(false);
    if visible {
        hide_panel(app);
    } else {
        show_panel(app);
    }
}

#[tauri::command]
pub fn toggle_panel(app: AppHandle) {
    toggle_panel_impl(&app);
}

#[tauri::command]
pub fn hide_window(app: AppHandle) {
    hide_panel(&app);
}

/// 查询当前是否处于固定（置顶常驻）状态。
#[tauri::command]
pub fn get_pinned(state: State<AppState>) -> bool {
    state.pinned.load(std::sync::atomic::Ordering::SeqCst)
}

/// 设置窗口固定状态。
///
/// 固定 = 置顶常驻：窗口保持最上层，且粘贴后不自动关闭，
/// 便于连续多次粘贴。取消固定恢复默认的"粘贴一次即关闭"。
///
/// 置顶能力由本开关独占控制：窗口基础配置为 `alwaysOnTop: false`，
/// 只有固定时才置顶。若基础配置就是置顶，这个开关将失去意义。
///
/// **取消固定时必须调用 `clear_topmost` 显式降级**：以管理员身份运行时，
/// 仅靠框架的 `set_always_on_top(false)` 无法改变窗口 Z 序（会被系统忽略），
/// 表现为"取消固定后依然置顶"。
///
/// 返回值取自系统实际样式，而非我们写入的意图值——这样即使系统拒绝了
/// 某次操作，界面也不会显示一个与事实不符的状态。
#[tauri::command]
pub fn set_pinned(app: AppHandle, state: State<AppState>, pinned: bool) -> bool {
    if let Some(win) = app.get_webview_window("main") {
        #[cfg(windows)]
        {
            if let Some(hwnd) = panel_hwnd(&app) {
                if pinned {
                    // 置顶：先走框架，再校验；失败则用显式 SetWindowPos 兜底
                    let _ = win.set_always_on_top(true);
                    if !crate::platform::is_topmost(hwnd) {
                        crate::platform::set_topmost(hwnd, true);
                    }
                } else {
                    // 取消置顶：显式降级是唯一可靠的路径，详见 clear_topmost 注释
                    let _ = win.set_always_on_top(false);
                    crate::platform::clear_topmost(hwnd);
                }
                // 以真实样式为准回写状态，避免"界面说取消了、实际还置顶"
                let actual = crate::platform::is_topmost(hwnd);
                state
                    .pinned
                    .store(actual, std::sync::atomic::Ordering::SeqCst);
                log::info!("窗口固定状态: 请求 {pinned} → 实际 {actual}");
                return actual;
            }
        }
        #[cfg(not(windows))]
        {
            let _ = win.set_always_on_top(pinned);
        }
    }
    state
        .pinned
        .store(pinned, std::sync::atomic::Ordering::SeqCst);
    pinned
}

/// 触发一次粘贴：先恢复目标窗口焦点，再把 Ctrl+V 投递给它。
///
/// 顺序至关重要，且**先恢复焦点、后隐藏面板**是本轮修正的核心：
///
///   1. **趁面板还是前台窗口时**，把焦点交还给记录的目标窗口
///   2. 隐藏面板（此后面板不再是前台）
///   3. 再次确认目标窗口已拿到焦点，未拿到则重试
///   4. 投递 Ctrl+V
///
/// 旧实现的顺序是"先隐藏、再恢复焦点"。在目标程序响应较慢时，面板的隐藏
/// 动画与目标窗口的激活会互相竞争，`SetForegroundWindow` 往往被系统拒绝，
/// 按键最终投递给了焦点悬空的窗口——用户看到的就是"点了没反应"。
///
/// **固定（置顶常驻）状态下**：面板不隐藏，聚焦动作仍会执行（用户可能
/// 刚点过别处），但粘贴后窗口保持最上层，便于连续粘贴多条。
#[tauri::command]
pub fn paste_to_active(app: AppHandle, state: State<AppState>) {
    use std::sync::atomic::Ordering;

    let cfg = state.config.lock().clone();
    let pinned = state.pinned.load(Ordering::SeqCst);

    #[cfg(windows)]
    {
        use windows::Win32::Foundation::HWND;

        // 目标来自前台窗口事件钩子——它记录的是"用户最后一次真正在用的
        // 非自身窗口"，与实际唤出时机无关，因此天然正确。
        let hwnd: HWND = crate::platform::last_foreign_foreground();

        if hwnd.0.is_null() || !crate::platform::is_window(hwnd) {
            log::warn!("粘贴已中止：无有效的目标窗口记录");
            if !pinned {
                hide_panel(&app);
            }
            return;
        }

        let name = crate::platform::process_name_of_window(hwnd).unwrap_or_default();

        // ---- 判断焦点是否已经在我们手里 ----
        //
        // "显示但不夺焦点"模式下，面板自始至终没有拿到前台焦点，焦点一直
        // 留在目标窗口（正是我们想要的）。此时**绝不能**再去 force_foreground：
        // 那一步虽然目标窗口已经是前台、会立刻返回 true，但"抢占焦点"这个
        // 动作本身在某些程序里会重新触发一次激活流程，反而把输入框的
        // 焦点状态搅乱（联想下拉重新弹出、命令面板重启等）。
        //
        // 换句话说：抢焦点是"焦点被面板夺走了"才需要的**补救**手段；
        // 焦点没丢，就不该去碰。
        let already_foreground = crate::platform::foreground_window() == hwnd;

        if !already_foreground {
            // ---- 第 1 步：把焦点交还给目标窗口 ----
            // 必须在**隐藏面板之前**：面板此刻还是前台，Windows 允许前台进程
            // 转交前台；一旦面板隐藏，焦点会短暂悬空，再抢就难了。
            let ok = crate::platform::force_foreground(hwnd);
            log::info!("粘贴 → {name}: 抢占焦点 {ok}");
        } else {
            log::debug!("粘贴 → {name}: 焦点已在目标窗口，无需抢占");
        }

        // ---- 第 2 步：隐藏面板（固定时跳过，保持常驻）----
        if !pinned {
            hide_panel(&app);
        }

        // ---- 第 3 步：等待焦点落定 ----
        // 窗口切换 50-200ms、焦点确认 20-100ms（业界实测区间）。
        // 用户配置的延迟优先；未配置时取 150ms，比早先的 120ms 更稳。
        //
        // 焦点本就未丢时（不夺焦点模式）可以更短：没有窗口切换要等，
        // 只是给目标程序一点时间处理剪贴板写入事件。
        let delay = if cfg.default_paste_delay > 0 {
            cfg.default_paste_delay
        } else if already_foreground {
            40
        } else {
            150
        };
        std::thread::sleep(std::time::Duration::from_millis(delay));

        // ---- 第 4 步：焦点未到位则重试 ----
        // 最多 3 轮，逐轮加长等待，覆盖响应较慢的目标程序。
        let mut focused = crate::platform::foreground_window() == hwnd;
        for attempt in 1..=3 {
            if focused {
                break;
            }
            log::warn!("粘贴：目标未获得焦点，第 {attempt} 次重试");
            crate::platform::force_foreground(hwnd);
            std::thread::sleep(std::time::Duration::from_millis(60 * attempt));
            focused = crate::platform::foreground_window() == hwnd;
        }

        // ---- 第 5 步：仅在焦点确认到位时投递按键 ----
        // 焦点没到位就投键，Ctrl+V 会落到别的窗口，造成**误输入**——
        // 那比粘贴失败更糟。所以宁可放弃本次，并把原因写进日志。
        if focused {
            crate::platform::send_paste();
            log::info!("粘贴完成 → {name}");
        } else {
            let got = crate::platform::foreground_window();
            let got_name = crate::platform::process_name_of_window(got).unwrap_or_default();
            log::warn!("粘贴已中止：焦点未能回到 {name}（当前前台: {got_name}）");
        }

        // 固定状态下面板需回到最上层（焦点已交给目标，此处只置顶不抢焦点）
        if pinned {
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.set_always_on_top(true);
            }
        }
    }

    #[cfg(not(windows))]
    {
        let _ = (cfg, pinned);
        hide_panel(&app);
    }
}

/// 重新注册全局快捷键。
///
/// 用户修改快捷键后必须调用：旧快捷键需注销，新快捷键需注册。
/// 此前缺失此逻辑，导致改设置后新快捷键完全不生效。
#[tauri::command]
pub fn rebind_hotkey(app: AppHandle, state: State<AppState>, hotkey: String) -> Result<(), String> {
    use tauri_plugin_global_shortcut::GlobalShortcutExt;

    let gs = app.global_shortcut();

    // 先注销全部已注册快捷键，避免旧键残留
    if let Err(e) = gs.unregister_all() {
        log::warn!("注销旧快捷键失败: {e}");
    }

    if hotkey.trim().is_empty() {
        state.config.lock().hotkey = hotkey;
        return Ok(());
    }

    match crate::hotkey::parse(&hotkey) {
        Some(sc) => {
            gs.register(sc).map_err(|e| {
                let msg = format!(
                    "快捷键「{hotkey}」注册失败：{e}。该组合可能已被系统或其他程序占用，请换一个。"
                );
                log::warn!("{msg}");
                msg
            })?;
            // 同步到内存配置，供热键处理器比对
            state.config.lock().hotkey = hotkey.clone();
            log::info!("全局快捷键已重新注册: {hotkey}");
            Ok(())
        }
        None => {
            let msg = format!(
                "无法解析快捷键「{hotkey}」。格式应为「修饰键+主键」，例如 Alt+V、Ctrl+Shift+V。"
            );
            log::warn!("{msg}");
            Err(msg)
        }
    }
}

/// 打开数据目录。
#[tauri::command]
pub fn open_data_dir(state: State<AppState>) -> Result<(), String> {
    if let Some(dir) = state.store.db_path.parent() {
        open_path(dir.to_string_lossy().to_string())?;
    }
    Ok(())
}

// ---------- 开机自启 ----------

/// 查询开机自启的真实状态。
///
/// 以**系统注册表实际状态**为准，而非配置文件的值——用户可能通过任务管理器
/// 或其他工具改动过启动项，此时配置文件会与事实不符。
#[tauri::command]
pub fn get_autostart(app: AppHandle) -> bool {
    use tauri_plugin_autostart::ManagerExt;
    app.autolaunch().is_enabled().unwrap_or(false)
}

/// 设置开机自启开关。
///
/// 返回设置后的真实状态，便于前端据此校正界面。
#[tauri::command]
pub fn set_autostart(
    app: AppHandle,
    state: State<AppState>,
    enabled: bool,
) -> Result<bool, String> {
    use tauri_plugin_autostart::ManagerExt;
    let mgr = app.autolaunch();

    let result = if enabled {
        mgr.enable()
    } else {
        mgr.disable()
    };
    result.map_err(|e| format!("设置开机自启失败：{e}"))?;

    // 同步到配置文件，保持两者一致。只改一个字段，不必 clone 整份配置
    // （整份克隆在这里纯属浪费：随后立刻被整体写回，中间没有任何读取）。
    {
        let mut cfg = state.config.lock();
        cfg.auto_start = enabled;
        let _ = cfg.save(&state.config_path);
    }

    Ok(mgr.is_enabled().unwrap_or(enabled))
}
