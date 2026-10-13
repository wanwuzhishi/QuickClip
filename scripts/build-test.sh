#!/usr/bin/env bash
# QuickClip 测试版打包脚本。
#
# 用途：每次代码变动后执行，产出一个带版本标记、可直接分发的测试版。
# 行为：
#   1. 递增 patch 版本号（0.1.N -> 0.1.N+1）
#   2. 版本号写入 package.json 与 tauri.conf.json，保证两者一致
#   3. 物理清空 dist/（绕开宿主环境的回收站超时问题）
#   4. 执行 Tauri 完整构建（前端 + Rust + NSIS 安装包）
#   5. 把产物收集到 dist-release/，清理旧版本（只留最近 3 个）
#   6. 生成 build-info.txt 记录本次构建信息
#
# 用法：bash scripts/build-test.sh

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

STAMP="$(date +%Y%m%d_%H%M%S)"
OUT_DIR="$ROOT/dist-release"
mkdir -p "$OUT_DIR"

echo "==> [1/6] 计算测试版本号"

# 读取 app 当前版本（以 tauri.conf.json 为准，保持单一事实来源）
CUR_VER="$(node -e "console.log(require('./src-tauri/tauri.conf.json').version)")"
IFS='.' read -r MAJ MIN PATCH <<< "$CUR_VER"
NEXT_PATCH=$((PATCH + 1))
NEW_VER="${MAJ}.${MIN}.${NEXT_PATCH}"
TEST_TAG="v${NEW_VER}-test.${STAMP}"
echo "     ${CUR_VER} -> ${NEW_VER}  (标签 ${TEST_TAG})"

echo "==> [2/6] 同步版本号到配置文件"
# 三处必须一致：package.json、tauri.conf.json、Cargo.toml。
#
# Cargo.toml 尤其不能漏：`commands::bootstrap` 用 `env!("CARGO_PKG_VERSION")`
# 把版本号报给前端，三处不一致时界面会显示一个错误的旧版本号
# （曾出现 package.json 已是 0.1.19 而 Cargo.toml 仍是 0.1.0，
# 界面上一直显示 0.1.0）。
node -e "
const fs=require('fs');
const ver='${NEW_VER}';
for (const f of ['package.json','src-tauri/tauri.conf.json']) {
  const j=JSON.parse(fs.readFileSync(f,'utf8'));
  j.version=ver;
  fs.writeFileSync(f, JSON.stringify(j,null,2)+'\n');
  console.log('     已更新', f);
}
let cargo=fs.readFileSync('src-tauri/Cargo.toml','utf8');
cargo=cargo.replace(/^version = \".*\"$/m, 'version = \"'+ver+'\"');
fs.writeFileSync('src-tauri/Cargo.toml', cargo);
console.log('     已更新 src-tauri/Cargo.toml');
"

echo "==> [3/6] 准备前端产物目录"
# 物理删除 dist/，然后交给 `tauri build` 的 beforeBuildCommand 去构建前端。
#
# 为什么不在这里先 `npm run build`：
# `tauri build` 内部还会再执行一次 beforeBuildCommand（配置里就是 `npm run build`），
# 前端会被构建两遍——第一遍的产物随即被第二遍清空重建，纯属浪费。
# 只构建一次，既快又避免"dist 非空 → Vite 清空 → 回收站超时"这条失败路径。
#
# 为什么必须物理删除而非让 Vite 自己清：
# Vite 的 prepareOutDir 走 `fs.rmSync`，在带 safe-delete shim 的宿主环境里
# 会被代理到"移入回收站"，该调用可能超时（ETIMEDOUT）导致构建整体失败。
# dist/ 是构建产物，无需保留，物理删除即可绕开。
rm -rf "$ROOT/dist" && mkdir -p "$ROOT/dist"

# 同时清空历史安装包目录。
#
# 这一步是必需的，不是清理癖：`tauri build` 生成的 NSIS 安装包**不会**
# 覆盖旧文件，而是按版本号追加新文件（QuickClip_0.1.21_x64-setup.exe、
# QuickClip_0.1.22_x64-setup.exe 会长期共存）。实测该目录曾堆积 19 个，
# 最早的可追溯到改名前的 ClipForge_0.1.1。
#
# 若保留这些残留，后续"哪个安装包是本次构建的"就无法从目录状态判断，
# 只能靠文件名精确匹配——而只要匹配规则写错一次（历史上正是如此），
# 用户就会拿到一个版本号标错、内容是完全旧版本的程序。
#
# 这些文件在 dist-release/ 里都有归集副本（且已按 3 个版本上限清理），
# 删除不会丢失任何东西。
rm -rf "$ROOT/src-tauri/target/release/bundle/nsis"
mkdir -p "$ROOT/src-tauri/target/release/bundle/nsis"

echo "==> [4/6] 构建 Tauri 应用与安装包（耗时较长，请耐心等待）"
npm run tauri build

echo "==> [5/6] 收集产物到 dist-release/"
BUNDLE="$ROOT/src-tauri/target/release/bundle"
EXE="$ROOT/src-tauri/target/release/quickclip.exe"

# 收集产物。任一必需产物缺失都**立即失败**，不静默跳过：
# 少一个文件的"成功构建"比直接报错更危险——用户拿到的是不完整的包。
collect() {
  local src="$1" name="$2"
  if [ ! -f "$src" ]; then
    echo "错误：找不到预期的构建产物 $src" >&2
    echo "      （版本 ${NEW_VER}，标签 ${TEST_TAG}）" >&2
    exit 1
  fi
  cp "$src" "$OUT_DIR/$name"
  echo "     $name"
}

collect "$EXE" "QuickClip_${TEST_TAG}_portable.exe"

# NSIS 安装包：**按本次版本号精确匹配文件名**。
#
# 绝不能用 `ls ... | head -1` 之类"取目录里第一个"的写法。
# 曾经正是那么写的，而目录里堆积着从 ClipForge_0.1.1 起的 19 个安装包，
# `ls` 按字典序排下去 head -1 取到的是 ClipForge_0.1.1——于是安装包被标上
# 新版本号、内容却是改名前的第一个旧版本。用户看到的现象就是
# 「安装版和便携版版本号相同，实际版本不一样，安装版还是旧版本」。
#
# 现在双保险：上面第 3 步已清空该目录，此处再按精确文件名取，
# 取不到就直接报错，不再有"取错一个旧包"的可能。
NSIS="$BUNDLE/nsis/QuickClip_${NEW_VER}_x64-setup.exe"
collect "$NSIS" "QuickClip_${TEST_TAG}_setup.exe"

echo "==> [6/6] 清理旧产物（只保留最近 ${KEEP_VERSIONS:-3} 个版本）"
KEEP_VERSIONS="${KEEP_VERSIONS:-3}"

# 列出 `dist-release/` 下所有产物。
#
# 为什么不用 `ls "$OUT_DIR"/QuickClip_*_test.*.exe` 这类通配符：
# 在部分 Git Bash 环境（含非交互 `bash -c`）下，含多个 `*` 的路径
# 不会展开，整串会作为字面量传给 `ls` 并报 "No such file or directory"，
# 被 `2>/dev/null` 吞掉后表现为"一个文件都没找到"，清理逻辑静默失效。
# 同理 `mapfile < <(...)` 与 `shopt -s nullglob` + 数组在该环境下也
# 会返回空列表。改为「列出目录 + 正则过滤」最稳。
LIST_FILE="$(mktemp)"
trap 'rm -f "$LIST_FILE"' EXIT
ls -1 "$OUT_DIR" 2>/dev/null \
  | grep -E '^(QuickClip|ClipForge)_v[0-9]+\.[0-9]+\.[0-9]+-test\..*\.exe$' \
  > "$LIST_FILE" || true

# 按版本序排序取最新 N 个。
#
# 注意：必须用 `sort -V`（版本序），不能按字符串——字符串序下
# `0.1.9` 排在 `0.1.10` 之后，会导致 0.1.10 / 0.1.11 被当成旧版本误删。
#
# 保留列表**按行**分隔：`grep -qx` 是整行匹配，若同一行塞多个版本
# （如 "0.1.11 0.1.12"）则永远匹配不上，一个文件都删不掉。
KEEP_LIST="$(
  grep -oE '[0-9]+\.[0-9]+\.[0-9]+' "$LIST_FILE" | sort -uV | tail -n "$KEEP_VERSIONS"
)"

