/**
 * QuickClip 前端主逻辑。
 *
 * 设计要点：
 * - 全部列表数据由 Rust 端内存缓存供给，前端不做二次持久化
 * - 键盘为第一交互路径（数字键直选、回车粘贴、方向键导航）
 * - 面板打开后默认聚焦搜索框，直接输入即搜索
 */

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

// ---------- 类型 ----------

type ObjType = 1 | 2 | 3 | 10;

interface ClipPayload {
  text: string;
  objType: ObjType;
}

interface ClipItem {
  id: string;
  item: ClipPayload;
  createTime: string;
  updateTime: string;
  isTop: boolean;
  subIndex: number;
  sourceApp?: string;
  hash: string;
}

interface FavorItem {
  id: string;
  item: ClipPayload;
  title: string;
  createTime: string;
  updateTime: string;
  parentId: string;
  isDeleted: boolean;
  description: string;
  children: string[];
  isFolder: boolean;
}

interface ClipConfig {
  [key: string]: unknown;
  showClipTab: boolean;
  showFavorTab: boolean;
  fontSize: number;
  showLineNumber: boolean;
  clipListSimpleMode: boolean;
  maxItemCount: number;
  maxItemSize: number;
  allowDuplicateCopy: boolean;
  ignoreWhiteSpace: boolean;
  plainTextPasteByDefault: boolean;
  defaultPasteDelay: number;
  usingNumberKeyToPaste: boolean;
  blackList: string;
  whiteList: string;
  hideMode: number;
  hotkey: string;
  theme: string;
  autoStart: boolean;
  startMinimized: boolean;
  windowPosition: string;
  fixedX: number;
  fixedY: number;
  lastX: number;
  lastY: number;
  showWithoutActivating: boolean;
}

interface BootstrapPayload {
  config: ClipConfig;
  clips: ClipItem[];
  favors: FavorItem[];
  dataDir: string;
  version: string;
}

// ---------- 状态 ----------

let state: {
  config: ClipConfig;
  clips: ClipItem[];
  favors: FavorItem[];
  dataDir: string;
  version: string;
} = {
  config: {} as ClipConfig,
  clips: [],
  favors: [],
  dataDir: "",
  version: "",
};

let activeTab: "clip" | "favor" = "clip";
let selectedIndex = 0;
let query = "";
/** 窗口是否固定（置顶常驻）。固定时粘贴后不自动关闭。 */
let windowPinned = false;
/**
 * 是否正在执行粘贴。
 *
 * 用途：单击条目即粘贴时，焦点会立即移交目标窗口并触发 `blur`。
 * 若此时 hideMode=1（失焦即隐藏）不加豁免，面板会在粘贴流程
 * 进行到一半时被提前隐藏，与流程自身的 hide 竞争。
 */
let pasting = false;
/** 已展开的文件夹 ID */
const expandedFolders = new Set<string>();

let $list: HTMLElement;
let $search: HTMLInputElement;
let $status: HTMLElement;

// ---------- 初始化 ----------

async function init() {
  $list = document.getElementById("list")!;
  $search = document.getElementById("search") as HTMLInputElement;
  $status = document.getElementById("status-text")!;

  const data = await invoke<BootstrapPayload>("bootstrap");
  state.config = data.config;
  state.clips = data.clips;
  state.favors = data.favors;
  state.dataDir = data.dataDir;
  state.version = data.version;

  applyTheme();
  applyConfigToDom();
  updateHint();
  bindUi();
  render();

  // 固定状态由后端持有，启动时同步一次，避免界面与事实不符
  try {
    windowPinned = await invoke<boolean>("get_pinned");
    applyPinState(windowPinned);
  } catch {
    // 查询失败时按未固定处理，不影响使用
  }

  // 三个事件（新增 / 增删改 / 单条更新）都会改变剪切板列表的内容，
  // 处理动作完全相同：重新拉一次列表再渲染。
  //
  // 合并成一个处理函数，而不是写三份一样的回调——否则将来要给
  // "列表变化"加逻辑（比如保留滚动位置）时，很容易只改其中一处。
  const onClipsChanged = async () => {
    state.clips = await invoke<ClipItem[]>("list_clips", { query: null });
    render();
  };
  await listen("clip-added", onClipsChanged);
  await listen("clips-changed", onClipsChanged);
  await listen("clip-updated", onClipsChanged);

  await listen("favors-changed", async () => {
    state.favors = await invoke<FavorItem[]>("list_favors");
    render();
  });

  await listen<{ activated: boolean }>("panel-shown", (event) => {
    selectedIndex = 0;
    render();

    // 路线 B（永不激活）下，面板刻意不是前台窗口，为的是让目标程序的
    // 输入框（浏览器地址栏联想、编辑器命令面板等）不在唤出瞬间被关闭。
    // 此时若前端执行 $search.focus()，等于把刚保住的焦点又抢回来，修复失效。
    //
    // 需要输入时改走"临时可聚焦"：见 ensureSearchFocusable 的处理。
    if (event.payload?.activated) {
      $search.focus();
      $search.select();
    } else {
      ensureSearchFocusable();
    }
  });
}

function applyTheme() {
  document.documentElement.classList.toggle("light", state.config.theme === "light");
  document.documentElement.style.setProperty("--font-size", `${state.config.fontSize}px`);
}

function applyConfigToDom() {
  const clipTab = document.querySelector('[data-tab="clip"]') as HTMLElement;
  const favorTab = document.querySelector('[data-tab="favor"]') as HTMLElement;
  clipTab.style.display = state.config.showClipTab ? "" : "none";
  favorTab.style.display = state.config.showFavorTab ? "" : "none";
}

// ---------- 事件绑定 ----------

