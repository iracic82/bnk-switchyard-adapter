//! OpenAI-compatible HTTP surface: `POST /v1/chat/completions`, health, readiness, metrics.

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use futures::{Stream, StreamExt};
use serde_json::{Value, json};
use switchyard_protocol::{
    LlmClientError, LlmResponse, LlmResponseChunk, LlmResponseStream, Metadata, ModelId, Request,
    Usage,
};
use switchyard_translation::{
    LlmStreamError, WireFormat, decode_request, encode_aggregated_response, encode_stream,
};
use tracing::Instrument;

use crate::engine::{Decision, Engine, EngineError};
use crate::registry::PlanError;
use crate::tenancy::{Admission, Denied, Tenant, Tenants};

const SESSION_HEADER: &str = "x-switchyard-session-id";
/// Upstream response headers worth passing to the client: which endpoint served it.
const FORWARDED_UPSTREAM_HEADERS: &[&str] = &["x-served-by", "x-gateway-destination-endpoint"];

pub struct AppState {
    pub engine: Engine,
    pub tenants: Tenants,
    pub request_timeout: Duration,
    pub routing_timeout: Duration,
    pub hop_header: HeaderName,
    pub ready: AtomicBool,
    pub live: Option<crate::live::Live>,
}

pub fn router(state: Arc<AppState>, max_body_bytes: usize) -> Router {
    Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics))
        .route("/live", get(crate::live::page))
        .route("/live/events", get(crate::live::events))
        .layer(DefaultBodyLimit::max(max_body_bytes))
        .with_state(state)
}

async fn readyz(State(state): State<Arc<AppState>>) -> Response {
    if state.ready.load(Ordering::Relaxed) {
        (StatusCode::OK, "ready").into_response()
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "starting").into_response()
    }
}

async fn metrics(State(state): State<Arc<AppState>>) -> Response {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4",
        )],
        state.engine.metrics.render(),
    )
        .into_response()
}

/// Everything about one request that ends up in metrics and response headers.
#[derive(Clone)]
struct Ctx {
    request_id: String,
    tenant: Arc<Tenant>,
    route: String,
    algorithm: String,
    model: ModelId,
    provider_model: String,
    pool: String,
    started: Instant,
    selection_cost: f64,
    span: tracing::Span,
    served_by: String,
    frontier: crate::config::Cost,
    decision_ms: u64,
    score: Option<f64>,
    degraded: bool,
    fallbacks: usize,
    prompt: String,
}

async fn chat_completions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let request_id = request_id(&headers);
    let span = tracing::info_span!(
        "chat_completion",
        request_id = %request_id,
        tenant = tracing::field::Empty,
        route = tracing::field::Empty,
        algorithm = tracing::field::Empty,
        selected_model = tracing::field::Empty,
        inference_pool = tracing::field::Empty,
        decision_ms = tracing::field::Empty,
        fallbacks = tracing::field::Empty,
        status = tracing::field::Empty,
        otel.kind = "server",
    );
    let mut response = handle(&state, &headers, body, &request_id)
        .instrument(span.clone())
        .await;
    span.record("status", response.status().as_u16());
    if let Ok(value) = HeaderValue::from_str(&request_id) {
        response.headers_mut().insert("x-request-id", value);
    }
    response
}

/// Accepts a client request ID only if it is short and plain; otherwise makes a new one.
fn request_id(headers: &HeaderMap) -> String {
    headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .filter(|v| {
            !v.is_empty()
                && v.len() <= 128
                && v.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b))
        })
        .map(str::to_owned)
        .unwrap_or_else(|| uuid::Uuid::now_v7().to_string())
}

