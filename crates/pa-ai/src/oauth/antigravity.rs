//! The Antigravity (Google Cloud Code Assist) OAuth flow: the PKCE
//! authorization against Google's OAuth endpoints, the registered loopback
//! callback raced against the manual paste, the form-encoded token exchange
//! and refresh, and the Cloud Code Assist project discovery. The client
//! id/secret are Google's public Antigravity desktop client (not a private
//! app secret).

use std::net::ToSocketAddrs;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use rand::Rng;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use url::Url;

use super::pkce::generate_pkce;
use super::provider_http::{
    ProviderHttp, ProviderHttpMethod, ProviderHttpRequest, ProviderHttpResponse,
};
use super::types::{OAuthLoginUi, OAuthPrompt};

/// The cancel error the driving surface maps to the silent cancelled outcome.
pub const LOGIN_CANCELLED: &str = "Login cancelled";

/// The registered redirect; the callback server binds exactly this.
pub const REDIRECT_URI: &str = "http://localhost:51121/oauth-callback";
/// The callback path the server answers (every other route 404s).
const CALLBACK_PATH: &str = "/oauth-callback";
/// The registered redirect port (the registration's, not a scan range).
const CALLBACK_PORT: u16 = 51_121;

/// Google's authorization endpoint.
pub const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
/// Google's token endpoint (exchange and refresh).
pub const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
/// The Google account profile the login reads the email from.
const USERINFO_URL: &str = "https://www.googleapis.com/oauth2/v1/userinfo?alt=json";

/// The OAuth scopes the Antigravity client requests.
pub const SCOPES: [&str; 6] = [
    "https://www.googleapis.com/auth/aicode",
    "https://www.googleapis.com/auth/cloud-platform",
    "https://www.googleapis.com/auth/userinfo.email",
    "https://www.googleapis.com/auth/userinfo.profile",
    "https://www.googleapis.com/auth/cclog",
    "https://www.googleapis.com/auth/experimentsandconfigs",
];

/// Google's public Antigravity desktop OAuth client id (the TS source stores
/// it base64-encoded; the decoded value is the wire value).
pub const CLIENT_ID: &str =
    "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com";
/// The public client secret that pairs with [`CLIENT_ID`].
pub const CLIENT_SECRET: &str = "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";

/// The token-exchange request bound; TS carries no explicit timeout here.
pub const DEFAULT_TOKEN_TIMEOUT_MS: u64 = 30_000;
/// The refresh runs under the auth store's file lock, which a peer declares
/// stale after 10 seconds; the request must fit inside that window.
pub const REFRESH_TIMEOUT_MS: u64 = 8_000;
/// Metadata lookups must be fast; a stalled endpoint falls through to the next.
pub const DISCOVERY_TIMEOUT_MS: u64 = 8_000;
/// The browser login must land inside this window (the TS five minutes).
pub const CALLBACK_TIMEOUT_MS: u64 = 5 * 60 * 1000;
/// The credential expiry skew (the TS `5 * 60 * 1000`).
const EXPIRY_SKEW_MS: i64 = 5 * 60 * 1000;
const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// The Cloud Code Assist endpoint the discovery probes first.
pub const DEFAULT_ENDPOINT: &str = "https://daily-cloudcode-pa.googleapis.com";
/// The discovery fallbacks, in order.
pub const ENDPOINT_FALLBACKS: [&str; 3] = [
    DEFAULT_ENDPOINT,
    "https://daily-cloudcode-pa.sandbox.googleapis.com",
    "https://cloudcode-pa.googleapis.com",
];
/// The CLI wire fingerprint the discovery requests carry.
pub const DEFAULT_USER_AGENT: &str =
    "antigravity/cli/1.2.4 (aidev_client; os_type=linux; arch=amd64; cl=982146307; auth_method=consumer)";

/// The loopback-only callback-host override (the same seam the Anthropic
/// callback server reads). Only `127.0.0.1`, `::1`, and `localhost` are
/// accepted, so a login code can never be captured off-machine.
const CALLBACK_HOST_ENV: &str = "PI_OAUTH_CALLBACK_HOST";
/// The project-id override [`default_project_id`] and [`resolve_project_id`] honor.
const PROJECT_ID_ENV: &str = "ANTIGRAVITY_PROJECT_ID";

const AUTH_INSTRUCTIONS: &str = "Complete Google sign-in. Prime Agent captures the local callback automatically - or, on a remote/headless machine, paste the callback URL when prompted.";
const PROMPT_MESSAGE: &str = "Remote/headless machine (your browser cannot reach localhost:51121)? After signing in, paste the full callback URL shown in your browser's address bar:";

/// The Antigravity credential: the Google OAuth tokens plus the account email.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AntigravityCredentials {
    pub access: String,
    /// The refresh token the login received; the refresh path keeps the prior
    /// one when the endpoint omits a replacement.
    pub refresh: String,
    /// Wall-clock epoch milliseconds.
    pub expires: i64,
    /// The account email (`accountId` in the auth store), when known.
    pub account_id: Option<String>,
}

/// The PKCE login: present Google's authorization URL, race the loopback
/// callback against the manual paste, and exchange the code for the
/// credentials the caller persists.
///
/// # Errors
///
/// Returns an error when the login is cancelled ([`LOGIN_CANCELLED`]), the
/// callback host or port is unavailable, the callback times out, the paste
/// mismatches, or the token exchange fails.
pub async fn login_antigravity(
    http: &dyn ProviderHttp,
    ui: &dyn OAuthLoginUi,
) -> Result<AntigravityCredentials, String> {
    if ui.is_cancelled() {
        return Err(LOGIN_CANCELLED.to_string());
    }
    let (verifier, challenge) = generate_pkce();
    // The state is independent of the PKCE verifier, so a leaked callback URL
    // cannot also disclose the code_verifier needed to mint tokens.
    let state = random_state();
    let host = resolve_callback_host()?;
    let server = AntigravityCallbackServer::start(&host, &state)?;
    login_with_server(http, ui, &server, &verifier, &challenge, &state).await
}

