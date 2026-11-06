//! OpenAI-compatible mock model server. It is not a model.
//!
//! - Answers `POST /v1/chat/completions` (buffered and SSE), including tool calls.
//! - Acts as Switchyard's capability-classifier judge when the request asks for the
//!   `CapabilityClassifierDecision` JSON schema. The verdict comes from a keyword heuristic
//!   with configurable error, not from understanding the task.
//! - Simulates task success for benchmark prompts tagged `[task:<id>:easy|hard]`, using
//!   fixed per-profile probabilities. This is a cost/routing simulation, not a quality claim.
//! - Simulates queueing with a fixed number of slots and exports vLLM-named gauges, so an
//!   endpoint picker has load signals to read. The KV-cache gauge is slots in use, not memory.
//!
//! Configuration is by environment variable; see `Settings::from_env`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use rand::RngExt;
use serde_json::{Value, json};
use tokio::sync::Semaphore;

struct Settings {
    model: String,
    pod: String,
    profile: String,
    ttft_ms: u64,
    tokens_per_sec: f64,
    slots: usize,
    fail_rate: f64,
    judge_error_rate: f64,
    success_easy: f64,
    success_hard: f64,
}

fn env<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

impl Settings {
    fn from_env() -> Self {
        let profile: String = env("MOCK_PROFILE", "small".to_string());
        let large = profile == "large";
        Self {
            model: env("MOCK_MODEL", "mock-small".to_string()),
            pod: std::env::var("MOCK_POD")
                .or_else(|_| std::env::var("HOSTNAME"))
                .unwrap_or_else(|_| "mock".into()),
            ttft_ms: env("MOCK_TTFT_MS", if large { 400 } else { 120 }),
            tokens_per_sec: env("MOCK_TOKENS_PER_SEC", if large { 40.0 } else { 120.0 }),
            slots: env("MOCK_SLOTS", 8),
            fail_rate: env("MOCK_FAIL_RATE", 0.0),
            judge_error_rate: env("MOCK_JUDGE_ERROR_RATE", 0.1),
            success_easy: env("MOCK_SUCCESS_EASY", if large { 0.98 } else { 0.95 }),
            success_hard: env("MOCK_SUCCESS_HARD", if large { 0.9 } else { 0.3 }),
            profile,
        }
    }
}

