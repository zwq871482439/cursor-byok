//! Serves `aiserver.v1.InferenceService/RunInference` locally for BYOK models.
//!
//! Cursor's managed-local-run harness (agent-harness >= 3.29) runs the agent
//! loop on the client and only asks the server to run single model calls:
//!
//! ```text
//! BidiAppend(run_request)    -> run_ready
//! BidiAppend(invoke_model)   -> invocation_response* -> invocation_end
//! BidiAppend(cancel/finish)  -> stream end
//! RunInferenceSSE / RunInferencePoll(request_id) <- server messages
//! ```
//!
//! Sessions are keyed by Bidi request id; appends may arrive before or after
//! the downstream stream is opened, so output goes through a replayable hub.
use std::{collections::HashMap, sync::Arc, time::Duration};

use bytes::Bytes;
use futures_util::StreamExt;
use parking_lot::Mutex;
use prost::Message;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{
    cursor::protocol::{
        connect,
        inference::{self as ri, client_message, server_message, stream_response},
    },
    model::{ModelInvocation, ModelSpec},
    provider::{FinishReason, ModelEvent, Provider},
    Error, Result,
};

use super::inference_convert;

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
const IDLE_SESSION_TTL: Duration = Duration::from_secs(30 * 60);
/// Connect `internal` code used for provider failures.
const CONNECT_INTERNAL: i32 = 13;

#[derive(Clone)]
pub struct InferenceSessions {
    inner: Arc<Mutex<HashMap<String, Arc<Session>>>>,
}

pub struct Session {
    request_id: String,
    state: Mutex<SessionState>,
    output: Mutex<Output>,
    closed: CancellationToken,
}

#[derive(Default)]
struct SessionState {
    conversation_id: String,
    model: Option<ModelSpec>,
    model_hash: String,
    next_seqno: i64,
    pending: std::collections::BTreeMap<i64, ri::RunInferenceClientMessage>,
    invocations: HashMap<String, CancellationToken>,
    provider_calls: u64,
    vendor: &'static str,
    /// Number of server messages already delivered over RunInferencePoll.
    poll_cursor: usize,
}

/// Every server message ever produced, plus live subscribers. SSE replays the
/// whole history; Poll resumes from the client-tracked seqno.
#[derive(Default)]
struct Output {
    messages: Vec<Bytes>,
    finished: bool,
    subscribers: Vec<mpsc::UnboundedSender<()>>,
}

impl Default for InferenceSessions {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl InferenceSessions {
    pub fn get(&self, request_id: &str) -> Option<Arc<Session>> {
        self.inner.lock().get(request_id).cloned()
    }

    fn get_or_create(&self, request_id: &str) -> Arc<Session> {
        let mut sessions = self.inner.lock();
        if let Some(session) = sessions.get(request_id) {
            return session.clone();
        }
        let session = Arc::new(Session {
            request_id: request_id.into(),
            state: Mutex::new(SessionState::default()),
            output: Mutex::new(Output::default()),
            closed: CancellationToken::new(),
        });
        sessions.insert(request_id.into(), session.clone());
        let registry = self.inner.clone();
        let watched = session.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = watched.closed.cancelled() => {
                    // Keep finished output briefly for late Poll reconnects.
                    tokio::time::sleep(Duration::from_secs(60)).await;
                }
                _ = tokio::time::sleep(IDLE_SESSION_TTL) => watched.finish(),
            }
            registry.lock().remove(&watched.request_id);
        });
        let heartbeat = session.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = heartbeat.closed.cancelled() => return,
                    _ = tokio::time::sleep(HEARTBEAT_INTERVAL) => {
                        heartbeat.emit(server_message::Message::Heartbeat(ri::Empty {}));
                    }
                }
            }
        });
        session
    }
}

impl Session {
    fn emit(&self, message: server_message::Message) {
        let encoded = ri::RunInferenceServerMessage {
            message: Some(message),
        }
        .encode_to_vec();
        let mut output = self.output.lock();
        if output.finished {
            return;
        }
        output.messages.push(Bytes::from(encoded));
        output.subscribers.retain(|subscriber| subscriber.send(()).is_ok());
    }

    fn finish(&self) {
        {
            let mut output = self.output.lock();
            if output.finished {
                return;
            }
            output.finished = true;
            output.subscribers.retain(|subscriber| subscriber.send(()).is_ok());
        }
        for (_, token) in self.state.lock().invocations.drain() {
            token.cancel();
        }
        self.closed.cancel();
    }

