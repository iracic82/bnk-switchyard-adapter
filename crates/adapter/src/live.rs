//! Live view: one event per routed or refused request, streamed to a single page.
//!
//! Disabled by default (`server.live_view`). It serves on the adapter port, which the
//! external BNK route does not expose (only `/v1/chat/completions` is routed), so open it
//! with a port-forward. Prompt previews are a separate opt-in
//! (`server.live_prompt_preview_chars`), because they show request content.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::broadcast;

use crate::http::AppState;

const RING: usize = 500;

#[derive(Serialize, Clone, Default)]
pub struct LiveEvent {
    pub ts_ms: u64,
    pub request_id: String,
    pub tenant: String,
    pub route: String,
    pub algorithm: String,
    pub model: String,
    pub provider_model: String,
    pub pool: String,
    /// The endpoint that answered, as reported by the model server.
    pub served_by: String,
    /// ok, stream_error, cancelled or refused.
    pub status: String,
    /// Refusal reason, or "degraded" when a tier was substituted by policy.
    pub reason: String,
    pub decision_ms: u64,
    pub ttft_ms: Option<u64>,
    pub e2e_ms: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub selection_cost_usd: f64,
    pub inference_cost_usd: f64,
    /// The same tokens priced on the route's most expensive model: the cost without routing.
    pub frontier_cost_usd: f64,
    /// The classifier's solve probability, when the algorithm reported one.
    pub score: Option<f64>,
    pub fallbacks: usize,
    pub prompt: String,
}

pub struct Live {
    tx: broadcast::Sender<Arc<str>>,
    ring: Mutex<VecDeque<Arc<str>>>,
    pub prompt_preview_chars: usize,
}

impl Live {
    pub fn new(prompt_preview_chars: usize) -> Self {
        let (tx, _) = broadcast::channel(256);
        Self {
            tx,
            ring: Mutex::new(VecDeque::with_capacity(RING)),
            prompt_preview_chars,
        }
    }

    pub fn publish(&self, mut event: LiveEvent) {
        event.ts_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let Ok(json) = serde_json::to_string(&event) else {
            return;
        };
        let json: Arc<str> = json.into();
        {
            let mut ring = self.ring.lock();
            if ring.len() == RING {
                ring.pop_front();
            }
            ring.push_back(Arc::clone(&json));
        }
        let _ = self.tx.send(json); // no subscribers is fine
    }

    /// The last user message, shortened, or empty when previews are off.
    pub fn preview(&self, body: &Value) -> String {
        if self.prompt_preview_chars == 0 {
            return String::new();
        }
        let text = body
            .get("messages")
            .and_then(Value::as_array)
            .and_then(|messages| {
                messages
                    .iter()
                    .rev()
                    .find(|m| m.get("role").and_then(Value::as_str) == Some("user"))
            })
            .map(|m| match m.get("content") {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Array(parts)) => parts
                    .iter()
                    .filter_map(|p| p.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join(" "),
                _ => String::new(),
            })
            .unwrap_or_default();
        let mut out: String = text.chars().take(self.prompt_preview_chars).collect();
        if text.chars().count() > self.prompt_preview_chars {
            out.push('…');
        }
        out
    }
}

pub async fn page(State(state): State<Arc<AppState>>) -> Response {
    if state.live.is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    Html(include_str!("live.html")).into_response()
}

pub async fn events(State(state): State<Arc<AppState>>) -> Response {
    let Some(live) = state.live.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let backlog: Vec<Arc<str>> = live.ring.lock().iter().cloned().collect();
    let mut rx = live.tx.subscribe();
    let stream = async_stream::stream! {
        for json in backlog {
            yield Ok::<_, Infallible>(Event::default().data(&*json));
        }
        loop {
            match rx.recv().await {
                Ok(json) => yield Ok(Event::default().data(&*json)),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    };
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
}
