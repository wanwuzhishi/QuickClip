//! Windows 平台原生能力封装：前台窗口探测、模拟粘贴、剪贴板序列号。
//!
//! 这里集中处理最容易出问题的三类系统交互，隔离 unsafe 边界：
//! 1. 获取当前前台窗口所属进程（用于记录来源应用、过滤黑名单）
//! 2. 向目标窗口发送 Ctrl+V（"无焦点粘贴"的关键）
//! 3. 读取剪贴板序列号，作为"是否被外部修改"的判据

#![cfg(windows)]

use windows::Win32::Foundation::{HWND, MAX_PATH};
use windows::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};
use std::sync::atomic::{AtomicIsize, Ordering};

/// 目标窗口可能已被销毁（用户关掉了它），此时任何操作都不该继续。
pub fn is_window(hwnd: HWND) -> bool {
    hwnd.0 as isize != 0
        && unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(hwnd).as_bool() }
}

/// 读取剪贴板序列号。序列号变化即代表剪贴板内容被改写。
pub fn clipboard_sequence_number() -> u32 {
    use windows::Win32::System::DataExchange::{GetClipboardSequenceNumber,};
    unsafe { GetClipboardSequenceNumber() }
}

/// 获取前台窗口句柄。
pub fn foreground_window() -> HWND {
    unsafe { GetForegroundWindow() }
}

/// 取得指定窗口所属进程的可执行文件名（不含路径）。
pub fn process_name_of_window(hwnd: HWND) -> Option<String> {
    if hwnd.is_invalid() {
        return None;
    }
    let mut pid = 0u32;
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
    }
    if pid == 0 {
        return None;
    }
    process_name_by_pid(pid)
}

/// 按 PID 取可执行文件名。
pub fn process_name_by_pid(pid: u32) -> Option<String> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; MAX_PATH as usize];
        let mut size = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut size,
        );
        let _ = windows::Win32::Foundation::CloseHandle(handle);
        if ok.is_err() {
            return None;
        }
        let full = String::from_utf16_lossy(&buf[..size as usize]);
        full.rsplit('\\').next().map(|s| s.to_string())
    }
}

/// 取得鼠标当前的屏幕坐标（左上角为原点，单位物理像素）。
///
/// 用途：面板默认跟随鼠标位置弹出（"就近出现"），
/// 这是剪贴板工具的常见交互——用户的注意力本就在鼠标处。
///
/// 注意：返回的是**物理像素**，与 Tauri 的 `PhysicalPosition` 一致，
/// 无需再做 DPI 换算。
pub fn cursor_position() -> Option<(i32, i32)> {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

    let mut pt = POINT::default();
    unsafe { GetCursorPos(&mut pt).ok()?; }
    Some((pt.x, pt.y))
}

/// 取得鼠标所在显示器的**工作区**尺寸（已排除任务栏）。
///
/// 用途：把面板摆在鼠标附近时，需保证不会越出屏幕。
/// 直接钳到工作区而不是整屏，是因为贴边弹出时下半截会被任务栏盖住。
pub fn work_area_at_cursor() -> Option<(i32, i32, i32, i32)> {
    use windows::Win32::Foundation::{POINT, RECT};
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

    let mut pt = POINT::default();
    unsafe { GetCursorPos(&mut pt).ok()?; }

    unsafe {
        let monitor = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        if monitor.is_invalid() {
            return None;
        }
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(monitor, &mut info).as_bool() {
            return None;
        }
        let RECT { left, top, right, bottom } = info.rcWork;
        Some((left, top, right, bottom))
    }
}

/// 当前进程 ID。
pub fn current_pid() -> u32 {
    unsafe { GetCurrentProcessId() }
}

