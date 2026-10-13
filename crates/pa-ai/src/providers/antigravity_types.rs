//! Shared Antigravity wire types, enums, and low-level helpers.
//!
//! Ported from `pi-antigravity` (extension 0.10.0):
//!
//! - `src/types/enums.ts` and `src/types/types.ts` — the shared wire surface.
//! - The `src/client/client.ts` and `src/utils/{util,security}.ts` helpers that the
//!   `antigravity_search`, `antigravity_image`, and `antigravity_usage` modules call.
//!
//! Integration notes for the parent agent:
//!
//! - `crate::providers::antigravity_models` already owns the endpoint and request-envelope
//!   constants, so they are re-exported here instead of redefined.
//! - The `client.ts` / `utils/*.ts` helpers near the bottom of this file are the transport
//!   worker's long-term home (`antigravity_util.rs`). They live here only so the extras
//!   modules compile before that file lands; unify them when both are in.
//! - The full request envelope (`antigravityRequestEnvelope`: sessionId, labels, `model_enum`,
//!   deterministic ids) is transport-owned; only the `requestId` shape the search module needs
//!   is provided here.
//! - `types.ts` items that map onto `pa-types` (`TextContent`, `ThinkingContent`, `ToolCall`,
//!   `SimpleStreamOptions`) or are transport/oauth internals (`CallbackServer`, per-model
//!   `AntigravityRouting`, `AntigravityStreamOptions`) are not duplicated here.
#![allow(dead_code)] // parity surface: not every ported item has a caller yet

use std::future::Future;
use std::pin::Pin;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

// Constants/helpers owned by the streaming transport (`antigravity.rs`), re-exported so the
// extras modules have one import path and never redefine them.
pub use crate::providers::antigravity::{
    antigravity_env, antigravity_headers, endpoint_candidates, ANTIGRAVITY_CLI_USER_AGENT,
};
// Request-type constants owned by the model catalog (`antigravity_models.rs`).
pub use crate::providers::antigravity_models::{
    AntigravityRouting, ANTIGRAVITY_ENDPOINT_FALLBACKS, ANTIGRAVITY_USER_AGENT, API_ANTIGRAVITY,
    DEFAULT_ANTIGRAVITY_BASE_URL, REQUEST_TYPE_AGENT, REQUEST_TYPE_IMAGE_GEN,
};

/// The extension's `isExtraToolEnabled(tool)`: the umbrella `NO_EXTRA_TOOLS` toggle and the
/// per-tool `NO_<TOOL>_TOOL` toggle both disable the tool; only the value `"1"` counts.
#[must_use]
pub fn is_extra_tool_enabled(tool: &str) -> bool {
    crate::providers::antigravity_models::extra_tool_enabled_values(
        antigravity_env("NO_EXTRA_TOOLS").as_deref(),
        antigravity_env(&format!("NO_{tool}_TOOL")).as_deref(),
    )
}

// ---------------------------------------------------------------------------------------------
// enums.ts
// ---------------------------------------------------------------------------------------------

/// Reasoning effort levels (`ThinkingEffort` in `enums.ts`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThinkingEffort {
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
}

impl ThinkingEffort {
    /// Wire value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            ThinkingEffort::Off => "off",
            ThinkingEffort::Minimal => "minimal",
            ThinkingEffort::Low => "low",
            ThinkingEffort::Medium => "medium",
            ThinkingEffort::High => "high",
            ThinkingEffort::Xhigh => "xhigh",
        }
    }
}

/// Provider tool-selection mode (`ToolChoice` in `enums.ts`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolChoice {
    Auto,
    None,
    Any,
    Required,
}

impl ToolChoice {
    /// Wire value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            ToolChoice::Auto => "auto",
            ToolChoice::None => "none",
            ToolChoice::Any => "any",
            ToolChoice::Required => "required",
        }
    }
}

/// Gemini `functionCallingConfig.mode` (`GeminiToolCallingMode` in `enums.ts`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum GeminiToolCallingMode {
    #[serde(rename = "NONE")]
    None,
    #[serde(rename = "ANY")]
    Any,
    #[serde(rename = "AUTO")]
    Auto,
    #[serde(rename = "VALIDATED")]
    Validated,
}

