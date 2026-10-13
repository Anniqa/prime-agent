//! Port of `pi-antigravity/src/models/discovery.ts`: refresh the Antigravity model catalog
//! from the Cloud Code `fetchAvailableModels` endpoint, hydrate it (and the dynamic enum
//! cache) from a persisted snapshot, and resolve the project id through the OAuth layer.
//!
//! The persistence *hook* is not ported: the extension publishes through the Pi models store
//! (`context.publish`); here [`refresh_antigravity_models`] returns the payload a caller can
//! persist instead of writing it itself. Project discovery reuses
//! [`crate::oauth::resolve_project_id`].

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::oauth::resolve_project_id;
use crate::oauth::{ProviderHttp, ProviderHttpMethod, ProviderHttpRequest, ProviderHttpResponse};
use crate::providers::antigravity::{antigravity_headers, endpoint_candidates};
use crate::providers::antigravity_grouping::{build_antigravity_catalog, resolved_catalog};
use crate::providers::antigravity_models::{
    apply_catalog, compiled_catalog, current_catalog, register_discovered_model_enums,
    restore_dynamic_model_enums, snapshot_dynamic_model_enums, AntigravityCatalog,
};
use crate::types::Model;

/// Default catalog refresh interval: four hours (`DEFAULT_CATALOG_REFRESH_INTERVAL_MS`).
pub const DEFAULT_CATALOG_REFRESH_INTERVAL_MS: i64 = 4 * 60 * 60 * 1000;

/// The extension-private persist key (`ANTIGRAVITY_PERSIST_KEY`).
pub const ANTIGRAVITY_PERSIST_KEY: &str = "pi-antigravity";

/// Metadata lookups must be fast; a stalled endpoint falls through to the next.
const DISCOVERY_TIMEOUT_MS: u64 = 8_000;

/// The extension-private state stored beside the model list (`PersistedAntigravityCatalog`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistedAntigravityCatalog {
    pub catalog: AntigravityCatalog,
    pub checked_at: i64,
    pub model_enums: HashMap<String, String>,
}

/// `getCatalogRefreshIntervalMs`: the `ANTIGRAVITY_*REFRESH_INTERVAL_MS` override or the default.
#[must_use]
pub fn get_catalog_refresh_interval_ms() -> i64 {
    let value = crate::providers::antigravity::antigravity_env("CATALOG_REFRESH_INTERVAL_MS")
        .or_else(|| crate::providers::antigravity::antigravity_env("REFRESH_INTERVAL_MS"));
    if let Some(value) = value {
        if let Some(parsed) = parse_int_prefix(&value) {
            if parsed >= 0 {
                return parsed;
            }
        }
    }
    DEFAULT_CATALOG_REFRESH_INTERVAL_MS
}

/// `hydrateAntigravityCatalog`: restore the persisted catalog and enums, returning `checkedAt`.
///
/// Applied before any offline/TTL decision, exactly like the extension.
pub fn hydrate_antigravity_catalog(stored: &Value) -> i64 {
    let Some(stored) = stored.as_object() else {
        return 0;
    };
    let Some(persisted) = stored
        .get(ANTIGRAVITY_PERSIST_KEY)
        .and_then(Value::as_object)
    else {
        return 0;
    };

    if let Some(enums) = persisted.get("modelEnums") {
        if is_string_map(enums) {
            if let Some(enums) = enums.as_object() {
                let restored: HashMap<String, String> = enums
                    .iter()
                    .map(|(key, value)| {
                        (key.clone(), value.as_str().unwrap_or_default().to_string())
                    })
                    .collect();
                restore_dynamic_model_enums(&restored);
            }
        }
    }
    if let Some(catalog) = persisted.get("catalog") {
        if is_catalog(catalog) {
            if let Ok(parsed) = serde_json::from_value::<AntigravityCatalog>(catalog.clone()) {
                apply_catalog(parsed);
            }
        }
    }

    persisted
        .get("checkedAt")
        .and_then(Value::as_i64)
        .filter(|checked_at| *checked_at > 0)
        .unwrap_or(0)
}

fn is_catalog(value: &Value) -> bool {
    value.as_object().is_some_and(|object| {
        object
            .get("models")
            .and_then(Value::as_array)
            .is_some_and(|models| !models.is_empty())
            && object.get("routing").is_some_and(Value::is_object)
    })
}

