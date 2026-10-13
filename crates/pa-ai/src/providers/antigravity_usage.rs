//! Antigravity account usage/quota and request diagnostics.
//!
//! Ported from `pi-antigravity` (extension 0.10.0):
//!
//! - `src/usage/usage.ts` — account/quota/model fetch and the usage/models formatters, plus the
//!   `client.ts` fetch helpers usage calls (project discovery and the available-models catalog;
//!   the transport has no public equivalent yet).
//! - `src/diagnostics/diagnostics.ts` — the last-request snapshot bag.
//!
//! Deviations recorded for the parent: `AccountUsage.endpoint` is `Option<String>` (the TS type
//! is unsound), and `resolveApiKeyFromContext` takes the already-resolved key instead of a Pi
//! command context.
#![allow(dead_code)] // parity surface: the doctor command that reads the snapshot is elsewhere

use std::sync::{Arc, Mutex, OnceLock};

use serde_json::{json, Map, Value};
use tokio_util::sync::CancellationToken;

use crate::providers::antigravity_types::{
    antigravity_env, antigravity_headers, endpoint_candidates, is_record, js_number, js_string,
    js_truthy, parse_api_key, redact_secrets, safe_error, strip_validation_urls, value_as_number,
    AccountUsage, AntigravityHttp, AvailableModelsRaw, LoadCodeAssistRaw, ModelInfoRaw,
    ModelQuotaRow, QuotaBucket, QuotaBucketRaw, QuotaGroup, QuotaGroupRaw, QuotaSummaryRaw,
    TierInfo, TierRaw,
};

// =============================================================================================
// diagnostics/diagnostics.ts
// =============================================================================================

/// Last completed request snapshot for `/antigravity.doctor`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiagnosticsSnapshot {
    pub status: Option<u16>,
    pub endpoint: Option<String>,
    pub error: Option<String>,
    pub project_id: Option<String>,
    pub resolved_runtime_model: Option<String>,
    pub available_models: Option<String>,
    pub matched_model_debug: Option<String>,
    pub latency_ms: Option<u64>,
    pub masked_email: Option<String>,
    pub token_expiry: Option<String>,
    pub tool_schema_warnings: Option<String>,
}

tokio::task_local! {
    static BAG: Arc<Mutex<DiagnosticsSnapshot>>;
}

fn last() -> &'static Mutex<DiagnosticsSnapshot> {
    static LAST: OnceLock<Mutex<DiagnosticsSnapshot>> = OnceLock::new();
    LAST.get_or_init(|| Mutex::new(DiagnosticsSnapshot::default()))
}

fn update<F: FnOnce(&mut DiagnosticsSnapshot)>(change: F) {
    if let Ok(bag) = BAG.try_with(|bag| Arc::clone(bag)) {
        if let Ok(mut guard) = bag.lock() {
            change(&mut guard);
            return;
        }
    }
    if let Ok(mut guard) = last().lock() {
        change(&mut guard);
    }
}

fn current() -> DiagnosticsSnapshot {
    if let Ok(snapshot) = BAG.try_with(|bag| bag.lock().map(|guard| guard.clone()).unwrap_or_default())
    {
        return snapshot;
    }
    last()
        .lock()
        .map(|guard| guard.clone())
        .unwrap_or_default()
}

/// Run work with an isolated diagnostics bag; commits it to the last snapshot when done.
pub async fn run_with_diagnostics<F, T>(work: F) -> T
where
    F: std::future::Future<Output = T>,
{
    let bag = Arc::new(Mutex::new(DiagnosticsSnapshot::default()));
    let result = BAG.scope(bag.clone(), work).await;
    if let Ok(guard) = bag.lock() {
        if let Ok(mut last) = last().lock() {
            *last = guard.clone();
        }
    }
    result
}

/// The last completed request snapshot.
#[must_use]
pub fn get_last_diagnostics() -> DiagnosticsSnapshot {
    current()
}

/// Endpoint from the active request bag (or the last snapshot outside a request).
#[must_use]
pub fn get_current_endpoint() -> Option<String> {
    current().endpoint
}

/// Matched-model debug from the active request bag (or the last snapshot).
#[must_use]
pub fn get_current_matched_model_debug() -> Option<String> {
    current().matched_model_debug
}

/// Available-models label from the active request bag (or the last snapshot).
#[must_use]
pub fn get_current_available_models() -> Option<String> {
    current().available_models
}

/// Record the last response status.
pub fn set_last_status(status: Option<u16>) {
    update(|bag| bag.status = status);
}

/// Record the last endpoint.
pub fn set_last_endpoint(endpoint: Option<&str>) {
    update(|bag| bag.endpoint = endpoint.map(str::to_string));
}

/// Record the last error, redacted, validation URLs stripped, and capped at 800 chars.
pub fn set_last_error(error: Option<&str>) {
    update(|bag| {
        bag.error = error.map(|value| {
            take_chars(
                &strip_validation_urls(&redact_secrets(value)),
                800,
            )
        });
    });
}

/// Record the resolved project id.
pub fn set_last_project_id(project_id: Option<&str>) {
    update(|bag| bag.project_id = project_id.map(str::to_string));
}

