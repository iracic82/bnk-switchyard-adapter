//! Prometheus metrics. Every label value comes from configuration (tenant, route, model,
//! pool), never from request content, so cardinality is bounded.
//!
//! Only what the adapter observes is exported. GPU, KV-cache and EPP numbers come from
//! the model servers and the F5 EPP, never from here.

use prometheus::{
    Encoder, HistogramOpts, HistogramVec, IntCounterVec, IntGauge, Opts, Registry, TextEncoder,
};

pub struct Metrics {
    registry: Registry,
    pub requests: IntCounterVec,
    pub rejections: IntCounterVec,
    pub decision_seconds: HistogramVec,
    pub routing_calls: IntCounterVec,
    pub backend_seconds: HistogramVec,
    pub ttft_seconds: HistogramVec,
    pub request_seconds: HistogramVec,
    pub tokens: IntCounterVec,
    pub cost_usd: prometheus::CounterVec,
    pub fallbacks: IntCounterVec,
    pub inflight: IntGauge,
}

const LATENCY_BUCKETS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0,
];

impl Metrics {
    pub fn new() -> Self {
        let registry = Registry::new();
        macro_rules! reg {
            ($metric:expr) => {{
                let metric = $metric;
                registry
                    .register(Box::new(metric.clone()))
                    .expect("metric names are unique");
                metric
            }};
        }
        let histogram = |name: &str, help: &str, labels: &[&str]| {
            HistogramVec::new(
                HistogramOpts::new(name, help).buckets(LATENCY_BUCKETS.to_vec()),
                labels,
            )
            .expect("valid histogram")
        };
        let counter = |name: &str, help: &str, labels: &[&str]| {
            IntCounterVec::new(Opts::new(name, help), labels).expect("valid counter")
        };
        Self {
            requests: reg!(counter(
                "sy_adapter_requests_total",
                "Chat completion requests by outcome",
                &["tenant", "route", "algorithm", "model", "pool", "status"]
            )),
            rejections: reg!(counter(
                "sy_adapter_rejections_total",
                "Requests refused by policy before routing",
                &["tenant", "reason"]
            )),
            decision_seconds: reg!(histogram(
                "sy_adapter_decision_seconds",
                "Time from request start to the Switchyard routing outcome",
                &["route", "algorithm"]
            )),
            routing_calls: reg!(counter(
                "sy_adapter_routing_calls_total",
                "Model and decision calls made by algorithms while deciding",
                &["algorithm", "model", "kind", "outcome"]
            )),
            backend_seconds: reg!(histogram(
                "sy_adapter_backend_seconds",
                "Answer call: time until the upstream response (headers for streams)",
                &["model", "pool"]
            )),
            ttft_seconds: reg!(histogram(
                "sy_adapter_ttft_seconds",
                "Streaming only: time from request start to the first content chunk",
                &["model", "pool"]
            )),
            request_seconds: reg!(histogram(
                "sy_adapter_request_seconds",
                "End-to-end request time as seen by the adapter",
                &["route", "model"]
            )),
            tokens: reg!(counter(
                "sy_adapter_tokens_total",
                "Tokens reported by the upstream usage block",
                &["tenant", "model", "phase", "kind"]
            )),
            cost_usd: reg!(
                prometheus::CounterVec::new(
                    Opts::new(
                        "sy_adapter_estimated_cost_usd_total",
                        "Estimated cost from configured token prices"
                    ),
                    &["tenant", "model", "phase"]
                )
                .expect("valid counter")
            ),
            fallbacks: reg!(counter(
                "sy_adapter_fallbacks_total",
                "Answer calls that moved to the next candidate",
                &["route", "from", "to", "reason"]
            )),
            inflight: reg!(
                IntGauge::new("sy_adapter_inflight_requests", "Requests in progress")
                    .expect("valid gauge")
            ),
            registry,
        }
    }

    pub fn render(&self) -> String {
        let mut buffer = Vec::new();
        let _ = TextEncoder::new().encode(&self.registry.gather(), &mut buffer);
        String::from_utf8(buffer).unwrap_or_default()
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}