    /// Returns messages from `from` onward, whether the run has finished, and a
    /// receiver notified on the next change.
    fn read_from(&self, from: usize) -> (Vec<Bytes>, bool, mpsc::UnboundedReceiver<()>) {
        let (sender, receiver) = mpsc::unbounded_channel();
        let mut output = self.output.lock();
        let messages = output.messages.get(from..).unwrap_or_default().to_vec();
        if !output.finished {
            output.subscribers.push(sender);
        }
        (messages, output.finished, receiver)
    }
}

/// Decodes a BidiAppend payload as a RunInference client message.
pub fn decode_client_message(payload: &[u8]) -> Result<ri::RunInferenceClientMessage> {
    Ok(ri::RunInferenceClientMessage::decode(payload)?)
}

/// Model id selected by a `run_request`, if this message is one.
pub fn requested_model_id(message: &ri::RunInferenceClientMessage) -> Option<&str> {
    match message.message.as_ref()? {
        client_message::Message::RunRequest(run) => run
            .requested_model
            .as_ref()
            .map(|model| model.model_id.as_str())
            .filter(|id| !id.is_empty()),
        _ => None,
    }
}

impl InferenceSessions {
    /// Applies one append in seqno order. `model_hash` must already be known
    /// to resolve to a BYOK model when the message is a `run_request`.
    pub fn append(
        &self,
        request_id: &str,
        seqno: i64,
        message: ri::RunInferenceClientMessage,
        provider: Arc<dyn Provider>,
        vendor: &'static str,
    ) -> Result<()> {
        let session = self.get_or_create(request_id);
        if !vendor.is_empty() {
            session.state.lock().vendor = vendor;
        }
        let ready = {
            let mut state = session.state.lock();
            if seqno < state.next_seqno || state.pending.contains_key(&seqno) {
                return Ok(());
            }
            state.pending.insert(seqno, message);
            let mut ready = Vec::new();
            loop {
                let next = state.next_seqno;
                let Some(message) = state.pending.remove(&next) else {
                    break;
                };
                state.next_seqno += 1;
                ready.push(message);
            }
            ready
        };
        for message in ready {
            session.handle(message, &provider)?;
        }
        Ok(())
    }
}

impl Session {
    fn handle(
        self: &Arc<Self>,
        message: ri::RunInferenceClientMessage,
        provider: &Arc<dyn Provider>,
    ) -> Result<()> {
        use client_message::Message;
        match message.message {
            Some(Message::RunRequest(run)) => self.start_run(run),
            Some(Message::InvokeModel(invoke)) => {
                self.invoke(invoke, provider.clone());
                Ok(())
            }
            Some(Message::CancelInvocation(cancel)) => {
                if let Some(token) = self.state.lock().invocations.remove(&cancel.invocation_id) {
                    token.cancel();
                }
                Ok(())
            }
            Some(Message::FinishRun(finish)) => {
                tracing::info!(request_id = self.request_id, aborted = finish.aborted, "RunInference finish_run");
                self.finish();
                Ok(())
            }
            // Heartbeats, telemetry, request logs and other unknown messages.
            Some(Message::ClientHeartbeat(_)) | None => Ok(()),
        }
    }

    fn start_run(&self, run: ri::RunInferenceRunRequest) -> Result<()> {
        let requested = run
            .requested_model
            .ok_or_else(|| Error::Protocol("RunInference run_request has no requested_model".into()))?;
        let spec = inference_convert::model_spec(&requested);
        let vendor = {
            let mut state = self.state.lock();
            state.conversation_id = run.conversation_id.clone();
            state.model_hash = requested.model_id.clone();
            state.model = Some(spec.clone());
            state.vendor
        };
        tracing::info!(
            request_id = self.request_id,
            conversation_id = run.conversation_id,
            model = requested.model_id,
            agent_mode = run.agent_mode.as_deref().unwrap_or(""),
            "RunInference run served by BYOK"
        );
        self.emit(server_message::Message::RunReady(run_ready(requested, &spec, vendor)));
        Ok(())
    }
}

