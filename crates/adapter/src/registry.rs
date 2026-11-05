//! Binds logical model names to BNK inference routes and builds the Switchyard algorithms.
//!
//! A logical model is served through exactly one URL plus fixed headers. The BNK HTTPRoute
//! that matches that URL (or header) names one InferencePool; the pool's F5 EPP then picks
//! the endpoint. Nothing here chooses an endpoint.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use switchyard_libsy::{
    Algorithm, LlmClassifierConfig, LlmTaskClassifier, Passthrough, Random, RuntimeModels,
};
use switchyard_llm_client::{Backend, HttpBackendConfig, ModelConfig, TranslatingLlmClient};
use switchyard_protocol::{Category, ModelId, RoutedLlmClient};

use crate::config::{Config, Cost, RouteConfig, TierFallback};

pub struct ModelInfo {
    pub provider_model: String,
    pub gateway_model: String,
    pub inference_pool: String,
    pub cost: Cost,
}

/// One public route: a Switchyard algorithm and the categories it routes over.
pub struct Route {
    pub name: String,
    pub algorithm: Arc<dyn Algorithm>,
    /// Category -> ordered logical models, before tenant filtering.
    pub categories: Vec<(Category, Vec<ModelId>)>,
    /// Models the algorithm may call while deciding that are operator-owned (the judge).
    pub routing_only: Vec<(Category, ModelId)>,
    /// Categories that must be non-empty after tenant filtering for the algorithm to work.
    pub required: Vec<Category>,
}

/// The models one request may use, after tenant policy is applied.
pub struct Plan {
    pub route: Arc<Route>,
    pub models: Arc<RuntimeModels>,
    /// Models allowed to produce the answer (tenant-authorized).
    pub answer_allowed: HashSet<ModelId>,
    /// Models allowed during routing: answer models plus operator-owned judge models.
    pub routing_allowed: HashSet<ModelId>,
    pub degraded: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    #[error("model {0} is not permitted for this tenant")]
    ModelNotAllowed(String),
    #[error("route {route} needs a {category} model and this tenant may use none")]
    TierNotAllowed { route: String, category: String },
}

pub struct Registry {
    pub client: Arc<dyn RoutedLlmClient>,
    pub models: BTreeMap<ModelId, ModelInfo>,
    pub routes: HashMap<String, Arc<Route>>,
}

impl Registry {
    pub fn build(config: &Config) -> anyhow::Result<Self> {
        let mut model_configs = Vec::new();
        let mut models = BTreeMap::new();
        for (name, entry) in &config.models {
            let mut extra_headers = entry.headers.clone();
            extra_headers.insert(config.server.hop_header.clone(), "1".into());
            let api_key = match &entry.api_key_env {
                Some(var) => Some(
                    std::env::var(var)
                        .with_context(|| format!("model {name}: env var {var} is not set"))?,
                ),
                None => None,
            };
            let backend = Backend::OpenAiChat(HttpBackendConfig {
                base_url: entry.base_url.clone(),
                api_key,
                forward_auth: false,
                extra_headers,
                extra_body: BTreeMap::new(),
                omit_body_fields: Default::default(),
                reasoning_effort: None,
                max_retries: entry.max_retries,
                failure_cooldown: Duration::ZERO,
                timeout: Some(Duration::from_secs(entry.timeout_secs)),
            });
            model_configs.push(
                ModelConfig::new(name.as_str(), backend, None)
                    .with_provider_model(entry.provider_model.as_str()),
            );
            models.insert(
                ModelId::from(name.as_str()),
                ModelInfo {
                    provider_model: entry.provider_model.clone(),
                    gateway_model: entry
                        .gateway_model
                        .clone()
                        .unwrap_or_else(|| entry.provider_model.clone()),
                    inference_pool: entry.inference_pool.clone().unwrap_or_default(),
                    cost: entry.cost,
                },
            );
        }
        let client = TranslatingLlmClient::new(&model_configs)
            .context("building the upstream model client")?
            .without_rate_limit_cooldown();

        let mut routes = HashMap::new();
        for (name, route) in &config.routes {
            routes.insert(name.clone(), Arc::new(build_route(name, route)?));
        }
        // Every logical model is also a route of its own: a direct, single-model passthrough.
        for name in config.models.keys() {
            routes.insert(
                name.clone(),
                Arc::new(Route {
                    name: name.clone(),
                    algorithm: Arc::new(Passthrough),
                    categories: vec![(Category::Any, vec![name.as_str().into()])],
                    routing_only: Vec::new(),
                    required: vec![Category::Any],
                }),
            );
        }
        Ok(Self {
            client: Arc::new(client),
            models,
            routes,
        })
    }