/// Record the resolved runtime model id.
pub fn set_last_resolved_runtime_model(model: Option<&str>) {
    update(|bag| bag.resolved_runtime_model = model.map(str::to_string));
}

/// Record the available-models label.
pub fn set_last_available_models(models: Option<&str>) {
    update(|bag| bag.available_models = models.map(str::to_string));
}

/// Record the matched-model debug, redacted and capped at 1200 chars.
pub fn set_last_matched_model_debug(debug: Option<&str>) {
    update(|bag| {
        bag.matched_model_debug = debug.map(|value| take_chars(&redact_secrets(value), 1200));
    });
}

/// Record the last request latency.
pub fn set_last_latency_ms(ms: Option<u64>) {
    update(|bag| bag.latency_ms = ms);
}

/// Record the masked account email.
pub fn set_last_masked_email(email: Option<&str>) {
    update(|bag| bag.masked_email = email.map(str::to_string));
}

/// Record the token expiry.
pub fn set_last_token_expiry(expiry: Option<&str>) {
    update(|bag| bag.token_expiry = expiry.map(str::to_string));
}

/// Store sanitized tool-schema omissions for the next doctor report.
pub fn set_last_tool_schema_warnings(warnings: Option<&[String]>) {
    update(|bag| {
        bag.tool_schema_warnings =
            warnings.map(|list| take_chars(&redact_secrets(&list.join(" | ")), 1200));
    });
}

/// Test helper: reset the last snapshot between cases.
pub fn reset_diagnostics_for_tests() {
    if let Ok(mut guard) = last().lock() {
        *guard = DiagnosticsSnapshot::default();
    }
}

// =============================================================================================
// usage/usage.ts
// =============================================================================================

/// One `postJson` result (`{endpoint, status, data}` in `usage.ts`).
#[derive(Clone, Debug)]
pub struct PostJsonResult {
    pub endpoint: String,
    pub status: u16,
    pub data: Value,
}

fn take_chars(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

fn clamp_fraction(value: Option<f64>) -> Option<f64> {
    let value = value?;
    if !value.is_finite() {
        return None;
    }
    Some(value.clamp(0.0, 1.0))
}

fn remaining_percent(remaining: Option<f64>) -> Option<f64> {
    remaining.map(|value| (value * 1000.0).round() / 10.0)
}

fn progress_bar(remaining: Option<f64>, width: usize) -> String {
    match remaining {
        None => format!("[{}]", "?".repeat(width)),
        Some(value) => {
            let filled = ((value * width as f64).round() as i64).clamp(0, width as i64) as usize;
            format!("[{}{}]", "#".repeat(filled), "-".repeat(width - filled))
        }
    }
}

fn format_reset(reset_time: Option<&str>) -> String {
    let Some(reset_time) = reset_time else {
        return "n/a".to_string();
    };
    let Some(timestamp) = parse_iso8601_ms(reset_time) else {
        return reset_time.to_string();
    };
    let delta = timestamp - crate::utils_inner::diagnostics::now_ms() as i64;
    if delta <= 0 {
        return "now".to_string();
    }
    let total_min = (delta as f64 / 60_000.0).round() as i64;
    let days = total_min / (60 * 24);
    let hours = (total_min % (60 * 24)) / 60;
    let minutes = total_min % 60;
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m")
    }
}

fn json_headers(token: &str) -> Vec<(String, String)> {
    let mut headers = antigravity_headers(token);
    headers.push(("Accept".to_string(), "application/json".to_string()));
    headers
}

async fn post_json(
    path: &str,
    token: &str,
    body: &Value,
    http: &dyn AntigravityHttp,
) -> Result<PostJsonResult, String> {
    let headers = json_headers(token);
    let mut last_error_text = String::new();
    for endpoint in endpoint_candidates() {
        let url = format!("{endpoint}{path}");
        match http
            .post(&url, headers.clone(), body.to_string().into_bytes(), None)
            .await
        {
            Ok(response) => {
                set_last_endpoint(Some(&endpoint));
                set_last_status(Some(response.status));
                let text = response.text();
                let data = serde_json::from_str::<Value>(&text)
                    .unwrap_or_else(|_| json!({ "raw": text.clone() }));
                if !response.ok() {
                    let message = data
                        .get("error")
                        .and_then(|error| error.get("message"))
                        .and_then(Value::as_str)
                        .map_or_else(|| text.clone(), str::to_string);
                    last_error_text = message;
                    if ![403, 404, 429, 500, 502, 503, 504].contains(&response.status) {
                        return Err(format!(
                            "{path} failed ({}): {}",
                            response.status,
                            take_chars(&last_error_text, 300)
                        ));
                    }
                    continue;
                }
                return Ok(PostJsonResult {
                    endpoint,
                    status: response.status,
                    data,
                });
            }
            Err(error) => {
                last_error_text = safe_error(&error);
                set_last_error(Some(&last_error_text));
            }
        }
    }
    Err(format!(
        "{path} failed: {}",
        if last_error_text.is_empty() {
            "no endpoint available".to_string()
        } else {
            last_error_text
        }
    ))
}

