# 剪贴板工具的「抢焦点导致粘贴失败」业界解法调研

调研时间：2026-10-11
调研对象：EcoPaste、mosheng1/QuickClipboard、Edge-Drop、hopy、MindfulClipboard、Smart-Clipboard

## 问题定义

Windows 有条硬规则：**只有当前持有前台焦点的进程，才有资格把前台转交给别的窗口**
（`SetForegroundWindow` 的隐式约束）。剪贴板面板为了接收键盘输入（搜索、方向键）
必然要抢焦点，于是：

1. 面板抢走焦点 → 目标窗口失去焦点
2. 用户点条目 → 面板想把焦点还回去 → **但系统可能静默拒绝**
3. `SendInput` 投递的 Ctrl+V 打在焦点悬空的窗口上 → 用户看到「点了没反应」

## 路线 A：抢回焦点（记录目标 → SetForegroundWindow → SendInput）

### 代表实现

**EcoPaste 早期版本**（issue #249）
社区成员 ChurchTao 给出的思路，被作者确认为最终方案：

> 在应用 `beforeOnShow` 时记录当前活动窗口句柄（前活跃窗口），
> 粘贴时对这个句柄调用 `SetForegroundWindow`。它的优先级很高，
> **无论窗口是否被 pin 住都能前置**，随后 `SendInput` 就会对它生效。

**mosheng1/QuickClipboard**（Tauri 2 项目）
`src-tauri/src/services/system/focus.rs`

```rust
static LAST_FOCUS_HWND: AtomicIsize = AtomicIsize::new(0);
static LAST_FOCUS_MUTEX: Mutex<()> = Mutex::new(());

// 在显示剪贴板窗口前调用
fn save_current_focus() { /* GetForegroundWindow() → LAST_FOCUS_HWND */ }

// 在隐藏剪贴板窗口时调用
fn restore_last_focus() { /* SetForegroundWindow(LAST_FOCUS_HWND)，然后清空 */ }

// 确保剪贴板窗口自己拿到焦点；若当前前台不是它，则先存下上一个
fn focus_clipboard_window() { /* ... */ }
```

**本项目（QuickClip）**
在路线 A 基础上做了两点加强：

| 加强点 | 说明 |
|---|---|
| 目标窗口用**事件钩子**持续追踪 | `SetWinEventHook(EVENT_SYSTEM_FOREGROUND, WINEVENT_OUTOFCONTEXT \| WINEVENT_SKIPOWNPROCESS)`，而非在 `show_panel` 里抓一次。后者时机不可靠——面板可能已是前台 |
| 焦点交接**校验后才投递** | `force_foreground` 三级策略（`AttachThreadInput` → `SW_MINIMIZE`+`SW_RESTORE` → `BringWindowToTop`），最多 3 轮重试，确认 `foreground_window() == target` 才 `SendInput`。宁可粘贴失败也不误输入到别的窗口 |

### 优点 / 缺点

- ✅ 窗口可以正常持有焦点 → 搜索框、方向键、数字键全部自然可用
- ✅ 不引入全局鼠标钩子
- ❌ 焦点交接依赖系统配合，极端情况下会被拒绝
- ❌ 提权边界（UIPI）无法跨越：管理员程序 ↔ 普通程序互不可抢

## 路线 B：窗口根本不拿焦点（WS_EX_NOACTIVATE）

### 代表实现

**EcoPaste 1.0**（2026-07 重构）
`src-tauri/src/window/windows.rs`

```rust
pub fn show_window(app_handle: &AppHandle, label: &str) -> Result<()> {
    let window = get_window(app_handle, label)?;
    if label == CLIPBOARD_WINDOW_LABEL {
        window.set_focusable(false)?;   // ← 核心：常态不可聚焦
        clear_pre_edit_foreground();
    }
    window.show()?;
    window.unminimize()?;

    if label == CLIPBOARD_WINDOW_LABEL {
        keyboard::enable_navigation_keys(app_handle);
        mouse::enable_outside_click_hide(app_handle);   // ← 配套：全局鼠标钩子
    } else {
        window.set_focus()?;             // 设置窗等正常窗口仍可聚焦
    }
    Ok(())
}

// 输入控件编辑期间「临时」恢复可聚焦，退出时还原焦点
pub fn set_clipboard_window_editing(app_handle: &AppHandle, editing: bool) -> Result<()> {
    let hwnd = ...;
    if editing {
        remember_pre_edit_foreground(hwnd);   // 记下此刻的前台窗口
        keyboard::disable_navigation_keys();
        window.set_focusable(true)?;
        window.set_focus()?;
        return Ok(());
    }
    let should_restore = unsafe { GetForegroundWindow() == hwnd };
    window.set_focusable(false)?;
    if should_restore { restore_pre_edit_foreground(hwnd); }
    else { clear_pre_edit_foreground(); }
    Ok(())
}
```