    /// Restricts a route to the tenant's models. Authorization lives here, not in algorithms:
    /// the algorithm is only ever shown models the tenant may use.
    pub fn plan(
        &self,
        route: &Arc<Route>,
        allowed: &HashSet<ModelId>,
        fallback: TierFallback,
    ) -> Result<Plan, PlanError> {
        let is_direct = self.models.contains_key(route.name.as_str());
        if is_direct && !allowed.contains(route.name.as_str()) {
            return Err(PlanError::ModelNotAllowed(route.name.clone()));
        }
        let mut by_category = HashMap::new();
        let mut answer_allowed = HashSet::new();
        let mut degraded = false;
        for (category, models) in &route.categories {
            let permitted: Vec<ModelId> = models
                .iter()
                .filter(|m| allowed.contains(*m))
                .cloned()
                .collect();
            if permitted.len() != models.len() {
                degraded = true;
            }
            answer_allowed.extend(permitted.iter().cloned());
            by_category.insert(category.clone(), permitted);
        }
        if answer_allowed.is_empty() {
            return Err(PlanError::TierNotAllowed {
                route: route.name.clone(),
                category: "permitted".into(),
            });
        }
        for category in &route.required {
            if by_category.get(category).is_some_and(|m| !m.is_empty()) {
                continue;
            }
            if fallback == TierFallback::Reject {
                return Err(PlanError::TierNotAllowed {
                    route: route.name.clone(),
                    category: category.as_str().to_string(),
                });
            }
            // Degrade: fill the missing tier from the tenant's permitted models on this route,
            // so the algorithm can only ever answer with something the tenant may use.
            let mut substitute: Vec<ModelId> = answer_allowed.iter().cloned().collect();
            substitute.sort();
            by_category.insert(category.clone(), substitute);
        }
        let mut routing_allowed = answer_allowed.clone();
        for (category, model) in &route.routing_only {
            by_category.insert(category.clone(), vec![model.clone()]);
            routing_allowed.insert(model.clone());
        }
        // `Any` lets passthrough-style fallbacks see every permitted answer model.
        by_category.entry(Category::Any).or_insert_with(|| {
            let mut all: Vec<ModelId> = answer_allowed.iter().cloned().collect();
            all.sort();
            all
        });
        let models = RuntimeModels::new(by_category).with_target_restriction();
        Ok(Plan {
            route: Arc::clone(route),
            models: Arc::new(models),
            answer_allowed,
            routing_allowed,
            degraded: degraded && !is_direct,
        })
    }

    /// The route's most expensive answer model, by output price: what the same request
    /// would cost if every request went to the frontier model.
    pub fn frontier_cost(&self, route: &Route) -> Cost {
        route
            .categories
            .iter()
            .flat_map(|(_, models)| models)
            .map(|m| self.cost(m))
            .max_by(|a, b| a.output_per_1m_usd.total_cmp(&b.output_per_1m_usd))
            .unwrap_or_default()
    }

    pub fn cost(&self, model: &ModelId) -> Cost {
        self.models.get(model).map(|m| m.cost).unwrap_or_default()
    }

    pub fn pool(&self, model: &ModelId) -> &str {
        self.models
            .get(model)
            .map(|m| m.inference_pool.as_str())
            .unwrap_or("")
    }

    pub fn gateway_model<'a>(&'a self, model: &'a ModelId) -> &'a str {
        self.models
            .get(model)
            .map(|m| m.gateway_model.as_str())
            .unwrap_or(model.as_str())
    }

    /// A known model, named by its logical name or its gateway model name.
    pub fn known_model(&self, name: &str) -> Option<ModelId> {
        if self.models.contains_key(name) {
            return Some(name.into());
        }
        self.models
            .iter()
            .find(|(_, info)| info.gateway_model == name)
            .map(|(id, _)| id.clone())
    }

    /// Where an ext_proc decision falls back to when routing fails: the most capable model the
    /// plan permits (last category first). Never a model outside `allowed`.
    pub fn fail_open_model(
        &self,
        route: &Route,
        allowed: &std::collections::HashSet<ModelId>,
    ) -> Option<ModelId> {
        route
            .categories
            .iter()
            .rev()
            .flat_map(|(_, models)| models)
            .find(|m| allowed.contains(*m))
            .cloned()
    }

    pub fn provider_model<'a>(&'a self, model: &'a ModelId) -> &'a str {
        self.models
            .get(model)
            .map(|m| m.provider_model.as_str())
            .unwrap_or(model.as_str())
    }
}

fn ids(models: &[String]) -> Vec<ModelId> {
    models.iter().map(|m| ModelId::from(m.as_str())).collect()
}

fn build_route(name: &str, route: &RouteConfig) -> anyhow::Result<Route> {
    let route = match route {
        RouteConfig::Passthrough { models } => Route {
            name: name.into(),
            algorithm: Arc::new(Passthrough),
            categories: vec![(Category::Any, ids(models))],
            routing_only: Vec::new(),
            required: vec![Category::Any],
        },
        RouteConfig::Random {
            models,
            weights,
            seed,
        } => Route {
            name: name.into(),
            algorithm: Arc::new(
                Random::new(weights.clone(), *seed)
                    .with_context(|| format!("route {name}: random router"))?,
            ),
            categories: vec![(Category::Any, ids(models))],
            routing_only: Vec::new(),
            required: vec![Category::Any],
        },
        RouteConfig::TaskClassifier {
            efficient,
            capable,
            judge,
            classifier,
        } => Route {
            name: name.into(),
            algorithm: Arc::new(
                LlmTaskClassifier::new(LlmClassifierConfig::Capability {
                    config: classifier.as_ref().clone(),
                })
                .with_context(|| format!("route {name}: task classifier"))?,
            ),
            categories: vec![
                (Category::Efficient, ids(efficient)),
                (Category::Capable, ids(capable)),
            ],
            routing_only: vec![(Category::Judge, judge.as_str().into())],
            required: vec![Category::Efficient, Category::Capable],
        },
    };
    Ok(route)
}