fn first_truthy(values: &[Option<&Value>]) -> Option<String> {
    for value in values.iter().filter_map(|entry| *entry) {
        if js_truthy(value) {
            return Some(js_string(value));
        }
    }
    None
}

/// `extractProjectId` in `client.ts`.
#[must_use]
pub fn extract_project_id(value: &Value) -> Option<String> {
    let map = value.as_object()?;
    let direct = [
        "antigravityProjectId",
        "projectId",
        "backendProjectId",
        "userDefinedCloudaicompanionProject",
        "cloudaicompanionProject",
        "project",
    ]
    .iter()
    .find_map(|key| map.get(*key).filter(|value| !value.is_null()));
    if let Some(direct) = direct {
        if let Some(text) = direct.as_str().filter(|text| !text.is_empty()) {
            return Some(text.to_string());
        }
        if let Some(nested) = direct
            .get("id")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        {
            return Some(nested.to_string());
        }
    }
    for key in ["projects", "projectIds", "cloudaicompanionProjects"] {
        if let Some(Value::Array(items)) = map.get(key) {
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

/// `resolveProjectId` in `client.ts`.
#[must_use]
pub fn resolve_project_id(
    _token: &str,
    warmed_project: Option<&str>,
    credential_project_id: Option<&str>,
    email: Option<&str>,
) -> String {
    if let Some(project) = antigravity_env("PROJECT_ID")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        return project;
    }
    if let Some(warmed) = warmed_project.filter(|value| !value.is_empty()) {
        return warmed.to_string();
    }
    if let Some(credential) = credential_project_id.filter(|value| !value.is_empty()) {
        return credential.to_string();
    }
    crate::oauth::default_project_id(email.unwrap_or("antigravity-default"))
}

async fn load_code_assist_safe(token: &str, http: &dyn AntigravityHttp) -> Option<PostJsonResult> {
    post_json(
        "/v1internal:loadCodeAssist",
        token,
        &json!({
            "metadata": {
                "ideType": "ANTIGRAVITY",
                "platform": "PLATFORM_UNSPECIFIED",
                "pluginType": "GEMINI",
            },
        }),
        http,
    )
    .await
    .ok()
}

/// The user-quota-summary RPC is gated behind a paid subscription: free-tier accounts get 403
/// `SUBSCRIPTION_REQUIRED` (#3501). It is best-effort diagnostics only — never let it block the
/// rest of the account data (models, tier, project).
async fn fetch_quota_summary_safe(
    token: &str,
    http: &dyn AntigravityHttp,
) -> Result<PostJsonResult, String> {
    match post_json("/v1internal:retrieveUserQuotaSummary", token, &json!({}), http).await {
        Ok(result) => Ok(result),
        Err(error) => {
            let message = safe_error(&error);
            set_last_error(Some(&message));
            Err(message)
        }
    }
}

/// `parseQuotaSummary` in `usage.ts`.
#[must_use]
pub fn parse_quota_summary(data: &Value) -> (Vec<QuotaGroup>, Option<String>) {
    let summary: QuotaSummaryRaw = serde_json::from_value(data.clone()).unwrap_or_default();
    let mut groups = Vec::new();
    if let Some(raw_groups) = summary.groups.as_ref().and_then(Value::as_array) {
        for group_value in raw_groups {
            let group: QuotaGroupRaw =
                serde_json::from_value(group_value.clone()).unwrap_or_default();
            let mut buckets = Vec::new();
            if let Some(raw_buckets) = group.buckets.as_ref().and_then(Value::as_array) {
                for bucket_value in raw_buckets {
                    let bucket: QuotaBucketRaw =
                        serde_json::from_value(bucket_value.clone()).unwrap_or_default();
                    let remaining = clamp_fraction(
                        bucket.remaining_fraction.as_ref().and_then(value_as_number),
                    );
                    let has_bucket_id = bucket.bucket_id.as_ref().is_some_and(js_truthy);
                    if remaining.is_none() && !has_bucket_id {
                        continue;
                    }
                    let bucket_id = first_truthy(&[
                        bucket.bucket_id.as_ref(),
                        bucket.display_name.as_ref(),
                    ])
                    .unwrap_or_else(|| "unknown".to_string());
                    let display_name = first_truthy(&[
                        bucket.display_name.as_ref(),
                        bucket.bucket_id.as_ref(),
                    ])
                    .unwrap_or_else(|| "Limit".to_string());
                    buckets.push(QuotaBucket {
                        bucket_id,
                        display_name,
                        window: bucket.window.as_ref().filter(|value| js_truthy(value)).map(js_string),
                        reset_time: bucket
                            .reset_time
                            .as_ref()
                            .filter(|value| js_truthy(value))
                            .map(js_string),
                        description: bucket
                            .description
                            .as_ref()
                            .filter(|value| js_truthy(value))
                            .map(js_string),
                        remaining_fraction: remaining.unwrap_or(0.0),
                    });
                }
            }
            if buckets.is_empty()
                && !group.display_name.as_ref().is_some_and(js_truthy)
            {
                continue;
            }
            groups.push(QuotaGroup {
                display_name: first_truthy(&[group.display_name.as_ref()])
                    .unwrap_or_else(|| "Quota group".to_string()),
                description: group
                    .description
                    .as_ref()
                    .filter(|value| js_truthy(value))
                    .map(js_string),
                buckets,
            });
        }
    }
    (
        groups,
        summary
            .description
            .as_ref()
            .filter(|value| js_truthy(value))
            .map(js_string),
    )
}

/// `parseModels` in `usage.ts`.
#[must_use]
pub fn parse_models(data: &Value) -> (Vec<ModelQuotaRow>, Option<String>) {
    let raw: AvailableModelsRaw = serde_json::from_value(data.clone()).unwrap_or_default();
    let models_obj = raw
        .models
        .as_ref()
        .filter(|value| value.is_object())
        .and_then(Value::as_object);
    let mut models = Vec::new();
    if let Some(models_map) = models_obj {
        for (model_id, info_value) in models_map {
            if !is_record(info_value) {
                continue;
            }
            let info: ModelInfoRaw = match serde_json::from_value(info_value.clone()) {
                Ok(info) => info,
                Err(_) => continue,
            };
            if info.is_internal.as_ref().is_some_and(js_truthy) || model_id.starts_with("chat_") {
                continue;
            }
            let quota_info = info.quota_info.as_ref().filter(|value| value.is_object());
            models.push(ModelQuotaRow {
                model_id: model_id.clone(),
                display_name: info
                    .display_name
                    .as_ref()
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| info.label.as_ref().and_then(Value::as_str).map(str::to_string))
                    .or_else(|| {
                        info.model_name
                            .as_ref()
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    }),
                remaining_fraction: clamp_fraction(
                    quota_info
                        .and_then(|quota| quota.get("remainingFraction"))
                        .and_then(value_as_number),
                ),
                reset_time: quota_info
                    .and_then(|quota| quota.get("resetTime"))
                    .filter(|value| js_truthy(value))
                    .map(js_string),
                model_provider: info
                    .model_provider
                    .as_ref()
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| {
                        info.api_provider
                            .as_ref()
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    }),
                supports_thinking: Some(
                    info.supports_thinking.as_ref().is_some_and(js_truthy),
                ),
                supports_images: Some(info.supports_images.as_ref().is_some_and(js_truthy)),
                recommended: Some(info.recommended.as_ref().is_some_and(js_truthy)),
            });
        }
    }
    models.sort_by(|left, right| left.model_id.cmp(&right.model_id));

    let default_agent_model_id = first_truthy(&[
        raw.default_agent_model_id.as_ref(),
        raw.default_agent_model.as_ref(),
    ]);
    (models, default_agent_model_id)
}

