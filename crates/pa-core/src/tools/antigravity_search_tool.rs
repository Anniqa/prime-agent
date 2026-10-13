//! The `google_search` tool: Antigravity Google Search grounding exposed to the model.
//!
//! Ports the tool half of `pi-antigravity` `src/features/google-search.ts` on top of the
//! already-ported `pa_ai::antigravity_search` surface: resolve the Antigravity
//! credential (`{token, projectId}`), call `execute_antigravity_search` (which walks the model
//! fallback list and the endpoint candidates, then formats a cited evidence brief), and return
//! that brief as a text content block.
//!
//! Only the model-facing definition lives here: the host (daemon/CLI) owns tool registration and
//! prompt-guideline wiring.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pa_ai::antigravity_search::{
    execute_antigravity_search, google_search_tool_enabled, google_search_tool_parameters,
    ExecuteSearchOptions, GOOGLE_SEARCH_TOOL_DESCRIPTION, GOOGLE_SEARCH_TOOL_LABEL,
    GOOGLE_SEARCH_TOOL_NAME, GOOGLE_SEARCH_TOOL_PROMPT_SNIPPET,
};
use pa_ai::antigravity_types::{safe_error, AntigravityHttp, ReqwestAntigravityHttp};
use pa_ai::oauth::{
    default_project_id, failover_to_next_account_with_dir, resolve_project_id, ReqwestProviderHttp,
};
use serde_json::{json, Value};

use crate::auth::{AuthStorage, ANTIGRAVITY_PROVIDER_ID};
use crate::tools::tool_definition::{
    AbortSignal, ExecuteFn, OnUpdate, ToolContentBlock, ToolDefinition, ToolExecutionResult,
    ToolUpdate,
};

/// Runtime wiring for [`antigravity_search_tool`].
#[derive(Clone)]
pub struct AntigravitySearchToolContext {
    /// Explicit Antigravity credential: a JSON `{token, projectId}` string, or a raw bearer
    /// token that the stored project id completes. `None` resolves the signed-in account from
    /// the auth store on every call, so a later `/login` is picked up.
    pub api_key: Option<String>,
    /// Agent-state dir holding `auth.json`. `None` uses `pa_types::platform::agent_dir()`
    /// (`PRIME_AGENT_CODING_AGENT_DIR` when set, else `~/.prime/agent`).
    pub agent_dir: Option<PathBuf>,
    /// Transport; tests script it, production uses [`ReqwestAntigravityHttp`].
    pub http: Arc<dyn AntigravityHttp>,
}

impl Default for AntigravitySearchToolContext {
    fn default() -> Self {
        Self {
            api_key: None,
            agent_dir: None,
            http: Arc::new(ReqwestAntigravityHttp::new()),
        }
    }
}

/// Builds the `google_search` definition with explicit wiring (credential, agent dir,
/// transport). The host calls this when it knows the agent dir.
#[must_use]
pub fn antigravity_search_tool_with_context(
    context: AntigravitySearchToolContext,
) -> ToolDefinition {
    let execute: ExecuteFn = Arc::new(move |_tool_call_id, params, signal, on_update| {
        let context = context.clone();
        Box::pin(async move { execute_google_search(context, params, signal, on_update).await })
    });
    ToolDefinition {
        name: GOOGLE_SEARCH_TOOL_NAME.to_string(),
        label: GOOGLE_SEARCH_TOOL_LABEL.to_string(),
        description: GOOGLE_SEARCH_TOOL_DESCRIPTION.to_string(),
        prompt_snippet: GOOGLE_SEARCH_TOOL_PROMPT_SNIPPET.to_string(),
        parameters: google_search_tool_parameters(),
        execution_mode: None,
        prepare_arguments: None,
        execute,
    }
}

/// The `google_search` tool with default wiring: the stored Antigravity credential and the real
/// HTTP transport. Equivalent to [`antigravity_search_tool_with_context`] with
/// [`AntigravitySearchToolContext::default`].
#[must_use]
pub fn antigravity_search_tool() -> ToolDefinition {
    antigravity_search_tool_with_context(AntigravitySearchToolContext::default())
}