impl GeminiToolCallingMode {
    /// Wire value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            GeminiToolCallingMode::None => "NONE",
            GeminiToolCallingMode::Any => "ANY",
            GeminiToolCallingMode::Auto => "AUTO",
            GeminiToolCallingMode::Validated => "VALIDATED",
        }
    }
}

/// Gemini content role (`GeminiRole` in `enums.ts`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum GeminiRole {
    #[serde(rename = "user")]
    User,
    #[serde(rename = "model")]
    Model,
}

impl GeminiRole {
    /// Wire value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            GeminiRole::User => "user",
            GeminiRole::Model => "model",
        }
    }
}

/// `requestType` envelope value (`AntigravityRequestType` in `enums.ts`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AntigravityRequestType {
    #[serde(rename = "agent")]
    Agent,
    #[serde(rename = "image_gen")]
    ImageGen,
}

impl AntigravityRequestType {
    /// Wire value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            AntigravityRequestType::Agent => REQUEST_TYPE_AGENT,
            AntigravityRequestType::ImageGen => REQUEST_TYPE_IMAGE_GEN,
        }
    }
}

/// `userAgent` envelope value (`AntigravityUserAgent` in `enums.ts`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AntigravityUserAgent {
    #[serde(rename = "antigravity")]
    Antigravity,
}

impl AntigravityUserAgent {
    /// Wire value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        ANTIGRAVITY_CLI_USER_AGENT
    }
}

/// Provider-level stop reasons (`StopReason` in `enums.ts`). Named with the `Antigravity`
/// prefix so it does not collide with `crate::types::StopReason`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AntigravityStopReason {
    Stop,
    Length,
    ToolUse,
    Error,
}

impl AntigravityStopReason {
    /// Wire value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            AntigravityStopReason::Stop => "stop",
            AntigravityStopReason::Length => "length",
            AntigravityStopReason::ToolUse => "toolUse",
            AntigravityStopReason::Error => "error",
        }
    }
}

// ---------------------------------------------------------------------------------------------
// types.ts — auth, models, Gemini wire shapes, stream shapes
// ---------------------------------------------------------------------------------------------

/// API identifier for the Antigravity chat API (`ANTIGRAVITY_API` in `types.ts`).
pub const ANTIGRAVITY_API: &str = "antigravity-api";

/// Credentials blob carried in the provider API key (`AntigravityApiKey` in `types.ts`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AntigravityApiKey {
    pub token: String,
    pub project_id: String,
}

/// One dynamically discovered runtime model (`DynamicModelInfo` in `types.ts`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DynamicModelInfo {
    pub id: String,
    pub experiments: Option<Vec<String>>,
    pub api_provider: Option<String>,
    pub model_provider: Option<String>,
    pub model: Option<String>,
}

/// `GeminiTextPart` in `types.ts`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiTextPart {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,
}

/// Inline base64 payload (`GeminiInlineDataPart.inlineData`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiInlineData {
    pub mime_type: String,
    pub data: String,
}

/// `GeminiInlineDataPart` in `types.ts`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiInlineDataPart {
    pub inline_data: GeminiInlineData,
}

/// `GeminiThoughtPart` in `types.ts`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiThoughtPart {
    pub thought: bool,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,
}

/// `GeminiFunctionCallPart.functionCall` in `types.ts`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiFunctionCall {
    pub name: String,
    #[serde(default)]
    pub args: Map<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

/// `GeminiFunctionCallPart` in `types.ts`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiFunctionCallPart {
    pub function_call: GeminiFunctionCall,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,
}

/// `GeminiFunctionResponsePart.functionResponse` in `types.ts`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiFunctionResponse {
    pub name: String,
    pub response: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

/// `GeminiFunctionResponsePart` in `types.ts`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiFunctionResponsePart {
    pub function_response: GeminiFunctionResponse,
}

/// `GeminiPart` in `types.ts` (a structural union of the five part shapes). Untagged so the
/// JSON matches whichever shape is present; the order mirrors the TS union.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum GeminiPart {
    Thought(GeminiThoughtPart),
    Text(GeminiTextPart),
    InlineData(GeminiInlineDataPart),
    FunctionCall(GeminiFunctionCallPart),
    FunctionResponse(GeminiFunctionResponsePart),
}

/// `GeminiContent` in `types.ts`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiContent {
    pub role: GeminiRole,
    pub parts: Vec<GeminiPart>,
}

