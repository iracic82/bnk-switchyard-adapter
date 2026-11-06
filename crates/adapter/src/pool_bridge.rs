//! Pool bridge: an InferencePool upstream for gateways that can only call a URL.
//!
//! BNK 2.4's AI gateway (Polaris) sends a model's traffic to a provider URL; it has no
//! InferencePool or endpoint-picker field, and a provider URL that is another BNK Gateway
//! VIP is reset. The bridge closes that gap. A provider points at
//! `http://<bridge>/pools/<pool>/v1`; for each request the bridge asks the pool's endpoint
//! picker (the F5 EPP, or any Gateway API Inference Extension EPP) which pod should serve it,
//! using the same ext_proc exchange a gateway uses, and forwards the request to that pod.
//!
//! The exchange: the bridge streams the request headers and the whole body to the picker;
//! the picker answers with `x-gateway-destination-endpoint: <ip>:<port>` (a header mutation,
//! and the same value in the `envoy.lb` dynamic metadata). When the upstream answers, the
//! bridge reports the response headers and the end of the body back, so the picker sees the
//! request finish, as it would behind a gateway.
//!
//! Any other header the picker sets is added to the forwarded request, and a body it replaces
//! is forwarded instead, as a gateway applies an ext_proc mutation. Dynamo's frontend sidecar in
//! direct mode, for example, serves the worker named in `x-dynamo-worker-instance-id`.
//!
//! Failure policy follows the pool's FailOpen: if the picker cannot be reached or does not
//! answer within `pick_timeout_ms`, the request goes to the pool's Service instead. An
//! immediate response from the picker (for example 429 when every pod is saturated) is
//! returned to the caller unchanged. Every response says which path it took in
//! `x-pool-pick` (`epp` or `fail-open`) and `x-pool-endpoint`.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue as HttpHeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use envoy_types::pb::envoy::config::core::v3::{HeaderMap as PbHeaderMap, HeaderValue};
use envoy_types::pb::envoy::service::ext_proc::v3::body_mutation::Mutation as BodyMut;
use envoy_types::pb::envoy::service::ext_proc::v3::external_processor_client::ExternalProcessorClient;
use envoy_types::pb::envoy::service::ext_proc::v3::processing_request::Request as Req;
use envoy_types::pb::envoy::service::ext_proc::v3::processing_response::Response as Resp;
use envoy_types::pb::envoy::service::ext_proc::v3::{
    CommonResponse, HttpBody, HttpHeaders, ProcessingRequest, ProcessingResponse,
};
use futures::StreamExt;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::{Channel, Endpoint};

use crate::config::PoolBridgeConfig;
use crate::metrics::Metrics;

/// The header (and `envoy.lb` metadata key) the Inference Extension defines for the pick.
pub const DESTINATION: &str = "x-gateway-destination-endpoint";

/// Routing headers only a picker may set. A client's copy is dropped on every path, so a caller
/// cannot choose the pod or the Dynamo worker itself, picker or not.
pub fn picker_owned(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n == DESTINATION || n.starts_with("x-dynamo-") || n.starts_with("x-gateway-")
}

/// Request headers never forwarded upstream (hop-by-hop, or recomputed).
const HOP_BY_HOP: &[&str] = &[
    "host",
    "content-length",
    "connection",
    "keep-alive",
    "proxy-connection",
    "transfer-encoding",
    "te",
    "trailer",
    "upgrade",
];

struct Pool {
    picker: Channel,
    fallback_url: String,
}

pub struct Bridge {
    pools: HashMap<String, Pool>,
    pick_timeout: Duration,
    http: reqwest::Client,
    metrics: Arc<Metrics>,
}

/// What the picker decided.
#[derive(Debug, PartialEq)]
pub enum Pick {
    /// Forward to this `ip:port`, with the picker's request mutations.
    Endpoint(String, Mutations),
    /// The picker answered the request itself (status, body).
    Immediate(u16, Vec<u8>),
}