/// 判断指定窗口是否属于本进程。
///
/// 用途：唤出面板前记录"目标窗口"时，若前台窗口其实是我们自己的窗口，
/// 就不能把它记为粘贴目标，否则粘贴会发给自身。
pub fn window_belongs_to_current_process(hwnd: HWND) -> bool {
    if hwnd.is_invalid() {
        return true; // 无效句柄一律视为"不应记录"
    }
    let mut pid = 0u32;
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
    }
    pid == current_pid()
}

/// 向前台窗口发送 Ctrl+V 组合键，实现"选中即粘贴"。
///
/// 用 `SendInput` 而非 `keybd_event`：前者可一次提交完整的按键序列，
/// 且能正确携带扫描码信息，对 Electron / WPF / UWP 等对 `WM_KEYDOWN`
/// 的 `lParam` 较真的程序兼容性更好。
///
/// 修饰键与主键之间留出间隔，避免目标程序来不及识别组合键。
pub fn send_paste() {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS,
        KEYEVENTF_KEYUP, VIRTUAL_KEY,
    };

    /// 构造一个键盘输入事件。
    fn key(vk: u16, up: bool) -> INPUT {
        let flags = if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) };
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(vk),
                    wScan: 0,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    unsafe {
        const VK_CONTROL: u16 = 0x11;
        const VK_V: u16 = 0x56;

        // 序列：Ctrl↓ V↓ V↑ Ctrl↑，一次提交
        let seq = [
            key(VK_CONTROL, false),
            key(VK_V, false),
            key(VK_V, true),
            key(VK_CONTROL, true),
        ];
        let sent = SendInput(&seq, std::mem::size_of::<INPUT>() as i32);
        if sent != seq.len() as u32 {
            log::warn!("SendInput 投递不完整: {sent}/{}", seq.len());
        }
    }

    // 组合键的按下与抬起之间需要极短的间隔，
    // 否则部分程序（尤其是游戏与终端类）会漏掉这次组合。
    std::thread::sleep(std::time::Duration::from_millis(15));
}

/// 发送单次按键（如回车）。
pub fn send_key(vk: u16) {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
        VIRTUAL_KEY,
    };

    fn ev(vk: u16, up: bool) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(vk),
                    wScan: 0,
                    dwFlags: if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) },
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }
    unsafe {
        let seq = [ev(vk, false), ev(vk, true)];
        let _ = SendInput(&seq, std::mem::size_of::<INPUT>() as i32);
    }
}

/// 全局记录"最近一个非自身的前台窗口"，作为粘贴目标的权威来源。
///
/// **为什么不在唤出面板时才抓取**：`show_panel` 执行时，前台窗口可能
/// 已经变成面板自己（例如面板刚被鼠标点过、或上一次隐藏尚未生效），
/// 此时抓到的目标就是错的，粘贴自然无从谈起。
///
/// **正确做法**（业界成熟方案，EcoPaste 等采用）：安装 `SetWinEventHook`
/// 监听 `EVENT_SYSTEM_FOREGROUND`，只要前台窗口发生变化就更新记录，
/// 并**跳过自身窗口**。这样无论面板何时被唤出，记录里的目标都是
/// "用户最后一次真正在用的那个窗口"，不依赖任何调用时机。
static LAST_FOREIGN_FOREGROUND: AtomicIsize = AtomicIsize::new(0);

/// 取最近一次记录的非自身前台窗口。
pub fn last_foreign_foreground() -> HWND {
    HWND(LAST_FOREIGN_FOREGROUND.load(Ordering::SeqCst) as *mut std::ffi::c_void)
}

/// 手动设置记录（供测试或特殊场景覆盖）。
pub fn set_last_foreign_foreground(hwnd: HWND) {
    LAST_FOREIGN_FOREGROUND.store(hwnd.0 as isize, Ordering::SeqCst);
}