/// `GeminiFunctionDeclaration` in `types.ts`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiFunctionDeclaration {
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters_json_schema: Option<Value>,
}

/// `GeminiToolConfig.functionCallingConfig` in `types.ts`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiFunctionCallingConfig {
    pub mode: GeminiToolCallingMode,
}

/// `GeminiToolConfig` in `types.ts`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiToolConfig {
    pub function_calling_config: GeminiFunctionCallingConfig,
}

/// `ThinkingWire` in `types.ts` (the `generationConfig.thinkingConfig` shape).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingWire {
    #[serde(default)]
    pub include_thoughts: bool,
    #[serde(default)]
    pub thinking_budget: i64,
}

/// `GeminiGenerationConfig` in `types.ts`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiGenerationConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_config: Option<ThinkingWire>,
}

/// `GeminiRequestBody.systemInstruction` in `types.ts`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiSystemInstruction {
    pub role: GeminiRole,
    pub parts: Vec<GeminiTextPart>,
}

/// `GeminiRequestBody` in `types.ts`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiRequestBody {
    pub contents: Vec<GeminiContent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_instruction: Option<GeminiSystemInstruction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation_config: Option<GeminiGenerationConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_config: Option<GeminiToolConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<Map<String, Value>>,
}

/// `AntigravityGenerateRequest` in `types.ts`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AntigravityGenerateRequest {
    pub project: String,
    pub model: String,
    pub request: GeminiRequestBody,
    pub request_type: AntigravityRequestType,
    pub user_agent: AntigravityUserAgent,
    pub request_id: String,
}

/// `StreamPart` in `types.ts`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamPart {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thought: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function_call: Option<StreamFunctionCall>,
}

/// `StreamPart.functionCall` in `types.ts`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamFunctionCall {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Map<String, Value>>,
}

/// `StreamUsageMetadata` in `types.ts`; unknown fields are kept in `extra`.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamUsageMetadata {
    #[serde(default)]
    pub prompt_token_count: Option<i64>,
    #[serde(default)]
    pub cached_content_token_count: Option<i64>,
    #[serde(default)]
    pub candidates_token_count: Option<i64>,
    #[serde(default)]
    pub thoughts_token_count: Option<i64>,
    #[serde(default)]
    pub total_token_count: Option<i64>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// `StreamCandidate.content` in `types.ts`.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamContent {
    #[serde(default)]
    pub parts: Vec<StreamPart>,
}

/// `StreamCandidate` in `types.ts`.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamCandidate {
    #[serde(default)]
    pub content: Option<StreamContent>,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

/// `StreamResponseData` in `types.ts`.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamResponseData {
    #[serde(default)]
    pub candidates: Vec<StreamCandidate>,
    #[serde(default)]
    pub usage_metadata: Option<StreamUsageMetadata>,
}

/// `StreamChunk.error` in `types.ts`.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamError {
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub code: Option<i64>,
}

/// `StreamChunk` in `types.ts` (`StreamResponseData` plus its optional error/response wrapper).
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamChunk {
    #[serde(default)]
    pub candidates: Vec<StreamCandidate>,
    #[serde(default)]
    pub usage_metadata: Option<StreamUsageMetadata>,
    #[serde(default)]
    pub error: Option<StreamError>,
    #[serde(default)]
    pub response: Option<StreamResponseData>,
}

// ---------------------------------------------------------------------------------------------
// types.ts — usage & quota shapes
// ---------------------------------------------------------------------------------------------

/// One quota bucket (`QuotaBucket` in `types.ts`).
#[derive(Clone, Debug, PartialEq)]
pub struct QuotaBucket {
    pub bucket_id: String,
    pub display_name: String,
    pub window: Option<String>,
    pub reset_time: Option<String>,
    pub description: Option<String>,
    pub remaining_fraction: f64,
}

/// A quota group (`QuotaGroup` in `types.ts`).
#[derive(Clone, Debug, PartialEq)]
pub struct QuotaGroup {
    pub display_name: String,
    pub description: Option<String>,
    pub buckets: Vec<QuotaBucket>,
}

/// One model row for the models list (`ModelQuotaRow` in `types.ts`).
#[derive(Clone, Debug, PartialEq)]
pub struct ModelQuotaRow {
    pub model_id: String,
    pub display_name: Option<String>,
    pub remaining_fraction: Option<f64>,
    pub reset_time: Option<String>,
    pub model_provider: Option<String>,
    pub supports_thinking: Option<bool>,
    pub supports_images: Option<bool>,
    pub recommended: Option<bool>,
}

