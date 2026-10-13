//! Antigravity model catalog and server-side wire tables.
//!
//! Ported from `pi-antigravity/src/models/{models.ts,discovery.ts,grouping.ts}` (extension
//! 0.10.0). This module owns the *mutable* catalog state the extension keeps in memory:
//! the current `AntigravityCatalog` (public models plus per-effort routing) and the dynamic
//! `MODEL_PLACEHOLDER_*` enum cache learned from discovery. The streaming transport in
//! `crate::providers::antigravity` asks it to resolve a public model id plus thinking effort
//! into the runtime ("wire") model id the Cloud Code Assist backend actually accepts, and to
//! look up the per-model output-token cap, wire thinking config, and label enum.
//!
//! The compiled fallback catalog (`models_generated.rs`) carries one entry per public model
//! id listed here; keep the two in sync when adding a model.

use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::types::{JsNumber, Model, ModelCost, ModelThinkingLevel};

/// API identifier the pa-ai registry serves for the Antigravity provider.
pub const API_ANTIGRAVITY: &str = "antigravity";

/// Human-readable provider name.
pub const ANTIGRAVITY_PROVIDER_NAME: &str = "Antigravity";

/// Primary Cloud Code Assist endpoint (`DEFAULT_ENDPOINT` in the extension's `client.ts`).
pub const DEFAULT_ANTIGRAVITY_BASE_URL: &str = "https://daily-cloudcode-pa.googleapis.com";

/// Endpoint candidates, tried in order (`ENDPOINT_FALLBACKS` in the extension's `client.ts`).
pub const ANTIGRAVITY_ENDPOINT_FALLBACKS: &[&str] = &[
    DEFAULT_ANTIGRAVITY_BASE_URL,
    "https://daily-cloudcode-pa.sandbox.googleapis.com",
    "https://cloudcode-pa.googleapis.com",
];

/// `requestType` envelope value for chat/agent traffic.
pub const REQUEST_TYPE_AGENT: &str = "agent";

/// `requestType` envelope value for image-generation traffic.
pub const REQUEST_TYPE_IMAGE_GEN: &str = "image_gen";

/// `userAgent` envelope value sent on every Antigravity request.
pub const ANTIGRAVITY_USER_AGENT: &str = "antigravity";

/// Runtime ("wire") model ids for one public model, per thinking effort.
///
/// `None` means the backend does not advertise that level. [`request_model_id`] applies the
/// same fallback order as the extension, so a missing level resolves to the nearest one the
/// backend does advertise instead of being sent verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AntigravityRouting {
    /// Runtime id used when thinking is off (also the default for unknown/absent effort).
    pub off: Option<String>,
    pub minimal: Option<String>,
    pub low: Option<String>,
    pub medium: Option<String>,
    pub high: Option<String>,
    pub xhigh: Option<String>,
    /// Last-resort runtime id when no level above resolves.
    pub default_request_id: Option<String>,
}

/// The compiled routing entry: string literals so the table itself can live in `.rodata`.
#[derive(Debug, Clone, Copy)]
struct StaticRouting {
    off: Option<&'static str>,
    minimal: Option<&'static str>,
    low: Option<&'static str>,
    medium: Option<&'static str>,
    high: Option<&'static str>,
    xhigh: Option<&'static str>,
    default_request_id: Option<&'static str>,
}

impl StaticRouting {
    fn expand(self) -> AntigravityRouting {
        let own = |value: Option<&'static str>| value.map(str::to_string);
        AntigravityRouting {
            off: own(self.off),
            minimal: own(self.minimal),
            low: own(self.low),
            medium: own(self.medium),
            high: own(self.high),
            xhigh: own(self.xhigh),
            default_request_id: own(self.default_request_id),
        }
    }
}

