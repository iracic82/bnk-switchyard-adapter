//! Adapter configuration: the model-to-pool registry, public routes, and tenant policy.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, bail, ensure};
use serde::Deserialize;
use switchyard_libsy::TaskClassifierConfig;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    pub identity: IdentityConfig,
    #[serde(default)]
    pub tenants: Vec<TenantConfig>,
    /// Logical model name -> where it is served. Clients may name these directly.
    pub models: BTreeMap<String, ModelEntry>,
    /// Public model names that run a Switchyard algorithm (for example `auto`).
    #[serde(default)]
    pub routes: BTreeMap<String, RouteConfig>,
    /// Optional Envoy ext_proc server: decide the model inside a gateway's request path.
    #[serde(default)]
    pub ext_proc: Option<ExtProcConfig>,
}

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct ExtProcConfig {
    pub listen: String,
    /// Tenant whose model list and limits apply. The gateway in front has already
    /// authenticated the caller.
    pub tenant: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ServerConfig {
    pub listen: String,
    pub max_body_bytes: usize,
    /// Whole-request deadline, including streaming.
    pub request_timeout_secs: u64,
    /// Deadline for the Switchyard decision (all routing-time calls).
    pub routing_timeout_secs: u64,
    /// Header the adapter adds to every upstream call and refuses on ingress.
    pub hop_header: String,
    /// After a shutdown signal: report not-ready and keep serving this long, so load
    /// balancers stop sending new requests before the listener closes.
    pub shutdown_drain_secs: u64,
    /// Serve the live decision view at `/live` (internal port only).
    pub live_view: bool,
    /// Characters of the last user message shown in the live view. 0 shows none.
    pub live_prompt_preview_chars: usize,
    /// Serve `POST /v1/chat/completions`. Turn off when the adapter only decides for a gateway
    /// (ext_proc): then it answers no client and needs no API keys.
    pub chat_api: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:8080".into(),
            max_body_bytes: 1024 * 1024,
            request_timeout_secs: 300,
            routing_timeout_secs: 30,
            hop_header: "x-switchyard-adapter-hop".into(),
            shutdown_drain_secs: 5,
            live_view: false,
            live_prompt_preview_chars: 0,
            chat_api: true,
        }
    }
}

#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IdentityMode {
    /// `Authorization: Bearer <key>`, checked against SHA-256 digests in tenant config.
    ApiKey,
    /// A header set by an authenticating proxy that strips any client copy.
    TrustedHeader,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdentityConfig {
    pub mode: IdentityMode,
    #[serde(default)]
    pub trusted_header: Option<String>,
}

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct TenantConfig {
    pub id: String,
    #[serde(default)]
    pub api_key_sha256: Vec<String>,
    /// Logical models this tenant may receive answers from.
    pub allowed_models: Vec<String>,
    /// Public routes this tenant may call. Missing means all routes.
    #[serde(default)]
    pub allowed_routes: Option<Vec<String>>,
    #[serde(default)]
    pub limits: Limits,
}

#[derive(Deserialize, Clone, Default)]
#[serde(deny_unknown_fields, default)]
pub struct Limits {
    pub requests_per_minute: Option<u32>,
    pub max_concurrent: Option<u32>,
    /// Upper bound on `max_tokens` / `max_completion_tokens`. Larger requests are rejected.
    pub max_tokens_per_request: Option<u64>,
    /// Estimated spend per rolling hour, from configured token prices. Not a GPU quota.
    pub cost_budget_usd_per_hour: Option<f64>,
    /// What to do when a route needs a tier the tenant may not use.
    pub tier_fallback: TierFallback,
}

#[derive(Deserialize, Clone, Copy, Default, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TierFallback {
    /// Refuse the request with 403.
    #[default]
    Reject,
    /// Route among the tenant's permitted models only, and say so in a response header.
    Degrade,
}

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct ModelEntry {
    /// Model name the inference server serves (sent upstream as `model`).
    pub provider_model: String,
    /// OpenAI-compatible base URL, normally a BNK internal Gateway VIP, ending in `/v1`.
    pub base_url: String,
    /// Fixed headers for this model, for example the pool selector header.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// `namespace/name` of the InferencePool behind `base_url`. Used only in telemetry.
    #[serde(default)]
    pub inference_pool: Option<String>,
    #[serde(default = "default_model_timeout")]
    pub timeout_secs: u64,
    #[serde(default = "default_retries")]
    pub max_retries: u32,
    /// Environment variable holding an upstream API key, if the backend needs one.
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub cost: Cost,
    /// Model name the gateway routes on in ext_proc mode (for example a Polaris AIModel
    /// name). Defaults to `provider_model`.
    #[serde(default)]
    pub gateway_model: Option<String>,
}