function bindUi() {
  document.querySelectorAll(".tab").forEach((el) => {
    el.addEventListener("click", () => {
      activeTab = (el as HTMLElement).dataset.tab as typeof activeTab;
      document.querySelectorAll(".tab").forEach((t) => t.classList.remove("active"));
      el.classList.add("active");
      selectedIndex = 0;
      render();
    });
  });

  $search.addEventListener("input", () => {
    query = $search.value;
    selectedIndex = 0;
    refreshClips();
  });

  // 搜索框失焦 → 把面板恢复成"永不激活"，释放临时可聚焦状态。
  //
  // 这是路线 B 的"戴回去"动作：用户不再输入时立刻收回可聚焦，
  // 之后无论面板怎么显示，都不会再抢目标程序的焦点。
  $search.addEventListener("blur", () => {
    // 设置面板内的输入框失焦不算数（用户正在改设置）
    if (isSettingsOpen()) return;
    void releaseSearchFocusable();
  });

  // 工具栏：固定 / 搜索 / 设置 / 关闭。
  document.getElementById("btn-settings")!.addEventListener("click", openSettings);
  document.getElementById("btn-collapse")!.addEventListener("click", toggleSearchBar);
  document.getElementById("btn-close-settings")!.addEventListener("click", closeSettings);
  document.getElementById("btn-save-settings")!.addEventListener("click", saveSettings);
  document.getElementById("btn-open-data")!.addEventListener("click", () => invoke("open_data_dir"));
  document.getElementById("btn-pin")!.addEventListener("click", toggleWindowPin);
  // 关闭按钮 = 隐藏到托盘（不是退出进程）。标题栏双击同样隐藏，
  // 两者并存不冲突：双击是"老手快捷方式"，按钮是"看得见的入口"。
  document.getElementById("btn-close")!.addEventListener("click", () => invoke("hide_window"));

  // 红绿灯热区阻止拖拽的逻辑已随按钮方案一同移除

  // 全局键盘
  document.addEventListener("keydown", onKeyDown);

  // 点击空白处收起右键菜单
  document.addEventListener("click", () => hideCtxMenu());
  $list.addEventListener("contextmenu", (e) => e.preventDefault());

  // 路线 B 下，面板不是前台窗口，所以"点击面板"这件事默认不会让浏览器
  // 把焦点交给面板内的元素。用户若想搜索，需要显式地把面板临时变为可激活。
  //
  // 因此：**点击面板空白区 → 临时开启可聚焦并聚焦搜索框**。
  // 点按钮 / 列表条目 / 设置面板各有既定行为，不干预。
  document.getElementById("app")!.addEventListener("mousedown", (e) => {
    const t = e.target as HTMLElement;
    if (t.closest("button") || t.closest(".item") || t.closest(".settings-panel")) return;
    if (document.activeElement === $search) return;
    void focusSearch();
  });

  // 标题栏双击 = 隐藏面板（替代原红点按钮，避免误触）
  document.querySelector(".titlebar")!.addEventListener("dblclick", (e) => {
    // 双击到按钮上时不触发
    if ((e.target as HTMLElement).closest("button")) return;
    invoke("hide_window");
  });

  // 窗口失焦自动隐藏（对齐参考实现的 HideMode=1）
  // 固定状态下豁免：固定的语义就是"常驻最上层"，
  // 若因失焦而隐藏，用户点一下别处面板就没了，与固定相矛盾。
  //
  // 粘贴进行中同样豁免：单击条目即粘贴时，焦点会立刻移交给目标窗口，
  // 由此产生的 blur 会在 pasteItem 完成前抢先隐藏面板——虽然结果看着
  // 相似（面板确实应该消失），但它会与粘贴流程自己的 hide 竞争，
  // 且在粘贴失败时把面板藏掉、用户看不到错误提示。
  //
  // 注意：路线 B 下窗口从不成为前台窗口，`blur` 基本不会触发，
  // "点别处隐藏"由全局鼠标钩子负责（见 pollHideRequest）。
  window.addEventListener("blur", () => {
    if (pasting) return;
    if (state.config.hideMode === 1 && !isSettingsOpen() && !windowPinned) {
      invoke("hide_window");
    }
  });

  startHideRequestPolling();
}

/**
 * 轮询后端"是否请求隐藏面板"。
 *
 * 路线 B 的窗口收不到 `WM_KILLFOCUS`，无法用 blur 检测"用户点了别处"。
 * 后端装了全局鼠标钩子来判断落点，命中则置一个标志；这里定时取走并执行隐藏。
 *
 * 频率取 200ms：这是纯内存读取的轻量调用，代价可忽略；而 200ms 的延迟
 * 对"点别处要等面板消失"这种交互完全无感。再快没有收益，再慢会显得迟钝。
 */
function startHideRequestPolling() {
  window.setInterval(async () => {
    // 不在这里判断"窗口是否可见"：`document.hidden` 反映的是页面可见性，
    // 而 Tauri 的 `hide()` 不保证让它变化，拿它当判据会误判。
    // 可见性判断放在后端（`poll_hide_request` 内部用 `is_visible` 检查），
    // 那才是唯一知道窗口真实状态的地方。
    try {
      const want = await invoke<boolean>("poll_hide_request");
      if (!want) return;
      // 固定状态与设置面板打开时豁免（与 blur 逻辑保持一致的判断）
      if (isSettingsOpen() || windowPinned) return;
      await invoke("hide_window");
    } catch {
      /* 忽略：下一次轮询会重试 */
    }
  }, 200);
}

/** 切换搜索框显隐（顶部放大镜按钮 / 绿色交通灯）。 */
function toggleSearchBar() {
  const bar = document.getElementById("searchbar")!;
  const hidden = bar.classList.toggle("hidden");
  if (hidden) {
    if (query) {
      query = "";
      $search.value = "";
      refreshClips();
    }
  } else {
    // 展开搜索栏时用户显然想输入，需要临时开启可聚焦
    void focusSearch();
  }
}