**Edge-Drop**（Windows 悬浮剪贴板架）
`WS_EX_NOACTIVATE`（展开时）+ `WS_EX_TRANSPARENT | WS_EX_LAYERED`（收起时点击穿透）。

**Smart-Clipboard**（Python + Tkinter）
作者自述：难点正是 "Getting `WS_EX_NOACTIVATE` right so Peek never steals focus"。

**MindfulClipboard**
用全局鼠标钩子可靠地实现「点外面立即关闭」，作者称其"flawlessly bypassing restrictive Windows UIPI focus rules"。

### 关键配套：全局鼠标钩子

`WS_EX_NOACTIVATE` 的窗口不会因为点击而变前台，因此 **「点窗外自动隐藏」必须自己做**。
EcoPaste 用 `WH_MOUSE_LL` 低级鼠标钩子（`src-tauri/src/mouse/windows.rs`）：

- 独立线程 + `SetWindowsHookExW(WH_MOUSE_LL, ...)` + 消息泵（`GetMessageW` 循环）
- 拦截 `WM_LBUTTONDOWN` / `WM_RBUTTONDOWN` / `WM_MBUTTONDOWN`
- 用 `window.outer_position()` + `outer_size()` 判光标是否在窗口矩形外
- **不吞事件**（`CallNextHookEx` 照常传递）→ 用户的点击正常落到目标窗口
- 通过 `PostThreadMessageW(tid, WM_QUIT, 0, 0)` 优雅卸载钩子

### 优点 / 缺点

- ✅ 从根上消除焦点争夺，没有「抢不回来」的可能
- ✅ 面板打开期间用户的输入焦点完全不被打断
- ❌ 需要全局鼠标钩子实现自动隐藏（额外复杂度 + 杀软关注点）
- ❌ 输入框需要「临时切换到可聚焦」的开关逻辑
- ❌ 部分控件（某些 IME、拖拽）在无焦点窗口下行为异常
- ❌ UIPI 提权边界依然无法跨越

## 按键投递：SendInput vs keybd_event vs Shift+Insert

**EcoPaste 用 `SendInput` 一次投递 4 个 INPUT**（Ctrl↓ V↓ V↑ Ctrl↑），
与本项目实现一致。

它有一段注释解释了**为什么是 Ctrl+V 而不是 Shift+Insert**：

> Monaco editor（VS Code 聊天输入框、所有 Web 内嵌代码编辑器）把 `Insert`
> 解释为「切换插入/改写模式」并吞掉 Shift 修饰，导致「粘贴失效 + 光标变成块状」。
> `Ctrl+V` 是 Windows / Chromium / Electron / 现代 IDE 输入控件普遍约定，
> 与剪贴板内容类型无关，覆盖面最广。

**结论：本项目选 `Ctrl+V + SendInput` 是正确的，不要改成 Shift+Insert。**

## 其他值得注意的点

### 剪贴板所有权竞争（Handy 项目的「可靠粘贴」）

来源：cjpais/Handy `src-tauri/src/paste_tx/windows.rs`

问题：写剪贴板 → 发 Ctrl+V → 立即还原剪贴板，但**目标程序可能还没读取**，
还原就发生在读取之前 → 粘出旧内容。

解法（receipt-sequenced paste）：

1. **惰性承诺**：不实际写入数据，只声明自己是剪贴板所有者，
   等对方来要时再提供
2. **读取回执**：目标程序请求数据时，系统触发 `WM_RENDERFORMAT` 回调
3. **静默期**：收到最后一次回执后再等 `QUIET_PERIOD`（200ms）才还原
   —— 因为 Chrome 这类程序会**多次**读取剪贴板

对本项目的意义：我们当前是「写剪贴板 → 隐藏 → 等 150ms → 发键」，
没有还原剪贴板的逻辑，所以不受此问题影响。但如果日后要加「粘贴后还原原剪贴板」，
必须处理这个竞态。

### 三条路线的焦点行为对照（Edge-Drop 总结）

| 方案 | 焦点行为 | 适用 | 失效场景 |
|---|---|---|---|
| 普通顶层窗口 | 点击即抢焦点 | 模态框、设置窗 | 剪贴板浮层、拖出面板 |
| `WS_EX_NOACTIVATE` | 永不抢焦点 | 提示气泡、剪贴板浮层、IME | 任何需要键盘输入的场合 |
| `WS_EX_TRANSPARENT`+`WS_EX_LAYERED` | 点击穿透 | 隐藏触发区、水印 | 需要交互的浮层 |

**正确组合**：展开态用 `WS_EX_NOACTIVATE`，收起态用
`WS_EX_TRANSPARENT | WS_EX_LAYERED`。两者解决不同问题，缺一不可。

### 自检清单（试出面板是否抢了焦点）

打开记事本 → 光标停在里面 → 唤出面板 → 点一个条目 → 不点别处，直接按 Ctrl+V：

