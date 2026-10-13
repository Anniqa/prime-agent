//! Google Antigravity ("Cloud Code Assist") streaming provider: OAuth-backed `generateContent`
//! streaming over `v1internal:streamGenerateContent`, endpoint fallback, project-id resolution, and
//! the CLI wire fingerprint.
//!
//! Ported from the `pi-antigravity` extension (`src/client/client.ts`, `src/stream/stream.ts`).
//! The Gemini part/usage/finish-reason handling is shared with the Google provider through
//! [`crate::providers::google_stream`] and [`crate::providers::google_shared`]; the model/runtime
//! tables live in [`crate::providers::antigravity_models`]; project discovery and the OAuth
//! credential live in [`crate::oauth`]. This module is the transport: credentials, the
//! `{project, model, userAgent, requestType, requestId, request}` envelope, runtime-model fallback,
//! and endpoint fallback.

use std::collections::HashMap;

use rand::RngCore;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::env_api_keys::get_env_api_key;
use crate::event_stream::{
    create_assistant_message_event_stream, AssistantMessageEvent, AssistantMessageEventStream,
    AssistantMessageEventWriter,
};
use crate::models::clamp_thinking_level;
use crate::oauth::{
    default_project_id, failover_to_next_account, resolve_project_id, sync_current_auth,
    ReqwestProviderHttp,
};
use crate::providers::antigravity_models::{
    fallback_runtime_model, max_output_tokens, request_model_id, thinking_config,
};
use crate::providers::google::GoogleToolChoice;
use crate::providers::google_shared::{convert_messages, convert_tools, map_tool_choice};
use crate::providers::google_stream::GoogleStreamState;
use crate::providers::simple_options::build_base_options;
use crate::registry::Provider;
use crate::types::{
    done_reason, error_reason, AssistantMessage, Context, Model, ModelThinkingLevel,
    SimpleStreamOptions, StopReason, StreamOptions, Usage,
};
use crate::utils_inner::diagnostics::now_ms;
use crate::utils_inner::http::{send, HttpResponse, RequestOptions, Transport};
use crate::utils_inner::json_parse::parse_json_with_repair;
use crate::utils_inner::sanitize_unicode::sanitize_surrogates;
use crate::utils_inner::sse::SseDecoder;
use crate::utils_inner::stream_failure::{
    format_stream_failure_message, record_stream_failure, stream_failure_from_stop_reason,
    ConnectionErrorProfile, ProviderError,
};

// Re-exported from the model/wire table module so consumers have a single source of truth.
pub use crate::providers::antigravity_models::{
    ANTIGRAVITY_ENDPOINT_FALLBACKS, ANTIGRAVITY_USER_AGENT, API_ANTIGRAVITY,
    DEFAULT_ANTIGRAVITY_BASE_URL, REQUEST_TYPE_AGENT,
};

/// Default HTTP `User-Agent`, matching the pure Antigravity CLI wire fingerprint
/// (`oauth::antigravity::DEFAULT_USER_AGENT`, not re-exported from that private module).
pub const ANTIGRAVITY_CLI_USER_AGENT: &str = "antigravity/cli/1.2.4 (aidev_client; os_type=linux; arch=amd64; cl=982146307; auth_method=consumer)";

const ANTIGRAVITY_SYSTEM_INSTRUCTION: &str = "You are Antigravity, a powerful agentic AI coding assistant designed by Google DeepMind. You are pair programming with a user to solve coding tasks. Be concise, practical, and tool-aware.";

const ANTIGRAVITY_NO_PREAMBLE_INSTRUCTION: &str = "CRITICAL: NEVER output rule checks, formatting guidelines, constraint checklists (e.g. \"No emdashes\"), or your thinking/personality preambles in the final response. Output only the final response.";

/// Provider-native options: the shared base plus the reasoning effort the runtime model and
/// thinking config are derived from (the extension's `AntigravityStreamOptions`).
#[derive(Clone, Default)]
pub struct AntigravityOptions {
    pub base: StreamOptions,
    /// `None` is treated as `Off` (the extension's `opts.reasoning ?? "off"`).
    pub reasoning: Option<ModelThinkingLevel>,
    pub tool_choice: Option<GoogleToolChoice>,
}

impl AntigravityOptions {
    pub fn from_base(base: StreamOptions) -> Self {
        Self {
            base,
            reasoning: None,
            tool_choice: None,
        }
    }
}

/// Read `ANTIGRAVITY_<NAME>`, then the legacy `NOAGY_<NAME>`; empty values fall through.
#[must_use]
pub fn antigravity_env(name: &str) -> Option<String> {
    let read = |prefix: &str| {
        std::env::var(format!("{prefix}{name}"))
            .ok()
            .filter(|value| !value.is_empty())
    };
    read("ANTIGRAVITY_").or_else(|| read("NOAGY_"))
}

/// `assertSafeApiBaseUrl` (security.ts): only https `*.googleapis.com` bases without credentials
/// are accepted, with a trailing slash stripped. Returns `None` for anything else.
fn safe_api_base_url(raw: &str) -> Option<String> {
    let url = url::Url::parse(raw).ok()?;
    if url.scheme() != "https" {
        return None;
    }
    if !url.username().is_empty() || url.password().is_some() {
        return None;
    }
    let host = url.host_str()?.to_lowercase();
    let allowed = host == "googleapis.com"
        || host.ends_with(".googleapis.com")
        || host.ends_with(".sandbox.googleapis.com");
    if !allowed {
        return None;
    }
    let origin = url.origin().ascii_serialization();
    let path = url.path().trim_end_matches('/');
    Some(if path.is_empty() {
        origin
    } else {
        format!("{origin}{path}")
    })
}

/// Candidate base URLs: an explicit `ANTIGRAVITY_BASE_URL` (validated) or the built-in endpoint
/// fallback list (`endpointCandidates`).
#[must_use]
pub fn endpoint_candidates() -> Vec<String> {
    if let Some(explicit) = antigravity_env("BASE_URL") {
        let explicit = explicit.trim();
        if !explicit.is_empty() {
            if let Some(safe) = safe_api_base_url(explicit) {
                return vec![safe];
            }
        }
    }
    ANTIGRAVITY_ENDPOINT_FALLBACKS
        .iter()
        .map(|endpoint| (*endpoint).to_string())
        .collect()
}

