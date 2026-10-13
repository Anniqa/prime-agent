//! Port of `pi-antigravity/src/models/grouping.ts`: turn a `fetchAvailableModels` payload into
//! the public model catalog plus per-effort routing, and the surrounding pure helpers.
//!
//! This module is pure (no I/O, no global state): `build_antigravity_catalog` is given the raw
//! `models` dictionary and a fallback catalog, exactly like the extension, and returns a fresh
//! [`AntigravityCatalog`].

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value};

use crate::providers::antigravity_models::{
    zero_cost, AntigravityRouting, API_ANTIGRAVITY, DEFAULT_ANTIGRAVITY_BASE_URL,
};
use crate::types::{Model, ModelInput, ModelThinkingLevel, ThinkingLevelMap};
pub use crate::providers::antigravity_models::AntigravityCatalog;

/// Thinking-suffix table, in the extension's order (longest/specific first).
const THINKING_SUFFIXES: &[(&str, ModelThinkingLevel)] = &[
    ("extra-low", ModelThinkingLevel::Low),
    ("extra-high", ModelThinkingLevel::Xhigh),
    ("thinking", ModelThinkingLevel::High),
    ("minimal", ModelThinkingLevel::Minimal),
    ("medium", ModelThinkingLevel::Medium),
    ("high", ModelThinkingLevel::High),
    ("low", ModelThinkingLevel::Low),
];

/// The Prime Agent thinking levels, in the extension's `PI_LEVELS` order.
const PI_LEVELS: [ModelThinkingLevel; 7] = [
    ModelThinkingLevel::Off,
    ModelThinkingLevel::Minimal,
    ModelThinkingLevel::Low,
    ModelThinkingLevel::Medium,
    ModelThinkingLevel::High,
    ModelThinkingLevel::Xhigh,
    ModelThinkingLevel::Max,
];

/// `(display name)` thinking-level patterns, in the extension's order.
static DISPLAY_LEVELS: LazyLock<Vec<(Regex, ModelThinkingLevel)>> = LazyLock::new(|| {
    vec![
        (
            Regex::new(r"(?i)\(\s*extra\s*low\s*\)").unwrap(),
            ModelThinkingLevel::Low,
        ),
        (
            Regex::new(r"(?i)\(\s*extra\s*high\s*\)").unwrap(),
            ModelThinkingLevel::Xhigh,
        ),
        (
            Regex::new(r"(?i)\(\s*thinking\s*\)").unwrap(),
            ModelThinkingLevel::High,
        ),
        (
            Regex::new(r"(?i)\(\s*minimal\s*\)").unwrap(),
            ModelThinkingLevel::Minimal,
        ),
        (
            Regex::new(r"(?i)\(\s*medium\s*\)").unwrap(),
            ModelThinkingLevel::Medium,
        ),
        (
            Regex::new(r"(?i)\(\s*high\s*\)").unwrap(),
            ModelThinkingLevel::High,
        ),
        (
            Regex::new(r"(?i)\(\s*low\s*\)").unwrap(),
            ModelThinkingLevel::Low,
        ),
    ]
});

/// Trailing `(level)` display suffix removed when deriving a family name.
static DISPLAY_FAMILY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\s*\((?:extra\s*low|extra\s*high|low|medium|high|minimal|thinking)\)\s*$")
        .unwrap()
});

/// `\b([a-z])` word-start uppercasing for display names.
static TITLE_CASE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b([a-z])").unwrap());

/// `^gemini-(\d+)(\.(\d+))?` version prefix.
static GEMINI_VERSION_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^gemini-(\d+)(?:\.(\d+))?").unwrap());

/// `isSelectableRuntimeModelId`: keep `gemini-*`/`claude-*`/`gpt-oss-*` runtime ids, drop
/// whitespace-bearing ids, `MODEL_*` enums, `chat_*`/`tab_*` internals, and image models.
#[must_use]
pub fn is_selectable_runtime_model_id(id: &str) -> bool {
    let lower = id.to_lowercase();
    if !(lower.starts_with("gemini-") || lower.starts_with("claude-") || lower.starts_with("gpt-oss-"))
    {
        return false;
    }
    if id.chars().any(char::is_whitespace) {
        return false;
    }
    if lower.starts_with("model_") {
        return false;
    }
    if lower.starts_with("chat_") || lower.starts_with("tab_") {
        return false;
    }
    if lower.contains("image") {
        return false;
    }
    true
}