fn is_string_map(value: &Value) -> bool {
    value
        .as_object()
        .is_some_and(|object| object.values().all(Value::is_string))
}

/// `parseApiKey`: the JSON credential carrying `token` and `projectId`.
#[must_use]
pub fn parse_api_key(raw: &str) -> Option<(String, String)> {
    let value: Value = serde_json::from_str(raw.trim()).ok()?;
    let token = value
        .get("token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())?;
    let project_id = value
        .get("projectId")
        .and_then(Value::as_str)
        .filter(|project| !project.is_empty())?;
    Some((token.to_string(), project_id.to_string()))
}

/// `discoverAntigravityModels`: fetch the available models and build the catalog.
///
/// # Errors
///
/// Returns the parse error when `api_key` is not a `{token, projectId}` credential, or the
/// fetch failure when no endpoint answered.
pub async fn discover_antigravity_models(
    http: &dyn ProviderHttp,
    api_key: &str,
) -> Result<AntigravityCatalog, String> {
    let Some((token, project_id)) = parse_api_key(api_key) else {
        return Err(
            "Invalid Antigravity credentials. Run /login antigravity. (missing token or projectId)"
                .to_string(),
        );
    };
    discover_with_token(http, &token, &project_id).await
}

/// Adaptation for prime-agent's credential shape: resolve the project id with the OAuth layer
/// ([`crate::oauth::resolve_project_id`]) instead of requiring it in the credential.
///
/// # Errors
///
/// Returns an error when the project cannot be resolved or the fetch fails.
pub async fn discover_antigravity_models_for_token(
    http: &dyn ProviderHttp,
    access_token: &str,
) -> Result<AntigravityCatalog, String> {
    let Some(project_id) = resolve_project_id(http, access_token).await else {
        return Err("Antigravity project discovery failed: no endpoint available".to_string());
    };
    discover_with_token(http, access_token, &project_id).await
}

async fn discover_with_token(
    http: &dyn ProviderHttp,
    token: &str,
    project_id: &str,
) -> Result<AntigravityCatalog, String> {
    let available = fetch_available_models_catalog(http, token, project_id).await?;
    let models = available
        .get("models")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if models.is_empty() {
        return Ok(AntigravityCatalog {
            models: Vec::new(),
            routing: HashMap::new(),
        });
    }
    register_discovered_model_enums(&models);
    Ok(build_antigravity_catalog(&models, &compiled_catalog()))
}

/// `fetchAvailableModelsCatalog`: merge the endpoint candidates' payloads.
async fn fetch_available_models_catalog(
    http: &dyn ProviderHttp,
    token: &str,
    project_id: &str,
) -> Result<Value, String> {
    let mut results = Vec::new();
    // The extension issues these concurrently; the merge is order-independent, so a simple
    // sequential pass yields the same result deterministically.
    for endpoint in endpoint_candidates() {
        results.push(fetch_available_models_from_endpoint(http, &endpoint, token, project_id).await);
    }
    merge_available_models_results(results)
}

async fn fetch_available_models_from_endpoint(
    http: &dyn ProviderHttp,
    endpoint: &str,
    token: &str,
    project_id: &str,
) -> Option<Value> {
    let request = ProviderHttpRequest {
        method: ProviderHttpMethod::Post,
        url: format!("{endpoint}/v1internal:fetchAvailableModels"),
        headers: antigravity_headers(token),
        body: Some(json!({ "project": project_id }).to_string()),
        follow_redirects: true,
    };
    let response = http.request(request, DISCOVERY_TIMEOUT_MS).await.ok()?;
    if !response.ok() {
        return None;
    }
    Some(serde_json::from_str(&response.body).unwrap_or_else(|_| json!({ "raw": response.body })))
}

/// `mergeAvailableModelsResults`: merge `models` and the default agent model across endpoints.
fn merge_available_models_results(results: Vec<Option<Value>>) -> Result<Value, String> {
    let mut merged = Map::new();
    let mut default_agent_model_id: Option<String> = None;
    let mut default_agent_model: Option<String> = None;
    let mut any = false;

    for data in results.into_iter().flatten() {
        any = true;
        if let Some(models) = data.get("models").and_then(Value::as_object) {
            for (key, value) in models {
                merged.insert(key.clone(), value.clone());
            }
            register_discovered_model_enums(models);
        }
        if let Some(value) = data.get("defaultAgentModelId").and_then(Value::as_str) {
            default_agent_model_id = Some(value.to_string());
        }
        if let Some(value) = data.get("defaultAgentModel").and_then(Value::as_str) {
            default_agent_model = Some(value.to_string());
        }
    }

    if !any {
        return Err("/v1internal:fetchAvailableModels failed: no endpoint available".to_string());
    }

    let mut out = Map::new();
    out.insert("models".to_string(), Value::Object(merged));
    if let Some(value) = default_agent_model_id {
        out.insert("defaultAgentModelId".to_string(), Value::String(value));
    }
    if let Some(value) = default_agent_model {
        out.insert("defaultAgentModel".to_string(), Value::String(value));
    }
    Ok(Value::Object(out))
}

/// The `refreshAntigravityModels` inputs (the extension's `RefreshModelsContext`, minus the
/// provider-owned persistence store).
#[derive(Debug, Clone, Default)]
pub struct RefreshModelsContext {
    /// The raw credential (a `{token, projectId}` JSON string); `None` skips the network.
    pub credential: Option<String>,
    /// The previously persisted state handed back by the models store.
    pub stored: Option<Value>,
    /// Whether the caller allows a network refresh (offline mode passes `false`).
    pub allow_network: bool,
    /// A forced/manual refresh bypasses the TTL and surfaces failures.
    pub force: bool,
    /// The aborted signal, folded into a bool.
    pub aborted: bool,
}

/// What a refresh produced: the effective models and the state to persist (when it changed).
#[derive(Debug, Clone)]
pub struct RefreshOutcome {
    pub models: Vec<Model>,
    /// The payload a store should persist: `{models, "pi-antigravity": {catalog, checkedAt, modelEnums}}`.
    pub persist: Option<Value>,
}

/// `refreshAntigravityModels`: hydrate, honour offline/TTL, refresh from the network, and apply
/// the resolved catalog. Returns the effective models plus the persist payload.
///
/// # Errors
///
/// Only a *forced* refresh surfaces a fetch failure; an unforced failure keeps last-known-good.
pub async fn refresh_antigravity_models(
    http: &dyn ProviderHttp,
    context: &RefreshModelsContext,
) -> Result<RefreshOutcome, String> {
    let checked_at = context
        .stored
        .as_ref()
        .map_or(0, hydrate_antigravity_catalog);
    let current = current_catalog();

    if !context.allow_network {
        return Ok(RefreshOutcome {
            models: current.models,
            persist: None,
        });
    }

    let Some(api_key) = context.credential.as_deref().filter(|key| !key.is_empty()) else {
        return Ok(RefreshOutcome {
            models: current.models,
            persist: None,
        });
    };
    if context.aborted {
        return Ok(RefreshOutcome {
            models: current.models,
            persist: None,
        });
    }

    let now = now_ms();
    if !context.force
        && checked_at > 0
        && now >= checked_at
        && now - checked_at < get_catalog_refresh_interval_ms()
    {
        return Ok(RefreshOutcome {
            models: current.models,
            persist: None,
        });
    }

    match discover_antigravity_models(http, api_key).await {
        Ok(discovered) => {
            if context.aborted {
                return Ok(RefreshOutcome {
                    models: current.models,
                    persist: None,
                });
            }
            let next = resolved_catalog(Some(&discovered), &current);
            if !next.models.is_empty() && !discovered.models.is_empty() {
                apply_catalog(next.clone());
                let refreshed_at = now_ms();
                let persisted = PersistedAntigravityCatalog {
                    catalog: next.clone(),
                    checked_at: refreshed_at,
                    model_enums: snapshot_dynamic_model_enums(),
                };
                let persist = json!({
                    "models": next.models.clone(),
                    ANTIGRAVITY_PERSIST_KEY: persisted,
                });
                return Ok(RefreshOutcome {
                    models: next.models,
                    persist: Some(persist),
                });
            }
        }
        Err(error) => {
            if context.force {
                return Err(error);
            }
        }
    }

    Ok(RefreshOutcome {
        models: current_catalog().models,
        persist: None,
    })
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as i64)
}