/// Refresh an expired credential.
///
/// # Errors
///
/// Returns an error when the token refresh fails.
pub async fn refresh_antigravity_token(
    http: &dyn ProviderHttp,
    refresh_token: &str,
) -> Result<AntigravityCredentials, String> {
    let body = form_body(&[
        ("client_id", CLIENT_ID),
        ("client_secret", CLIENT_SECRET),
        ("refresh_token", refresh_token),
        ("grant_type", "refresh_token"),
    ]);
    let response = post_form(http, TOKEN_URL, &body, REFRESH_TIMEOUT_MS)
        .await
        .map_err(|message| format!("Antigravity token refresh failed: {message}"))?;
    if !response.ok() {
        return Err(format!(
            "Antigravity token refresh failed: {}",
            sanitize_oauth_provider_error(&response.body)
        ));
    }
    let json = parse_json(&response.body, "Antigravity token refresh")?;
    let access = required_token(&json, "access_token", "Antigravity token refresh")?;
    // A refresh usually keeps the same refresh token; the wire one wins when present.
    let refresh = json
        .get("refresh_token")
        .and_then(serde_json::Value::as_str)
        .filter(|token| !token.is_empty())
        .map_or_else(|| refresh_token.to_string(), str::to_string);
    Ok(AntigravityCredentials {
        access,
        refresh,
        expires: expiry_from(lifetime_seconds(json.get("expires_in"))),
        // The refresh signature carries only the refresh token, so the email
        // cannot be preserved here; the caller keeps the stored one when it can.
        account_id: None,
    })
}

/// Discover the Cloud Code Assist project id for `access_token` (the TS
/// `loadCodeAssist`): probe the endpoints in order, then fall back to the
/// companion-project listing. `None` when every endpoint fails.
pub async fn load_code_assist(http: &dyn ProviderHttp, access_token: &str) -> Option<String> {
    for endpoint in ENDPOINT_FALLBACKS {
        let url = format!("{endpoint}/v1internal:loadCodeAssist");
        let response = http
            .request(
                discovery_request(
                    &url,
                    access_token,
                    r#"{"metadata":{"ideType":"ANTIGRAVITY"}}"#,
                ),
                DISCOVERY_TIMEOUT_MS,
            )
            .await;
        let Ok(response) = response else {
            continue;
        };
        if !response.ok() {
            continue;
        }
        // The first reachable endpoint decides: its project, else the listing.
        let project = serde_json::from_str::<serde_json::Value>(&response.body)
            .ok()
            .and_then(|value| extract_project_id(&value));
        return match project {
            Some(project) => Some(project),
            None => list_cloud_ai_companion_projects(http, access_token).await,
        };
    }
    None
}

/// The transport-facing project resolution (the TS `resolveProjectId`): the
/// `ANTIGRAVITY_PROJECT_ID` override first, then [`load_code_assist`].
pub async fn resolve_project_id(http: &dyn ProviderHttp, access_token: &str) -> Option<String> {
    if let Some(project) = project_id_override() {
        return Some(project);
    }
    load_code_assist(http, access_token).await
}

/// The fallback project id when discovery yields nothing: the
/// `ANTIGRAVITY_PROJECT_ID` override, else a stable UUID derived from `seed`
/// (the TS `defaultProjectId`).
#[must_use]
pub fn default_project_id(seed: &str) -> String {
    project_id_override().unwrap_or_else(|| stable_project_id(seed))
}

/// The stable RFC 4122 shaped UUID the TS `stableProjectId` derives from a
/// seed: the first 16 bytes of SHA-1("antigravity:" + seed), stamped as a v5
/// UUID.
#[must_use]
pub fn stable_project_id(seed: &str) -> String {
    let mut bytes = sha1_digest(format!("antigravity:{seed}").as_bytes())[..16].to_vec();
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex = hex::encode(&bytes);
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// The flow after the bind: present the URL, acquire the code, exchange it.
async fn login_with_server(
    http: &dyn ProviderHttp,
    ui: &dyn OAuthLoginUi,
    server: &AntigravityCallbackServer,
    verifier: &str,
    challenge: &str,
    state: &str,
) -> Result<AntigravityCredentials, String> {
    ui.on_auth(
        &authorization_url(challenge, state),
        Some(AUTH_INSTRUCTIONS),
    );
    let code = acquire_code(server, ui, state).await?;
    if ui.is_cancelled() {
        return Err(LOGIN_CANCELLED.to_string());
    }
    exchange_authorization_code(http, &code, verifier).await
}

fn authorization_url(challenge: &str, state: &str) -> String {
    let scope = SCOPES.join(" ");
    let mut url = Url::parse(AUTH_URL).expect("the authorize url parses");
    for (name, value) in [
        ("client_id", CLIENT_ID),
        ("response_type", "code"),
        ("redirect_uri", REDIRECT_URI),
        ("scope", scope.as_str()),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
        ("state", state),
        ("access_type", "offline"),
        ("prompt", "consent"),
    ] {
        url.query_pairs_mut().append_pair(name, value);
    }
    url.to_string()
}

/// Acquire the authorization code from the loopback callback or the manual
/// paste, whichever lands first, under the login deadline and the cancel flag.
async fn acquire_code(
    server: &AntigravityCallbackServer,
    ui: &dyn OAuthLoginUi,
    state: &str,
) -> Result<CallbackCode, String> {
    let manual = ui.on_manual_code_input();
    let manual_answer = async {
        match manual {
            Some(future) => future.await,
            None => std::future::pending::<Option<String>>().await,
        }
    };
    tokio::pin!(manual_answer);
    // The browser redirect wins over a late manual paste: the manual answer
    // only fills in when the callback settles the wait (the TS race).
    let wait = server.wait_for_code();
    tokio::pin!(wait);
    let deadline = std::time::Instant::now() + Duration::from_millis(CALLBACK_TIMEOUT_MS);
    let mut tick = tokio::time::interval(CANCEL_POLL_INTERVAL);
    let raced = loop {
        if ui.is_cancelled() {
            return Err(LOGIN_CANCELLED.to_string());
        }
        if std::time::Instant::now() >= deadline {
            return Err("OAuth callback timed out waiting for browser login".to_string());
        }
        tokio::select! {
            _ = tick.tick() => {}
            result = &mut wait => break RaceOutcome::Callback(result),
            answer = &mut manual_answer => break RaceOutcome::Manual(answer),
        }
    };
    let code = match raced {
        RaceOutcome::Callback(Ok(code)) => Some(code),
        RaceOutcome::Callback(Err(message)) => return Err(message),
        RaceOutcome::Manual(None) => return Err(LOGIN_CANCELLED.to_string()),
        RaceOutcome::Manual(Some(input)) => parse_pasted_callback(&input, state)?,
    };
    if let Some(code) = code {
        return Ok(code);
    }
    let answer = ui
        .on_prompt(&OAuthPrompt {
            message: PROMPT_MESSAGE.to_string(),
            placeholder: Some(REDIRECT_URI.to_string()),
            allow_empty: false,
        })
        .await;
    let input = answer.ok_or_else(|| LOGIN_CANCELLED.to_string())?;
    parse_pasted_callback(&input, state)?.ok_or_else(|| "Missing authorization code".to_string())
}

enum RaceOutcome {
    /// The callback settled: a code, or the flow error the server reported.
    Callback(Result<CallbackCode, String>),
    /// The paste input, or `None` when cancelled.
    Manual(Option<String>),
}

/// Parse a callback URL (or bare query string) pasted by the user. `None` for
/// an empty paste; a descriptive error for a malformed or mismatched one. The
/// checks mirror the local callback server.
fn parse_pasted_callback(raw: &str, expected_state: &str) -> Result<Option<CallbackCode>, String> {
    let text = raw.trim();
    if text.is_empty() {
        return Ok(None);
    }
    let url = Url::parse(text).or_else(|_| {
        let query = text.strip_prefix('?').unwrap_or(text);
        Url::parse(&format!("http://localhost:51121{CALLBACK_PATH}?{query}"))
    })
    .map_err(|_| "Pasted text is missing 'code' or 'state'. Paste the FULL callback URL from your browser's address bar (http://localhost:51121/oauth-callback?...).".to_string())?;
    let param = |name: &str| {
        url.query_pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.to_string())
            .filter(|value| !value.is_empty())
    };
    if let Some(error) = param("error") {
        return Err(format!("OAuth error from browser: {error}"));
    }
    let Some(code) = param("code") else {
        return Err("Pasted text is missing 'code' or 'state'. Paste the FULL callback URL from your browser's address bar (http://localhost:51121/oauth-callback?...).".to_string());
    };
    let Some(state) = param("state") else {
        return Err("Pasted text is missing 'code' or 'state'. Paste the FULL callback URL from your browser's address bar (http://localhost:51121/oauth-callback?...).".to_string());
    };
    if state != expected_state {
        return Err("State mismatch: that callback is from a different sign-in. Re-run /login antigravity, sign in again, and paste the new URL.".to_string());
    }
    Ok(Some(CallbackCode { code, state }))
}