/// `resolvedCatalog`: prefer the discovered catalog when it has models, else the current one.
#[must_use]
pub fn resolved_catalog(
    discovered: Option<&AntigravityCatalog>,
    current: &AntigravityCatalog,
) -> AntigravityCatalog {
    match discovered {
        Some(discovered) if !discovered.models.is_empty() => discovered.clone(),
        _ => current.clone(),
    }
}

/// One runtime group while building: the public id, its per-effort variants, and the
/// capability metadata the payload advertises.
struct RuntimeGroup {
    public_id: String,
    variants: BTreeMap<ModelThinkingLevel, String>,
    unsuffixed: Option<String>,
    display_names: Vec<String>,
    supports_thinking: Option<bool>,
    supports_images: Option<bool>,
}

/// The subset of a raw `ModelInfoRaw` entry the grouping logic reads.
struct ModelInfo {
    display_name: Option<String>,
    is_internal: bool,
    supports_thinking: Option<bool>,
    supports_images: Option<bool>,
}

fn parse_model_info(value: &Value) -> ModelInfo {
    let text = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    };
    ModelInfo {
        display_name: text("displayName")
            .or_else(|| text("label"))
            .or_else(|| text("modelName")),
        is_internal: value.get("isInternal").and_then(Value::as_bool).unwrap_or(false),
        supports_thinking: value.get("supportsThinking").and_then(Value::as_bool),
        supports_images: value.get("supportsImages").and_then(Value::as_bool),
    }
}

/// Known backend aliases whose runtime ids do not share a thinking suffix with the public family.
fn runtime_alias(runtime_id: &str) -> Option<(&'static str, ModelThinkingLevel)> {
    match runtime_id {
        "gemini-3-flash-agent" => Some(("gemini-3.5-flash", ModelThinkingLevel::High)),
        "gemini-pro-agent" => Some(("gemini-3.1-pro", ModelThinkingLevel::High)),
        _ => None,
    }
}

/// `buildAntigravityCatalog`: group the raw runtime ids into public models and routing.
#[must_use]
pub fn build_antigravity_catalog(
    raw_models: &Map<String, Value>,
    fallback: &AntigravityCatalog,
) -> AntigravityCatalog {
    let mut groups: BTreeMap<String, RuntimeGroup> = BTreeMap::new();

    for (runtime_id, value) in raw_models {
        if !is_selectable_runtime_model_id(runtime_id) {
            continue;
        }
        let info = parse_model_info(value);
        if info.is_internal {
            continue;
        }
        let display_name = info.display_name.clone();

        if let Some(base) = runtime_id.strip_suffix("-tiered") {
            let group = ensure_group(&mut groups, base);
            absorb_metadata(group, &info, display_name);
            if group.unsuffixed.is_none() {
                group.unsuffixed = Some(runtime_id.clone());
            }
            continue;
        }

        let alias = runtime_alias(runtime_id);
        let suffix = if alias.is_some() {
            None
        } else {
            parse_thinking_suffix(runtime_id)
        };
        let public_id = alias
            .map(|(public_id, _)| public_id.to_string())
            .or_else(|| suffix.as_ref().map(|(base, _)| base.clone()))
            .unwrap_or_else(|| runtime_id.clone());
        let level = alias
            .map(|(_, level)| level)
            .or_else(|| level_from_display_name(display_name.as_deref()))
            .or_else(|| suffix.as_ref().map(|(_, level)| *level));

        let group = ensure_group(&mut groups, &public_id);
        absorb_metadata(group, &info, display_name);
        match level {
            Some(level) => {
                group.variants.insert(level, runtime_id.clone());
            }
            None => group.unsuffixed = Some(runtime_id.clone()),
        }
    }

    merge_agent_singletons(&mut groups);

    if groups.is_empty() {
        return fallback.clone();
    }

    let mut models: Vec<Model> = Vec::new();
    let mut routing: std::collections::HashMap<String, AntigravityRouting> =
        std::collections::HashMap::new();

    for group in groups.values() {
        let fallback_model = fallback.models.iter().find(|model| model.id == group.public_id);
        let fallback_routing = fallback.routing.get(&group.public_id);
        if let (Some(model), Some(entry)) = (fallback_model, fallback_routing) {
            models.push(model.clone());
            routing.insert(group.public_id.clone(), entry.clone());
            continue;
        }
        let (model, entry) = synthesize_model(group, &fallback.models);
        models.push(model);
        routing.insert(group.public_id.clone(), entry);
    }

    for fallback_model in &fallback.models {
        if routing.contains_key(&fallback_model.id) {
            continue;
        }
        let Some(entry) = fallback.routing.get(&fallback_model.id) else {
            continue;
        };
        models.push(fallback_model.clone());
        routing.insert(fallback_model.id.clone(), entry.clone());
    }

    models.sort_by(compare_public_models);
    AntigravityCatalog { models, routing }
}