/**
 * 切换窗口固定（置顶常驻）状态。
 *
 * 两种模式对应两套交互：
 * - 未固定：点击条目 → 粘贴一次 → 面板自动关闭（默认的"用完即走"）
 * - 已固定：窗口保持最上层，粘贴后不关闭，便于连续粘贴多条
 *
 * 界面以后端返回的真实状态为准，避免乐观更新与实际不符。
 */
async function toggleWindowPin() {
  const next = !windowPinned;
  try {
    const actual = await invoke<boolean>("set_pinned", { pinned: next });
    windowPinned = actual;
    applyPinState(actual);
    setStatus(actual ? "已固定窗口：保持最上层" : "已取消固定：粘贴后自动关闭");
  } catch (e) {
    setStatus(`设置固定状态失败：${e}`);
  }
}

/** 把固定状态反映到按钮外观上。 */
function applyPinState(pinned: boolean) {
  const btn = document.getElementById("btn-pin");
  if (btn) {
    btn.classList.toggle("on", pinned);
    btn.title = pinned ? "已固定：保持最上层（点击取消）" : "固定窗口（置顶常驻）";
  }
}

function isSettingsOpen(): boolean {
  return document.getElementById("settings-overlay")!.classList.contains("show");
}

async function refreshClips() {
  state.clips = await invoke<ClipItem[]>("list_clips", { query: query || null });
  render();
}

// ---------- 键盘 ----------

function onKeyDown(e: KeyboardEvent) {
  // 录入快捷键期间，键盘事件全部交给录入逻辑处理
  if (hotkeyCapturing) return;

  if (isSettingsOpen()) {
    if (e.key === "Escape") closeSettings();
    return;
  }

  const items = currentSelectableItems();

  switch (e.key) {
    case "ArrowDown":
      e.preventDefault();
      selectedIndex = Math.min(selectedIndex + 1, items.length - 1);
      ensureVisible();
      render();
      break;
    case "ArrowUp":
      e.preventDefault();
      selectedIndex = Math.max(selectedIndex - 1, 0);
      ensureVisible();
      render();
      break;
    case "Enter": {
      e.preventDefault();
      const it = items[selectedIndex];
      if (it) activateItem(it.id, e.ctrlKey);
      break;
    }
    case "Escape":
      e.preventDefault();
      if (query) {
        query = "";
        $search.value = "";
        refreshClips();
      } else {
        invoke("hide_window");
      }
      break;
    case "Delete":
      if (document.activeElement !== $search && items[selectedIndex]) {
        e.preventDefault();
        deleteItem(items[selectedIndex].id);
      }
      break;
    default:
      // 数字键直选粘贴（1-9），仅在搜索框未聚焦或无输入时生效
      if (
        state.config.usingNumberKeyToPaste &&
        /^[1-9]$/.test(e.key) &&
        document.activeElement !== $search
      ) {
        const idx = parseInt(e.key, 10) - 1;
        if (idx < items.length) {
          e.preventDefault();
          pasteItem(items[idx].id, e.ctrlKey);
        }
      }
      // 其他可打印字符自动聚焦搜索框
      else if (
        e.key.length === 1 &&
        !e.ctrlKey &&
        !e.altKey &&
        document.activeElement !== $search
      ) {
        void focusSearch();
      }
  }
}

function ensureVisible() {
  const el = $list.querySelector(".item.selected") as HTMLElement | null;
  el?.scrollIntoView({ block: "nearest" });
}

/** 当前标签页下可通过键盘选择的条目（扁平化顺序）。 */
function currentSelectableItems(): { id: string }[] {
  if (activeTab === "clip") return state.clips;
  return flattenFavors();
}

function flattenFavors(): FavorItem[] {
  const out: FavorItem[] = [];
  const walk = (parentId: string, depth: number) => {
    if (depth > 4) return;
    state.favors
      .filter((f) => f.parentId === parentId && !f.isDeleted)
      .forEach((f) => {
        out.push(f);
        if (f.isFolder && expandedFolders.has(f.id)) walk(f.id, depth + 1);
      });
  };
  walk("", 0);
  return out;
}

// ---------- 渲染 ----------

function render() {
  if (activeTab === "clip") renderClips();
  else renderFavors();
  updateStatus();
}

function renderClips() {
  if (state.clips.length === 0) {
    $list.innerHTML = emptyState(
      query ? "没有匹配的内容" : "暂无剪切板记录",
      query ? "换个关键词试试" : "复制任意内容后会自动出现在这里"
    );
    return;
  }

  $list.innerHTML = state.clips
    .map((it, i) => renderClipRow(it, i))
    .join("");

  bindItemEvents();
}

function renderClipRow(it: ClipItem, i: number): string {
  const sel = i === selectedIndex ? " selected" : "";
  const pin = it.isTop ? " pinned" : "";
  const simple = state.config.clipListSimpleMode ? " simple" : "";
  const num = state.config.showLineNumber
    ? `<div class="item-num">${i < 9 ? i + 1 : ""}</div>`
    : "";

  let preview = "";
  let badge = "";

  switch (it.item.objType) {
    case 2:
      badge = `<span class="badge img">图片</span>`;
      preview = `<img class="item-thumb" src="${assetUrl(it.item.text)}" alt="图片" />`;
      break;
    case 3: {
      badge = `<span class="badge file">文件</span>`;
      const lines = it.item.text.split("\n");
      preview = `<div class="item-text">${esc(lines.slice(0, 3).join("\n"))}${lines.length > 3 ? `\n…共 ${lines.length} 个文件` : ""}</div>`;
      break;
    }
    case 10:
      badge = `<span class="badge rich">富文本</span>`;
      preview = `<div class="item-text">${esc(stripHtml(it.item.text))}</div>`;
      break;
    default:
      preview = `<div class="item-text">${esc(it.item.text)}</div>`;
  }

  const source = it.sourceApp ? `<span>${esc(it.sourceApp)}</span>` : "";

  return `
    <div class="item${sel}${pin}${simple}" data-id="${it.id}" data-index="${i}">
      ${num}
      <div class="item-body">
        ${badge ? `<div class="item-meta" style="margin:0 0 2px">${badge}</div>` : ""}
        ${preview}
        <div class="item-meta">
          <span>${timeAgo(it.updateTime)}</span>
          ${source}
        </div>
      </div>
      <div class="item-actions">
        <button class="mini-btn" data-act="pin" title="置顶">📌</button>
        <button class="mini-btn" data-act="favor" title="收藏">★</button>
        <button class="mini-btn" data-act="del" title="删除">🗑</button>
      </div>
    </div>`;
}

