//! Envoy external processing (ext_proc) server: Switchyard as a gateway's model router.
//!
//! The gateway (Envoy, or BNK's AI gateway through its semantic-router slot) keeps the
//! client connection and the upstream hop. For each request it sends the headers and the
//! buffered body here; the adapter runs the route's Switchyard algorithm and answers with:
//!
//! - the body with `model` rewritten to the selected model's `gateway_model`,
//! - `x-selected-model` (the header vLLM semantic-router sets) and `x-switchyard-*` headers,
//! - `content-length` removed, and `clear_route_cache` set so the gateway re-matches routes.
//!
//! Configure the gateway with `request_body_mode: BUFFERED`. Requests whose `model` is not
//! a Switchyard route pass through unchanged.

use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use envoy_types::pb::envoy::config::core::v3::{HeaderValue, HeaderValueOption};
use envoy_types::pb::envoy::service::ext_proc::v3::external_processor_server::{
    ExternalProcessor, ExternalProcessorServer,
};
use envoy_types::pb::envoy::service::ext_proc::v3::processing_request::Request as Req;
use envoy_types::pb::envoy::service::ext_proc::v3::processing_response::Response as Resp;
use envoy_types::pb::envoy::service::ext_proc::v3::{
    BodyMutation, BodyResponse, CommonResponse, HeaderMutation, HeadersResponse, ProcessingRequest,
    ProcessingResponse, TrailersResponse, body_mutation,
};
use futures::Stream;
use serde_json::Value;
use switchyard_protocol::{ModelId, Request};
use switchyard_translation::{WireFormat, decode_request};
use tonic::{Status, Streaming};

use crate::http::AppState;
use crate::live::LiveEvent;

pub struct ExtProc {
    state: Arc<AppState>,
    tenant: String,
}

impl ExtProc {
    pub fn server(state: Arc<AppState>, tenant: String) -> ExternalProcessorServer<Self> {
        ExternalProcessorServer::new(Self { state, tenant })
    }
}

type Outbound = Pin<Box<dyn Stream<Item = Result<ProcessingResponse, Status>> + Send>>;

#[tonic::async_trait]
impl ExternalProcessor for ExtProc {
    type ProcessStream = Outbound;

    async fn process(
        &self,
        request: tonic::Request<Streaming<ProcessingRequest>>,
    ) -> Result<tonic::Response<Self::ProcessStream>, Status> {
        let mut inbound = request.into_inner();
        let state = Arc::clone(&self.state);
        let tenant = self.tenant.clone();
        let outbound = async_stream::try_stream! {
            let mut headers = http::HeaderMap::new();
            let mut body = Vec::new();
            while let Some(message) = inbound.message().await? {
                match message.request {
                    Some(Req::RequestHeaders(h)) => {
                        for header in h.headers.map(|m| m.headers).unwrap_or_default() {
                            let value = if header.raw_value.is_empty() { header.value.into_bytes() } else { header.raw_value };
                            if let (Ok(name), Ok(value)) = (
                                http::HeaderName::from_bytes(header.key.as_bytes()),
                                http::HeaderValue::from_bytes(&value),
                            ) {
                                headers.append(name, value);
                            }
                        }
                        yield respond(Resp::RequestHeaders(HeadersResponse::default()));
                    }
                    Some(Req::RequestBody(chunk)) => {
                        body.extend_from_slice(&chunk.body);
                        if !chunk.end_of_stream {
                            // Streamed mode: let chunks through; only a complete body is routed.
                            yield respond(Resp::RequestBody(BodyResponse::default()));
                            continue;
                        }
                        let decided = decide(&state, &tenant, &headers, std::mem::take(&mut body)).await;
                        yield respond(Resp::RequestBody(decided));
                    }
                    Some(Req::ResponseHeaders(_)) => yield respond(Resp::ResponseHeaders(HeadersResponse::default())),
                    Some(Req::ResponseBody(_)) => yield respond(Resp::ResponseBody(BodyResponse::default())),
                    Some(Req::RequestTrailers(_)) => yield respond(Resp::RequestTrailers(TrailersResponse::default())),
                    Some(Req::ResponseTrailers(_)) => yield respond(Resp::ResponseTrailers(TrailersResponse::default())),
                    None => {}
                }
            }
        };
        Ok(tonic::Response::new(Box::pin(outbound)))
    }
}

fn respond(response: Resp) -> ProcessingResponse {
    ProcessingResponse {
        response: Some(response),
        ..Default::default()
    }
}

