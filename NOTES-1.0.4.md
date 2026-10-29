# Cursor BYOK 1.0.4-zhao — wire fixes review notes

Branch `zhao/byok-1.0.4-wire-fixes`, based on `main` (d4d5342, 1.0.1-zhao). Commits are ordered per fix:

1. **1.0.2 wire_sanitize**: CSS repair (drop mistyped CSS fields, truncate garbage, UUID-as-CSS → empty CSS) plus a clear_css fallback. Wire dumps go to `~/.cursor-byok-v3/logs/wire-dumps`.
2. **1.0.3 prune_run_request_fields**: stage 3 drops AgentRunRequest top-level fields that fail to decode on their own. This commit turned out to be **wrong** for the payload it targeted; see root cause 2.
3. **1.0.4 RunInference detection**: the actual fix.
4. **1.0.4 Anthropic user-terminated guard**: safety net.
5. **Version bump and these notes.**

## Root causes

### 1. "protobuf decode error: ConversationStateStructure.summary ... ThirtyTwoBit" (request bb9c24b5)
Cursor 3.27.22 (installed 2026-10-27) started sending some agent turns as
`aiserver.v1.InferenceService/RunInference*`, which goes over the **same** `/aiserver.v1.BidiService/BidiAppend`
as `agent.v1.AgentService/Run`. BYOK always decoded `BidiAppend.data` as `agent.v1.AgentClientMessage`.
The real message is `aiserver.v1.RunInferenceClientMessage { run_request = 1: RunInferenceRunRequest }`, and
`RunInferenceRunRequest` has field 1 = `conversation_id` (UUID string). Read as an AgentRunRequest, that field is
`conversation_state`, and the ASCII byte `'5'` (0x35) parses as tag "field 6 (summary), wire type 5".

Schema from Cursor's `workbench.glass.main.js` (3.27.22):
```
aiserver.v1.RunInferenceClientMessage: 1 run_request(RunInferenceRunRequest) 2 invoke_model 3 cancel_invocation
  4 finish_run 5 client_heartbeat 6 agent_event 7 run_request_log 9 resolve_subagent_models 10 client_first_call_timings
aiserver.v1.RunInferenceRunRequest: 1 conversation_id(str) 2 conversation_group_id 3 requested_model 4 routing_conversation(rep)
  5 agent_mode(str) 6 subagent_type_name 7 selected_subagent_models(rep) 8 subagent_model_overrides 9 is_summarize_action
  10 supports_long_reasoning_reminder 11 agent_module 12 previous_request_id 15 execute_plan 16 turn_agent_mode
  17 supports_agent_loop_error 18 supports_delta_invocations 19 cached_config_fingerprint 20 speculative_invocations
  21 sends_client_first_call_timings 22 prewarm 25 supports_model_handles
service aiserver.v1.InferenceService { RunInference (bidi), RunInferenceSSE, RunInferencePoll, Stream, ... }
```
`agent.v1.AgentRunRequest` / `ConversationAction` / `UserMessageAction` in that bundle match our
`protocols/cursor/agent_v1.proto`, so the agent schema had **not** drifted. In the BYOK logs, conversation 58ca631b ran
normally through AgentService until 09:56Z and hit this error from 10:06Z on.

### 2. "Anthropic 400: This model does not support assistant message prefill" (run 008f9eb0, req_011CgWNe9cM1XP4JJutMHDV8)
1.0.3's prune "recovered" the RunInference payload into a fake AgentRunRequest:
- It dropped field 4 (`routing_conversation` = `{1:1, 2:"继续下"}`, which is the user's text).
- It dropped the other mistyped fields (7, 12, 17, 18, 25).
- It kept field 5 `agent_mode="agent"`, which became `conversation_id="agent"`.
- It left no `action` and an empty CSS.

BYOK's built-in Anthropic provider (`provider_type=anthropic`, `https://toknex.me/v1/messages`,
model `claude-opus-5-5`) then sent **0 messages**. llm_calls shows `message_count=0, http_status=400`.
toknex/Anthropic returns the prefill error for an empty `messages` array.