impl Bridge {
    pub fn new(config: &PoolBridgeConfig, metrics: Arc<Metrics>) -> anyhow::Result<Self> {
        let mut pools = HashMap::new();
        for (name, pool) in &config.pools {
            let picker = Endpoint::from_shared(pool.epp.clone())?
                .connect_timeout(Duration::from_millis(config.pick_timeout_ms))
                .connect_lazy();
            pools.insert(
                name.clone(),
                Pool {
                    picker,
                    fallback_url: pool.fallback_url.trim_end_matches('/').to_string(),
                },
            );
        }
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .pool_idle_timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            pools,
            pick_timeout: Duration::from_millis(config.pick_timeout_ms),
            http,
            metrics,
        })
    }

    pub fn router(self: Arc<Self>, max_body_bytes: usize) -> Router {
        Router::new()
            .route("/pools/{pool}/{*rest}", any(forward))
            .route("/healthz", any(|| async { "ok" }))
            .layer(DefaultBodyLimit::max(max_body_bytes))
            .with_state(self)
    }
}

/// The picker exchange, kept open until the upstream response ends.
struct PickerStream {
    tx: mpsc::Sender<ProcessingRequest>,
}

impl PickerStream {
    /// Tells the picker the response started (status) and, on drop of the body, that it ended.
    fn response_started(&self, status: u16) {
        let headers = PbHeaderMap {
            headers: vec![header(":status", &status.to_string())],
        };
        let _ = self.tx.try_send(ProcessingRequest {
            request: Some(Req::ResponseHeaders(HttpHeaders {
                headers: Some(headers),
                end_of_stream: false,
                ..Default::default()
            })),
            ..Default::default()
        });
    }
}

impl Drop for PickerStream {
    fn drop(&mut self) {
        let _ = self.tx.try_send(ProcessingRequest {
            request: Some(Req::ResponseBody(HttpBody {
                body: Vec::new(),
                end_of_stream: true,
                ..Default::default()
            })),
            ..Default::default()
        });
    }
}

fn header(key: &str, value: &str) -> HeaderValue {
    HeaderValue {
        key: key.to_string(),
        value: value.to_string(),
        raw_value: value.as_bytes().to_vec(),
    }
}

/// Request changes a picker asked for: headers to set, and a replacement body.
#[derive(Debug, Default, PartialEq)]
pub struct Mutations {
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
}

fn common_of(response: &ProcessingResponse) -> Option<&CommonResponse> {
    match &response.response {
        Some(Resp::RequestHeaders(h)) => h.response.as_ref(),
        Some(Resp::RequestBody(b)) => b.response.as_ref(),
        _ => None,
    }
}

/// Adds the header and body mutations in one picker answer to `into`. Pseudo-headers and the
/// destination header itself are left out: the bridge addresses the pod directly.
pub fn collect_mutations(response: &ProcessingResponse, into: &mut Mutations) {
    let Some(common) = common_of(response) else {
        return;
    };
    if let Some(mutation) = &common.header_mutation {
        for option in &mutation.set_headers {
            let Some(h) = &option.header else { continue };
            let key = h.key.to_ascii_lowercase();
            if key.starts_with(':') || key == DESTINATION || HOP_BY_HOP.contains(&key.as_str()) {
                continue;
            }
            let value = if h.raw_value.is_empty() {
                h.value.clone()
            } else {
                String::from_utf8_lossy(&h.raw_value).into_owned()
            };
            into.headers.retain(|(k, _)| *k != key);
            into.headers.push((key, value));
        }
    }
    if let Some(BodyMut::Body(body)) = common
        .body_mutation
        .as_ref()
        .and_then(|b| b.mutation.as_ref())
    {
        into.body = Some(body.clone());
    }
}

/// The endpoint in a picker answer: the header mutation first, then `envoy.lb` metadata.
/// A comma-separated list means primary first, then fallbacks; the primary is used.
pub fn endpoint_of(response: &ProcessingResponse) -> Option<String> {
    let first = |v: &str| {
        v.split(',')
            .map(str::trim)
            .find(|s| !s.is_empty())
            .map(str::to_string)
    };
    if let Some(mutation) = common_of(response).and_then(|c| c.header_mutation.as_ref()) {
        for option in &mutation.set_headers {
            if let Some(h) = &option.header
                && h.key.eq_ignore_ascii_case(DESTINATION)
            {
                let value = if h.raw_value.is_empty() {
                    h.value.clone()
                } else {
                    String::from_utf8_lossy(&h.raw_value).into_owned()
                };
                if let Some(v) = first(&value) {
                    return Some(v);
                }
            }
        }
    }
    use envoy_types::pb::google::protobuf::value::Kind;
    let lb = response.dynamic_metadata.as_ref()?.fields.get("envoy.lb")?;
    if let Some(Kind::StructValue(s)) = &lb.kind
        && let Some(Kind::StringValue(v)) = s.fields.get(DESTINATION).and_then(|v| v.kind.as_ref())
    {
        return first(v);
    }
    None
}

