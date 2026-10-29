//! Implements Cursor HTTP endpoints outside the Agent Run stream.
use axum::{
    body::{to_bytes, Body, Bytes},
    extract::{DefaultBodyLimit, Extension, State},
    http::{header, HeaderMap, HeaderValue, Request, Response, StatusCode},
    routing::{get, post},
    Router,
};
use tower_http::decompression::RequestDecompressionLayer;

use crate::{
    api::cursor::{
        bidi, inference,
        proxy::{self, CursorProxy},
        run_sse,
    },
    cursor::{
        protocol::{
            connect,
            proto::{agent::v1 as agent, aiserver::v1 as ai},
        },
        services::{
            account, analytics, commit_message, compatibility, entitlement::FreeEntitlementCache,
            knowledge, model_catalog, server_config, tab,
        },
        transport::{TransportParent, TransportRegistry},
    },
    Result,
};

pub fn router(
    registry: TransportRegistry,
    clients: crate::network::NetworkClients,
) -> Result<Router> {
    let proxy = CursorProxy::cursor(clients);
    let knowledge = knowledge::KnowledgeService::managed()?;
    Ok(router_with_proxy(registry, proxy, knowledge))
}

fn router_with_proxy(
    registry: TransportRegistry,
    proxy: CursorProxy,
    knowledge_service: knowledge::KnowledgeService,
) -> Router {
    let web_cache = registry.web_cache().router();
    let free_entitlements = FreeEntitlementCache::default();
    Router::new()
        .route("/__byok-api__/healthz", get(health))
        .route("/agent.v1.AgentService/RunSSE", post(run_sse_handler))
        .route("/aiserver.v1.BidiService/BidiAppend", post(bidi_handler))
        .route(
            "/aiserver.v1.InferenceService/RunInferenceSSE",
            post(run_inference_sse_handler),
        )
        .route(
            "/aiserver.v1.InferenceService/RunInferencePoll",
            post(run_inference_poll_handler),
        )
        .route(
            "/aiserver.v1.AiService/AvailableDocs",
            post(compatibility::available_docs),
        )
        .route(
            "/aiserver.v1.DashboardService/GetEffectiveUserPlugins",
            post(compatibility::effective_user_plugins),
        )
        .route(
            "/aiserver.v1.DashboardService/GetUserPrivacyMode",
            post(compatibility::user_privacy_mode),
        )
        .route(
            "/agent.v1.AgentService/UpdateConversationMetadata",
            post(compatibility::update_conversation_metadata),
        )
        .route(
            "/aiserver.v1.AiService/GetServerConfig",
            post(server_config::get),
        )
        .route(
            "/aiserver.v1.ServerConfigService/GetServerConfig",
            post(server_config::get),
        )
        .route(
            "/aiserver.v1.AiService/AvailableModels",
            post(model_catalog::available_models),
        )
        .route(
            "/agent.v1.AgentService/GetUsableModels",
            post(model_catalog::usable_models),
        )
        .route(
            "/aiserver.v1.AiService/GetUsableModels",
            post(model_catalog::usable_models),
        )
        .route(
            "/aiserver.v1.AiService/WriteGitCommitMessage",
            post(commit_message::write_git_commit_message),
        )
        .route(
            "/aiserver.v1.NetworkService/IsConnected",
            post(is_connected),
        )
        .route(
            "/agent.v1.AgentService/GetDefaultModelForCli",
            post(model_catalog::default_model_for_cli),
        )
        .route(
            "/aiserver.v1.AiService/GetDefaultModelForCli",
            post(model_catalog::default_model_for_cli),
        )
        .route(
            "/aiserver.v1.AiService/GetDefaultModel",
            post(model_catalog::default_model),
        )
        .route(
            "/aiserver.v1.AiService/GetDefaultModelNudgeData",
            post(model_catalog::default_model_nudge),
        )
        .route(
            "/aiserver.v1.AuthService/GetEmail",
            post(account::get_email),
        )
        .route(
            "/aiserver.v1.AuthService/GetUserMeta",
            post(account::get_user_meta),
        )
        .route("/aiserver.v1.DashboardService/GetMe", post(account::get_me))
        .route(
            "/aiserver.v1.DashboardService/GetTeams",
            post(account::get_teams),
        )
        .route(
            "/aiserver.v1.DashboardService/GetUserProfile",
            post(account::get_user_profile),
        )
        .route(
            "/aiserver.v1.DashboardService/GetCurrentPeriodUsage",
            post(account::current_period_usage),
        )
        .route(
            "/aiserver.v1.DashboardService/GetUsageLimitStatusAndActiveGrants",
            post(account::usage_limit_status),
        )
        .route(
            "/aiserver.v1.AiService/KnowledgeBaseAdd",
            post(knowledge::add),
        )
        .route(
            "/aiserver.v1.AiService/KnowledgeBaseList",
            post(knowledge::list),
        )
        .route(
            "/aiserver.v1.AiService/KnowledgeBaseUpdate",
            post(knowledge::update),
        )
        .route(
            "/aiserver.v1.AiService/KnowledgeBaseRemove",
            post(knowledge::remove),
        )
        .route(
            analytics::BOOTSTRAP_STATSIG_PATH,
            post(analytics::bootstrap_statsig),
        )
        .route("/auth/full_stripe_profile", get(account::stripe_profile))
        .route("/auth/stripe_profile", get(account::stripe_profile))
        .merge(tab::router())
        .route_layer(DefaultBodyLimit::disable())
        .route_layer(RequestDecompressionLayer::new())
        .fallback(proxy::forward)
        .method_not_allowed_fallback(proxy::forward)
        .layer(Extension(proxy))
        .layer(Extension(knowledge_service))
        .layer(Extension(free_entitlements))
        .with_state(registry)
        .merge(web_cache)
}