fn ensure_group<'a>(
    groups: &'a mut BTreeMap<String, RuntimeGroup>,
    public_id: &str,
) -> &'a mut RuntimeGroup {
    groups.entry(public_id.to_string()).or_insert_with(|| RuntimeGroup {
        public_id: public_id.to_string(),
        variants: BTreeMap::new(),
        unsuffixed: None,
        display_names: Vec::new(),
        supports_thinking: None,
        supports_images: None,
    })
}

fn absorb_metadata(group: &mut RuntimeGroup, info: &ModelInfo, display_name: Option<String>) {
    if let Some(name) = display_name {
        group.display_names.push(name);
    }
    if info.supports_thinking == Some(true) {
        group.supports_thinking = Some(true);
    } else if info.supports_thinking == Some(false) && group.supports_thinking != Some(true) {
        group.supports_thinking = Some(false);
    }
    if info.supports_images == Some(true) {
        group.supports_images = Some(true);
    } else if info.supports_images == Some(false) && group.supports_images.is_none() {
        group.supports_images = Some(false);
    }
}

/// Fold `*-agent` singleton groups into their public family (`mergeAgentSingletons`).
fn merge_agent_singletons(groups: &mut BTreeMap<String, RuntimeGroup>) {
    let agent_ids: Vec<String> = groups
        .keys()
        .filter(|public_id| public_id.ends_with("-agent"))
        .cloned()
        .collect();

    for agent_id in agent_ids {
        let Some(group) = groups.get(&agent_id) else {
            continue;
        };
        if !group.variants.is_empty() {
            continue;
        }
        let first_display = group.display_names.first().cloned();
        let Some(family) = first_display.as_deref().and_then(display_family) else {
            continue;
        };
        let Some(unsuffixed) = group.unsuffixed.clone() else {
            continue;
        };
        let supports_thinking = group.supports_thinking;

        let target_id = groups
            .iter()
            .find(|(public_id, candidate)| {
                public_id.as_str() != agent_id.as_str()
                    && candidate
                        .display_names
                        .iter()
                        .any(|name| display_family(name).as_deref() == Some(family.as_str()))
            })
            .map(|(public_id, _)| public_id.clone());
        let Some(target_id) = target_id else {
            continue;
        };

        let level = first_display
            .as_deref()
            .and_then(|name| level_from_display_name(Some(name)))
            .unwrap_or(ModelThinkingLevel::High);
        if let Some(target) = groups.get_mut(&target_id) {
            target.variants.insert(level, unsuffixed);
            if supports_thinking == Some(true) {
                target.supports_thinking = Some(true);
            } else if supports_thinking == Some(false) && target.supports_thinking != Some(true) {
                target.supports_thinking = Some(false);
            }
        }
        groups.remove(&agent_id);
    }
}

