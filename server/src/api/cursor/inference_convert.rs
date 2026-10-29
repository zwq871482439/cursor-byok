//! Converts RunInference harness requests into provider-independent requests.
//!
//! In managed-local-run mode Cursor's harness owns the agent loop, prompt and
//! tool execution; BYOK only turns one `InferenceStreamRequest` into one
//! provider call. System messages become `PromptSpec.instructions`.
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::Value;

use crate::{
    cursor::protocol::inference::{self as ri, content_part::Part, core_message::Content},
    model::{
        ContentPart, ModelRequest, ModelSpec, ProjectedContent, ProjectedMessage, PromptSpec,
        ProviderReplayState, Role, ToolCallContent, ToolDefinition, ToolResultContent,
    },
    Error, Result,
};

/// Freeform (grammar) tools take raw text, which JSON-tool providers cannot
/// emit. They are advertised with their JSON schema (one string property) and
/// the string is unwrapped back to raw text on the way out. Returns
/// tool name -> wrapper property name.
pub fn freeform_tools(request: &ri::InferenceStreamRequest) -> std::collections::HashMap<String, String> {
    request
        .tools
        .iter()
        .filter(|tool| tool.custom_tool_format.is_some())
        .map(|tool| {
            let schema = object_schema(tool.parameters.as_ref().map(struct_to_json).unwrap_or(Value::Null));
            let property = schema
                .get("properties")
                .and_then(Value::as_object)
                .and_then(|properties| {
                    (properties.len() == 1).then(|| properties.keys().next().cloned()).flatten()
                })
                .unwrap_or_else(|| "input".into());
            (tool.name.clone(), property)
        })
        .collect()
}

/// Raw-text argument for a freeform tool from the provider's JSON arguments.
pub fn freeform_args(json_args: &str, property: &str) -> String {
    match serde_json::from_str::<Value>(json_args) {
        Ok(Value::Object(map)) => match map.get(property) {
            Some(Value::String(text)) => text.clone(),
            Some(other) => other.to_string(),
            None => json_args.to_owned(),
        },
        _ => json_args.to_owned(),
    }
}

pub fn model_request(request: &ri::InferenceStreamRequest, model: ModelSpec) -> Result<ModelRequest> {
    let freeform = freeform_tools(request);
    let mut instructions = Vec::new();
    let mut history = Vec::with_capacity(request.messages.len());
    for (index, message) in request.messages.iter().enumerate() {
        let id = format!("ri-{index}");
        match message.role {
            ri::ROLE_SYSTEM => {
                let text = plain_text(message);
                if !text.is_empty() {
                    instructions.push(text);
                }
            }
            ri::ROLE_USER => history.push(ProjectedMessage {
                message_id: id,
                role: Role::User,
                content: ProjectedContent::Parts(user_parts(message)?),
            }),
            ri::ROLE_ASSISTANT => history.push(assistant(id, message, &freeform)),
            ri::ROLE_TOOL => history.extend(tool_results(&id, message)),
            other => {
                return Err(Error::Protocol(format!(
                    "RunInference message {index} has unknown role {other}"
                )))
            }
        }
    }
    let mut model = model;
    if let Some(max) = request
        .model_config
        .as_ref()
        .and_then(|config| config.max_tokens)
        .filter(|max| *max > 0)
    {
        model.max_output_tokens = Some(max as u64);
    }
    Ok(ModelRequest {
        prompt: PromptSpec {
            instructions: instructions.join("\n\n"),
            tools: request.tools.iter().map(tool_definition).collect(),
        },
        model,
        history,
    })
}