async fn handle(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    body: Bytes,
    request_id: &str,
) -> Response {
    let metrics = &state.engine.metrics;
    // A request that already passed through this adapter means a routing loop.
    if headers.contains_key(&state.hop_header) {
        tracing::error!("request already carries the adapter hop header; refusing a routing loop");
        metrics.rejections.with_label_values(&["", "loop"]).inc();
        return error(
            StatusCode::LOOP_DETECTED,
            "request has already passed through the Switchyard adapter",
            "loop_detected",
        );
    }
    let tenant = match state.tenants.authenticate(headers) {
        Ok(tenant) => tenant,
        Err(denied) => return denied_response(state, "", "", &denied),
    };
    tracing::Span::current().record("tenant", tenant.id.as_str());

    let mut body: Value = match serde_json::from_slice(&body) {
        Ok(Value::Object(map)) => Value::Object(map),
        Ok(_) => {
            return error(
                StatusCode::BAD_REQUEST,
                "body must be a JSON object",
                "invalid_request_error",
            );
        }
        Err(e) => {
            return error(
                StatusCode::BAD_REQUEST,
                &format!("invalid JSON: {e}"),
                "invalid_request_error",
            );
        }
    };
    let Some(route_name) = body.get("model").and_then(Value::as_str).map(str::to_owned) else {
        return error(
            StatusCode::BAD_REQUEST,
            "body must include a string `model`",
            "invalid_request_error",
        );
    };
    tracing::Span::current().record("route", route_name.as_str());
    let registry = &state.engine.registry;
    let Some(route) = registry.routes.get(&route_name).cloned() else {
        return error(
            StatusCode::NOT_FOUND,
            &format!("unknown model or route {route_name}"),
            "model_not_found",
        );
    };
    let is_direct = registry.models.contains_key(route_name.as_str());
    if let Err(denied) = tenant.check_route(&route_name, is_direct) {
        return denied_response(state, &tenant.id, &route_name, &denied);
    }
    let max_tokens = body
        .get("max_completion_tokens")
        .or_else(|| body.get("max_tokens"))
        .and_then(Value::as_u64);
    if let Err(denied) = tenant.check_max_tokens(max_tokens) {
        return denied_response(state, &tenant.id, &route_name, &denied);
    }
    let plan = match registry.plan(&route, &tenant.allowed_models, tenant.limits.tier_fallback) {
        Ok(plan) => plan,
        Err(PlanError::ModelNotAllowed(model)) => {
            publish_refusal(state, &tenant.id, &route_name, "model_not_allowed");
            metrics
                .rejections
                .with_label_values(&[tenant.id.as_str(), "model_not_allowed"])
                .inc();
            return error(
                StatusCode::FORBIDDEN,
                &format!("model {model} is not permitted for tenant {}", tenant.id),
                "model_not_allowed",
            );
        }
        Err(e @ PlanError::TierNotAllowed { .. }) => {
            publish_refusal(state, &tenant.id, &route_name, "tier_not_allowed");
            metrics
                .rejections
                .with_label_values(&[tenant.id.as_str(), "tier_not_allowed"])
                .inc();
            return error(StatusCode::FORBIDDEN, &e.to_string(), "tier_not_allowed");
        }
    };
    let admission = match tenant.admit() {
        Ok(admission) => admission,
        Err(denied) => return denied_response(state, &tenant.id, &route_name, &denied),
    };

    let prompt = state
        .live
        .as_ref()
        .map(|live| live.preview(&body))
        .unwrap_or_default();
    // Only trusted translation hops may carry Switchyard preservation state.
    if let Some(meta) = body.get_mut("metadata").and_then(Value::as_object_mut) {
        meta.remove(switchyard_translation::util::SWITCHYARD_METADATA_KEY);
    }
    let streaming = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let client_wants_usage = body
        .pointer("/stream_options/include_usage")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if streaming && !client_wants_usage {
        // Ask upstream for usage so tokens and cost are accounted; the usage-only chunk is
        // removed again before the client sees the stream.
        let options = body
            .as_object_mut()
            .expect("checked object")
            .entry("stream_options")
            .or_insert_with(|| json!({}));
        if let Some(options) = options.as_object_mut() {
            options.insert("include_usage".into(), Value::Bool(true));
        }
    }
    let llm_request = match decode_request(WireFormat::OpenAiChat, &body) {
        Ok(request) => request,
        Err(e) => {
            return error(
                StatusCode::BAD_REQUEST,
                &e.to_string(),
                "invalid_request_error",
            );
        }
    };
    let request = Request {
        llm_request,
        raw_request: Some(body),
        metadata: Some(metadata(headers, &tenant.id, request_id)),
    };

    let deadline = tokio::time::Instant::now() + state.request_timeout;
    let started = Instant::now();
    metrics.inflight.inc();
    let _inflight = InflightGuard(Arc::clone(state));

    let decision = match state
        .engine
        .decide(&plan, request, state.routing_timeout)
        .await
    {
        Ok(decision) => decision,
        Err(e) => return engine_error(state, &tenant.id, &route_name, &e),
    };
    let algorithm = decision.algorithm.clone();
    tracing::Span::current().record("algorithm", algorithm.as_str());
    tracing::Span::current().record("decision_ms", decision.elapsed.as_millis() as u64);
    let selection_cost = selection_cost(state, &tenant, &decision);
    let decision_ms = decision.elapsed.as_millis();
    let evidence = decision.evidence.clone();

    let answer = match tokio::time::timeout_at(deadline, state.engine.answer(&plan, decision)).await
    {
        Ok(Ok(answer)) => answer,
        Ok(Err(e)) => {
            tenant.record_spend(selection_cost);
            return engine_error(state, &tenant.id, &route_name, &e);
        }
        Err(_) => {
            tenant.record_spend(selection_cost);
            return error(StatusCode::GATEWAY_TIMEOUT, "request timed out", "timeout");
        }
    };
    let ctx = Ctx {
        request_id: request_id.to_owned(),
        tenant: Arc::clone(&tenant),
        route: route_name,
        algorithm,
        provider_model: registry.provider_model(&answer.model).to_owned(),
        pool: registry.pool(&answer.model).to_owned(),
        model: answer.model.clone(),
        started,
        selection_cost,
        span: answer.span.clone(),
        served_by: answer
            .response
            .upstream_headers
            .get("x-served-by")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned(),
        frontier: registry.frontier_cost(&plan.route),
        decision_ms: decision_ms as u64,
        score: evidence
            .as_ref()
            .and_then(|e| e.get("score"))
            .and_then(Value::as_f64),
        degraded: plan.degraded,
        fallbacks: answer.fallbacks.len(),
        prompt,
    };
    let span = tracing::Span::current();
    span.record("selected_model", ctx.model.as_str());
    span.record("inference_pool", ctx.pool.as_str());
    span.record("fallbacks", answer.fallbacks.len());
    tracing::info!(
        evidence = %evidence.as_ref().map(|v| v.to_string()).unwrap_or_default(),
        decision_ms = decision_ms as u64,
        from_routing = answer.from_routing,
        "routed"
    );

    let mut response = respond(
        state,
        &ctx,
        answer.response,
        streaming,
        client_wants_usage,
        deadline,
        admission,
    );
    let headers = response.headers_mut();
    set_header(headers, "x-switchyard-route", &ctx.route);
    set_header(headers, "x-switchyard-algorithm", &ctx.algorithm);
    set_header(headers, "x-switchyard-model", ctx.model.as_str());
    set_header(headers, "x-switchyard-provider-model", &ctx.provider_model);
    set_header(headers, "x-switchyard-inference-pool", &ctx.pool);
    set_header(
        headers,
        "x-switchyard-decision-ms",
        &decision_ms.to_string(),
    );
    set_header(
        headers,
        "x-switchyard-backend-ms",
        &answer.backend_elapsed.as_millis().to_string(),
    );
    if !answer.fallbacks.is_empty() {
        let chain: Vec<String> = answer
            .fallbacks
            .iter()
            .map(|f| format!("{}>{}:{}", f.from, f.to, f.reason))
            .collect();
        set_header(headers, "x-switchyard-fallbacks", &chain.join(","));
    }
    if plan.degraded {
        set_header(headers, "x-switchyard-policy", "degraded");
    }
    response
}

