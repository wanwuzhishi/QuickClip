//! 全局鼠标钩子：实现「点击面板外部自动隐藏」。
//!
//! ## 为什么必须要它
//!
//! 面板采用「永不激活」（`WS_EX_NOACTIVATE`）后，它**不会成为前台窗口**，
//! 因此系统永远不会给它发 `WM_KILLFOCUS` / `WM_ACTIVATE` —— 常规的
//! 「失焦即隐藏」在它身上失效。要保留「点别处就收起」这个符合直觉的行为，
//! 只能自己装一个全局低级鼠标钩子，判断点击落点是否在面板之外。
//!
//! ## 实现要点
//!
//! - `WH_MOUSE_LL` 是**低级**钩子，回调在安装它的线程的消息循环中执行，
//!   不需要注入其他进程（这也是它相对 `WH_MOUSE` 的优势：不会被杀软拦）。
//! - 因此必须有一个**持续运转的消息循环**，否则回调永远不触发。
//!   这里专门起一个后台线程 `GetMessage` 泵来承担。
//! - 回调在系统鼠标输入的关键路径上，**必须足够快**。所以回调里只做
//!   「取坐标 → 判断落点 → 用轻量方式通知主线程」，不做任何窗口操作。
//! - 回调必须调用 `CallNextHookEx` 把事件传下去，否则会吞掉全局鼠标输入。

#![cfg(windows)]

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

/// 主窗口句柄（裸地址），供钩子回调判断点击落点。
///
/// 存成 `isize` 而非 `HWND`：跨线程共享要求 `Send`，而 `HWND` 是裸指针
/// 包装、不满足；裸地址传输最简单，回调里再还原。
static PANEL_HWND: AtomicIsize = AtomicIsize::new(0);

/// 已安装的钩子句柄。非 0 即表示已装，避免重复安装。
static HOOK: AtomicIsize = AtomicIsize::new(0);

/// 是否启用了"永不激活"模式。
///
/// 钩子常驻不卸载，但在未启用该模式时（用户关了设置）不应拦截点击——
/// 那时窗口是普通可激活窗口，失焦隐藏由前端 `blur` 处理，钩子不该插手。
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// 「点击了面板外部」的待处理标志。
///
/// 钩子线程不能直接调 Tauri API（那些要求在主线程），所以这里置一个标志，
/// 由前端定时轮询或下次交互时取用。用最朴素的原子标志而非事件通道，
/// 是因为这个动作极其简单（"该隐藏了"），不值得引入队列。
static HIDE_REQUESTED: AtomicBool = AtomicBool::new(false);

/// 记录面板句柄并启用钩子逻辑（幂等安装钩子）。
pub fn ensure_installed(panel_hwnd: isize) {
    if panel_hwnd != 0 {
        PANEL_HWND.store(panel_hwnd, Ordering::SeqCst);
    }
    ACTIVE.store(true, Ordering::SeqCst);

    if HOOK.load(Ordering::SeqCst) != 0 {
        return; // 已安装
    }

    std::thread::spawn(move || {
        let hook = crate::platform::install_mouse_hook(mouse_proc);
        if hook == 0 {
            log::warn!("点窗外隐藏：鼠标钩子安装失败，该行为将不可用");
            return;
        }
        HOOK.store(hook, Ordering::SeqCst);
        log::info!("点窗外隐藏：全局鼠标钩子已安装");

        // 消息循环必须持续运转，否则 hook 回调不会被调度。
        unsafe {
            use windows::Win32::UI::WindowsAndMessaging::{
                DispatchMessageW, GetMessageW, TranslateMessage, MSG,
            };

            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                let _ = DispatchMessageW(&msg);
            }
        }
    });
}

/// 关闭钩子逻辑（用户关掉"永不激活"设置时调用）。
///
/// 只关标志、不卸载钩子：重新启用时省一次安装，且卸载钩子需要
/// 在钩子线程内做（`UnhookWindowsHookEx` 有线程亲和性），成本不值当。
pub fn set_active(active: bool) {
    ACTIVE.store(active, Ordering::SeqCst);
}

/// 取走并清空"请求隐藏"标志。由主线程（Tauri 命令）调用。
pub fn take_hide_request() -> bool {
    HIDE_REQUESTED.swap(false, Ordering::SeqCst)
}

/// 钩子回调：左键按下时判断是否点在面板之外。
unsafe extern "system" fn mouse_proc(code: i32, wparam: usize, lparam: isize) -> isize {
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::CallNextHookEx;

    // code < 0 时必须直接放行，这是 Win32 钩子的约定
    if code >= 0
        && ACTIVE.load(Ordering::Relaxed)
        && wparam as u32 == crate::platform::WM_LBUTTONDOWN
    {
        if let Some((x, y)) = point_of(lparam) {
            let raw = PANEL_HWND.load(Ordering::Relaxed);
            if raw != 0 {
                let hwnd = windows::Win32::Foundation::HWND(raw as *mut std::ffi::c_void);
                // 三点都很快：句柄有效性、可见性、矩形包含，均为内存或单次系统调用
                if crate::platform::is_window(hwnd)
                    && crate::platform::is_visible(hwnd)
                    && !crate::platform::rect_contains(hwnd, x, y)
                {
                    HIDE_REQUESTED.store(true, Ordering::SeqCst);
                }
            }
        }
    }
    // 0.58 的签名要求 WPARAM / LPARAM 包装类型，返回 LRESULT
    CallNextHookEx(None, code, WPARAM(wparam), LPARAM(lparam)).0
}

/// 从 `MSLLHOOKSTRUCT` 取出鼠标坐标（物理像素，屏幕坐标）。
///
/// 结构体定义：`{ POINT pt; DWORD mouseData; DWORD flags; DWORD time; ULONG_PTR extra; }`，
/// 前两个 `i32` 就是 `pt.x` / `pt.y`。
unsafe fn point_of(lparam: isize) -> Option<(i32, i32)> {
    if lparam == 0 {
        return None;
    }
    let p = lparam as *const i32;
    Some((*p, *p.add(1)))
}