function renderFavors() {
  const roots = state.favors.filter((f) => f.parentId === "" && !f.isDeleted);
  if (roots.length === 0) {
    $list.innerHTML = emptyState("收藏夹为空", "在剪切板列表点击 ★ 即可收藏");
    return;
  }

  // 一次性建立 id → 序号 的映射，避免渲染时对每个条目做一次线性查找
  // （早期用 flat.findIndex，收藏条目多时是 O(n²)）。
  const flat = flattenFavors();
  const indexOf = new Map(flat.map((f, i) => [f.id, i]));
  const html: string[] = [];
  const walk = (parentId: string, depth: number) => {
    if (depth > 4) return;
    state.favors
      .filter((f) => f.parentId === parentId && !f.isDeleted)
      .forEach((f) => {
        const idx = indexOf.get(f.id) ?? -1;
        if (f.isFolder) {
          const open = expandedFolders.has(f.id);
          html.push(`
            <div class="folder-row indent-${Math.min(depth, 3)}" data-folder="${f.id}">
              <span class="arrow${open ? " open" : ""}">▶</span>
              <span>${esc(f.title || "未命名分组")}</span>
              <span class="badge">${f.children.length}</span>
            </div>`);
          if (open) walk(f.id, depth + 1);
        } else {
          html.push(renderFavorRow(f, idx, depth));
        }
      });
  };
  walk("", 0);

  $list.innerHTML = html.join("");
  bindItemEvents();
  $list.querySelectorAll("[data-folder]").forEach((el) => {
    el.addEventListener("click", (e) => {
      e.stopPropagation();
      const id = (el as HTMLElement).dataset.folder!;
      if (expandedFolders.has(id)) expandedFolders.delete(id);
      else expandedFolders.add(id);
      render();
    });
  });
}

function renderFavorRow(f: FavorItem, idx: number, depth: number): string {
  const sel = idx === selectedIndex ? " selected" : "";
  return `
    <div class="item indent-${Math.min(depth, 3)}${sel}" data-id="${f.id}" data-index="${idx}" data-favor="1">
      <div class="item-body">
        <div class="item-text">${esc(f.title || f.item.text)}</div>
      </div>
      <div class="item-actions">
        <button class="mini-btn" data-act="del" title="删除">🗑</button>
      </div>
    </div>`;
}

function bindItemEvents() {
  $list.querySelectorAll(".item").forEach((el) => {
    const node = el as HTMLElement;
    const id = node.dataset.id!;

    // 单击条目直接激活：
    // - 剪切板 / 收藏 → 立即粘贴到目标窗口（面板随之隐藏）
    // - 路径页 → 打开该路径
    // 早期是"单击选中 + 双击粘贴"，但双击存在 ~500ms 判定延迟，
    // 且单击只选中不做事，用户容易误以为没反应。改为单击即用后，
    // 整个操作路径缩短为一步，与主流剪贴板工具一致。
    node.addEventListener("click", (e) => {
      const target = e.target as HTMLElement;
      // 行内小按钮（删除 / 置顶 / 加入收藏等）自行处理，不触发激活
      if (target.closest("[data-act]")) return;
      const idx = parseInt(node.dataset.index || "0", 10);
      selectedIndex = idx;
      void activateItem(id, e.ctrlKey);
    });

    node.addEventListener("contextmenu", (e) => {
      e.preventDefault();
      e.stopPropagation();
      showCtxMenu(e.clientX, e.clientY, id, node);
    });

    node.querySelectorAll("[data-act]").forEach((btn) => {
      btn.addEventListener("click", (e) => {
        e.stopPropagation();
        const act = (btn as HTMLElement).dataset.act!;
        if (act === "pin") togglePin(id);
        else if (act === "favor") addToFavor(id);
        else if (act === "del") deleteItem(id);
      });
    });
  });
}

function emptyState(title: string, sub: string): string {
  return `<div class="empty"><div class="big">✂</div><div>${esc(title)}</div><small>${esc(sub)}</small></div>`;
}

function updateStatus() {
  const counts = `剪切板 ${state.clips.length} · 收藏 ${state.favors.length}`;
  $status.textContent = counts;
}

/** 底部提示跟随当前快捷键，避免文档与配置不一致。 */
function updateHint() {
  const hint = document.querySelector(".hint") as HTMLElement | null;
  if (!hint) return;
  const hk = state.config.hotkey || "Alt+V";
  hint.textContent = `单击粘贴 · 回车粘贴 · ${hk} 唤出`;
}

// ---------- 路线 B：搜索框的临时可聚焦 ----------

/**
 * 请求把面板临时恢复成"可激活"，以便搜索框接收键盘输入。
 *
 * 背景：路线 B 给窗口加了 `WS_EX_NOACTIVATE`，这种窗口**永远拿不到键盘焦点**，
 * 搜索框也就打不了字。这是该路线的固有代价，唯一解法是输入前临时去掉样式。
 *
 * 调用时机：用户点击搜索框（或点面板后我们主动聚焦搜索框）。
 * 对应的"戴回去"在 `releaseSearchFocusable` 中，由搜索框失焦触发。
 *
 * 时序上有个易错点：**必须先等后端把样式改完，再调 `focus()`**。
 * 若先 `focus()`，此刻窗口还带着 NOACTIVATE，浏览器会拒绝让它成为
 * 活动文档，焦点落空；等样式改完时已经错过了那次聚焦动作。
 * 所以这里 `await` 后端返回后再聚焦。
 */