/// Subscription tier (`TierInfo` in `types.ts`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TierInfo {
    pub id: Option<String>,
    pub name: Option<String>,
    pub description: Option<String>,
}

/// Account usage snapshot (`AccountUsage` in `types.ts`).
///
/// Deviation: TS declares `endpoint: string`, but `fetchAccountUsage` can leave it undefined
/// when no endpoint reported one, so it is `Option<String>` here.
#[derive(Clone, Debug, PartialEq)]
pub struct AccountUsage {
    pub project_id: String,
    pub endpoint: Option<String>,
    pub email: Option<String>,
    pub product_tier: Option<TierInfo>,
    pub paid_tier: Option<TierInfo>,
    pub plan_label: Option<String>,
    pub groups: Vec<QuotaGroup>,
    pub group_description: Option<String>,
    /// Set when the subscription-gated quota-summary RPC was unavailable (e.g. free-tier
    /// `SUBSCRIPTION_REQUIRED`).
    pub quota_summary_error: Option<String>,
    pub models: Vec<ModelQuotaRow>,
    pub default_agent_model_id: Option<String>,
    pub fetched_at: u64,
}

/// `ApiErrorBody` in `types.ts`.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct ApiErrorBody {
    #[serde(default)]
    pub error: Option<Value>,
    #[serde(default)]
    pub raw: Option<String>,
}

/// `QuotaBucketRaw` in `types.ts` (every field is `unknown` in TS).
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaBucketRaw {
    #[serde(default)]
    pub bucket_id: Option<Value>,
    #[serde(default)]
    pub display_name: Option<Value>,
    #[serde(default)]
    pub window: Option<Value>,
    #[serde(default)]
    pub reset_time: Option<Value>,
    #[serde(default)]
    pub description: Option<Value>,
    #[serde(default)]
    pub remaining_fraction: Option<Value>,
}

/// `QuotaGroupRaw` in `types.ts`.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaGroupRaw {
    #[serde(default)]
    pub display_name: Option<Value>,
    #[serde(default)]
    pub description: Option<Value>,
    #[serde(default)]
    pub buckets: Option<Value>,
}

/// `QuotaSummaryRaw` in `types.ts`.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaSummaryRaw {
    #[serde(default)]
    pub description: Option<Value>,
    #[serde(default)]
    pub groups: Option<Value>,
}

/// `ModelInfoRaw` in `types.ts`.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelInfoRaw {
    #[serde(default)]
    pub is_internal: Option<Value>,
    #[serde(default)]
    pub display_name: Option<Value>,
    #[serde(default)]
    pub label: Option<Value>,
    #[serde(default)]
    pub model_name: Option<Value>,
    #[serde(default)]
    pub model: Option<Value>,
    #[serde(default)]
    pub model_provider: Option<Value>,
    #[serde(default)]
    pub api_provider: Option<Value>,
    #[serde(default)]
    pub supports_thinking: Option<Value>,
    #[serde(default)]
    pub supports_images: Option<Value>,
    #[serde(default)]
    pub recommended: Option<Value>,
    #[serde(default)]
    pub quota_info: Option<Value>,
}

/// `AvailableModelsRaw` in `types.ts`.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AvailableModelsRaw {
    #[serde(default)]
    pub models: Option<Value>,
    #[serde(default)]
    pub default_agent_model_id: Option<Value>,
    #[serde(default)]
    pub default_agent_model: Option<Value>,
}

/// `TierRaw` in `types.ts`.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TierRaw {
    #[serde(default)]
    pub id: Option<Value>,
    #[serde(default)]
    pub name: Option<Value>,
    #[serde(default)]
    pub description: Option<Value>,
}

/// `LoadCodeAssistRaw` in `types.ts`.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadCodeAssistRaw {
    #[serde(default)]
    pub current_tier: Option<Value>,
    #[serde(default)]
    pub paid_tier: Option<Value>,
}

// ---------------------------------------------------------------------------------------------
// HTTP seam
// ---------------------------------------------------------------------------------------------

/// One non-streaming Antigravity HTTP response: status plus the full body.
#[derive(Clone, Debug)]
pub struct AntigravityHttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

