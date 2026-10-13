//! The `generate_image` tool: Antigravity image generation exposed to the model.
//!
//! Ports the tool half of `pi-antigravity` `src/features/generate-image.ts` on top of the
//! already-ported `pa_ai::antigravity_image` surface: resolve the Antigravity
//! credential (`{token, projectId}`), call `generate_antigravity_image` (which validates the
//! model/aspect ratio, falls back across endpoints, and writes the images to disk), handle
//! account failover on HTTP 429 quota exhaustion, then return the saved paths plus every
//! image as an `image` content block.
//!
//! Only the model-facing definition lives here: the host (daemon/CLI) owns tool
//! registration and prompt-guideline wiring.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pa_ai::antigravity_image::{
    generate_antigravity_image, generate_image_tool_enabled, generate_image_tool_parameters,
    GenerateImageOptions, GENERATE_IMAGE_TOOL_DESCRIPTION, GENERATE_IMAGE_TOOL_LABEL,
    GENERATE_IMAGE_TOOL_NAME, GENERATE_IMAGE_TOOL_PROMPT_SNIPPET,
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

/// Runtime wiring for [`antigravity_image_tool`].
#[derive(Clone)]
pub struct AntigravityImageToolContext {
    /// Session working directory: the base for relative `path` values and the
    /// default `.pi/generated-images/` target.
    pub cwd: String,
    /// Explicit Antigravity credential. `None` resolves the signed-in account
    /// from the auth store on every call, so a later `/login` is picked up.
    pub api_key: Option<String>,
    /// Agent-state dir holding `auth.json`. `None` uses
    /// `pa_types::platform::agent_dir()` (`PRIME_AGENT_CODING_AGENT_DIR` when set,
    /// else `~/.prime/agent`).
    pub agent_dir: Option<PathBuf>,
    /// Transport; tests script it, production uses [`ReqwestAntigravityHttp`].
    pub http: Arc<dyn AntigravityHttp>,
}

impl Default for AntigravityImageToolContext {
    fn default() -> Self {
        Self {
            cwd: std::env::current_dir().map_or_else(
                |_| ".".to_string(),
                |dir| dir.to_string_lossy().into_owned(),
            ),
            api_key: None,
            agent_dir: None,
            http: Arc::new(ReqwestAntigravityHttp::new()),
        }
    }
}

/// Builds the `generate_image` definition with explicit wiring (cwd, credential,
/// transport). The host calls this when it knows the session cwd and agent dir.
#[must_use]
pub fn antigravity_image_tool_with_context(context: AntigravityImageToolContext) -> ToolDefinition {
    let execute: ExecuteFn = Arc::new(move |_tool_call_id, params, signal, on_update| {
        let context = context.clone();
        Box::pin(async move { execute_generate_image(context, params, signal, on_update).await })
    });
    ToolDefinition {
        name: GENERATE_IMAGE_TOOL_NAME.to_string(),
        label: GENERATE_IMAGE_TOOL_LABEL.to_string(),
        description: GENERATE_IMAGE_TOOL_DESCRIPTION.to_string(),
        prompt_snippet: GENERATE_IMAGE_TOOL_PROMPT_SNIPPET.to_string(),
        parameters: generate_image_tool_parameters(),
        execution_mode: None,
        prepare_arguments: None,
        execute,
    }
}

/// The `generate_image` tool with default wiring: process cwd, stored Antigravity
/// credential, real HTTP transport. Equivalent to
/// [`antigravity_image_tool_with_context`] with [`AntigravityImageToolContext::default`].
#[must_use]
pub fn antigravity_image_tool() -> ToolDefinition {
    antigravity_image_tool_with_context(AntigravityImageToolContext::default())
}

/// Whether `generate_image` should be registered
/// (`is_extra_tool_enabled("IMAGE")`: `ANTIGRAVITY_NO_EXTRA_TOOLS` and
/// `ANTIGRAVITY_NO_IMAGE_TOOL` both disable it).
#[must_use]
pub fn is_image_tool_enabled() -> bool {
    generate_image_tool_enabled()
}