/// `parseThinkingSuffix`: split a `-<level>` suffix off a runtime id.
fn parse_thinking_suffix(runtime_id: &str) -> Option<(String, ModelThinkingLevel)> {
    let lower = runtime_id.to_lowercase();
    for (suffix, level) in THINKING_SUFFIXES {
        let needle = format!("-{suffix}");
        if lower.ends_with(&needle) {
            let base_len = runtime_id.len() - needle.len();
            return Some((runtime_id[..base_len].to_string(), *level));
        }
    }
    None
}

fn level_from_display_name(display_name: Option<&str>) -> Option<ModelThinkingLevel> {
    let display_name = display_name?;
    for (pattern, level) in DISPLAY_LEVELS.iter() {
        if pattern.is_match(display_name) {
            return Some(*level);
        }
    }
    None
}

/// `displayFamily`: drop a trailing `(level)` marker and normalize case.
fn display_family(display_name: &str) -> Option<String> {
    let family = DISPLAY_FAMILY_RE.replace(display_name, "").trim().to_lowercase();
    if family.is_empty() {
        None
    } else {
        Some(family)
    }
}

/// `humanizePublicId`: turn a runtime-ish id into a readable model name.
#[must_use]
pub fn humanize_public_id(id: &str) -> String {
    let tokens: Vec<&str> = id.split('-').collect();
    let mut words: Vec<String> = Vec::new();
    let mut index = 0;
    while index < tokens.len() {
        let token = tokens[index];
        if token.is_empty() {
            index += 1;
            continue;
        }
        let next = tokens.get(index + 1).copied();
        if token == "gpt" && next == Some("oss") {
            words.push("GPT-OSS".to_string());
            index += 2;
            continue;
        }
        if is_all_digits(token) && next.is_some_and(is_all_digits) {
            words.push(format!("{token}.{}", next.unwrap_or_default()));
            index += 2;
            continue;
        }
        if token.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            words.push(token.to_uppercase());
            index += 1;
            continue;
        }
        let mut chars = token.chars();
        if let Some(first) = chars.next() {
            words.push(format!("{}{}", first.to_uppercase(), chars.as_str()));
        }
        index += 1;
    }
    words.join(" ")
}

fn is_all_digits(token: &str) -> bool {
    !token.is_empty() && token.bytes().all(|byte| byte.is_ascii_digit())
}

fn synthesize_model(
    group: &RuntimeGroup,
    fallback_models: &[Model],
) -> (Model, AntigravityRouting) {
    let template = family_template(&group.public_id, fallback_models);
    let advertised = advertised_thinking_levels(group);
    let routing =
        routing_from_variants(&group.public_id, &group.variants, group.unsuffixed.as_deref());
    let supports_images = group
        .supports_images
        .unwrap_or_else(|| template.map_or(true, |model| model.input.contains(&ModelInput::Image)));
    let reasoning = !advertised.is_empty()
        || group.supports_thinking == Some(true)
        || (group.supports_thinking.is_none() && template.is_some_and(|model| model.reasoning));

    let model = Model {
        id: group.public_id.clone(),
        name: public_model_name(group),
        api: API_ANTIGRAVITY.to_string(),
        provider: API_ANTIGRAVITY.to_string(),
        base_url: DEFAULT_ANTIGRAVITY_BASE_URL.to_string(),
        reasoning,
        thinking_level_map: reasoning.then(|| thinking_level_map_from_levels(&advertised)),
        input: if supports_images {
            vec![ModelInput::Text, ModelInput::Image]
        } else {
            vec![ModelInput::Text]
        },
        cost: template.map_or_else(zero_cost, |model| model.cost.clone()),
        context_window: template.map_or(128_000, |model| model.context_window),
        max_tokens: template.map_or(8192, |model| model.max_tokens),
        max_tokens_explicit: false,
        featured: None,
        headers: None,
        compat: None,
    };
    (model, routing)
}

/// `advertisedThinkingLevels`: the backend-advertised levels, or `High` for an unsuffixed
/// thinking-capable model.
fn advertised_thinking_levels(group: &RuntimeGroup) -> BTreeSet<ModelThinkingLevel> {
    let mut levels: BTreeSet<ModelThinkingLevel> = group.variants.keys().copied().collect();
    if !levels.is_empty() {
        return levels;
    }
    if group.supports_thinking == Some(false) {
        return levels;
    }
    if group.supports_thinking == Some(true) || group.unsuffixed.is_some() {
        levels.insert(ModelThinkingLevel::High);
    }
    levels
}