/// `run_ready` must carry a recognised `prompt_model_metadata` (vendor and
/// prompt_version from the harness allow-lists) or the harness fails the
/// turn with `unrecognized-model-family`; non-Agent modes additionally need
/// an `agent_mode_configs` row.
fn run_ready(
    requested: ri::InferenceRequestedModel,
    spec: &ModelSpec,
    vendor: &'static str,
) -> ri::RunInferenceRunReady {
    let vendor = if vendor.is_empty() { "unknown" } else { vendor };
    let modes = [
        ("agent", true),
        ("chat", false),
        ("plan", true),
        ("debug", false),
        ("triage", false),
        ("multitask", false),
    ];
    ri::RunInferenceRunReady {
        resolved_model: Some(requested.clone()),
        supports_self_summary: false,
        routed_model_display_name: spec.display_name.clone(),
        prompt_model_metadata: Some(ri::RunInferencePromptModelMetadata {
            vendor: vendor.into(),
            prompt_version: "latest".into(),
            is_claude_4x: vendor == "anthropic",
            is_gpt5_family: false,
            reasoning_effort: spec.reasoning.effort.clone(),
            agent_token_limit: spec
                .context_window_tokens
                .map(|tokens| tokens.min(i32::MAX as u64) as i32),
            persona: String::new(),
            enable_line_numbers: true,
            self_identity_name: String::new(),
            agent_mode_configs: modes
                .iter()
                .map(|(mode, ask)| ri::RunInferenceAgentModeConfig {
                    mode_id: (*mode).into(),
                    enable_ask_question_tool: *ask,
                })
                .collect(),
        }),
        summarization: Some(ri::RunInferenceSummarizationConfig {
            model: Some(requested),
            is_fallback_to_main_model: true,
            max_prompt_chars: 400_000,
        }),
        accepts_client_heartbeat: true,
        accepts_aborted_finish: true,
    }
}


impl Session {
    fn invoke(self: &Arc<Self>, invoke: ri::RunInferenceInvokeModel, provider: Arc<dyn Provider>) {
        let invocation_id = invoke.invocation_id.clone();
        let cancel = CancellationToken::new();
        let (conversation_id, model, call_index) = {
            let mut state = self.state.lock();
            state.invocations.insert(invocation_id.clone(), cancel.clone());
            state.provider_calls += 1;
            (state.conversation_id.clone(), state.model.clone(), state.provider_calls)
        };
        let session = self.clone();
        tokio::spawn(async move {
            let result = session
                .run_invocation(&invoke, model, conversation_id, call_index, provider, cancel.clone())
                .await;
            session.state.lock().invocations.remove(&invocation_id);
            let error = match result {
                Ok(()) => None,
                Err(_) if cancel.is_cancelled() => None,
                Err(error) => {
                    tracing::warn!(request_id = session.request_id, invocation_id, %error, "RunInference invocation failed");
                    session.respond(&invocation_id, stream_response::Response::Error(ri::InferenceStreamError {
                        message: error.to_string(),
                        code: "provider_error".into(),
                        error_type: 1,
                        ..Default::default()
                    }));
                    Some(ri::RunInferenceInvocationError {
                        code: CONNECT_INTERNAL,
                        message: error.to_string(),
                    })
                }
            };
            session.emit(server_message::Message::InvocationEnd(ri::RunInferenceInvocationEnd {
                invocation_id,
                error,
            }));
        });
    }