fn plain_text(message: &ri::InferenceCoreMessage) -> String {
    match &message.content {
        Some(Content::Text(text)) => text.clone(),
        Some(Content::Parts(parts)) => parts
            .parts
            .iter()
            .filter_map(|part| match &part.part {
                Some(Part::Text(text)) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

fn user_parts(message: &ri::InferenceCoreMessage) -> Result<Vec<ContentPart>> {
    match &message.content {
        Some(Content::Text(text)) => Ok(vec![ContentPart::Text { text: text.clone() }]),
        Some(Content::Parts(parts)) => content_parts(&parts.parts),
        Some(Content::ToolContent(_)) | None => Ok(Vec::new()),
    }
}

fn content_parts(parts: &[ri::InferenceContentPart]) -> Result<Vec<ContentPart>> {
    let mut output = Vec::with_capacity(parts.len());
    for part in parts {
        match &part.part {
            Some(Part::Text(text)) => output.push(ContentPart::Text {
                text: text.text.clone(),
            }),
            Some(Part::Image(image)) => output.push(image_part(&image.data, image.mime_type.as_deref())?),
            Some(Part::File(file)) if file.media_type.starts_with("image/") => {
                output.push(image_part(&file.data, Some(&file.media_type))?)
            }
            Some(Part::File(file)) => output.push(ContentPart::Text {
                text: file_text(file),
            }),
            None => {}
        }
    }
    Ok(output)
}

fn file_text(file: &ri::InferenceFilePart) -> String {
    let name = file.filename.as_deref().unwrap_or("attachment");
    match decode_data(&file.data) {
        Ok((_, bytes)) => match String::from_utf8(bytes) {
            Ok(text) => format!("<file name=\"{name}\">\n{text}\n</file>"),
            Err(_) => format!("[binary attachment {name} ({}) omitted]", file.media_type),
        },
        Err(_) => format!("[attachment {name} ({}) omitted]", file.media_type),
    }
}

fn image_part(data: &str, mime: Option<&str>) -> Result<ContentPart> {
    let (url_mime, bytes) = decode_data(data)?;
    let mime_type = url_mime
        .or_else(|| mime.map(str::to_owned))
        .filter(|mime| !mime.is_empty())
        .unwrap_or_else(|| "image/png".into());
    Ok(ContentPart::Image {
        mime_type,
        data: bytes,
    })
}

/// Accepts bare base64 or a `data:<mime>;base64,<payload>` URL.
fn decode_data(data: &str) -> Result<(Option<String>, Vec<u8>)> {
    let (mime, payload) = match data.strip_prefix("data:").and_then(|rest| rest.split_once(',')) {
        Some((header, payload)) => (
            header.split(';').next().map(str::to_owned).filter(|m| !m.is_empty()),
            payload,
        ),
        None => (None, data),
    };
    let bytes = STANDARD
        .decode(payload.trim())
        .map_err(|error| Error::Protocol(format!("invalid RunInference base64 data: {error}")))?;
    Ok((mime, bytes))
}

fn assistant(
    id: String,
    message: &ri::InferenceCoreMessage,
    freeform: &std::collections::HashMap<String, String>,
) -> ProjectedMessage {
    let thinking = message
        .reasoning_parts
        .iter()
        .filter(|part| !part.is_redacted)
        .map(|part| part.text.as_str())
        .collect::<Vec<_>>()
        .join("");
    let calls = message
        .tool_calls
        .iter()
        .enumerate()
        .map(|(index, call)| ToolCallContent {
            index,
            call_id: call.tool_call_id.clone(),
            name: call.tool_name.clone(),
            arguments: match freeform.get(&call.tool_name) {
                // History holds the raw text; re-wrap it for the JSON schema.
                Some(property) => serde_json::json!({ property.as_str(): freeform_text(call) }),
                None => tool_call_args(call),
            },
        })
        .collect();
    ProjectedMessage {
        message_id: id,
        role: Role::Assistant,
        content: ProjectedContent::Assistant {
            text: plain_text(message),
            thinking,
            replay_state: anthropic_replay(message),
            calls,
        },
    }
}

/// Re-sends signed Anthropic thinking blocks so extended thinking survives
/// tool rounds (Anthropic rejects tool_use turns that drop their thinking).
fn anthropic_replay(message: &ri::InferenceCoreMessage) -> Option<ProviderReplayState> {
    let blocks: Vec<Value> = message
        .reasoning_parts
        .iter()
        .filter_map(|part| {
            if part.is_redacted {
                part.redacted_data
                    .as_ref()
                    .map(|data| serde_json::json!({"type": "redacted_thinking", "data": data}))
            } else {
                part.signature.as_ref().filter(|s| !s.is_empty()).map(|signature| {
                    serde_json::json!({"type": "thinking", "thinking": part.text, "signature": signature})
                })
            }
        })
        .collect();
    (!blocks.is_empty()).then(|| ProviderReplayState {
        provider_kind: "anthropic".into(),
        value: serde_json::json!({ "blocks": blocks }),
    })
}

fn freeform_text(call: &ri::InferenceToolCall) -> String {
    if let Some(raw) = &call.raw_tool_call_args {
        return raw.clone();
    }
    match call.args.as_ref().map(struct_to_json) {
        Some(Value::Object(map)) if map.len() == 1 => match map.values().next() {
            Some(Value::String(text)) => text.clone(),
            Some(other) => other.to_string(),
            None => String::new(),
        },
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

fn tool_call_args(call: &ri::InferenceToolCall) -> Value {
    if let Some(args) = &call.args {
        return struct_to_json(args);
    }
    call.raw_tool_call_args
        .as_deref()
        .and_then(|raw| serde_json::from_str(raw).ok())
        .unwrap_or_else(|| serde_json::json!({}))
}

fn tool_results(id: &str, message: &ri::InferenceCoreMessage) -> Vec<ProjectedMessage> {
    let Some(Content::ToolContent(content)) = &message.content else {
        return Vec::new();
    };
    content
        .parts
        .iter()
        .enumerate()
        .map(|(index, part)| {
            let provider_parts = content_parts(&part.experimental_content).unwrap_or_default();
            ProjectedMessage {
                message_id: format!("{id}-{index}"),
                role: Role::Tool,
                content: ProjectedContent::ToolResult(ToolResultContent {
                    call_id: part.tool_call_id.clone(),
                    name: part.tool_name.clone(),
                    content: tool_result_text(part.result.as_ref()),
                    is_error: part.is_error,
                    image: None,
                    provider_parts,
                }),
            }
        })
        .collect()
}

fn tool_result_text(result: Option<&prost_types::Value>) -> String {
    match result.map(value_to_json) {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) => text,
        Some(other) => other.to_string(),
    }
}

fn tool_definition(tool: &ri::InferenceAgentTool) -> ToolDefinition {
    let parameters = object_schema(tool.parameters.as_ref().map(struct_to_json).unwrap_or(Value::Null));
    ToolDefinition {
        name: tool.name.clone(),
        description: tool.description.clone(),
        parameters,
    }
}

/// Providers require a top-level `{"type": "object"}` input schema (Anthropic:
/// `tools.N.custom.input_schema.type: Field required`).
///
/// The harness wraps built-in tool schemas as `{jsonSchema, validate}` (its
/// `Ei()` helper); `JSON.stringify` drops the symbol keys and `validate`, so
/// the wire carries `{"jsonSchema": {...}}`. Unwrap that, and default a missing
/// root `type` (argument-less tools send `{}`, unions send only `anyOf`).
fn object_schema(schema: Value) -> Value {
    let Value::Object(mut map) = schema else {
        return serde_json::json!({"type": "object", "properties": {}});
    };
    if !map.contains_key("type") && !map.contains_key("properties") {
        if let Some(Value::Object(inner)) = map.remove("jsonSchema") {
            return object_schema(Value::Object(inner));
        }
    }
    if !map.contains_key("type") {
        map.insert("type".into(), Value::String("object".into()));
    }
    if map.get("type").and_then(Value::as_str) == Some("object") {
        map.entry("properties").or_insert_with(|| serde_json::json!({}));
    }
    Value::Object(map)
}

pub fn struct_to_json(value: &prost_types::Struct) -> Value {
    Value::Object(
        value
            .fields
            .iter()
            .map(|(key, value)| (key.clone(), value_to_json(value)))
            .collect(),
    )
}

pub fn value_to_json(value: &prost_types::Value) -> Value {
    use prost_types::value::Kind;
    match &value.kind {
        None | Some(Kind::NullValue(_)) => Value::Null,
        Some(Kind::NumberValue(number)) => {
            if number.fract() == 0.0 && number.abs() < 9.0e15 {
                Value::from(*number as i64)
            } else {
                serde_json::Number::from_f64(*number).map_or(Value::Null, Value::Number)
            }
        }
        Some(Kind::StringValue(text)) => Value::String(text.clone()),
        Some(Kind::BoolValue(flag)) => Value::Bool(*flag),
        Some(Kind::StructValue(inner)) => struct_to_json(inner),
        Some(Kind::ListValue(list)) => Value::Array(list.values.iter().map(value_to_json).collect()),
    }
}

pub fn json_to_struct(value: &Value) -> Option<prost_types::Struct> {
    let Value::Object(map) = value else {
        return None;
    };
    Some(prost_types::Struct {
        fields: map
            .iter()
            .map(|(key, value)| (key.clone(), json_to_value(value)))
            .collect(),
    })
}

fn json_to_value(value: &Value) -> prost_types::Value {
    use prost_types::value::Kind;
    let kind = match value {
        Value::Null => Kind::NullValue(0),
        Value::Bool(flag) => Kind::BoolValue(*flag),
        Value::Number(number) => Kind::NumberValue(number.as_f64().unwrap_or_default()),
        Value::String(text) => Kind::StringValue(text.clone()),
        Value::Array(items) => Kind::ListValue(prost_types::ListValue {
            values: items.iter().map(json_to_value).collect(),
        }),
        Value::Object(_) => Kind::StructValue(json_to_struct(value).unwrap_or_default()),
    };
    prost_types::Value { kind: Some(kind) }
}


/// Mirrors `cursor::compile::model::from_requested` for the inference schema.
pub fn model_spec(requested: &ri::InferenceRequestedModel) -> ModelSpec {
    let mut spec = ModelSpec::new(requested.model_id.clone());
    spec.reasoning.enabled = requested.max_mode;
    for parameter in &requested.parameters {
        let value = parameter.value.trim();
        match parameter.id.as_str() {
            "effort" | "reasoning" => {
                spec.reasoning.effort = (value != "none" && !value.is_empty()).then(|| value.to_string());
                spec.reasoning.enabled |= spec.reasoning.effort.is_some();
            }
            "thinking" => spec.reasoning.enabled |= value == "true",
            "fast" if value == "true" => spec.latency = crate::model::ModelLatency::Fast,
            "context" => spec.context_window_tokens = crate::model::parse_token_count(value),
            _ => {}
        }
    }
    spec
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cursor::protocol::inference::{
        content_part, core_message, InferenceContentPart, InferenceContentParts,
        InferenceCoreMessage, InferenceTextPart, InferenceToolCall, InferenceToolResultContent,
        InferenceToolResultPart,
    };

    fn text(role: i32, text: &str) -> InferenceCoreMessage {
        InferenceCoreMessage {
            role,
            content: Some(core_message::Content::Text(text.into())),
            ..Default::default()
        }
    }

    #[test]
    fn system_becomes_instructions_and_tool_round_is_preserved() {
        let args = json_to_struct(&serde_json::json!({"path": "a.rs", "n": 3})).unwrap();
        let request = ri::InferenceStreamRequest {
            messages: vec![
                text(ri::ROLE_SYSTEM, "be terse"),
                InferenceCoreMessage {
                    role: ri::ROLE_USER,
                    content: Some(core_message::Content::Parts(InferenceContentParts {
                        parts: vec![InferenceContentPart {
                            part: Some(content_part::Part::Text(InferenceTextPart { text: "hi".into() })),
                        }],
                    })),
                    ..Default::default()
                },
                InferenceCoreMessage {
                    role: ri::ROLE_ASSISTANT,
                    tool_calls: vec![InferenceToolCall {
                        tool_call_id: "c1".into(),
                        tool_name: "Read".into(),
                        args: Some(args),
                        raw_tool_call_args: None,
                    }],
                    ..Default::default()
                },
                InferenceCoreMessage {
                    role: ri::ROLE_TOOL,
                    content: Some(core_message::Content::ToolContent(InferenceToolResultContent {
                        parts: vec![InferenceToolResultPart {
                            tool_call_id: "c1".into(),
                            tool_name: "Read".into(),
                            result: Some(prost_types::Value {
                                kind: Some(prost_types::value::Kind::StringValue("ok".into())),
                            }),
                            ..Default::default()
                        }],
                    })),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let converted = model_request(&request, ModelSpec::new("m")).unwrap();
        assert_eq!(converted.prompt.instructions, "be terse");
        assert_eq!(converted.history.len(), 3);
        let ProjectedContent::Assistant { calls, .. } = &converted.history[1].content else {
            panic!("expected assistant");
        };
        assert_eq!(calls[0].arguments, serde_json::json!({"path": "a.rs", "n": 3}));
        let ProjectedContent::ToolResult(result) = &converted.history[2].content else {
            panic!("expected tool result");
        };
        assert_eq!(result.content, "ok");
    }

    #[test]
    fn harness_tool_schemas_become_object_schemas() {
        // AI-SDK wrapper as serialized by the harness (symbol keys dropped).
        let wrapped = serde_json::json!({"jsonSchema": {"type": "object", "properties": {"path": {"type": "string"}}}});
        assert_eq!(
            object_schema(wrapped),
            serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}})
        );
        assert_eq!(object_schema(serde_json::json!({})), serde_json::json!({"type": "object", "properties": {}}));
        assert_eq!(
            object_schema(serde_json::json!({"jsonSchema": {}})),
            serde_json::json!({"type": "object", "properties": {}})
        );
        assert_eq!(object_schema(Value::Null)["type"], "object");
        let union = object_schema(serde_json::json!({"anyOf": [{"type": "object"}]}));
        assert_eq!(union["type"], "object");
    }

    #[test]
    fn freeform_tools_round_trip_raw_text() {
        let schema = json_to_struct(&serde_json::json!({
            "jsonSchema": {"type": "object", "properties": {"patch": {"type": "string"}}}
        }));
        let request = ri::InferenceStreamRequest {
            tools: vec![ri::InferenceAgentTool {
                name: "ApplyPatch".into(),
                parameters: schema,
                custom_tool_format: Some(ri::InferenceCustomToolFormat {
                    r#type: "grammar".into(),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            messages: vec![InferenceCoreMessage {
                role: ri::ROLE_ASSISTANT,
                tool_calls: vec![InferenceToolCall {
                    tool_call_id: "c".into(),
                    tool_name: "ApplyPatch".into(),
                    args: None,
                    raw_tool_call_args: Some("*** Begin Patch".into()),
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        let freeform = freeform_tools(&request);
        assert_eq!(freeform.get("ApplyPatch").map(String::as_str), Some("patch"));
        assert_eq!(freeform_args("{\"patch\":\"*** Begin Patch\\n\"}", "patch"), "*** Begin Patch\n");
        let converted = model_request(&request, ModelSpec::new("m")).unwrap();
        assert_eq!(converted.prompt.tools[0].parameters["properties"]["patch"]["type"], "string");
        let ProjectedContent::Assistant { calls, .. } = &converted.history[0].content else {
            panic!("expected assistant");
        };
        assert_eq!(calls[0].arguments, serde_json::json!({"patch": "*** Begin Patch"}));
    }

    #[test]
    fn data_urls_decode_with_their_mime_type() {
        let ContentPart::Image { mime_type, data } =
            image_part("data:image/jpeg;base64,AAEC", None).unwrap()
        else {
            panic!("expected image");
        };
        assert_eq!(mime_type, "image/jpeg");
        assert_eq!(data, vec![0, 1, 2]);
    }

    #[test]
    fn model_parameters_map_to_spec() {
        let spec = model_spec(&ri::InferenceRequestedModel {
            model_id: "h".into(),
            parameters: vec![
                ri::InferenceModelParameterValue { id: "reasoning".into(), value: "high".into() },
                ri::InferenceModelParameterValue { id: "context".into(), value: "500k".into() },
            ],
            ..Default::default()
        });
        assert_eq!(spec.reasoning.effort.as_deref(), Some("high"));
        assert_eq!(spec.context_window_tokens, Some(500_000));
    }
}