/// CLI wire headers for one Antigravity request (`antigravityHeaders`); never an `Accept` header.
#[must_use]
pub fn antigravity_headers(token: &str) -> Vec<(String, String)> {
    vec![
        ("Authorization".to_string(), format!("Bearer {token}")),
        ("Content-Type".to_string(), "application/json".to_string()),
        (
            "User-Agent".to_string(),
            antigravity_env("USER_AGENT").unwrap_or_else(|| ANTIGRAVITY_CLI_USER_AGENT.to_string()),
        ),
    ]
}

/// Parsed Antigravity credentials: the bearer token plus an optional project id.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AntigravityApiKey {
    token: String,
    project_id: Option<String>,
}

/// `parseApiKey`: accept a raw bearer token or a JSON credential carrying `token`/`access`/
/// `access_token` (and optionally a project id).
fn parse_api_key(raw: &str) -> Option<AntigravityApiKey> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with('{') {
        if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
            if value.is_object() {
                let token = ["token", "access", "access_token"]
                    .iter()
                    .find_map(|key| value.get(*key).and_then(Value::as_str))
                    .filter(|token| !token.is_empty());
                if let Some(token) = token {
                    return Some(AntigravityApiKey {
                        token: token.to_string(),
                        project_id: extract_project_id(&value),
                    });
                }
                return None;
            }
        }
    }
    Some(AntigravityApiKey {
        token: trimmed.to_string(),
        project_id: None,
    })
}

/// `extractProjectId`: read a project id from the direct fields, a nested `id`, or the project
/// lists of a discovery payload.
fn extract_project_id(value: &Value) -> Option<String> {
    let direct = [
        "antigravityProjectId",
        "projectId",
        "backendProjectId",
        "userDefinedCloudaicompanionProject",
        "cloudaicompanionProject",
        "project",
    ]
    .iter()
    .find_map(|key| value.get(*key));
    if let Some(direct) = direct {
        if let Some(id) = direct.as_str().filter(|id| !id.is_empty()) {
            return Some(id.to_string());
        }
        if let Some(id) = direct
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        {
            return Some(id.to_string());
        }
    }
    for key in ["projects", "projectIds", "cloudaicompanionProjects"] {
        if let Some(items) = value.get(key).and_then(Value::as_array) {
            for item in items {
                if let Some(found) = extract_project_id(item) {
                    return Some(found);
                }
                if let Some(id) = item.as_str().filter(|id| !id.is_empty()) {
                    return Some(id.to_string());
                }
            }
        }
    }
    None
}

/// The stored credential fields read from `~/.prime/agent/auth.json` (`antigravity` entry).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct StoredAntigravityCredential {
    token: Option<String>,
    project_id: Option<String>,
    email: Option<String>,
}

/// Read the token/project/email fields of one stored `antigravity` credential entry.
fn credential_from_entry(entry: &Value) -> StoredAntigravityCredential {
    let token = ["access", "token", "access_token"]
        .iter()
        .find_map(|key| entry.get(*key).and_then(Value::as_str))
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let project_id = ["projectId", "project"]
        .iter()
        .find_map(|key| entry.get(*key).and_then(Value::as_str))
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let email = ["email", "accountId"]
        .iter()
        .find_map(|key| entry.get(*key).and_then(Value::as_str))
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    StoredAntigravityCredential {
        token,
        project_id,
        email,
    }
}

/// The `antigravity` entry of `auth.json` in the agent directory, when present.
fn read_credential_entry() -> Option<Value> {
    let path = pa_types::platform::agent_dir()?.join("auth.json");
    let text = std::fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    value.get("antigravity").cloned()
}

/// Merge the credential sources: an explicit/JSON `api_key`, the provider env var, and the stored
/// credential file. An explicit key's `projectId` wins over a stored file's project id.
fn merge_credentials(
    raw_key: Option<&str>,
    env_token: Option<String>,
    stored: Option<StoredAntigravityCredential>,
) -> StoredAntigravityCredential {
    let parsed = raw_key.and_then(parse_api_key);
    let token = parsed
        .as_ref()
        .map(|key| key.token.clone())
        .filter(|token| !token.trim().is_empty())
        .or_else(|| env_token.filter(|token| !token.trim().is_empty()))
        .or_else(|| stored.as_ref().and_then(|stored| stored.token.clone()));
    let project_id = parsed
        .and_then(|key| key.project_id)
        .or_else(|| stored.as_ref().and_then(|stored| stored.project_id.clone()));
    let email = stored.and_then(|stored| stored.email);
    StoredAntigravityCredential {
        token,
        project_id,
        email,
    }
}

/// The resolved streaming credential: a bearer token plus the Cloud Code Assist project id.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedAntigravityCredential {
    token: String,
    project: String,
}

/// Resolve the token and project id: the stored credential (or explicit key) first, then
/// [`resolve_project_id`] discovery, then [`default_project_id`] seeded by the account email. The
/// credential `auth.json` holds is also synced into the multi-account store, so every login becomes
/// a failover candidate.
async fn resolve_credentials(
    model: &Model,
    options: &AntigravityOptions,
) -> ResolvedAntigravityCredential {
    let raw_key = options
        .base
        .api_key
        .clone()
        .filter(|key| !key.trim().is_empty());
    let env_token = get_env_api_key(&model.provider);
    let stored = read_credential_entry().as_ref().map(credential_from_entry);
    let merged = merge_credentials(raw_key.as_deref(), env_token, stored);
    let StoredAntigravityCredential {
        token,
        project_id,
        email,
    } = merged;
    let token = token.unwrap_or_default();
    // Keep the account store in step with whatever the login last wrote.
    sync_current_auth();
    let project = project_for_credentials(&token, project_id, email.as_deref()).await;
    ResolvedAntigravityCredential { token, project }
}