    fn respond(&self, invocation_id: &str, response: stream_response::Response) {
        self.emit(server_message::Message::InvocationResponse(ri::RunInferenceInvocationResponse {
            invocation_id: invocation_id.into(),
            response: Some(ri::InferenceStreamResponse { response: Some(response) }),
        }));
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_invocation(
        &self,
        invoke: &ri::RunInferenceInvokeModel,
        model: Option<ModelSpec>,
        conversation_id: String,
        call_index: u64,
        provider: Arc<dyn Provider>,
        cancel: CancellationToken,
    ) -> Result<()> {
        if invoke.delta_base.is_some() || invoke.compressed_request.is_some() {
            return Err(Error::Protocol(
                "RunInference delta/compressed invocations are not supported by BYOK".into(),
            ));
        }
        let request = invoke
            .request
            .as_ref()
            .ok_or_else(|| Error::Protocol("RunInference invoke_model has no request".into()))?;
        // Summarization calls may name their own model; default to the run model.
        let model = match request.requested_model.as_ref().filter(|m| !m.model_id.is_empty()) {
            Some(requested) => inference_convert::model_spec(requested),
            None => model.ok_or_else(|| Error::Protocol("invoke_model before run_request".into()))?,
        };
        let model_request = inference_convert::model_request(request, model)?;
        let freeform = inference_convert::freeform_tools(request);
        let run_id = format!("ri-{}", self.request_id);
        self.respond(&invoke.invocation_id, stream_response::Response::InvocationId(ri::InferenceInvocationIdInfo {
            invocation_id: invoke.invocation_id.clone(),
        }));
        let invocation = ModelInvocation {
            call_id: format!("{}-{}", invoke.invocation_id, uuid::Uuid::new_v4().simple()),
            run_id,
            conversation_id,
            provider_call_index: call_index,
            request: model_request,
        };
        let mut stream = provider.stream(invocation, cancel.clone());
        let mut writer = ResponseWriter::new(self, &invoke.invocation_id, freeform);
        loop {
            let event = tokio::select! {
                _ = cancel.cancelled() => return Err(Error::Cancelled),
                event = stream.next() => event,
            };
            match event {
                Some(Ok(event)) => {
                    if writer.event(event) {
                        break;
                    }
                }
                Some(Err(error)) => return Err(error),
                None => break,
            }
        }
        writer.complete();
        Ok(())
    }
}


#[derive(Default)]
struct PendingCall {
    call_id: String,
    name: String,
    args: String,
}

/// Translates normalized provider events into harness stream parts and builds
/// the final `response_info` the harness stores as the assistant message.
struct ResponseWriter<'a> {
    session: &'a Session,
    invocation_id: &'a str,
    text: String,
    thinking: String,
    calls: std::collections::BTreeMap<usize, PendingCall>,
    finished_calls: Vec<PendingCall>,
    replay: Option<crate::model::ProviderReplayState>,
    usage: crate::model::Usage,
    finish: Option<FinishReason>,
    /// Freeform tool name -> JSON wrapper property (see `freeform_tools`).
    freeform: HashMap<String, String>,
}

impl<'a> ResponseWriter<'a> {
    fn new(session: &'a Session, invocation_id: &'a str, freeform: HashMap<String, String>) -> Self {
        Self {
            session,
            invocation_id,
            freeform,
            text: String::new(),
            thinking: String::new(),
            calls: Default::default(),
            finished_calls: Vec::new(),
            replay: None,
            usage: Default::default(),
            finish: None,
        }
    }

    fn send(&self, response: stream_response::Response) {
        self.session.respond(self.invocation_id, response);
    }

    /// Returns true once the provider reported completion.
    fn event(&mut self, event: ModelEvent) -> bool {
        use stream_response::Response;
        match event {
            ModelEvent::TextDelta(text) if !text.is_empty() => {
                self.text.push_str(&text);
                self.send(Response::TextPart(ri::InferenceTextStreamPart { text, is_final: false }));
            }
            ModelEvent::ThinkingDelta(text) if !text.is_empty() => {
                self.thinking.push_str(&text);
                self.send(Response::ThinkingPart(ri::InferenceThinkingStreamPart {
                    text,
                    signature: None,
                    is_final: false,
                }));
            }
            ModelEvent::ToolCallStart { index, call_id, name } => {
                self.send(Response::ToolCallPart(ri::InferenceToolCallStreamPart {
                    tool_call_id: call_id.clone(),
                    tool_name: name.clone(),
                    args: String::new(),
                    is_complete: false,
                    tool_index: Some(index as i32),
                }));
                self.calls.insert(index, PendingCall { call_id, name, args: String::new() });
            }
            ModelEvent::ToolCallArgumentsDelta { index, delta } => {
                if let Some(call) = self.calls.get_mut(&index) {
                    call.args.push_str(&delta);
                    // Freeform args are JSON-wrapped by the provider; the raw
                    // text is only sent once complete.
                    if self.freeform.contains_key(&call.name) {
                        return false;
                    }
                    let call_id = call.call_id.clone();
                    self.send(Response::ToolCallPart(ri::InferenceToolCallStreamPart {
                        tool_call_id: call_id,
                        tool_name: String::new(),
                        args: delta,
                        is_complete: false,
                        tool_index: Some(index as i32),
                    }));
                }
            }
            ModelEvent::ToolCallEnd { index } => self.end_call(index),
            ModelEvent::ProviderReplayState(state) => self.replay = Some(state),
            ModelEvent::Usage(usage) => self.usage += usage,
            ModelEvent::Done(finish) => {
                self.finish = Some(finish);
                return true;
            }
            _ => {}
        }
        false
    }

