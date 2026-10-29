# 2026-10-29 Cursor Agent 协议漂移排查记录

> 结论先行：Cursor 通过服务端下发的 agent-harness 模块（`managed-local-run`）把 Agent 循环挪到了客户端本地，
> 上行请求从 `AgentService.Run` / `AgentClientMessage{AgentRunRequest}` 变成了
> **`InferenceService.RunInference` / `RunInferenceClientMessage{RunInferenceRunRequest}`**，
> 但仍走同一个 `BidiAppend` 通道。cursor-byok 仍按 `AgentClientMessage` 解码，所以报 decode error；
> 1.0.3 的 `wire_sanitize` 把它“修”到能解，但实际上丢掉了用户消息，进而触发 Anthropic prefill 400。

环境：Windows，Cursor 3.27.22（product.json date 2026-10-27），agent-harness generation `b951911e…`
（`mainVersion=3.29.0-pre.23`，sequence 316，builtAt 2026-10-28T20:52Z），channel `stable`。

---

## 1. 时间线（北京时间）

| 时间 | 事件 |
|---|---|
| 10-27 01:15 | Cursor 3.27.22 安装 |
| 10-28 全天 | BYOK 日志 0 次 decode error |
| 10-29 12:13 | 最后一次正常的 BYOK 对话 |
| 10-29 12:32 | 第一次 `protobuf decode error`（`ConversationStateStructure.summary`）|
| 12:32–16:25 | 偶有成功（58 次 tool round）→ 服务端 gate 灰度切换 |
| 15:28 | harness 模块 `b951911e`（seq 316）开始被 serve（`last-served.json`）|
| 18:04 | 换 1.0.2，仍失败 |
| 18:30 | 换 1.0.3（wire_sanitize），decode 恢复，但出现 Anthropic prefill 400 |

累计：当日 201 次 decode error（168 次 `summary`、12 次 `file_states`、21 次其他）。

## 2. 证据链

### 2.1 Cursor 侧：managed-local-run