- 粘贴落进记事本 → 面板未抢焦点，正确
- 什么都没发生 / 记事本里光标消失了 → 面板抢了焦点

## 对本项目（QuickClip）的结论

1. **当前路线 A 没有走偏**。`SetForegroundWindow` + 记录目标 + `SendInput`
   是 EcoPaste 与 QuickClipboard 共同验证过的方案，且我们在
   **目标追踪方式**（事件钩子 vs 显示前抓一次）和
   **投递前校验**（焦点确认 + 重试）上比它们更严格。

2. **路线 B 是备选，不是升级**。切到 `WS_EX_NOACTIVATE` 需要同时：
   - 引入全局鼠标钩子实现「点窗外隐藏」
   - 给搜索框加「临时可聚焦」开关
   - 处理无焦点窗口下 IME / 拖拽的异常
   代价不小，而且会破坏「单击即粘贴」等依赖窗口可交互的交互。

3. **不要用 `Shift+Insert`**。Monaco 类编辑器会吞掉它并把光标变成块状。

4. **UIPI 是共同天花板**。两条路线都跨不过管理员权限边界，
   只能靠「以管理员身份运行本程序」解决。

---

## 后续：本轮实际落地（2026-10-11 晚）

用户反馈了新症状：**粘贴到目标窗口的输入框时，输入框会因为失去焦点而关闭，
导致粘贴失败。**

这暴露了路线 A 的一个盲区——它只解决了「怎么把焦点还回去」，
没解决「**焦点根本不该被夺走**」。目标程序的输入框若「失焦即关闭」
（浏览器地址栏联想下拉、页面内搜索条、编辑器命令面板、聊天框 @ 提及浮层），
那么面板 `set_focus()` 的瞬间输入框就没了，之后还焦点也无济于事。

于是落地方案变成**路线 A + 路线 B 的一半**：

| 借用自 | 做法 | 作用 |
|---|---|---|
| 路线 B | `SetWindowPos` + `SWP_NOACTIVATE` 显示面板 | 焦点全程不被夺走，输入框存活 |
| 路线 A | 记录目标 + 焦点校验 + `SendInput` | 兜底：需要时仍能抢焦点并可靠投递 |

**没有**采用路线 B 的 `WS_EX_NOACTIVATE` 扩展样式，因为那会让窗口彻底不可激活，
搜索框就没法输入了；`SWP_NOACTIVATE` 只影响**这一次** `SetWindowPos` 调用，
窗口本身依然可激活（用户点击后照样能拿焦点），代价小得多。

配套调整：

- 唤出后**不自动聚焦搜索框**（否则前端一 `focus()` 又把焦点抢回来，修复失效），
  改为**点击面板任意空白处**再聚焦。由后端通过 `panel-shown` 事件的
  `activated` 字段告知前端本次是否已激活。
- `paste_to_active` 增加判断：**焦点已在目标就跳过 `force_foreground`**。
  理由是抢焦点是"焦点被夺走了"的补救手段；焦点没丢时再抢，会在部分程序里
  重新触发激活流程，反而打乱输入框状态。
- 焦点未丢时的等待从 150ms 降到 40ms：没有窗口切换要等，
  只需留给目标程序处理剪贴板写入。

用一张表概括两种"不抢焦点"手段的区别：

| | `WS_EX_NOACTIVATE`（路线 B） | `SWP_NOACTIVATE`（本轮采用） |
|---|---|---|
| 粒度 | 窗口级、持久 | 调用级、一次性 |
| 用户点击后能否输入 | 不能，需临时切换样式 | 能，窗口仍可正常激活 |
| 是否需全局鼠标钩子 | 需要（否则点窗外不消失） | 不需要 |
| 副作用 | IME / 拖拽需额外处理 | 无 |

结论：**`SWP_NOACTIVATE` 是介于路线 A 与路线 B 之间的低成本折中**，
用一次性的不激活显示换取"输入框不被关闭"，同时保留窗口的可交互性。
这是本轮修复的最终形态。

## 参考链接

- EcoPaste issue #249（问题原始讨论）
  https://github.com/EcoPasteHub/EcoPaste/issues/249
- EcoPaste Windows 窗口管理
  https://github.com/EcoPasteHub/EcoPaste/blob/master/src-tauri/src/window/windows.rs
- EcoPaste 鼠标钩子
  https://github.com/EcoPasteHub/EcoPaste/blob/master/src-tauri/src/mouse/windows.rs
- EcoPaste 按键模拟
  https://github.com/EcoPasteHub/EcoPaste/blob/master/src-tauri/src/keystroke/windows.rs
- QuickClipboard 焦点管理
  https://deepwiki.com/mosheng1/QuickClipboard/5.1-window-management
- Edge-Drop：非抢焦点浮层解析
  https://www.edgedrop.app/blog/click-through-edges-that-do-not-steal-focus
- Handy：可靠粘贴（剪贴板所有权）
  https://deepwiki.com/cjpais/Handy/5.3-model-selector