    fn end_call(&mut self, index: usize) {
        let Some(mut call) = self.calls.remove(&index) else {
            return;
        };
        if let Some(property) = self.freeform.get(&call.name) {
            call.args = inference_convert::freeform_args(&call.args, property);
        } else if call.args.trim().is_empty() {
            call.args = "{}".into();
        }
        self.send(stream_response::Response::ToolCallPart(ri::InferenceToolCallStreamPart {
            tool_call_id: call.call_id.clone(),
            tool_name: call.name.clone(),
            args: call.args.clone(),
            is_complete: true,
            tool_index: Some(index as i32),
        }));
        self.finished_calls.push(call);
    }
}


impl ResponseWriter<'_> {
    fn complete(mut self) {
        use stream_response::Response;
        let open: Vec<usize> = self.calls.keys().copied().collect();
        for index in open {
            self.end_call(index);
        }
        if self.finish.is_none() {
            tracing::warn!(invocation_id = self.invocation_id, "provider stream ended without Done");
        }
        let input = self.usage.input_tokens.unwrap_or_default();
        let output = self.usage.output_tokens.unwrap_or_default();
        let clamp = |value: u64| value.min(i32::MAX as u64) as i32;
        self.send(Response::Usage(ri::InferenceUsageInfo {
            prompt_tokens: clamp(input),
            completion_tokens: clamp(output),
            total_tokens: Some(clamp(self.usage.total_tokens.unwrap_or(input + output))),
        }));
        self.send(Response::ExtendedUsage(ri::InferenceExtendedUsageInfo {
            input_tokens: clamp(input),
            output_tokens: clamp(output),
            cache_read_tokens: clamp(self.usage.cache_read_tokens.unwrap_or_default()),
            cache_write_tokens: clamp(self.usage.cache_write_tokens.unwrap_or_default()),
            max_tokens: 0,
            reasoning_tokens: self.usage.reasoning_tokens.map(clamp),
        }));
        let message = self.response_message();
        self.send(Response::ResponseInfo(ri::InferenceResponseInfo {
            id: self.invocation_id.into(),
            model: String::new(),
            created_at: chrono::Utc::now().timestamp_millis(),
            messages: vec![message],
            error_message: None,
        }));
        self.send(Response::TextPart(ri::InferenceTextStreamPart {
            text: String::new(),
            is_final: true,
        }));
    }

    fn response_message(&self) -> ri::InferenceResponseMessage {
        let reasoning_parts = self.reasoning_parts();
        ri::InferenceResponseMessage {
            id: self.invocation_id.into(),
            role: ri::ROLE_ASSISTANT,
            content: (!self.text.is_empty()).then(|| self.text.clone()),
            tool_calls: self
                .finished_calls
                .iter()
                .map(|call| ri::InferenceToolCall {
                    tool_call_id: call.call_id.clone(),
                    tool_name: call.name.clone(),
                    args: if self.freeform.contains_key(&call.name) {
                        None
                    } else {
                        serde_json::from_str::<serde_json::Value>(&call.args)
                            .ok()
                            .as_ref()
                            .and_then(inference_convert::json_to_struct)
                    },
                    raw_tool_call_args: Some(call.args.clone()),
                })
                .collect(),
            reasoning_parts,
            anthropic_native_content: None,
        }
    }

    /// Anthropic thinking blocks round-trip one-to-one (signature or redacted
    /// data) so the next request can replay them; other providers return the
    /// accumulated thinking text.
    fn reasoning_parts(&self) -> Vec<ri::InferenceReasoningPart> {
        let blocks = self
            .replay
            .as_ref()
            .filter(|state| state.provider_kind == "anthropic")
            .and_then(|state| state.value.get("blocks"))
            .and_then(serde_json::Value::as_array);
        if let Some(blocks) = blocks {
            let text = |block: &serde_json::Value, key: &str| {
                block.get(key).and_then(serde_json::Value::as_str).map(str::to_owned)
            };
            return blocks
                .iter()
                .map(|block| match block.get("type").and_then(serde_json::Value::as_str) {
                    Some("redacted_thinking") => ri::InferenceReasoningPart {
                        is_redacted: true,
                        text: String::new(),
                        signature: None,
                        redacted_data: text(block, "data"),
                    },
                    _ => ri::InferenceReasoningPart {
                        is_redacted: false,
                        text: text(block, "thinking").unwrap_or_default(),
                        signature: text(block, "signature"),
                        redacted_data: None,
                    },
                })
                .collect();
        }
        if self.thinking.is_empty() {
            return Vec::new();
        }
        vec![ri::InferenceReasoningPart {
            is_redacted: false,
            text: self.thinking.clone(),
            signature: None,
            redacted_data: None,
        }]
    }
}