async fn exchange_authorization_code(
    http: &dyn ProviderHttp,
    code: &CallbackCode,
    verifier: &str,
) -> Result<AntigravityCredentials, String> {
    let body = form_body(&[
        ("client_id", CLIENT_ID),
        ("client_secret", CLIENT_SECRET),
        ("code", code.code.as_str()),
        ("grant_type", "authorization_code"),
        ("redirect_uri", REDIRECT_URI),
        ("code_verifier", verifier),
    ]);
    let response = post_form(http, TOKEN_URL, &body, DEFAULT_TOKEN_TIMEOUT_MS)
        .await
        .map_err(|message| format!("Token exchange failed: {message}"))?;
    if !response.ok() {
        return Err(format!(
            "Token exchange failed: {}",
            sanitize_oauth_provider_error(&response.body)
        ));
    }
    let json = parse_json(&response.body, "Token exchange")?;
    let access = required_token(&json, "access_token", "Token exchange")?;
    let refresh = json
        .get("refresh_token")
        .and_then(serde_json::Value::as_str)
        .filter(|token| !token.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            "No refresh token received. Re-run /login antigravity and allow offline access."
                .to_string()
        })?;
    let account_id = get_user_email(http, &access).await;
    Ok(AntigravityCredentials {
        access,
        refresh,
        expires: expiry_from(lifetime_seconds(json.get("expires_in"))),
        account_id,
    })
}

/// Read the account email from Google's profile endpoint; a failure is not
/// fatal (the login still succeeds without an `accountId`).
async fn get_user_email(http: &dyn ProviderHttp, access_token: &str) -> Option<String> {
    let response = http
        .request(
            ProviderHttpRequest {
                method: ProviderHttpMethod::Get,
                url: USERINFO_URL.to_string(),
                headers: vec![
                    (
                        "Authorization".to_string(),
                        format!("Bearer {access_token}"),
                    ),
                    ("Accept".to_string(), "application/json".to_string()),
                ],
                body: None,
                follow_redirects: true,
            },
            DEFAULT_TOKEN_TIMEOUT_MS,
        )
        .await
        .ok()?;
    if !response.ok() {
        return None;
    }
    let json: serde_json::Value = serde_json::from_str(&response.body).ok()?;
    json.get("email")
        .and_then(serde_json::Value::as_str)
        .filter(|email| !email.is_empty())
        .map(str::to_string)
}

async fn list_cloud_ai_companion_projects(
    http: &dyn ProviderHttp,
    access_token: &str,
) -> Option<String> {
    for endpoint in ENDPOINT_FALLBACKS {
        let url = format!("{endpoint}/v1internal:listCloudAICompanionProjects");
        let response = http
            .request(
                discovery_request(&url, access_token, "{}"),
                DISCOVERY_TIMEOUT_MS,
            )
            .await;
        let Ok(response) = response else {
            continue;
        };
        if !response.ok() {
            continue;
        }
        // The first reachable endpoint decides (TS returns its extract, even None).
        return serde_json::from_str::<serde_json::Value>(&response.body)
            .ok()
            .and_then(|value| extract_project_id(&value));
    }
    None
}

fn discovery_request(url: &str, access_token: &str, body: &str) -> ProviderHttpRequest {
    ProviderHttpRequest {
        method: ProviderHttpMethod::Post,
        url: url.to_string(),
        headers: vec![
            (
                "Authorization".to_string(),
                format!("Bearer {access_token}"),
            ),
            ("Content-Type".to_string(), "application/json".to_string()),
            ("User-Agent".to_string(), DEFAULT_USER_AGENT.to_string()),
        ],
        body: Some(body.to_string()),
        follow_redirects: true,
    }
}

/// Pull the project id out of a discovery payload (the TS `extractProjectId`
/// field table, including the nested `id` and list shapes).
fn extract_project_id(value: &serde_json::Value) -> Option<String> {
    let map = value.as_object()?;
    for field in [
        "antigravityProjectId",
        "projectId",
        "backendProjectId",
        "userDefinedCloudaicompanionProject",
        "cloudaicompanionProject",
        "project",
    ] {
        if let Some(found) = map.get(field).and_then(project_string) {
            return Some(found);
        }
    }
    for field in ["projects", "projectIds", "cloudaicompanionProjects"] {
        if let Some(serde_json::Value::Array(items)) = map.get(field) {
            for item in items {
                if let Some(found) = extract_project_id(item) {
                    return Some(found);
                }
                if let Some(text) = item.as_str().filter(|text| !text.is_empty()) {
                    return Some(text.to_string());
                }
            }
        }
    }
    None
}