const fn default_model_timeout() -> u64 {
    120
}
const fn default_retries() -> u32 {
    1
}

#[derive(Deserialize, Clone, Copy, Default)]
#[serde(deny_unknown_fields, default)]
pub struct Cost {
    pub input_per_1m_usd: f64,
    pub output_per_1m_usd: f64,
}

impl Cost {
    pub fn estimate(&self, input_tokens: u64, output_tokens: u64) -> f64 {
        (input_tokens as f64 * self.input_per_1m_usd
            + output_tokens as f64 * self.output_per_1m_usd)
            / 1_000_000.0
    }
}

#[derive(Deserialize)]
#[serde(tag = "algorithm", rename_all = "snake_case", deny_unknown_fields)]
pub enum RouteConfig {
    /// First model answers; the rest are ordered fallbacks.
    Passthrough { models: Vec<String> },
    /// Weighted random choice among `models`.
    Random {
        models: Vec<String>,
        #[serde(default)]
        weights: Option<Vec<f64>>,
        #[serde(default)]
        seed: Option<u64>,
    },
    /// Switchyard's LLM task classifier: a judge model picks efficient or capable.
    TaskClassifier {
        efficient: Vec<String>,
        capable: Vec<String>,
        /// Model used for the judge call. Operator-owned; not subject to tenant model lists.
        judge: String,
        classifier: Box<TaskClassifierConfig>,
    },
}

