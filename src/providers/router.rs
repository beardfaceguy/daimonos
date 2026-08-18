//! Multi-provider routing (operator request 2026-08-17).
//!
//! One session, several provider adapters. `MultiProvider` implements
//! [`LlmProvider`] and dispatches each call by `opts.model`, so everything
//! downstream — the agent loop, #1240 failover, the `/model` picker, startup
//! model discovery — keeps working on plain model strings with zero changes.
//!
//! Routing rules, in order:
//! 1. **Explicit prefix** `provider:slug` (e.g. `openai:gpt-5`): route to that
//!    named adapter with the prefix stripped. Always wins; works even when
//!    catalogs are unavailable.
//! 2. **Catalog membership**: the first configured adapter whose live catalog
//!    lists the slug. Built once per instance from `list_models`, in adapter
//!    order, so the primary wins slug collisions.
//! 3. **Primary fallback**: unknown slugs go to the first adapter — the same
//!    place they went before multi-provider existed.

use std::collections::HashMap;

use async_trait::async_trait;
use tokio::sync::OnceCell;

use super::{CompleteOpts, Context, LlmProvider, LlmResponse, StreamEvent};

/// How long the one-time routing-table build may spend per adapter catalog.
/// Bounded so a dead secondary can never stall the first turn.
const CATALOG_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

pub struct MultiProvider {
    /// `(name, adapter)`, primary first. Names are the `agent.env` provider
    /// names (`anthropic` | `openai` | `openrouter`) and serve as the
    /// explicit-prefix namespace.
    adapters: Vec<(String, Box<dyn LlmProvider>)>,
    /// Routing table + ordered catalogs. Built lazily from live catalogs on
    /// first route, then cached for the session.
    routes: OnceCell<Routes>,
}

/// One-time catalog fetch result: the slug→adapter table for routing, plus
/// each adapter's catalog in the provider's own (newest-first) order so the
/// merged `list_models` preserves the ordering the #1240 failover chain
/// depends on.
#[derive(Default)]
struct Routes {
    table: HashMap<String, usize>,
    catalogs: Vec<Vec<String>>,
}

impl MultiProvider {
    /// `adapters` must be non-empty; index 0 is the primary (fallback target).
    pub fn new(adapters: Vec<(String, Box<dyn LlmProvider>)>) -> Self {
        assert!(!adapters.is_empty(), "MultiProvider needs >= 1 adapter");
        MultiProvider {
            adapters,
            routes: OnceCell::new(),
        }
    }

    /// Resolve `model` to `(adapter, effective_model)`.
    async fn route<'a>(&'a self, model: &'a str) -> (&'a dyn LlmProvider, &'a str) {
        // Rule 1: explicit `provider:slug` prefix.
        if let Some((prefix, slug)) = model.split_once(':') {
            if let Some((_, adapter)) = self.adapters.iter().find(|(name, _)| name == prefix) {
                return (adapter.as_ref(), slug);
            }
        }
        // Rule 2: catalog membership (first adapter listing the slug wins).
        let routes = self.routes().await;
        if let Some(&index) = routes.table.get(model) {
            return (self.adapters[index].1.as_ref(), model);
        }
        // Rule 3: primary fallback.
        (self.adapters[0].1.as_ref(), model)
    }

    /// The cached slug → adapter table, built from live catalogs on first use.
    /// A failed or timed-out catalog contributes nothing (its models still
    /// reach it via the explicit prefix or primary fallback).
    async fn routes(&self) -> &Routes {
        self.routes
            .get_or_init(|| async {
                let mut routes = Routes {
                    catalogs: Vec::with_capacity(self.adapters.len()),
                    ..Routes::default()
                };
                for (index, (name, adapter)) in self.adapters.iter().enumerate() {
                    let catalog =
                        match tokio::time::timeout(CATALOG_TIMEOUT, adapter.list_models()).await {
                            Ok(Some(models)) => models,
                            _ => {
                                tracing::warn!(
                                    target: "daimonos::providers",
                                    provider = %name,
                                    "catalog unavailable for routing; its models need an explicit prefix"
                                );
                                routes.catalogs.push(Vec::new());
                                continue;
                            }
                        };
                    for slug in &catalog {
                        routes.table.entry(slug.clone()).or_insert(index);
                    }
                    routes.catalogs.push(catalog);
                }
                routes
            })
            .await
    }
}