/// `routingFromVariants`: pick the best runtime id per advertised level.
fn routing_from_variants(
    public_id: &str,
    variants: &BTreeMap<ModelThinkingLevel, String>,
    unsuffixed: Option<&str>,
) -> AntigravityRouting {
    let default_request_id = variants
        .get(&ModelThinkingLevel::Low)
        .or_else(|| variants.get(&ModelThinkingLevel::Minimal))
        .or_else(|| variants.get(&ModelThinkingLevel::Medium))
        .or_else(|| variants.get(&ModelThinkingLevel::High))
        .cloned()
        .or_else(|| unsuffixed.map(str::to_string))
        .unwrap_or_else(|| public_id.to_string());

    let pick = |keys: &[ModelThinkingLevel]| -> String {
        for key in keys {
            if let Some(id) = variants.get(key) {
                return id.clone();
            }
        }
        unsuffixed
            .map(str::to_string)
            .unwrap_or_else(|| default_request_id.clone())
    };

    AntigravityRouting {
        off: Some(pick(&[
            ModelThinkingLevel::Low,
            ModelThinkingLevel::Minimal,
            ModelThinkingLevel::Medium,
            ModelThinkingLevel::High,
        ])),
        minimal: Some(pick(&[
            ModelThinkingLevel::Minimal,
            ModelThinkingLevel::Low,
            ModelThinkingLevel::Medium,
            ModelThinkingLevel::High,
        ])),
        low: Some(pick(&[
            ModelThinkingLevel::Low,
            ModelThinkingLevel::Minimal,
            ModelThinkingLevel::Medium,
            ModelThinkingLevel::High,
        ])),
        medium: Some(pick(&[
            ModelThinkingLevel::Medium,
            ModelThinkingLevel::Low,
            ModelThinkingLevel::High,
            ModelThinkingLevel::Minimal,
        ])),
        high: Some(pick(&[
            ModelThinkingLevel::High,
            ModelThinkingLevel::Medium,
            ModelThinkingLevel::Low,
            ModelThinkingLevel::Minimal,
        ])),
        xhigh: Some(pick(&[
            ModelThinkingLevel::Xhigh,
            ModelThinkingLevel::High,
            ModelThinkingLevel::Medium,
            ModelThinkingLevel::Low,
        ])),
        default_request_id: Some(default_request_id),
    }
}

/// `thinkingLevelMapFromLevels`: one map entry per Prime Agent level.
fn thinking_level_map_from_levels(levels: &BTreeSet<ModelThinkingLevel>) -> ThinkingLevelMap {
    let mut map: ThinkingLevelMap = BTreeMap::new();
    for level in PI_LEVELS {
        map.insert(
            level,
            levels.contains(&level).then(|| level.wire_name().to_string()),
        );
    }
    if levels.is_empty() {
        map.insert(ModelThinkingLevel::High, Some("high".to_string()));
    }
    map
}

/// First fallback model matching `predicate`.
fn find_model(fallback_models: &[Model], predicate: impl Fn(&Model) -> bool) -> Option<&Model> {
    fallback_models.iter().find(|model| predicate(model))
}

/// `familyTemplate`: the compiled fallback model whose family matches the public id.
fn family_template<'a>(public_id: &str, fallback_models: &'a [Model]) -> Option<&'a Model> {
    let lower = public_id.to_lowercase();
    if lower.starts_with("gemini-") && lower.contains("-flash") {
        return find_model(fallback_models, |model| {
            let id = model.id.to_lowercase();
            id.starts_with("gemini-") && id.contains("-flash")
        });
    }
    if lower.starts_with("gemini-") && lower.contains("-pro") {
        return find_model(fallback_models, |model| {
            let id = model.id.to_lowercase();
            id.starts_with("gemini-") && id.contains("-pro")
        });
    }
    if lower.starts_with("claude-opus") {
        return find_model(fallback_models, |model| model.id.starts_with("claude-opus"));
    }
    if lower.starts_with("claude-") {
        return find_model(fallback_models, |model| model.id.starts_with("claude-sonnet"))
            .or_else(|| find_model(fallback_models, |model| model.id.starts_with("claude-")));
    }
    if lower.starts_with("gpt-oss") {
        return find_model(fallback_models, |model| model.id.starts_with("gpt-oss"));
    }
    if lower.starts_with("gemini-") {
        return find_model(fallback_models, |model| model.id.starts_with("gemini-"));
    }
    None
}