impl RouteConfig {
    pub fn answer_models(&self) -> BTreeSet<&str> {
        match self {
            Self::Passthrough { models } | Self::Random { models, .. } => {
                models.iter().map(String::as_str).collect()
            }
            Self::TaskClassifier {
                efficient, capable, ..
            } => efficient
                .iter()
                .chain(capable)
                .map(String::as_str)
                .collect(),
        }
    }
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("parsing config {}", path.display()))
    }

    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let config: Self = toml::from_str(text)?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> anyhow::Result<()> {
        ensure!(!self.models.is_empty(), "at least one model is required");
        ensure!(
            !self.server.hop_header.is_empty(),
            "server.hop_header must not be empty"
        );
        if self.identity.mode == IdentityMode::TrustedHeader {
            ensure!(
                self.identity.trusted_header.is_some(),
                "identity.trusted_header is required when identity.mode = \"trusted_header\""
            );
        }
        let known = |name: &str| self.models.contains_key(name);
        for (name, model) in &self.models {
            ensure!(
                model.base_url.starts_with("http://") || model.base_url.starts_with("https://"),
                "model {name}: base_url must be an http(s) URL"
            );
            for header in model.headers.keys() {
                ensure!(
                    !header.eq_ignore_ascii_case(&self.server.hop_header),
                    "model {name}: header {header} is reserved for loop detection"
                );
            }
        }
        for (name, route) in &self.routes {
            ensure!(
                !self.models.contains_key(name),
                "route {name} has the same name as a model; names must be unique"
            );
            for model in route.answer_models() {
                ensure!(known(model), "route {name} names unknown model {model}");
            }
            match route {
                RouteConfig::Passthrough { models } => {
                    ensure!(!models.is_empty(), "route {name}: models must not be empty")
                }
                RouteConfig::Random {
                    models, weights, ..
                } => {
                    ensure!(!models.is_empty(), "route {name}: models must not be empty");
                    if let Some(weights) = weights {
                        ensure!(
                            weights.len() == models.len(),
                            "route {name}: weights must match models one to one"
                        );
                    }
                }
                RouteConfig::TaskClassifier {
                    efficient,
                    capable,
                    judge,
                    ..
                } => {
                    ensure!(
                        !efficient.is_empty() && !capable.is_empty(),
                        "route {name}: efficient and capable must not be empty"
                    );
                    ensure!(
                        known(judge),
                        "route {name} names unknown judge model {judge}"
                    );
                }
            }
        }
        let mut ids = BTreeSet::new();
        let mut digests = BTreeSet::new();
        for tenant in &self.tenants {
            ensure!(ids.insert(&tenant.id), "duplicate tenant id {}", tenant.id);
            for model in &tenant.allowed_models {
                ensure!(
                    known(model),
                    "tenant {} allows unknown model {model}",
                    tenant.id
                );
            }
            for route in tenant.allowed_routes.iter().flatten() {
                ensure!(
                    self.routes.contains_key(route),
                    "tenant {} allows unknown route {route}",
                    tenant.id
                );
            }
            for digest in &tenant.api_key_sha256 {
                if digest.len() != 64 || hex::decode(digest).is_err() {
                    bail!(
                        "tenant {}: api_key_sha256 entries must be 64 hex characters",
                        tenant.id
                    );
                }
                ensure!(
                    digests.insert(digest.to_ascii_lowercase()),
                    "an API key digest is assigned to more than one tenant"
                );
            }
            if let Some(budget) = tenant.limits.cost_budget_usd_per_hour {
                ensure!(
                    budget.is_finite() && budget >= 0.0,
                    "tenant {}: budget must be >= 0",
                    tenant.id
                );
            }
        }
        if let Some(ext) = &self.ext_proc {
            ensure!(
                self.tenants.iter().any(|t| t.id == ext.tenant),
                "ext_proc.tenant {} is not a configured tenant",
                ext.tenant
            );
        }
        ensure!(
            self.server.chat_api || self.ext_proc.is_some(),
            "server.chat_api = false needs [ext_proc], or the adapter serves nothing"
        );
        if self.identity.mode == IdentityMode::ApiKey && self.server.chat_api {
            ensure!(
                self.tenants.iter().any(|t| !t.api_key_sha256.is_empty()),
                "identity.mode = \"api_key\" needs at least one tenant API key digest"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) const SAMPLE: &str = r#"
[identity]
mode = "api_key"

[[tenants]]
id = "acme"
api_key_sha256 = ["2bb80d537b1da3e38bd30361aa855686bde0eacd7162fef6a25fe97bf527a25b"]
allowed_models = ["small"]

[models.small]
provider_model = "s"
base_url = "http://small/v1"

[models.large]
provider_model = "l"
base_url = "http://large/v1"

[routes.auto]
algorithm = "task_classifier"
efficient = ["small"]
capable = ["large"]
judge = "small"
classifier = { base_threshold = 0.5 }
"#;

    #[test]
    fn parses_sample() {
        let config = Config::parse(SAMPLE).unwrap();
        assert_eq!(config.server.hop_header, "x-switchyard-adapter-hop");
        assert!(matches!(
            config.routes["auto"],
            RouteConfig::TaskClassifier { .. }
        ));
    }

    #[test]
    fn rejects_unknown_model_reference() {
        let text = SAMPLE.replace("capable = [\"large\"]", "capable = [\"huge\"]");
        let error = Config::parse(&text).err().unwrap();
        assert!(format!("{error:#}").contains("unknown model huge"));
    }

    #[test]
    fn rejects_route_named_like_model() {
        let text = SAMPLE.replace("[routes.auto]", "[routes.small]");
        assert!(Config::parse(&text).is_err());
    }

    #[test]
    fn rejects_reserved_hop_header_in_model_headers() {
        let text = SAMPLE.replace(
            "base_url = \"http://large/v1\"",
            "base_url = \"http://large/v1\"\nheaders = { \"x-switchyard-adapter-hop\" = \"0\" }",
        );
        assert!(Config::parse(&text).is_err());
    }

    #[test]
    fn shipped_configs_parse() {
        for (name, text) in [
            (
                "config/adapter.local.toml",
                include_str!("../../../config/adapter.local.toml"),
            ),
            (
                "deploy/k8s/base/adapter.toml",
                include_str!("../../../deploy/k8s/base/adapter.toml"),
            ),
            (
                "overlays/single-internal-gateway/adapter.toml",
                include_str!("../../../deploy/k8s/overlays/single-internal-gateway/adapter.toml"),
            ),
            (
                "deploy/kind-smoke/adapter.toml",
                include_str!("../../../deploy/kind-smoke/adapter.toml"),
            ),
        ] {
            Config::parse(text).unwrap_or_else(|e| panic!("{name}: {e:#}"));
        }
        let lab = include_str!("../../../deploy/instruqt/adapter.toml")
            .replace("__POOL_NS__", "aigw-models");
        let lab =
            Config::parse(&lab).unwrap_or_else(|e| panic!("deploy/instruqt/adapter.toml: {e:#}"));
        assert_eq!(
            lab.ext_proc.as_ref().map(|e| e.listen.as_str()),
            Some("0.0.0.0:50051")
        );
    }

    #[test]
    fn ext_proc_only_needs_no_api_keys() {
        let text = SAMPLE
            .replace(
                "api_key_sha256 = [\"2bb80d537b1da3e38bd30361aa855686bde0eacd7162fef6a25fe97bf527a25b\"]\n",
                "",
            )
            .replace("[identity]", "[server]\nchat_api = false\n\n[identity]")
            + "\n[ext_proc]\nlisten = \"127.0.0.1:0\"\ntenant = \"acme\"\n";
        Config::parse(&text).unwrap();
        let no_ext = text.split("\n[ext_proc]").next().unwrap().to_string();
        assert!(
            Config::parse(&no_ext).is_err(),
            "chat_api = false without ext_proc serves nothing"
        );
    }

    #[test]
    fn rejects_unknown_fields() {
        let text = SAMPLE.replace("[identity]", "[identity]\nbogus = 1");
        assert!(Config::parse(&text).is_err());
    }
}