impl AntigravityHttpResponse {
    /// The body decoded as UTF-8 (lossy, like an HTTP text read).
    #[must_use]
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// The body parsed as JSON, or `null` when it is not valid JSON.
    #[must_use]
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    /// Whether the status is 2xx.
    #[must_use]
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// The transport seam the search/image/usage modules call. Tests script it; production uses
/// [`ReqwestAntigravityHttp`]. Follows the crate's existing `Pin<Box<dyn Future>>` seam style.
pub trait AntigravityHttp: Send + Sync {
    /// One JSON POST; the error string is the transport failure message.
    fn post<'a>(
        &'a self,
        url: &'a str,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
        signal: Option<CancellationToken>,
    ) -> Pin<Box<dyn Future<Output = Result<AntigravityHttpResponse, String>> + Send + 'a>>;
}

/// Production transport: the crate's shared reqwest client (already connection-pooled).
pub struct ReqwestAntigravityHttp;

impl ReqwestAntigravityHttp {
    #[must_use]
    pub fn new() -> Self {
        ReqwestAntigravityHttp
    }
}

impl Default for ReqwestAntigravityHttp {
    fn default() -> Self {
        Self::new()
    }
}

impl AntigravityHttp for ReqwestAntigravityHttp {
    fn post<'a>(
        &'a self,
        url: &'a str,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
        signal: Option<CancellationToken>,
    ) -> Pin<Box<dyn Future<Output = Result<AntigravityHttpResponse, String>> + Send + 'a>> {
        Box::pin(async move {
            let mut response =
                crate::utils_inner::http::send(crate::utils_inner::http::RequestOptions {
                    method: reqwest::Method::POST,
                    url: url.to_string(),
                    headers,
                    body: Some(String::from_utf8_lossy(&body).into_owned()),
                    signal,
                    timeout_ms: Some(30_000),
                    connection:
                        crate::utils_inner::stream_failure::ConnectionErrorProfile::RawFetch,
                    transport: crate::utils_inner::http::Transport::Http1,
                })
                .await
                .map_err(|error| error.to_string())?;
            let status = response.status;
            let text = response
                .read_all_text()
                .await
                .map_err(|error| error.to_string())?;
            Ok(AntigravityHttpResponse {
                status,
                body: text.into_bytes(),
            })
        })
    }
}

// ---------------------------------------------------------------------------------------------
// JS value semantics (JS `number` is f64; truthiness/string coercion as in the TS extension)
// ---------------------------------------------------------------------------------------------

/// JS `!!value` truthiness for a JSON value.
#[must_use]
pub fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|number| number != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// JS `String(value)` for a JSON value.
#[must_use]
pub fn js_string(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => match number.as_i64() {
            Some(value) => value.to_string(),
            None => number
                .as_f64()
                .map_or_else(|| number.to_string(), js_number),
        },
        Value::String(text) => text.clone(),
        Value::Array(items) => items.iter().map(js_string).collect::<Vec<_>>().join(","),
        Value::Object(_) => "[object Object]".to_string(),
    }
}