/// `parseTier` in `usage.ts`.
#[must_use]
pub fn parse_tier(value: Option<&Value>) -> Option<TierInfo> {
    let value = value?;
    if !is_record(value) {
        return None;
    }
    let tier: TierRaw = serde_json::from_value(value.clone()).ok()?;
    if !tier.id.as_ref().is_some_and(js_truthy) && !tier.name.as_ref().is_some_and(js_truthy) {
        return None;
    }
    Some(TierInfo {
        id: tier.id.as_ref().filter(|value| js_truthy(value)).map(js_string),
        name: tier
            .name
            .as_ref()
            .filter(|value| js_truthy(value))
            .map(js_string),
        description: tier
            .description
            .as_ref()
            .filter(|value| js_truthy(value))
            .map(js_string),
    })
}

async fn fetch_available_models_from_endpoint(
    endpoint: &str,
    token: &str,
    project_id: &str,
    signal: Option<CancellationToken>,
    http: &dyn AntigravityHttp,
) -> Option<PostJsonResult> {
    let body = json!({ "project": project_id }).to_string();
    match http
        .post(
            &format!("{endpoint}/v1internal:fetchAvailableModels"),
            antigravity_headers(token),
            body.into_bytes(),
            signal,
        )
        .await
    {
        Ok(response) => {
            let text = response.text();
            let data =
                serde_json::from_str::<Value>(&text).unwrap_or_else(|_| json!({ "raw": text.clone() }));
            if !response.ok() {
                let message = data
                    .get("error")
                    .and_then(|error| error.get("message"))
                    .and_then(Value::as_str)
                    .map_or_else(|| text.clone(), str::to_string);
                set_last_error(Some(&message));
                return None;
            }
            Some(PostJsonResult {
                endpoint: endpoint.to_string(),
                status: response.status,
                data,
            })
        }
        Err(error) => {
            set_last_error(Some(&safe_error(&error)));
            None
        }
    }
}