/// Runs the picker exchange for one request. Returns the pick and the open stream.
async fn pick(
    picker: Channel,
    headers: Vec<HeaderValue>,
    body: Vec<u8>,
) -> Result<(Pick, Option<PickerStream>), String> {
    let (tx, rx) = mpsc::channel::<ProcessingRequest>(8);
    tx.try_send(ProcessingRequest {
        request: Some(Req::RequestHeaders(HttpHeaders {
            headers: Some(PbHeaderMap { headers }),
            end_of_stream: false,
            ..Default::default()
        })),
        ..Default::default()
    })
    .map_err(|e| e.to_string())?;
    tx.try_send(ProcessingRequest {
        request: Some(Req::RequestBody(HttpBody {
            body,
            end_of_stream: true,
            ..Default::default()
        })),
        ..Default::default()
    })
    .map_err(|e| e.to_string())?;
    let mut client = ExternalProcessorClient::new(picker);
    let mut inbound = client
        .process(ReceiverStream::new(rx))
        .await
        .map_err(|s| format!("{}: {}", s.code(), s.message()))?
        .into_inner();
    let mut mutations = Mutations::default();
    while let Some(message) = inbound.next().await {
        let message = message.map_err(|s| format!("{}: {}", s.code(), s.message()))?;
        if let Some(Resp::ImmediateResponse(immediate)) = &message.response {
            let code = immediate
                .status
                .as_ref()
                .map(|s| s.code as u16)
                .unwrap_or(503);
            return Ok((Pick::Immediate(code, immediate.body.clone()), None));
        }
        collect_mutations(&message, &mut mutations);
        if let Some(endpoint) = endpoint_of(&message) {
            // Drain the rest of the picker's answers in the background so its sends never block.
            tokio::spawn(async move { while inbound.next().await.is_some() {} });
            return Ok((
                Pick::Endpoint(endpoint, mutations),
                Some(PickerStream { tx }),
            ));
        }
    }
    Err("the picker closed the stream without choosing an endpoint".to_string())
}

fn error(status: StatusCode, code: &str, message: &str) -> Response {
    let body =
        serde_json::json!({"error": {"message": message, "type": "pool_bridge", "code": code}});
    (status, axum::Json(body)).into_response()
}

