//! In-process tests: the adapter against scripted upstreams that record what they receive.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use bnk_switchyard_adapter::config::Config;
use parking_lot::Mutex;
use serde_json::{Value, json};

#[derive(Clone, Copy)]
enum Behaviour {
    Ok,
    Status503,
    BadRequestJson,
    Slow,
}

#[derive(Default)]
struct Seen {
    bodies: Vec<Value>,
    headers: Vec<HeaderMap>,
}

struct Upstream {
    addr: SocketAddr,
    seen: Arc<Mutex<Seen>>,
}

async fn upstream(model: &'static str, behaviour: Behaviour) -> Upstream {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let state = (Arc::clone(&seen), model, behaviour);
    let app = axum::Router::new()
        .route(
            "/v1/chat/completions",
            post(
                |State((seen, model, behaviour)): State<(
                    Arc<Mutex<Seen>>,
                    &'static str,
                    Behaviour,
                )>,
                 headers: HeaderMap,
                 Json(body): Json<Value>| async move {
                    {
                        let mut seen = seen.lock();
                        seen.bodies.push(body.clone());
                        seen.headers.push(headers);
                    }
                    respond(model, behaviour, &body).await
                },
            ),
        )
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Upstream { addr, seen }
}

async fn respond(model: &str, behaviour: Behaviour, body: &Value) -> Response {
    match behaviour {
        Behaviour::Status503 => {
            return (StatusCode::SERVICE_UNAVAILABLE, "overloaded").into_response();
        }
        Behaviour::BadRequestJson => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": {"message": "bad temperature", "type": "BadRequestError", "code": 400}})),
            )
                .into_response();
        }
        Behaviour::Slow => tokio::time::sleep(Duration::from_secs(5)).await,
        Behaviour::Ok => {}
    }
    let content = if body
        .pointer("/response_format/json_schema/name")
        .and_then(Value::as_str)
        == Some("CapabilityClassifierDecision")
    {
        json!({"crux": "x", "primary_rule": "SUP-1", "capability_boundary": "supported", "p_solve": 0.95})
            .to_string()
    } else {
        format!("answer from {model}")
    };
    Json(json!({
        "id": "chatcmpl-1", "object": "chat.completion", "created": 1, "model": model,
        "choices": [{"index": 0, "message": {"role": "assistant", "content": content}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
    }))
    .into_response()
}

const KEY: &str = "sk-test";

fn key_digest() -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(KEY.as_bytes()))
}

fn config(small: SocketAddr, large: SocketAddr, extra: &str) -> Config {
    Config::parse(&format!(
        r#"
[server]
listen = "127.0.0.1:0"
routing_timeout_secs = 2
request_timeout_secs = 3

[identity]
mode = "api_key"

[[tenants]]
id = "t1"
api_key_sha256 = ["{digest}"]
allowed_models = ["small", "large"]

[models.small]
provider_model = "prov-small"
base_url = "http://{small}/v1"
headers = {{ "x-switchyard-pool" = "small" }}
inference_pool = "ns/small-pool"
max_retries = 0
timeout_secs = 2
cost = {{ input_per_1m_usd = 1.0, output_per_1m_usd = 2.0 }}

[models.large]
provider_model = "prov-large"
base_url = "http://{large}/v1"
headers = {{ "x-switchyard-pool" = "large" }}
inference_pool = "ns/large-pool"
max_retries = 0
timeout_secs = 2

[routes.fallback]
algorithm = "passthrough"
models = ["large", "small"]

[routes.auto]
algorithm = "task_classifier"
efficient = ["small"]
capable = ["large"]
judge = "small"
classifier = {{ base_threshold = 0.6 }}
{extra}
"#,
        digest = key_digest()
    ))
    .unwrap()
}

async fn adapter(config: Config) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        bnk_switchyard_adapter::serve(&config, listener, std::future::pending())
            .await
            .unwrap()
    });
    addr
}

async fn send(addr: SocketAddr, body: Value) -> (StatusCode, HeaderMap, Value) {
    let response = reqwest::Client::new()
        .post(format!("http://{addr}/v1/chat/completions"))
        .bearer_auth(KEY)
        .header("x-request-id", "rid-42")
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = StatusCode::from_u16(response.status().as_u16()).unwrap();
    let headers = response.headers().clone();
    let body = response.json().await.unwrap_or(Value::Null);
    (status, headers, body)
}

fn hello(model: &str) -> Value {
    json!({"model": model, "messages": [{"role": "user", "content": "hello"}], "temperature": 0.3, "seed": 7, "logit_bias": {"5": 1}})
}