/// `publicModelName`: `<Family> (Antigravity)`.
fn public_model_name(group: &RuntimeGroup) -> String {
    let family = group
        .display_names
        .iter()
        .find_map(|name| display_family(name));
    match family {
        Some(family) => format!("{} (Antigravity)", title_case(&family)),
        None => format!("{} (Antigravity)", humanize_public_id(&group.public_id)),
    }
}

fn title_case(value: &str) -> String {
    TITLE_CASE_RE
        .replace_all(value, |caps: &regex::Captures| caps[1].to_uppercase())
        .into_owned()
}

fn parse_gemini_version(id: &str) -> i32 {
    let Some(captures) = GEMINI_VERSION_RE.captures(id) else {
        return 0;
    };
    let major: i32 = captures[1].parse().unwrap_or(0);
    let minor: i32 = captures
        .get(2)
        .and_then(|capture| capture.as_str().parse().ok())
        .unwrap_or(0);
    major * 1000 + minor
}

/// `modelRank`: the display ordering bucket for a public id.
fn model_rank(id: &str) -> (i32, i32) {
    let lower = id.to_lowercase();
    let version = parse_gemini_version(id);
    if lower.starts_with("gemini-") && lower.contains("-flash") && !lower.contains("pro") {
        return (0, -version);
    }
    if lower.starts_with("claude-opus") {
        return (1, 0);
    }
    if lower.starts_with("claude-sonnet") {
        return (2, 0);
    }
    if lower.starts_with("claude-") {
        return (3, 0);
    }
    if lower.starts_with("gemini-") && lower.contains("-pro") {
        return (4, -version);
    }
    if lower.starts_with("gemini-") {
        return (5, -version);
    }
    if lower.starts_with("gpt-oss") {
        return (6, 0);
    }
    (7, 0)
}

fn compare_public_models(left: &Model, right: &Model) -> std::cmp::Ordering {
    model_rank(&left.id)
        .cmp(&model_rank(&right.id))
        .then_with(|| left.id.cmp(&right.id))
}

#[cfg(test)]
mod tests {
    //! Mirrors the `buildAntigravityCatalog` / `humanizePublicId` / filtering assertions from
    //! `scripts/test-model-discovery.ts`.

    use super::*;
    use serde_json::json;

    fn info(display_name: &str) -> Value {
        json!({ "displayName": display_name, "supportsThinking": true, "supportsImages": true })
    }

    fn raw(entries: &[(&str, Value)]) -> Map<String, Value> {
        entries
            .iter()
            .map(|(id, value)| ((*id).to_string(), value.clone()))
            .collect()
    }

    #[test]
    fn discovers_new_families_and_filters_hidden_and_image_models() {
        let fallback = crate::providers::antigravity_models::compiled_catalog();
        let catalog = build_antigravity_catalog(
            &raw(&[
                ("gemini-3.9-flash-low", info("Gemini 3.9 Flash (Low)")),
                ("gemini-3.9-flash-medium", info("Gemini 3.9 Flash (Medium)")),
                ("gemini-3.9-flash-high", info("Gemini 3.9 Flash (High)")),
                ("chat_hidden", info("Hidden chat")),
                ("gemini-3-pro-image", info("Gemini 3 Pro Image")),
            ]),
            &fallback,
        );

        let model = catalog
            .models
            .iter()
            .find(|model| model.id == "gemini-3.9-flash")
            .expect("discovers new model families");
        assert!(!catalog.models.iter().any(|model| model.id == "chat_hidden"));
        assert!(!catalog.models.iter().any(|model| model.id == "gemini-3-pro-image"));
        assert_eq!(
            catalog.routing["gemini-3.9-flash"].medium.as_deref(),
            Some("gemini-3.9-flash-medium")
        );
        // Synthesized from the flash family template.
        assert_eq!(model.name, "Gemini 3.9 Flash (Antigravity)");
        assert_eq!(model.context_window, 1_048_576);
        assert_eq!(model.max_tokens, 65_536);
        assert!(model.reasoning);
        assert_eq!(model.api, API_ANTIGRAVITY);
        assert_eq!(model.base_url, DEFAULT_ANTIGRAVITY_BASE_URL);
        let levels: Vec<ModelThinkingLevel> = model
            .thinking_level_map
            .as_ref()
            .unwrap()
            .iter()
            .filter(|(_, value)| value.is_some())
            .map(|(level, _)| *level)
            .collect();
        assert_eq!(
            levels,
            vec![
                ModelThinkingLevel::Low,
                ModelThinkingLevel::Medium,
                ModelThinkingLevel::High
            ]
        );
    }