/// WinEvent 回调：前台窗口一变就记录（自身窗口除外）。
unsafe extern "system" fn foreground_hook_proc(
    _hook: windows::Win32::UI::Accessibility::HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    _id_object: i32,
    _id_child: i32,
    _thread: u32,
    _time: u32,
) {
    use windows::Win32::UI::WindowsAndMessaging::EVENT_SYSTEM_FOREGROUND;

    if event != EVENT_SYSTEM_FOREGROUND {
        return;
    }
    if hwnd.0.is_null() || !is_window(hwnd) {
        return;
    }
    // 自身窗口（含面板与其子窗口）绝不作为粘贴目标
    if window_belongs_to_current_process(hwnd) {
        return;
    }
    // 只记录有标题的顶层窗口，过滤掉工具提示、输入法等辅助窗口
    if !is_real_top_level(hwnd) {
        return;
    }
    LAST_FOREIGN_FOREGROUND.store(hwnd.0 as isize, Ordering::SeqCst);
}

/// 判断窗口是否是"真正的顶层应用窗口"。
///
/// 事件钩子会收到大量辅助窗口（输入法候选、工具提示、拖拽幽灵窗等），
/// 把它们记成粘贴目标会导致按键投递给不存在的地方。筛选条件：
/// - 有 `WS_EX_APPWINDOW` 或父窗口为空（顶层）
/// - 不可见 / 无标题的一律排除
fn is_real_top_level(hwnd: HWND) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetAncestor, GetWindowLongPtrW, GetWindowTextLengthW, IsWindowVisible, GA_ROOT,
        GWL_EXSTYLE, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    };

    unsafe {
        if !IsWindowVisible(hwnd).as_bool() {
            return false;
        }
        // 有标题才算真实应用窗口
        if GetWindowTextLengthW(hwnd) == 0 {
            return false;
        }
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        // 工具窗口 / 不可激活窗口通常是辅助 UI，不作目标
        if ex & WS_EX_TOOLWINDOW.0 != 0 || ex & WS_EX_NOACTIVATE.0 != 0 {
            return false;
        }
        // 必须是顶层（自身的 root 就是自己）
        GetAncestor(hwnd, GA_ROOT) == hwnd
    }
}

/// 已安装的钩子句柄。存起来是为了语义明确：钩子在该进程内长期有效，
/// 不需要（也不应）手动释放——进程退出时系统统一回收。
static FOREGROUND_HOOK: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

/// 启动前台窗口追踪钩子。只需调用一次，钩子在整个进程生命周期内有效。
///
/// 返回 false 表示钩子安装失败（此时会退化为"唤出时临时抓取"的旧逻辑）。
pub fn install_foreground_tracker() -> bool {
    use windows::Win32::UI::Accessibility::SetWinEventHook;
    use windows::Win32::UI::WindowsAndMessaging::{
        EVENT_SYSTEM_FOREGROUND, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS,
    };

    // 避免重复安装
    if FOREGROUND_HOOK.load(Ordering::SeqCst) != 0 {
        return true;
    }

    unsafe {
        let hook = SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            None,
            Some(foreground_hook_proc),
            0,
            0,
            // OUTOFCONTEXT：回调在调用线程的消息循环中执行，无需注入目标进程，
            // 稳定且不会被杀软拦截。SKIPOWNPROCESS 让系统直接过滤掉自身窗口。
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        );
        if hook.is_invalid() {
            log::warn!("前台窗口追踪钩子安装失败，将退化为唤出时抓取");
            return false;
        }
        FOREGROUND_HOOK.store(hook.0 as isize, Ordering::SeqCst);
        log::info!("前台窗口追踪钩子已安装");
        true
    }
}

