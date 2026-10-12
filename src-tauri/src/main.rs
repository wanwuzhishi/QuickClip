//! 应用主入口：初始化状态、注册托盘与全局快捷键、启动剪贴板监听。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use quickclip_lib::clipboard::ClipService;
use quickclip_lib::commands::{self, AppState};
use quickclip_lib::config::ClipConfig;
use quickclip_lib::db::Store;
use parking_lot::Mutex;
use std::sync::Arc;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{Emitter, Manager, RunEvent};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};

/// 开机自启时携带的命令行参数。
///
/// 该值必须静态声明并传给 `autostart::init`——插件写入系统启动项时会把它
/// 一并写入注册表，因此不能在运行时动态决定。
pub const SILENT_FLAG: &str = "--silent";

fn main() {
    // 注意：此处不调用 env_logger::init()。
    // log crate 只允许注册一个全局 logger，若先注册 env_logger，
    // 后续的文件 logger 注册会静默失败，导致日志写不进文件。
    // 应用需要把启动状态落盘以便排查问题，故日志统一走文件。

    // 先于任何初始化判断：本次是否由开机自启触发。
    // 使用它而非配置项，是因为自启场景必须无条件静默，
    // 不受用户"启动时静默"开关影响。
    let launched_silently = std::env::args().any(|a| a == SILENT_FLAG);

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec![SILENT_FLAG]),
        ))
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, shortcut, event| {
                    // 只在按下时响应，避免抬起事件触发第二次切换
                    if event.state() != ShortcutState::Pressed {
                        return;
                    }
                    let cfg = app.state::<AppState>().config.lock().clone();
                    let want = quickclip_lib::hotkey::parse(&cfg.hotkey);
                    if want.map(|w| w == *shortcut).unwrap_or(false) {
                        // 切换语义：按一次显示，再按一次隐藏
                        commands::toggle_panel_impl(app);
                    }
                })
                .build(),
        )
        .setup(move |app| {
            // ---------- 数据目录与状态 ----------
            let data_dir = app
                .path()
                .app_data_dir()
                .unwrap_or_else(|_| std::path::PathBuf::from("."));

            // 建目录 → 初始化日志 → 数据迁移。
            //
            // 日志必须先于迁移：迁移过程本身要写日志（迁没迁、走了哪条路径、
            // 失败原因），否则排查"老数据为何没跟上来"时只能看到空文件。
            // 此前顺序相反，迁移日志全部丢失。
            //
            // 先建目录不会破坏迁移判断：`migrate_legacy_data_dir` 看的是
            // "新目录**有条目**"而非"目录存在"，空目录照样会触发迁移。
            std::fs::create_dir_all(&data_dir)?;
            init_file_logger(&data_dir);
            migrate_legacy_data_dir(&data_dir);

            let db_path = data_dir.join("userdata.db");
            let config_path = data_dir.join("Configs").join("ClipConfig.json");

            let store = Arc::new(Store::open(&db_path)?);
            let config = Arc::new(Mutex::new(ClipConfig::load(&config_path)));
            let clip_service = Arc::new(ClipService::new());

            // 首次运行时持久化默认配置，便于用户手工编辑
            if !config_path.exists() {
                let _ = config.lock().save(&config_path);
            }

            app.manage(AppState {
                store: store.clone(),
                config: config.clone(),
                clip_service: clip_service.clone(),
                config_path: config_path.clone(),
                launched_silently,
                // 固定状态是运行时的，每次启动都从"未固定"开始
                pinned: std::sync::atomic::AtomicBool::new(false),
                // 路线 B（永不激活）开关的软件侧镜像，启动时尚未启用
                no_activate: std::sync::atomic::AtomicBool::new(false),
            });

            // ---------- 开机自启状态同步 ----------
            // 以系统注册表实际状态为准，修正配置文件可能的偏差
            // （用户可能通过任务管理器禁用过启动项）
            {
                use tauri_plugin_autostart::ManagerExt;
                match app.autolaunch().is_enabled() {
                    Ok(actual) => {
                        let mut cfg = config.lock();
                        log::info!("开机自启当前状态: {actual}");
                        if cfg.auto_start != actual {
                            log::info!(
                                "配置值与实际不符（配置 {}），已同步",
                                cfg.auto_start
                            );
                            cfg.auto_start = actual;
                            let _ = cfg.save(&config_path);
                        }
                    }
                    Err(e) => log::warn!("查询开机自启状态失败: {e}"),
                }
            }

            // ---------- 剪贴板监听 ----------
            let handle = app.handle().clone();
            let on_change = Arc::new(move |_item: &quickclip_lib::config::ClipItem| {
                let _ = handle.emit("clip-added", &());
            });
            quickclip_lib::clipboard::spawn_watcher(
                clip_service.clone(),
                store.clone(),
                config.clone(),
                on_change,
            );

            // ---------- 前台窗口追踪 ----------
            // 安装事件钩子持续记录"用户最后在用的非自身窗口"，作为粘贴目标。
            // 必须在窗口显示之前完成，否则首次唤出可能还没有目标记录。
            #[cfg(windows)]
            {
                quickclip_lib::platform::install_foreground_tracker();
            }

            // ---------- 全局快捷键 ----------
            // 注册失败不影响启动，但会记录日志便于排查按键被占用的情况
            let hotkey_str = config.lock().hotkey.clone();
            if let Some(sc) = quickclip_lib::hotkey::parse(&hotkey_str) {
                match app.global_shortcut().register(sc) {
                    Ok(_) => log::info!("全局快捷键已注册: {hotkey_str}"),
                    Err(e) => log::warn!("全局快捷键「{hotkey_str}」注册失败: {e}"),
                }
            } else {
                log::warn!("快捷键「{hotkey_str}」格式无法解析，已跳过注册");
            }

            // ---------- 系统托盘 ----------
            let show_i = MenuItem::with_id(app, "show", "显示面板", true, None::<&str>)?;
            let clear_i = MenuItem::with_id(app, "clear", "清空历史", true, None::<&str>)?;
            let open_data_i =
                MenuItem::with_id(app, "open_data", "打开数据目录", true, None::<&str>)?;
            let sep = PredefinedMenuItem::separator(app)?;
            let quit_i = MenuItem::with_id(app, "quit", "退出 QuickClip", true, None::<&str>)?;
            let menu =
                Menu::with_items(app, &[&show_i, &clear_i, &open_data_i, &sep, &quit_i])?;

            // 托盘对象必须持有到应用结束。若绑定到 `_tray` 这类下划线变量，
            // 会在 setup 闭包返回时立刻被 drop，托盘图标与菜单一并消失。
            let tray = TrayIconBuilder::with_id("main-tray")
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("QuickClip - 剪切板管理器")
                .menu(&menu)
                // 左键单击不弹菜单，改为唤出面板；菜单仅在右键弹出
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => commands::show_panel(app),
                    "clear" => {
                        let state = app.state::<AppState>();
                        let _ = state.store.clear_clips();
                        state.store.cache.write().clips.clear();
                        let _ = app.emit("clips-changed", &());
                    }
                    "open_data" => {
                        let state = app.state::<AppState>();
                        if let Some(dir) = state.store.db_path.parent() {
                            let _ = commands::open_path(dir.to_string_lossy().to_string());
                        }
                    }
                    "quit" => {
                        app.state::<AppState>()
                            .clip_service
                            .running
                            .store(false, std::sync::atomic::Ordering::SeqCst);
                        app.exit(0);
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    // 仅在左键"抬起"时唤出面板，避免按下与抬起各触发一次
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        commands::show_panel(tray.app_handle());
                    }
                })
                .build(app)?;

            // 交给 Tauri 托管，防止 setup 结束后被回收
            app.manage(tray);

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::bootstrap,
            commands::save_config,
            commands::list_clips,
            commands::copy_clip,
            commands::delete_clips,
            commands::toggle_top,
            commands::list_favors,
            commands::add_favor,
            commands::delete_favor,
            commands::open_path,
            commands::toggle_panel,
            commands::hide_window,
            commands::paste_to_active,
            commands::open_data_dir,
            commands::rebind_hotkey,
            commands::get_autostart,
            commands::set_autostart,
            commands::get_pinned,
            commands::set_pinned,
            commands::set_search_focusable,
            commands::poll_hide_request,
        ])
        .on_window_event(|window, event| {
            // 点击关闭按钮时隐藏而非退出，符合常驻托盘的交互预期
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .build(tauri::generate_context!())
        .expect("QuickClip 构建失败");

    // 用 run + RunEvent::Ready 而非 .run()：
    // 窗口在 setup 阶段尚未完成创建，此时调用 hide() 会落在空处。
    // Ready 事件标志窗口与事件循环已就绪，是控制窗口显示时机的可靠节点。
    app.run(|app_handle, event| {
        if let RunEvent::Ready = event {
            let state = app_handle.state::<AppState>();
            let cfg = state.config.lock().clone();

            if let Some(win) = app_handle.get_webview_window("main") {
                // 启动时一律从不固定状态开始，显式复位置顶，
                // 避免窗口继承上一轮运行或系统记忆的 topmost 状态。
                let _ = win.set_always_on_top(false);

                // 静默启动**只认**命令行里的 `--silent`——该参数由本程序写入
                // 开机自启启动项，只有开机自启才会带上。
                //
                // 为什么不再叠加"父进程是不是 explorer.exe"这类判断：
                // 用户手动双击图标时父进程**同样是 explorer.exe**，
                // 两者无法靠父进程区分。曾经据此做过兜底，结果是
                // `startMinimized` 默认为 true 时手动启动也被静默掉，
                // 正是用户反馈的"手动打开不弹窗口"。
                //
                // 现在的语义简单明确：
                //   带 --silent → 开机自启 → 静默驻留托盘
                //   不带       → 手动启动 → 必定先显示窗口
                // 用户若希望开机自启时也弹窗，把设置里的
                // 「开机自启时静默到托盘」关掉即可（见下）。
                let silent = state.launched_silently && cfg.start_minimized;

                if silent {
                    let _ = win.hide();
                    log::info!("静默启动：面板保持隐藏（开机自启 + 用户开启静默）");
                } else {
                    let _ = win.show();
                    let _ = win.set_focus();
                    log::info!("正常启动：面板已显示");
                }
            }
        }
    });
}

/// 从旧版本的数据目录迁移数据。
///
/// 应用曾用标识符 `com.clipforge.app`，改名后变成了 `com.quickclip.app`，
/// 而 Tauri 的数据目录由标识符决定——不迁移的话用户会"丢失"全部历史与配置。
///
/// 迁移策略（保守优先，宁可不动也不误删）：
/// - 仅在**新目录不存在或为空**时迁移，避免覆盖用户改名后产生的新数据；
/// - 优先**整体重命名**（同盘符下是原子操作，快且不留副本）；
/// - 重命名失败（跨盘符、被占用等）时退化为**递归复制**，且**不删除旧目录**，
///   留作人工兜底。
fn migrate_legacy_data_dir(new_dir: &std::path::Path) {
    const LEGACY_ID: &str = "com.clipforge.app";

    // 定位旧目录：与当前数据目录同级，即 %APPDATA%\<标识符>
    let Some(parent) = new_dir.parent() else { return };
    let legacy_dir = parent.join(LEGACY_ID);

    if !legacy_dir.is_dir() {
        return;
    }
    // 旧目录存在，但新目录已有内容 → 用户已在用新版，不打扰
    if new_dir.exists() && dir_has_entries(new_dir) {
        log::info!("检测到旧数据目录 {LEGACY_ID}，但新目录已有数据，跳过迁移");
        return;
    }

    log::info!(
        "检测到旧数据目录，开始迁移：{} → {}",
        legacy_dir.display(),
        new_dir.display()
    );

    // 优先整体重命名（最干净，不留冗余副本）
    match std::fs::rename(&legacy_dir, new_dir) {
        Ok(_) => {
            log::info!("迁移完成（目录重命名）");
            return;
        }
        Err(e) => {
            log::warn!("目录重命名失败（{e}），退化为复制方式");
        }
    }

    // 退化路径：递归复制，失败则保持旧目录原样不动
    match copy_dir_recursive(&legacy_dir, new_dir) {
        Ok(_) => log::info!(
            "迁移完成（复制方式）。旧目录已保留：{}，确认无误后可手动删除",
            legacy_dir.display()
        ),
        Err(e) => log::error!("数据迁移失败：{e}。将按全新安装继续，旧数据未受影响"),
    }
}

/// 判断目录是否含有任何条目（含隐藏项）。
fn dir_has_entries(dir: &std::path::Path) -> bool {
    std::fs::read_dir(dir)
        .map(|mut it| it.next().is_some())
        .unwrap_or(false)
}

/// 递归复制目录内容。目标目录会被创建。
fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let target = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// 初始化文件日志。
///
/// 日志量很小（仅在启动与关键状态变更时记录），采用同步追加写即可，
/// 无需引入异步日志框架。
fn init_file_logger(data_dir: &std::path::Path) {
    use std::io::Write;

    let log_path = data_dir.join("quickclip.log");
    // 截断旧日志，只保留本次运行记录
    if let Ok(mut f) = std::fs::File::create(&log_path) {
        let _ = writeln!(
            f,
            "=== QuickClip 启动 {} ===",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
        );
        let _ = writeln!(f, "命令行参数: {:?}", std::env::args().collect::<Vec<_>>());
        let _ = f.flush();
    }

    struct FileWriter {
        path: std::path::PathBuf,
    }
    impl log::Log for FileWriter {
        fn enabled(&self, _: &log::Metadata) -> bool {
            true
        }
        fn log(&self, record: &log::Record) {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
            {
                let _ = writeln!(
                    f,
                    "[{}] [{}] {}",
                    chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
                    record.level(),
                    record.args()
                );
            }
        }
        fn flush(&self) {}
    }

    // set_boxed_logger 只允许调用一次；失败说明已有 logger，属预期情况
    if log::set_boxed_logger(Box::new(FileWriter {
        path: log_path.clone(),
    }))
    .is_ok()
    {
        log::set_max_level(log::LevelFilter::Info);
    }
}