/// A string field, or an object's `id` (the TS `asString` over a bare value
/// and its nested `.id`).
fn project_string(value: &serde_json::Value) -> Option<String> {
    if let Some(text) = value.as_str().filter(|text| !text.is_empty()) {
        return Some(text.to_string());
    }
    value
        .as_object()?
        .get("id")
        .and_then(serde_json::Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

async fn post_form(
    http: &dyn ProviderHttp,
    url: &str,
    body: &str,
    timeout_ms: u64,
) -> Result<ProviderHttpResponse, String> {
    http.request(
        ProviderHttpRequest {
            method: ProviderHttpMethod::Post,
            url: url.to_string(),
            headers: vec![
                (
                    "Content-Type".to_string(),
                    "application/x-www-form-urlencoded".to_string(),
                ),
                ("Accept".to_string(), "application/json".to_string()),
            ],
            body: Some(body.to_string()),
            follow_redirects: true,
        },
        timeout_ms,
    )
    .await
}

fn form_body(fields: &[(&str, &str)]) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(fields.iter().copied())
        .finish()
}

fn parse_json(body: &str, label: &str) -> Result<serde_json::Value, String> {
    serde_json::from_str(body).map_err(|error| format!("{label} returned invalid JSON: {error}"))
}

fn required_token(json: &serde_json::Value, field: &str, label: &str) -> Result<String, String> {
    json.get(field)
        .and_then(serde_json::Value::as_str)
        .filter(|token| !token.is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("{label} response is missing {field}"))
}

/// The provider error summary: the JSON `error`/`error_description` pair when
/// the body is JSON, else a bounded slice of the body.
fn sanitize_oauth_provider_error(text: &str) -> String {
    let trimmed = text.trim();
    if let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(trimmed) {
        let parts: Vec<String> = ["error", "error_description"]
            .iter()
            .filter_map(|key| map.get(*key).and_then(serde_json::Value::as_str))
            .filter(|part| !part.is_empty())
            .map(str::to_string)
            .collect();
        if !parts.is_empty() {
            return parts.join(": ").chars().take(300).collect();
        }
    }
    let bounded: String = trimmed.chars().take(300).collect();
    let bounded = crate::providers::antigravity_types::redact_secrets(&bounded);
    if bounded.is_empty() {
        "unknown OAuth provider error".to_string()
    } else {
        bounded
    }
}

/// The wire `expires_in`, as whole seconds; a missing or non-positive value
/// reads as zero (the credential then refreshes immediately).
// The wire's expires_in is an integer second count read through JSON f64; i64 is the port's unit.
#[allow(clippy::cast_possible_truncation)]
fn lifetime_seconds(value: Option<&serde_json::Value>) -> i64 {
    value
        .and_then(serde_json::Value::as_f64)
        .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
        .map_or(0, |seconds| seconds as i64)
}

// Saturating: a hostile `expires_in` must not overflow the sum.
fn expiry_from(seconds: i64) -> i64 {
    now_ms()
        .saturating_add(seconds.saturating_mul(1000))
        .saturating_sub(EXPIRY_SKEW_MS)
}

// Epoch millis fit i64 for ~292 million years.
#[allow(clippy::cast_possible_truncation)]
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(i64::MAX, |elapsed| elapsed.as_millis() as i64)
}