/// 把指定窗口置为前台并赋予输入焦点。返回是否成功。
///
/// Windows 有一条硬性限制：**只有当前持有前台焦点的进程，才有资格把前台
/// 转交给别的窗口**。而我们的面板为了接收用户按键，恰恰抢走了前台焦点——
/// 于是单个 `SetForegroundWindow` 会被系统静默忽略，表现为"点了没反应"。
///
/// 下面按可靠性从高到低依次尝试，任一成功即返回：
///
/// 1. **`AttachThreadInput` 挂接输入队列**：把本线程与目标线程的输入队列
///    临时合并后，本线程便"成为"前台线程，再调用 `SetForegroundWindow`
///    即被系统接受。这是社区公认最可靠的绕行方案。
/// 2. **先最小化再还原**：`ShowWindow(SW_MINIMIZE)` + `SW_RESTORE` 会让
///    窗口管理器重新走一次激活流程，绕过前台锁。
/// 3. **`BringWindowToTop` + `SetForegroundWindow`**：最后的兜底。
///
/// 每次尝试之间都重新读取当前前台窗口——一旦已经切换成功就没必要继续，
/// 也能避免多余的窗口动画干扰用户。
pub fn force_foreground(hwnd: HWND) -> bool {
    use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    use windows::Win32::UI::WindowsAndMessaging::{
        BringWindowToTop, GetWindowThreadProcessId, IsIconic, SetForegroundWindow, ShowWindow,
        SW_MINIMIZE, SW_RESTORE,
    };

    if !is_window(hwnd) {
        return false;
    }
    if unsafe { GetForegroundWindow() } == hwnd {
        return true;
    }

    unsafe {
        let target_thread = GetWindowThreadProcessId(hwnd, None);
        let cur_thread = GetCurrentThreadId();

        // ---- 策略 1：挂接输入队列 ----
        let attached = target_thread != 0
            && target_thread != cur_thread
            && AttachThreadInput(cur_thread, target_thread, true).as_bool();

        let _ = SetForegroundWindow(hwnd);
        if attached {
            let _ = AttachThreadInput(cur_thread, target_thread, false);
        }

        if GetForegroundWindow() == hwnd {
            return true;
        }

        // ---- 策略 2：最小化再还原，强制走一次激活流程 ----
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        } else {
            let _ = ShowWindow(hwnd, SW_MINIMIZE);
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        let _ = SetForegroundWindow(hwnd);

        if GetForegroundWindow() == hwnd {
            return true;
        }

        // ---- 策略 3：兜底 ----
        let _ = BringWindowToTop(hwnd);
        let _ = SetForegroundWindow(hwnd);

        GetForegroundWindow() == hwnd
    }
}

/// 把窗口显示出来，但**不夺取前台焦点**（show without activate）。
///
/// 这是解决"粘贴到输入框时输入框被关闭"的关键。
///
/// **问题机制**：许多目标程序的输入框是"失焦即关闭"的——浏览器地址栏联想、
/// 页面内搜索条、编辑器命令面板、聊天框 @ 提及浮层等，一旦失去前台焦点就
/// 立刻销毁。而面板为接收用户按键通常要 `set_focus()` 抢焦点，于是：
/// 输入框关闭 → 面板粘贴时把焦点还回去，但输入框已经不存在了 → Ctrl+V
/// 落在窗口本体上，用户看到"输入框没了，也没粘贴上"。
///
/// **解法**：用 `SetWindowPos` + `SWP_NOACTIVATE` 显示窗口而不激活。
/// 目标输入框全程保持焦点，粘贴时只需投递按键即可，无需任何焦点转交。
///
/// `topmost` 决定是否**同时置顶**：
/// - 固定（置顶常驻）模式传 `true`，面板保持在最上层，便于连续粘贴；
/// - 普通模式传 `false`，只把窗口显示出来，**绝不改变 Z 序**——
///   否则会出现"用户没开固定，窗口却被置顶"的状态错乱，
///   并让 `is_topmost()` 的返回值与 `pinned` 标志脱节。
///
/// `SWP_SHOWWINDOW` 让窗口同时完成"显示"动作，因此调用方无需再调 `show()`。
///
/// 注意：`HWND_TOPMOST` / `HWND_NOTOPMOST` 是**插入位置**参数，
/// 配合 `SWP_NOACTIVATE` 使用只改变 Z 序与可见性，不触碰焦点。
pub fn show_no_activate(hwnd: HWND, topmost: bool) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, HWND_NOTOPMOST, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
        SWP_SHOWWINDOW,
    };

    if !is_window(hwnd) {
        return false;
    }
    let after = if topmost { HWND_TOPMOST } else { HWND_NOTOPMOST };
    unsafe {
        SetWindowPos(
            hwnd,
            after,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
        )
        .is_ok()
    }
}