/// `parseInt` semantics: leading whitespace, optional sign, then digits.
fn parse_int_prefix(value: &str) -> Option<i64> {
    let trimmed = value.trim_start();
    let (sign, rest) = match trimmed.strip_prefix('-') {
        Some(rest) => (-1_i64, rest),
        None => (1, trimmed.strip_prefix('+').unwrap_or(trimmed)),
    };
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse::<i64>().ok().map(|parsed| parsed * sign)
}

#[cfg(test)]
mod tests {
    //! Mirrors `scripts/test-model-discovery.ts`: persisted offline catalog, dynamic enums,
    //! TTL skipping, and the persist payload, with a scripted `ProviderHttp` (no network).

    use super::*;
    use crate::providers::antigravity_grouping::build_antigravity_catalog;
    use crate::providers::antigravity_models::{
        clear_model_enum_cache, compiled_catalog, current_models, global_state_test_lock,
        model_enum, register_model_enum, reset_catalog, runtime_model,
    };
    use std::sync::Mutex;

    fn api_key_json() -> String {
        json!({ "token": "fake", "projectId": "fake" }).to_string()
    }

    /// A scripted transport: 200 with `body` for `fetchAvailableModels`, 404 otherwise.
    struct ScriptedHttp {
        body: String,
        serve: bool,
        calls: Mutex<Vec<String>>,
    }