fn respond(
    state: &Arc<AppState>,
    ctx: &Ctx,
    mut response: switchyard_protocol::Response,
    streaming: bool,
    client_wants_usage: bool,
    deadline: tokio::time::Instant,
    admission: Admission,
) -> Response {
    let upstream_headers = std::mem::take(&mut response.upstream_headers);
    let mut http_response = match response.llm_response {
        LlmResponse::Agg(agg) => {
            let usage = agg.usage.clone();
            account(state, ctx, Some(&usage), "ok", None);
            drop(admission);
            match encode_aggregated_response(
                &agg,
                WireFormat::OpenAiChat,
                Some(&ctx.provider_model),
            ) {
                Ok(body) => Json(body).into_response(),
                Err(e) => error(
                    StatusCode::BAD_GATEWAY,
                    &format!("response encoding failed: {e}"),
                    "upstream_error",
                ),
            }
        }
        LlmResponse::Stream(stream) => {
            let stream = observe_stream(
                Arc::clone(state),
                ctx.clone(),
                stream,
                client_wants_usage,
                admission,
            );
            match encode_stream(
                stream,
                WireFormat::OpenAiChat,
                Some(ctx.provider_model.clone()),
            ) {
                Ok(events) => sse(events, deadline).into_response(),
                Err(e) => error(
                    StatusCode::BAD_GATEWAY,
                    &format!("stream encoding failed: {e}"),
                    "upstream_error",
                ),
            }
        }
    };
    if streaming
        && !matches!(http_response.headers().get(axum::http::header::CONTENT_TYPE), Some(v) if v.as_bytes().starts_with(b"text/event-stream"))
    {
        tracing::debug!("client asked for a stream; upstream answered with a buffered response");
    }
    for name in FORWARDED_UPSTREAM_HEADERS {
        if let Some(value) = upstream_headers.get(*name) {
            http_response.headers_mut().insert(*name, value.clone());
        }
    }
    http_response
}