/// `mergeAvailableModelsResults` in `client.ts`.
pub fn merge_available_models_results(
    results: Vec<Option<PostJsonResult>>,
) -> Result<PostJsonResult, String> {
    let mut merged: Map<String, Value> = Map::new();
    let mut default_agent_model_id: Option<Value> = None;
    let mut default_agent_model: Option<Value> = None;
    let mut last_endpoint = String::new();
    let mut last_status = 0_u16;

    for result in results.into_iter().flatten() {
        set_last_endpoint(Some(&result.endpoint));
        set_last_status(Some(result.status));
        last_endpoint = result.endpoint.clone();
        last_status = result.status;
        if let Some(models) = result
            .data
            .get("models")
            .filter(|value| value.is_object())
            .and_then(Value::as_object)
        {
            for (key, value) in models {
                merged.insert(key.clone(), value.clone());
            }
        }
        if let Some(value) = result
            .data
            .get("defaultAgentModelId")
            .filter(|value| value.is_string())
        {
            default_agent_model_id = Some(value.clone());
        }
        if let Some(value) = result
            .data
            .get("defaultAgentModel")
            .filter(|value| value.is_string())
        {
            default_agent_model = Some(value.clone());
        }
    }

    if last_endpoint.is_empty() {
        return Err("/v1internal:fetchAvailableModels failed: no endpoint available".to_string());
    }

    Ok(PostJsonResult {
        endpoint: last_endpoint,
        status: last_status,
        data: json!({
            "models": Value::Object(merged),
            "defaultAgentModelId": default_agent_model_id,
            "defaultAgentModel": default_agent_model,
        }),
    })
}

async fn fetch_available_models_catalog(
    token: &str,
    project_id: &str,
    signal: Option<CancellationToken>,
    http: &dyn AntigravityHttp,
) -> Result<PostJsonResult, String> {
    let mut results = Vec::new();
    for endpoint in endpoint_candidates() {
        results.push(
            fetch_available_models_from_endpoint(&endpoint, token, project_id, signal.clone(), http)
                .await,
        );
    }
    merge_available_models_results(results)
}

/// `fetchAccountUsage` in `usage.ts`.
pub async fn fetch_account_usage(
    api_key_raw: Option<&str>,
    http: &dyn AntigravityHttp,
) -> Result<AccountUsage, String> {
    let creds = parse_api_key(api_key_raw)?;
    let initial_project_id = if creds.project_id.is_empty() {
        resolve_project_id(&creds.token, None, None, None)
    } else {
        creds.project_id.clone()
    };

    let (assist, summary, available) = tokio::join!(
        load_code_assist_safe(&creds.token, http),
        fetch_quota_summary_safe(&creds.token, http),
        fetch_available_models_catalog(&creds.token, &initial_project_id, None, http),
    );

    let discovered_project = assist
        .as_ref()
        .and_then(|result| extract_project_id(&result.data));
    let project_id = resolve_project_id(
        &creds.token,
        discovered_project.as_deref(),
        Some(creds.project_id.as_str()),
        None,
    );
    set_last_project_id(Some(&project_id));

    let (summary_result, quota_summary_error) = match summary {
        Ok(result) => (Some(result), None),
        Err(error) => (None, Some(error)),
    };
    let (groups, group_description) = match &summary_result {
        Some(result) => parse_quota_summary(&result.data),
        None => (Vec::new(), None),
    };

    let available = available?;
    let (models, default_agent_model_id) = parse_models(&available.data);

    let assist_data = assist.as_ref().map_or(Value::Null, |result| result.data.clone());
    let assist_raw: LoadCodeAssistRaw =
        serde_json::from_value(assist_data).unwrap_or_default();
    let product_tier = parse_tier(assist_raw.current_tier.as_ref());
    let paid_tier = parse_tier(assist_raw.paid_tier.as_ref());

    // Google returns currentTier=free-tier even for Google AI Pro accounts; the real
    // subscription lives in paidTier (e.g. g1-pro-tier / Google AI Pro).
    let plan_label = paid_tier
        .as_ref()
        .and_then(|tier| tier.name.as_ref().map(|name| tier_label(name, tier.id.as_deref())))
        .or_else(|| {
            product_tier
                .as_ref()
                .and_then(|tier| tier.name.as_ref().map(|name| tier_label(name, tier.id.as_deref())))
        });

    let endpoint = summary_result
        .as_ref()
        .map(|result| result.endpoint.clone())
        .or_else(|| Some(available.endpoint.clone()))
        .or_else(|| assist.map(|result| result.endpoint));

    Ok(AccountUsage {
        project_id,
        endpoint,
        email: None,
        product_tier,
        paid_tier,
        plan_label,
        groups,
        group_description,
        quota_summary_error,
        models,
        default_agent_model_id,
        fetched_at: crate::utils_inner::diagnostics::now_ms(),
    })
}

fn tier_label(name: &str, id: Option<&str>) -> String {
    match id {
        Some(id) if !id.is_empty() => format!("{name} ({id})"),
        _ => name.to_string(),
    }
}

fn quota_error_note(message: &str) -> String {
    let pattern = regex::Regex::new(r"(?i)SUBSCRIPTION_REQUIRED|#3501|(?:lack|missing).*license")
        .expect("the quota error pattern compiles");
    if pattern.is_match(message) {
        return "Aggregate quota summary needs a paid subscription (free-tier can't use that endpoint). Per-model usage is still available via /antigravity.models.".to_string();
    }
    format!(
        "Aggregate quota summary unavailable: {}",
        take_chars(message, 160)
    )
}

