//! Pool bridge against a stand-in endpoint picker that speaks the Inference Extension's
//! ext_proc contract, a stand-in model pod and a stand-in Service.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::Router;
use axum::routing::post;
use bnk_switchyard_adapter::config::{BridgePool, PoolBridgeConfig};
use bnk_switchyard_adapter::metrics::Metrics;
use bnk_switchyard_adapter::pool_bridge::{Bridge, DESTINATION};
use envoy_types::pb::envoy::config::core::v3::{HeaderValue, HeaderValueOption};
use envoy_types::pb::envoy::service::ext_proc::v3::external_processor_server::{
    ExternalProcessor, ExternalProcessorServer,
};
use envoy_types::pb::envoy::service::ext_proc::v3::processing_request::Request as Req;
use envoy_types::pb::envoy::service::ext_proc::v3::processing_response::Response as Resp;
use envoy_types::pb::envoy::service::ext_proc::v3::{
    CommonResponse, HeaderMutation, HeadersResponse, ImmediateResponse, ProcessingRequest,
    ProcessingResponse,
};
use envoy_types::pb::envoy::r#type::v3::HttpStatus;
use futures::{Stream, StreamExt};
use tokio::net::TcpListener;
use tonic::{Request, Response, Status, Streaming};

#[derive(Clone)]
struct Picker {
    endpoint: Option<String>,
    saw_model: Arc<AtomicBool>,
    saw_end: Arc<AtomicBool>,
}

type Out = Pin<Box<dyn Stream<Item = Result<ProcessingResponse, Status>> + Send>>;

#[tonic::async_trait]
impl ExternalProcessor for Picker {
    type ProcessStream = Out;
    async fn process(
        &self,
        req: Request<Streaming<ProcessingRequest>>,
    ) -> Result<Response<Out>, Status> {
        let mut inbound = req.into_inner();
        let me = self.clone();
        let out = async_stream::stream! {
            while let Some(Ok(msg)) = inbound.next().await {
                match msg.request {
                    Some(Req::RequestBody(b)) if b.end_of_stream => {
                        let body: serde_json::Value = serde_json::from_slice(&b.body).unwrap_or_default();
                        me.saw_model.store(body["model"] == "Qwen/Qwen3-0.6B", Ordering::SeqCst);
                        let response = match &me.endpoint {
                            Some(ep) => Resp::RequestHeaders(HeadersResponse { response: Some(CommonResponse {
                                header_mutation: Some(HeaderMutation { set_headers: vec![
                                    HeaderValueOption {
                                        header: Some(HeaderValue { key: DESTINATION.into(), value: String::new(), raw_value: ep.clone().into_bytes() }),
                                        ..Default::default() },
                                    // as Dynamo's picker names the worker for the frontend sidecar
                                    HeaderValueOption {
                                        header: Some(HeaderValue { key: "x-dynamo-worker-instance-id".into(), value: "w-1".into(), raw_value: vec![] }),
                                        ..Default::default() },
                                ], ..Default::default() }),
                                ..Default::default() }) }),
                            None => Resp::ImmediateResponse(ImmediateResponse {
                                status: Some(HttpStatus { code: 429 }), body: b"saturated".to_vec(), ..Default::default() }),
                        };
                        yield Ok(ProcessingResponse { response: Some(response), ..Default::default() });
                    }
                    Some(Req::ResponseBody(b)) if b.end_of_stream => me.saw_end.store(true, Ordering::SeqCst),
                    _ => {}
                }
            }
        };
        Ok(Response::new(Box::pin(out)))
    }
}

async fn http_server(name: &'static str) -> SocketAddr {
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |h: axum::http::HeaderMap| async move {
            let worker = h
                .get("x-dynamo-worker-instance-id")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("none")
                .to_string();
            (
                [("x-served-by", name.to_string()), ("x-worker-seen", worker)],
                format!("{{\"served\":\"{name}\"}}"),
            )
        }),
    );
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    addr
}

async fn picker(endpoint: Option<String>) -> (SocketAddr, Picker) {
    let p = Picker {
        endpoint,
        saw_model: Default::default(),
        saw_end: Default::default(),
    };
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let svc = ExternalProcessorServer::new(p.clone());
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(svc)
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(l))
            .await
            .unwrap()
    });
    (addr, p)
}

async fn bridge(epp: String, fallback: SocketAddr) -> SocketAddr {
    let cfg = PoolBridgeConfig {
        listen: "127.0.0.1:0".into(),
        pick_timeout_ms: 500,
        pools: BTreeMap::from([(
            "small".to_string(),
            BridgePool {
                epp,
                fallback_url: format!("http://{fallback}"),
                default_max_tokens: None,
            },
        )]),
    };
    let app = Arc::new(Bridge::new(&cfg, Arc::new(Metrics::new())).unwrap()).router(1 << 20);
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    addr
}

async fn ask(bridge: SocketAddr) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("http://{bridge}/pools/small/v1/chat/completions"))
        .header("content-type", "application/json")
        // a caller trying to choose the worker itself: must never reach a pod
        .header("x-dynamo-worker-instance-id", "client-choice")
        .body(r#"{"model":"Qwen/Qwen3-0.6B","messages":[{"role":"user","content":"hi"}]}"#)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn forwards_to_the_pod_the_picker_chose() {
    let pod = http_server("pod").await;
    let svc = http_server("service").await;
    let (epp, p) = picker(Some(pod.to_string())).await;
    let r = ask(bridge(format!("http://{epp}"), svc).await).await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["x-pool-pick"], "epp");
    assert_eq!(r.headers()["x-pool-endpoint"], pod.to_string().as_str());
    assert_eq!(r.headers()["x-served-by"], "pod");
    assert_eq!(
        r.headers()["x-worker-seen"],
        "w-1",
        "the picker's header reached the pod"
    );
    assert_eq!(r.text().await.unwrap(), r#"{"served":"pod"}"#);
    assert!(
        p.saw_model.load(Ordering::SeqCst),
        "the picker got the request body"
    );
    for _ in 0..50 {
        if p.saw_end.load(Ordering::SeqCst) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the picker never saw the end of the response");
}

#[tokio::test]
async fn fails_open_to_the_service_when_the_picker_is_down() {
    let svc = http_server("service").await;
    let dead = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap();
    let r = ask(bridge(format!("http://{dead}"), svc).await).await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["x-pool-pick"], "fail-open");
    assert_eq!(r.text().await.unwrap(), r#"{"served":"service"}"#);
}

#[tokio::test]
async fn returns_the_pickers_own_refusal() {
    let svc = http_server("service").await;
    let (epp, _) = picker(None).await;
    let r = ask(bridge(format!("http://{epp}"), svc).await).await;
    assert_eq!(r.status(), 429);
    assert_eq!(r.headers()["x-pool-pick"], "epp-refused");
}

#[tokio::test]
async fn fails_open_when_the_picked_pod_is_gone() {
    let svc = http_server("service").await;
    let gone = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap();
    let (epp, _) = picker(Some(gone.to_string())).await;
    let r = ask(bridge(format!("http://{epp}"), svc).await).await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["x-pool-pick"], "fail-open");
    assert_eq!(
        r.headers()["x-worker-seen"],
        "none",
        "no worker header on a fail-open retry"
    );
    assert_eq!(r.text().await.unwrap(), r#"{"served":"service"}"#);
}