## Fix (1.0.4)
- `wire_sanitize::is_run_inference_client_message`: the payload counts as RunInference when the ACM's field 1 has an inner
  field 1 that is a 36-character UUID string **and** an inner field 3 that is length-delimited (LD). In that case sanitize,
  clear_css and prune all return `None`.
- `handlers.rs::bidi_handler`: RunInference appends get `registry.mark_upstream(request_id)` and are forwarded
  to Cursor unchanged. A later append that fails to decode on a request id already marked upstream is also forwarded.
- `prune_run_request_fields` refuses to drop ARR field 2 (`action`). `bidi.rs` rejects a pruned RunRequest
  whose action is missing.
- `provider/anthropic.rs::user_terminated_messages`: pops trailing assistant turns that have no `tool_use`.
  It returns `Error::Protocol` when a trailing assistant has a `tool_use` without its `tool_result`, or when no messages
  are left. It never invents user text. This complements the existing `run/history.rs::user_terminated`, which works on
  projected history; the gap was empty user parts being dropped during serialization, or an empty history.

## Dumps used (`docs/wire-dumps/`)
- `bb9c24b5-1793268574329-raw.hex`: 586 B, RunInference run_request (conversation 58ca631b..., no routing text).
- `008f9eb0-1793269850613-raw.hex`: 976 B, RunInference run_request with `routing_conversation` "继续下".

## Tests
`cargo test -p cursor-server --lib`: 169/169 pass, on the Linux box and in the Windows 1.0.4 build on the laptop.
- `wire_sanitize::tests::{plan_fixed64_is_dropped_and_decodes, uuid_as_css_is_run_inference_not_repaired,
  clean_baseline_unchanged_and_decodes, truncate_garbage_tail, clear_conversation_state_fallback}`
- `wire_sanitize::regression_run_inference::{wire_sanitize_run_inference_payloads_are_detected_and_not_repaired,
  wire_sanitize_real_acm_is_not_run_inference, wire_sanitize_prune_refuses_to_drop_action}`
- `provider::anthropic::tests::user_terminated_messages_{rejects_empty, drops_trailing_text_prefill,
  only_assistant_is_error, rejects_dangling_tool_use, keeps_tool_result_tail}`

Windows installer (not yet installed): `Cursor BYOK_1.0.4_x64-setup.exe`, 20,901,914 B,
SHA256 `F827913C60B197FEEA17BDAF3F8044B34C8C42730A707BF2AE3DEBE38A1CD536`.

## Open questions / not done
- **RunInference is not implemented.** Forwarded turns reach Cursor's backend with a BYOK model id
  (`d41538c3ec5dbddc`), which Cursor will probably reject. Serving them locally means implementing
  `RunInferenceSSE` + `invoke_model` → provider → `RunInferenceServerMessage` (`$zb`). Also unknown: what makes Cursor pick
  RunInference over AgentService/Run (a server flag? the conversation's state?).
- **Re-extract the proto** from Cursor 3.27.22 into `protocols/cursor/` (an `aiserver_v1` RunInference* set) so detection
  can use real prost types instead of the UUID heuristic. The heuristic would misclassify a real ACM whose CSS is exactly
  a UUID string and which also has model_details. That seems unlikely, but it's untested against real traffic.
- **openai_chat / openai_responses providers** don't have the empty/assistant-terminated guard (Anthropic only).
- **Is the 1.0.3 prune stage still useful?** It now only runs for real ACMs and can't drop `action`. Consider removing it.
- **Stale row:** 1.0.3 left a bogus conversation `agent` (checkpoint 75) in the user's cursor-byok.db. It hasn't been cleaned up.
- **Updater key:** the tauri updater pubkey doesn't match the local signing key (`.tauri/cursor-byok.local.key`).
  The installer is fine, but auto-update signatures won't verify.
- **Old dumps (10:14Z) with `ConversationStateStructure.file_states` errors** weren't analysed. They may be RunInference
  too, or real CSS drift.