/// The Cloud Code Assist project for one credential: the carried project id, else
/// [`resolve_project_id`] discovery, else [`default_project_id`] seeded by the account email.
async fn project_for_credentials(
    token: &str,
    project_id: Option<String>,
    email: Option<&str>,
) -> String {
    if let Some(project) = project_id {
        return project;
    }
    if token.is_empty() {
        return default_project_id(email.unwrap_or("antigravity-default"));
    }
    let http = ReqwestProviderHttp::new();
    resolve_project_id(&http, token)
        .await
        .unwrap_or_else(|| default_project_id(email.unwrap_or("antigravity-default")))
}

/// A v4-shaped UUID from random bytes.
fn random_uuid() -> String {
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format_uuid(&bytes)
}

/// A deterministic UUID-shaped id from a seed (the reference uses a v5 UUID over SHA-1; this port
/// hashes with SHA-256, which is available without a new dependency).
fn stable_uuid(seed: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(seed.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format_uuid(&bytes)
}

fn format_uuid(bytes: &[u8; 16]) -> String {
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// Build the envelope `requestId` (`agent/<conversation>/<timestamp>/<trajectory>/<step>`).
fn build_request_id(session_id: Option<&str>, step: usize) -> String {
    let (conversation_id, trajectory_id) =
        match session_id.map(str::trim).filter(|id| !id.is_empty()) {
            Some(seed) => (
                stable_uuid(&format!("antigravity:conv:session:{seed}")),
                stable_uuid(&format!("antigravity:traj:session:{seed}")),
            ),
            None => (random_uuid(), random_uuid()),
        };
    format!(
        "agent/{conversation_id}/{}/{trajectory_id}/{step}",
        now_ms()
    )
}

/// Build the inner `generateContent` request (`buildRequest`): contents, system instruction,
/// generation config (thinking + output cap), and tools.
fn build_request_body(
    model: &Model,
    context: &Context,
    options: &AntigravityOptions,
    runtime_model: &str,
) -> Value {
    let mut body = Map::new();
    body.insert(
        "contents".to_string(),
        json!(convert_messages(model, context)),
    );

    let system_text = context
        .system_prompt
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty());
    let system_parts: Vec<Value> = match system_text {
        Some(text) => vec![json!({ "text": sanitize_surrogates(text) })],
        None => vec![
            json!({ "text": ANTIGRAVITY_SYSTEM_INSTRUCTION }),
            json!({ "text": ANTIGRAVITY_NO_PREAMBLE_INSTRUCTION }),
        ],
    };
    body.insert(
        "systemInstruction".to_string(),
        json!({ "role": "user", "parts": system_parts }),
    );

    let effort = options.reasoning.unwrap_or(ModelThinkingLevel::Off);
    let mut generation_config = Map::new();
    if let Some(temperature) = options.base.temperature {
        generation_config.insert("temperature".to_string(), json!(temperature));
    }
    let max_allowed = max_output_tokens(&model.id, Some(runtime_model));
    let requested = options.base.max_tokens.unwrap_or_else(|| {
        if model.max_tokens == 0 {
            max_allowed
        } else {
            model.max_tokens
        }
    });
    generation_config.insert(
        "maxOutputTokens".to_string(),
        json!(requested.min(max_allowed)),
    );
    if let Some(thinking) = thinking_config(runtime_model, effort) {
        // Cloud Code rejects `thinkingBudget: 0` with HTTP 400 ("Request contains an invalid
        // argument"), so omit the budget when it is zero. `includeThoughts: false` alone is
        // accepted and stays closest to the extension's wire config.
        let thinking_wire = if thinking.thinking_budget == 0 {
            json!({ "includeThoughts": thinking.include_thoughts })
        } else {
            json!({
                "includeThoughts": thinking.include_thoughts,
                "thinkingBudget": thinking.thinking_budget,
            })
        };
        generation_config.insert("thinkingConfig".to_string(), thinking_wire);
    }
    body.insert(
        "generationConfig".to_string(),
        Value::Object(generation_config),
    );

    let has_tools = context
        .tools
        .as_ref()
        .is_some_and(|tools| !tools.is_empty());
    if has_tools {
        if let Some(tools) = &context.tools {
            let is_claude = model.id.starts_with("claude-") || runtime_model.starts_with("claude-");
            let use_legacy = is_claude || model.id.starts_with("gpt-oss-");
            if let Some(converted) = convert_tools(tools, use_legacy) {
                body.insert("tools".to_string(), json!(converted));
            }
        }
        if let Some(choice) = options.tool_choice {
            if choice != GoogleToolChoice::Auto {
                body.insert(
                    "toolConfig".to_string(),
                    json!({
                        "functionCallingConfig": { "mode": map_tool_choice(choice.as_str()) },
                    }),
                );
            }
        }
    }

    Value::Object(body)
}

/// Wrap the inner request in the Cloud Code Assist envelope (`model` is the runtime id).
fn build_envelope(runtime_model: &str, request: &Value, project: &str, request_id: &str) -> Value {
    json!({
        "project": project,
        "model": runtime_model,
        "userAgent": ANTIGRAVITY_USER_AGENT,
        "requestType": REQUEST_TYPE_AGENT,
        "requestId": request_id,
        "request": request,
    })
}

/// Cloud Code Assist wraps each generateContent response in `response`.
fn response_payload(parsed: &Value) -> &Value {
    parsed.get("response").unwrap_or(parsed)
}

/// Convert an Antigravity SSE error frame into a provider error.
fn chunk_error(error: &Value) -> ProviderError {
    let status = error
        .get("code")
        .and_then(Value::as_u64)
        .and_then(|code| u16::try_from(code).ok());
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("Antigravity stream error");
    match status {
        Some(status) => ProviderError::from_http_status_body(
            status,
            &json!({ "error": error.clone() }).to_string(),
            HashMap::new(),
        ),
        None => ProviderError::Message(message.to_string()),
    }
}

/// Statuses worth trying on the next endpoint (`stream.ts` endpoint loop).
fn retryable_status(status: u16) -> bool {
    matches!(status, 403 | 404 | 429 | 500 | 502 | 503 | 504)
}

/// Whether a 429 is a hard quota wall that should switch accounts. Mirrors the TS
/// `friendlyAntigravityError` hard-limit branch: an explicit reset hint, an individual-quota
/// message, or a quota phrase that is not rate limiting.
fn is_quota_wall(status: u16, body: &str) -> bool {
    if status != 429 {
        return false;
    }
    let text = body.to_lowercase();
    if text.contains("individual quota reached") {
        return true;
    }
    if text.contains("reset in") || text.contains("resets in") {
        return true;
    }
    let rate_limited =
        text.contains("rate limit") || text.contains("rate-limit") || text.contains("ratelimit");
    !rate_limited
        && [
            "quota exceeded",
            "exceeded your",
            "limit reached",
            "reached your",
            "daily limit",
        ]
        .iter()
        .any(|phrase| text.contains(phrase))
}

/// On a hard quota wall, switch to the next stored account not yet tried (the TS
/// `failoverToNextAccount` branch): update `credentials` in place, record the tried token, and
/// return `true`. Returns `false` for any other status or when the store has no untried account.
async fn switch_account_on_quota(
    status: u16,
    body: &str,
    credentials: &mut ResolvedAntigravityCredential,
    tried_access_tokens: &mut Vec<String>,
) -> bool {
    if !is_quota_wall(status, body) {
        return false;
    }
    let http = ReqwestProviderHttp::new();
    let Some(next) = failover_to_next_account(&http, tried_access_tokens.as_slice()).await else {
        return false;
    };
    let project =
        project_for_credentials(&next.token, next.project_id, next.email.as_deref()).await;
    tried_access_tokens.push(next.token.clone());
    credentials.token = next.token;
    credentials.project = project;
    true
}

/// Base URLs to try: a model `baseUrl` (plus its known counterpart) or the built-in list.
fn stream_endpoint_bases(model_base_url: &str) -> Vec<String> {
    let base = model_base_url.trim().trim_end_matches('/');
    if base.is_empty() {
        return endpoint_candidates();
    }
    let mut bases = vec![base.to_string()];
    if base.contains("daily-cloudcode-pa") {
        // The production endpoint is the last entry of the fallback list.
        bases.push(ANTIGRAVITY_ENDPOINT_FALLBACKS[2].to_string());
    } else if base.contains("cloudcode-pa") {
        bases.push(DEFAULT_ANTIGRAVITY_BASE_URL.to_string());
    }
    bases
}

/// Request headers: the CLI fingerprint plus `model.headers`/`options.headers` overrides
/// (`options` wins).
fn request_headers(
    token: &str,
    model: &Model,
    options: &AntigravityOptions,
) -> Vec<(String, String)> {
    let mut headers = antigravity_headers(token);
    if let Some(model_headers) = &model.headers {
        for (name, value) in model_headers {
            headers.retain(|(existing, _)| !existing.eq_ignore_ascii_case(name));
            headers.push((name.clone(), value.clone()));
        }
    }
    if let Some(option_headers) = &options.base.headers {
        for (name, value) in option_headers {
            headers.retain(|(existing, _)| !existing.eq_ignore_ascii_case(name));
            headers.push((name.clone(), value.clone()));
        }
    }
    headers
}

/// Parse one SSE data payload, surface error frames, and feed the shared Gemini chunk handler.
fn process_sse(
    data: &str,
    state: &mut GoogleStreamState,
    model: &Model,
    output: &mut AssistantMessage,
    writer: &AssistantMessageEventWriter,
) -> Result<(), ProviderError> {
    if data.trim().is_empty() {
        return Ok(());
    }
    let parsed = parse_json_with_repair(data).map_err(|error| {
        ProviderError::Message(format!("Could not parse Antigravity SSE chunk: {error}"))
    })?;
    if let Some(error) = parsed.get("error") {
        if !error.is_null() {
            return Err(chunk_error(error));
        }
    }
    if let Some(response) = parsed.get("response") {
        if let Some(error) = response.get("error") {
            if !error.is_null() {
                return Err(chunk_error(error));
            }
        }
    }
    state
        .handle_chunk(response_payload(&parsed), model, output, writer)
        .map_err(ProviderError::Message)
}

/// Consume the SSE body through the shared Gemini stream state.
async fn stream_body(
    response: &mut HttpResponse,
    model: &Model,
    output: &mut AssistantMessage,
    writer: &AssistantMessageEventWriter,
) -> Result<(), ProviderError> {
    let mut state = GoogleStreamState::new();
    let mut decoder = SseDecoder::new();
    loop {
        let Some(chunk) = response.next_text().await? else {
            break;
        };
        for sse in decoder.push_text(&chunk) {
            process_sse(&sse.data, &mut state, model, output, writer)?;
        }
    }
    for sse in decoder.finish() {
        process_sse(&sse.data, &mut state, model, output, writer)?;
    }
    state.finish(output, writer);
    Ok(())
}

/// Whether the cancellation token is set.
fn is_cancelled(signal: &Option<tokio_util::sync::CancellationToken>) -> bool {
    signal
        .as_ref()
        .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
}

/// Stream one Antigravity turn (the `stream_google` analogue).
pub fn stream_antigravity(
    model: &Model,
    context: &Context,
    options: Option<&AntigravityOptions>,
) -> AssistantMessageEventStream {
    let options = options.cloned();
    let model = model.clone();
    let context = context.clone();
    let (writer, reader) = create_assistant_message_event_stream();

    tokio::spawn(async move {
        let mut output = AssistantMessage {
            content: Vec::new(),
            api: API_ANTIGRAVITY.to_string(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            stop_reason_raw: None,
            error_message: None,
            timestamp: now_ms(),
            rest: Map::default(),
        };

        let result = run_stream(&model, &context, options.as_ref(), &mut output, &writer).await;
        match result {
            Ok(()) => {
                writer.push(AssistantMessageEvent::Done {
                    reason: done_reason(output.stop_reason),
                    message: output,
                });
                writer.end(None);
            }
            Err(error) => {
                output.stop_reason = if error == ProviderError::Aborted {
                    StopReason::Aborted
                } else {
                    StopReason::Error
                };
                output.error_message = Some(format_stream_failure_message(&error));
                record_stream_failure(
                    (&model.provider, &model.id, &model.api),
                    &mut output,
                    &error,
                );
                writer.push(AssistantMessageEvent::Error {
                    reason: error_reason(output.stop_reason),
                    error: output.clone(),
                });
                writer.end(Some(output));
            }
        }
    });

    reader
}

// Long by design: mirrors the provider's stream shape (runtime-model and endpoint fallback).
#[allow(clippy::too_many_lines)]
async fn run_stream(
    model: &Model,
    context: &Context,
    options: Option<&AntigravityOptions>,
    output: &mut AssistantMessage,
    writer: &AssistantMessageEventWriter,
) -> Result<(), ProviderError> {
    let options = options.cloned().unwrap_or_default();
    if is_cancelled(&options.base.signal) {
        return Err(ProviderError::Aborted);
    }

    let mut credentials = resolve_credentials(model, &options).await;
    if credentials.token.is_empty() {
        return Err(ProviderError::Message(
            "No Antigravity OAuth credentials. Run the antigravity login, or set ANTIGRAVITY_API_KEY."
                .to_string(),
        ));
    }
    // The access tokens this stream has already used; a hard quota wall switches
    // to a stored account whose token is not in this set (the TS `triedAccessTokens`).
    let mut tried_access_tokens = vec![credentials.token.clone()];

    let effort = options.reasoning.unwrap_or(ModelThinkingLevel::Off);
    let mut runtime_models = vec![request_model_id(&model.id, effort).to_string()];
    if let Some(fallback) = fallback_runtime_model(&runtime_models[0], effort) {
        if fallback != runtime_models[0] {
            runtime_models.push(fallback);
        }
    }

    let bases = stream_endpoint_bases(&model.base_url);
    let mut last_error: Option<ProviderError> = None;
    let mut started = false;

    for runtime_model in &runtime_models {
        if is_cancelled(&options.base.signal) {
            return Err(ProviderError::Aborted);
        }

        let request = build_request_body(model, context, &options, runtime_model);
        let step = request
            .get("contents")
            .and_then(Value::as_array)
            .map_or(1, Vec::len)
            .max(1);
        let request_id = build_request_id(options.base.session_id.as_deref(), step);
        let mut params = build_envelope(runtime_model, &request, &credentials.project, &request_id);
        if let Some(on_payload) = &options.base.on_payload {
            if let Some(next) = on_payload(params.clone(), model) {
                params = next;
            }
        }

        for (index, base) in bases.iter().enumerate() {
            if is_cancelled(&options.base.signal) {
                return Err(ProviderError::Aborted);
            }
            let url = format!("{base}/v1internal:streamGenerateContent?alt=sse");
            let headers = request_headers(&credentials.token, model, &options);
            let response = send(RequestOptions {
                method: reqwest::Method::POST,
                url,
                headers,
                body: Some(params.to_string()),
                signal: options.base.signal.clone(),
                timeout_ms: options.base.timeout_ms,
                connection: ConnectionErrorProfile::RawFetch,
                transport: Transport::Http1,
            })
            .await;

            let mut response = match response {
                Ok(response) => response,
                Err(error) => {
                    last_error = Some(error);
                    if index + 1 < bases.len() {
                        continue;
                    }
                    break;
                }
            };

            if response.status >= 400 {
                let mut body = response.read_all_text().await.unwrap_or_default();
                // A hard 429 quota wall switches to the next stored account and
                // re-sends once with its token (the TS `failoverToNextAccount`
                // branch); the envelope is rebuilt with the freshly resolved project.
                if switch_account_on_quota(
                    response.status,
                    &body,
                    &mut credentials,
                    &mut tried_access_tokens,
                )
                .await
                {
                    params =
                        build_envelope(runtime_model, &request, &credentials.project, &request_id);
                    if let Some(on_payload) = &options.base.on_payload {
                        if let Some(next) = on_payload(params.clone(), model) {
                            params = next;
                        }
                    }
                    let headers = request_headers(&credentials.token, model, &options);
                    let retried = send(RequestOptions {
                        method: reqwest::Method::POST,
                        url: format!("{base}/v1internal:streamGenerateContent?alt=sse"),
                        headers,
                        body: Some(params.to_string()),
                        signal: options.base.signal.clone(),
                        timeout_ms: options.base.timeout_ms,
                        connection: ConnectionErrorProfile::RawFetch,
                        transport: Transport::Http1,
                    })
                    .await;
                    response = match retried {
                        Ok(response) => response,
                        Err(error) => {
                            last_error = Some(error);
                            if index + 1 < bases.len() {
                                continue;
                            }
                            break;
                        }
                    };
                    if response.status >= 400 {
                        body = response.read_all_text().await.unwrap_or_default();
                    }
                }
                if response.status >= 400 {
                    let mut error = ProviderError::from_http_status_body(
                        response.status,
                        &body,
                        response.headers.clone(),
                    );
                    if let ProviderError::Http(http) = &mut error {
                        http.sdk_name = Some("AntigravityError".to_string());
                    }
                    last_error = Some(error);
                    if retryable_status(response.status) && index + 1 < bases.len() {
                        continue;
                    }
                    break;
                }
            }

            output.content.clear();
            output.usage = Usage::default();
            output.stop_reason = StopReason::Stop;
            output.stop_reason_raw = None;
            output.error_message = None;
            if !started {
                writer.push(AssistantMessageEvent::Start {
                    partial: output.clone(),
                });
                started = true;
            }
            stream_body(&mut response, model, output, writer).await?;

            if is_cancelled(&options.base.signal) {
                return Err(ProviderError::Aborted);
            }
            if !output.content.is_empty() {
                if matches!(output.stop_reason, StopReason::Aborted | StopReason::Error) {
                    return Err(ProviderError::StreamFailure(
                        stream_failure_from_stop_reason(output.stop_reason_raw.as_deref(), None),
                    ));
                }
                return Ok(());
            }
            last_error = Some(ProviderError::Message(
                "Antigravity API returned an empty response".to_string(),
            ));
            break;
        }
    }

    Err(last_error
        .unwrap_or_else(|| ProviderError::Message("All Antigravity endpoints failed".to_string())))
}

/// Stream a simple option set, mapping the shared reasoning selector to the Antigravity effort
/// (`stream_simple_google` analogue).
pub fn stream_simple_antigravity(
    model: &Model,
    context: &Context,
    options: Option<&SimpleStreamOptions>,
) -> AssistantMessageEventStream {
    let api_key = options
        .and_then(|options| options.base.api_key.clone())
        .or_else(|| get_env_api_key(&model.provider));
    let base = match api_key {
        Some(api_key) => build_base_options(model, options, Some(&api_key)),
        None => build_base_options(model, options, None),
    };
    let effort = options
        .and_then(|options| options.reasoning)
        .map(|reasoning| clamp_thinking_level(model, reasoning))
        .unwrap_or(ModelThinkingLevel::Off);
    let stream_options = AntigravityOptions {
        base,
        reasoning: Some(effort),
        tool_choice: None,
    };
    stream_antigravity(model, context, Some(&stream_options))
}

/// Registry provider for the `antigravity` API.
pub struct AntigravityProvider;

impl Provider for AntigravityProvider {
    fn api(&self) -> &str {
        API_ANTIGRAVITY
    }

    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&StreamOptions>,
    ) -> AssistantMessageEventStream {
        let options = options.map(|base| AntigravityOptions::from_base(base.clone()));
        stream_antigravity(model, context, options.as_ref())
    }

    fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&SimpleStreamOptions>,
    ) -> AssistantMessageEventStream {
        stream_simple_antigravity(model, context, options)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AssistantContent, ModelCost, ModelInput};
    use pa_types::JsNumber;

    /// Serializes tests that mutate process-global environment variables.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn test_model(id: &str, reasoning: bool) -> Model {
        Model {
            id: id.to_string(),
            name: id.to_string(),
            api: API_ANTIGRAVITY.to_string(),
            provider: "antigravity".to_string(),
            base_url: String::new(),
            reasoning,
            thinking_level_map: None,
            input: vec![ModelInput::Text],
            cost: ModelCost {
                input: JsNumber::from(0.0),
                output: JsNumber::from(0.0),
                cache_read: JsNumber::from(0.0),
                cache_write: JsNumber::from(0.0),
            },
            context_window: 128_000,
            max_tokens: 8192,
            max_tokens_explicit: false,
            featured: None,
            headers: None,
            compat: None,
        }
    }

    fn test_context(system_prompt: Option<&str>) -> Context {
        Context {
            system_prompt: system_prompt.map(std::string::ToString::to_string),
            messages: Vec::new(),
            tools: None,
        }
    }

    fn test_output() -> AssistantMessage {
        AssistantMessage {
            content: Vec::new(),
            api: API_ANTIGRAVITY.to_string(),
            provider: "antigravity".to_string(),
            model: "gemini-3.8-flash".to_string(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            stop_reason_raw: None,
            error_message: None,
            timestamp: 0,
            rest: Map::default(),
        }
    }

    fn test_options(effort: Option<ModelThinkingLevel>) -> AntigravityOptions {
        AntigravityOptions {
            base: StreamOptions::default(),
            reasoning: effort,
            tool_choice: None,
        }
    }

    #[test]
    fn extracts_project_id_shapes() {
        assert_eq!(
            extract_project_id(&json!({ "projectId": "direct" })),
            Some("direct".to_string())
        );
        assert_eq!(
            extract_project_id(&json!({ "project": { "id": "nested" } })),
            Some("nested".to_string())
        );
        assert_eq!(
            extract_project_id(&json!({ "cloudaicompanionProjects": ["list"] })),
            Some("list".to_string())
        );
        // `asString` only accepts non-empty strings; a nested `.id` is read only for the direct
        // project fields, never for a list item.
        assert_eq!(
            extract_project_id(&json!({ "cloudaicompanionProjects": [{ "id": "list" }] })),
            None
        );
        assert_eq!(
            extract_project_id(&json!({ "cloudaicompanionProjects": [{ "projectId": "list" }] })),
            Some("list".to_string())
        );
        assert_eq!(
            extract_project_id(&json!({ "projectIds": ["first"] })),
            Some("first".to_string())
        );
        assert_eq!(extract_project_id(&json!({ "projectId": "" })), None);
        assert_eq!(extract_project_id(&json!({})), None);
    }

    #[test]
    fn classifies_quota_walls() {
        assert!(is_quota_wall(429, "Individual quota reached"));
        assert!(is_quota_wall(
            429,
            r#"{"error":{"message":"Quota exceeded for quota metric"}}"#
        ));
        assert!(is_quota_wall(429, "Resets in 3 hours."));
        assert!(!is_quota_wall(
            429,
            "ResourceExhausted: rate limit exceeded"
        ));
        assert!(!is_quota_wall(429, "plain throttling"));
        assert!(!is_quota_wall(500, "quota exceeded"));
    }

    #[test]
    fn parses_raw_and_json_credentials() {
        assert_eq!(
            parse_api_key("ya29.raw-token"),
            Some(AntigravityApiKey {
                token: "ya29.raw-token".to_string(),
                project_id: None,
            })
        );
        assert_eq!(
            parse_api_key(r#"{"token":"tok","projectId":"proj"}"#),
            Some(AntigravityApiKey {
                token: "tok".to_string(),
                project_id: Some("proj".to_string()),
            })
        );
        assert_eq!(
            parse_api_key(r#"{"access":"acc"}"#),
            Some(AntigravityApiKey {
                token: "acc".to_string(),
                project_id: None,
            })
        );
        assert_eq!(parse_api_key(r#"{"other":"field"}"#), None);
        assert_eq!(parse_api_key("{}"), None);
        assert_eq!(parse_api_key("   "), None);
    }

    #[test]
    fn stored_credential_entry_reads_the_parent_shape() {
        let entry = credential_from_entry(&json!({
            "access": "token-1",
            "refresh": "refresh-1",
            "expires": 123,
            "projectId": "proj-1",
            "email": "user@example.com",
            "type": "oauth",
        }));
        assert_eq!(
            entry,
            StoredAntigravityCredential {
                token: Some("token-1".to_string()),
                project_id: Some("proj-1".to_string()),
                email: Some("user@example.com".to_string()),
            }
        );
        // Prime Agent's `AuthCredential::Oauth` stores the email as `accountId`.
        let entry = credential_from_entry(&json!({
            "access": "token-2",
            "refresh": "refresh-2",
            "expires": 123,
            "accountId": "user@example.com",
        }));
        assert_eq!(entry.email.as_deref(), Some("user@example.com"));
    }

    #[test]
    fn credential_sources_merge_with_key_project_winning() {
        let stored = Some(StoredAntigravityCredential {
            token: Some("file-token".to_string()),
            project_id: Some("file-project".to_string()),
            email: Some("user@example.com".to_string()),
        });
        // The JSON key's project id wins over the stored file's project id.
        let merged = merge_credentials(
            Some(r#"{"token":"key-token","projectId":"key-project"}"#),
            Some("env-token".to_string()),
            stored.clone(),
        );
        assert_eq!(merged.token.as_deref(), Some("key-token"));
        assert_eq!(merged.project_id.as_deref(), Some("key-project"));

        // When the explicit JSON key has no project id, stored project id is used.
        let fallback_proj = merge_credentials(
            Some(r#"{"token":"key-token"}"#),
            Some("env-token".to_string()),
            stored.clone(),
        );
        assert_eq!(fallback_proj.token.as_deref(), Some("key-token"));
        assert_eq!(fallback_proj.project_id.as_deref(), Some("file-project"));

        // The env var is only used when no key is present.
        let env_only = merge_credentials(None, Some("env-token".to_string()), stored);
        assert_eq!(env_only.token.as_deref(), Some("env-token"));

        // A JSON key without token does not block fallback to env or stored.
        let json_without_token = merge_credentials(
            Some(r#"{"other":"data"}"#),
            Some("env-token".to_string()),
            None,
        );
        assert_eq!(json_without_token.token.as_deref(), Some("env-token"));

        // No source at all leaves the token empty (the provider then errors).
        let empty = merge_credentials(None, None, None);
        assert_eq!(empty.token, None);
        assert_eq!(empty.project_id, None);
        assert_eq!(empty.email, None);
    }

    #[test]
    fn cli_headers_have_no_accept_and_keep_authorization() {
        let headers = antigravity_headers("tok");
        assert!(!headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("accept")));
        assert!(headers
            .iter()
            .any(|(name, value)| name == "Authorization" && value == "Bearer tok"));
        assert!(headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("content-type")));
        assert!(headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("user-agent")));
    }

    #[test]
    fn endpoint_bases_prefer_the_model_base_url() {
        assert_eq!(
            stream_endpoint_bases(DEFAULT_ANTIGRAVITY_BASE_URL),
            vec![
                DEFAULT_ANTIGRAVITY_BASE_URL.to_string(),
                "https://cloudcode-pa.googleapis.com".to_string(),
            ]
        );
        assert_eq!(
            stream_endpoint_bases("https://example.test/"),
            vec!["https://example.test".to_string()]
        );
    }

    #[test]
    fn envelope_has_the_cloud_code_assist_shape() {
        let envelope = build_envelope(
            "gemini-3.8-flash-high",
            &json!({ "contents": [] }),
            "proj",
            "req-1",
        );
        assert_eq!(envelope["project"], "proj");
        assert_eq!(envelope["model"], "gemini-3.8-flash-high");
        assert_eq!(envelope["userAgent"], "antigravity");
        assert_eq!(envelope["requestType"], "agent");
        assert_eq!(envelope["requestId"], "req-1");
        assert_eq!(envelope["request"]["contents"], json!([]));
    }

    #[test]
    fn request_body_carries_system_instruction_and_thinking() {
        let model = test_model("gemini-3.8-flash", true);
        let mut options = test_options(Some(ModelThinkingLevel::High));
        options.base.temperature = Some(0.5);
        options.base.max_tokens = Some(4096);

        let body = build_request_body(
            &model,
            &test_context(Some("sys")),
            &options,
            "gemini-3.8-flash-high",
        );
        assert_eq!(body["systemInstruction"]["role"], "user");
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "sys");
        assert_eq!(body["generationConfig"]["temperature"], 0.5);
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 4096);
        // gemini-* at High: dynamic thinking budget (-1), thoughts included.
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            -1
        );
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["includeThoughts"],
            true
        );

        let fallback = build_request_body(
            &model,
            &test_context(None),
            &options,
            "gemini-3.8-flash-high",
        );
        assert_eq!(
            fallback["systemInstruction"]["parts"]
                .as_array()
                .map(|parts| parts.len()),
            Some(2)
        );
    }

    #[test]
    fn thinking_budget_is_omitted_only_when_off() {
        let claude = test_model("claude-sonnet-4-6", true);

        // Off: `thinkingBudget: 0` is rejected by Cloud Code (HTTP 400), so the key is absent.
        let off = build_request_body(
            &claude,
            &test_context(None),
            &test_options(Some(ModelThinkingLevel::Off)),
            "claude-sonnet-4-6",
        );
        assert!(off["generationConfig"]["thinkingConfig"]
            .get("thinkingBudget")
            .is_none());
        assert_eq!(
            off["generationConfig"]["thinkingConfig"]["includeThoughts"],
            false
        );

        // Non-off budgets are still sent unchanged.
        let on = build_request_body(
            &claude,
            &test_context(None),
            &test_options(Some(ModelThinkingLevel::Medium)),
            "claude-sonnet-4-6",
        );
        assert_eq!(
            on["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            1024
        );
        assert_eq!(
            on["generationConfig"]["thinkingConfig"]["includeThoughts"],
            true
        );

        // Gemini at High keeps the dynamic (-1) budget.
        let gemini = test_model("gemini-3.8-flash", true);
        let dynamic = build_request_body(
            &gemini,
            &test_context(None),
            &test_options(Some(ModelThinkingLevel::High)),
            "gemini-3.8-flash-high",
        );
        assert_eq!(
            dynamic["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            -1
        );
    }

    #[test]
    fn claude_models_use_the_legacy_tool_bridge() {
        let model = test_model("claude-sonnet-4-6", true);
        let mut context = test_context(None);
        context.tools = Some(vec![crate::types::Tool {
            name: "read".to_string(),
            description: "read a file".to_string(),
            parameters: json!({ "type": "object", "properties": {} }),
        }]);
        let mut options = test_options(Some(ModelThinkingLevel::Off));
        options.tool_choice = Some(GoogleToolChoice::Any);

        let body = build_request_body(&model, &context, &options, "claude-sonnet-4-6");
        let declaration = &body["tools"][0]["functionDeclarations"][0];
        assert!(declaration.get("parameters").is_some());
        assert!(declaration.get("parametersJsonSchema").is_none());
        assert_eq!(body["toolConfig"]["functionCallingConfig"]["mode"], "ANY");
        // Off must not send `thinkingBudget: 0` — Cloud Code rejects it with HTTP 400.
        assert!(body["generationConfig"]["thinkingConfig"]
            .get("thinkingBudget")
            .is_none());
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["includeThoughts"],
            false
        );
    }

    #[test]
    fn response_payload_unwraps_and_chunk_error_maps() {
        let wrapped = json!({ "response": { "candidates": [] } });
        assert_eq!(response_payload(&wrapped)["candidates"], json!([]));
        let bare = json!({ "candidates": [] });
        assert_eq!(response_payload(&bare)["candidates"], json!([]));

        let mapped = chunk_error(&json!({ "code": 503, "message": "No capacity available" }));
        match mapped {
            ProviderError::Http(error) => assert_eq!(error.status, Some(503)),
            other => panic!("expected an HTTP error, got {other:?}"),
        }
        let plain = chunk_error(&json!({ "message": "boom" }));
        assert_eq!(plain, ProviderError::Message("boom".to_string()));
    }

    #[test]
    fn request_ids_are_stable_for_a_session_seed() {
        let first = build_request_id(Some("session-1"), 3);
        let first_parts: Vec<&str> = first.split('/').collect();
        assert_eq!(first_parts.len(), 5);
        assert_eq!(first_parts[0], "agent");
        assert_eq!(first_parts[4], "3");
        assert_eq!(first_parts[1].len(), 36);
        assert_eq!(first_parts[3].len(), 36);

        // The conversation/trajectory ids are derived from the seed alone, not the timestamp.
        let second = build_request_id(Some("session-1"), 3);
        let second_parts: Vec<&str> = second.split('/').collect();
        assert_eq!(first_parts[1], second_parts[1]);
        assert_eq!(first_parts[3], second_parts[3]);
    }

    #[test]
    fn sse_chunks_reuse_the_gemini_stream_state() {
        let model = test_model("gemini-3.8-flash", true);
        let (writer, _reader) = create_assistant_message_event_stream();
        let mut output = test_output();
        let mut state = GoogleStreamState::new();

        process_sse(
            r#"{"response":{"candidates":[{"content":{"parts":[{"text":"Hello"}]}}]}}"#,
            &mut state,
            &model,
            &mut output,
            &writer,
        )
        .unwrap();
        process_sse(
            r#"{"response":{"candidates":[{"content":{"parts":[{"thought":true,"text":"hmm"}]}}]}}"#,
            &mut state,
            &model,
            &mut output,
            &writer,
        )
        .unwrap();
        process_sse(
            r#"{"response":{"candidates":[{"content":{"parts":[{"functionCall":{"name":"read","args":{"path":"a.ts"}}}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":100,"cachedContentTokenCount":40,"candidatesTokenCount":7,"thoughtsTokenCount":3,"totalTokenCount":110}}}"#,
            &mut state,
            &model,
            &mut output,
            &writer,
        )
        .unwrap();
        state.finish(&output, &writer);

        let text = output
            .content
            .iter()
            .filter_map(|block| match block {
                AssistantContent::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        assert_eq!(text, "Hello");
        assert_eq!(output.stop_reason, StopReason::ToolUse);
        assert_eq!(output.usage.input, 60);
        assert_eq!(output.usage.output, 10);
        assert_eq!(output.usage.total_tokens, 110);
        assert!(output
            .content
            .iter()
            .any(|block| matches!(block, AssistantContent::ToolCall(call) if call.name == "read")));
    }

    #[test]
    fn error_frames_abort_the_stream() {
        let model = test_model("gemini-3.8-flash", true);
        let (writer, _reader) = create_assistant_message_event_stream();
        let mut output = test_output();
        let mut state = GoogleStreamState::new();
        let result = process_sse(
            r#"{"error":{"code":503,"message":"No capacity available"}}"#,
            &mut state,
            &model,
            &mut output,
            &writer,
        );
        assert!(result.is_err());

        // Nested error frame: {"response": {"error": ...}}
        let mut state2 = GoogleStreamState::new();
        let mut output2 = test_output();
        let result2 = process_sse(
            r#"{"response":{"error":{"code":429,"message":"Resource exhausted"}}}"#,
            &mut state2,
            &model,
            &mut output2,
            &writer,
        );
        assert!(result2.is_err());
    }

    #[test]
    fn endpoint_candidates_env_override_is_validated() {
        let _guard = ENV_LOCK.lock().unwrap();

        let previous = std::env::var("ANTIGRAVITY_BASE_URL").ok();
        std::env::remove_var("ANTIGRAVITY_BASE_URL");
        assert_eq!(endpoint_candidates().len(), 3);

        std::env::set_var("ANTIGRAVITY_BASE_URL", "https://custom.googleapis.com/v1/");
        assert_eq!(
            endpoint_candidates(),
            vec!["https://custom.googleapis.com/v1".to_string()]
        );

        std::env::set_var("ANTIGRAVITY_BASE_URL", "http://insecure.example.com");
        assert_eq!(endpoint_candidates().len(), 3);

        match previous {
            Some(value) => std::env::set_var("ANTIGRAVITY_BASE_URL", value),
            None => std::env::remove_var("ANTIGRAVITY_BASE_URL"),
        }
    }

    #[test]
    fn provider_exposes_the_antigravity_api() {
        let provider = AntigravityProvider;
        assert_eq!(provider.api(), API_ANTIGRAVITY);
    }
}