/// Which downstream framing the client opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Downstream {
    /// `RunInferenceSSE`: Connect frames of `RunInferenceServerMessage`.
    Sse,
    /// `RunInferencePoll`: Connect frames of `BidiPollResponse` carrying the
    /// hex-encoded server message and a client-checked contiguous seqno.
    Poll,
}

impl Session {
    /// Streams server messages until the run finishes. SSE always replays from
    /// the start (one stream per run); Poll resumes at the delivered cursor so
    /// a re-poll never repeats or skips a seqno.
    pub fn downstream(self: Arc<Self>, mode: Downstream) -> impl futures_util::Stream<Item = std::result::Result<Bytes, std::convert::Infallible>> {
        async_stream::stream! {
            let mut next = match mode {
                Downstream::Sse => 0,
                Downstream::Poll => self.state.lock().poll_cursor,
            };
            loop {
                let (messages, finished, mut changed) = self.read_from(next);
                for message in messages {
                    let frame = match mode {
                        Downstream::Sse => encode_frame(&message),
                        Downstream::Poll => {
                            let response = ri::BidiPollResponse {
                                seqno: next as i64,
                                data: hex::encode(&message),
                                eof: None,
                            };
                            encode_frame(&response.encode_to_vec())
                        }
                    };
                    next += 1;
                    if mode == Downstream::Poll {
                        self.state.lock().poll_cursor = next;
                    }
                    yield Ok(frame);
                }
                if finished {
                    if mode == Downstream::Poll {
                        let eof = ri::BidiPollResponse {
                            seqno: next as i64,
                            data: String::new(),
                            eof: Some(true),
                        };
                        yield Ok(encode_frame(&eof.encode_to_vec()));
                    }
                    yield Ok(connect::encode_end_stream());
                    return;
                }
                if changed.recv().await.is_none() {
                    // Output hub dropped subscribers without finishing; re-read.
                    continue;
                }
            }
        }
    }
}