/// 强制取消窗口的"最上层"属性。
///
/// 必须用 `SetWindowPos(HWND_NOTOPMOST)` 而非只依赖框架的 `set_always_on_top(false)`：
/// Windows 对已置顶的窗口有"顶层窗口停留"行为，并且**以管理员身份运行**时，
/// 其他普通权限进程无法改变本窗口的 Z 序——只有来自本进程内部的显式调用
/// 才可靠地把窗口降回普通层。
///
/// `SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE` 保证只动 Z 序，
/// 不移动、不缩放、不抢焦点。
pub fn clear_topmost(hwnd: HWND) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, HWND_NOTOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    };

    if !is_window(hwnd) {
        return false;
    }
    unsafe {
        // SetWindowPos 传的是"插入位置"的裸 HWND，返回 Result 而非 BOOL
        SetWindowPos(
            hwnd,
            HWND_NOTOPMOST,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        )
        .is_ok()
    }
}

/// 设置窗口的"最上层"属性（显式版本）。
///
/// 与 `clear_topmost` 对称：置顶有时也需要绕过框架层直接操作，
/// 例如窗口曾被外部程序改变过 Z 序，框架的内部状态已与系统脱节。
pub fn set_topmost(hwnd: HWND, topmost: bool) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, HWND_NOTOPMOST, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    };

    if !is_window(hwnd) {
        return false;
    }
    let after = if topmost { HWND_TOPMOST } else { HWND_NOTOPMOST };
    unsafe {
        SetWindowPos(
            hwnd,
            after,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        )
        .is_ok()
    }
}

/// 查询窗口当前是否处于"最上层"（`WS_EX_TOPMOST`）。
///
/// 这是判断置顶状态的**唯一权威来源**：框架层的状态标记可能与系统实际
/// 情况脱节（尤其是被外部程序或权限差异干扰后），直接读扩展样式最可靠。
pub fn is_topmost(hwnd: HWND) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{GetWindowLongPtrW, GWL_EXSTYLE, WS_EX_TOPMOST};

    if !is_window(hwnd) {
        return false;
    }
    unsafe {
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        ex & WS_EX_TOPMOST.0 != 0
    }
}

// ==================== 路线 B：窗口永不激活（WS_EX_NOACTIVATE） ====================
//
// 目标：面板从显示到隐藏全程不夺取前台焦点，从根本上消除"唤出面板
// 导致目标程序输入框失焦关闭"的问题。
//
// 与路线 A 的 `SWP_NOACTIVATE` 区别：后者只让**某一次 SetWindowPos**
// 不激活窗口，窗口本身仍可被点击激活；本路线直接给窗口加上
// `WS_EX_NOACTIVATE` 扩展样式，使其**永久**不参与激活流程。
//
// 代价与配套（缺一即不可用）：
//   1. 搜索框永远拿不到键盘焦点 → 需要"临时可聚焦"（见 set_focusable）
//   2. 窗口不激活 → 收不到 WM_KILLFOCUS → "点窗外隐藏"需自己实现（鼠标钩子）
//   3. 拖动、IME 等输入路径需另行验证

