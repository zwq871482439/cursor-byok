# Zhao 维护说明（1.0.1-zhao）

本仓库是 [leookun/cursor-byok](https://github.com/leookun/cursor-byok) 的 fork（上游已 archived，停在官方 `v1.0.1`）。  
Zhao / 领航用它在 Cursor Agent 协议漂移时自己打 **`1.0.1-zhao.N`** 补丁版，不依赖上游发版。

本地工作副本（Zhao 本机）：`C:\Users\slow\tools\cursor-byok-src`  
- `origin` → `https://github.com/zwq871482439/cursor-byok.git`  
- `upstream` → `https://github.com/leookun/cursor-byok.git`（只读对照）

## 什么时候要更新

看 BYOK 日志 `~/.cursor-byok-v3/logs/cursor-byok.YYYY-MM-DD.log`（Windows：`%USERPROFILE%\.cursor-byok-v3\logs\`）出现：

- `protobuf decode error` + `AgentRunRequest.conversation_state`
- `invalid wire type value: 6` / `7`
- `ConversationStateStructure.file_states` + `buffer underflow` / `unexpected end group tag`

同时仍有 `routing Cursor Run to BYOK provider` 成功记录 → 典型「长对话 / 带 file_states 的请求」踩到旧 proto，不是整机挂死。

症状：Cursor 里选 BYOK 模型，长会话报 400；**新开短对话可能仍正常**。

## 版本号约定

对外包名继续叫 cursor-byok，版本串：

| 位置 | 字段 |
|------|------|
| `apps/desktop/package.json` | `"version": "1.0.1-zhao.N"` |
| `apps/desktop/src-tauri/tauri.conf.json` | `"version": "1.0.1-zhao.N"` |

- `N` 从 `0` 起：仅文档/流程为 `1.0.1-zhao.0`；每次成功「重抽 proto + 能编过 + 长对话验证」再 +1。  
- Git tag：`v1.0.1-zhao.N`（与上面一致）。  
- 不要改成无的 major，方便对照官方 `1.0.1` 行为。

## 更新路子（出问题后按这个做）

### 0. 前置

- 已装：Rust（cargo）、Go、protoc、Node/npm、Tauri 2 桌面构建依赖  
- Cursor 已更新到要兼容的版本，并**完全退出**后再抽协议（避免抽到半更新文件）

### 1. 从当前 Cursor 重抽 proto

仓库自带抽取器：`support/cursor-protocol-extractor/`。

**Windows（Zhao 机）推荐直接 `go run`（与 extract.sh 等价）：**

```powershell
$CursorRoot = "$env:LOCALAPPDATA\Programs\cursor"
$Repo = "C:\Users\slow\tools\cursor-byok-src"
$Out = "$Repo\protocols\cursor-new"
New-Item -ItemType Directory -Force -Path $Out | Out-Null

Push-Location "$Repo\support\cursor-protocol-extractor"
go run ./extractor `
  -input "$CursorRoot\resources\app\out\vs\workbench\workbench.desktop.main.js" `
  -input "$CursorRoot\resources\app\out\vs\workbench\api\node\extensionHostProcess.js" `
  -input "$CursorRoot\resources\app\extensions\cursor-always-local\dist\main.js" `
  -output $Out `
  -skip-format
Pop-Location
```

有 Git Bash / WSL 时也可用：

```bash
./support/cursor-protocol-extractor/scripts/extract.sh \
  "/c/Users/slow/AppData/Local/Programs/cursor" \
  "./protocols/cursor-new"
```

对照体积：新 `agent_v1.proto` / `aiserver_v1.proto` 通常比旧的大一截才说明抽到了新字段。

### 2. 替换协议源

Rust server 只吃 `protocols/cursor`（见 `server/build.rs` → `agent_v1.proto` → prost）：

```powershell
$Repo = "C:\Users\slow\tools\cursor-byok-src"
# 备份旧协议
Copy-Item -Recurse "$Repo\protocols\cursor" "$Repo\protocols\cursor-backup-$(Get-Date -Format yyyyMMdd-HHmm)"
# 用新抽取覆盖（保留目录名 cursor）
Remove-Item -Recurse -Force "$Repo\protocols\cursor"
Move-Item "$Repo\protocols\cursor-new" "$Repo\protocols\cursor"
```

若 `make check` / `cargo build` 因新增/改名字段编译失败：按编译错误改 `server/src/cursor/**` 里对旧字段的假设（oneof、可选字段），不要手改 `.proto` 去「迁就」旧代码——以 Cursor 抽出的定义为准。

### 3. bump 版本并构建

```powershell
# 手工把 package.json + tauri.conf.json 的 version 改成 1.0.1-zhao.N
cd C:\Users\slow\tools\cursor-byok-src
npm --prefix apps/desktop install
make check          # 能过再往下
make build-desktop  # 产出 Tauri 安装包
```

安装新桌面端后：停掉旧 `cursor-byok-desktop`，确认代理仍是 `http://127.0.0.1:3755`，再开 Cursor。

### 4. 验收

1. 新开短对话，BYOK 模型能回，日志有 `routing Cursor Run to BYOK provider`。  
2. **用之前会炸的长对话**再发一条：不应再刷 `protobuf decode error` / `wire type` / `file_states` underflow。  
3. 通过后：`git commit` + `git tag v1.0.1-zhao.N` + `git push origin main --tags`。

### 5. 失败时的临时办法

- 新开对话（避开坏的 `conversation_state`）  
- 或临时清 Cursor `http.proxy`、停 BYOK，改走官方/直连（不改本 fork）

## 不要做的事

- 不要指望上游再发修协议的 release（repo archived）。  
- 不要把 Override Base URL 指到 `127.0.0.1`（Cursor 云端会 SSRF 403）；BYOK 走本地 MITM 代理。  
- 不要用公网隧道「绕过」本地 BYOK，除非 Zhao 明确要求。

## 相关现象备忘（2026-10）

- 日志里从约 **2026-10-13** 起就有 `invalid wire type value: 7`；**10-25** 起多见 `file_states` 解码失败。  
- 同一天可同时存在成功 routing 与大量 400 → 协议漂移是间歇踩坑。  
- Cursor 论坛同类报错，官方常指向本机 MITM/第三方代理（即 BYOK 这类）。