/// Records metrics, tokens and estimated cost for one finished (or abandoned) answer.
fn account(
    state: &AppState,
    ctx: &Ctx,
    usage: Option<&Usage>,
    status: &str,
    ttft: Option<Duration>,
) {
    let m = &state.engine.metrics;
    let tenant = ctx.tenant.id.as_str();
    let model = ctx.model.as_str();
    m.requests
        .with_label_values(&[
            tenant,
            ctx.route.as_str(),
            ctx.algorithm.as_str(),
            model,
            ctx.pool.as_str(),
            status,
        ])
        .inc();
    m.request_seconds
        .with_label_values(&[ctx.route.as_str(), model])
        .observe(ctx.started.elapsed().as_secs_f64());
    if let Some(ttft) = ttft {
        m.ttft_seconds
            .with_label_values(&[model, ctx.pool.as_str()])
            .observe(ttft.as_secs_f64());
    }
    let (input, output) = usage
        .map(|u| (u.input_tokens.unwrap_or(0), u.output_tokens.unwrap_or(0)))
        .unwrap_or((0, 0));
    m.tokens
        .with_label_values(&[tenant, model, "inference", "input"])
        .inc_by(input);
    m.tokens
        .with_label_values(&[tenant, model, "inference", "output"])
        .inc_by(output);
    let cost = state
        .engine
        .registry
        .cost(&ctx.model)
        .estimate(input, output);
    m.cost_usd
        .with_label_values(&[tenant, model, "inference"])
        .inc_by(cost);
    ctx.tenant.record_spend(cost + ctx.selection_cost);
    if let Some(live) = &state.live {
        live.publish(crate::live::LiveEvent {
            request_id: ctx.request_id.clone(),
            tenant: tenant.to_owned(),
            route: ctx.route.clone(),
            algorithm: ctx.algorithm.clone(),
            model: model.to_owned(),
            provider_model: ctx.provider_model.clone(),
            pool: ctx.pool.clone(),
            served_by: ctx.served_by.clone(),
            status: status.to_owned(),
            reason: if ctx.degraded {
                "degraded".into()
            } else {
                String::new()
            },
            decision_ms: ctx.decision_ms,
            ttft_ms: ttft.map(|t| t.as_millis() as u64),
            e2e_ms: ctx.started.elapsed().as_millis() as u64,
            input_tokens: input,
            output_tokens: output,
            selection_cost_usd: ctx.selection_cost,
            inference_cost_usd: cost,
            frontier_cost_usd: ctx.frontier.estimate(input, output),
            score: ctx.score,
            fallbacks: ctx.fallbacks,
            prompt: ctx.prompt.clone(),
            ..Default::default()
        });
    }
    ctx.span.record("input_tokens", input);
    ctx.span.record("output_tokens", output);
    ctx.span.record("inference_cost_usd", cost);
    ctx.span.record("outcome", status);
    // Span durations end at response headers; this is the full time including the stream.
    ctx.span
        .record("e2e_ms", ctx.started.elapsed().as_millis() as u64);
    if let Some(ttft) = ttft {
        ctx.span.record("ttft_ms", ttft.as_millis() as u64);
    }
    tracing::info!(
        request_id = %ctx.request_id,
        tenant,
        route = %ctx.route,
        algorithm = %ctx.algorithm,
        selected_model = model,
        inference_pool = %ctx.pool,
        input_tokens = input,
        output_tokens = output,
        usage_reported = usage.is_some(),
        ttft_ms = ttft.map(|t| t.as_millis() as u64),
        e2e_ms = ctx.started.elapsed().as_millis() as u64,
        selection_cost_usd = ctx.selection_cost,
        inference_cost_usd = cost,
        status,
        "request finished"
    );
}