/// Public model id -> runtime routing. Mirrors `ANTIGRAVITY_ROUTING` in the extension.
///
/// `gemini-3.1-pro-high` and `gemini-3-flash-agent` are backend aliases: their runtime ids do
/// not share a thinking suffix with the public family, so they are listed on the public model
/// they belong to (`gemini-3.1-pro` and `gemini-3.5-flash`).
static STATIC_ROUTING: &[(&str, StaticRouting)] = &[
    (
        "claude-opus-4-6",
        StaticRouting {
            off: None,
            minimal: Some("claude-opus-4-6-thinking"),
            low: Some("claude-opus-4-6-thinking"),
            medium: Some("claude-opus-4-6-thinking"),
            high: Some("claude-opus-4-6-thinking"),
            xhigh: None,
            default_request_id: Some("claude-opus-4-6-thinking"),
        },
    ),
    (
        "claude-sonnet-4-6",
        StaticRouting {
            off: Some("claude-sonnet-4-6"),
            minimal: Some("claude-sonnet-4-6"),
            low: Some("claude-sonnet-4-6"),
            medium: Some("claude-sonnet-4-6"),
            high: Some("claude-sonnet-4-6"),
            xhigh: Some("claude-sonnet-4-6"),
            default_request_id: Some("claude-sonnet-4-6"),
        },
    ),
    (
        "gemini-3.1-pro",
        StaticRouting {
            off: Some("gemini-3.1-pro-low"),
            minimal: Some("gemini-3.1-pro-low"),
            low: Some("gemini-3.1-pro-low"),
            medium: Some("gemini-3.1-pro-low"),
            high: Some("gemini-pro-agent"),
            xhigh: Some("gemini-pro-agent"),
            default_request_id: Some("gemini-3.1-pro-low"),
        },
    ),
    (
        "gemini-3.5-flash",
        StaticRouting {
            off: Some("gemini-3.5-flash-extra-low"),
            minimal: Some("gemini-3.5-flash-extra-low"),
            low: Some("gemini-3.5-flash-extra-low"),
            medium: Some("gemini-3.5-flash-low"),
            high: Some("gemini-3-flash-agent"),
            xhigh: Some("gemini-3-flash-agent"),
            default_request_id: Some("gemini-3.5-flash-extra-low"),
        },
    ),
    (
        "gemini-3.6-flash",
        StaticRouting {
            off: Some("gemini-3.6-flash-low"),
            minimal: None,
            low: Some("gemini-3.6-flash-low"),
            medium: Some("gemini-3.6-flash-medium"),
            high: Some("gemini-3.6-flash-high"),
            xhigh: Some("gemini-3.6-flash-high"),
            default_request_id: Some("gemini-3.6-flash-low"),
        },
    ),
    (
        "gemini-3.7-flash",
        StaticRouting {
            off: Some("gemini-3.7-flash-low"),
            minimal: None,
            low: Some("gemini-3.7-flash-low"),
            medium: Some("gemini-3.7-flash-medium"),
            high: Some("gemini-3.7-flash-high"),
            xhigh: Some("gemini-3.7-flash-high"),
            default_request_id: Some("gemini-3.7-flash-low"),
        },
    ),
    (
        "gemini-3.8-flash",
        StaticRouting {
            off: Some("gemini-3.8-flash-low"),
            minimal: None,
            low: Some("gemini-3.8-flash-low"),
            medium: Some("gemini-3.8-flash-medium"),
            high: Some("gemini-3.8-flash-high"),
            xhigh: Some("gemini-3.8-flash-high"),
            default_request_id: Some("gemini-3.8-flash-low"),
        },
    ),
    (
        "gpt-oss-120b",
        StaticRouting {
            off: Some("gpt-oss-120b-medium"),
            minimal: Some("gpt-oss-120b-medium"),
            low: Some("gpt-oss-120b-medium"),
            medium: Some("gpt-oss-120b-medium"),
            high: Some("gpt-oss-120b-medium"),
            xhigh: Some("gpt-oss-120b-medium"),
            default_request_id: Some("gpt-oss-120b-medium"),
        },
    ),
];

fn static_routing(model_id: &str) -> Option<StaticRouting> {
    STATIC_ROUTING
        .iter()
        .find(|(id, _)| *id == model_id)
        .map(|(_, routing)| *routing)
}

/// One public model catalog plus its routing (the extension's `AntigravityCatalog`).
///
/// The extension stores `ProviderModelConfig[]`; here the entries are full [`Model`]s whose
/// `api`/`provider`/`base_url` are already filled in (the extension adds those later in
/// `toStoredModels`), so the catalog is directly usable by the transport.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AntigravityCatalog {
    pub models: Vec<Model>,
    pub routing: HashMap<String, AntigravityRouting>,
}

/// The compiled fallback catalog: the `models_generated.rs` entries plus the static routing.
#[must_use]
pub fn compiled_catalog() -> AntigravityCatalog {
    let models = crate::models_generated::get_models(API_ANTIGRAVITY)
        .into_iter()
        .cloned()
        .collect();
    let routing = STATIC_ROUTING
        .iter()
        .map(|(id, routing)| ((*id).to_string(), routing.expand()))
        .collect();
    AntigravityCatalog { models, routing }
}

fn state() -> &'static RwLock<AntigravityCatalog> {
    static STATE: OnceLock<RwLock<AntigravityCatalog>> = OnceLock::new();
    STATE.get_or_init(|| RwLock::new(compiled_catalog()))
}

fn read_state() -> AntigravityCatalog {
    state().read().unwrap().clone()
}

/// The current public models (`getCurrentAntigravityModels`).
#[must_use]
pub fn current_models() -> Vec<Model> {
    state().read().unwrap().models.clone()
}

/// The current routing (`getCurrentAntigravityRouting`).
#[must_use]
pub fn current_routing() -> HashMap<String, AntigravityRouting> {
    state().read().unwrap().routing.clone()
}

/// The current catalog (`getCurrentAntigravityCatalog`).
#[must_use]
pub fn current_catalog() -> AntigravityCatalog {
    read_state()
}

/// Replace the current catalog (`applyAntigravityCatalog`).
///
/// # Panics
///
/// Panics if the catalog lock is poisoned (a thread panicked while holding it).
pub fn apply_catalog(catalog: AntigravityCatalog) {
    *state().write().unwrap() = catalog;
}

/// Restore the compiled catalog (`resetAntigravityCatalogForTests`).
pub fn reset_catalog() {
    apply_catalog(compiled_catalog());
}

/// Effective routing for a public model id: the current catalog first, then the compiled
/// table (the extension's `currentRouting[id] ?? ANTIGRAVITY_ROUTING[id]`).
#[must_use]
pub fn routing(model_id: &str) -> Option<AntigravityRouting> {
    if let Some(found) = state().read().unwrap().routing.get(model_id) {
        return Some(found.clone());
    }
    static_routing(model_id).map(StaticRouting::expand)
}

