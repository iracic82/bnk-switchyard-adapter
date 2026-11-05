//! Drives `Algorithm::run_stream` to a routing outcome, then serves the answer.
//!
//! Routing-time work arrives as `Step::CallModel` (for example the task classifier's judge
//! call) and `Step::CallDecision`. Both are served here, so every model call an algorithm
//! makes goes through the same registry, policy checks and BNK route as the answer call.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::stream::{FuturesUnordered, StreamExt, TryStreamExt};
use futures::{FutureExt, future::BoxFuture};
use parking_lot::Mutex;
use serde_json::Value;
use switchyard_libsy::{CallDecision, CallModel, LibsyError, RoutingOutcome, State, Step};
use switchyard_protocol::{
    LlmClientError, LlmResponse, LlmResponseChunk, LlmResponseStream, ModelId, Request, Response,
    RoutedLlmClient, Usage,
};
use switchyard_translation::prepare_request_for_target;
use tracing::Instrument;

use crate::metrics::Metrics;
use crate::registry::{Plan, Registry};

/// A model call made while deciding, kept for cost and telemetry.
#[derive(Clone, Debug)]
pub struct RoutingCall {
    pub model: ModelId,
    pub kind: &'static str,
    pub ok: bool,
    pub duration: Duration,
    pub usage: Option<Usage>,
}

pub struct Decision {
    pub outcome: RoutingOutcome,
    pub algorithm: String,
    pub elapsed: Duration,
    pub calls: Vec<RoutingCall>,
    pub evidence: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct Fallback {
    pub from: ModelId,
    pub to: ModelId,
    pub reason: &'static str,
}

pub struct Answer {
    pub model: ModelId,
    pub response: Response,
    pub fallbacks: Vec<Fallback>,
    /// Time until the upstream returned a response (headers, for a stream).
    pub backend_elapsed: Duration,
    /// True when the algorithm produced the answer while routing.
    pub from_routing: bool,
    /// Span for the answer call. Kept open until a streamed answer finishes.
    pub span: tracing::Span,
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("routing did not finish within {0:?}")]
    RoutingTimeout(Duration),
    #[error("algorithm selected {0}, which this request may not use")]
    PolicyViolation(ModelId),
    #[error("routing failed: {0}")]
    Routing(LibsyError),
    #[error("upstream model {model} failed: {source}")]
    Upstream {
        model: ModelId,
        source: LlmClientError,
    },
}

pub struct Engine {
    pub registry: Arc<Registry>,
    pub metrics: Arc<Metrics>,
}