fn header(key: &str, value: &str) -> HeaderValueOption {
    HeaderValueOption {
        header: Some(HeaderValue {
            key: key.to_owned(),
            raw_value: value.as_bytes().to_vec(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Runs the route's algorithm and returns the body response for the gateway. Never fails
/// the request: on routing errors it routes to the route's capable model.
async fn decide(
    state: &Arc<AppState>,
    tenant_id: &str,
    headers: &http::HeaderMap,
    raw: Vec<u8>,
) -> BodyResponse {
    let Ok(Value::Object(mut map)) = serde_json::from_slice::<Value>(&raw) else {
        return BodyResponse::default();
    };
    let registry = &state.engine.registry;
    let Some(route_name) = map.get("model").and_then(Value::as_str).map(str::to_owned) else {
        return BodyResponse::default();
    };
    // A concrete model is labelled for the gateway's routes, not decided again.
    if let Some(model) = registry.known_model(&route_name) {
        let gateway_model = registry.gateway_model(&model).to_owned();
        map.insert("model".into(), Value::String(gateway_model.clone()));
        let Ok(body) = serde_json::to_vec(&Value::Object(map)) else {
            return BodyResponse::default();
        };
        return mutation(
            vec![
                header("x-selected-model", &gateway_model),
                header("x-switchyard-model", model.as_str()),
                header("x-switchyard-route", "direct"),
            ],
            body,
        );
    }
    // Anything that is neither a model nor a Switchyard route passes through unchanged.
    let Some(route) = registry.routes.get(&route_name).cloned() else {
        return BodyResponse::default();
    };
    let Some(tenant) = state.tenants.by_id(tenant_id) else {
        return BodyResponse::default();
    };
    let request_id = headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty() && v.len() <= 128)
        .map(str::to_owned)
        .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
    let span = tracing::info_span!("ext_proc_decision", request_id = %request_id, route = %route_name, tenant = %tenant.id);
    let _enter = span.enter();
    let started = Instant::now();
    let prompt = state
        .live
        .as_ref()
        .map(|live| live.preview(&Value::Object(map.clone())))
        .unwrap_or_default();

    let decision = async {
        let plan = registry
            .plan(&route, &tenant.allowed_models, tenant.limits.tier_fallback)
            .map_err(|e| e.to_string())?;
        let llm_request = decode_request(WireFormat::OpenAiChat, &Value::Object(map.clone()))
            .map_err(|e| e.to_string())?;
        let request = Request {
            llm_request,
            raw_request: Some(Value::Object(map.clone())),
            metadata: Some(crate::http::metadata(headers, &tenant.id, &request_id)),
        };
        let decision = state
            .engine
            .decide(&plan, request, state.routing_timeout)
            .await
            .map_err(|e| e.to_string())?;
        Ok::<_, String>((plan, decision))
    }
    .await;

    let (model, algorithm, score, selection_cost, reason): (
        ModelId,
        String,
        Option<f64>,
        f64,
        String,
    ) = match &decision {
        Ok((_, d)) => (
            d.outcome.selected_model_ids[0].clone(),
            d.algorithm.clone(),
            d.evidence
                .as_ref()
                .and_then(|e| e.get("score"))
                .and_then(Value::as_f64),
            crate::http::selection_cost(state, &tenant, d),
            String::new(),
        ),
        Err(error) => {
            tracing::warn!(%error, "ext_proc routing failed; using the route's capable model");
            match registry.fail_open_model(&route) {
                Some(model) => (
                    model,
                    route.algorithm.name().to_owned(),
                    None,
                    0.0,
                    "fail_open".into(),
                ),
                None => return BodyResponse::default(),
            }
        }
    };
    let decision_ms = started.elapsed().as_millis() as u64;
    let gateway_model = registry.gateway_model(&model).to_owned();
    map.insert("model".into(), Value::String(gateway_model.clone()));
    let Ok(body) = serde_json::to_vec(&Value::Object(map)) else {
        return BodyResponse::default();
    };
    tracing::info!(selected_model = %model, gateway_model = %gateway_model, decision_ms, "ext_proc routed");
    tenant.record_spend(selection_cost);
    if let Some(live) = &state.live {
        live.publish(LiveEvent {
            request_id: request_id.clone(),
            tenant: tenant.id.clone(),
            route: route_name.clone(),
            algorithm: algorithm.clone(),
            model: model.to_string(),
            provider_model: gateway_model.clone(),
            pool: registry.pool(&model).to_owned(),
            status: "routed".into(),
            reason: reason.clone(),
            decision_ms,
            selection_cost_usd: selection_cost,
            score,
            prompt,
            ..Default::default()
        });
    }
    let mut set_headers = vec![
        header("x-selected-model", &gateway_model),
        header("x-switchyard-model", model.as_str()),
        header("x-switchyard-route", &route_name),
        header("x-switchyard-algorithm", &algorithm),
        header("x-switchyard-decision-ms", &decision_ms.to_string()),
        header("x-request-id", &request_id),
    ];
    if !reason.is_empty() {
        set_headers.push(header("x-switchyard-policy", &reason));
    }
    mutation(set_headers, body)
}

/// Headers and body for the gateway, with the route cache cleared so it re-matches routes.
fn mutation(set_headers: Vec<HeaderValueOption>, body: Vec<u8>) -> BodyResponse {
    BodyResponse {
        response: Some(CommonResponse {
            header_mutation: Some(HeaderMutation {
                set_headers,
                remove_headers: vec!["content-length".into()],
            }),
            body_mutation: Some(BodyMutation {
                mutation: Some(body_mutation::Mutation::Body(body)),
            }),
            clear_route_cache: true,
            ..Default::default()
        }),
    }
}