/// Resolve public model id + thinking effort to the runtime model id sent on the wire.
///
/// Unmapped model ids are returned unchanged (custom models are probed dynamically at
/// request time). The fallback order matches `getAntigravityRequestModelId` in the extension.
#[must_use]
pub fn request_model_id(model_id: &str, effort: ModelThinkingLevel) -> String {
    let Some(r) = routing(model_id) else {
        return model_id.to_string();
    };
    let resolved = match effort {
        ModelThinkingLevel::Off => r.off.or(r.minimal).or(r.low).or(r.default_request_id),
        ModelThinkingLevel::Xhigh => r
            .xhigh
            .or(r.high)
            .or(r.low)
            .or(r.minimal)
            .or(r.off)
            .or(r.default_request_id),
        // The extension's extra `minimal` term after `low` is redundant.
        ModelThinkingLevel::Minimal => r.minimal.or(r.low).or(r.off).or(r.default_request_id),
        ModelThinkingLevel::Low => r.low.or(r.minimal).or(r.off).or(r.default_request_id),
        ModelThinkingLevel::Medium => {
            r.medium.or(r.low).or(r.minimal).or(r.off).or(r.default_request_id)
        }
        ModelThinkingLevel::High => {
            r.high.or(r.low).or(r.minimal).or(r.off).or(r.default_request_id)
        }
        // `Max` has no backend level: like the extension it falls through to `low`.
        ModelThinkingLevel::Max => r.low.or(r.minimal).or(r.off).or(r.default_request_id),
    };
    resolved.unwrap_or_else(|| model_id.to_string())
}

/// Runtime id for a model with thinking off (the extension's no-effort default).
#[must_use]
pub fn runtime_model(model_id: &str) -> String {
    request_model_id(model_id, ModelThinkingLevel::Off)
}

/// Next-gen fallback runtime id when the primary one is unavailable (`3.8 -> 3.7 -> 3.6`).
///
/// Mirrors `getFallbackRuntimeModel`; `None` means there is no fallback.
#[must_use]
pub fn fallback_runtime_model(runtime_model: &str, effort: ModelThinkingLevel) -> Option<String> {
    if let Some(tier) = runtime_model.strip_prefix("gemini-3.8-flash-") {
        return Some(format!("gemini-3.7-flash-{tier}"));
    }
    if runtime_model == "gemini-3.8-flash" {
        return Some("gemini-3.7-flash-low".to_string());
    }
    if runtime_model == "gemini-3.7-flash-tiered" {
        return Some(request_model_id("gemini-3.6-flash", effort));
    }
    if let Some(tier) = runtime_model.strip_prefix("gemini-3.7-flash-") {
        return Some(format!("gemini-3.6-flash-{tier}"));
    }
    if runtime_model == "gemini-3.7-flash" {
        return Some("gemini-3.6-flash-low".to_string());
    }
    None
}

/// Verified maximum output tokens the backend accepts, per model or runtime id.
///
/// Requesting more returns HTTP 400. Mirrors `RUNTIME_MAX_OUTPUT_TOKENS`.
static MAX_OUTPUT_TOKENS: &[(&str, u64)] = &[
    ("gemini-3.8-flash", 65_536),
    ("gemini-3.8-flash-low", 65_536),
    ("gemini-3.8-flash-medium", 65_536),
    ("gemini-3.8-flash-high", 65_536),
    ("gemini-3.7-flash", 65_536),
    ("gemini-3.7-flash-tiered", 65_536),
    // Rollout-era ids kept for pinned runtime overrides.
    ("gemini-3.7-flash-low", 65_536),
    ("gemini-3.7-flash-medium", 65_536),
    ("gemini-3.7-flash-high", 65_536),
    ("gemini-3.6-flash", 65_536),
    ("gemini-3.6-flash-low", 65_536),
    ("gemini-3.6-flash-medium", 65_536),
    ("gemini-3.6-flash-high", 65_536),
    ("gemini-3.5-flash", 65_536),
    ("gemini-3.5-flash-extra-low", 65_536),
    ("gemini-3.5-flash-low", 65_536),
    ("gemini-3-flash-agent", 65_536),
    ("gemini-3.1-pro", 65_535),
    ("gemini-3.1-pro-low", 65_535),
    ("gemini-3.1-pro-high", 65_535),
    ("gemini-pro-agent", 65_535),
    ("claude-opus-4-6", 64_000),
    ("claude-opus-4-6-thinking", 64_000),
    ("claude-sonnet-4-6", 64_000),
    ("gpt-oss-120b", 32_768),
    ("gpt-oss-120b-medium", 32_768),
];

fn max_output_tokens_lookup(key: &str) -> Option<u64> {
    MAX_OUTPUT_TOKENS
        .iter()
        .find(|(id, _)| *id == key)
        .map(|(_, tokens)| *tokens)
}

/// Maximum output tokens to request: runtime id first, then public id, then family default.
#[must_use]
pub fn max_output_tokens(model_id: &str, runtime_model: Option<&str>) -> u64 {
    if let Some(tokens) = runtime_model.and_then(max_output_tokens_lookup) {
        return tokens;
    }
    if let Some(tokens) = max_output_tokens_lookup(model_id) {
        return tokens;
    }
    if let Some(runtime) = runtime_model {
        if runtime.starts_with("claude-") {
            return 64_000;
        }
        if runtime.starts_with("gpt-oss-") {
            return 32_768;
        }
        if runtime.starts_with("gemini-3.1-pro") || runtime == "gemini-pro-agent" {
            return 65_535;
        }
        if runtime.starts_with("gemini-") {
            return 65_536;
        }
    }
    8192
}

/// Wire thinking config: `includeThoughts` plus a token budget (`-1` = dynamic).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThinkingWire {
    pub include_thoughts: bool,
    pub thinking_budget: i64,
}

impl ThinkingWire {
    const DISABLED: Self = Self {
        include_thoughts: false,
        thinking_budget: 0,
    };
}