async fn health() -> StatusCode {
    StatusCode::NO_CONTENT
}

/// `NetworkService/IsConnected` probe. Cursor's always-local extension checks
/// connectivity roughly 10s after any slow request starts; a 404/error here is
/// treated as "network disconnected" and aborts in-flight work (e.g. commit
/// message generation) even while the model is still streaming. Always answer
/// connected with an empty `IsConnectedResponse` so local BYOK generation is
/// never cancelled by this probe.
async fn is_connected() -> Result<Response<Body>> {
    let payload = connect::encode_message(&ai::IsConnectedResponse {})?;
    let mut response = Response::new(Body::from(payload));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/proto"),
    );
    Ok(response)
}

async fn run_sse_handler(
    State(registry): State<TransportRegistry>,
    Extension(proxy): Extension<CursorProxy>,
    request: Request<Body>,
) -> Result<Response<Body>> {
    let (parts, body) = buffered(request).await?;
    let request: agent::BidiRequestId = connect::decode_unary(&body)?;
    let route = registry.wait_route(&request.request_id).await;
    let trace = registry.trace(&request.request_id);
    trace.resume();
    trace.request(
        "run_sse_request",
        body.clone(),
        serde_json::json!({"request_id": request.request_id}),
    );
    match route {
        crate::cursor::transport::TransportRoute::Local => {
            run_sse::stream(&registry, &request.request_id).await
        }
        crate::cursor::transport::TransportRoute::Upstream(generation) => {
            let response = proxy::forward(
                Extension(proxy),
                Request::from_parts(parts, Body::from(body)),
            )
            .await?;
            Ok(run_sse::upstream(
                registry,
                request.request_id,
                generation,
                response,
                Some(trace),
            )
            .await)
        }
    }
}