pub(crate) fn selection_cost(state: &AppState, tenant: &Tenant, decision: &Decision) -> f64 {
    let m = &state.engine.metrics;
    let mut total = 0.0;
    for call in &decision.calls {
        let Some(usage) = &call.usage else { continue };
        let (input, output) = (
            usage.input_tokens.unwrap_or(0),
            usage.output_tokens.unwrap_or(0),
        );
        let model = call.model.as_str();
        m.tokens
            .with_label_values(&[tenant.id.as_str(), model, "selection", "input"])
            .inc_by(input);
        m.tokens
            .with_label_values(&[tenant.id.as_str(), model, "selection", "output"])
            .inc_by(output);
        let cost = state
            .engine
            .registry
            .cost(&call.model)
            .estimate(input, output);
        m.cost_usd
            .with_label_values(&[tenant.id.as_str(), model, "selection"])
            .inc_by(cost);
        total += cost;
    }
    total
}

/// Accounting for a stream happens once, when the stream ends or the client goes away.
struct StreamAccount {
    state: Arc<AppState>,
    ctx: Ctx,
    usage: Option<Usage>,
    ttft: Option<Duration>,
    status: &'static str,
    _admission: Admission,
}

impl Drop for StreamAccount {
    fn drop(&mut self) {
        account(
            &self.state,
            &self.ctx,
            self.usage.as_ref(),
            self.status,
            self.ttft,
        );
    }
}

fn observe_stream(
    state: Arc<AppState>,
    ctx: Ctx,
    mut stream: LlmResponseStream,
    client_wants_usage: bool,
    admission: Admission,
) -> LlmResponseStream {
    Box::pin(async_stream::stream! {
        let mut account = StreamAccount {
            state, ctx, usage: None, ttft: None, status: "cancelled", _admission: admission,
        };
        while let Some(item) = stream.next().await {
            match &item {
                Ok(event) => {
                    let chunks = event.normalized();
                    if account.ttft.is_none() && chunks.iter().any(|c| matches!(c,
                        LlmResponseChunk::TextDelta { .. }
                        | LlmResponseChunk::ToolCallDelta { .. }
                        | LlmResponseChunk::ReasoningDelta { .. }))
                    {
                        account.ttft = Some(account.ctx.started.elapsed());
                    }
                    let mut usage_only = !chunks.is_empty();
                    for chunk in chunks {
                        match chunk {
                            LlmResponseChunk::Usage(usage) => account.usage = Some(usage.clone()),
                            LlmResponseChunk::StreamError { .. } | LlmResponseChunk::DecodeError { .. } => {
                                account.status = "stream_error";
                                usage_only = false;
                            }
                            LlmResponseChunk::MessageStop { .. } => {
                                if account.status == "cancelled" { account.status = "ok"; }
                                usage_only = false;
                            }
                            _ => usage_only = false,
                        }
                    }
                    if usage_only && !client_wants_usage {
                        continue;
                    }
                }
                Err(_) => account.status = "stream_error",
            }
            yield item;
        }
        if account.status == "cancelled" {
            // The upstream ended without a stop event.
            account.status = "ok";
        }
    })
}

type SseStream = std::pin::Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>;

fn sse(
    events: switchyard_translation::RawEventStream,
    deadline: tokio::time::Instant,
) -> Sse<SseStream> {
    let framed = async_stream::stream! {
        let mut events = events;
        let mut failed = false;
        loop {
            let item = match tokio::time::timeout_at(deadline, events.next()).await {
                Ok(Some(item)) => item,
                Ok(None) => break,
                Err(_) => {
                    failed = true;
                    yield Ok(stream_error("request timed out", "timeout"));
                    break;
                }
            };
            match item {
                Ok(value) => yield Ok(Event::default().data(value.to_string())),
                Err(LlmStreamError::Upstream(value)) => {
                    failed = true;
                    yield Ok(Event::default().data(value.to_string()));
                    break;
                }
                Err(LlmStreamError::Client(e)) => {
                    failed = true;
                    yield Ok(stream_error(&e.to_string(), "upstream_stream_error"));
                    break;
                }
            }
        }
        // `[DONE]` tells OpenAI clients the answer is complete; never send it after a failure.
        if !failed {
            yield Ok(Event::default().data("[DONE]"));
        }
    };
    Sse::new(Box::pin(framed) as SseStream)
}

fn stream_error(message: &str, code: &str) -> Event {
    Event::default().data(
        json!({"error": {"message": message, "type": "upstream_error", "code": code}}).to_string(),
    )
}