impl Engine {
    /// Runs the algorithm's `run_stream` lifecycle until `Step::Done`.
    pub async fn decide(
        &self,
        plan: &Plan,
        request: Request,
        timeout: Duration,
    ) -> Result<Decision, EngineError> {
        let started = Instant::now();
        let algorithm = plan.route.algorithm.name().to_string();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let result = tokio::time::timeout(timeout, self.drive(plan, request, &algorithm, &calls))
            .await
            .map_err(|_| EngineError::RoutingTimeout(timeout))
            .and_then(|r| r);
        let elapsed = started.elapsed();
        self.metrics
            .decision_seconds
            .with_label_values(&[plan.route.name.as_str(), algorithm.as_str()])
            .observe(elapsed.as_secs_f64());
        let calls = std::mem::take(&mut *calls.lock());
        for call in &calls {
            self.metrics
                .routing_calls
                .with_label_values(&[
                    algorithm.as_str(),
                    call.model.as_str(),
                    call.kind,
                    if call.ok { "ok" } else { "error" },
                ])
                .inc();
        }
        let outcome = result?;
        // Defense in depth: libsy's target restriction already limits candidates to the
        // plan's models; check again against the tenant's answer set.
        for model in &outcome.selected_model_ids {
            if !plan.answer_allowed.contains(model) {
                return Err(EngineError::PolicyViolation(model.clone()));
            }
        }
        let evidence = outcome
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.evidence.clone());
        Ok(Decision {
            outcome,
            algorithm,
            elapsed,
            calls,
            evidence,
        })
    }

    async fn drive(
        &self,
        plan: &Plan,
        request: Request,
        algorithm: &str,
        calls: &Arc<Mutex<Vec<RoutingCall>>>,
    ) -> Result<RoutingOutcome, EngineError> {
        let mut steps = Arc::clone(&plan.route.algorithm).run_stream(
            request,
            Arc::clone(&plan.models),
            State::default(),
        );
        // Calls run concurrently; each carries its own reply channel back to the algorithm.
        let mut in_flight: FuturesUnordered<BoxFuture<'static, switchyard_libsy::Result<()>>> =
            FuturesUnordered::new();
        loop {
            tokio::select! {
                Some(served) = in_flight.next() => {
                    served.map_err(EngineError::Routing)?;
                }
                step = steps.next() => match step {
                    None => return Err(EngineError::Routing(LibsyError::MissingFinalResponse)),
                    Some(Err(error)) => return Err(EngineError::Routing(error)),
                    Some(Ok(Step::CallModel(call))) => {
                        if let Some(model) = call.models.iter().find(|m| !plan.routing_allowed.contains(*m)) {
                            return Err(EngineError::PolicyViolation(model.clone()));
                        }
                        tracing::debug!(algorithm, model = %call.models[0], "serving routing model call");
                        in_flight.push(serve_model(*call, Arc::clone(&self.registry.client), Arc::clone(calls)).boxed());
                    }
                    Some(Ok(Step::CallDecision(call))) => {
                        in_flight.push(serve_decision(*call, Arc::clone(calls)).boxed());
                    }
                    Some(Ok(Step::Done(outcome))) => return Ok(*outcome),
                }
            }
        }
    }

    /// Calls the selected model, then the ordered fallbacks on retryable failures.
    pub async fn answer(&self, plan: &Plan, decision: Decision) -> Result<Answer, EngineError> {
        let outcome = decision.outcome;
        let selected = outcome
            .selected_model_id()
            .map_err(EngineError::Routing)?
            .clone();
        if let Some(response) = outcome.response {
            return Ok(Answer {
                model: response.served_model().cloned().unwrap_or(selected),
                response,
                fallbacks: Vec::new(),
                backend_elapsed: Duration::ZERO,
                from_routing: true,
                span: tracing::Span::none(),
            });
        }
        let candidates = &outcome.selected_model_ids;
        let mut fallbacks = Vec::new();
        for (index, target) in candidates.iter().enumerate() {
            if !plan.answer_allowed.contains(target) {
                return Err(EngineError::PolicyViolation(target.clone()));
            }
            let mut request = outcome
                .candidate_requests
                .get(target)
                .unwrap_or(match &outcome.fallback_request {
                    Some(fallback) if index > 0 => fallback,
                    _ => &outcome.request,
                })
                .clone();
            prepare_request_for_target(&mut request.llm_request, target, None);
            let span = tracing::info_span!(
                "inference_call",
                otel.kind = "client",
                candidate = index + 1,
                selected_model = %target,
                provider_model = self.registry.provider_model(target),
                inference_pool = self.registry.pool(target),
                backend_ms = tracing::field::Empty,
                ttft_ms = tracing::field::Empty,
                e2e_ms = tracing::field::Empty,
                input_tokens = tracing::field::Empty,
                output_tokens = tracing::field::Empty,
                inference_cost_usd = tracing::field::Empty,
                outcome = tracing::field::Empty,
            );
            let started = Instant::now();
            let result = self
                .registry
                .client
                .call(request)
                .instrument(span.clone())
                .await;
            let backend_elapsed = started.elapsed();
            span.record("backend_ms", backend_elapsed.as_millis() as u64);
            self.metrics
                .backend_seconds
                .with_label_values(&[target.as_str(), self.registry.pool(target)])
                .observe(backend_elapsed.as_secs_f64());
            match result {
                Ok(mut response) => {
                    response.set_served_model(target);
                    return Ok(Answer {
                        model: target.clone(),
                        response,
                        fallbacks,
                        backend_elapsed,
                        from_routing: false,
                        span,
                    });
                }
                Err(error) => {
                    span.record("outcome", "error");
                    let next = candidates.get(index + 1);
                    match (next, fallback_reason(&error)) {
                        (Some(next), Some(reason)) => {
                            tracing::warn!(from = %target, to = %next, reason, "answer call failed; trying next candidate");
                            self.metrics
                                .fallbacks
                                .with_label_values(&[
                                    plan.route.name.as_str(),
                                    target.as_str(),
                                    next.as_str(),
                                    reason,
                                ])
                                .inc();
                            fallbacks.push(Fallback {
                                from: target.clone(),
                                to: next.clone(),
                                reason,
                            });
                        }
                        _ => {
                            return Err(EngineError::Upstream {
                                model: target.clone(),
                                source: error,
                            });
                        }
                    }
                }
            }
        }
        Err(EngineError::Routing(LibsyError::NoTargets))
    }
}