    impl ScriptedHttp {
        fn serving(body: &str) -> Self {
            Self {
                body: body.to_string(),
                serve: true,
                calls: Mutex::new(Vec::new()),
            }
        }

        fn failing() -> Self {
            Self {
                body: "{}".to_string(),
                serve: false,
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl ProviderHttp for ScriptedHttp {
        fn request(
            &self,
            request: ProviderHttpRequest,
            _timeout_ms: u64,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<ProviderHttpResponse, String>> + Send + '_>,
        > {
            self.calls.lock().unwrap().push(request.url.clone());
            let serves = self.serve && request.url.contains("fetchAvailableModels");
            let body = self.body.clone();
            Box::pin(async move {
                if serves {
                    Ok(ProviderHttpResponse { status: 200, body })
                } else {
                    Ok(ProviderHttpResponse {
                        status: 404,
                        body: "{}".to_string(),
                    })
                }
            })
        }
    }

    fn dynamic_catalog_raw() -> Map<String, Value> {
        let mut raw = Map::new();
        raw.insert(
            "gemini-9.9-flash-low".to_string(),
            json!({ "displayName": "Gemini 9.9 Flash (Low)" }),
        );
        raw
    }

    #[test]
    fn hydrate_ignores_absent_and_malformed_state() {
        let _guard = global_state_test_lock();
        assert_eq!(hydrate_antigravity_catalog(&Value::Null), 0);
        assert_eq!(
            hydrate_antigravity_catalog(&json!({ ANTIGRAVITY_PERSIST_KEY: {} })),
            0
        );
        assert_eq!(hydrate_antigravity_catalog(&json!({})), 0);
    }

    #[test]
    fn offline_restart_restores_dynamic_catalog_and_enums() {
        let _guard = global_state_test_lock();
        reset_catalog();
        clear_model_enum_cache();

        let dynamic = build_antigravity_catalog(&dynamic_catalog_raw(), &compiled_catalog());
        apply_catalog(dynamic.clone());
        clear_model_enum_cache();
        register_model_enum("gemini-9.9-flash-low", "MODEL_DYNAMIC_999");
        let stored = json!({
            ANTIGRAVITY_PERSIST_KEY: {
                "catalog": dynamic,
                "checkedAt": 123,
                "modelEnums": snapshot_dynamic_model_enums(),
            }
        });

        reset_catalog();
        clear_model_enum_cache();

        assert_eq!(hydrate_antigravity_catalog(&stored), 123);
        assert!(current_models().iter().any(|model| model.id == "gemini-9.9-flash"));
        assert_eq!(runtime_model("gemini-9.9-flash"), "gemini-9.9-flash-low");
        assert_eq!(
            model_enum("gemini-9.9-flash-low").as_deref(),
            Some("MODEL_DYNAMIC_999")
        );
        assert_eq!(
            model_enum("gemini-9.9-flash").as_deref(),
            Some("MODEL_DYNAMIC_999")
        );

        reset_catalog();
        clear_model_enum_cache();
    }

    #[tokio::test]
    async fn fresh_persisted_state_skips_network() {
        let _guard = global_state_test_lock();
        reset_catalog();
        clear_model_enum_cache();
        let stored = json!({
            ANTIGRAVITY_PERSIST_KEY: {
                "catalog": compiled_catalog(),
                "checkedAt": now_ms() - 1_000,
                "modelEnums": {},
            }
        });
        let http = ScriptedHttp::serving(r#"{"models":{"gemini-10.0-flash-low":{"model":"X"}}}"#);
        let context = RefreshModelsContext {
            credential: Some(api_key_json()),
            stored: Some(stored),
            allow_network: true,
            force: false,
            aborted: false,
        };
        let outcome = refresh_antigravity_models(&http, &context).await.unwrap();
        assert_eq!(outcome.models.len(), 8);
        assert!(http.calls().is_empty(), "fresh state must not touch the network");
    }

    #[tokio::test]
    async fn offline_refresh_returns_current_models() {
        let _guard = global_state_test_lock();
        reset_catalog();
        clear_model_enum_cache();
        let http = ScriptedHttp::serving("{}");
        let context = RefreshModelsContext {
            credential: Some(api_key_json()),
            stored: None,
            allow_network: false,
            force: false,
            aborted: false,
        };
        let outcome = refresh_antigravity_models(&http, &context).await.unwrap();
        assert_eq!(outcome.models.len(), 8);
        assert!(http.calls().is_empty());
    }

    #[tokio::test]
    async fn successful_discovery_applies_and_persists() {
        let _guard = global_state_test_lock();
        reset_catalog();
        clear_model_enum_cache();
        let http = ScriptedHttp::serving(
            r#"{"models":{"gemini-10.0-flash-low":{"displayName":"Gemini 10.0 Flash (Low)","model":"MODEL_DYNAMIC_1000"}}}"#,
        );
        let context = RefreshModelsContext {
            credential: Some(api_key_json()),
            stored: None,
            allow_network: true,
            force: true,
            aborted: false,
        };
        let outcome = refresh_antigravity_models(&http, &context).await.unwrap();
        assert!(outcome.models.iter().any(|model| model.id == "gemini-10.0-flash"));
        let persist = outcome.persist.expect("a successful refresh persists");
        assert!(persist[ANTIGRAVITY_PERSIST_KEY]["catalog"]["routing"]["gemini-10.0-flash"]
            .is_object());
        assert_eq!(
            persist[ANTIGRAVITY_PERSIST_KEY]["modelEnums"]["gemini-10.0-flash-low"],
            "MODEL_DYNAMIC_1000"
        );
        assert!(http
            .calls()
            .iter()
            .all(|url| url.contains("fetchAvailableModels")));
        reset_catalog();
        clear_model_enum_cache();
    }

    #[tokio::test]
    async fn forced_refresh_surfaces_failures() {
        let _guard = global_state_test_lock();
        reset_catalog();
        clear_model_enum_cache();
        let http = ScriptedHttp::failing();
        let context = RefreshModelsContext {
            credential: Some(api_key_json()),
            stored: None,
            allow_network: true,
            force: true,
            aborted: false,
        };
        assert!(refresh_antigravity_models(&http, &context).await.is_err());
    }

    #[tokio::test]
    async fn unforced_failure_keeps_last_known_good() {
        let _guard = global_state_test_lock();
        reset_catalog();
        clear_model_enum_cache();
        let http = ScriptedHttp::failing();
        let context = RefreshModelsContext {
            credential: Some(api_key_json()),
            stored: None,
            allow_network: true,
            force: false,
            aborted: false,
        };
        let outcome = refresh_antigravity_models(&http, &context).await.unwrap();
        assert_eq!(outcome.models.len(), 8);
        assert!(outcome.persist.is_none());
    }

    #[tokio::test]
    async fn discover_requires_json_project_credentials() {
        let _guard = global_state_test_lock();
        let http = ScriptedHttp::serving("{}");
        assert!(discover_antigravity_models(&http, "not-json").await.is_err());
        assert!(discover_antigravity_models(&http, r#"{"token":"t"}"#).await.is_err());
        assert!(parse_api_key(r#"{"token":"t","projectId":"p"}"#).is_some());
    }

    #[tokio::test]
    async fn discovery_without_models_returns_an_empty_catalog() {
        let _guard = global_state_test_lock();
        let http = ScriptedHttp::serving(r#"{"models":{}}"#);
        let catalog = discover_antigravity_models(&http, &api_key_json()).await.unwrap();
        assert!(catalog.models.is_empty());
        assert!(catalog.routing.is_empty());
    }
}