#[tokio::test]
async fn upstream_sees_provider_model_pool_header_and_no_client_credentials() {
    let small = upstream("prov-small", Behaviour::Ok).await;
    let large = upstream("prov-large", Behaviour::Ok).await;
    let addr = adapter(config(small.addr, large.addr, "")).await;
    let (status, headers, body) = send(addr, hello("small")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers["x-switchyard-inference-pool"], "ns/small-pool");
    assert_eq!(body["model"], "prov-small");
    let seen = small.seen.lock();
    let upstream_body = &seen.bodies[0];
    assert_eq!(upstream_body["model"], "prov-small");
    // OpenAI parameters arrive unchanged.
    assert_eq!(upstream_body["temperature"], 0.3);
    assert_eq!(upstream_body["seed"], 7);
    assert_eq!(upstream_body["logit_bias"], json!({"5": 1}));
    let upstream_headers = &seen.headers[0];
    assert_eq!(upstream_headers["x-switchyard-pool"], "small");
    assert_eq!(upstream_headers["x-switchyard-adapter-hop"], "1");
    assert_eq!(upstream_headers["x-request-id"], "rid-42");
    assert!(
        upstream_headers.get("authorization").is_none(),
        "the tenant's API key must not reach the inference pool"
    );
    assert!(large.seen.lock().bodies.is_empty());
}

#[tokio::test]
async fn retryable_failure_falls_back_to_next_candidate() {
    let small = upstream("prov-small", Behaviour::Ok).await;
    let large = upstream("prov-large", Behaviour::Status503).await;
    let addr = adapter(config(small.addr, large.addr, "")).await;
    let (status, headers, body) = send(addr, hello("fallback")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers["x-switchyard-model"], "small");
    assert_eq!(
        headers["x-switchyard-fallbacks"],
        "large>small:upstream_status"
    );
    assert_eq!(large.seen.lock().bodies.len(), 1);
}

#[tokio::test]
async fn client_error_is_passed_through_without_fallback() {
    let small = upstream("prov-small", Behaviour::Ok).await;
    let large = upstream("prov-large", Behaviour::BadRequestJson).await;
    let addr = adapter(config(small.addr, large.addr, "")).await;
    let (status, _, body) = send(addr, hello("fallback")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["message"], "bad temperature");
    assert!(
        small.seen.lock().bodies.is_empty(),
        "4xx must not fall back"
    );
}

#[tokio::test]
async fn classifier_judge_call_is_served_and_counted_as_a_routing_call() {
    let small = upstream("prov-small", Behaviour::Ok).await;
    let large = upstream("prov-large", Behaviour::Ok).await;
    let addr = adapter(config(small.addr, large.addr, "")).await;
    let (status, headers, _) = send(addr, hello("auto")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["x-switchyard-model"], "small");
    let seen = small.seen.lock();
    assert_eq!(seen.bodies.len(), 2, "one judge call, one answer call");
    assert_eq!(
        seen.bodies[0]["response_format"]["json_schema"]["name"],
        "CapabilityClassifierDecision"
    );
}

#[tokio::test]
async fn judge_failure_fails_open_to_capable() {
    let small = upstream("prov-small", Behaviour::Status503).await;
    let large = upstream("prov-large", Behaviour::Ok).await;
    let addr = adapter(config(small.addr, large.addr, "")).await;
    let (status, headers, body) = send(addr, hello("auto")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers["x-switchyard-model"], "large");
}

#[tokio::test]
async fn slow_routing_times_out_with_504() {
    let small = upstream("prov-small", Behaviour::Slow).await;
    let large = upstream("prov-large", Behaviour::Ok).await;
    let mut config = config(small.addr, large.addr, "");
    // The judge's own timeout is longer than the routing deadline.
    config.server.routing_timeout_secs = 1;
    let addr = adapter(config).await;
    let started = std::time::Instant::now();
    let (status, _, body) = send(addr, hello("auto")).await;
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT, "{body}");
    assert_eq!(body["error"]["code"], "routing_timeout");
}

mod ext_proc {
    use super::*;
    use envoy_types::pb::envoy::config::core::v3::{HeaderMap as EnvoyHeaders, HeaderValue};
    use envoy_types::pb::envoy::service::ext_proc::v3::external_processor_client::ExternalProcessorClient;
    use envoy_types::pb::envoy::service::ext_proc::v3::processing_request::Request as Req;
    use envoy_types::pb::envoy::service::ext_proc::v3::processing_response::Response as Resp;
    use envoy_types::pb::envoy::service::ext_proc::v3::{
        HttpBody, HttpHeaders, ProcessingRequest, body_mutation,
    };