/// JS `String(number)` for an f64 (`50` not `50.0`, `33.3` as-is).
#[must_use]
pub fn js_number(value: f64) -> String {
    if value.is_finite() && value.fract() == 0.0 && value.abs() < 9.007_199_254_740_992e15 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

/// `value` when it is a JSON number.
#[must_use]
pub fn value_as_number(value: &Value) -> Option<f64> {
    value.as_f64()
}

/// `isRecord` in `utils/util.ts`.
#[must_use]
pub fn is_record(value: &Value) -> bool {
    value.is_object()
}

/// `asString` in `utils/util.ts` (a non-empty string, else `None`).
#[must_use]
pub fn as_string(value: &Value) -> Option<String> {
    match value {
        Value::String(text) if !text.is_empty() => Some(text.clone()),
        _ => None,
    }
}

/// `sanitizeText` in `utils/util.ts`, reusing the crate's surrogate sanitizer.
#[must_use]
pub fn sanitize_text(text: &str) -> String {
    crate::utils_inner::sanitize_unicode::sanitize_surrogates(text)
}

// ---------------------------------------------------------------------------------------------
// client.ts / util.ts helpers (transport's long-term home; see the module note)
// ---------------------------------------------------------------------------------------------

/// `parseApiKey` in `client.ts`.
pub fn parse_api_key(api_key_raw: Option<&str>) -> Result<AntigravityApiKey, String> {
    let Some(raw) = api_key_raw else {
        return Err("No Antigravity OAuth credentials. Run /login antigravity.".to_string());
    };
    match serde_json::from_str::<AntigravityApiKeyFile>(raw) {
        Ok(parsed) if !parsed.token.is_empty() && !parsed.project_id.is_empty() => {
            Ok(AntigravityApiKey {
                token: parsed.token,
                project_id: parsed.project_id,
            })
        }
        Ok(_) => Err(format!(
            "Invalid Antigravity credentials. Run /login antigravity. (missing token or projectId)"
        )),
        Err(error) => Err(format!(
            "Invalid Antigravity credentials. Run /login antigravity. ({})",
            safe_error(&error.to_string())
        )),
    }
}

#[derive(Deserialize)]
struct AntigravityApiKeyFile {
    #[serde(default)]
    token: String,
    #[serde(default, rename = "projectId")]
    project_id: String,
}

/// `jsonOrTextError` in `client.ts`.
#[must_use]
pub fn json_or_text_error(text: &str) -> String {
    if let Ok(parsed) = serde_json::from_str::<Value>(text) {
        if let Some(message) = parsed
            .get("error")
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
        {
            return message.to_string();
        }
    }
    text.to_string()
}

// ---------------------------------------------------------------------------------------------
// security.ts
// ---------------------------------------------------------------------------------------------

fn replace_all(value: &str, pattern: &str, replacement: &str, case_insensitive: bool) -> String {
    let pattern = if case_insensitive {
        format!("(?i){pattern}")
    } else {
        pattern.to_string()
    };
    match Regex::new(&pattern) {
        Ok(regex) => regex.replace_all(value, replacement).into_owned(),
        Err(_) => value.to_string(),
    }
}

/// `redactSecrets` in `utils/security.ts`.
#[must_use]
pub fn redact_secrets(text: &str) -> String {
    let text = replace_all(
        text,
        r"\bya29\.[A-Za-z0-9._~+/-]+=*",
        "[redacted-access-token]",
        false,
    );
    let text = replace_all(
        text.as_str(),
        r"\b1/[A-Za-z0-9_-]{20,}",
        "[redacted-refresh-token]",
        false,
    );
    let text = replace_all(
        text.as_str(),
        r"\bBearer\s+[A-Za-z0-9._~+/-]+=*",
        "Bearer [redacted]",
        true,
    );
    let text = replace_all(
        text.as_str(),
        r#"("?(?:access_token|refresh_token|id_token|token|client_secret|code_verifier|authorization)"?\s*[:=]\s*")[^"]*(")"#,
        "$1[redacted]$2",
        true,
    );
    replace_all(
        text.as_str(),
        r#"("?(?:access_token|refresh_token|id_token|token|client_secret|code_verifier|authorization)"?\s*[:=]\s*)[^\s&,}]+"#,
        "$1[redacted]",
        true,
    )
}

/// `safeError` in `utils/security.ts`.
#[must_use]
pub fn safe_error(message: &str) -> String {
    redact_secrets(message)
}

/// `stripValidationUrls` in `utils/security.ts`.
#[must_use]
pub fn strip_validation_urls(text: &str) -> String {
    let text = replace_all(
        text,
        r#"https://accounts\.google\.com/[^\s"'<>\\]+"#,
        "[redacted-validation-url]",
        false,
    );
    text.replace(
        " Open this link in a browser signed in to the same Google account to verify: [redacted-validation-url]",
        "",
    )
}

/// `maskEmail` in `utils/security.ts`.
#[must_use]
pub fn mask_email(email: Option<&str>) -> Option<String> {
    let email = email?;
    let mut parts = email.split('@');
    let name = parts.next().unwrap_or("");
    let domain = parts.next().unwrap_or("");
    if parts.next().is_some() || name.is_empty() || domain.is_empty() {
        return Some("[redacted-email]".to_string());
    }
    let characters: Vec<char> = name.chars().collect();
    let last = characters.last().copied().unwrap_or(' ');
    let masked = if characters.len() > 2 {
        format!("{}***{}", characters[0], last)
    } else {
        format!("{}***", characters[0])
    };
    Some(format!("{masked}@{domain}"))
}

// ---------------------------------------------------------------------------------------------
// ids
// ---------------------------------------------------------------------------------------------

/// A random RFC 4122 v4 UUID (`crypto.randomUUID()`).
#[must_use]
pub fn random_uuid_v4() -> String {
    use rand::RngCore;
    let mut bytes = [0_u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex = hex::encode(bytes);
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// The `requestId` half of `antigravityRequestEnvelope` in `utils/util.ts`
/// (`agent/<uuid>/<ms>/<uuid>/<step>`). The full envelope (sessionId, labels, `model_enum`) is
/// transport-owned.
#[must_use]
pub fn agent_request_id(step: u32) -> String {
    format!(
        "agent/{}/{}/{}/{}",
        random_uuid_v4(),
        crate::utils_inner::diagnostics::now_ms(),
        random_uuid_v4(),
        step.max(1)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn redacts_tokens_like_the_extension() {
        // Verified against the reference regex chain (executed with node): after the `ya29`
        // rule rewrites the token, the final `authorization` rule also masks the scheme word,
        // so this output is parity, not a bug.
        assert_eq!(
            redact_secrets("Authorization: Bearer ya29.ABCDEFGHIJKLMNOP"),
            "Authorization: [redacted] [redacted-access-token]"
        );
        assert_eq!(
            redact_secrets("access_token=secret-value"),
            "access_token=[redacted]"
        );
        assert_eq!(
            redact_secrets("refresh=1/ABCDEFGHIJKLMNOPQRSTUV"),
            "refresh=[redacted-refresh-token]"
        );
    }

    #[test]
    fn parse_api_key_matches_the_extension() {
        assert!(parse_api_key(None)
            .unwrap_err()
            .contains("No Antigravity OAuth credentials"));
        let good = parse_api_key(Some(r#"{"token":"t","projectId":"p"}"#)).unwrap();
        assert_eq!(good.token, "t");
        assert_eq!(good.project_id, "p");
        assert!(parse_api_key(Some("bad"))
            .unwrap_err()
            .contains("Invalid Antigravity credentials"));
        assert!(parse_api_key(Some(r#"{"token":"t"}"#))
            .unwrap_err()
            .contains("missing token or projectId"));
    }

    #[test]
    fn thinking_wire_deserializes_with_defaults() {
        let empty: ThinkingWire = serde_json::from_str("{}").unwrap();
        assert!(!empty.include_thoughts);
        assert_eq!(empty.thinking_budget, 0);

        let partial: ThinkingWire = serde_json::from_str(r#"{"includeThoughts": true}"#).unwrap();
        assert!(partial.include_thoughts);
        assert_eq!(partial.thinking_budget, 0);

        let budget_only: ThinkingWire =
            serde_json::from_str(r#"{"thinkingBudget": 2048}"#).unwrap();
        assert!(!budget_only.include_thoughts);
        assert_eq!(budget_only.thinking_budget, 2048);
    }

    #[test]
    fn endpoint_candidates_default_to_the_ordered_fallbacks() {
        std::env::remove_var("ANTIGRAVITY_BASE_URL");
        let candidates = endpoint_candidates();
        assert!(!candidates.is_empty());
        assert!(candidates
            .iter()
            .all(|endpoint| endpoint.starts_with("https://")));
    }

    #[test]
    fn masks_and_strips_like_the_extension() {
        assert_eq!(
            mask_email(Some("alice@example.com")).as_deref(),
            Some("a***e@example.com")
        );
        assert_eq!(
            mask_email(Some("ab@example.com")).as_deref(),
            Some("a***@example.com")
        );
        assert_eq!(mask_email(None), None);
        assert!(
            strip_validation_urls("see https://accounts.google.com/o/oauth2/approval?x=1 now")
                .contains("[redacted-validation-url]")
        );
    }

    #[test]
    fn js_value_semantics() {
        assert!(!js_truthy(&Value::Null));
        assert!(!js_truthy(&json!(0)));
        assert!(!js_truthy(&json!("")));
        assert!(js_truthy(&json!([])));
        assert_eq!(js_number(50.0), "50");
        assert_eq!(js_number(33.3), "33.3");
        assert_eq!(js_string(&json!(0)), "0");
    }

    #[test]
    fn uuid_is_v4_shaped() {
        let uuid = random_uuid_v4();
        assert_eq!(uuid.len(), 36);
        assert_eq!(&uuid[14..15], "4");
    }
}