/// 给窗口加上 / 去掉 `WS_EX_NOACTIVATE` 扩展样式。
///
/// `no_activate = true`  → 窗口永不激活（面板常态）
/// `no_activate = false` → 恢复普通可激活窗口（临时，供搜索框输入）
///
/// 实现上用 `GetWindowLongPtrW` / `SetWindowLongPtrW` 直接改扩展样式，
/// 改完必须调用 `SetWindowPos(..., SWP_FRAMECHANGED | SWP_NOACTIVATE)`
/// 让系统重新评估窗口的非客户区——否则样式虽然写进去了，
/// 窗口管理器仍按旧样式行事，表现为"改了没效果"。
pub fn set_no_activate(hwnd: HWND, no_activate: bool) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, SetWindowLongPtrW, SetWindowPos, GWL_EXSTYLE, SWP_FRAMECHANGED,
        SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, WS_EX_NOACTIVATE,
    };

    if !is_window(hwnd) {
        return false;
    }
    unsafe {
        let cur = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        let want = if no_activate {
            cur | WS_EX_NOACTIVATE.0
        } else {
            cur & !WS_EX_NOACTIVATE.0
        };
        if cur == want {
            return true; // 已是目标状态，省掉一次内核调用
        }
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, want as isize);
        // 样式变更必须配合 FRAMECHANGED 才会被窗口管理器采纳。
        // SWP_NOACTIVATE 保证这次刷新本身不会把窗口激活起来。
        SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        )
        .is_ok()
    }
}

/// 查询窗口当前是否带 `WS_EX_NOACTIVATE`。
pub fn is_no_activate(hwnd: HWND) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, GWL_EXSTYLE, WS_EX_NOACTIVATE,
    };
    if !is_window(hwnd) {
        return false;
    }
    unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_NOACTIVATE.0 != 0 }
}

/// 窗口当前是否可见。
pub fn is_visible(hwnd: HWND) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::IsWindowVisible;
    if !is_window(hwnd) {
        return false;
    }
    unsafe { IsWindowVisible(hwnd).as_bool() }
}

/// 判断屏幕坐标点（物理像素）是否落在窗口矩形内。
///
/// 用途：全局鼠标钩子里判断"这一击是否点在面板之外"。
/// 用 `GetWindowRect`（含边框的整个窗口矩形）而非客户区：
/// 用户的直觉是"点在窗口这块地方里 / 外"，边框也应算在内。
pub fn rect_contains(hwnd: HWND, x: i32, y: i32) -> bool {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;

    if !is_window(hwnd) {
        return false;
    }
    let mut r = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut r) }.is_err() {
        return false;
    }
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

/// 全局鼠标钩子类型：左键按下。
///
/// 用途：`WS_EX_NOACTIVATE` 的窗口不会成为前台窗口，因此**收不到
/// `WM_KILLFOCUS` / `WM_ACTIVATE`**，"点击窗口外部自动隐藏"这类逻辑
/// 无法靠窗口消息实现，必须用全局钩子自己判断点击落点。
pub const WM_LBUTTONDOWN: u32 = 0x0201;

/// 安装全局低级鼠标钩子。
///
/// `callback` 会在**钩子线程**的消息循环中被调用，因此：
/// - 必须有一个持续运转的消息循环（`GetMessage` 泵），否则回调永不触发
/// - 回调里不能做耗时操作，否则整个系统的鼠标输入都会被拖慢
///
/// 返回钩子句柄；0 表示安装失败。
pub fn install_mouse_hook(callback: unsafe extern "system" fn(i32, usize, isize) -> isize) -> isize {
    use windows::Win32::UI::WindowsAndMessaging::{
        SetWindowsHookExW, WH_MOUSE_LL,
    };

    unsafe {
        // 0.58 的签名：idhook 是 WINDOWS_HOOK_ID 类型（非 i32），返回 Result<HHOOK>
        match SetWindowsHookExW(WH_MOUSE_LL, Some(std::mem::transmute(callback)), None, 0) {
            Ok(hook) => hook.0 as isize,
            Err(e) => {
                log::warn!("全局鼠标钩子安装失败: {e}");
                0
            }
        }
    }
}