fn project_id_override() -> Option<String> {
    std::env::var(PROJECT_ID_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// The loopback-only callback host (the TS `resolveCallbackHost`).
fn resolve_callback_host() -> Result<String, String> {
    let raw = std::env::var(CALLBACK_HOST_ENV).unwrap_or_default();
    let host = if raw.trim().is_empty() {
        "127.0.0.1".to_string()
    } else {
        raw.trim().to_lowercase()
    };
    if host == "127.0.0.1" || host == "::1" {
        return Ok(host);
    }
    if host == "localhost" {
        return Ok("127.0.0.1".to_string());
    }
    Err(format!(
        "Unsafe {CALLBACK_HOST_ENV}=\"{host}\". Only loopback hosts are allowed: 127.0.0.1, ::1, localhost."
    ))
}

fn random_state() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill(&mut bytes[..]);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// SHA-1 (FIPS 180-1), used only to derive the stable fallback project id.
/// The crate has no production SHA-1 dependency, so the digest is computed here.
// The round function binds the standard SHA-1 working variables a..e.
#[allow(clippy::many_single_char_names)]
fn sha1_digest(input: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [
        0x6745_2301,
        0xEFCD_AB89,
        0x98BA_DCFE,
        0x1032_5476,
        0xC3D2_E1F0,
    ];
    let length_bits = (input.len() as u64).wrapping_mul(8);
    let mut message = input.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&length_bits.to_be_bytes());
    for chunk in message.chunks_exact(64) {
        let mut words = [0u32; 80];
        for (index, word) in chunk.chunks_exact(4).enumerate() {
            words[index] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for index in 16..80 {
            words[index] =
                (words[index - 3] ^ words[index - 8] ^ words[index - 14] ^ words[index - 16])
                    .rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (index, word) in words.iter().enumerate() {
            let (function, constant) = match index {
                0..=19 => ((b & c) | (!b & d), 0x5A82_7999_u32),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(function)
                .wrapping_add(e)
                .wrapping_add(constant)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }
    let mut out = [0u8; 20];
    for (index, word) in h.iter().enumerate() {
        out[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CallbackCode {
    code: String,
    state: String,
}

/// The callback server's shared, once-settled outcome.
#[derive(Default, Debug)]
struct CallbackShared {
    result: tokio::sync::Mutex<Option<Result<CallbackCode, String>>>,
    notify: tokio::sync::Notify,
}

impl CallbackShared {
    async fn settle(&self, result: Result<CallbackCode, String>) {
        let mut slot = self.result.lock().await;
        if slot.is_none() {
            *slot = Some(result);
            drop(slot);
            // `notify_one` stores a permit when no waiter is registered yet, so
            // a wait that just saw the empty slot still wakes on its next poll.
            self.notify.notify_one();
        }
    }
}

/// The login's loopback callback server on the registered redirect port.
/// Dropping aborts the accept loop, releasing the port.
#[derive(Debug)]
struct AntigravityCallbackServer {
    shared: Arc<CallbackShared>,
    #[cfg(test)]
    port: u16,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for AntigravityCallbackServer {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl AntigravityCallbackServer {
    fn start(host: &str, state: &str) -> Result<Self, String> {
        Self::bind(host, CALLBACK_PORT, state)
    }

    fn bind(host: &str, port: u16, state: &str) -> Result<Self, String> {
        // `SO_REUSEADDR`: lingering TIME_WAIT connections on the registered
        // port must not fail the next login's bind (Unix only; on Windows the
        // flag would let a second socket bind over a live listener).
        let addr: std::net::SocketAddr = (host, port)
            .to_socket_addrs()
            .map_err(|error| format!("port {port}: {error}"))?
            .next()
            .ok_or_else(|| format!("port {port}: no addresses"))?;
        let socket = if addr.is_ipv4() {
            tokio::net::TcpSocket::new_v4()
        } else {
            tokio::net::TcpSocket::new_v6()
        }
        .map_err(|error| format!("port {port}: {error}"))?;
        #[cfg(unix)]
        socket
            .set_reuseaddr(true)
            .map_err(|error| format!("port {port}: {error}"))?;
        socket
            .bind(addr)
            .map_err(|error| format!("port {port}: {error}"))?;
        let listener = socket
            .listen(1024)
            .map_err(|error| format!("port {port}: {error}"))?;
        #[cfg(test)]
        let bound_port = listener.local_addr().map_or(port, |addr| addr.port());
        let shared = Arc::new(CallbackShared::default());
        let task_shared = Arc::clone(&shared);
        let state = state.to_string();
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let shared = Arc::clone(&task_shared);
                let state = state.clone();
                tokio::spawn(async move {
                    serve_callback(stream, &shared, &state).await;
                });
            }
        });
        Ok(AntigravityCallbackServer {
            shared,
            #[cfg(test)]
            port: bound_port,
            task: Some(task),
        })
    }

    #[cfg(test)]
    fn port(&self) -> u16 {
        self.port
    }

    async fn wait_for_code(&self) -> Result<CallbackCode, String> {
        loop {
            if let Some(result) = self.shared.result.lock().await.take() {
                return result;
            }
            self.shared.notify.notified().await;
        }
    }
}

/// One browser request: answer the page, settle the login on a matching
/// redirect (or on the provider's terminal error), and keep waiting on
/// unknown routes.
async fn serve_callback(mut stream: tokio::net::TcpStream, shared: &CallbackShared, state: &str) {
    let Some(request) = read_request_head(&mut stream).await else {
        let _ = write_response(
            &mut stream,
            "400 Bad Request",
            &error_page("Antigravity callback route not found.", None),
        )
        .await;
        return;
    };
    let target = request
        .lines()
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .nth(1)
        .unwrap_or_default();
    let path = target.split('?').next().unwrap_or_default();
    if path != CALLBACK_PATH {
        let _ = write_response(
            &mut stream,
            "404 Not Found",
            &error_page("Antigravity OAuth callback route not found.", None),
        )
        .await;
        return;
    }
    let query = target.split_once('?').map_or("", |(_, query)| query);
    if let Some(error) = query_param(query, "error") {
        let safe = escape_html(&error.chars().take(200).collect::<String>());
        let _ = write_response(
            &mut stream,
            "400 Bad Request",
            &error_page(&format!("Antigravity authentication failed: {safe}"), None),
        )
        .await;
        shared
            .settle(Err(format!(
                "OAuth error: {}",
                error.chars().take(200).collect::<String>()
            )))
            .await;
        return;
    }
    let (code, echoed) = match (query_param(query, "code"), query_param(query, "state")) {
        (Some(code), Some(echoed)) if !code.is_empty() => (code, echoed),
        _ => {
            let _ = write_response(
                &mut stream,
                "400 Bad Request",
                &error_page(
                    "Antigravity authentication failed: missing code or state.",
                    None,
                ),
            )
            .await;
            shared
                .settle(Err("Missing code or state in OAuth callback".to_string()))
                .await;
            return;
        }
    };
    if echoed != state {
        let _ = write_response(
            &mut stream,
            "400 Bad Request",
            &error_page("Antigravity authentication failed: invalid state.", None),
        )
        .await;
        shared.settle(Err("OAuth state mismatch".to_string())).await;
        return;
    }
    shared
        .settle(Ok(CallbackCode {
            code,
            state: echoed,
        }))
        .await;
    let _ = write_response(
        &mut stream,
        "200 OK",
        &success_page("Antigravity authentication complete. You can close this window and return to Prime Agent."),
    )
    .await;
}

/// Read one request head, accumulating until the blank line.
async fn read_request_head(stream: &mut tokio::net::TcpStream) -> Option<String> {
    let mut head = Vec::with_capacity(1024);
    let mut buffer = [0u8; 8192];
    loop {
        if head.windows(4).any(|window| window == b"\r\n\r\n") {
            return Some(String::from_utf8_lossy(&head).to_string());
        }
        if head.len() >= buffer.len() {
            return Some(String::from_utf8_lossy(&head).to_string());
        }
        let read = tokio::time::timeout(Duration::from_secs(30), stream.read(&mut buffer))
            .await
            .ok()?
            .ok()?;
        if read == 0 {
            return None;
        }
        head.extend_from_slice(&buffer[..read]);
    }
}

async fn write_response(
    stream: &mut tokio::net::TcpStream,
    status: &str,
    body: &str,
) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.flush().await
}

/// One decoded query parameter (`+` as space, percent-decoded).
fn query_param(query: &str, name: &str) -> Option<String> {
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if percent_decode(&key.replace('+', " ")) == name {
            return Some(percent_decode(&value.replace('+', " ")));
        }
    }
    None
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hi = bytes[index + 1];
            let lo = bytes[index + 2];
            if (hi as char).is_ascii_hexdigit() && (lo as char).is_ascii_hexdigit() {
                if let Ok(byte) =
                    u8::from_str_radix(core::str::from_utf8(&[hi, lo]).unwrap_or(""), 16)
                {
                    out.push(byte);
                    index += 3;
                    continue;
                }
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn escape_html(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

/// The callback pages: a dark minimal page carrying the login outcome.
fn render_page(title: &str, message: &str, details: Option<&str>) -> String {
    let details_html = details
        .map(|details| {
            format!(
                "    <div class=\"details\">{}</div>\n",
                escape_html(details)
            )
        })
        .unwrap_or_default();
    format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n  <meta charset=\"utf-8\" />\n  <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\" />\n  <title>{}</title>\n  <style>\n    body {{ margin: 0; min-height: 100vh; display: flex; align-items: center;\n           justify-content: center; padding: 24px; background: #000; color: #fff;\n           font-family: ui-sans-serif, system-ui, sans-serif; text-align: center; }}\n    main {{ width: 100%; max-width: 560px; }}\n    h1 {{ margin: 0 0 10px; font-size: 28px; line-height: 1.15; font-weight: 650; }}\n    p {{ margin: 0; line-height: 1.7; color: #a1a1aa; font-size: 15px; }}\n    .details {{ margin-top: 16px; font-family: ui-monospace, monospace; font-size: 13px;\n                color: #a1a1aa; white-space: pre-wrap; word-break: break-word; }}\n  </style>\n</head>\n<body>\n  <main>\n    <h1>Authentication</h1>\n    <p>{}</p>\n{}\n  </main>\n</body>\n</html>",
        escape_html(title),
        escape_html(message),
        details_html
    )
}

fn success_page(message: &str) -> String {
    render_page("Prime Agent authentication completed", message, None)
}

fn error_page(message: &str, details: Option<&str>) -> String {
    render_page("Prime Agent authentication failed", message, details)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, VecDeque};
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;
    use tokio::io::AsyncWriteExt as _;

    type ScriptedResponse = ProviderHttpResponse;

    /// Queued responses per url (popped in order), recording every request.
    #[derive(Default)]
    struct ScriptedHttp {
        queued: Mutex<HashMap<String, VecDeque<ScriptedResponse>>>,
        requests: Mutex<Vec<ProviderHttpRequest>>,
    }

    impl ScriptedHttp {
        fn new() -> Self {
            Self::default()
        }

        fn response(status: u16, body: &str) -> ScriptedResponse {
            ScriptedResponse {
                status,
                body: body.to_string(),
            }
        }

        fn queue(self, url: &str, responses: Vec<ScriptedResponse>) -> Self {
            self.queued
                .lock()
                .unwrap()
                .insert(url.to_string(), responses.into_iter().collect());
            self
        }

        fn request_bodies(&self, url: &str) -> Vec<String> {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .filter(|request| request.url == url)
                .filter_map(|request| request.body.clone())
                .collect()
        }
    }

    impl ProviderHttp for ScriptedHttp {
        fn request(
            &self,
            request: ProviderHttpRequest,
            _timeout_ms: u64,
        ) -> Pin<Box<dyn Future<Output = Result<ScriptedResponse, String>> + Send + '_>> {
            self.requests.lock().unwrap().push(request.clone());
            let response = self
                .queued
                .lock()
                .unwrap()
                .get_mut(&request.url)
                .and_then(VecDeque::pop_front);
            Box::pin(
                async move { response.ok_or_else(|| format!("{} was not scripted", request.url)) },
            )
        }
    }

    /// How the scripted surface answers the manual paste.
    enum PasteMode {
        /// Paste the loopback callback URL with the flow's own state.
        CallbackUrl,
        /// Paste a fixed string.
        Fixed(&'static str),
        /// Never answer (the browser-callback path).
        Pending,
        /// No manual paste surface at all.
        Absent,
    }

    struct ScriptedUi {
        auth_url: Mutex<Option<String>>,
        state: Mutex<Option<String>>,
        paste: PasteMode,
        cancelled: Arc<AtomicBool>,
        cancel_on_auth: bool,
    }

    impl ScriptedUi {
        fn new(paste: PasteMode) -> Self {
            ScriptedUi {
                auth_url: Mutex::new(None),
                state: Mutex::new(None),
                paste,
                cancelled: Arc::new(AtomicBool::new(false)),
                cancel_on_auth: false,
            }
        }
    }

    impl OAuthLoginUi for ScriptedUi {
        fn on_auth(&self, url: &str, _instructions: Option<&str>) {
            let state = url::Url::parse(url).ok().and_then(|parsed| {
                parsed
                    .query_pairs()
                    .find(|(key, _)| key == "state")
                    .map(|(_, value)| value.to_string())
            });
            *self.state.lock().unwrap() = state;
            *self.auth_url.lock().unwrap() = Some(url.to_string());
            if self.cancel_on_auth {
                self.cancelled.store(true, Ordering::Relaxed);
            }
        }

        fn on_prompt(
            &self,
            _prompt: &OAuthPrompt,
        ) -> Pin<Box<dyn Future<Output = Option<String>> + Send + '_>> {
            Box::pin(std::future::pending())
        }

        fn on_progress(&self, _message: &str) {}

        fn on_manual_code_input(
            &self,
        ) -> Option<Pin<Box<dyn Future<Output = Option<String>> + Send + '_>>> {
            type Answer = Pin<Box<dyn Future<Output = Option<String>> + Send>>;
            match &self.paste {
                PasteMode::Absent => None,
                PasteMode::Pending => {
                    let future: Answer = Box::pin(std::future::pending());
                    Some(future)
                }
                PasteMode::Fixed(text) => {
                    let text = text.to_string();
                    let future: Answer = Box::pin(async move { Some(text) });
                    Some(future)
                }
                PasteMode::CallbackUrl => {
                    let state = self.state.lock().unwrap().clone();
                    let future: Answer = Box::pin(async move {
                        Some(format!(
                            "{REDIRECT_URI}?code=the-code&state={}",
                            state.unwrap_or_default()
                        ))
                    });
                    Some(future)
                }
            }
        }

        fn is_cancelled(&self) -> bool {
            self.cancelled.load(Ordering::Relaxed)
        }
    }

    fn token_http() -> ScriptedHttp {
        ScriptedHttp::new()
            .queue(
                TOKEN_URL,
                vec![ScriptedHttp::response(
                    200,
                    r#"{"access_token":"the-access","refresh_token":"the-refresh","expires_in":3600}"#,
                )],
            )
            .queue(
                USERINFO_URL,
                vec![ScriptedHttp::response(200, r#"{"email":"a@example.com"}"#)],
            )
    }

    /// The flow on a free loopback port (the registered port is `start`'s
    /// production bind).
    async fn login_on_a_free_port(
        http: &ScriptedHttp,
        ui: &ScriptedUi,
    ) -> Result<AntigravityCredentials, String> {
        let (verifier, challenge) = generate_pkce();
        let state = random_state();
        let server = AntigravityCallbackServer::bind("127.0.0.1", 0, &state)
            .expect("a free loopback port binds");
        login_with_server(http, ui, &server, &verifier, &challenge, &state).await
    }

    #[test]
    fn the_authorization_url_carries_the_ts_parameters() {
        let url = authorization_url("the-challenge", "the-state");
        let parsed = url::Url::parse(&url).unwrap();
        assert_eq!(parsed.host_str(), Some("accounts.google.com"));
        assert_eq!(parsed.path(), "/o/oauth2/v2/auth");
        let param = |name: &str| {
            parsed
                .query_pairs()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.to_string())
                .unwrap_or_default()
        };
        assert_eq!(param("client_id"), CLIENT_ID);
        assert_eq!(param("response_type"), "code");
        assert_eq!(param("redirect_uri"), REDIRECT_URI);
        assert_eq!(param("scope"), SCOPES.join(" "));
        assert_eq!(param("code_challenge"), "the-challenge");
        assert_eq!(param("code_challenge_method"), "S256");
        assert_eq!(param("state"), "the-state");
        assert_eq!(param("access_type"), "offline");
        assert_eq!(param("prompt"), "consent");
    }

    #[tokio::test]
    async fn the_exchange_body_matches_the_ts_grant() {
        let http = token_http();
        let ui = ScriptedUi::new(PasteMode::CallbackUrl);
        let credentials = login_on_a_free_port(&http, &ui).await.unwrap();
        assert_eq!(credentials.access, "the-access");
        assert_eq!(credentials.refresh, "the-refresh");
        assert_eq!(credentials.account_id.as_deref(), Some("a@example.com"));
        let body = http.request_bodies(TOKEN_URL).remove(0);
        assert!(body.contains("grant_type=authorization_code"), "{body}");
        assert!(body.contains("client_id=1071006060591"), "{body}");
        assert!(body.contains("code=the-code"), "{body}");
        assert!(
            body.contains("redirect_uri=http%3A%2F%2Flocalhost%3A51121%2Foauth-callback"),
            "{body}"
        );
        assert!(body.contains("code_verifier="), "{body}");
        // TS: expires = now + expires_in * 1000 - 5 minutes.
        #[allow(clippy::cast_possible_truncation)]
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let skew = (credentials.expires - now - (3600 * 1000 - EXPIRY_SKEW_MS)).abs();
        assert!(skew < 10_000, "the expiry arithmetic: {skew}");
    }

    #[tokio::test]
    async fn a_missing_refresh_token_fails_the_exchange() {
        let http = ScriptedHttp::new().queue(
            TOKEN_URL,
            vec![ScriptedHttp::response(
                200,
                r#"{"access_token":"the-access","expires_in":3600}"#,
            )],
        );
        let ui = ScriptedUi::new(PasteMode::CallbackUrl);
        let error = login_on_a_free_port(&http, &ui).await.unwrap_err();
        assert_eq!(
            error,
            "No refresh token received. Re-run /login antigravity and allow offline access."
        );
    }

    #[tokio::test]
    async fn a_failed_exchange_surfaces_the_provider_error() {
        let http = ScriptedHttp::new().queue(
            TOKEN_URL,
            vec![ScriptedHttp::response(
                400,
                r#"{"error":"invalid_grant","error_description":"Bad Request"}"#,
            )],
        );
        let ui = ScriptedUi::new(PasteMode::CallbackUrl);
        let error = login_on_a_free_port(&http, &ui).await.unwrap_err();
        assert_eq!(error, "Token exchange failed: invalid_grant: Bad Request");
    }

    #[tokio::test]
    async fn the_refresh_posts_the_grant_and_keeps_the_prior_token() {
        let http = ScriptedHttp::new().queue(
            TOKEN_URL,
            vec![ScriptedHttp::response(
                200,
                r#"{"access_token":"new-access","expires_in":600}"#,
            )],
        );
        let credentials = refresh_antigravity_token(&http, "the-old-refresh")
            .await
            .unwrap();
        assert_eq!(credentials.access, "new-access");
        // The endpoint omitted a refresh token: the prior one stays.
        assert_eq!(credentials.refresh, "the-old-refresh");
        assert_eq!(credentials.account_id, None);
        let body = http.request_bodies(TOKEN_URL).remove(0);
        assert!(body.contains("grant_type=refresh_token"), "{body}");
        assert!(body.contains("refresh_token=the-old-refresh"), "{body}");
        assert!(body.contains("client_secret=GOCSPX-"), "{body}");
    }

    #[tokio::test]
    async fn a_failed_refresh_surfaces_the_provider_error() {
        let http = ScriptedHttp::new().queue(
            TOKEN_URL,
            vec![ScriptedHttp::response(
                400,
                r#"{"error":"invalid_grant","error_description":"Token has been expired or revoked."}"#,
            )],
        );
        let error = refresh_antigravity_token(&http, "the-old-refresh")
            .await
            .unwrap_err();
        assert_eq!(
            error,
            "Antigravity token refresh failed: invalid_grant: Token has been expired or revoked."
        );
    }

    #[tokio::test]
    async fn an_unreachable_token_endpoint_surfaces_the_transport_error() {
        // Nothing scripted: the transport fails the request.
        let http = ScriptedHttp::new();
        let error = refresh_antigravity_token(&http, "the-old-refresh")
            .await
            .unwrap_err();
        assert!(
            error.starts_with("Antigravity token refresh failed: "),
            "{error}"
        );
    }

    #[test]
    fn the_paste_parser_matches_the_ts_table() {
        let state = "the-state";
        let callback = "http://localhost:51121/oauth-callback?code=the-code&state=the-state";
        let parsed = parse_pasted_callback(callback, state).unwrap().unwrap();
        assert_eq!(parsed.code, "the-code");
        assert_eq!(parsed.state, state);
        // A bare query string is accepted.
        assert!(parse_pasted_callback("state=the-state&code=c2", state)
            .unwrap()
            .is_some());
        // An empty paste yields no code (the prompt fallback).
        assert_eq!(parse_pasted_callback("   ", state).unwrap(), None);
        // A missing parameter is descriptive.
        assert!(
            parse_pasted_callback("http://localhost:51121/oauth-callback?code=only", state)
                .unwrap_err()
                .contains("missing 'code' or 'state'")
        );
        // A mismatched state is rejected.
        assert!(parse_pasted_callback(
            "http://localhost:51121/oauth-callback?code=c&state=other",
            state
        )
        .unwrap_err()
        .contains("State mismatch"));
        // The provider's error response is surfaced.
        assert_eq!(
            parse_pasted_callback(
                "http://localhost:51121/oauth-callback?error=access_denied",
                state
            )
            .unwrap_err(),
            "OAuth error from browser: access_denied"
        );
    }

    #[tokio::test]
    async fn a_state_mismatch_paste_fails_the_login() {
        let http = token_http();
        let ui = ScriptedUi::new(PasteMode::Fixed(
            "http://localhost:51121/oauth-callback?code=c&state=wrong",
        ));
        let error = login_on_a_free_port(&http, &ui).await.unwrap_err();
        assert!(error.contains("State mismatch"), "{error}");
        assert!(http.request_bodies(TOKEN_URL).is_empty());
    }

    #[tokio::test]
    async fn a_cancelled_surface_ends_the_login() {
        let http = token_http();
        let mut ui = ScriptedUi::new(PasteMode::Pending);
        ui.cancel_on_auth = true;
        let error = login_on_a_free_port(&http, &ui).await.unwrap_err();
        assert_eq!(error, LOGIN_CANCELLED);
        assert!(http.request_bodies(TOKEN_URL).is_empty());
    }

    #[test]
    fn the_stable_project_id_matches_the_ts_shape() {
        let first = stable_project_id("the-seed");
        assert_eq!(first, stable_project_id("the-seed"));
        assert_ne!(first, stable_project_id("other-seed"));
        assert_eq!(first.len(), 36);
        assert_eq!(first, "8ae14b19-d80a-57da-b137-1b72c0d6a882");
    }

    #[test]
    fn the_sha1_digest_matches_the_known_vectors() {
        assert_eq!(
            hex::encode(sha1_digest(b"abc")),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(
            hex::encode(sha1_digest(b"")),
            "da39a3ee5e6b4b0d3255bfef95601890afd80709"
        );
    }

    #[test]
    fn extract_project_id_reads_the_ts_field_table() {
        assert_eq!(
            extract_project_id(&serde_json::json!({"projectId": "p-1"})),
            Some("p-1".to_string())
        );
        assert_eq!(
            extract_project_id(&serde_json::json!({"cloudaicompanionProject": "p-2"})),
            Some("p-2".to_string())
        );
        // The nested `id` shape.
        assert_eq!(
            extract_project_id(&serde_json::json!({"project": {"id": "p-3"}})),
            Some("p-3".to_string())
        );
        // The list shape takes bare strings; a nested object is not coerced
        // (TS `asString` reads a string only).
        assert_eq!(
            extract_project_id(&serde_json::json!({"projects": ["p-4"]})),
            Some("p-4".to_string())
        );
        assert_eq!(
            extract_project_id(&serde_json::json!({"projects": [{"id": "p-4"}]})),
            None
        );
        assert_eq!(
            extract_project_id(&serde_json::json!({"projectIds": ["p-5"]})),
            Some("p-5".to_string())
        );
        assert_eq!(extract_project_id(&serde_json::json!({"other": 1})), None);
    }

    #[tokio::test]
    async fn load_code_assist_falls_through_endpoints() {
        let sandbox = ENDPOINT_FALLBACKS[1];
        let http = ScriptedHttp::new()
            .queue(
                &format!("{DEFAULT_ENDPOINT}/v1internal:loadCodeAssist"),
                vec![ScriptedHttp::response(500, "boom")],
            )
            .queue(
                &format!("{sandbox}/v1internal:loadCodeAssist"),
                vec![ScriptedHttp::response(
                    200,
                    r#"{"projectId":"the-project"}"#,
                )],
            );
        assert_eq!(
            load_code_assist(&http, "the-token").await,
            Some("the-project".to_string())
        );
    }

    #[tokio::test]
    async fn load_code_assist_falls_back_to_the_companion_projects() {
        let http = ScriptedHttp::new()
            .queue(
                &format!("{DEFAULT_ENDPOINT}/v1internal:loadCodeAssist"),
                vec![ScriptedHttp::response(200, "{}")],
            )
            .queue(
                &format!("{DEFAULT_ENDPOINT}/v1internal:listCloudAICompanionProjects"),
                vec![ScriptedHttp::response(
                    200,
                    r#"{"projects":["listed-project"]}"#,
                )],
            );
        assert_eq!(
            load_code_assist(&http, "the-token").await,
            Some("listed-project".to_string())
        );
    }

    async fn request(port: u16, target: &str) -> String {
        use tokio::io::AsyncReadExt as _;
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("the callback server accepts the connection");
        stream
            .write_all(format!("GET {target} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())
            .await
            .expect("the request writes");
        let mut response = String::new();
        let mut buffer = [0u8; 4096];
        loop {
            let read = stream.read(&mut buffer).await.unwrap_or(0);
            if read == 0 {
                break;
            }
            response.push_str(&String::from_utf8_lossy(&buffer[..read]));
            if response.contains("</html>") {
                break;
            }
        }
        response
    }

    #[tokio::test]
    async fn a_matching_redirect_settles_the_code_and_state() {
        let server = AntigravityCallbackServer::bind("127.0.0.1", 0, "the-state")
            .expect("a free loopback port binds");
        let response = request(
            server.port(),
            "/oauth-callback?code=the-code&state=the-state",
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.contains("Antigravity authentication complete"));
        assert_eq!(
            server.wait_for_code().await,
            Ok(CallbackCode {
                code: "the-code".to_string(),
                state: "the-state".to_string(),
            })
        );
    }

    #[tokio::test]
    async fn an_error_callback_settles_the_flow_error() {
        let server = AntigravityCallbackServer::bind("127.0.0.1", 0, "the-state")
            .expect("a free loopback port binds");
        let response = request(server.port(), "/oauth-callback?error=access_denied").await;
        assert!(response.starts_with("HTTP/1.1 400 Bad Request"));
        assert_eq!(
            server.wait_for_code().await,
            Err("OAuth error: access_denied".to_string())
        );
    }

    #[tokio::test]
    async fn a_state_mismatch_callback_settles_the_flow_error() {
        let server = AntigravityCallbackServer::bind("127.0.0.1", 0, "the-state")
            .expect("a free loopback port binds");
        let response = request(server.port(), "/oauth-callback?code=the-code&state=other").await;
        assert!(response.starts_with("HTTP/1.1 400 Bad Request"));
        assert_eq!(
            server.wait_for_code().await,
            Err("OAuth state mismatch".to_string())
        );
    }

    #[test]
    fn the_error_summary_reads_the_provider_json() {
        assert_eq!(
            sanitize_oauth_provider_error(r#"{"error":"invalid_grant","error_description":"Bad"}"#),
            "invalid_grant: Bad"
        );
        assert_eq!(sanitize_oauth_provider_error("plain text"), "plain text");
        assert_eq!(
            sanitize_oauth_provider_error("   "),
            "unknown OAuth provider error"
        );
    }

    #[test]
    fn percent_decode_safe_with_multibyte_utf8_and_hex() {
        assert_eq!(percent_decode("a%€b"), "a%€b");
        assert_eq!(percent_decode("a%41b"), "aAb");
    }

    #[test]
    fn sanitize_oauth_provider_error_redacts_tokens_in_raw_text() {
        let raw = "Error: ya29.a0AfH6SMCxyz token invalid";
        let sanitized = sanitize_oauth_provider_error(raw);
        assert!(!sanitized.contains("ya29."));
        assert!(sanitized.contains("[redacted-access-token]"));
    }
}