/// Whether `google_search` should be registered
/// (`is_extra_tool_enabled("SEARCH")`: `ANTIGRAVITY_NO_EXTRA_TOOLS` and
/// `ANTIGRAVITY_NO_SEARCH_TOOL` both disable it).
#[must_use]
pub fn is_search_tool_enabled() -> bool {
    google_search_tool_enabled()
}

async fn execute_google_search(
    context: AntigravitySearchToolContext,
    params: Value,
    signal: Option<AbortSignal>,
    on_update: Option<OnUpdate>,
) -> anyhow::Result<ToolExecutionResult> {
    let api_key = resolve_search_api_key(&context).await?;
    let Some(api_key) = api_key.filter(|key| !key.trim().is_empty()) else {
        anyhow::bail!("No Antigravity credentials. Run /login antigravity first.");
    };

    let query = string_param(&params, "query").unwrap_or_default();
    if let Some(on_update) = on_update {
        on_update(ToolUpdate {
            content: vec![ToolContentBlock::text(format!(
                "Searching Google for: \"{}\"\u{2026}",
                query.trim()
            ))],
            details: Some(json!({})),
        });
    }

    let mut current_api_key = api_key;
    let mut tried_tokens: Vec<String> = Vec::new();
    if let Ok(value) = serde_json::from_str::<Value>(&current_api_key) {
        if let Some(token) = credential_token(&value) {
            tried_tokens.push(token);
        }
    }

    let text = loop {
        let options = ExecuteSearchOptions {
            api_key: Some(current_api_key.clone()),
            query: query.clone(),
            instruction: string_param(&params, "instruction"),
            urls: string_list_param(&params, "urls"),
            thinking: bool_param(&params, "thinking"),
            signal: signal.clone(),
        };

        match execute_antigravity_search(&options, context.http.as_ref()).await {
            Ok(text) => break text,
            Err(error) => {
                if is_search_quota_exhausted(&error) {
                    let provider_http = ReqwestProviderHttp::new();
                    if let Some(next) = failover_to_next_account_with_dir(
                        &provider_http,
                        context.agent_dir.as_deref(),
                        &tried_tokens,
                    )
                    .await
                    {
                        tried_tokens.push(next.token.clone());
                        let project = match next.project_id {
                            Some(project) => project,
                            None => resolve_project_id(&provider_http, &next.token)
                                .await
                                .unwrap_or_else(|| {
                                    default_project_id(
                                        next.email.as_deref().unwrap_or("antigravity-default"),
                                    )
                                }),
                        };
                        current_api_key =
                            json!({ "token": next.token, "projectId": project }).to_string();
                        continue;
                    }
                    anyhow::bail!(
                        "Antigravity search quota exhausted for all accounts: {}",
                        safe_error(&error)
                    );
                }
                anyhow::bail!("{}", safe_error(&error));
            }
        }
    };

    Ok(ToolExecutionResult::text(text))
}

/// Resolves the credential the search request needs: a `{token, projectId}` JSON string. The
/// explicit context key wins; otherwise the auth store (refresh-aware) or the provider env var
/// supplies the token, and the project id comes from `auth.json`, then model discovery, then the
/// deterministic fallback.
async fn resolve_search_api_key(
    context: &AntigravitySearchToolContext,
) -> anyhow::Result<Option<String>> {
    let explicit = context
        .api_key
        .as_deref()
        .map(str::trim)
        .filter(|key| !key.is_empty());
    if let Some(explicit) = explicit {
        if let Some(complete) = complete_credential(explicit) {
            return Ok(Some(complete));
        }
    }

    let agent_dir = context
        .agent_dir
        .clone()
        .or_else(pa_types::platform::agent_dir);
    let entry = agent_dir.as_deref().and_then(read_auth_entry);
    let stored_project = entry.as_ref().and_then(credential_project);
    let email = entry
        .as_ref()
        .and_then(|entry| entry.get("email"))
        .and_then(Value::as_str)
        .filter(|email| !email.is_empty())
        .map(str::to_string);

    let mut raw = explicit.map(str::to_string);
    if raw.is_none() {
        if let Some(agent_dir) = agent_dir.as_deref() {
            raw = AuthStorage::resolve_api_key(agent_dir, ANTIGRAVITY_PROVIDER_ID).await?;
        }
    }
    if raw.is_none() {
        raw = entry.as_ref().and_then(credential_token);
    }
    if raw.is_none() {
        raw = pa_ai::env_api_keys::get_env_api_key(ANTIGRAVITY_PROVIDER_ID);
    }

    let Some(raw) = raw.filter(|raw| !raw.trim().is_empty()) else {
        return Ok(None);
    };
    if let Some(complete) = complete_credential(&raw) {
        return Ok(Some(complete));
    }

    let token = raw.trim().to_string();
    let token = serde_json::from_str::<serde_json::Value>(&token)
        .ok()
        .and_then(|value| credential_token(&value))
        .unwrap_or(token);
    let project = match stored_project {
        Some(project) => project,
        None => resolve_project_id(&ReqwestProviderHttp::new(), &token)
            .await
            .unwrap_or_else(|| {
                default_project_id(email.as_deref().unwrap_or("antigravity-default"))
            }),
    };
    Ok(Some(
        json!({ "token": token, "projectId": project }).to_string(),
    ))
}