    async fn start(small: SocketAddr, large: SocketAddr) -> u16 {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let extra = format!("[ext_proc]\nlisten = \"127.0.0.1:{port}\"\ntenant = \"t1\"\n");
        let mut text_config = config(small, large, &extra);
        for model in text_config.models.values_mut() {
            model.gateway_model = Some(format!("gw-{}", model.provider_model));
        }
        let _ = adapter(text_config).await;
        for _ in 0..50 {
            if tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_ok()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        port
    }

    /// Plays the gateway: sends request headers, then the buffered body.
    async fn exchange(port: u16, body: Value) -> Vec<Resp> {
        let mut client = ExternalProcessorClient::connect(format!("http://127.0.0.1:{port}"))
            .await
            .unwrap();
        let headers = HttpHeaders {
            headers: Some(EnvoyHeaders {
                headers: vec![HeaderValue {
                    key: "x-request-id".into(),
                    raw_value: b"ep-1".to_vec(),
                    ..Default::default()
                }],
            }),
            end_of_stream: false,
            ..Default::default()
        };
        let messages = vec![
            ProcessingRequest {
                request: Some(Req::RequestHeaders(headers)),
                ..Default::default()
            },
            ProcessingRequest {
                request: Some(Req::RequestBody(HttpBody {
                    body: serde_json::to_vec(&body).unwrap(),
                    end_of_stream: true,
                    ..Default::default()
                })),
                ..Default::default()
            },
        ];
        let mut inbound = client
            .process(futures::stream::iter(messages))
            .await
            .unwrap()
            .into_inner();
        let mut out = Vec::new();
        while let Some(message) = inbound.message().await.unwrap() {
            out.push(message.response.unwrap());
            if out.len() == 2 {
                break;
            }
        }
        out
    }

    fn header<'a>(
        common: &'a envoy_types::pb::envoy::service::ext_proc::v3::CommonResponse,
        key: &str,
    ) -> Option<&'a [u8]> {
        common
            .header_mutation
            .as_ref()?
            .set_headers
            .iter()
            .find_map(|h| {
                let h = h.header.as_ref()?;
                (h.key == key).then_some(h.raw_value.as_slice())
            })
    }

    #[tokio::test]
    async fn auto_route_is_decided_and_rewritten_for_the_gateway() {
        let small = upstream("prov-small", Behaviour::Ok).await;
        let large = upstream("prov-large", Behaviour::Ok).await;
        let port = start(small.addr, large.addr).await;
        let responses = exchange(port, hello("auto")).await;
        assert!(matches!(responses[0], Resp::RequestHeaders(_)));
        let Resp::RequestBody(body) = &responses[1] else {
            panic!("expected a body response")
        };
        let common = body.response.as_ref().expect("a mutation");
        assert!(
            common.clear_route_cache,
            "the gateway must re-match routes on the new model"
        );
        assert_eq!(
            header(common, "x-selected-model"),
            Some(&b"gw-prov-small"[..])
        );
        assert_eq!(header(common, "x-switchyard-model"), Some(&b"small"[..]));
        assert_eq!(header(common, "x-request-id"), Some(&b"ep-1"[..]));
        let Some(body_mutation::Mutation::Body(rewritten)) = common
            .body_mutation
            .as_ref()
            .and_then(|m| m.mutation.clone())
        else {
            panic!("expected a body mutation")
        };
        let rewritten: Value = serde_json::from_slice(&rewritten).unwrap();
        assert_eq!(rewritten["model"], "gw-prov-small");
        assert_eq!(rewritten["temperature"], 0.3, "other fields are kept");
        assert!(
            common
                .header_mutation
                .as_ref()
                .unwrap()
                .remove_headers
                .contains(&"content-length".to_string())
        );
        // The judge call is the only upstream call: the gateway sends the answer itself.
        assert_eq!(small.seen.lock().bodies.len(), 1);
        assert!(large.seen.lock().bodies.is_empty());
    }

    #[tokio::test]
    async fn concrete_model_names_are_labelled_not_decided() {
        let small = upstream("prov-small", Behaviour::Ok).await;
        let large = upstream("prov-large", Behaviour::Ok).await;
        let port = start(small.addr, large.addr).await;
        let responses = exchange(port, hello("large")).await;
        let Resp::RequestBody(body) = &responses[1] else {
            panic!("expected a body response")
        };
        let common = body.response.as_ref().expect("a label");
        assert_eq!(
            header(common, "x-selected-model"),
            Some(&b"gw-prov-large"[..])
        );
        assert!(
            small.seen.lock().bodies.is_empty(),
            "no judge call for a concrete model"
        );
        // Unknown names pass through untouched.
        let responses = exchange(port, hello("something-else")).await;
        let Resp::RequestBody(body) = &responses[1] else {
            panic!("expected a body response")
        };
        assert!(body.response.is_none());
    }

    #[tokio::test]
    async fn judge_failure_still_routes_to_the_capable_model() {
        let small = upstream("prov-small", Behaviour::Status503).await;
        let large = upstream("prov-large", Behaviour::Ok).await;
        let port = start(small.addr, large.addr).await;
        let responses = exchange(port, hello("auto")).await;
        let Resp::RequestBody(body) = &responses[1] else {
            panic!("expected a body response")
        };
        let common = body.response.as_ref().expect("a mutation");
        assert_eq!(header(common, "x-switchyard-model"), Some(&b"large"[..]));
    }
}