async fn bidi_handler(
    State(registry): State<TransportRegistry>,
    Extension(proxy): Extension<CursorProxy>,
    request: Request<Body>,
) -> Result<Response<Body>> {
    let (parts, body) = buffered(request).await?;
    let request: ai::BidiAppendRequest = connect::decode_unary(&body)?;
    let request_id = request
        .request_id
        .as_ref()
        .map(|id| id.request_id.clone())
        .unwrap_or_default();
    if !request_id.is_empty() && registry.inference().get(&request_id).is_some() {
        // Follow-up append (invoke_model / cancel / finish) on a local run.
        return inference_append(&registry, &request, &request_id, "").await;
    }
    if bidi::is_run_inference_append(&request) {
        // Cursor's managed-local-run harness: InferenceService/RunInference.
        let payload = hex::decode(&request.data)
            .map_err(|error| crate::Error::Protocol(format!("invalid BidiAppend hex: {error}")))?;
        let message = inference::decode_client_message(&payload)?;
        let model = inference::requested_model_id(&message).unwrap_or_default().to_owned();
        let local_model = registry.store().model(&model).await?;
        if model.starts_with(crate::plugin::ADAPTER_ID_PREFIX) || local_model.is_some() {
            let vendor = local_model.map_or("unknown", |config| inference_vendor(config.provider_type()));
            return inference_append(&registry, &request, &request_id, vendor).await;
        }
        tracing::info!(request_id, model, "RunInference for a Cursor model; forwarding upstream");
        if !request_id.is_empty() {
            registry.mark_upstream(&request_id).await;
        }
        return proxy::forward(
            Extension(proxy),
            Request::from_parts(parts, Body::from(body)),
        )
        .await;
    }
    let decoded = match bidi::decode(&request) {
        Ok(decoded) => decoded,
        Err(error) if !request_id.is_empty() && registry.upstream(&request_id).await => {
            // Follow-up append on a stream already handed to Cursor upstream.
            tracing::debug!(request_id, %error, "undecodable append on upstream stream; forwarding");
            return proxy::forward(
                Extension(proxy),
                Request::from_parts(parts, Body::from(body)),
            )
            .await;
        }
        Err(error) => return Err(error),
    };
    let first_model = decoded.model_id().map(str::to_owned);
    let conversation_id = decoded.conversation_id().map(str::to_owned);
    let trace_metadata = decoded.trace_metadata();
    let trace = registry.trace(&decoded.request_id);
    let local = if let Some(model_id) = decoded.model_id() {
        // 插件模型 ID 只在本地有意义,永远不转发到 Cursor 官方上游。
        if model_id.starts_with(crate::plugin::ADAPTER_ID_PREFIX)
            || registry.store().model(model_id).await?.is_some()
        {
            tracing::info!(
                request_id = decoded.request_id,
                model_id,
                "routing Cursor Run to BYOK provider"
            );
            true
        } else {
            tracing::info!(
                request_id = decoded.request_id,
                model_id,
                "routing Cursor Run to Cursor upstream"
            );
            false
        }
    } else if registry.local(&decoded.request_id).await.is_some() {
        true
    } else if registry.upstream(&decoded.request_id).await {
        false
    } else {
        trace.resume();
        trace.request(
            "bidi_request",
            body.clone(),
            trace_outcome(trace_metadata, false, "missing_transport", None),
        );
        return Err(crate::Error::Protocol(
            "first BidiAppend message must select a model".into(),
        ));
    };
    if first_model.is_some() {
        trace.begin(
            conversation_id.as_deref(),
            if local {
                "local_byok"
            } else {
                "cursor_official"
            },
            first_model.as_deref(),
        );
    } else {
        trace.resume();
    }
    if !local {
        if first_model.is_some() {
            registry.mark_upstream(&decoded.request_id).await;
        }
        trace.request(
            "bidi_request",
            body.clone(),
            trace_outcome(trace_metadata, true, "upstream", None),
        );
        return proxy::forward(
            Extension(proxy),
            Request::from_parts(parts, Body::from(body)),
        )
        .await;
    }
    let parent = match parent_headers(&parts.headers) {
        Ok(parent) => parent,
        Err(error) => {
            trace.request(
                "bidi_request",
                body,
                trace_outcome(
                    trace_metadata,
                    false,
                    "invalid_parent",
                    Some(error.to_string()),
                ),
            );
            return Err(error);
        }
    };
    match bidi::append(&registry, decoded, parent).await {
        Ok(_) => trace.request(
            "bidi_request",
            body,
            trace_outcome(trace_metadata, true, "local", None),
        ),
        Err(error) => {
            trace.request(
                "bidi_request",
                body,
                trace_outcome(
                    trace_metadata,
                    false,
                    "command_rejected",
                    Some(error.to_string()),
                ),
            );
            return Err(error);
        }
    }
    let mut response = Response::new(axum::body::Body::empty());
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/proto"),
    );
    Ok(response)
}

async fn inference_append(
    registry: &TransportRegistry,
    request: &ai::BidiAppendRequest,
    request_id: &str,
    vendor: &'static str,
) -> Result<Response<Body>> {
    if request_id.is_empty() {
        return Err(crate::Error::Protocol("RunInference BidiAppend has no request_id".into()));
    }
    let payload = hex::decode(&request.data)
        .map_err(|error| crate::Error::Protocol(format!("invalid BidiAppend hex: {error}")))?;
    let message = inference::decode_client_message(&payload)?;
    registry.inference().append(
        request_id,
        request.append_seqno,
        message,
        registry.provider(),
        vendor,
    )?;
    let mut response = Response::new(Body::from(connect::encode_message(&ai::BidiAppendResponse {})?));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/proto"),
    );
    Ok(response)
}

