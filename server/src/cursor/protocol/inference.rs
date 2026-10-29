//! Hand-written prost subset of `aiserver.v1.InferenceService/RunInference`.
//!
//! Field numbers come from the agent-harness schema extracted into
//! `support/harness-schema/runinference.closure.txt` (harness b951911e).
//! Only fields BYOK reads or writes are declared; prost skips the rest.

/// Client -> server envelope (BidiAppend payload).
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RunInferenceClientMessage {
    #[prost(oneof = "client_message::Message", tags = "1, 2, 3, 4, 5")]
    pub message: Option<client_message::Message>,
}

pub mod client_message {
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Message {
        #[prost(message, tag = "1")]
        RunRequest(super::RunInferenceRunRequest),
        #[prost(message, tag = "2")]
        InvokeModel(super::RunInferenceInvokeModel),
        #[prost(message, tag = "3")]
        CancelInvocation(super::RunInferenceCancelInvocation),
        #[prost(message, tag = "4")]
        FinishRun(super::RunInferenceFinishRun),
        #[prost(message, tag = "5")]
        ClientHeartbeat(super::Empty),
    }
}

#[derive(Clone, Copy, PartialEq, ::prost::Message)]
pub struct Empty {}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RunInferenceRunRequest {
    #[prost(string, tag = "1")]
    pub conversation_id: String,
    #[prost(message, optional, tag = "3")]
    pub requested_model: Option<InferenceRequestedModel>,
    #[prost(string, optional, tag = "5")]
    pub agent_mode: Option<String>,
    #[prost(string, optional, tag = "6")]
    pub subagent_type_name: Option<String>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct InferenceRequestedModel {
    #[prost(string, tag = "1")]
    pub model_id: String,
    #[prost(bool, tag = "2")]
    pub max_mode: bool,
    #[prost(message, repeated, tag = "3")]
    pub parameters: Vec<InferenceModelParameterValue>,
    #[prost(bool, tag = "4")]
    pub built_in_model: bool,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct InferenceModelParameterValue {
    #[prost(string, tag = "1")]
    pub id: String,
    #[prost(string, tag = "2")]
    pub value: String,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RunInferenceInvokeModel {
    #[prost(string, tag = "1")]
    pub invocation_id: String,
    #[prost(message, optional, tag = "2")]
    pub request: Option<InferenceStreamRequest>,
    #[prost(int32, tag = "3")]
    pub purpose: i32,
    /// Set only when the server advertised delta invocations; BYOK never does.
    #[prost(bytes = "vec", optional, tag = "5")]
    pub delta_base: Option<Vec<u8>>,
    /// Set only when the server advertised request compression; BYOK never does.
    #[prost(bytes = "vec", optional, tag = "7")]
    pub compressed_request: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RunInferenceCancelInvocation {
    #[prost(string, tag = "1")]
    pub invocation_id: String,
}

#[derive(Clone, Copy, PartialEq, ::prost::Message)]
pub struct RunInferenceFinishRun {
    #[prost(bool, tag = "1")]
    pub aborted: bool,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct InferenceStreamRequest {
    #[prost(message, repeated, tag = "1")]
    pub messages: Vec<InferenceCoreMessage>,
    #[prost(message, repeated, tag = "2")]
    pub tools: Vec<InferenceAgentTool>,
    #[prost(message, optional, tag = "4")]
    pub model_config: Option<InferenceModelConfig>,
    #[prost(string, optional, tag = "5")]
    pub model_id: Option<String>,
    #[prost(message, optional, tag = "7")]
    pub requested_model: Option<InferenceRequestedModel>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct InferenceModelConfig {
    #[prost(int32, optional, tag = "1")]
    pub max_tokens: Option<i32>,
}

/// `InferenceMessageRole`: 1 USER, 2 ASSISTANT, 3 TOOL, 4 SYSTEM.
pub const ROLE_USER: i32 = 1;
pub const ROLE_ASSISTANT: i32 = 2;
pub const ROLE_TOOL: i32 = 3;
pub const ROLE_SYSTEM: i32 = 4;

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct InferenceCoreMessage {
    #[prost(int32, tag = "1")]
    pub role: i32,
    #[prost(oneof = "core_message::Content", tags = "2, 3, 6")]
    pub content: Option<core_message::Content>,
    #[prost(message, repeated, tag = "4")]
    pub tool_calls: Vec<InferenceToolCall>,
    #[prost(message, repeated, tag = "7")]
    pub reasoning_parts: Vec<InferenceReasoningPart>,
    #[prost(string, optional, tag = "18")]
    pub anthropic_native_content: Option<String>,
}

pub mod core_message {
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Content {
        #[prost(string, tag = "2")]
        Text(String),
        #[prost(message, tag = "3")]
        Parts(super::InferenceContentParts),
        #[prost(message, tag = "6")]
        ToolContent(super::InferenceToolResultContent),
    }
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct InferenceContentParts {
    #[prost(message, repeated, tag = "1")]
    pub parts: Vec<InferenceContentPart>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct InferenceContentPart {
    #[prost(oneof = "content_part::Part", tags = "1, 2, 3")]
    pub part: Option<content_part::Part>,
}

pub mod content_part {
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Part {
        #[prost(message, tag = "1")]
        Text(super::InferenceTextPart),
        #[prost(message, tag = "2")]
        Image(super::InferenceImagePart),
        #[prost(message, tag = "3")]
        File(super::InferenceFilePart),
    }
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct InferenceTextPart {
    #[prost(string, tag = "1")]
    pub text: String,
}

/// `data` is a base64 string or a `data:<mime>;base64,` URL.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct InferenceImagePart {
    #[prost(string, tag = "1")]
    pub data: String,
    #[prost(string, optional, tag = "2")]
    pub mime_type: Option<String>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct InferenceFilePart {
    #[prost(string, tag = "1")]
    pub data: String,
    #[prost(string, tag = "2")]
    pub media_type: String,
    #[prost(string, optional, tag = "3")]
    pub filename: Option<String>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct InferenceToolResultContent {
    #[prost(message, repeated, tag = "1")]
    pub parts: Vec<InferenceToolResultPart>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct InferenceToolResultPart {
    #[prost(string, tag = "1")]
    pub tool_call_id: String,
    #[prost(string, tag = "2")]
    pub tool_name: String,
    #[prost(message, optional, tag = "3")]
    pub result: Option<::prost_types::Value>,
    #[prost(bool, tag = "4")]
    pub is_error: bool,
    #[prost(message, repeated, tag = "5")]
    pub experimental_content: Vec<InferenceContentPart>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct InferenceToolCall {
    #[prost(string, tag = "1")]
    pub tool_call_id: String,
    #[prost(string, tag = "2")]
    pub tool_name: String,
    #[prost(message, optional, tag = "3")]
    pub args: Option<::prost_types::Struct>,
    #[prost(string, optional, tag = "4")]
    pub raw_tool_call_args: Option<String>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct InferenceReasoningPart {
    #[prost(bool, tag = "1")]
    pub is_redacted: bool,
    #[prost(string, tag = "2")]
    pub text: String,
    #[prost(string, optional, tag = "3")]
    pub signature: Option<String>,
    #[prost(string, optional, tag = "4")]
    pub redacted_data: Option<String>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct InferenceAgentTool {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(string, tag = "2")]
    pub description: String,
    /// Serialized AI-SDK schema object: `{"jsonSchema": {...}}`.
    #[prost(message, optional, tag = "3")]
    pub parameters: Option<::prost_types::Struct>,
    /// Freeform (grammar) tools such as ApplyPatch take raw text arguments.
    #[prost(message, optional, tag = "4")]
    pub custom_tool_format: Option<InferenceCustomToolFormat>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct InferenceCustomToolFormat {
    #[prost(string, tag = "1")]
    pub r#type: String,
    #[prost(string, tag = "2")]
    pub definition: String,
    #[prost(string, tag = "3")]
    pub syntax: String,
}


/// Server -> client envelope (RunInferenceSSE / RunInferencePoll payload).
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RunInferenceServerMessage {
    #[prost(oneof = "server_message::Message", tags = "1, 2, 3, 4")]
    pub message: Option<server_message::Message>,
}

pub mod server_message {
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Message {
        #[prost(message, tag = "1")]
        Heartbeat(super::Empty),
        #[prost(message, tag = "2")]
        RunReady(super::RunInferenceRunReady),
        #[prost(message, tag = "3")]
        InvocationResponse(super::RunInferenceInvocationResponse),
        #[prost(message, tag = "4")]
        InvocationEnd(super::RunInferenceInvocationEnd),
    }
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RunInferenceRunReady {
    #[prost(message, optional, tag = "1")]
    pub resolved_model: Option<InferenceRequestedModel>,
    #[prost(bool, tag = "2")]
    pub supports_self_summary: bool,
    #[prost(string, optional, tag = "3")]
    pub routed_model_display_name: Option<String>,
    #[prost(message, optional, tag = "4")]
    pub prompt_model_metadata: Option<RunInferencePromptModelMetadata>,
    #[prost(message, optional, tag = "5")]
    pub summarization: Option<RunInferenceSummarizationConfig>,
    #[prost(bool, tag = "7")]
    pub accepts_client_heartbeat: bool,
    #[prost(bool, tag = "13")]
    pub accepts_aborted_finish: bool,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RunInferenceSummarizationConfig {
    #[prost(message, optional, tag = "1")]
    pub model: Option<InferenceRequestedModel>,
    #[prost(bool, tag = "2")]
    pub is_fallback_to_main_model: bool,
    #[prost(int32, tag = "3")]
    pub max_prompt_chars: i32,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RunInferencePromptModelMetadata {
    /// One of anthropic/openai/google/xai/zai/cursor/unknown.
    #[prost(string, tag = "1")]
    pub vendor: String,
    /// One of dsv3-1018/dsv3-1205/cursor-0226/latest/gpt5-codex/codex-cloud/haiku.
    #[prost(string, tag = "2")]
    pub prompt_version: String,
    #[prost(bool, tag = "16")]
    pub is_claude_4x: bool,
    #[prost(bool, tag = "23")]
    pub is_gpt5_family: bool,
    #[prost(string, optional, tag = "30")]
    pub reasoning_effort: Option<String>,
    #[prost(int32, optional, tag = "32")]
    pub agent_token_limit: Option<i32>,
    #[prost(string, tag = "35")]
    pub persona: String,
    #[prost(bool, tag = "41")]
    pub enable_line_numbers: bool,
    #[prost(string, tag = "44")]
    pub self_identity_name: String,
    #[prost(message, repeated, tag = "46")]
    pub agent_mode_configs: Vec<RunInferenceAgentModeConfig>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RunInferenceAgentModeConfig {
    #[prost(string, tag = "1")]
    pub mode_id: String,
    #[prost(bool, tag = "2")]
    pub enable_ask_question_tool: bool,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RunInferenceInvocationResponse {
    #[prost(string, tag = "1")]
    pub invocation_id: String,
    #[prost(message, optional, tag = "2")]
    pub response: Option<InferenceStreamResponse>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RunInferenceInvocationEnd {
    #[prost(string, tag = "1")]
    pub invocation_id: String,
    #[prost(message, optional, tag = "2")]
    pub error: Option<RunInferenceInvocationError>,
}

/// Connect error: `code` is the numeric Connect code (e.g. 13 internal).
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RunInferenceInvocationError {
    #[prost(int32, tag = "1")]
    pub code: i32,
    #[prost(string, tag = "2")]
    pub message: String,
}


#[derive(Clone, PartialEq, ::prost::Message)]
pub struct InferenceStreamResponse {
    #[prost(oneof = "stream_response::Response", tags = "1, 2, 3, 4, 5, 7, 8, 9")]
    pub response: Option<stream_response::Response>,
}

pub mod stream_response {
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Response {
        #[prost(message, tag = "1")]
        TextPart(super::InferenceTextStreamPart),
        #[prost(message, tag = "2")]
        ToolCallPart(super::InferenceToolCallStreamPart),
        #[prost(message, tag = "3")]
        Usage(super::InferenceUsageInfo),
        #[prost(message, tag = "4")]
        ResponseInfo(super::InferenceResponseInfo),
        #[prost(message, tag = "5")]
        ExtendedUsage(super::InferenceExtendedUsageInfo),
        #[prost(message, tag = "7")]
        InvocationId(super::InferenceInvocationIdInfo),
        #[prost(message, tag = "8")]
        Error(super::InferenceStreamError),
        #[prost(message, tag = "9")]
        ThinkingPart(super::InferenceThinkingStreamPart),
    }
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct InferenceTextStreamPart {
    #[prost(string, tag = "1")]
    pub text: String,
    #[prost(bool, tag = "2")]
    pub is_final: bool,
}

/// Streaming protocol: a part with `tool_name` set starts the call, parts with
/// empty name append `args` deltas, and `is_complete` carries the full args.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct InferenceToolCallStreamPart {
    #[prost(string, tag = "1")]
    pub tool_call_id: String,
    #[prost(string, tag = "2")]
    pub tool_name: String,
    #[prost(string, tag = "3")]
    pub args: String,
    #[prost(bool, tag = "4")]
    pub is_complete: bool,
    #[prost(int32, optional, tag = "5")]
    pub tool_index: Option<i32>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct InferenceThinkingStreamPart {
    #[prost(string, tag = "1")]
    pub text: String,
    #[prost(string, optional, tag = "2")]
    pub signature: Option<String>,
    #[prost(bool, tag = "3")]
    pub is_final: bool,
}

#[derive(Clone, Copy, PartialEq, ::prost::Message)]
pub struct InferenceUsageInfo {
    #[prost(int32, tag = "1")]
    pub prompt_tokens: i32,
    #[prost(int32, tag = "2")]
    pub completion_tokens: i32,
    #[prost(int32, optional, tag = "3")]
    pub total_tokens: Option<i32>,
}

#[derive(Clone, Copy, PartialEq, ::prost::Message)]
pub struct InferenceExtendedUsageInfo {
    #[prost(int32, tag = "1")]
    pub input_tokens: i32,
    #[prost(int32, tag = "2")]
    pub output_tokens: i32,
    #[prost(int32, tag = "3")]
    pub cache_read_tokens: i32,
    #[prost(int32, tag = "4")]
    pub cache_write_tokens: i32,
    #[prost(int32, tag = "5")]
    pub max_tokens: i32,
    #[prost(int32, optional, tag = "6")]
    pub reasoning_tokens: Option<i32>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct InferenceInvocationIdInfo {
    #[prost(string, tag = "1")]
    pub invocation_id: String,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct InferenceStreamError {
    #[prost(string, tag = "1")]
    pub message: String,
    #[prost(string, tag = "2")]
    pub code: String,
    #[prost(bool, tag = "3")]
    pub is_input_token_limit_error: bool,
    #[prost(bool, tag = "4")]
    pub is_output_token_limit_error: bool,
    #[prost(int32, tag = "5")]
    pub error_type: i32,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct InferenceResponseInfo {
    #[prost(string, tag = "1")]
    pub id: String,
    #[prost(string, tag = "2")]
    pub model: String,
    #[prost(int64, tag = "3")]
    pub created_at: i64,
    #[prost(message, repeated, tag = "4")]
    pub messages: Vec<InferenceResponseMessage>,
    #[prost(string, optional, tag = "5")]
    pub error_message: Option<String>,
}

/// `assistant_part_order`: 1 REASONING, 2 TEXT, 3 TOOL_CALL.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct InferenceResponseMessage {
    #[prost(string, tag = "1")]
    pub id: String,
    #[prost(int32, tag = "2")]
    pub role: i32,
    #[prost(string, optional, tag = "3")]
    pub content: Option<String>,
    #[prost(message, repeated, tag = "4")]
    pub tool_calls: Vec<InferenceToolCall>,
    #[prost(message, repeated, tag = "6")]
    pub reasoning_parts: Vec<InferenceReasoningPart>,
    #[prost(string, optional, tag = "12")]
    pub anthropic_native_content: Option<String>,
}

/// `aiserver.v1.BidiPollRequest` (RunInferencePoll input).
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct BidiPollRequest {
    #[prost(message, optional, tag = "1")]
    pub request_id: Option<super::proto::aiserver::v1::BidiRequestId>,
    #[prost(bool, optional, tag = "2")]
    pub start_request: Option<bool>,
}

/// `aiserver.v1.BidiPollResponse`: `data` is the hex of one server message.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct BidiPollResponse {
    #[prost(int64, tag = "1")]
    pub seqno: i64,
    #[prost(string, tag = "2")]
    pub data: String,
    #[prost(bool, optional, tag = "3")]
    pub eof: Option<bool>,
}

