//! Tenant identity and policy limits. Kept apart from model selection on purpose.
//!
//! The limits are request-rate, concurrency, per-request token caps and an *estimated*
//! spend budget computed from configured token prices. None of them is a GPU quota or a
//! token quota enforced at the model server.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use http::HeaderMap;
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use switchyard_protocol::ModelId;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::config::{Config, IdentityMode, Limits, TenantConfig};

pub struct Tenant {
    pub id: String,
    pub allowed_models: HashSet<ModelId>,
    pub allowed_routes: Option<HashSet<String>>,
    pub limits: Limits,
    rate: Option<Mutex<TokenBucket>>,
    concurrency: Option<Arc<Semaphore>>,
    spend: Mutex<SpendWindow>,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum Denied {
    #[error("missing or invalid credentials")]
    Unauthenticated,
    #[error("route {0} is not permitted for this tenant")]
    RouteNotAllowed(String),
    #[error("request rate limit exceeded")]
    RateLimited,
    #[error("too many concurrent requests")]
    ConcurrencyLimited,
    #[error("estimated spend budget for the current hour is used up")]
    BudgetExhausted,
    #[error("max tokens {requested} exceeds this tenant's limit of {limit}")]
    MaxTokens { requested: u64, limit: u64 },
}

impl Denied {
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Unauthenticated => "unauthenticated",
            Self::RouteNotAllowed(_) => "route_not_allowed",
            Self::RateLimited => "rate_limited",
            Self::ConcurrencyLimited => "concurrency_limited",
            Self::BudgetExhausted => "budget_exhausted",
            Self::MaxTokens { .. } => "max_tokens",
        }
    }
}

/// Held for the life of one request; releases the concurrency slot on drop.
pub struct Admission {
    _permit: Option<OwnedSemaphorePermit>,
}

impl Tenant {
    fn new(config: &TenantConfig) -> Self {
        Self {
            id: config.id.clone(),
            allowed_models: config
                .allowed_models
                .iter()
                .map(|m| ModelId::from(m.as_str()))
                .collect(),
            allowed_routes: config
                .allowed_routes
                .as_ref()
                .map(|routes| routes.iter().cloned().collect()),
            limits: config.limits.clone(),
            rate: config
                .limits
                .requests_per_minute
                .map(|rpm| Mutex::new(TokenBucket::per_minute(rpm))),
            concurrency: config
                .limits
                .max_concurrent
                .map(|n| Arc::new(Semaphore::new(n as usize))),
            spend: Mutex::new(SpendWindow::new()),
        }
    }

    pub fn check_route(&self, route: &str, is_direct_model: bool) -> Result<(), Denied> {
        if is_direct_model {
            return Ok(()); // direct model names are checked against allowed_models
        }
        match &self.allowed_routes {
            Some(routes) if !routes.contains(route) => Err(Denied::RouteNotAllowed(route.into())),
            _ => Ok(()),
        }
    }

    pub fn check_max_tokens(&self, requested: Option<u64>) -> Result<(), Denied> {
        match (requested, self.limits.max_tokens_per_request) {
            (Some(requested), Some(limit)) if requested > limit => {
                Err(Denied::MaxTokens { requested, limit })
            }
            _ => Ok(()),
        }
    }

    /// Admission control. Rate is consumed only when every other check passes.
    pub fn admit(&self) -> Result<Admission, Denied> {
        if let Some(budget) = self.limits.cost_budget_usd_per_hour
            && self.spend.lock().spent(Instant::now()) >= budget
        {
            return Err(Denied::BudgetExhausted);
        }
        let permit = match &self.concurrency {
            Some(semaphore) => Some(
                Arc::clone(semaphore)
                    .try_acquire_owned()
                    .map_err(|_| Denied::ConcurrencyLimited)?,
            ),
            None => None,
        };
        if let Some(rate) = &self.rate
            && !rate.lock().take(Instant::now())
        {
            return Err(Denied::RateLimited);
        }
        Ok(Admission { _permit: permit })
    }

    /// Records estimated spend. Requests already admitted can overshoot the budget.
    pub fn record_spend(&self, usd: f64) {
        if usd > 0.0 {
            self.spend.lock().add(Instant::now(), usd);
        }
    }
}

pub struct Tenants {
    mode: IdentityMode,
    trusted_header: Option<String>,
    by_digest: HashMap<[u8; 32], Arc<Tenant>>,
    by_id: HashMap<String, Arc<Tenant>>,
}

impl Tenants {
    pub fn new(config: &Config) -> Self {
        let mut by_digest = HashMap::new();
        let mut by_id = HashMap::new();
        for tenant_config in &config.tenants {
            let tenant = Arc::new(Tenant::new(tenant_config));
            for digest in &tenant_config.api_key_sha256 {
                let mut key = [0u8; 32];
                // Validated as 64 hex characters at config load.
                if hex::decode_to_slice(digest, &mut key).is_ok() {
                    by_digest.insert(key, Arc::clone(&tenant));
                }
            }
            by_id.insert(tenant.id.clone(), tenant);
        }
        Self {
            mode: config.identity.mode,
            trusted_header: config.identity.trusted_header.clone(),
            by_digest,
            by_id,
        }
    }

    pub fn by_id(&self, id: &str) -> Option<Arc<Tenant>> {
        self.by_id.get(id).cloned()
    }