/// Wire thinking config for a runtime model id, by family (mirrors `getThinkingConfig`).
///
/// Takes the *runtime* id, as the extension does; `None` for an unknown family.
#[must_use]
pub fn thinking_config(wire_model_id: &str, effort: ModelThinkingLevel) -> Option<ThinkingWire> {
    let off = matches!(effort, ModelThinkingLevel::Off);
    if wire_model_id.starts_with("claude-") {
        if off {
            return Some(ThinkingWire::DISABLED);
        }
        return Some(ThinkingWire {
            include_thoughts: true,
            thinking_budget: 1024,
        });
    }
    if wire_model_id.starts_with("gpt-oss-") {
        if off {
            return Some(ThinkingWire::DISABLED);
        }
        return Some(ThinkingWire {
            include_thoughts: true,
            thinking_budget: 8192,
        });
    }
    if wire_model_id.starts_with("gemini-3.5-flash") || wire_model_id == "gemini-3-flash-agent" {
        if off {
            return Some(ThinkingWire::DISABLED);
        }
        let thinking_budget = match effort {
            ModelThinkingLevel::High | ModelThinkingLevel::Xhigh => 10_000,
            ModelThinkingLevel::Medium => 4_000,
            _ => 1_000,
        };
        return Some(ThinkingWire {
            include_thoughts: true,
            thinking_budget,
        });
    }
    if wire_model_id.starts_with("gemini-3.1-pro") || wire_model_id == "gemini-pro-agent" {
        if off {
            return Some(ThinkingWire::DISABLED);
        }
        let thinking_budget = match effort {
            ModelThinkingLevel::High | ModelThinkingLevel::Xhigh => 10_001,
            _ => 1_001,
        };
        return Some(ThinkingWire {
            include_thoughts: true,
            thinking_budget,
        });
    }
    if wire_model_id.starts_with("gemini-") {
        if off {
            return Some(ThinkingWire::DISABLED);
        }
        let thinking_budget = match effort {
            ModelThinkingLevel::High | ModelThinkingLevel::Xhigh => -1,
            ModelThinkingLevel::Medium => 4_000,
            _ => 1_000,
        };
        return Some(ThinkingWire {
            include_thoughts: true,
            thinking_budget,
        });
    }
    None
}

/// Static `wire model id -> MODEL_PLACEHOLDER_* label` map (`ANTIGRAVITY_MODEL_ENUM`).
///
/// The extension also learns extra enums at runtime from `fetchAvailableModels` discovery;
/// that dynamic layer is [`register_model_enum`] / [`register_discovered_model_enums`].
static MODEL_ENUMS: &[(&str, &str)] = &[
    ("gemini-3.8-flash", "MODEL_PLACEHOLDER_M318"),
    ("gemini-3.8-flash-high", "MODEL_PLACEHOLDER_M318"),
    ("gemini-3.8-flash-medium", "MODEL_PLACEHOLDER_M319"),
    ("gemini-3.8-flash-low", "MODEL_PLACEHOLDER_M320"),
    ("gemini-3.8-flash-tiered", "MODEL_PLACEHOLDER_M322"),
    ("gemini-3.7-flash", "MODEL_PLACEHOLDER_M298"),
    ("gemini-3.7-flash-high", "MODEL_PLACEHOLDER_M298"),
    ("gemini-3.7-flash-medium", "MODEL_PLACEHOLDER_M299"),
    ("gemini-3.7-flash-low", "MODEL_PLACEHOLDER_M300"),
    ("gemini-3.7-flash-tiered", "MODEL_PLACEHOLDER_M301"),
    ("gemini-3.6-flash", "MODEL_PLACEHOLDER_M71"),
    ("gemini-3.6-flash-high", "MODEL_PLACEHOLDER_M71"),
    ("gemini-3.6-flash-medium", "MODEL_PLACEHOLDER_M72"),
    ("gemini-3.6-flash-low", "MODEL_PLACEHOLDER_M73"),
    ("gemini-3.6-flash-tiered", "MODEL_PLACEHOLDER_M196"),
    ("gemini-3.5-flash", "MODEL_PLACEHOLDER_M20"),
    ("gemini-3.5-flash-extra-low", "MODEL_PLACEHOLDER_M187"),
    ("gemini-3.5-flash-low", "MODEL_PLACEHOLDER_M20"),
    ("gemini-3-flash-agent", "MODEL_PLACEHOLDER_M84"),
    ("gemini-3.1-pro", "MODEL_PLACEHOLDER_M36"),
    ("gemini-3.1-pro-low", "MODEL_PLACEHOLDER_M36"),
    ("gemini-3.1-pro-high", "MODEL_PLACEHOLDER_M37"),
    ("gemini-pro-agent", "MODEL_PLACEHOLDER_M16"),
    ("claude-sonnet-4-6", "MODEL_PLACEHOLDER_M35"),
    ("claude-opus-4-6", "MODEL_PLACEHOLDER_M26"),
    ("claude-opus-4-6-thinking", "MODEL_PLACEHOLDER_M26"),
    ("gpt-oss-120b", "MODEL_OPENAI_GPT_OSS_120B_MEDIUM"),
    ("gpt-oss-120b-medium", "MODEL_OPENAI_GPT_OSS_120B_MEDIUM"),
];

fn static_model_enum(wire_model_id: &str) -> Option<&'static str> {
    MODEL_ENUMS
        .iter()
        .find(|(id, _)| *id == wire_model_id)
        .map(|(_, value)| *value)
}

fn enum_cache() -> &'static RwLock<HashMap<String, String>> {
    static CACHE: OnceLock<RwLock<HashMap<String, String>>> = OnceLock::new();
    CACHE.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Register one dynamically discovered enum (`registerModelEnum`).