struct Mock {
    s: Settings,
    slots: Arc<Semaphore>,
    running: AtomicUsize,
    waiting: AtomicUsize,
    requests: AtomicU64,
    prompt_tokens: AtomicU64,
    generation_tokens: AtomicU64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_target(false).init();
    let settings = Settings::from_env();
    let port: u16 = env("MOCK_PORT", 8000);
    tracing::info!(model = %settings.model, profile = %settings.profile, pod = %settings.pod, port, "mock-llm starting");
    let mock = Arc::new(Mock {
        slots: Arc::new(Semaphore::new(settings.slots.max(1))),
        s: settings,
        running: AtomicUsize::new(0),
        waiting: AtomicUsize::new(0),
        requests: AtomicU64::new(0),
        prompt_tokens: AtomicU64::new(0),
        generation_tokens: AtomicU64::new(0),
    });
    let app = Router::new()
        .route("/v1/chat/completions", post(chat))
        .route("/v1/models", get(models))
        .route("/health", get(|| async { "ok" }))
        .route("/metrics", get(metrics))
        .with_state(mock);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn models(State(m): State<Arc<Mock>>) -> Json<Value> {
    Json(
        json!({"object": "list", "data": [{"id": m.s.model, "object": "model", "owned_by": "mock"}]}),
    )
}

async fn metrics(State(m): State<Arc<Mock>>) -> String {
    let model = &m.s.model;
    let running = m.running.load(Ordering::Relaxed);
    let waiting = m.waiting.load(Ordering::Relaxed);
    let kv = running as f64 / m.s.slots.max(1) as f64;
    format!(
        "# HELP vllm:num_requests_running SIMULATED by mock-llm: requests holding a slot.\n\
         # TYPE vllm:num_requests_running gauge\n\
         vllm:num_requests_running{{model_name=\"{model}\"}} {running}\n\
         # HELP vllm:num_requests_waiting SIMULATED by mock-llm: requests waiting for a slot.\n\
         # TYPE vllm:num_requests_waiting gauge\n\
         vllm:num_requests_waiting{{model_name=\"{model}\"}} {waiting}\n\
         # HELP vllm:kv_cache_usage_perc SIMULATED by mock-llm: slots in use / slots, not KV memory.\n\
         # TYPE vllm:kv_cache_usage_perc gauge\n\
         vllm:kv_cache_usage_perc{{model_name=\"{model}\"}} {kv}\n\
         # HELP vllm:cache_config_info SIMULATED by mock-llm: cache settings in vLLM's label format.\n\
         # TYPE vllm:cache_config_info gauge\n\
         vllm:cache_config_info{{block_size=\"16\",cache_dtype=\"auto\",enable_prefix_caching=\"True\",gpu_memory_utilization=\"0.9\",num_cpu_blocks=\"0\",num_gpu_blocks=\"{}\",model_name=\"{model}\"}} 1\n\
         # TYPE vllm:prompt_tokens_total counter\n\
         vllm:prompt_tokens_total{{model_name=\"{model}\"}} {}\n\
         # TYPE vllm:generation_tokens_total counter\n\
         vllm:generation_tokens_total{{model_name=\"{model}\"}} {}\n\
         # TYPE mock_requests_total counter\n\
         mock_requests_total{{model_name=\"{model}\",pod=\"{}\"}} {}\n",
        m.s.slots * 512,
        m.prompt_tokens.load(Ordering::Relaxed),
        m.generation_tokens.load(Ordering::Relaxed),
        m.s.pod,
        m.requests.load(Ordering::Relaxed),
    )
}

fn openai_error(status: StatusCode, message: &str, kind: &str) -> Response {
    (
        status,
        Json(json!({"error": {"message": message, "type": kind, "code": status.as_u16()}})),
    )
        .into_response()
}

fn approx_tokens(text: &str) -> u64 {
    (text.len() as u64).div_ceil(4).max(1)
}

fn message_text(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn task_tag(text: &str) -> Option<(String, bool)> {
    let start = text.find("[task:")?;
    let rest = &text[start + 6..];
    let end = rest.find(']')?;
    let mut parts = rest[..end].splitn(2, ':');
    let id = parts.next()?.to_string();
    let hard = parts.next()? == "hard";
    Some((id, hard))
}

/// A graded stand-in for a real judge: a solve probability for the efficient model, from the
/// wording (benchmark tags are ignored). Each hard marker lowers it, softer signals ("plan",
/// "why") lower it a little, and a per-prompt jitter keeps prompts with the same markers apart.
/// With `error_rate` the verdict is pushed far the wrong way, as a real judge sometimes is.
/// Graded on purpose: a route's `base_threshold` then changes which steps escalate.
fn judge_p_solve(text: &str, error_rate: f64) -> f64 {
    use std::hash::{Hash, Hasher};
    let text = strip_tag(&text.to_lowercase());
    let hard = HARD_MARKERS
        .iter()
        .filter(|m| text.contains(*m))
        .count()
        .min(3) as f64;
    let soft = SOFT_MARKERS
        .iter()
        .filter(|m| text.contains(*m))
        .count()
        .min(2) as f64;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    let jitter = (h.finish() % 1000) as f64 / 1000.0 * 0.16 - 0.08;
    let mut p = 0.88 - 0.22 * hard - 0.12 * soft + jitter;
    if text.len() > 1500 {
        p -= 0.2;
    }
    if rand::rng().random::<f64>() < error_rate {
        p += if p < 0.55 { 0.35 } else { -0.35 };
    }
    (p.clamp(0.02, 0.98) * 100.0).round() / 100.0
}

const SOFT_MARKERS: &[&str] = &["plan", "why", "strategy", "while", "explain", "work out"];

fn strip_tag(text: &str) -> String {
    match (text.find("[task:"), text.find(']')) {
        (Some(a), Some(b)) if b > a => format!("{}{}", &text[..a], &text[b + 1..]),
        _ => text.to_string(),
    }
}

const HARD_MARKERS: &[&str] = &[
    "prove",
    "design",
    "architect",
    "refactor",
    "optimi",
    "concurren",
    "distributed",
    "race condition",
    "derive",
    "trade-off",
    "tradeoff",
    "debug",
    "migrate",
    "formal",
    "multi-step",
    "root cause",
];

async fn chat(
    State(m): State<Arc<Mock>>,
    _headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    m.requests.fetch_add(1, Ordering::Relaxed);
    let requested = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if requested != m.s.model {
        // vLLM refuses unknown model names the same way; this catches wrong model-to-pool mapping.
        return openai_error(
            StatusCode::NOT_FOUND,
            &format!("The model `{requested}` does not exist."),
            "NotFoundError",
        );
    }
    if m.s.fail_rate > 0.0 && rand::rng().random::<f64>() < m.s.fail_rate {
        return openai_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "simulated overload",
            "ServiceUnavailableError",
        );
    }
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let prompt: String = messages
        .iter()
        .map(message_text)
        .collect::<Vec<_>>()
        .join("\n");
    let prompt_tokens = approx_tokens(&prompt);
    let last_user = messages
        .iter()
        .rev()
        .find(|msg| msg.get("role").and_then(Value::as_str) == Some("user"))
        .map(message_text)
        .unwrap_or_default();
    let last_role = messages
        .last()
        .and_then(|msg| msg.get("role"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let schema_name = body
        .pointer("/response_format/json_schema/name")
        .and_then(Value::as_str);

    // Decide the answer before taking a slot; generation time depends on its length.
    let (content, tool_call, finish) = if schema_name == Some("CapabilityClassifierDecision") {
        let p = judge_p_solve(&last_user, m.s.judge_error_rate);
        let verdict = if p >= 0.7 {
            json!({"crux": "short, well-defined task", "primary_rule": "SUP-1", "capability_boundary": "supported", "p_solve": p})
        } else if p >= 0.4 {
            json!({"crux": "some reasoning; the small model may manage", "primary_rule": "UNC-1", "capability_boundary": "uncertain", "p_solve": p})
        } else {
            json!({"crux": "multi-step reasoning or design work", "primary_rule": "LIM-1", "capability_boundary": "unsupported", "p_solve": p})
        };
        (verdict.to_string(), None, "stop")
    } else if body
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|t| !t.is_empty())
        && last_role == "user"
        && last_user.contains("[call-tool]")
    {
        let name = body
            .pointer("/tools/0/function/name")
            .and_then(Value::as_str)
            .unwrap_or("tool");
        (
            String::new(),
            Some(
                json!({"id": format!("call_{}", uuid::Uuid::now_v7().simple()), "type": "function", "function": {"name": name, "arguments": "{\"query\":\"mock\"}"}}),
            ),
            "tool_calls",
        )
    } else {
        let (result, words) = match task_tag(&last_user) {
            Some((id, hard)) => {
                let p = if hard {
                    m.s.success_hard
                } else {
                    m.s.success_easy
                };
                let pass = rand::rng().random::<f64>() < p;
                (
                    format!("TASK {id} RESULT: {}", if pass { "PASS" } else { "FAIL" }),
                    if hard { 180 } else { 40 },
                )
            }
            None if last_role == "tool" => (
                "The tool returned a result; here is the summary.".to_string(),
                20,
            ),
            None => (
                format!("Mock answer from {} ({}).", m.s.model, m.s.profile),
                30,
            ),
        };
        let max = body
            .get("max_completion_tokens")
            .or_else(|| body.get("max_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(u64::MAX);
        let words = (words as u64).min(max.saturating_sub(10).max(1)) as usize;
        let filler: Vec<&str> = std::iter::repeat_n("token", words).collect();
        (format!("{result} {}", filler.join(" ")), None, "stop")
    };
    let completion_tokens = if tool_call.is_some() {
        12
    } else {
        approx_tokens(&content)
    };

    m.waiting.fetch_add(1, Ordering::Relaxed);
    let permit = Arc::clone(&m.slots)
        .acquire_owned()
        .await
        .expect("semaphore open");
    m.waiting.fetch_sub(1, Ordering::Relaxed);
    m.running.fetch_add(1, Ordering::Relaxed);
    m.prompt_tokens.fetch_add(prompt_tokens, Ordering::Relaxed);
    m.generation_tokens
        .fetch_add(completion_tokens, Ordering::Relaxed);
    let guard = SlotGuard {
        mock: Arc::clone(&m),
        _permit: Some(permit),
    };

    let ttft = Duration::from_millis(m.s.ttft_ms + prompt_tokens / 20);
    let per_token = Duration::from_secs_f64(1.0 / m.s.tokens_per_sec.max(1.0));
    let id = format!("chatcmpl-{}", uuid::Uuid::now_v7().simple());
    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let usage = json!({"prompt_tokens": prompt_tokens, "completion_tokens": completion_tokens, "total_tokens": prompt_tokens + completion_tokens});
    let model = m.s.model.clone();
    let pod = m.s.pod.clone();

    if body.get("stream").and_then(Value::as_bool).unwrap_or(false) {
        let include_usage = body
            .pointer("/stream_options/include_usage")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let stream = async_stream::stream! {
            let _guard = guard;
            tokio::time::sleep(ttft).await;
            let chunk = |delta: Value, finish: Option<&str>| json!({"id": id, "object": "chat.completion.chunk", "created": created, "model": model, "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]});
            yield Ok::<_, std::convert::Infallible>(Event::default().data(chunk(json!({"role": "assistant", "content": ""}), None).to_string()));
            if let Some(call) = &tool_call {
                yield Ok(Event::default().data(chunk(json!({"tool_calls": [{"index": 0, "id": call["id"], "type": "function", "function": {"name": call["function"]["name"], "arguments": call["function"]["arguments"]}}]}), None).to_string()));
            } else {
                let words: Vec<&str> = content.split(' ').collect();
                for piece in words.chunks(4) {
                    tokio::time::sleep(per_token * piece.len() as u32).await;
                    yield Ok(Event::default().data(chunk(json!({"content": format!("{} ", piece.join(" "))}), None).to_string()));
                }
            }
            yield Ok(Event::default().data(chunk(json!({}), Some(finish)).to_string()));
            if include_usage {
                yield Ok(Event::default().data(json!({"id": id, "object": "chat.completion.chunk", "created": created, "model": model, "choices": [], "usage": usage}).to_string()));
            }
            yield Ok(Event::default().data("[DONE]"));
        };
        return ([("x-served-by", pod)], Sse::new(stream)).into_response();
    }

    tokio::time::sleep(ttft + per_token * completion_tokens as u32).await;
    drop(guard);
    let message = match tool_call {
        Some(call) => json!({"role": "assistant", "content": null, "tool_calls": [call]}),
        None => json!({"role": "assistant", "content": content}),
    };
    (
        [("x-served-by", pod)],
        Json(json!({"id": id, "object": "chat.completion", "created": created, "model": model,
            "choices": [{"index": 0, "message": message, "finish_reason": finish}], "usage": usage})),
    )
        .into_response()
}

struct SlotGuard {
    mock: Arc<Mock>,
    _permit: Option<tokio::sync::OwnedSemaphorePermit>,
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        self.mock.running.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::judge_p_solve;

    #[test]
    fn judge_is_graded_by_wording() {
        let easy = judge_p_solve("[task:a:easy] Translate 'good morning' into Spanish.", 0.0);
        let one = judge_p_solve("Architect a retry strategy for a payment API.", 0.0);
        let two = judge_p_solve("Find the root cause of this distributed deadlock.", 0.0);
        assert!(easy >= 0.8, "easy {easy}");
        assert!((0.4..0.75).contains(&one), "one marker {one}");
        assert!(two < 0.55, "two markers {two}");
        assert!(easy > one && one > two);
        // the same prompt gets the same verdict without errors
        assert_eq!(
            judge_p_solve("Spell 'debug' backwards.", 0.0),
            judge_p_solve("Spell 'debug' backwards.", 0.0)
        );
    }
}