async function ensureSearchFocusable() {
  try {
    await invoke("set_search_focusable", { focusable: true });
  } catch {
    // 后端失败也继续尝试聚焦：非 Windows 或旧版本下样式本就不存在
  }
}

/**
 * 把面板恢复成"永不激活"，释放临时可聚焦状态。
 *
 * 调用时机：搜索框失焦（用户点了别处 / 按了 Esc 收起键盘操作）。
 * 恢复后再点别处，面板就不会抢焦点，目标输入框得以安全。
 */
async function releaseSearchFocusable() {
  try {
    await invoke("set_search_focusable", { focusable: false });
  } catch {
    /* 忽略 */
  }
}

/** 聚焦搜索框：先临时开启可聚焦，再真正调用 focus。 */
async function focusSearch() {
  await ensureSearchFocusable();
  $search.focus();
  $search.select();
}

// ---------- 操作 ----------

async function pasteItem(id: string, plain: boolean) {
  pasting = true;
  try {
    await invoke("copy_clip", { id, plainText: plain });
    await invoke("paste_to_active");
  } catch (e) {
    setStatus(`粘贴失败：${e}`);
  } finally {
    pasting = false;
  }
}

/** 激活列表项：两个标签页的行为一致，都是立即粘贴。 */
async function activateItem(id: string, plain: boolean) {
  await pasteItem(id, plain);
}

async function deleteItem(id: string) {
  if (activeTab === "favor") {
    await invoke("delete_favor", { id });
    state.favors = await invoke<FavorItem[]>("list_favors");
  } else {
    await invoke("delete_clips", { ids: [id] });
    state.clips = await invoke<ClipItem[]>("list_clips", { query: query || null });
  }
  selectedIndex = Math.max(0, selectedIndex - 1);
  render();
}

async function togglePin(id: string) {
  await invoke("toggle_top", { id });
  state.clips = await invoke<ClipItem[]>("list_clips", { query: query || null });
  render();
}

async function addToFavor(id: string) {
  await invoke("add_favor", { id, parentId: null, title: null });
  setStatus("已加入收藏");
}

function setStatus(msg: string) {
  $status.textContent = msg;
  setTimeout(updateStatus, 1800);
}

// ---------- 右键菜单 ----------

function showCtxMenu(x: number, y: number, id: string, node: HTMLElement) {
  const menu = document.getElementById("ctx-menu")!;
  const isFavor = !!node.dataset.favor;

  const items = isFavor
    ? [
        `<div class="ctx-item" data-cmd="paste">粘贴</div>`,
        `<div class="ctx-item" data-cmd="copy">复制到剪切板</div>`,
        `<div class="ctx-sep"></div>`,
        `<div class="ctx-item danger" data-cmd="del">删除</div>`,
      ]
    : [
        `<div class="ctx-item" data-cmd="paste">粘贴</div>`,
        `<div class="ctx-item" data-cmd="plain">纯文本粘贴</div>`,
        `<div class="ctx-item" data-cmd="copy">复制到剪切板</div>`,
        `<div class="ctx-sep"></div>`,
        `<div class="ctx-item" data-cmd="pin">置顶 / 取消置顶</div>`,
        `<div class="ctx-item" data-cmd="favor">加入收藏</div>`,
        `<div class="ctx-sep"></div>`,
        `<div class="ctx-item danger" data-cmd="del">删除</div>`,
      ];

  menu.innerHTML = items.join("");
  menu.classList.add("show");

  const rect = menu.getBoundingClientRect();
  const maxX = window.innerWidth - rect.width - 4;
  const maxY = window.innerHeight - rect.height - 4;
  menu.style.left = `${Math.min(x, maxX)}px`;
  menu.style.top = `${Math.min(y, maxY)}px`;

  menu.querySelectorAll(".ctx-item").forEach((el) => {
    el.addEventListener("click", async () => {
      const cmd = (el as HTMLElement).dataset.cmd!;
      hideCtxMenu();
      if (cmd === "paste") await pasteItem(id, false);
      else if (cmd === "plain") await pasteItem(id, true);
      else if (cmd === "copy") await invoke("copy_clip", { id, plainText: false });
      else if (cmd === "pin") await togglePin(id);
      else if (cmd === "favor") await addToFavor(id);
      else if (cmd === "del") await deleteItem(id);
    });
  });
}

function hideCtxMenu() {
  document.getElementById("ctx-menu")!.classList.remove("show");
}

// ---------- 设置面板 ----------

interface FieldDef {
  key: keyof ClipConfig;
  label: string;
  desc?: string;
  type: "bool" | "number" | "text" | "select" | "hotkey";
  options?: { value: string; label: string }[];
  min?: number;
  max?: number;
  /**
   * 是否即时生效。
   * 用于开机自启这类需要写系统设置、无法等"保存"按钮的项：
   * 一旦切换立即调用后端并回报结果，不成功则回滚开关状态。
   */
  immediate?: boolean;
}