async fn forward(
    State(bridge): State<Arc<Bridge>>,
    Path((pool_name, rest)): Path<(String, String)>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(pool) = bridge.pools.get(&pool_name) else {
        return error(
            StatusCode::NOT_FOUND,
            "unknown_pool",
            "no such pool on this bridge",
        );
    };
    let path = match uri.query() {
        Some(q) => format!("/{rest}?{q}"),
        None => format!("/{rest}"),
    };
    let request_id = headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());

    // Only requests with a body carry a model for the picker to score; the rest (GET /v1/models)
    // go to the Service.
    let started = Instant::now();
    let mut set = Mutations::default();
    let (target, how, stream) = if method == Method::POST && !body.is_empty() {
        let mut pb = vec![
            header(":method", method.as_str()),
            header(":path", &path),
            header(":scheme", "http"),
            header(
                ":authority",
                headers
                    .get("host")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or(""),
            ),
            header("x-request-id", &request_id),
        ];
        for (name, value) in &headers {
            let n = name.as_str();
            if n == "host" || n == "x-request-id" || n == "authorization" || picker_owned(n) {
                continue;
            }
            if let Ok(v) = value.to_str() {
                pb.push(header(n, v));
            }
        }
        match tokio::time::timeout(
            bridge.pick_timeout,
            pick(pool.picker.clone(), pb, body.to_vec()),
        )
        .await
        {
            Ok(Ok((Pick::Endpoint(ep, m), stream))) => {
                set = m;
                (format!("http://{ep}"), "epp", stream)
            }
            Ok(Ok((Pick::Immediate(code, payload), _))) => {
                bridge
                    .metrics
                    .bridge_requests
                    .with_label_values(&[&pool_name, "epp_refused"])
                    .inc();
                tracing::info!(pool = %pool_name, status = code, %request_id, "pool bridge: picker answered the request");
                let status = StatusCode::from_u16(code).unwrap_or(StatusCode::SERVICE_UNAVAILABLE);
                let mut response = Response::new(Body::from(payload));
                *response.status_mut() = status;
                response
                    .headers_mut()
                    .insert("x-pool-pick", HttpHeaderValue::from_static("epp-refused"));
                return response;
            }
            Ok(Err(reason)) => {
                tracing::warn!(pool = %pool_name, %reason, %request_id, "pool bridge: picker failed, failing open to the Service");
                (pool.fallback_url.clone(), "fail-open", None)
            }
            Err(_) => {
                tracing::warn!(pool = %pool_name, timeout_ms = bridge.pick_timeout.as_millis() as u64, %request_id,
                    "pool bridge: picker timed out, failing open to the Service");
                (pool.fallback_url.clone(), "fail-open", None)
            }
        }
    } else {
        (pool.fallback_url.clone(), "service", None)
    };
    bridge
        .metrics
        .bridge_pick_seconds
        .with_label_values(&[&pool_name, how])
        .observe(started.elapsed().as_secs_f64());
    let pick_ms = started.elapsed().as_millis() as u64;

    // The picker's mutations apply only to the pod it picked, not to a fail-open retry.
    let send = |target: &str, picked: bool| {
        let body = match (&set.body, picked) {
            (Some(b), true) => Bytes::from(b.clone()),
            _ => body.clone(),
        };
        let mut upstream = bridge
            .http
            .request(method.clone(), format!("{target}{path}"))
            .body(body);
        for (name, value) in &headers {
            if !HOP_BY_HOP.contains(&name.as_str())
                && !picker_owned(name.as_str())
                && !(picked && set.headers.iter().any(|(k, _)| k == name.as_str()))
            {
                upstream = upstream.header(name, value);
            }
        }
        if picked {
            for (k, v) in &set.headers {
                upstream = upstream.header(k, v);
            }
        }
        upstream.header("x-request-id", &request_id).send()
    };
    let (mut target, mut how, mut stream) = (target, how, stream);
    let mut sent = send(&target, how == "epp").await;
    // A picked pod that cannot be reached (it just went away) fails open like the picker does.
    if let Err(err) = &sent
        && how == "epp"
        && err.is_connect()
    {
        tracing::warn!(pool = %pool_name, target = %target, %err, %request_id, "pool bridge: picked endpoint unreachable, failing open to the Service");
        (target, how, stream) = (pool.fallback_url.clone(), "fail-open", None);
        sent = send(&target, false).await;
    }
    let response = match sent {
        Ok(r) => r,
        Err(err) => {
            bridge
                .metrics
                .bridge_requests
                .with_label_values(&[&pool_name, "upstream_error"])
                .inc();
            tracing::warn!(pool = %pool_name, target = %target, %err, %request_id, "pool bridge: upstream failed");
            return error(
                StatusCode::BAD_GATEWAY,
                "upstream_unreachable",
                "the selected endpoint did not answer",
            );
        }
    };
    bridge
        .metrics
        .bridge_requests
        .with_label_values(&[&pool_name, how])
        .inc();
    let endpoint = target.trim_start_matches("http://").to_string();
    tracing::info!(pool = %pool_name, pick = how, endpoint = %endpoint, status = response.status().as_u16(),
        pick_ms, upstream_ms = started.elapsed().as_millis() as u64 - pick_ms, %request_id, "pool bridge forwarded");
    if let Some(s) = &stream {
        s.response_started(response.status().as_u16());
    }

    let status =
        StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut out = HeaderMap::new();
    for (name, value) in response.headers() {
        if HOP_BY_HOP.contains(&name.as_str()) {
            continue;
        }
        if let (Ok(n), Ok(v)) = (
            HeaderName::from_bytes(name.as_str().as_bytes()),
            HttpHeaderValue::from_bytes(value.as_bytes()),
        ) {
            out.append(n, v);
        }
    }
    out.insert("x-pool-pick", HttpHeaderValue::from_static(how));
    if let Ok(v) = HttpHeaderValue::from_str(&endpoint) {
        out.insert("x-pool-endpoint", v);
    }
    // The picker stream lives as long as the body, so its drop reports the end of the response.
    let body = response.bytes_stream().map(move |chunk| {
        let _keep = &stream;
        chunk
    });
    let mut reply = Response::new(Body::from_stream(body));
    *reply.status_mut() = status;
    *reply.headers_mut() = out;
    reply
}