/// Reads the `antigravity` entry of `agent_dir/auth.json`, when present.
fn read_auth_entry(agent_dir: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(agent_dir.join("auth.json")).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    value.get(ANTIGRAVITY_PROVIDER_ID).cloned()
}

/// Normalizes a JSON credential carrying both a token and a project id; `None` when the value is
/// not such a credential (for example a raw bearer token).
fn complete_credential(raw: &str) -> Option<String> {
    let value: Value = serde_json::from_str(raw.trim()).ok()?;
    let token = credential_token(&value)?;
    let project = credential_project(&value)?;
    Some(json!({ "token": token, "projectId": project }).to_string())
}

fn credential_token(value: &Value) -> Option<String> {
    ["token", "access", "access_token"]
        .iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
        .filter(|token| !token.is_empty())
        .map(str::to_string)
}

fn credential_project(value: &Value) -> Option<String> {
    ["antigravityProjectId", "projectId", "project_id", "project"]
        .iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
        .filter(|project| !project.is_empty())
        .map(str::to_string)
}

/// Whether a search error indicates that the account's quota has been exhausted.
pub(crate) fn is_search_quota_exhausted(error: &str) -> bool {
    crate::tools::antigravity_image_tool::is_image_quota_exhausted(error)
}

/// A string tool parameter, or `None` when absent or not a string.
fn string_param(params: &Value, key: &str) -> Option<String> {
    params.get(key).and_then(Value::as_str).map(str::to_string)
}

/// A string-array tool parameter, or `None` when absent or not an array.
fn string_list_param(params: &Value, key: &str) -> Option<Vec<String>> {
    params.get(key).and_then(Value::as_array).map(|items| {
        items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect()
    })
}