/// `run_ready.prompt_model_metadata.vendor` selects the harness prompt family.
fn inference_vendor(kind: crate::model::ProviderType) -> &'static str {
    match kind {
        crate::model::ProviderType::Anthropic => "anthropic",
        crate::model::ProviderType::OpenAiChat | crate::model::ProviderType::OpenAiResponses => {
            "openai"
        }
        crate::model::ProviderType::Plugin => "unknown",
    }
}

async fn run_inference_sse_handler(
    State(registry): State<TransportRegistry>,
    Extension(proxy): Extension<CursorProxy>,
    request: Request<Body>,
) -> Result<Response<Body>> {
    let (parts, body) = buffered(request).await?;
    let request: ai::BidiRequestId = connect::decode_unary(&body)?;
    match wait_inference(&registry, &request.request_id).await {
        Some(session) => Ok(inference_stream(session, inference::Downstream::Sse)),
        None => proxy::forward(Extension(proxy), Request::from_parts(parts, Body::from(body))).await,
    }
}

async fn run_inference_poll_handler(
    State(registry): State<TransportRegistry>,
    Extension(proxy): Extension<CursorProxy>,
    request: Request<Body>,
) -> Result<Response<Body>> {
    let (parts, body) = buffered(request).await?;
    let poll: crate::cursor::protocol::inference::BidiPollRequest = connect::decode_unary(&body)?;
    let request_id = poll.request_id.map(|id| id.request_id).unwrap_or_default();
    match wait_inference(&registry, &request_id).await {
        Some(session) => Ok(inference_stream(session, inference::Downstream::Poll)),
        None => proxy::forward(Extension(proxy), Request::from_parts(parts, Body::from(body))).await,
    }
}

/// The downstream request can race the first BidiAppend; wait briefly for
/// either a local session or an upstream route before falling through.
async fn wait_inference(
    registry: &TransportRegistry,
    request_id: &str,
) -> Option<std::sync::Arc<inference::Session>> {
    for _ in 0..100 {
        if let Some(session) = registry.inference().get(request_id) {
            return Some(session);
        }
        if registry.upstream(request_id).await {
            return None;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    None
}

fn inference_stream(
    session: std::sync::Arc<inference::Session>,
    mode: inference::Downstream,
) -> Response<Body> {
    let mut response = Response::new(Body::from_stream(session.downstream(mode)));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/connect+proto"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
}

fn trace_outcome(
    mut metadata: serde_json::Value,
    accepted: bool,
    route_outcome: &str,
    error: Option<String>,
) -> serde_json::Value {
    if let Some(metadata) = metadata.as_object_mut() {
        metadata.insert("accepted".into(), accepted.into());
        metadata.insert("route_outcome".into(), route_outcome.into());
        if let Some(error) = error {
            metadata.insert("error".into(), error.into());
        }
    }
    metadata
}

async fn buffered(request: Request<Body>) -> Result<(axum::http::request::Parts, Bytes)> {
    let (parts, body) = request.into_parts();
    let body = to_bytes(body, usize::MAX)
        .await
        .map_err(|error| crate::Error::Protocol(format!("cannot read request body: {error}")))?;
    Ok((parts, body))
}

fn parent_headers(headers: &HeaderMap) -> Result<Option<TransportParent>> {
    let request_id = header_text(headers, "x-parent-request-id")?;
    let tool_call_id = header_text(headers, "x-parent-agent-tool-call-id")?;
    match (request_id, tool_call_id) {
        (None, None) => Ok(None),
        (Some(request_id), Some(tool_call_id)) => Ok(Some(TransportParent {
            request_id: request_id.into(),
            tool_call_id: tool_call_id.into(),
        })),
        _ => Err(crate::Error::Protocol(
            "Cursor subagent request must include both parent headers".into(),
        )),
    }
}

fn header_text<'a>(headers: &'a HeaderMap, name: &str) -> Result<Option<&'a str>> {
    headers
        .get(name)
        .map(|value| value.to_str())
        .transpose()
        .map_err(|error| crate::Error::Protocol(format!("invalid {name} header: {error}")))
}