async fn execute_generate_image(
    context: AntigravityImageToolContext,
    params: serde_json::Value,
    signal: Option<AbortSignal>,
    on_update: Option<OnUpdate>,
) -> anyhow::Result<ToolExecutionResult> {
    let api_key = resolve_image_api_key(&context).await?;
    let Some(api_key) = api_key.filter(|key| !key.trim().is_empty()) else {
        anyhow::bail!("No Antigravity credentials. Run /login antigravity first.");
    };

    if let Some(on_update) = on_update {
        on_update(ToolUpdate {
            content: vec![ToolContentBlock::text("Generating image\u{2026}")],
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

    let result = loop {
        let options = GenerateImageOptions {
            api_key: current_api_key.clone(),
            cwd: context.cwd.clone(),
            prompt: string_param(&params, "prompt").unwrap_or_default(),
            aspect_ratio: string_param(&params, "aspectRatio"),
            model: string_param(&params, "model"),
            path: string_param(&params, "path"),
            signal: signal.clone(),
            input_images: Vec::new(),
        };

        match generate_antigravity_image(&options, context.http.as_ref()).await {
            Ok(result) => break result,
            Err(error) => {
                if is_image_quota_exhausted(&error) {
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
                        "Antigravity image quota exhausted for all accounts: {}",
                        safe_error(&error)
                    );
                }
                anyhow::bail!("{}", safe_error(&error));
            }
        }
    };

    let notes = result.text.join(" ").trim().to_string();
    let suffix = if notes.is_empty() {
        String::new()
    } else {
        format!(". {notes}")
    };
    let text = format!("Saved image to {}{}", result.saved_paths.join(", "), suffix);
    let mut content = vec![ToolContentBlock::text(text)];
    for image in &result.images {
        content.push(ToolContentBlock::Image {
            data: image.data.clone(),
            mime_type: image.mime_type.clone(),
        });
    }
    Ok(ToolExecutionResult {
        content,
        details: Some(json!({ "model": result.model, "savedPaths": result.saved_paths })),
        is_error: false,
    })
}

/// Resolves the credential the image request needs: a `{token, projectId}` JSON string. The
/// explicit context key wins; otherwise the auth store (refresh-aware) or the provider env var
/// supplies the token, and the project id comes from `auth.json`, then model discovery, then the
/// deterministic fallback.
async fn resolve_image_api_key(
    context: &AntigravityImageToolContext,
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

/// Whether an image generation error indicates that the account's quota has been exhausted.
pub(crate) fn is_image_quota_exhausted(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    // Standalone 429 (word boundary) — matches "HTTP 429", "(429)", but not "14293".
    let has_429 = lower
        .split(|c: char| !c.is_ascii_digit())
        .any(|token| token == "429");
    has_429
        || lower.contains("resource_exhausted")
        || lower.contains("resource exhausted")
        || lower.contains("resource has been exhausted")
        || lower.contains("exhausted your capacity")
        || lower.contains("quota")
        || lower.contains("too many requests")
        || lower.contains("rate limit")
}

/// A string tool parameter, or `None` when absent or not a string.
fn string_param(params: &serde_json::Value, key: &str) -> Option<String> {
    params
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::VecDeque;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Mutex;

    use base64::Engine;
    use pa_ai::antigravity_types::AntigravityHttpResponse;
    use tokio_util::sync::CancellationToken;

    type HttpHandler = Box<
        dyn Fn(&str, &[(String, String)]) -> Result<AntigravityHttpResponse, String> + Send + Sync,
    >;

    /// A scripted transport: serves a queue of responses or calls a handler, and records the calls.
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
            self.calls.lock().unwrap().clone()
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
            self.calls.lock().unwrap().push(url.to_string());
            if let Some(handler) = &self.handler {
                let response = handler(url, &headers);
                return Box::pin(async move { response });
            }
            let mut script = self.responses.lock().unwrap();
            let response = if script.is_empty() {
                Err("no scripted response".to_string())
            } else if script.len() == 1 {
                // A single scripted response replays for every retry attempt.
                script[0].clone()
            } else {
                script.pop_front().unwrap()
            };
            drop(script);
            Box::pin(async move { response })
        }
    }

    fn context(
        cwd: &std::path::Path,
        http: Arc<dyn AntigravityHttp>,
    ) -> AntigravityImageToolContext {
        AntigravityImageToolContext {
            cwd: cwd.to_string_lossy().into_owned(),
            api_key: Some(r#"{"token":"t","projectId":"p"}"#.to_string()),
            agent_dir: None,
            http,
        }
    }

    fn image_body() -> String {
        let data = base64::engine::general_purpose::STANDARD.encode([0x89, 0x50, 0x4e, 0x47]);
        json!({
            "response": { "candidates": [{ "content": { "parts": [
                { "text": "done" },
                { "inlineData": { "mimeType": "image/png", "data": data } },
            ] } }] }
        })
        .to_string()
    }

    #[test]
    fn tool_spec_matches_the_extension() {
        let tool = antigravity_image_tool();
        assert_eq!(tool.name, "generate_image");
        assert_eq!(tool.label, "Generate image");
        assert!(tool
            .description
            .starts_with("Generate an image via Antigravity"));
        assert_eq!(
            tool.prompt_snippet,
            "Generate images via Antigravity OAuth (Gemini image models)"
        );
        assert_eq!(tool.parameters["required"], json!(["prompt"]));
        let mut keys: Vec<&str> = tool.parameters["properties"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["aspectRatio", "model", "path", "prompt"]);
        assert!(tool.execution_mode.is_none());
        assert!(tool.prepare_arguments.is_none());
    }

    /// Environment mutations are process-global: the gate test holds this lock
    /// and restores the previous values.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_env(names: &[(&str, Option<&str>)], body: impl FnOnce()) {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous: Vec<(&str, Option<std::ffi::OsString>)> = names
            .iter()
            .map(|(name, _)| (*name, std::env::var_os(name)))
            .collect();
        for (name, value) in names {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
        body();
        for (name, value) in previous {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }

    #[test]
    fn gate_honors_the_extra_tool_toggles() {
        with_env(
            &[
                ("ANTIGRAVITY_NO_EXTRA_TOOLS", None),
                ("ANTIGRAVITY_NO_IMAGE_TOOL", None),
            ],
            || assert!(is_image_tool_enabled()),
        );
        with_env(&[("ANTIGRAVITY_NO_IMAGE_TOOL", Some("1"))], || {
            assert!(!is_image_tool_enabled());
        });
        with_env(
            &[
                ("ANTIGRAVITY_NO_IMAGE_TOOL", None),
                ("ANTIGRAVITY_NO_EXTRA_TOOLS", Some("1")),
            ],
            || assert!(!is_image_tool_enabled()),
        );
        with_env(
            &[
                ("ANTIGRAVITY_NO_EXTRA_TOOLS", None),
                ("ANTIGRAVITY_NO_IMAGE_TOOL", Some("yes")),
            ],
            || assert!(is_image_tool_enabled()),
        );
    }

    #[tokio::test]
    async fn success_saves_the_image_and_returns_image_content() {
        let dir = tempfile::tempdir().unwrap();
        let http = Arc::new(ScriptedHttp::new(vec![ScriptedHttp::json(&image_body())]));
        let tool = antigravity_image_tool_with_context(context(dir.path(), http.clone()));
        let updates: Arc<Mutex<Vec<ToolUpdate>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&updates);
        let on_update: OnUpdate = Arc::new(move |update| sink.lock().unwrap().push(update));

        let result = (tool.execute)(
            "call-1",
            json!({ "prompt": "a cat" }),
            None,
            Some(on_update),
        )
        .await
        .unwrap();

        assert_eq!(http.calls().len(), 1);
        let text = result.content[0].as_text().unwrap();
        assert!(text.starts_with("Saved image to "), "{text}");
        assert!(text.contains(".pi/generated-images/"), "{text}");
        assert!(text.ends_with(". done"), "{text}");
        assert_eq!(result.content.len(), 2);
        let ToolContentBlock::Image { data, mime_type } = &result.content[1] else {
            panic!("expected an image block");
        };
        assert_eq!(mime_type.as_str(), "image/png");
        assert_eq!(
            data.as_str(),
            base64::engine::general_purpose::STANDARD
                .encode([0x89, 0x50, 0x4e, 0x47])
                .as_str()
        );
        let saved = result.details.as_ref().unwrap()["savedPaths"][0]
            .as_str()
            .unwrap();
        assert!(std::path::Path::new(saved).exists(), "missing {saved}");
        assert_eq!(
            result.details.as_ref().unwrap()["model"],
            json!("gemini-3.1-flash-image")
        );
        assert_eq!(updates.lock().unwrap().len(), 1);
        assert_eq!(
            updates.lock().unwrap()[0].content[0].as_text().unwrap(),
            "Generating image\u{2026}"
        );
    }

    #[tokio::test]
    async fn http_failure_surfaces_as_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let http = Arc::new(ScriptedHttp::new(vec![ScriptedHttp::status(500, "boom")]));
        let tool = antigravity_image_tool_with_context(context(dir.path(), http));
        let error = (tool.execute)("call-1", json!({ "prompt": "a cat" }), None, None)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("boom"), "{error}");
    }

    #[tokio::test]
    async fn missing_credentials_report_the_login_hint() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let saved_key = std::env::var_os("ANTIGRAVITY_API_KEY");
        std::env::remove_var("ANTIGRAVITY_API_KEY");

        let dir = tempfile::tempdir().unwrap();
        let scripted = Arc::new(ScriptedHttp::new(Vec::new()));
        let http: Arc<dyn AntigravityHttp> = scripted.clone();
        let tool = antigravity_image_tool_with_context(AntigravityImageToolContext {
            cwd: dir.path().to_string_lossy().into_owned(),
            api_key: None,
            agent_dir: Some(dir.path().to_path_buf()),
            http,
        });
        let error = (tool.execute)("call-1", json!({ "prompt": "a cat" }), None, None)
            .await
            .unwrap_err();

        if let Some(saved) = saved_key {
            std::env::set_var("ANTIGRAVITY_API_KEY", saved);
        }

        assert!(
            error
                .to_string()
                .contains("No Antigravity credentials. Run /login antigravity first."),
            "{error}"
        );
        assert!(scripted.calls().is_empty());
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
        let context = AntigravityImageToolContext {
            cwd: ".".to_string(),
            api_key: None,
            agent_dir: Some(dir.path().to_path_buf()),
            http: Arc::new(ScriptedHttp::new(Vec::new())),
        };
        let resolved = resolve_image_api_key(&context).await.unwrap().unwrap();
        let value: Value = serde_json::from_str(&resolved).unwrap();
        assert_eq!(value["token"], "access-123");
        assert_eq!(value["projectId"], "proj-456");

        // Completing a raw bearer token with stored project id:
        let raw_context = AntigravityImageToolContext {
            cwd: ".".to_string(),
            api_key: Some("raw-bearer-token".to_string()),
            agent_dir: Some(dir.path().to_path_buf()),
            http: Arc::new(ScriptedHttp::new(Vec::new())),
        };
        let resolved_raw = resolve_image_api_key(&raw_context).await.unwrap().unwrap();
        let value_raw: Value = serde_json::from_str(&resolved_raw).unwrap();
        assert_eq!(value_raw["token"], "raw-bearer-token");
        assert_eq!(value_raw["projectId"], "proj-456");

        // JSON credential with token only, completed with stored project id:
        let token_json_context = AntigravityImageToolContext {
            cwd: ".".to_string(),
            api_key: Some(r#"{"token":"extracted-token"}"#.to_string()),
            agent_dir: Some(dir.path().to_path_buf()),
            http: Arc::new(ScriptedHttp::new(Vec::new())),
        };
        let resolved_json = resolve_image_api_key(&token_json_context)
            .await
            .unwrap()
            .unwrap();
        let value_json: Value = serde_json::from_str(&resolved_json).unwrap();
        assert_eq!(value_json["token"], "extracted-token");
        assert_eq!(value_json["projectId"], "proj-456");
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

    #[test]
    fn quota_exhausted_matcher_identifies_rate_limits() {
        assert!(is_image_quota_exhausted("HTTP 429: Rate limit exceeded"));
        assert!(is_image_quota_exhausted("HTTP 429"));
        assert!(is_image_quota_exhausted("(429)"));
        assert!(is_image_quota_exhausted("Too Many Requests"));
        assert!(is_image_quota_exhausted("RESOURCE_EXHAUSTED"));
        assert!(is_image_quota_exhausted("resource has been exhausted"));
        assert!(is_image_quota_exhausted("exhausted your capacity"));
        assert!(is_image_quota_exhausted("Quota exceeded"));
        assert!(is_image_quota_exhausted("rate limit exceeded"));

        assert!(!is_image_quota_exhausted("saved to image-14293.png"));
        assert!(!is_image_quota_exhausted("prompt limit reached"));
        assert!(!is_image_quota_exhausted("unknown failure"));
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

        // ScriptedHttp: Account 1 returns 429 quota exhausted, Account 2 returns 200 with image data.
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
                ScriptedHttp::json(&image_body())
            } else {
                Err(format!("unexpected token in auth: {auth}"))
            }
        }));

        let tool = antigravity_image_tool_with_context(AntigravityImageToolContext {
            cwd: dir.path().to_string_lossy().into_owned(),
            api_key: None,
            agent_dir: Some(dir.path().to_path_buf()),
            http: http.clone(),
        });

        let result = (tool.execute)(
            "call-1",
            json!({ "prompt": "a mountain landscape", "model": "gemini-3.1-flash-image" }),
            None,
            None,
        )
        .await;

        match previous {
            Some(prev) => std::env::set_var("PRIME_AGENT_CODING_AGENT_DIR", prev),
            None => std::env::remove_var("PRIME_AGENT_CODING_AGENT_DIR"),
        }

        let result = result.expect("should failover to account 2 and succeed");
        assert_eq!(result.content.len(), 2);
        let text = result.content[0].as_text().unwrap();
        assert!(text.starts_with("Saved image to "), "{text}");
        let ToolContentBlock::Image { data, mime_type } = &result.content[1] else {
            panic!("expected an image block");
        };
        assert_eq!(mime_type.as_str(), "image/png");
        assert_eq!(
            data.as_str(),
            base64::engine::general_purpose::STANDARD
                .encode([0x89, 0x50, 0x4e, 0x47])
                .as_str()
        );
        let calls = http.calls();
        assert!(
            calls.len() >= 2,
            "expected calls to account 1 (failing) and account 2 (succeeding), got {}",
            calls.len()
        );
    }
}