#[async_trait]
impl LlmProvider for MultiProvider {
    async fn complete(&self, ctx: &Context, opts: &CompleteOpts) -> LlmResponse {
        let (adapter, slug) = self.route(&opts.model).await;
        if slug == opts.model {
            return adapter.complete(ctx, opts).await;
        }
        let mut opts = opts.clone();
        opts.model = slug.to_string();
        adapter.complete(ctx, &opts).await
    }

    async fn stream(
        &self,
        ctx: &Context,
        opts: &CompleteOpts,
        on_event: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> LlmResponse {
        let (adapter, slug) = self.route(&opts.model).await;
        if slug == opts.model {
            return adapter.stream(ctx, opts, on_event).await;
        }
        let mut opts = opts.clone();
        opts.model = slug.to_string();
        adapter.stream(ctx, &opts, on_event).await
    }

    /// Session-level capability follows the primary adapter: the ACP client
    /// decides image acceptance once per session, before any model switch is
    /// knowable.
    fn supports_images(&self) -> bool {
        self.adapters[0].1.supports_images()
    }

    async fn context_window(&self, model: &str) -> Option<u64> {
        let (adapter, slug) = self.route(model).await;
        adapter.context_window(slug).await
    }

    /// Merged catalog in adapter order (primary's models first), deduped
    /// first-wins — the same precedence the routing table uses. `None` only
    /// when every adapter's catalog is unavailable.
    async fn list_models(&self) -> Option<Vec<String>> {
        // Building the routing table performs the fetches; reuse its cached
        // result so startup discovery and routing agree and cost one fetch
        // per adapter total. Adapter order (primary first), each catalog in
        // its provider's own newest-first order, deduped first-wins — the
        // same precedence the routing table uses.
        let routes = self.routes().await;
        if routes.table.is_empty() {
            return None;
        }
        let mut seen = std::collections::HashSet::new();
        let mut merged: Vec<String> = Vec::new();
        for catalog in &routes.catalogs {
            for slug in catalog {
                if seen.insert(slug.as_str()) {
                    merged.push(slug.clone());
                }
            }
        }
        Some(merged)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    type Seen = std::sync::Arc<Mutex<Vec<(String, String)>>>;

    /// Records `(adapter_label, model)` for every complete call; catalog is
    /// scripted (`None` = unavailable).
    struct Probe {
        label: &'static str,
        catalog: Option<Vec<String>>,
        seen: Seen,
    }

    #[async_trait]
    impl LlmProvider for Probe {
        async fn complete(&self, _ctx: &Context, opts: &CompleteOpts) -> LlmResponse {
            self.seen
                .lock()
                .unwrap()
                .push((self.label.to_string(), opts.model.clone()));
            LlmResponse::error("probe")
        }

        async fn list_models(&self) -> Option<Vec<String>> {
            self.catalog.clone()
        }

        async fn context_window(&self, model: &str) -> Option<u64> {
            // Encode which adapter answered so tests can assert delegation.
            Some(1000 + self.label.len() as u64 + model.len() as u64)
        }
    }

    fn probes() -> (MultiProvider, Seen) {
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let a = Probe {
            label: "anthropic",
            catalog: Some(vec!["claude-opus-5".into(), "shared-slug".into()]),
            seen: seen.clone(),
        };
        let b = Probe {
            label: "openai",
            catalog: Some(vec!["gpt-5".into(), "shared-slug".into()]),
            seen: seen.clone(),
        };
        let router = MultiProvider::new(vec![
            ("anthropic".to_string(), Box::new(a) as Box<dyn LlmProvider>),
            ("openai".to_string(), Box::new(b) as Box<dyn LlmProvider>),
        ]);
        (router, seen)
    }

    fn opts(model: &str) -> CompleteOpts {
        CompleteOpts {
            model: model.to_string(),
            ..CompleteOpts::default()
        }
    }

    fn ctx() -> Context {
        Context {
            messages: vec![],
            system: None,
            tools: vec![],
            stable_prefix_len: 0,
        }
    }

    #[tokio::test]
    async fn routes_bare_slugs_by_catalog_membership() {
        let (router, seen) = probes();
        router.complete(&ctx(), &opts("gpt-5")).await;
        router.complete(&ctx(), &opts("claude-opus-5")).await;
        assert_eq!(
            seen.lock().unwrap().clone(),
            vec![
                ("openai".to_string(), "gpt-5".to_string()),
                ("anthropic".to_string(), "claude-opus-5".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn explicit_prefix_wins_and_is_stripped() {
        let (router, seen) = probes();
        // gpt-5 is in openai's catalog, but the prefix forces anthropic —
        // explicit intent beats catalog membership.
        router.complete(&ctx(), &opts("anthropic:gpt-5")).await;
        router.complete(&ctx(), &opts("openai:gpt-5")).await;
        assert_eq!(
            seen.lock().unwrap().clone(),
            vec![
                ("anthropic".to_string(), "gpt-5".to_string()),
                ("openai".to_string(), "gpt-5".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn unknown_slugs_and_slug_collisions_go_to_the_primary() {
        let (router, seen) = probes();
        router.complete(&ctx(), &opts("mystery-model")).await;
        // Both catalogs list shared-slug; first-configured (primary) wins.
        router.complete(&ctx(), &opts("shared-slug")).await;
        // An unknown prefix is not a route — it is part of the model name
        // (OpenRouter slugs contain '/', vendors may ship ':' someday), so it
        // falls through to catalog/primary rules.
        router.complete(&ctx(), &opts("unknown:thing")).await;
        assert_eq!(
            seen.lock().unwrap().clone(),
            vec![
                ("anthropic".to_string(), "mystery-model".to_string()),
                ("anthropic".to_string(), "shared-slug".to_string()),
                ("anthropic".to_string(), "unknown:thing".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn merged_catalog_preserves_adapter_order_and_dedupes_first_wins() {
        let (router, _) = probes();
        assert_eq!(
            router.list_models().await,
            Some(vec![
                "claude-opus-5".to_string(),
                "shared-slug".to_string(),
                "gpt-5".to_string(),
            ])
        );
    }

    #[tokio::test]
    async fn a_dead_catalog_still_routes_via_prefix_and_primary() {
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let a = Probe {
            label: "anthropic",
            catalog: Some(vec!["claude-opus-5".into()]),
            seen: seen.clone(),
        };
        let b = Probe {
            label: "openai",
            catalog: None, // catalog fetch failed
            seen: seen.clone(),
        };
        let router = MultiProvider::new(vec![
            ("anthropic".to_string(), Box::new(a) as Box<dyn LlmProvider>),
            ("openai".to_string(), Box::new(b) as Box<dyn LlmProvider>),
        ]);
        // Bare openai slug can't be discovered -> primary fallback.
        router.complete(&ctx(), &opts("gpt-5")).await;
        // Explicit prefix still reaches the dead-catalog adapter.
        router.complete(&ctx(), &opts("openai:gpt-5")).await;
        assert_eq!(
            seen.lock().unwrap().clone(),
            vec![
                ("anthropic".to_string(), "gpt-5".to_string()),
                ("openai".to_string(), "gpt-5".to_string()),
            ]
        );
        // Merged catalog = the healthy adapter's models only.
        assert_eq!(
            router.list_models().await,
            Some(vec!["claude-opus-5".to_string()])
        );
    }

    #[tokio::test]
    async fn context_window_delegates_with_the_stripped_slug() {
        let (router, _) = probes();
        // "openai:gpt-5" -> openai adapter ("openai".len()=6) with "gpt-5" (5).
        assert_eq!(router.context_window("openai:gpt-5").await, Some(1011));
        // Bare catalog slug -> anthropic adapter (9) with full name (13).
        assert_eq!(router.context_window("claude-opus-5").await, Some(1022));
    }
}