- `%APPDATA%\Cursor\agent-harness\`
  - `current` → `b951911e447652508ba64b4b209ceb5762710e1423deddf7082a6be93bca7cbc`
  - `versions\<gen>\manifest.json`：`capabilities = [managed-local-run, seam-run-options, turn-prewarm]`，带 ECDSA 签名（`manifest.sig`，keyId `harness-prod-1`）
  - `hosts\<pid>.json`：`"enabledBy": "gate"`、`"requiredCapabilities": ["managed-local-run"]`，floor 为 `static`
- Agent Host 日志：`[agent-harness] gen b951911e4476 … transport created (managed-local-run, seam-run-options, turn-prewarm)`、
  `RunInference attempt opened`；harness 代码中 `reconnectEndpoint: "InferenceService.RunInference"`。
- **static floor 本身也要求 `managed-local-run`**，删掉下载的 generation 回退不到旧协议。
- 开关由服务端 gate 控制；唯一相关 env 是 `CURSOR_AGENT_HARNESS_EAGER_LOAD`（与禁用无关）。

### 2.2 字节级证据：上行包其实是 RunInferenceRunRequest

用 `tools/pbdump.ps1` 解 `~/.cursor-byok-v3/logs/wire-dumps/*-fail.txt`：

```
msg.1                              ← ClientMessage field 1 (run_request)
  .1  "58ca631b-3ec0-4603-…"       ← conversation_id (string)
  .3  {1:"d41538c3ec5dbddc", 3:{context=500k}, 3:{reasoning=high}, 3:{fast=false}}  ← requested_model
  .4  {1: 1, 2: "继续"}            ← routing_conversation（RunInferenceRoutingMessage{role,text}）
  .5  "agent"                      ← agent_mode
  .7  {default} {grok-4.7 …} …     ← selected_subagent_models (repeated)
  .11 {gen b951911e…, "stable", commit} ← agent_module
  .17/.18/.25 varint               ← supports_agent_loop_error / supports_delta_invocations / supports_model_handles
```

| 字段号 | RunInferenceRunRequest（实际） | AgentRunRequest（BYOK 解码用） |
|---|---|---|
| 1 | `conversation_id` string | `conversation_state` message |
| 3 | `requested_model` | `model_details` |
| 4 | `routing_conversation`（**用户消息**） | `mcp_tools` |
| 5 | `agent_mode` | `conversation_id` |
| 7 | `selected_subagent_models` | `skill_options` |
| 11 | `agent_module` | `subagent_type_name` |

#### 为什么报错字段随会话变化

BYOK 把 `conversation_id` 的 UUID ASCII 当作 `ConversationStateStructure` 解码，首字节被当成 tag：

- `'5'` = 0x35 → field 6 / wire type 5 → `summary: invalid wire type: ThirtyTwoBit`
- `'d'` = 0x64 → field 12 / wire type 4 → `file_states …`
- 其他字符 → `invalid wire type value: 6/7` 等

所以“新开对话也坏”，且每个对话报的字段不同。

### 2.3 harness generation 之间的 schema diff

`57c5831e`（seq 298）→ `b951911e`（seq 316）只有少量追加字段（`AgentRunRequest.37 emit_model_call_usage`、
`RunInferenceRunRequest.26/27`、`RunInferenceClientMessage.11 delta_base_seed` 等），
**`AgentRunRequest` / `ConversationStateStructure` 字段号未变**。
所以问题不是 proto 过期，而是**请求类型换了**，重抽 `agent_v1.proto` 修不好。

## 3. 1.0.3 的 Anthropic prefill 400：根因

日志（2026-10-29T10:30:50Z）：

```
AgentClientMessage decode failed; trying wire_sanitize        (summary ThirtyTwoBit)
wire_sanitize still failed; trying clear_conversation_state   (McpTools.mcp_tools Varint)
AgentClientMessage recovered via prune_run_request_fields
  notes=["drop ARR field 4 wire=2 len=15 @ 73", "drop ARR field 7 …" x10,
         "drop ARR field 12 …", "drop ARR field 17/18/25 …"]
routing Cursor Run to BYOK provider … model_id="d41538c3ec5dbddc"
run{… conversation_id=agent}: Run claimed conversation ownership checkpoint_id=75
Run ended outcome=Failed(Provider("Anthropic 400 … does not support assistant message prefill …"))
```

逐条解释：

1. **丢掉的 field 4 就是用户这次发的消息**（`routing_conversation`，len=15，内容约为 `{1:1, 2:"继续"}`）。
2. field 1（UUID）被 `clear_conversation_state` 清空，于是既没有 `conversation_state`，也没有 `action`，
   这一轮**没有新的用户输入**。
3. field 5 `"agent"`（实际是 `agent_mode`）被当成 `AgentRunRequest.conversation_id`，日志里因此出现
   `conversation_id=agent`。结果是**所有会话都落到同一个 BYOK 会话 `agent`**，并复用它的旧 checkpoint（id=75），
   而这段历史以 assistant 结尾。
4. 历史以 assistant 结尾，又没有新的 user 消息，Anthropic 会把请求当成 prefill，返回 400。

`server/src/run/history.rs::user_terminated` 只在 `last.role == Role::Assistant` 时补一条 transient user 消息。
`provider/anthropic.rs::anthropic_messages()` 会跳过 content 为空的消息（`if !content.is_empty()`），
跳过之后末尾可能变成 assistant，这种情况下 `user_terminated` 不会补，也会触发 400。
不过**主因是上面 1–3 条**：即使在 provider 层补一条 "continue"，模型也拿不到用户真正说的话，
而且不同会话会串到同一个 `agent` 会话里。

### 修复方向（按优先级）

1. **不要用 prune 去“兼容”RunInference 包**。在 `bidi.rs` 里先判断消息类型：
   如果 ARR 的 field 1 是 UUID ASCII 字符串（已有 `looks_like_uuid_ascii`），且 field 5 是 `agent`/`ask`/`plan` 这类短字符串，
   就按 `RunInferenceClientMessage` 解码，走新的处理分支；否则才按 `AgentClientMessage` 处理。
2. **实现 RunInference 最小闭环**（见第 4 节）。`RunInferenceRunRequest.routing_conversation[].text` 只是路由用的摘要，
   真正发给模型的完整 prompt 在后续 `invoke_model.request`（或 `compressed_request`）里。
3. 过渡期止血（短期内只想先让对话跑通时）：
   - prune 之后如果 `conversation_state` 和 `action` 都为空，**直接返回明确的错误**，不要继续跑 Run；
   - `conversation_id` 改为取原 field 1 的 UUID，不要用 `"agent"`，避免会话串线；
   - 在 `anthropic_messages()` 生成的 JSON 上再做一次“末尾必须是 user”的兜底。
     这只能消掉 400，丢掉的用户消息依然找不回来。

## 4. RunInference 协议摘要（来自 b951911e 内置 schema）

```
Cursor agent-harness（Agent 循环在本地，工具也在本地执行）
   │  BidiAppend(seqno, data_binary = RunInferenceClientMessage)
   ▼
RunInferenceClientMessage (oneof message)
  1 run_request        RunInferenceRunRequest      ← 首帧
  2 invoke_model       RunInferenceInvokeModel     ← 每次需要调用模型时
  3 cancel_invocation  {1 invocation_id}
  4 finish_run         {1 aborted, 2 turn_completed?, 3 tool_call_batches*}
  5 client_heartbeat
  6 agent_event / 7 run_request_log / 9 resolve_subagent_models
  10 client_first_call_timings / 11 delta_base_seed

RunInferenceServerMessage (oneof message)          ← 经 RunSSE 下行
  1 heartbeat           {1 accepts_speculative_invocations, 2 accepts_compressed_speculative_invocations}
  2 run_ready           RunInferenceRunReady       ← 必须最先回
  3 invocation_response {1 invocation_id, 2 response}   ← 流式模型输出
  4 invocation_end      {1 invocation_id, 2 error?, 3 delta_refused?}
  5 subagent_models_resolved

RunInferenceInvokeModel
  1 invocation_id  2 request  3 purpose  4 tool_call_batches*  5 delta_base?
  6 config_fingerprint?  7 compressed_request{1 encoding, 2 decoded_size, 3 data}  8 request_encoding_arm
```

最小可用实现建议：

- `run_ready`：`resolved_model` 直接回 requested_model；**能力位全部置 false**（`accepts_delta_invocations`、
  `accepts_client_heartbeat`、`accepts_agent_events` 等），`request_compression` 不设置。
  这样客户端会退回最简单的模式：每次 invoke_model 都带完整 request，不压缩，也不用 delta。
- `invoke_model`：取出未压缩的 `request`，转成 OpenAI/Anthropic 请求，先流式回 `invocation_response`，最后回 `invocation_end`。
  工具调用由客户端执行，服务端不需要 `exec_server_message`。
- `request` / `response` 在 schema 里写作 `#0` 这样的引用，具体类型要到 harness cjs 里找：
  每个消息 `static $()` 返回的数组从第 2 项开始，按顺序对应 `#0`、`#1`……引用的类。
- `run_ready` 如果回错，客户端会报 `RunInference run_ready differed from the one the attempt assumed`。

## 5. 文件说明

```
docs/protocol-drift-2026-10-29/
├── README.md                          # 本文
├── schema/
│   ├── harness-7e7b687d.schema.txt    # seq 265, 3.28.0-pre.49, 2384 messages
│   ├── harness-57c5831e.schema.txt    # seq 298, 3.29.0-pre.7,  2387 messages
│   ├── harness-b951911e.schema.txt    # seq 316, 3.29.0-pre.23, 2389 messages（当前）
│   └── RunInference-b951911e.txt      # 从上一份中过滤出的 RunInference* 消息
└── tools/
    ├── extract-harness-schema.ps1     # 从 %APPDATA%\Cursor\agent-harness 抽 schema，可用 -Diff 对比旧版
    └── pbdump.ps1                     # 解析 wire-dumps/*.txt 里的 raw_hex，打印 protobuf 字段树
```

schema 行格式：`Message|<字段号> <字段名> <类型>[?|*]|…`。类型代码：`8=bool 9=string 12=bytes 13=uint32 5=int32 3/4=int64 1=double`；
`#N` 表示引用的第 N 个消息/枚举，`k,v` 表示 map，行尾的单词是所属的 oneof。

```powershell
# 每次 Cursor 下发新的 harness 后
.\tools\extract-harness-schema.ps1 -Diff b951911e
# 解析一个失败包
.\tools\pbdump.ps1 -File "$env:USERPROFILE\.cursor-byok-v3\logs\wire-dumps\<id>-fail.txt" -MaxDepth 3
```

## 6. 注意

- 本项目是对第三方协议的逆向，可能违反 Cursor 服务条款。Cursor 平均每 1–2 天推送一个新的 harness generation。
- `~/.cursor-byok-v3/ca/ca.key` 是本机 MITM 根证书的私钥，不要外传。
- wire-dumps 里有明文对话内容，提交前一定要脱敏。本目录没有包含任何 dump 原文。