# 版本号相同的 portable 与 setup 必须同生共死，否则用户会拿到不配套的组合
while IFS= read -r name; do
  [ -n "$name" ] || continue
  ver="$(printf '%s' "$name" | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1)"
  [ -n "$ver" ] || continue
  if ! printf '%s\n' "$KEEP_LIST" | grep -qx "$ver"; then
    rm -f "$OUT_DIR/$name"
    echo "     已删除 $name"
  fi
done < "$LIST_FILE"

echo "     保留版本：$(printf '%s ' $KEEP_LIST)"

# 记录构建信息，便于回溯测试的是哪一版
#
# 同样避免通配符：改用 `ls -1 <dir> | grep` 匹配本版本产物。
list_with_size() {
  # $1 = 匹配正则；输出 "  文件名  (大小)"
  ls -1l "$OUT_DIR" 2>/dev/null \
    | grep -E "$1" \
    | awk '{printf "  %s  (%s)\n", $9, $5}'
}

{
  echo "构建时间: $(date '+%Y-%m-%d %H:%M:%S')"
  echo "版本号 : ${NEW_VER}"
  echo "测试标签: ${TEST_TAG}"
  echo "平台   : Windows x64"
  echo ""
  echo "本次产物:"
  list_with_size "^.*QuickClip_${TEST_TAG}.*\.exe$"
  echo ""
  echo "dist-release/ 现有全部产物:"
  list_with_size "\.exe$"
} > "$OUT_DIR/build-info.txt"

echo ""
echo "============================================"
echo " 打包完成：${TEST_TAG}"
echo " 输出目录：$OUT_DIR"
echo "============================================"
list_with_size "^.*QuickClip_${TEST_TAG}.*\.exe$"