/// Builds request metadata from trusted values only. The client's session ID is kept as a
/// routing hint but scoped to the tenant, so tenants cannot share routing state.
pub(crate) fn metadata(headers: &HeaderMap, tenant: &str, request_id: &str) -> Metadata {
    let mut forwarded = http::HeaderMap::new();
    if let Ok(value) = HeaderValue::from_str(request_id) {
        forwarded.insert("x-request-id", value);
    }
    let session_id = headers
        .get(SESSION_HEADER)
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty() && v.len() <= 256)
        .map(|v| format!("{tenant}:{v}"));
    Metadata {
        session_id,
        correlation_id: Some(request_id.to_owned()),
        http_headers: Some(forwarded),
        ..Metadata::default()
    }
}

struct InflightGuard(Arc<AppState>);

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.0.engine.metrics.inflight.dec();
    }
}

fn set_header(headers: &mut HeaderMap, name: &'static str, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        headers.insert(name, value);
    }
}

fn error(status: StatusCode, message: &str, code: &str) -> Response {
    let kind = if status.is_server_error() {
        "server_error"
    } else {
        "invalid_request_error"
    };
    (
        status,
        Json(json!({"error": {"message": message, "type": kind, "code": code}})),
    )
        .into_response()
}

/// Publishes a policy refusal to the live view.
fn publish_refusal(state: &AppState, tenant: &str, route: &str, reason: &str) {
    if let Some(live) = &state.live {
        live.publish(crate::live::LiveEvent {
            tenant: tenant.to_owned(),
            route: route.to_owned(),
            status: "refused".into(),
            reason: reason.to_owned(),
            ..Default::default()
        });
    }
}

fn denied_response(state: &AppState, tenant: &str, route: &str, denied: &Denied) -> Response {
    publish_refusal(state, tenant, route, denied.reason());
    state
        .engine
        .metrics
        .rejections
        .with_label_values(&[tenant, denied.reason()])
        .inc();
    let status = match denied {
        Denied::Unauthenticated => StatusCode::UNAUTHORIZED,
        Denied::RouteNotAllowed(_) => StatusCode::FORBIDDEN,
        Denied::MaxTokens { .. } => StatusCode::BAD_REQUEST,
        Denied::RateLimited | Denied::ConcurrencyLimited | Denied::BudgetExhausted => {
            StatusCode::TOO_MANY_REQUESTS
        }
    };
    error(status, &denied.to_string(), denied.reason())
}

fn engine_error(state: &AppState, tenant: &str, route: &str, e: &EngineError) -> Response {
    tracing::warn!(error = %e, "request failed");
    state
        .engine
        .metrics
        .requests
        .with_label_values(&[tenant, route, "", "", "", "error"])
        .inc();
    match e {
        EngineError::RoutingTimeout(_) => error(
            StatusCode::GATEWAY_TIMEOUT,
            &e.to_string(),
            "routing_timeout",
        ),
        EngineError::PolicyViolation(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &e.to_string(),
            "policy_violation",
        ),
        EngineError::Upstream { source, .. } => upstream_error(source),
        EngineError::Routing(switchyard_libsy::LibsyError::ClientCall { source, .. }) => {
            upstream_error(source)
        }
        EngineError::Routing(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &e.to_string(),
            "routing_error",
        ),
    }
}

/// Passes the upstream status and body through unchanged when the body is an OpenAI error
/// object, so clients see the inference server's own error.
fn upstream_error(source: &LlmClientError) -> Response {
    match source {
        LlmClientError::UpstreamHttp { status, body } => {
            let status = StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            match serde_json::from_str::<Value>(body) {
                Ok(value) if value.get("error").is_some() => (status, Json(value)).into_response(),
                _ => error(status, body, "upstream_error"),
            }
        }
        LlmClientError::ContextWindowExceeded { message, .. } => {
            error(StatusCode::BAD_REQUEST, message, "context_length_exceeded")
        }
        LlmClientError::Timeout { .. } => error(
            StatusCode::GATEWAY_TIMEOUT,
            &source.to_string(),
            "upstream_timeout",
        ),
        LlmClientError::Transport { .. } | LlmClientError::TemporarilyUnavailable => error(
            StatusCode::BAD_GATEWAY,
            &source.to_string(),
            "upstream_unavailable",
        ),
        LlmClientError::InvalidRequest { .. } | LlmClientError::RequestTranslation(_) => error(
            StatusCode::BAD_REQUEST,
            &source.to_string(),
            "invalid_request_error",
        ),
        _ => error(
            StatusCode::BAD_GATEWAY,
            &source.to_string(),
            "upstream_error",
        ),
    }
}