pub fn register_model_enum(wire_model_id: &str, model_enum: &str) {
    if !wire_model_id.is_empty() && !model_enum.is_empty() {
        enum_cache()
            .write()
            .unwrap()
            .insert(wire_model_id.to_string(), model_enum.to_string());
    }
}

/// Register every enum in a `fetchAvailableModels` `models` dictionary
/// (`registerDiscoveredModelEnums`): the nested `model` field carries the enum.
pub fn register_discovered_model_enums(models: &serde_json::Map<String, Value>) {
    for (wire_id, info) in models {
        if let Some(model_enum) = info.get("model").and_then(Value::as_str) {
            register_model_enum(wire_id, model_enum);
        }
    }
}

/// `model_enum` request label for a runtime id, when known.
///
/// Dynamic cache first, then the static table; a public id falls back to the wire id it
/// routes to, so it resolves to the enum of the runtime variant discovery would have found.
#[must_use]
pub fn model_enum(wire_model_id: &str) -> Option<String> {
    if let Some(value) = dynamic_or_static_enum(wire_model_id) {
        return Some(value);
    }
    dynamic_or_static_enum(&runtime_model(wire_model_id))
}

fn dynamic_or_static_enum(wire_model_id: &str) -> Option<String> {
    if let Some(cached) = enum_cache().read().unwrap().get(wire_model_id) {
        return Some(cached.clone());
    }
    static_model_enum(wire_model_id).map(str::to_string)
}

/// Serializable snapshot of the dynamically learned enums (`snapshotDynamicModelEnums`).
#[must_use]
pub fn snapshot_dynamic_model_enums() -> HashMap<String, String> {
    enum_cache().read().unwrap().clone()
}

/// Replace the dynamically learned enums (`restoreDynamicModelEnums`).
pub fn restore_dynamic_model_enums(model_enums: &HashMap<String, String>) {
    let mut cache = enum_cache().write().unwrap();
    cache.clear();
    for (wire_model_id, model_enum) in model_enums {
        if !wire_model_id.is_empty() && !model_enum.is_empty() {
            cache.insert(wire_model_id.clone(), model_enum.clone());
        }
    }
}

/// Drop every dynamically learned enum (`clearModelEnumCache`).
pub fn clear_model_enum_cache() {
    enum_cache().write().unwrap().clear();
}

/// The model-facing extra tools an `ANTIGRAVITY_NO_*` toggle can disable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtraTool {
    Search,
    Image,
}

/// Whether a model-facing extra tool is registered (`isExtraToolEnabled`): the umbrella
/// `ANTIGRAVITY_NO_EXTRA_TOOLS` and the per-tool `ANTIGRAVITY_NO_{SEARCH,IMAGE}_TOOL` both
/// disable it; only the value `"1"` counts.
#[must_use]
pub fn extra_tool_enabled(tool: ExtraTool) -> bool {
    let name = match tool {
        ExtraTool::Search => "NO_SEARCH_TOOL",
        ExtraTool::Image => "NO_IMAGE_TOOL",
    };
    extra_tool_enabled_values(
        crate::providers::antigravity::antigravity_env("NO_EXTRA_TOOLS").as_deref(),
        crate::providers::antigravity::antigravity_env(name).as_deref(),
    )
}

/// The pure `isExtraToolEnabled` predicate over the two toggle values.
#[must_use]
pub fn extra_tool_enabled_values(no_extra_tools: Option<&str>, no_tool: Option<&str>) -> bool {
    no_extra_tools != Some("1") && no_tool != Some("1")
}

/// Fast grounding model used for interactive `google_search` calls.
pub const DEFAULT_SEARCH_MODEL: &str = "gemini-3.5-flash-lite";

/// Search model candidates, tried in order when the primary is missing or overloaded.
pub const SEARCH_MODEL_FALLBACKS: &[&str] = &[
    DEFAULT_SEARCH_MODEL,
    "gemini-3.1-flash-lite",
    "gemini-3-flash",
];

/// System instruction that shapes the compact search evidence brief.
pub const SEARCH_SYSTEM_INSTRUCTION: &str = "You are a careful research assistant using Google Search grounding.\nReturn a compact evidence brief with exactly these sections:\n## Findings\n## Gaps\n## Next Checks\n\nRules:\n- Verify each claim inside the specific item, function, or section it refers to. Do not copy an attribute from a neighboring entry on the same page.\n- Prefer primary documentation and exact names, versions, and constraints over marketing summaries.\n- If a fact is not supported by a retrieved source, put it under Gaps instead of stating it as a finding.\n- Keep the brief short enough for an interactive agent tool call.";

/// Thinking budget for a search request that opts into planning; `0` keeps lookups fast.
pub const SEARCH_THINKING_BUDGET: i64 = 4096;

/// The `googleSearch` tool declaration for a Gemini `tools` array.
///
/// Add it to the request body as `"tools": [google_search_tool()]`; this is what enables
/// Google Search grounding. Combine with [`url_context_tool`] to also fetch given URLs.
#[must_use]
pub fn google_search_tool() -> Value {
    json!({ "googleSearch": json!({}) })
}

/// The `urlContext` tool declaration for a Gemini `tools` array.
#[must_use]
pub fn url_context_tool() -> Value {
    json!({ "urlContext": json!({}) })
}

/// The zero-cost entry the extension uses for synthesized models without a family template.
pub(crate) fn zero_cost() -> ModelCost {
    ModelCost {
        input: JsNumber::from(0.0),
        output: JsNumber::from(0.0),
        cache_read: JsNumber::from(0.0),
        cache_write: JsNumber::from(0.0),
    }
}