    #[test]
    fn merges_agent_singletons_into_their_family() {
        let fallback = crate::providers::antigravity_models::compiled_catalog();
        let catalog = build_antigravity_catalog(
            &raw(&[
                ("gemini-3.9-flash-low", info("Gemini 3.9 Flash (Low)")),
                ("gemini-3.9-flash-agent", info("Gemini 3.9 Flash")),
            ]),
            &fallback,
        );
        assert!(!catalog.models.iter().any(|model| model.id == "gemini-3.9-flash-agent"));
        assert_eq!(
            catalog.routing["gemini-3.9-flash"].high.as_deref(),
            Some("gemini-3.9-flash-agent")
        );
        assert_eq!(
            catalog.routing["gemini-3.9-flash"].low.as_deref(),
            Some("gemini-3.9-flash-low")
        );
    }

    #[test]
    fn tiered_entries_route_to_the_fallback_family() {
        let fallback = crate::providers::antigravity_models::compiled_catalog();
        let catalog = build_antigravity_catalog(
            &raw(&[("gemini-3.7-flash-tiered", info("Gemini 3.7 Flash"))]),
            &fallback,
        );
        // The compiled 3.7 family is reused (model + routing), and the other fallback
        // models are appended so the catalog is never partially wiped.
        assert_eq!(catalog.models.len(), fallback.models.len());
        assert_eq!(
            catalog.routing["gemini-3.7-flash"].high.as_deref(),
            Some("gemini-3.7-flash-high")
        );
    }

    #[test]
    fn humanizes_public_ids() {
        assert_eq!(humanize_public_id("gemini-3.1-pro"), "Gemini 3.1 Pro");
        assert_eq!(humanize_public_id("gpt-oss-120b"), "GPT-OSS 120B");
        assert_eq!(humanize_public_id("claude-opus-4-6"), "Claude Opus 4.6");
    }

    #[test]
    fn selection_filters_internal_and_image_ids() {
        assert!(is_selectable_runtime_model_id("gemini-3.9-flash-low"));
        assert!(is_selectable_runtime_model_id("claude-sonnet-4-6"));
        assert!(is_selectable_runtime_model_id("gpt-oss-120b-medium"));
        assert!(!is_selectable_runtime_model_id("chat_hidden"));
        assert!(!is_selectable_runtime_model_id("tab_completion"));
        assert!(!is_selectable_runtime_model_id("gemini-3-pro-image"));
        assert!(!is_selectable_runtime_model_id("MODEL_PLACEHOLDER_M1"));
        assert!(!is_selectable_runtime_model_id("gemini 3 flash"));
    }

    #[test]
    fn resolved_catalog_prefers_discovered_with_models() {
        let fallback = crate::providers::antigravity_models::compiled_catalog();
        let empty = AntigravityCatalog {
            models: Vec::new(),
            routing: std::collections::HashMap::new(),
        };
        assert_eq!(resolved_catalog(Some(&empty), &fallback), fallback);
        assert_eq!(
            resolved_catalog(Some(&fallback), &empty).models.len(),
            fallback.models.len()
        );
        assert_eq!(resolved_catalog(None, &fallback), fallback);
    }
}