const SETTINGS: (FieldDef | { section: string })[] = [
  { section: "常规" },
  { key: "hotkey", label: "唤出快捷键", desc: "点击后直接按下想要的组合键", type: "hotkey" },
  { key: "theme", label: "外观", type: "select", options: [
    { value: "dark", label: "深色" },
    { value: "light", label: "浅色" },
  ]},
  { key: "fontSize", label: "字号", type: "number", min: 10, max: 20 },
  { key: "showLineNumber", label: "显示序号", type: "bool" },
  { key: "clipListSimpleMode", label: "紧凑单行模式", desc: "每项仅显示一行", type: "bool" },

  { section: "记录规则" },
  { key: "maxItemCount", label: "最大历史条数", desc: "超出后自动清理最旧记录", type: "number", min: 50, max: 10000 },
  { key: "maxItemSize", label: "单条最大字符数", desc: "超长内容截断保存", type: "number", min: 100, max: 1000000 },
  { key: "allowDuplicateCopy", label: "允许重复内容", desc: "关闭则相同内容只保留一条", type: "bool" },
  { key: "ignoreWhiteSpace", label: "忽略纯空白内容", type: "bool" },

  { section: "粘贴行为" },
  { key: "usingNumberKeyToPaste", label: "数字键直接粘贴", desc: "按 1-9 直接粘贴对应条目", type: "bool" },
  { key: "plainTextPasteByDefault", label: "默认纯文本粘贴", desc: "去除富文本格式", type: "bool" },
  { key: "defaultPasteDelay", label: "粘贴延迟(毫秒)", desc: "应对响应较慢的目标程序", type: "number", min: 0, max: 2000 },

  { section: "启动与后台" },
  { key: "autoStart", label: "开机自启", desc: "随系统启动，后台运行", type: "bool", immediate: true },
  { key: "startMinimized", label: "开机自启时静默到托盘", desc: "关闭后开机自启也会弹出面板；手动打开一律显示窗口", type: "bool" },

  { section: "窗口行为" },
  { key: "showWithoutActivating", label: "唤出时不抢焦点", desc: "解决「粘贴到输入框时输入框被关闭」：浏览器地址栏、命令面板等失焦即关。开启后点面板任意处即可开始搜索", type: "bool" },
  { key: "windowPosition", label: "窗口位置", type: "select", options: [
    { value: "cursor", label: "跟随鼠标（默认）" },
    { value: "last", label: "上次关闭位置" },
    { value: "fixed", label: "固定位置" },
  ]},
  { key: "fixedX", label: "固定位置 X", desc: "仅「固定位置」模式生效，屏幕像素坐标", type: "number", min: -10000, max: 10000 },
  { key: "fixedY", label: "固定位置 Y", desc: "仅「固定位置」模式生效，屏幕像素坐标", type: "number", min: -10000, max: 10000 },
  { key: "hideMode", label: "隐藏模式", type: "select", options: [
    { value: "0", label: "不自动隐藏" },
    { value: "1", label: "失焦即隐藏" },
    { value: "2", label: "手动隐藏" },
  ]},
  { key: "showClipTab", label: "显示「剪切板」页", type: "bool" },
  { key: "showFavorTab", label: "显示「收藏」页", type: "bool" },

  { section: "过滤名单" },
  { key: "blackList", label: "黑名单进程", desc: "逗号分隔，命中则不记录", type: "text" },
  { key: "whiteList", label: "白名单进程", desc: "逗号分隔，非空时仅记录命中项", type: "text" },
];

async function openSettings() {
  // 设置面板里有数字输入框、快捷键录入等需要键盘操作，
  // 路线 B 下必须先临时把面板恢复成可激活，否则输入框没法用。
  await ensureSearchFocusable();

  const body = document.getElementById("settings-body")!;
  clearSettingsError();
  body.innerHTML = renderSettingsGroups();

  // 布尔开关
  body.querySelectorAll(".switch").forEach((sw) => {
    sw.addEventListener("click", async () => {
      const node = sw as HTMLElement;
      const key = node.dataset.key as keyof ClipConfig;
      const immediate = node.dataset.immediate === "1";
      const turningOn = !node.classList.contains("on");

      if (immediate) {
        // 即时生效项：先乐观更新界面，失败则回滚
        node.classList.toggle("on");
        try {
          if (key === "autoStart") {
            const actual = await invoke<boolean>("set_autostart", { enabled: turningOn });
            // 以后端返回的真实状态为准
            node.classList.toggle("on", actual);
            state.config.autoStart = actual;
            setStatus(actual ? "已开启开机自启" : "已关闭开机自启");
          }
        } catch (e) {
          node.classList.toggle("on", !turningOn);
          setStatus(String(e));
        }
      } else {
        node.classList.toggle("on");
      }
    });
  });

  // 快捷键录入：点击进入"监听"状态，随后按下的第一个组合键即被记录
  body.querySelectorAll(".hotkey-input").forEach((btn) => {
    btn.addEventListener("click", () => beginHotkeyCapture(btn as HTMLElement));
  });

  // 窗口位置策略：坐标输入框只在"固定位置"下有意义，其余情况下收起，
  // 避免用户以为改了坐标会影响"跟随鼠标"模式。
  const posSel = body.querySelector('[data-key="windowPosition"]') as HTMLSelectElement | null;
  if (posSel) {
    const syncCoordRows = () => {
      const on = posSel.value === "fixed";
      for (const k of ["fixedX", "fixedY"]) {
        const row = body.querySelector(`[data-key="${k}"]`)?.closest(".setting-row");
        if (row) (row as HTMLElement).style.display = on ? "" : "none";
      }
    };
    posSel.addEventListener("change", syncCoordRows);
    syncCoordRows();
  }

  // 打开设置时同步自启的真实状态（可能与配置文件不一致）
  try {
    const actual = await invoke<boolean>("get_autostart");
    state.config.autoStart = actual;
    const sw = body.querySelector('[data-key="autoStart"]') as HTMLElement | null;
    if (sw) sw.classList.toggle("on", actual);
  } catch {
    // 查询失败时保留配置值，不阻断设置面板
  }

  document.getElementById("settings-overlay")!.classList.add("show");
}

/**
 * 进入快捷键录入状态，捕获用户按下的第一个有效组合键。
 *
 * 交互：点击按钮 → 显示"请按下组合键…" → 用户按键 → 立即回显并退出监听。
 * 与逐字输入相比，这种方式不可能拼错，也天然避免了"格式不对"的困惑。
 */