/// Shared test serialization: `apply_catalog` / enum-cache state is process-global, so tests
/// that touch it must run one at a time.
#[cfg(test)]
pub(crate) fn global_state_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    //! Behavioural parity tests mirroring `scripts/test-model-routing.ts` (routing table,
    //! public model set, thinking levels, output caps, fallbacks, thinking budgets, enum
    //! labels), the extra-tool toggle table from `scripts/test-extra-tools-toggle.ts`, and
    //! the dynamic catalog/enum state from `scripts/test-model-discovery.ts`.

    use super::*;
    use ModelThinkingLevel::{High, Low, Medium, Minimal, Off, Xhigh};

    /// `(public id, effort, expected runtime id)` taken from the extension's route cases.
    const ROUTE_CASES: &[(&str, ModelThinkingLevel, &str)] = &[
        ("gemini-3.8-flash", Off, "gemini-3.8-flash-low"),
        ("gemini-3.8-flash", Minimal, "gemini-3.8-flash-low"),
        ("gemini-3.8-flash", Low, "gemini-3.8-flash-low"),
        ("gemini-3.8-flash", Medium, "gemini-3.8-flash-medium"),
        ("gemini-3.8-flash", High, "gemini-3.8-flash-high"),
        ("gemini-3.8-flash", Xhigh, "gemini-3.8-flash-high"),
        ("gemini-3.7-flash", Off, "gemini-3.7-flash-low"),
        ("gemini-3.7-flash", Minimal, "gemini-3.7-flash-low"),
        ("gemini-3.7-flash", Low, "gemini-3.7-flash-low"),
        ("gemini-3.7-flash", Medium, "gemini-3.7-flash-medium"),
        ("gemini-3.7-flash", High, "gemini-3.7-flash-high"),
        ("gemini-3.7-flash", Xhigh, "gemini-3.7-flash-high"),
        ("gemini-3.6-flash", Off, "gemini-3.6-flash-low"),
        ("gemini-3.6-flash", Minimal, "gemini-3.6-flash-low"),
        ("gemini-3.6-flash", Low, "gemini-3.6-flash-low"),
        ("gemini-3.6-flash", Medium, "gemini-3.6-flash-medium"),
        ("gemini-3.6-flash", High, "gemini-3.6-flash-high"),
        ("gemini-3.6-flash", Xhigh, "gemini-3.6-flash-high"),
        ("gemini-3.5-flash", Off, "gemini-3.5-flash-extra-low"),
        ("gemini-3.5-flash", Minimal, "gemini-3.5-flash-extra-low"),
        ("gemini-3.5-flash", Low, "gemini-3.5-flash-extra-low"),
        ("gemini-3.5-flash", Medium, "gemini-3.5-flash-low"),
        ("gemini-3.5-flash", High, "gemini-3-flash-agent"),
        ("gemini-3.5-flash", Xhigh, "gemini-3-flash-agent"),
        ("gemini-3.1-pro", Off, "gemini-3.1-pro-low"),
        ("gemini-3.1-pro", Minimal, "gemini-3.1-pro-low"),
        ("gemini-3.1-pro", Low, "gemini-3.1-pro-low"),
        ("gemini-3.1-pro", Medium, "gemini-3.1-pro-low"),
        ("gemini-3.1-pro", High, "gemini-pro-agent"),
        ("gemini-3.1-pro", Xhigh, "gemini-pro-agent"),
        ("claude-sonnet-4-6", Off, "claude-sonnet-4-6"),
        ("claude-sonnet-4-6", Xhigh, "claude-sonnet-4-6"),
        ("claude-opus-4-6", High, "claude-opus-4-6-thinking"),
        ("gpt-oss-120b", High, "gpt-oss-120b-medium"),
        ("unknown-model", High, "unknown-model"),
    ];

    #[test]
    fn routes_public_ids_to_runtime_ids() {
        let _guard = global_state_test_lock();
        for (model, effort, expected) in ROUTE_CASES.iter().copied() {
            assert_eq!(request_model_id(model, effort), expected, "{model} ({effort:?})");
        }
    }

    #[test]
    fn runtime_model_is_the_no_effort_default() {
        let _guard = global_state_test_lock();
        assert_eq!(runtime_model("gemini-3.8-flash"), "gemini-3.8-flash-low");
        assert_eq!(runtime_model("claude-opus-4-6"), "claude-opus-4-6-thinking");
        assert_eq!(runtime_model("claude-sonnet-4-6"), "claude-sonnet-4-6");
        assert_eq!(runtime_model("gemini-custom-9"), "gemini-custom-9");
    }

    #[test]
    fn catalog_exposes_expected_models_and_thinking_levels() {
        let _guard = global_state_test_lock();
        let models = crate::models_generated::get_models(API_ANTIGRAVITY);
        let mut ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(
            ids,
            vec![
                "claude-opus-4-6",
                "claude-sonnet-4-6",
                "gemini-3.1-pro",
                "gemini-3.5-flash",
                "gemini-3.6-flash",
                "gemini-3.7-flash",
                "gemini-3.8-flash",
                "gpt-oss-120b",
            ]
        );

        let expected: &[(&str, &[ModelThinkingLevel])] = &[
            ("gemini-3.8-flash", &[Low, Medium, High]),
            ("gemini-3.7-flash", &[Low, Medium, High]),
            ("gemini-3.6-flash", &[Low, Medium, High]),
            ("gemini-3.5-flash", &[Low, Medium, High]),
            ("gemini-3.1-pro", &[Low, High]),
            ("claude-opus-4-6", &[High]),
            ("claude-sonnet-4-6", &[High]),
            ("gpt-oss-120b", &[Medium]),
        ];
        for (id, levels) in expected.iter().copied() {
            let model = models
                .iter()
                .find(|model| model.id == id)
                .unwrap_or_else(|| panic!("missing model {id}"));
            let map = model
                .thinking_level_map
                .as_ref()
                .unwrap_or_else(|| panic!("{id} has no thinking map"));
            let supported: Vec<ModelThinkingLevel> = map
                .iter()
                .filter(|(_, value)| value.is_some())
                .map(|(level, _)| *level)
                .collect();
            assert_eq!(supported.as_slice(), levels, "{id} thinking levels");
            assert_eq!(model.api, API_ANTIGRAVITY);
            assert_eq!(model.provider, API_ANTIGRAVITY);
            assert_eq!(model.base_url, DEFAULT_ANTIGRAVITY_BASE_URL);
            assert_eq!(model.max_tokens, max_output_tokens(id, None), "{id} max tokens");
            assert!(model.reasoning, "{id} must be a reasoning model");
            assert!(routing(id).is_some(), "{id} must have routing");
        }
    }

    #[test]
    fn caps_output_tokens_per_family() {
        let _guard = global_state_test_lock();
        assert_eq!(max_output_tokens("gemini-3.8-flash", Some("gemini-3.8-flash-low")), 65_536);
        assert_eq!(max_output_tokens("gemini-3.7-flash", Some("gemini-3.7-flash-tiered")), 65_536);
        assert_eq!(max_output_tokens("gemini-3.6-flash", Some("gemini-3.6-flash-low")), 65_536);
        assert_eq!(max_output_tokens("gemini-3.1-pro", Some("gemini-3.1-pro-low")), 65_535);
        assert_eq!(max_output_tokens("gemini-3.1-pro", Some("gemini-pro-agent")), 65_535);
        assert_eq!(max_output_tokens("claude-sonnet-4-6", Some("claude-sonnet-4-6")), 64_000);
        assert_eq!(max_output_tokens("gpt-oss-120b", Some("gpt-oss-120b-medium")), 32_768);
        assert_eq!(max_output_tokens("unknown", Some("gemini-9-flash")), 65_536);
        assert_eq!(max_output_tokens("unknown", None), 8192);
    }

    #[test]
    fn falls_back_across_generations() {
        let _guard = global_state_test_lock();
        assert_eq!(
            fallback_runtime_model("gemini-3.8-flash-low", Off).as_deref(),
            Some("gemini-3.7-flash-low")
        );
        assert_eq!(
            fallback_runtime_model("gemini-3.8-flash-medium", Medium).as_deref(),
            Some("gemini-3.7-flash-medium")
        );
        assert_eq!(
            fallback_runtime_model("gemini-3.8-flash-high", High).as_deref(),
            Some("gemini-3.7-flash-high")
        );
        assert_eq!(
            fallback_runtime_model("gemini-3.8-flash", Off).as_deref(),
            Some("gemini-3.7-flash-low")
        );
        assert_eq!(
            fallback_runtime_model("gemini-3.7-flash-low", Off).as_deref(),
            Some("gemini-3.6-flash-low")
        );
        assert_eq!(
            fallback_runtime_model("gemini-3.7-flash-medium", Medium).as_deref(),
            Some("gemini-3.6-flash-medium")
        );
        assert_eq!(
            fallback_runtime_model("gemini-3.7-flash-high", High).as_deref(),
            Some("gemini-3.6-flash-high")
        );
        assert_eq!(
            fallback_runtime_model("gemini-3.7-flash-tiered", Medium).as_deref(),
            Some("gemini-3.6-flash-medium")
        );
        assert_eq!(
            fallback_runtime_model("gemini-3.7-flash", Off).as_deref(),
            Some("gemini-3.6-flash-low")
        );
        assert_eq!(fallback_runtime_model("gemini-3.6-flash-low", Off), None);
        assert_eq!(fallback_runtime_model("claude-sonnet-4-6", Off), None);
    }

    #[test]
    fn thinking_config_per_family() {
        let _guard = global_state_test_lock();
        let on = |budget| {
            Some(ThinkingWire {
                include_thoughts: true,
                thinking_budget: budget,
            })
        };
        assert_eq!(thinking_config("gemini-3.8-flash-low", Low), on(1000));
        assert_eq!(thinking_config("gemini-3.8-flash-medium", Medium), on(4000));
        assert_eq!(thinking_config("gemini-3.8-flash-high", High), on(-1));
        assert_eq!(thinking_config("gemini-3.7-flash-high", High), on(-1));
        assert_eq!(thinking_config("gemini-3.6-flash-medium", Medium), on(4000));
        assert_eq!(thinking_config("gemini-3.7-flash-low", Off), Some(ThinkingWire::DISABLED));
        assert_eq!(thinking_config("gemini-3.5-flash-low", Medium), on(4000));
        assert_eq!(thinking_config("gemini-3.5-flash-extra-low", Low), on(1000));
        assert_eq!(thinking_config("gemini-3-flash-agent", High), on(10_000));
        assert_eq!(thinking_config("gemini-3.1-pro-low", High), on(10_001));
        assert_eq!(thinking_config("gemini-3.1-pro-low", Low), on(1_001));
        assert_eq!(thinking_config("gemini-pro-agent", Xhigh), on(10_001));
        assert_eq!(thinking_config("claude-sonnet-4-6", High), on(1024));
        assert_eq!(thinking_config("gpt-oss-120b-medium", Medium), on(8192));
        assert_eq!(thinking_config("gpt-oss-120b-medium", Off), Some(ThinkingWire::DISABLED));
        assert_eq!(thinking_config("unknown-model", High), None);
    }

    #[test]
    fn model_enum_labels_and_routed_fallback() {
        let _guard = global_state_test_lock();
        assert_eq!(
            model_enum("gemini-3.8-flash-high").as_deref(),
            Some("MODEL_PLACEHOLDER_M318")
        );
        assert_eq!(
            model_enum("gemini-3.8-flash-low").as_deref(),
            Some("MODEL_PLACEHOLDER_M320")
        );
        assert_eq!(
            model_enum("gemini-3.6-flash-high").as_deref(),
            Some("MODEL_PLACEHOLDER_M71")
        );
        assert_eq!(model_enum("gemini-pro-agent").as_deref(), Some("MODEL_PLACEHOLDER_M16"));
        assert_eq!(model_enum("claude-sonnet-4-6").as_deref(), Some("MODEL_PLACEHOLDER_M35"));
        assert_eq!(
            model_enum("gpt-oss-120b-medium").as_deref(),
            Some("MODEL_OPENAI_GPT_OSS_120B_MEDIUM")
        );
        // A direct static entry wins over the routed fallback (models.ts:383-387:
        // `gemini-3.8-flash` -> M318). The routed path only fires when the wire id has no
        // direct entry (covered by the dynamic-cache test: `gemini-9.9-flash` -> M999).
        assert_eq!(
            model_enum("gemini-3.8-flash").as_deref(),
            Some("MODEL_PLACEHOLDER_M318")
        );
        assert_eq!(model_enum("unknown-model"), None);
    }

    #[test]
    fn dynamic_catalog_and_enum_cache_are_applied_and_reset() {
        let _guard = global_state_test_lock();
        reset_catalog();
        clear_model_enum_cache();

        // A discovered catalog replaces the current one and reroutes its public id.
        let mut routing = HashMap::new();
        routing.insert(
            "gemini-9.9-flash".to_string(),
            AntigravityRouting {
                off: Some("gemini-9.9-flash-low".to_string()),
                low: Some("gemini-9.9-flash-low".to_string()),
                default_request_id: Some("gemini-9.9-flash-low".to_string()),
                ..AntigravityRouting::default()
            },
        );
        apply_catalog(AntigravityCatalog {
            models: Vec::new(),
            routing,
        });
        assert_eq!(runtime_model("gemini-9.9-flash"), "gemini-9.9-flash-low");
        assert!(current_models().is_empty());
        assert_eq!(current_routing().len(), 1);

        register_model_enum("gemini-9.9-flash-low", "MODEL_DYNAMIC_999");
        assert_eq!(
            model_enum("gemini-9.9-flash-low").as_deref(),
            Some("MODEL_DYNAMIC_999")
        );
        assert_eq!(model_enum("gemini-9.9-flash").as_deref(), Some("MODEL_DYNAMIC_999"));
        let snapshot = snapshot_dynamic_model_enums();
        assert_eq!(snapshot.get("gemini-9.9-flash-low").map(String::as_str), Some("MODEL_DYNAMIC_999"));

        clear_model_enum_cache();
        assert_eq!(model_enum("gemini-9.9-flash-low"), None);
        restore_dynamic_model_enums(&snapshot);
        assert_eq!(
            model_enum("gemini-9.9-flash-low").as_deref(),
            Some("MODEL_DYNAMIC_999")
        );

        reset_catalog();
        clear_model_enum_cache();
        assert_eq!(runtime_model("gemini-9.9-flash"), "gemini-9.9-flash");
        assert_eq!(current_models().len(), 8);
        assert_eq!(model_enum("gemini-3.8-flash-low").as_deref(), Some("MODEL_PLACEHOLDER_M320"));
    }

    #[test]
    fn extra_tool_toggle_table() {
        // The umbrella and per-tool toggles; only the value "1" disables.
        assert!(extra_tool_enabled_values(None, None));
        assert!(!extra_tool_enabled_values(Some("1"), None));
        assert!(!extra_tool_enabled_values(None, Some("1")));
        assert!(extra_tool_enabled_values(Some("0"), Some("0")));
        assert!(extra_tool_enabled_values(Some("true"), Some("yes")));
    }

    #[test]
    fn search_tool_declarations_and_models() {
        assert_eq!(DEFAULT_SEARCH_MODEL, "gemini-3.5-flash-lite");
        let fallbacks: Vec<&str> = SEARCH_MODEL_FALLBACKS.to_vec();
        assert_eq!(
            fallbacks,
            vec!["gemini-3.5-flash-lite", "gemini-3.1-flash-lite", "gemini-3-flash"]
        );
        assert_eq!(
            google_search_tool(),
            serde_json::json!({ "googleSearch": serde_json::json!({}) })
        );
        assert_eq!(
            url_context_tool(),
            serde_json::json!({ "urlContext": serde_json::json!({}) })
        );
        assert_eq!(SEARCH_THINKING_BUDGET, 4096);
    }

    #[test]
    fn endpoint_fallbacks_are_ordered_and_contain_the_default() {
        assert_eq!(ANTIGRAVITY_ENDPOINT_FALLBACKS[0], DEFAULT_ANTIGRAVITY_BASE_URL);
        assert_eq!(ANTIGRAVITY_ENDPOINT_FALLBACKS.len(), 3);
    }
}