/// A boolean tool parameter, or `None` when absent or not a boolean.
fn bool_param(params: &Value, key: &str) -> Option<bool> {
    params.get(key).and_then(Value::as_bool)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::VecDeque;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Mutex;

    use pa_ai::antigravity_models::{extra_tool_enabled, ExtraTool};
    use pa_ai::antigravity_types::AntigravityHttpResponse;
    use tokio_util::sync::CancellationToken;

    const FAKE_CREDENTIAL: &str = r#"{"token":"t","projectId":"p"}"#;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    type HttpHandler = Box<
        dyn Fn(&str, &[(String, String)]) -> Result<AntigravityHttpResponse, String> + Send + Sync,
    >;

    /// A scripted transport: serves a queue of responses or calls a handler, and records the calls made.
    struct ScriptedHttp {
        calls: Mutex<Vec<String>>,
        responses: Mutex<VecDeque<Result<AntigravityHttpResponse, String>>>,
        handler: Option<HttpHandler>,
    }

    impl ScriptedHttp {
        fn new(responses: Vec<Result<AntigravityHttpResponse, String>>) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                responses: Mutex::new(responses.into_iter().collect()),
                handler: None,
            }
        }

        fn from_handler<F>(handler: F) -> Self
        where
            F: Fn(&str, &[(String, String)]) -> Result<AntigravityHttpResponse, String>
                + Send
                + Sync
                + 'static,
        {
            Self {
                calls: Mutex::new(Vec::new()),
                responses: Mutex::new(VecDeque::new()),
                handler: Some(Box::new(handler)),
            }
        }

        fn empty() -> Self {
            Self::new(Vec::new())
        }

        fn json(body: &str) -> Result<AntigravityHttpResponse, String> {
            Ok(AntigravityHttpResponse {
                status: 200,
                body: body.as_bytes().to_vec(),
            })
        }

        fn status(status: u16, body: &str) -> Result<AntigravityHttpResponse, String> {
            Ok(AntigravityHttpResponse {
                status,
                body: body.as_bytes().to_vec(),
            })
        }

        fn calls(&self) -> Vec<String> {
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }
    }

    impl AntigravityHttp for ScriptedHttp {
        fn post<'a>(
            &'a self,
            url: &'a str,
            headers: Vec<(String, String)>,
            _body: Vec<u8>,
            _signal: Option<CancellationToken>,
        ) -> Pin<Box<dyn Future<Output = Result<AntigravityHttpResponse, String>> + Send + 'a>>
        {
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(url.to_string());
            if let Some(handler) = &self.handler {
                let response = handler(url, &headers);
                return Box::pin(async move { response });
            }
            let mut script = self
                .responses
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let response = if script.is_empty() {
                Err("no scripted response".to_string())
            } else if script.len() == 1 {
                script[0].clone()
            } else {
                script.pop_front().unwrap()
            };
            drop(script);
            Box::pin(async move { response })
        }
    }

    fn context(api_key: Option<&str>, http: Arc<ScriptedHttp>) -> AntigravitySearchToolContext {
        AntigravitySearchToolContext {
            api_key: api_key.map(str::to_string),
            agent_dir: None,
            http,
        }
    }

    /// A wrapped Antigravity response: one thought part plus one visible, grounded part.
    fn search_body() -> String {
        json!({
            "response": { "candidates": [{
                "content": { "role": "model", "parts": [
                    { "thought": true, "text": "Searching internal thoughts..." },
                    { "text": "MiMo-V2.6 was released on September 22, 2026." }
                ] },
                "finishReason": "STOP",
                "groundingMetadata": {
                    "webSearchQueries": ["mimo 2.6 release", "mimo specs"],
                    "groundingChunks": [{
                        "web": { "uri": "https://example.com/article", "title": "Example Article" }
                    }]
                }
            }] }
        })
        .to_string()
    }

    #[test]
    fn tool_spec_matches_the_extension() {
        let tool = antigravity_search_tool();
        assert_eq!(tool.name, "google_search");
        assert_eq!(tool.label, "Google Search");
        assert_eq!(tool.description, GOOGLE_SEARCH_TOOL_DESCRIPTION);
        assert_eq!(tool.prompt_snippet, GOOGLE_SEARCH_TOOL_PROMPT_SNIPPET);
        assert_eq!(tool.parameters["required"], json!(["query"]));
        assert_eq!(tool.parameters["additionalProperties"], json!(false));
        let mut keys: Vec<&str> = tool.parameters["properties"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["instruction", "query", "thinking", "urls"]);
        assert!(tool.execution_mode.is_none());
        assert!(tool.prepare_arguments.is_none());
    }

    #[test]
    fn gate_matches_is_extra_tool_enabled() {
        assert_eq!(is_search_tool_enabled(), google_search_tool_enabled());
        assert_eq!(
            is_search_tool_enabled(),
            extra_tool_enabled(ExtraTool::Search)
        );
    }

    #[tokio::test]
    async fn success_returns_the_formatted_brief() {
        let http = Arc::new(ScriptedHttp::new(vec![ScriptedHttp::json(&search_body())]));
        let tool =
            antigravity_search_tool_with_context(context(Some(FAKE_CREDENTIAL), http.clone()));
        let updates: Arc<Mutex<Vec<ToolUpdate>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&updates);
        let on_update: OnUpdate = Arc::new(move |update| {
            sink.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(update);
        });

        let result = (tool.execute)(
            "call-1",
            json!({ "query": "mimo 2.6", "thinking": true }),
            None,
            Some(on_update),
        )
        .await
        .unwrap();

        assert_eq!(http.calls().len(), 1);
        assert_eq!(result.content.len(), 1);
        let text = result.content[0].as_text().unwrap();
        assert!(
            text.contains("MiMo-V2.6 was released on September 22, 2026."),
            "{text}"
        );
        assert!(text.contains("### Sources"), "{text}");
        assert!(
            text.contains("[Example Article](https://example.com/article)"),
            "{text}"
        );
        assert!(
            text.contains("*Search queries: `mimo 2.6 release`, `mimo specs`*"),
            "{text}"
        );
        let updates = updates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(updates.len(), 1);
        assert_eq!(
            updates[0].content[0].as_text().unwrap(),
            "Searching Google for: \"mimo 2.6\"\u{2026}"
        );
    }

    #[tokio::test]
    async fn http_failure_surfaces_as_an_error() {
        let http = Arc::new(ScriptedHttp::new(vec![ScriptedHttp::status(
            401,
            r#"{"error":{"message":"API key not valid"}}"#,
        )]));
        let tool = antigravity_search_tool_with_context(context(Some(FAKE_CREDENTIAL), http));
        let error = (tool.execute)("call-1", json!({ "query": "q" }), None, None)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("API key not valid"), "{error}");
    }

    #[tokio::test]
    async fn query_is_required() {
        let http = Arc::new(ScriptedHttp::empty());
        let tool =
            antigravity_search_tool_with_context(context(Some(FAKE_CREDENTIAL), http.clone()));
        let error = (tool.execute)("call-1", json!({}), None, None)
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "Search query is required.");
        assert!(http.calls().is_empty());
    }

    #[tokio::test]
    async fn missing_credentials_report_the_login_hint() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let saved = std::env::var_os("ANTIGRAVITY_API_KEY");
        std::env::remove_var("ANTIGRAVITY_API_KEY");

        let dir = tempfile::tempdir().unwrap();
        let http = Arc::new(ScriptedHttp::empty());
        let tool = antigravity_search_tool_with_context(AntigravitySearchToolContext {
            api_key: None,
            agent_dir: Some(dir.path().to_path_buf()),
            http: http.clone(),
        });
        let error = (tool.execute)("call-1", json!({ "query": "q" }), None, None)
            .await
            .unwrap_err();

        if let Some(saved) = saved {
            std::env::set_var("ANTIGRAVITY_API_KEY", saved);
        }
        assert!(
            error
                .to_string()
                .contains("No Antigravity credentials. Run /login antigravity first."),
            "{error}"
        );
        assert!(http.calls().is_empty());
    }

    #[test]
    fn credential_normalization_requires_both_fields() {
        assert_eq!(
            complete_credential(r#"{"access":"tok","project":"proj"}"#).as_deref(),
            Some(r#"{"token":"tok","projectId":"proj"}"#)
        );
        assert_eq!(
            complete_credential(r#"{"access":"tok","project_id":"proj"}"#).as_deref(),
            Some(r#"{"token":"tok","projectId":"proj"}"#)
        );
        assert_eq!(
            complete_credential(r#"{"access":"tok","antigravityProjectId":"proj"}"#).as_deref(),
            Some(r#"{"token":"tok","projectId":"proj"}"#)
        );
        assert!(complete_credential("ya29.raw-token").is_none());
        assert!(complete_credential(r#"{"token":"tok"}"#).is_none());
    }

    #[tokio::test]
    async fn resolve_api_key_produces_token_and_project_id_json() {
        let dir = tempfile::tempdir().unwrap();
        let auth_path = dir.path().join("auth.json");
        std::fs::write(
            &auth_path,
            r#"{"antigravity":{"type":"oauth","access":"access-123","expires":0,"projectId":"proj-456"}}"#,
        )
        .unwrap();
        let context = AntigravitySearchToolContext {
            api_key: None,
            agent_dir: Some(dir.path().to_path_buf()),
            http: Arc::new(ScriptedHttp::empty()),
        };
        let resolved = resolve_search_api_key(&context).await.unwrap().unwrap();
        let value: Value = serde_json::from_str(&resolved).unwrap();
        assert_eq!(value["token"], "access-123");
        assert_eq!(value["projectId"], "proj-456");

        // Completing a raw bearer token with stored project id:
        let raw_context = AntigravitySearchToolContext {
            api_key: Some("raw-bearer-token".to_string()),
            agent_dir: Some(dir.path().to_path_buf()),
            http: Arc::new(ScriptedHttp::empty()),
        };
        let resolved_raw = resolve_search_api_key(&raw_context).await.unwrap().unwrap();
        let value_raw: Value = serde_json::from_str(&resolved_raw).unwrap();
        assert_eq!(value_raw["token"], "raw-bearer-token");
        assert_eq!(value_raw["projectId"], "proj-456");

        // JSON credential with token only, completed with stored project id:
        let token_json_context = AntigravitySearchToolContext {
            api_key: Some(r#"{"token":"extracted-token"}"#.to_string()),
            agent_dir: Some(dir.path().to_path_buf()),
            http: Arc::new(ScriptedHttp::empty()),
        };
        let resolved_json = resolve_search_api_key(&token_json_context)
            .await
            .unwrap()
            .unwrap();
        let value_json: Value = serde_json::from_str(&resolved_json).unwrap();
        assert_eq!(value_json["token"], "extracted-token");
        assert_eq!(value_json["projectId"], "proj-456");
    }

    #[tokio::test]
    async fn failover_to_next_account_on_quota_exhausted() {
        let dir = tempfile::tempdir().unwrap();
        let agent_dir_str = dir.path().to_string_lossy().into_owned();

        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = std::env::var_os("PRIME_AGENT_CODING_AGENT_DIR");
        std::env::set_var("PRIME_AGENT_CODING_AGENT_DIR", &agent_dir_str);

        // Populate multi-account store with Account 2 and Account 1.
        let acc2 = pa_ai::oauth::AccountCredentials {
            access: "access-token-2".to_string(),
            refresh: "refresh-token-2".to_string(),
            expires: i64::MAX / 2,
            project_id: Some("project-2".to_string()),
            email: Some("account2@example.com".to_string()),
        };
        let acc1 = pa_ai::oauth::AccountCredentials {
            access: "access-token-1".to_string(),
            refresh: "refresh-token-1".to_string(),
            expires: i64::MAX / 2,
            project_id: Some("project-1".to_string()),
            email: Some("account1@example.com".to_string()),
        };
        pa_ai::oauth::remember_account(&acc2);
        pa_ai::oauth::remember_account(&acc1);

        // auth.json holds Account 1 as active.
        let auth_path = dir.path().join("auth.json");
        std::fs::write(
            &auth_path,
            r#"{"antigravity":{"type":"oauth","access":"access-token-1","expires":0,"projectId":"project-1"}}"#,
        )
        .unwrap();

        // ScriptedHttp: Account 1 returns 429 quota exhausted, Account 2 returns 200 with search data.
        let http = Arc::new(ScriptedHttp::from_handler(|_url, headers| {
            let auth = headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
                .map(|(_, v)| v.as_str())
                .unwrap_or_default();
            if auth.contains("access-token-1") {
                ScriptedHttp::status(
                    429,
                    r#"{"error":{"code":429,"message":"You have exhausted your capacity on this model. Your quota will reset in 3 hours.","status":"RESOURCE_EXHAUSTED"}}"#,
                )
            } else if auth.contains("access-token-2") {
                ScriptedHttp::json(&search_body())
            } else {
                Err(format!("unexpected token in auth: {auth}"))
            }
        }));

        let tool = antigravity_search_tool_with_context(AntigravitySearchToolContext {
            api_key: None,
            agent_dir: Some(dir.path().to_path_buf()),
            http: http.clone(),
        });

        let result = (tool.execute)(
            "call-1",
            json!({ "query": "mimo 2.6", "thinking": true }),
            None,
            None,
        )
        .await;

        match previous {
            Some(prev) => std::env::set_var("PRIME_AGENT_CODING_AGENT_DIR", prev),
            None => std::env::remove_var("PRIME_AGENT_CODING_AGENT_DIR"),
        }

        let result = result.expect("should failover to account 2 and succeed");
        assert_eq!(result.content.len(), 1);
        let text = result.content[0].as_text().unwrap();
        assert!(text.contains("MiMo-V2.6 was released on September 22, 2026."));
        let calls = http.calls();
        assert!(
            calls.len() >= 2,
            "expected calls to account 1 (failing) and account 2 (succeeding), got {}",
            calls.len()
        );
    }
}