let hotkeyCapturing: HTMLElement | null = null;

function beginHotkeyCapture(btn: HTMLElement) {
  // 再次点击同一按钮 = 取消
  if (hotkeyCapturing === btn) {
    endHotkeyCapture();
    return;
  }
  if (hotkeyCapturing) endHotkeyCapture();

  hotkeyCapturing = btn;
  btn.classList.add("capturing");
  const txt = btn.querySelector(".hotkey-text")!;
  txt.textContent = "请按下组合键…";

  // 用捕获阶段拦截，确保在其它 keydown 处理之前拿到事件
  window.addEventListener("keydown", onHotkeyCaptureKey, true);
}

function endHotkeyCapture() {
  if (!hotkeyCapturing) return;
  hotkeyCapturing.classList.remove("capturing");
  const btn = hotkeyCapturing;
  hotkeyCapturing = null;
  window.removeEventListener("keydown", onHotkeyCaptureKey, true);
  // 还原显示为当前值
  const txt = btn.querySelector(".hotkey-text")!;
  txt.textContent = String(state.config.hotkey || "") || "点击录入";
}

function onHotkeyCaptureKey(e: KeyboardEvent) {
  e.preventDefault();
  e.stopPropagation();

  if (!hotkeyCapturing) return;
  const btn = hotkeyCapturing;

  // Esc 取消录入
  if (e.key === "Escape") {
    endHotkeyCapture();
    return;
  }

  // 修饰键自身不作为主键——等待主键
  if (["Control", "Alt", "Shift", "Meta"].includes(e.key)) {
    // 实时预览已按下的修饰键，给用户反馈
    const mods = describeModifiers(e);
    if (mods) btn.querySelector(".hotkey-text")!.textContent = `${mods}+…`;
    return;
  }

  const mods = describeModifiers(e);
  const main = describeMainKey(e);

  // 必须有修饰键，否则会劫持普通按键输入
  if (!mods) {
    btn.querySelector(".hotkey-text")!.textContent = "需含 Ctrl / Alt / Shift";
    return;
  }
  if (!main) {
    btn.querySelector(".hotkey-text")!.textContent = "该键不支持";
    return;
  }

  // 录入结果只写进按钮的 dataset，**不碰 state.config.hotkey**。
  //
  // 这一点是刻意的：`saveSettings` 靠"新值 !== state.config.hotkey"
  // 来判断快捷键是否变过，从而决定要不要调用 rebind_hotkey。若在这里
  // 提前写入 state.config，那个判断就恒为 false，结果是"配置存了、
  // 但全局快捷键从没重新注册"——新组合完全不生效。
  const combo = `${mods}+${main}`;
  btn.dataset.value = combo;
  endHotkeyCapture();
  btn.querySelector(".hotkey-text")!.textContent = combo;
}

/** 把当前按下的修饰键转成配置字符串，无修饰键时返回空串。 */
function describeModifiers(e: KeyboardEvent): string {
  const parts: string[] = [];
  if (e.ctrlKey) parts.push("Ctrl");
  if (e.altKey) parts.push("Alt");
  if (e.shiftKey) parts.push("Shift");
  if (e.metaKey) parts.push("Win");
  return parts.join("+");
}

/**
 * 把主键转成后端可解析的名称。
 *
 * 与 `hotkey.rs::parse_key` 的命名保持一致（不区分大小写）。
 */
function describeMainKey(e: KeyboardEvent): string | null {
  const k = e.key;
  if (k.length === 1) {
    // 字母统一大写、数字与符号原样
    return /[a-zA-Z]/.test(k) ? k.toUpperCase() : k;
  }
  const map: Record<string, string> = {
    " ": "Space",
    Enter: "Enter",
    Tab: "Tab",
    Escape: "Esc",
    ArrowUp: "Up",
    ArrowDown: "Down",
    ArrowLeft: "Left",
    ArrowRight: "Right",
    Backquote: "`",
    Minus: "-",
    Equal: "=",
    BracketLeft: "[",
    BracketRight: "]",
    Backslash: "\\",
    Semicolon: ";",
    Quote: "'",
    Comma: ",",
    Period: ".",
    Slash: "/",
  };
  if (map[k]) return map[k];
  // F1-F12
  if (/^F([1-9]|1[0-2])$/.test(k)) return k;
  return null;
}

/**
 * 把设置项按 section 组装成 iOS 风格分组卡片。
 *
 * 分组包裹的意义：让一组行共享一个圆角容器与内部细分割线，
 * 而不是依赖 `:has()` 这类相邻选择器去推断组边界——后者在
 * 分组只有一行、或顺序变化时容易失效。
 */
function renderSettingsGroups(): string {
  const groups: { title: string; fields: FieldDef[] }[] = [];
  for (const f of SETTINGS) {
    if ("section" in f) {
      groups.push({ title: f.section, fields: [] });
    } else if (groups.length) {
      groups[groups.length - 1].fields.push(f as FieldDef);
    }
  }
  return groups
    .map(
      (g) => `
      <div class="setting-section">${g.title}</div>
      <div class="setting-group">${g.fields.map(renderField).join("")}</div>`
    )
    .join("");
}