fn encode_frame(payload: &[u8]) -> Bytes {
    let mut output = Vec::with_capacity(5 + payload.len());
    output.push(0);
    output.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    output.extend_from_slice(payload);
    Bytes::from(output)
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ProviderStream;

    struct FakeProvider;

    impl Provider for FakeProvider {
        fn stream(&self, invocation: ModelInvocation, _: CancellationToken) -> ProviderStream {
            assert_eq!(invocation.request.prompt.instructions, "sys");
            assert_eq!(invocation.request.prompt.tools[0].name, "Read");
            Box::pin(futures_util::stream::iter(vec![
                Ok(ModelEvent::TextDelta("hello".into())),
                Ok(ModelEvent::ToolCallStart { index: 0, call_id: "c1".into(), name: "Read".into() }),
                Ok(ModelEvent::ToolCallArgumentsDelta { index: 0, delta: "{\"path\":".into() }),
                Ok(ModelEvent::ToolCallArgumentsDelta { index: 0, delta: "\"a\"}".into() }),
                Ok(ModelEvent::ToolCallEnd { index: 0 }),
                Ok(ModelEvent::Done(FinishReason::ToolUse)),
            ]))
        }
    }

    fn client(message: client_message::Message) -> ri::RunInferenceClientMessage {
        ri::RunInferenceClientMessage { message: Some(message) }
    }

    fn text_message(role: i32, text: &str) -> ri::InferenceCoreMessage {
        ri::InferenceCoreMessage {
            role,
            content: Some(ri::core_message::Content::Text(text.into())),
            ..Default::default()
        }
    }

    fn decode_poll(frames: &[u8]) -> Vec<server_message::Message> {
        connect::decode_frames(frames)
            .unwrap()
            .into_iter()
            .filter(|(flags, _)| flags & connect::END_STREAM_FLAG == 0)
            .filter_map(|(_, payload)| {
                let poll = ri::BidiPollResponse::decode(payload).unwrap();
                (poll.eof != Some(true)).then(|| {
                    let bytes = hex::decode(poll.data).unwrap();
                    ri::RunInferenceServerMessage::decode(bytes.as_slice()).unwrap().message.unwrap()
                })
            })
            .filter(|message| !matches!(message, server_message::Message::Heartbeat(_)))
            .collect()
    }

    fn has_invocation_end(session: &Session) -> bool {
        session.output.lock().messages.iter().any(|bytes| {
            matches!(
                ri::RunInferenceServerMessage::decode(bytes.as_ref()).unwrap().message,
                Some(server_message::Message::InvocationEnd(_))
            )
        })
    }

    async fn run_session() -> Vec<server_message::Message> {
        let sessions = InferenceSessions::default();
        let provider: Arc<dyn Provider> = Arc::new(FakeProvider);
        let run = client_message::Message::RunRequest(ri::RunInferenceRunRequest {
            conversation_id: "conv".into(),
            requested_model: Some(ri::InferenceRequestedModel { model_id: "hash".into(), ..Default::default() }),
            agent_mode: Some("agent".into()),
            subagent_type_name: None,
        });
        let invoke = client_message::Message::InvokeModel(ri::RunInferenceInvokeModel {
            invocation_id: "inv".into(),
            request: Some(ri::InferenceStreamRequest {
                messages: vec![text_message(ri::ROLE_SYSTEM, "sys"), text_message(ri::ROLE_USER, "hi")],
                tools: vec![ri::InferenceAgentTool { name: "Read".into(), ..Default::default() }],
                ..Default::default()
            }),
            ..Default::default()
        });
        // Out-of-order append must wait for seqno 0.
        sessions.append("req", 1, client(invoke), provider.clone(), "anthropic").unwrap();
        let session = sessions.get("req").unwrap();
        assert!(session.output.lock().messages.is_empty());
        sessions.append("req", 0, client(run), provider.clone(), "anthropic").unwrap();
        for _ in 0..200 {
            if has_invocation_end(&session) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let finish = client_message::Message::FinishRun(ri::RunInferenceFinishRun { aborted: false });
        sessions.append("req", 2, client(finish), provider, "").unwrap();
        let frames: Vec<Vec<u8>> = session
            .downstream(Downstream::Poll)
            .map(|frame| frame.unwrap().to_vec())
            .collect()
            .await;
        decode_poll(&frames.concat())
    }

    #[tokio::test]
    async fn run_request_and_invoke_stream_a_complete_response() {
        let messages = run_session().await;
        let server_message::Message::RunReady(ready) = &messages[0] else {
            panic!("first message must be run_ready: {messages:?}");
        };
        let metadata = ready.prompt_model_metadata.as_ref().unwrap();
        assert_eq!(
            (metadata.vendor.as_str(), metadata.prompt_version.as_str()),
            ("anthropic", "latest")
        );
        assert!(metadata.agent_mode_configs.iter().any(|mode| mode.mode_id == "agent"));

        let responses: Vec<_> = messages
            .iter()
            .filter_map(|message| match message {
                server_message::Message::InvocationResponse(r) => r.response.clone()?.response,
                _ => None,
            })
            .collect();
        assert!(responses
            .iter()
            .any(|r| matches!(r, stream_response::Response::TextPart(p) if p.text == "hello")));
        let complete = responses.iter().find_map(|r| match r {
            stream_response::Response::ToolCallPart(p) if p.is_complete => Some(p),
            _ => None,
        });
        assert_eq!(complete.unwrap().args, "{\"path\":\"a\"}");
        let info = responses.iter().find_map(|r| match r {
            stream_response::Response::ResponseInfo(info) => Some(info),
            _ => None,
        });
        let message = &info.unwrap().messages[0];
        assert_eq!(message.content.as_deref(), Some("hello"));
        assert_eq!(message.tool_calls[0].tool_name, "Read");
        assert!(matches!(
            responses.last(),
            Some(stream_response::Response::TextPart(p)) if p.is_final
        ));
        assert!(matches!(
            messages.last(),
            Some(server_message::Message::InvocationEnd(end)) if end.error.is_none()
        ));
    }
}