/// Which failures are worth sending to the next candidate. Client errors (4xx other than
/// 408/429) are not: the next model would most likely refuse the same request.
pub fn fallback_reason(error: &LlmClientError) -> Option<&'static str> {
    match error {
        LlmClientError::ContextWindowExceeded { .. } => Some("context_window"),
        LlmClientError::Transport { .. } => Some("transport"),
        LlmClientError::TemporarilyUnavailable => Some("unavailable"),
        LlmClientError::Timeout { .. } => Some("timeout"),
        LlmClientError::UpstreamHttp { status, .. }
            if status.is_server_error()
                || *status == http::StatusCode::TOO_MANY_REQUESTS
                || *status == http::StatusCode::REQUEST_TIMEOUT =>
        {
            Some("upstream_status")
        }
        _ => None,
    }
}

async fn serve_model(
    call: CallModel,
    client: Arc<dyn RoutedLlmClient>,
    calls: Arc<Mutex<Vec<RoutingCall>>>,
) -> switchyard_libsy::Result<()> {
    let target = call.models[0].clone();
    let mut request = call.request.clone();
    prepare_request_for_target(&mut request.llm_request, &target, None);
    call.respond(async move {
        let started = Instant::now();
        let result = async {
            let mut response = client.call(request).await?;
            let mut usage = response.llm_response.as_agg().map(|agg| agg.usage.clone());
            // Routing calls are buffered so their usage is known; the replayed stream keeps
            // every event, in case the algorithm returns this response as the answer.
            if let LlmResponse::Stream(stream) = response.llm_response {
                let (replay, streamed_usage) = buffer_stream(stream).await?;
                usage = streamed_usage;
                response.llm_response = LlmResponse::Stream(replay);
            }
            response.set_served_model(&target);
            Ok::<_, LlmClientError>((response, usage))
        }
        .await;
        let duration = started.elapsed();
        let (result, usage) = match result {
            Ok((response, usage)) => (Ok(response), usage),
            Err(error) => (Err(error), None),
        };
        calls.lock().push(RoutingCall {
            model: target.clone(),
            kind: "model",
            ok: result.is_ok(),
            duration,
            usage,
        });
        result.map_err(|error| LibsyError::client_call(target, error))
    })
    .await
}

/// No decision service is configured. The algorithm receives an error and applies its own
/// fallback policy (the task classifier fails open to its capable tier).
async fn serve_decision(
    call: CallDecision,
    calls: Arc<Mutex<Vec<RoutingCall>>>,
) -> switchyard_libsy::Result<()> {
    let model = call.model.clone();
    calls.lock().push(RoutingCall {
        model: model.clone(),
        kind: "decision",
        ok: false,
        duration: Duration::ZERO,
        usage: None,
    });
    call.respond(std::future::ready(Err(LibsyError::client_call(
        model,
        LlmClientError::Configuration {
            message: "this adapter has no decision service configured".into(),
        },
    ))))
    .await
}

async fn buffer_stream(
    mut stream: LlmResponseStream,
) -> Result<(LlmResponseStream, Option<Usage>), LlmClientError> {
    let mut events = Vec::new();
    let mut usage = None;
    while let Some(event) = stream.try_next().await? {
        for chunk in event.normalized() {
            match chunk {
                LlmResponseChunk::Usage(latest) => usage = Some(latest.clone()),
                LlmResponseChunk::StreamError { message } => {
                    return Err(LlmClientError::UpstreamHttp {
                        status: http::StatusCode::BAD_GATEWAY,
                        body: message.clone(),
                    });
                }
                LlmResponseChunk::DecodeError { message } => {
                    return Err(LlmClientError::ResponseTranslation(message.clone()));
                }
                _ => {}
            }
        }
        events.push(event);
    }
    Ok((
        futures::stream::iter(events.into_iter().map(Ok)).boxed(),
        usage,
    ))
}

/// The models a routing outcome may use, for logs.
pub fn model_list(models: &HashSet<ModelId>) -> String {
    let mut names: Vec<&str> = models.iter().map(ModelId::as_str).collect();
    names.sort();
    names.join(",")
}