/// `formatUsageSummary` in `usage.ts`.
#[must_use]
pub fn format_usage_summary(usage: &AccountUsage) -> String {
    let mut lines: Vec<String> = Vec::new();
    if let Some(plan_label) = &usage.plan_label {
        lines.push(plan_label.clone());
    }

    if usage.groups.is_empty() {
        if let Some(error) = &usage.quota_summary_error {
            lines.push(quota_error_note(error));
        } else {
            lines.push("No quota groups returned.".to_string());
        }
        return lines.join("\n");
    }

    for group in &usage.groups {
        if !lines.is_empty() {
            lines.push(String::new());
        }
        lines.push(group.display_name.clone());
        for bucket in &group.buckets {
            let remaining = remaining_percent(Some(bucket.remaining_fraction));
            lines.push(format!(
                "  {} {}: {}% left · resets {}",
                progress_bar(Some(bucket.remaining_fraction), 20),
                bucket.display_name,
                remaining.map_or_else(|| "?".to_string(), js_number),
                format_reset(bucket.reset_time.as_deref()),
            ));
        }
    }

    lines.join("\n").trim_end().to_string()
}

/// `formatModelsList` in `usage.ts`.
#[must_use]
pub fn format_models_list(usage: &AccountUsage, all: bool) -> String {
    let mut lines = vec![
        "Antigravity available models".to_string(),
        format!("project={}", usage.project_id),
    ];
    if let Some(default_model) = &usage.default_agent_model_id {
        lines.push(format!("defaultAgentModel={default_model}"));
    }
    lines.push(String::new());

    let rows: Vec<&ModelQuotaRow> = if all {
        usage.models.iter().collect()
    } else {
        usage
            .models
            .iter()
            .filter(|row| {
                let lower = row.model_id.to_lowercase();
                !lower.contains("tab_") && !lower.contains("chat_")
            })
            .collect()
    };

    if rows.is_empty() {
        lines.push("No models returned.".to_string());
        return lines.join("\n");
    }

    let max_id = rows
        .iter()
        .map(|row| row.model_id.chars().count())
        .max()
        .unwrap_or(0)
        .max(8);

    for row in rows {
        let remaining = remaining_percent(row.remaining_fraction);
        let mut flags = Vec::new();
        if row.recommended.unwrap_or(false) {
            flags.push("recommended");
        }
        if row.supports_thinking.unwrap_or(false) {
            flags.push("thinking");
        }
        if row.supports_images.unwrap_or(false) {
            flags.push("images");
        }
        let flags = flags.join(",");
        let name = match &row.display_name {
            Some(display) if display != &row.model_id => format!("  {display}"),
            _ => String::new(),
        };
        let remaining_text = match remaining {
            Some(value) => pad_start(&js_number(value), 5),
            None => "  ?".to_string(),
        };
        let reset = pad_end(&format_reset(row.reset_time.as_deref()), 8);
        let flags_text = if flags.is_empty() {
            String::new()
        } else {
            format!("  [{flags}]")
        };
        lines.push(format!(
            "{}  rem {}%  reset {reset}{flags_text}{name}",
            pad_end(&row.model_id, max_id),
            remaining_text,
        ));
    }
    lines.push(String::new());
    lines.push("Note: remaining % is pool-shared (not a private per-model budget).".to_string());
    lines.join("\n")
}

fn pad_end(value: &str, width: usize) -> String {
    let length = value.chars().count();
    if length >= width {
        value.to_string()
    } else {
        format!("{value}{}", " ".repeat(width - length))
    }
}

fn pad_start(value: &str, width: usize) -> String {
    let length = value.chars().count();
    if length >= width {
        value.to_string()
    } else {
        format!("{}{value}", " ".repeat(width - length))
    }
}