/// Pools named in a bridge config, for logs.
pub fn describe(config: &PoolBridgeConfig) -> BTreeMap<String, String> {
    config
        .pools
        .iter()
        .map(|(name, p)| (name.clone(), p.epp.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use envoy_types::pb::envoy::config::core::v3::HeaderValueOption;
    use envoy_types::pb::envoy::service::ext_proc::v3::{
        BodyMutation, HeaderMutation, HeadersResponse,
    };
    use envoy_types::pb::google::protobuf::{Struct, Value, value::Kind};

    fn headers_response(value: &str) -> ProcessingResponse {
        ProcessingResponse {
            response: Some(Resp::RequestHeaders(HeadersResponse {
                response: Some(CommonResponse {
                    header_mutation: Some(HeaderMutation {
                        set_headers: vec![HeaderValueOption {
                            header: Some(HeaderValue {
                                key: DESTINATION.into(),
                                value: String::new(),
                                raw_value: value.as_bytes().to_vec(),
                            }),
                            ..Default::default()
                        }],
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
            })),
            ..Default::default()
        }
    }

    #[test]
    fn endpoint_from_header_mutation_takes_the_primary() {
        let r = headers_response("10.0.0.7:8000,10.0.0.8:8000");
        assert_eq!(endpoint_of(&r).as_deref(), Some("10.0.0.7:8000"));
    }

    #[test]
    fn endpoint_from_envoy_lb_metadata() {
        let inner = Struct {
            fields: [(
                DESTINATION.to_string(),
                Value {
                    kind: Some(Kind::StringValue("10.1.2.3:8000".into())),
                },
            )]
            .into(),
        };
        let r = ProcessingResponse {
            dynamic_metadata: Some(Struct {
                fields: [(
                    "envoy.lb".to_string(),
                    Value {
                        kind: Some(Kind::StructValue(inner)),
                    },
                )]
                .into(),
            }),
            ..Default::default()
        };
        assert_eq!(endpoint_of(&r).as_deref(), Some("10.1.2.3:8000"));
    }

    #[test]
    fn mutations_keep_picker_headers_and_body() {
        let mut r = headers_response("10.0.0.7:8000");
        if let Some(Resp::RequestHeaders(h)) = &mut r.response {
            let common = h.response.as_mut().unwrap();
            let set = &mut common.header_mutation.as_mut().unwrap().set_headers;
            for (k, v) in [
                ("x-dynamo-worker-instance-id", "7f3a"),
                (":authority", "x"),
                ("Host", "y"),
            ] {
                set.push(HeaderValueOption {
                    header: Some(HeaderValue {
                        key: k.into(),
                        value: v.into(),
                        raw_value: vec![],
                    }),
                    ..Default::default()
                });
            }
            common.body_mutation = Some(BodyMutation {
                mutation: Some(BodyMut::Body(b"{}".to_vec())),
            });
        }
        let mut m = Mutations::default();
        collect_mutations(&r, &mut m);
        assert_eq!(
            m.headers,
            vec![(
                "x-dynamo-worker-instance-id".to_string(),
                "7f3a".to_string()
            )]
        );
        assert_eq!(m.body.as_deref(), Some(&b"{}"[..]));
    }

    #[test]
    fn no_endpoint_in_a_plain_continue() {
        assert_eq!(endpoint_of(&ProcessingResponse::default()), None);
    }
}