function renderField(f: FieldDef): string {
  const val = state.config[f.key];
  let control = "";
  if (f.type === "bool") {
    const imm = f.immediate ? ' data-immediate="1"' : "";
    control = `<div class="switch${val ? " on" : ""}" data-key="${f.key}" data-type="bool"${imm}></div>`;
  } else if (f.type === "number") {
    control = `<input type="number" data-key="${f.key}" data-type="number" value="${val}" min="${f.min ?? ""}" max="${f.max ?? ""}" />`;
  } else if (f.type === "select") {
    const opts = (f.options || [])
      .map((o) => `<option value="${o.value}"${String(val) === o.value ? " selected" : ""}>${o.label}</option>`)
      .join("");
    control = `<select data-key="${f.key}" data-type="select">${opts}</select>`;
  } else if (f.type === "hotkey") {
    // 快捷键录入：不再让用户逐字敲 "Alt+V"，而是点击后直接按组合键，
    // 由键盘事件自动识别。值仍存为字符串，保持后端契约不变。
    //
    // data-value 是"本次编辑的值"：初始等于配置里的当前值，
    // 录入后被 onHotkeyCaptureKey 覆写。saveSettings 据此判断是否变过。
    const cur = String(val ?? "");
    control = `<button type="button" class="hotkey-input" data-key="${f.key}" data-type="hotkey" data-value="${esc(cur)}">
        <span class="hotkey-text">${esc(cur) || "点击录入"}</span>
      </button>`;
  } else {
    control = `<input type="text" data-key="${f.key}" data-type="text" value="${esc(String(val ?? ""))}" />`;
  }
  return `
    <div class="setting-row">
      <div class="setting-label">${f.label}${f.desc ? `<small>${f.desc}</small>` : ""}</div>
      ${control}
    </div>`;
}

async function saveSettings() {
  const body = document.getElementById("settings-body")!;
  const next: ClipConfig = { ...state.config };

  body.querySelectorAll("[data-key]").forEach((el) => {
    const node = el as HTMLElement;
    const key = node.dataset.key as keyof ClipConfig;
    const type = node.dataset.type;

    // 即时生效项（如开机自启）已在切换时由后端处理并同步到 state.config，
    // 此处跳过，避免用界面状态覆盖真实值。
    if (node.dataset.immediate === "1") return;

    if (type === "bool") {
      (next[key] as unknown as boolean) = node.classList.contains("on");
    } else if (type === "number") {
      const v = parseInt((node as HTMLInputElement).value, 10);
      (next[key] as unknown as number) = isNaN(v) ? 0 : v;
    } else if (type === "select") {
      const v = (node as HTMLSelectElement).value;
      // select 的值类型取决于目标字段：数字型（如 hideMode）转 int，
      // 字符串型（如 windowPosition）原样保留。早期一律 parseInt，
      // 会让字符串型字段被写成 NaN。
      const current = state.config[key];
      (next[key] as unknown as unknown) =
        typeof current === "number" ? parseInt(v, 10) : v;
    } else if (type === "hotkey") {
      // 录入结果存在 dataset.value 里（见 onHotkeyCaptureKey）；
      // 未重新录入时它等于原值，据此判断快捷键是否变过。
      const captured = node.dataset.value ?? "";
      (next[key] as unknown as string) = captured;
    } else {
      (next[key] as unknown as string) = (node as HTMLInputElement).value;
    }
  });

  // 快捷键变更必须先重绑定并确认成功，再落盘配置。
  // 否则会出现"配置已存但新快捷键没生效"的不一致状态。
  const hotkeyChanged = next.hotkey !== state.config.hotkey;
  if (hotkeyChanged) {
    clearSettingsError();
    try {
      await invoke("rebind_hotkey", { hotkey: next.hotkey });
    } catch (e) {
      // 错误必须显示在设置面板内——底部状态栏被覆盖层遮住，用户看不到
      showSettingsError(String(e));
      return;
    }
  }

  await invoke("save_config", { config: next });
  state.config = next;
  applyTheme();
  applyConfigToDom();
  updateHint();
  closeSettings();
  render();
  setStatus(hotkeyChanged ? "设置已保存，快捷键已生效" : "设置已保存");
}

/** 在设置面板内显示错误信息。 */
function showSettingsError(msg: string) {
  const foot = document.querySelector(".settings-foot")!;
  let box = document.getElementById("settings-error");
  if (!box) {
    box = document.createElement("div");
    box.id = "settings-error";
    box.className = "settings-error";
    foot.parentElement!.insertBefore(box, foot);
  }
  box.textContent = msg;
  box.classList.add("show");
}

function clearSettingsError() {
  const box = document.getElementById("settings-error");
  if (box) box.classList.remove("show");
}

function closeSettings() {
  document.getElementById("settings-overlay")!.classList.remove("show");
  // 关闭设置后不再需要键盘输入，立刻收回临时可聚焦状态
  void releaseSearchFocusable();
}

// ---------- 工具 ----------

function esc(s: string): string {
  return s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

function stripHtml(s: string): string {
  if (!s.startsWith("<") && !s.includes("<!--StartFragment")) return s;
  return s.replace(/<[^>]*>/g, "").replace(/\s+/g, " ").trim();
}

function assetUrl(rel: string): string {
  // 图片以相对路径记录（如 `Image/20261011_120000_123.png`），经 Tauri 的
  // asset 协议加载。
  //
  // **必须用 `encodeURI` 而非 `encodeURIComponent`**：后者会把路径分隔符
  // `/` 一并编码成 `%2F`，得到 `Image%2Fxxx.png`，资源协议按字面路径
  // 找不到文件，表现为"列表里图片位置空白"。
  // `encodeURI` 只转义非法字符，保留 `/`、`:` 等路径结构字符。
  return `http://asset.localhost/${encodeURI(rel)}`;
}

function timeAgo(iso: string): string {
  const t = new Date(iso).getTime();
  if (isNaN(t)) return "";
  const diff = Date.now() - t;
  const min = Math.floor(diff / 60000);
  if (min < 1) return "刚刚";
  if (min < 60) return `${min} 分钟前`;
  const hr = Math.floor(min / 60);
  if (hr < 24) return `${hr} 小时前`;
  const day = Math.floor(hr / 24);
  if (day < 30) return `${day} 天前`;
  return new Date(t).toLocaleDateString("zh-CN");
}

init().catch((e) => {
  document.getElementById("list")!.innerHTML = `<div class="empty"><div class="big">⚠</div><div>初始化失败</div><small>${esc(String(e))}</small></div>`;
});