/// `resolveApiKeyFromContext` in `usage.ts`: the Rust equivalent receives the already-resolved
/// provider key (there is no Pi command context in this crate) and applies the same empty check.
#[must_use]
pub fn resolve_api_key_from_context(api_key: Option<&str>) -> Option<String> {
    api_key
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

// ---------------------------------------------------------------------------------------------
// ISO 8601 parsing (the subset `Date.parse` handles for quota reset times)
// ---------------------------------------------------------------------------------------------

fn parse_iso8601_ms(value: &str) -> Option<i64> {
    let value = value.trim();
    let separator = value.find('T').or_else(|| value.find(' '))?;
    let (date, rest) = value.split_at(separator);
    let time = rest.get(1..)?;

    let mut date_parts = date.split('-');
    let year: i64 = date_parts.next()?.parse().ok()?;
    let month: i64 = date_parts.next()?.parse().ok()?;
    let day: i64 = date_parts.next()?.parse().ok()?;

    let (time, offset_minutes) = split_offset(time)?;
    let mut time_parts = time.split(':');
    let hour: i64 = time_parts.next()?.parse().ok()?;
    let minute: i64 = time_parts.next().unwrap_or("0").parse().ok()?;
    let seconds_field = time_parts.next().unwrap_or("0");
    let (second, millis) = match seconds_field.split_once('.') {
        Some((seconds, fraction)) => {
            let second: i64 = seconds.parse().ok()?;
            let fraction = format!("{fraction:0<3}");
            let millis: i64 = fraction.get(..3)?.parse().ok()?;
            (second, millis)
        }
        None => (seconds_field.parse().ok()?, 0),
    };

    let days = days_from_civil(year, month, day);
    Some(
        days * 86_400_000 + (hour * 3600 + minute * 60 + second) * 1000 + millis
            - offset_minutes * 60_000,
    )
}

fn split_offset(time: &str) -> Option<(&str, i64)> {
    if let Some(stripped) = time.strip_suffix('Z').or_else(|| time.strip_suffix('z')) {
        return Some((stripped, 0));
    }
    let after_first = time.get(1..).unwrap_or("");
    if let Some(index) = after_first.find('+').map(|index| index + 1) {
        let (base, offset) = time.split_at(index);
        return parse_offset(base, offset.get(1..).unwrap_or(""), 1);
    }
    if let Some(index) = after_first.find('-').map(|index| index + 1) {
        let (base, offset) = time.split_at(index);
        return parse_offset(base, offset.get(1..).unwrap_or(""), -1);
    }
    Some((time, 0))
}

fn parse_offset<'a>(base: &'a str, offset: &'a str, sign: i64) -> Option<(&'a str, i64)> {
    let (hours, minutes) = offset.split_once(':').unwrap_or((offset, "0"));
    let hours: i64 = hours.parse().ok()?;
    let minutes: i64 = minutes.parse().ok()?;
    Some((base, sign * (hours * 60 + minutes)))
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let shifted_month = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_usage() -> AccountUsage {
        AccountUsage {
            project_id: "test".to_string(),
            endpoint: Some("test".to_string()),
            email: None,
            product_tier: None,
            paid_tier: None,
            plan_label: None,
            groups: Vec::new(),
            group_description: None,
            quota_summary_error: None,
            models: Vec::new(),
            default_agent_model_id: None,
            fetched_at: 0,
        }
    }

    #[test]
    fn usage_formatter_handles_the_3501_error() {
        let message_3501 = "/v1internal:retrieveUserQuotaSummary failed: You are currently configured to use a Google Cloud Project but lack a Gemini Code Assist license. Please contact your administrator to request a license. (#3501)";
        let output = format_usage_summary(&AccountUsage {
            quota_summary_error: Some(message_3501.to_string()),
            ..base_usage()
        });
        assert!(
            output.contains("needs a paid subscription")
                || output.contains("free-tier can't use that endpoint")
        );
    }

    #[test]
    fn usage_formatter_renders_groups_and_buckets() {
        let usage = AccountUsage {
            plan_label: Some("Google AI Pro (g1-pro-tier)".to_string()),
            groups: vec![QuotaGroup {
                display_name: "Gemini".to_string(),
                description: None,
                buckets: vec![QuotaBucket {
                    bucket_id: "requests".to_string(),
                    display_name: "Requests".to_string(),
                    window: None,
                    reset_time: None,
                    description: None,
                    remaining_fraction: 0.5,
                }],
            }],
            ..base_usage()
        };
        let output = format_usage_summary(&usage);
        assert!(output.starts_with("Google AI Pro (g1-pro-tier)"));
        assert!(output.contains("Gemini"));
        assert!(output.contains("[##########----------] Requests: 50% left · resets n/a"));
    }

    #[test]
    fn parses_quota_groups_and_models() {
        let (groups, description) = parse_quota_summary(&json!({
            "description": "pool",
            "groups": [
                { "displayName": "Gemini", "buckets": [
                    { "bucketId": "req", "displayName": "Requests", "remainingFraction": 1.7 },
                    { "displayName": "No id but fraction", "remainingFraction": 0.25 },
                    { "bucketId": "no-fraction" },
                    { "remainingFraction": 0.5 },
                ]},
                { "buckets": [ { "bucketId": "orphan" } ] },
            ],
        }));
        assert_eq!(description.as_deref(), Some("pool"));
        // Reference parity: a bucket survives when it has a fraction OR a truthy bucketId, and
        // a group survives when it has buckets or a name, so both groups are kept and the
        // fraction-only bucket falls back to "unknown".
        assert_eq!(groups.len(), 2);
        let buckets = &groups[0].buckets;
        assert_eq!(buckets.len(), 4);
        assert_eq!(buckets[0].bucket_id, "req");
        assert_eq!(buckets[0].remaining_fraction, 1.0);
        assert_eq!(buckets[1].bucket_id, "No id but fraction");
        assert_eq!(buckets[1].remaining_fraction, 0.25);
        assert_eq!(buckets[2].bucket_id, "no-fraction");
        assert_eq!(buckets[2].remaining_fraction, 0.0);
        assert_eq!(buckets[3].bucket_id, "unknown");
        assert_eq!(buckets[3].remaining_fraction, 0.5);
        assert_eq!(groups[1].display_name, "Quota group");
        assert_eq!(groups[1].buckets.len(), 1);
        assert_eq!(groups[1].buckets[0].bucket_id, "orphan");
        assert_eq!(groups[1].buckets[0].remaining_fraction, 0.0);

        let (models, default_model) = parse_models(&json!({
            "models": {
                "gemini-3-flash": { "displayName": "Gemini 3 Flash", "quotaInfo": { "remainingFraction": 0.5 }, "recommended": true },
                "chat_hidden": { "quotaInfo": {} },
                "internal": { "isInternal": true },
                "gemini-3-pro": { "label": "Gemini 3 Pro", "supportsThinking": true },
            },
            "defaultAgentModelId": "gemini-3-flash",
        }));
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].model_id, "gemini-3-flash");
        assert_eq!(models[0].display_name.as_deref(), Some("Gemini 3 Flash"));
        assert_eq!(models[0].remaining_fraction, Some(0.5));
        assert_eq!(models[0].recommended, Some(true));
        assert_eq!(models[1].model_id, "gemini-3-pro");
        assert_eq!(models[1].display_name.as_deref(), Some("Gemini 3 Pro"));
        assert_eq!(models[1].supports_thinking, Some(true));
        assert_eq!(default_model.as_deref(), Some("gemini-3-flash"));
    }

    #[test]
    fn parses_tiers_and_project_ids() {
        assert_eq!(
            parse_tier(Some(&json!({ "name": "Google AI Pro", "id": "g1-pro-tier" }))),
            Some(TierInfo {
                id: Some("g1-pro-tier".to_string()),
                name: Some("Google AI Pro".to_string()),
                description: None,
            })
        );
        assert_eq!(parse_tier(Some(&json!({}))), None);
        assert_eq!(parse_tier(None), None);

        assert_eq!(
            extract_project_id(&json!({ "cloudaicompanionProject": "p-1" })),
            Some("p-1".to_string())
        );
        assert_eq!(
            extract_project_id(&json!({ "project": { "id": "p-2" } })),
            Some("p-2".to_string())
        );
        assert_eq!(
            extract_project_id(&json!({ "projects": ["p-3"] })),
            Some("p-3".to_string())
        );
        assert_eq!(extract_project_id(&json!({ "other": true })), None);
    }

    #[test]
    fn formats_the_models_list() {
        let usage = AccountUsage {
            project_id: "proj".to_string(),
            default_agent_model_id: Some("gemini-3-flash".to_string()),
            models: vec![
                ModelQuotaRow {
                    model_id: "gemini-3-flash".to_string(),
                    display_name: Some("Gemini 3 Flash".to_string()),
                    remaining_fraction: Some(0.5),
                    reset_time: None,
                    model_provider: None,
                    supports_thinking: Some(true),
                    supports_images: Some(false),
                    recommended: Some(true),
                },
                ModelQuotaRow {
                    model_id: "tab_hidden".to_string(),
                    display_name: None,
                    remaining_fraction: None,
                    reset_time: None,
                    model_provider: None,
                    supports_thinking: Some(false),
                    supports_images: Some(false),
                    recommended: Some(false),
                },
            ],
            ..base_usage()
        };
        let output = format_models_list(&usage, false);
        assert!(output.contains("project=proj"));
        assert!(output.contains("defaultAgentModel=gemini-3-flash"));
        assert!(output.contains("gemini-3-flash"));
        assert!(!output.contains("tab_hidden"));
        assert!(output.contains("[recommended,thinking]"));
        assert!(format_models_list(&usage, true).contains("tab_hidden"));
    }

    #[test]
    fn parses_iso_reset_times() {
        assert_eq!(parse_iso8601_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso8601_ms("2026-10-12T17:02:03.456Z"), Some(
            days_from_civil(2026, 10, 12) * 86_400_000 + (17 * 3600 + 2 * 60 + 3) * 1000 + 456
        ));
        assert_eq!(parse_iso8601_ms("not-a-date"), None);
        assert_eq!(format_reset(None), "n/a");
        assert_eq!(format_reset(Some("not-a-date")), "not-a-date");
    }

    #[tokio::test]
    async fn diagnostics_snapshot_round_trips_redacts_and_isolates() {
        reset_diagnostics_for_tests();
        set_last_status(Some(429));
        set_last_endpoint(Some("https://example.test"));
        set_last_error(Some("Bearer ya29.SECRETSECRET"));
        let snapshot = get_last_diagnostics();
        assert_eq!(snapshot.status, Some(429));
        assert_eq!(snapshot.endpoint.as_deref(), Some("https://example.test"));
        assert_eq!(
            snapshot.error.as_deref(),
            Some("Bearer [redacted-access-token]")
        );

        reset_diagnostics_for_tests();
        assert_eq!(get_last_diagnostics(), DiagnosticsSnapshot::default());

        let value = run_with_diagnostics(async {
            set_last_endpoint(Some("https://inside.test"));
            get_current_endpoint()
        })
        .await;
        assert_eq!(value.as_deref(), Some("https://inside.test"));
        // The committed snapshot survives after the scope ends.
        assert_eq!(
            get_last_diagnostics().endpoint.as_deref(),
            Some("https://inside.test")
        );
    }
}