    /// Resolves the tenant from authenticated context only. Never from `x-switchyard-*` or
    /// any other client-chosen identity header.
    pub fn authenticate(&self, headers: &HeaderMap) -> Result<Arc<Tenant>, Denied> {
        match self.mode {
            IdentityMode::ApiKey => {
                let key = headers
                    .get(http::header::AUTHORIZATION)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.strip_prefix("Bearer "))
                    .map(str::trim)
                    .filter(|k| !k.is_empty())
                    .ok_or(Denied::Unauthenticated)?;
                let digest: [u8; 32] = Sha256::digest(key.as_bytes()).into();
                self.by_digest
                    .get(&digest)
                    .cloned()
                    .ok_or(Denied::Unauthenticated)
            }
            IdentityMode::TrustedHeader => {
                let name = self
                    .trusted_header
                    .as_deref()
                    .ok_or(Denied::Unauthenticated)?;
                let id = headers
                    .get(name)
                    .and_then(|v| v.to_str().ok())
                    .ok_or(Denied::Unauthenticated)?;
                self.by_id.get(id).cloned().ok_or(Denied::Unauthenticated)
            }
        }
    }
}

struct TokenBucket {
    capacity: f64,
    tokens: f64,
    per_sec: f64,
    last: Instant,
}

impl TokenBucket {
    fn per_minute(rpm: u32) -> Self {
        let capacity = f64::from(rpm.max(1));
        Self {
            capacity,
            tokens: capacity,
            per_sec: capacity / 60.0,
            last: Instant::now(),
        }
    }

    fn take(&mut self, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.per_sec).min(self.capacity);
        self.last = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// Rolling one-hour spend, in one-minute buckets.
struct SpendWindow {
    buckets: Vec<(Instant, f64)>,
}

const WINDOW: Duration = Duration::from_secs(3600);

impl SpendWindow {
    fn new() -> Self {
        Self {
            buckets: Vec::new(),
        }
    }

    fn prune(&mut self, now: Instant) {
        self.buckets
            .retain(|(at, _)| now.saturating_duration_since(*at) < WINDOW);
    }

    fn add(&mut self, now: Instant, usd: f64) {
        self.prune(now);
        match self.buckets.last_mut() {
            Some((at, total)) if now.saturating_duration_since(*at) < Duration::from_secs(60) => {
                *total += usd
            }
            _ => self.buckets.push((now, usd)),
        }
    }

    fn spent(&mut self, now: Instant) -> f64 {
        self.prune(now);
        self.buckets.iter().map(|(_, usd)| usd).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "sk-acme-demo";

    fn config(extra: &str) -> Config {
        let digest = hex::encode(Sha256::digest(KEY.as_bytes()));
        Config::parse(&format!(
            r#"
[identity]
mode = "api_key"
[[tenants]]
id = "acme"
api_key_sha256 = ["{digest}"]
allowed_models = ["small"]
{extra}
[models.small]
provider_model = "s"
base_url = "http://small/v1"
"#
        ))
        .unwrap()
    }

    fn bearer(key: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::AUTHORIZATION,
            format!("Bearer {key}").parse().unwrap(),
        );
        headers
    }

    #[test]
    fn api_key_resolves_tenant_and_ignores_identity_headers() {
        let tenants = Tenants::new(&config(""));
        assert_eq!(tenants.authenticate(&bearer(KEY)).unwrap().id, "acme");
        assert!(tenants.authenticate(&bearer("wrong")).is_err());
        let mut spoofed = HeaderMap::new();
        spoofed.insert("x-tenant-id", "acme".parse().unwrap());
        spoofed.insert("x-switchyard-agent-id", "acme".parse().unwrap());
        assert_eq!(
            tenants.authenticate(&spoofed).err(),
            Some(Denied::Unauthenticated)
        );
    }

    #[test]
    fn concurrency_limit_releases_on_drop() {
        let tenants = Tenants::new(&config("limits = { max_concurrent = 1 }"));
        let tenant = tenants.authenticate(&bearer(KEY)).unwrap();
        let first = tenant.admit().unwrap();
        assert_eq!(tenant.admit().err(), Some(Denied::ConcurrencyLimited));
        drop(first);
        assert!(tenant.admit().is_ok());
    }

    #[test]
    fn rate_limit_and_budget() {
        let tenants = Tenants::new(&config(
            "limits = { requests_per_minute = 2, cost_budget_usd_per_hour = 0.01 }",
        ));
        let tenant = tenants.authenticate(&bearer(KEY)).unwrap();
        assert!(tenant.admit().is_ok());
        assert!(tenant.admit().is_ok());
        assert_eq!(tenant.admit().err(), Some(Denied::RateLimited));
        tenant.record_spend(0.02);
        assert_eq!(tenant.admit().err(), Some(Denied::BudgetExhausted));
    }

    #[test]
    fn max_tokens_cap() {
        let tenants = Tenants::new(&config("limits = { max_tokens_per_request = 100 }"));
        let tenant = tenants.authenticate(&bearer(KEY)).unwrap();
        assert!(tenant.check_max_tokens(Some(100)).is_ok());
        assert!(tenant.check_max_tokens(None).is_ok());
        assert!(tenant.check_max_tokens(Some(101)).is_err());
    }
}
