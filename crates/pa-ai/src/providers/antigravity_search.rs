//! Antigravity Google Search grounding and the optional `google_search` model tool.
//!
//! Ported from `pi-antigravity` (extension 0.10.0):
//!
//! - `src/search/search.ts` — request build/parse, citation formatting, and the endpoint
//!   fallback execution loop.
//! - `src/features/google-search.ts` — the tool definition (`google_search`) and its
//!   enable/disable decision. Tool *registration* and the Pi `onUpdate` progress callback are
//!   UI concerns and are not ported; the spec constants below are what a Rust tool registry
//!   should register.
#![allow(dead_code)] // parity surface: tool-registration glue lives outside pa-ai

use std::collections::{BTreeMap, HashSet};

use serde::Serialize;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::providers::antigravity_types::{
    agent_request_id, antigravity_headers, endpoint_candidates, is_extra_tool_enabled, is_record,
    json_or_text_error, parse_api_key, safe_error, value_as_number, AntigravityHttp,
};

/// Default search model: fast grounding model for interactive tool calls.
pub const DEFAULT_SEARCH_MODEL: &str = "gemini-3.5-flash-lite";

/// Fallback runtime candidates if the primary model is missing or out of capacity.
pub const SEARCH_MODEL_FALLBACKS: &[&str] = &[
    "gemini-3.5-flash-lite",
    "gemini-3.1-flash-lite",
    "gemini-3-flash",
];

/// System instruction for grounded research briefs.
pub const SEARCH_SYSTEM_INSTRUCTION: &str = r#"You are a careful research assistant using Google Search grounding.
Return a compact evidence brief with exactly these sections:
## Findings
## Gaps
## Next checks

Rules:
- Verify each claim inside the specific item, function, or section it refers to. Do not copy an attribute from a neighboring entry on the same page.
- Prefer primary documentation and exact names, versions, and constraints over marketing summaries.
- If a fact is not supported by a retrieved source, put it under Gaps instead of stating it as a finding.
- Keep the brief short enough for an interactive agent tool call."#;

/// One grounded web source.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SearchSource {
    /// Original groundingChunks index, including gaps from skipped non-web chunks.
    pub index: Option<usize>,
    pub title: String,
    pub url: String,
}

/// One grounding support segment (a citation span over a content Part).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SearchSupport {
    pub text: String,
    pub start_index: Option<f64>,
    pub end_index: Option<f64>,
    pub part_index: Option<f64>,
    pub source_indices: Vec<f64>,
}

/// One visible (non-thought) candidate part, keeping its original index.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SearchPart {
    pub index: usize,
    pub text: String,
}

/// Structured search result.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct SearchResult {
    pub text: String,
    pub sources: Vec<SearchSource>,
    pub queries: Vec<String>,
    /// Optional so existing callers can still construct results with the original shape.
    pub parts: Option<Vec<SearchPart>>,
    pub supports: Option<Vec<SearchSupport>>,
}

/// Arguments for the interactive `/antigravity.search` command.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchCommandArgs {
    pub query: String,
    pub urls: Option<Vec<String>>,
    pub thinking: Option<bool>,
}

/// Options accepted by [`build_search_request`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchRequestOptions {
    pub query: String,
    pub instruction: Option<String>,
    pub urls: Option<Vec<String>>,
    pub thinking: Option<bool>,
}

/// Options for [`execute_antigravity_search`].
#[derive(Clone, Debug, Default)]
pub struct ExecuteSearchOptions {
    pub api_key: Option<String>,
    pub query: String,
    pub instruction: Option<String>,
    pub urls: Option<Vec<String>>,
    pub thinking: Option<bool>,
    pub signal: Option<CancellationToken>,
}

/// Parse arguments for the interactive `/antigravity.search` command.
///
/// Supports `--thinking` and `--url <url>` (repeatable).
#[must_use]
pub fn parse_search_command_args(args: &str) -> SearchCommandArgs {
    let tokens: Vec<&str> = args.split_whitespace().collect();
    let mut out = SearchCommandArgs::default();
    let mut rest: Vec<&str> = Vec::new();
    let mut index = 0;
    while index < tokens.len() {
        let token = tokens[index];
        let next = tokens.get(index + 1).copied();
        if token == "--thinking" {
            out.thinking = Some(true);
            index += 1;
            continue;
        }
        if token == "--url" {
            if let Some(url) = next {
                out.urls.get_or_insert_with(Vec::new).push(url.to_string());
                index += 2;
                continue;
            }
        }
        rest.push(token);
        index += 1;
    }
    out.query = rest.join(" ");
    out
}

/// Build the Antigravity wire request payload for Gemini Google Search grounding.
#[must_use]
pub fn build_search_request(
    options: &SearchRequestOptions,
    model: &str,
    project_id: &str,
) -> Value {
    let instruction = options
        .instruction
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());

    let mut system_instruction_text = SEARCH_SYSTEM_INSTRUCTION.to_string();
    if let Some(instruction) = instruction {
        system_instruction_text.push_str(&format!(
            "\n\n[SPECIFIC DIRECTIVE FROM LEAD AGENT]:\n{instruction}\nYou MUST strictly follow this directive during search query formulation and synthesis."
        ));
    }

    let mut prompt = options.query.trim().to_string();
    if let Some(instruction) = instruction {
        prompt = format!("[Lead Agent Directive]: {instruction}\n\nSearch Query: {prompt}");
    }
    let urls: &[String] = options.urls.as_deref().unwrap_or(&[]);
    if !urls.is_empty() {
        prompt.push_str(&format!("\n\nURLs to analyze in detail:\n{}", urls.join("\n")));
    }

    let mut tools = vec![json!({ "googleSearch": {} })];
    if !urls.is_empty() {
        tools.push(json!({ "urlContext": {} }));
    }

    // Default budget stays at 0 so interactive lookups stay fast; --thinking opts into planning.
    let thinking_budget = if options.thinking == Some(true) { 4096 } else { 0 };
    // Cloud Code rejects `thinkingBudget: 0` with HTTP 400 ("Request contains an invalid
    // argument"), so send the budget only when it is non-zero. `includeThoughts: false` alone is
    // accepted and stays closest to the extension's wire config.
    let thinking_config = if thinking_budget == 0 {
        json!({ "includeThoughts": false })
    } else {
        json!({ "thinkingBudget": thinking_budget, "includeThoughts": false })
    };

    json!({
        "project": project_id,
        "model": model,
        "request": {
            "systemInstruction": {
                "role": "user",
                "parts": [{ "text": system_instruction_text }],
            },
            "contents": [
                {
                    "role": "user",
                    "parts": [{ "text": prompt }],
                },
            ],
            "tools": tools,
            "generationConfig": {
                "thinkingConfig": thinking_config,
            },
        },
        "requestType": "agent",
        "userAgent": "antigravity",
        "requestId": agent_request_id(1),
    })
}

/// Read candidates from either the wrapped Antigravity or the direct Gemini response.
#[must_use]
pub fn candidate_list(data: &Value) -> Value {
    if !is_record(data) {
        return Value::Null;
    }
    if data.get("response").is_some_and(is_record) {
        return data
            .get("response")
            .and_then(|response| response.get("candidates"))
            .cloned()
            .unwrap_or(Value::Null);
    }
    data.get("candidates").cloned().unwrap_or(Value::Null)
}

fn first_unknown_item(value: &Value) -> Option<&Value> {
    value.as_array().and_then(|items| items.first())
}

/// Parse visible candidate text and grounding metadata from an Antigravity response.
///
/// Preserves original part and source indices so UTF-8 citation offsets remain usable.
#[must_use]
pub fn parse_search_response(data: &Value) -> SearchResult {
    let mut result = SearchResult {
        text: String::new(),
        sources: Vec::new(),
        queries: Vec::new(),
        parts: Some(Vec::new()),
        supports: Some(Vec::new()),
    };
    if !is_record(data) {
        return result;
    }

    let response_obj = if data.get("response").is_some_and(is_record) {
        data.get("response").unwrap_or(data)
    } else {
        data
    };
    let candidates: Vec<Value> = response_obj
        .get("candidates")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let Some(candidate) = candidates.first() else {
        let error_obj = data
            .get("error")
            .filter(|value| value.is_object())
            .or_else(|| response_obj.get("error").filter(|value| value.is_object()));
        let message = error_obj
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("No candidate returned from Antigravity Search");
        result.text = format!("Error: {message}");
        return result;
    };
    if !is_record(candidate) {
        result.text = "Error: No candidate returned from Antigravity Search".to_string();
        return result;
    }

    if let Some(parts) = candidate
        .get("content")
        .and_then(|content| content.get("parts"))
        .and_then(Value::as_array)
    {
        let mut collected = Vec::new();
        for (index, part) in parts.iter().enumerate() {
            let is_thought = part.get("thought").is_some_and(crate::providers::antigravity_types::js_truthy);
            let text = part.get("text").and_then(Value::as_str);
            if !is_thought {
                if let Some(text) = text {
                    if !text.is_empty() {
                        collected.push(SearchPart {
                            index,
                            text: text.to_string(),
                        });
                    }
                }
            }
        }
        result.text = collected
            .iter()
            .map(|part| part.text.clone())
            .collect::<Vec<_>>()
            .join("\n\n");
        result.parts = Some(collected);
    }

    let Some(grounding) = candidate
        .get("groundingMetadata")
        .filter(|value| value.is_object())
    else {
        return result;
    };

    if let Some(queries) = grounding.get("webSearchQueries").and_then(Value::as_array) {
        result.queries = queries
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
    }

    if let Some(chunks) = grounding.get("groundingChunks").and_then(Value::as_array) {
        for (index, chunk) in chunks.iter().enumerate() {
            if !is_record(chunk) {
                continue;
            }
            let web = chunk.get("web");
            let Some(uri) = web.and_then(|web| web.get("uri")).and_then(Value::as_str) else {
                continue;
            };
            let title = web
                .and_then(|web| web.get("title"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|title| !title.is_empty())
                .unwrap_or(uri)
                .to_string();
            result.sources.push(SearchSource {
                index: Some(index),
                title,
                url: uri.to_string(),
            });
        }
    }

    if let Some(supports) = grounding.get("groundingSupports").and_then(Value::as_array) {
        let source_indices: HashSet<i64> = result
            .sources
            .iter()
            .enumerate()
            .map(|(position, source)| source.index.map_or(position as i64, |index| index as i64))
            .collect();
        let text_part_indices: HashSet<i64> = result
            .parts
            .as_ref()
            .map(|parts| parts.iter().map(|part| part.index as i64).collect())
            .unwrap_or_default();
        for support in supports {
            if !is_record(support) {
                continue;
            }
            let Some(segment) = support.get("segment").filter(|value| value.is_object()) else {
                continue;
            };
            let Some(segment_text) = segment.get("text").and_then(Value::as_str) else {
                continue;
            };
            let Some(chunk_indices) = support.get("groundingChunkIndices").and_then(Value::as_array)
            else {
                continue;
            };
            let part_index = match segment.get("partIndex") {
                None | Some(Value::Null) => Some(0.0),
                Some(value) => value_as_number(value),
            };
            let Some(part_index) = part_index else {
                continue;
            };
            if part_index.fract() != 0.0 || !text_part_indices.contains(&(part_index as i64)) {
                continue;
            }
            let indices: Vec<f64> = chunk_indices
                .iter()
                .filter_map(|index| {
                    let number = value_as_number(index)?;
                    if number.fract() != 0.0 || !source_indices.contains(&(number as i64)) {
                        return None;
                    }
                    Some(number)
                })
                .collect();
            if indices.is_empty() {
                continue;
            }
            result.supports.get_or_insert_with(Vec::new).push(SearchSupport {
                text: segment_text.to_string(),
                start_index: segment.get("startIndex").and_then(value_as_number),
                end_index: segment.get("endIndex").and_then(value_as_number),
                part_index: segment.get("partIndex").and_then(value_as_number),
                source_indices: indices,
            });
        }
    }

    result
}

/// Google's offsets are UTF-8 bytes within a content Part, not character indices. Verify each
/// segment before inserting citations; skip unsafe mappings.
fn cite_search_text(res: &SearchResult) -> String {
    let supports = res.supports.as_deref().unwrap_or(&[]);
    if supports.is_empty() {
        return res.text.clone();
    }
    let default_parts = vec![SearchPart {
        index: 0,
        text: res.text.clone(),
    }];
    let text_parts = res.parts.as_deref().unwrap_or(&default_parts);
    let joined = text_parts
        .iter()
        .map(|part| part.text.clone())
        .collect::<Vec<_>>()
        .join("\n\n");
    // Respect callers editing result.text; original offsets no longer apply.
    if joined != res.text {
        return res.text.clone();
    }

    let source_indices: HashSet<i64> = res
        .sources
        .iter()
        .enumerate()
        .map(|(position, source)| source.index.map_or(position as i64, |index| index as i64))
        .collect();

    let mut rendered: Vec<String> = Vec::new();
    for part in text_parts {
        let bytes = part.text.as_bytes().to_vec();
        let mut insertions: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
        for support in supports {
            let start = support.start_index.unwrap_or(0.0);
            let Some(end) = support.end_index else {
                continue;
            };
            if support.part_index.unwrap_or(0.0) != part.index as f64
                || start.fract() != 0.0
                || start < 0.0
                || end.fract() != 0.0
                || end <= start
                || end > bytes.len() as f64
            {
                continue;
            }
            let segment = &bytes[start as usize..end as usize];
            if String::from_utf8_lossy(segment).as_ref() != support.text.as_str() {
                continue;
            }
            let entry = insertions.entry(end as i64).or_default();
            for index in &support.source_indices {
                if index.fract() == 0.0 && *index >= 0.0 && source_indices.contains(&(*index as i64)) {
                    let value = *index as i64;
                    if !entry.contains(&value) {
                        entry.push(value);
                    }
                }
            }
        }

        let mut cited = bytes;
        for (end, indices) in insertions.iter().rev() {
            let references: String = indices.iter().map(|index| format!("[{}]", index + 1)).collect();
            let position = *end as usize;
            let mut next = Vec::with_capacity(cited.len() + references.len());
            next.extend_from_slice(&cited[..position]);
            next.extend_from_slice(references.as_bytes());
            next.extend_from_slice(&cited[position..]);
            cited = next;
        }
        rendered.push(String::from_utf8_lossy(&cited).into_owned());
    }

    let joined = rendered.join("\n\n");
    if joined.is_empty() {
        res.text.clone()
    } else {
        joined
    }
}

/// Format a structured search result as clean Markdown for agent consumption.
#[must_use]
pub fn format_search_result(res: &SearchResult) -> String {
    let mut sections: Vec<String> = Vec::new();

    let cited = cite_search_text(res);
    if !cited.is_empty() {
        sections.push(cited);
    }

    if !res.sources.is_empty() {
        let has_supports = res.supports.as_ref().is_some_and(|supports| !supports.is_empty());
        let list = res
            .sources
            .iter()
            .enumerate()
            .map(|(position, source)| {
                let number = source.index.map_or(position as i64, |index| index as i64) + 1;
                let prefix = if has_supports {
                    format!("[{number}] ")
                } else {
                    String::new()
                };
                format!("- {prefix}[{}]({})", source.title, source.url)
            })
            .collect::<Vec<_>>()
            .join("\n");
        sections.push(format!("### Sources\n{list}"));
    }

    if !res.queries.is_empty() {
        let queries = res
            .queries
            .iter()
            .map(|query| format!("`{query}`"))
            .collect::<Vec<_>>()
            .join(", ");
        sections.push(format!("*Search queries: {queries}*"));
    }

    sections.join("\n\n")
}

fn take_chars(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

/// Execute real-time Google Search grounding via the Antigravity backend with automatic
/// fallback across models and endpoints.
pub async fn execute_antigravity_search(
    options: &ExecuteSearchOptions,
    http: &dyn AntigravityHttp,
) -> Result<String, String> {
    let query = options.query.trim();
    if query.is_empty() {
        return Err("Search query is required.".to_string());
    }

    let creds = parse_api_key(options.api_key.as_deref())?;
    let headers = antigravity_headers(&creds.token);
    let endpoints = endpoint_candidates();

    let mut last_error = "no endpoint available".to_string();
    let request_options = SearchRequestOptions {
        query: options.query.clone(),
        instruction: options.instruction.clone(),
        urls: options.urls.clone(),
        thinking: options.thinking,
    };

    for &model in SEARCH_MODEL_FALLBACKS {
        let body = build_search_request(&request_options, model, &creds.project_id).to_string();
        for endpoint in &endpoints {
            if options
                .signal
                .as_ref()
                .is_some_and(CancellationToken::is_cancelled)
            {
                return Err("Search request was aborted".to_string());
            }
            let url = format!("{endpoint}/v1internal:generateContent");
            let response = http
                .post(
                    &url,
                    headers.clone(),
                    body.clone().into_bytes(),
                    options.signal.clone(),
                )
                .await;

            match response {
                Ok(response) => {
                    if !response.ok() {
                        last_error = take_chars(&json_or_text_error(&response.text()), 400);
                        if response.status == 404
                            || [429, 500, 502, 503, 504].contains(&response.status)
                        {
                            continue;
                        }
                        // Fail fast on non-retryable client errors (e.g. 400/401/403).
                        return Err(last_error);
                    }

                    let data = response.json();
                    let candidates = candidate_list(&data);
                    let candidate_text = first_unknown_item(&candidates)
                        .and_then(|candidate| candidate.get("content"))
                        .and_then(|content| content.get("parts"))
                        .and_then(Value::as_array)
                        .and_then(|parts| parts.first())
                        .and_then(|part| part.get("text"))
                        .and_then(Value::as_str);

                    if let Some(text) = candidate_text {
                        if text.contains("is no longer available. Please switch") {
                            last_error =
                                format!("Model {model} deprecated: {}", take_chars(text, 100));
                            break;
                        }
                    }

                    let parsed = parse_search_response(&data);
                    return Ok(format_search_result(&parsed));
                }
                Err(error) => {
                    if options
                        .signal
                        .as_ref()
                        .is_some_and(CancellationToken::is_cancelled)
                    {
                        return Err(error);
                    }
                    last_error = take_chars(&safe_error(&error), 400);
                }
            }
        }
    }

    Err(format!("Antigravity Google Search failed: {last_error}"))
}

// ---------------------------------------------------------------------------------------------
// features/google-search.ts — the optional model-facing tool
// ---------------------------------------------------------------------------------------------

/// Tool name registered with the agent.
pub const GOOGLE_SEARCH_TOOL_NAME: &str = "google_search";
/// Human-readable tool label.
pub const GOOGLE_SEARCH_TOOL_LABEL: &str = "Google Search";
/// Tool description shown to the model.
pub const GOOGLE_SEARCH_TOOL_DESCRIPTION: &str = "Search the web using Google Search via Antigravity Grounding. Returns a compact evidence brief with source citations. Use this whenever current, real-time information or web research is needed.";
/// One-line prompt snippet.
pub const GOOGLE_SEARCH_TOOL_PROMPT_SNIPPET: &str =
    "Real-time Google Search grounding via Antigravity";
/// Extra guidance lines for the model prompt.
pub const GOOGLE_SEARCH_TOOL_PROMPT_GUIDELINES: &[&str] = &[
    "Use google_search when you need real-time, up-to-date web information, latest news, documentation, or fact checking.",
    "Pass specific URLs into the urls array if you want the search engine to fetch and analyze specific pages.",
    "Set instruction to provide special research directives from the lead agent (e.g. focus on issues, compare benchmarks, restrict time range).",
];

/// Whether the `google_search` tool should be registered (`isExtraToolEnabled("SEARCH")`).
#[must_use]
pub fn google_search_tool_enabled() -> bool {
    is_extra_tool_enabled("SEARCH")
}

/// JSON Schema for the `google_search` parameters (`Type.Object` in the TS extension).
#[must_use]
pub fn google_search_tool_parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "query": { "type": "string", "description": "Search query or question." },
            "instruction": {
                "type": "string",
                "description": "Specific directive or focus from the lead agent (e.g., '重点关注英文社区近七天的讨论', 'focus on GitHub issues and benchmarks', 'ignore marketing announcements').",
            },
            "urls": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Optional specific URLs to fetch and analyze alongside the search.",
            },
            "thinking": {
                "type": "boolean",
                "description": "Enable deeper thinking for complex analysis (default: false).",
            },
        },
        "required": ["query"],
        "additionalProperties": false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_command_args() {
        let simple = parse_search_command_args("mimo 2.6 release date");
        assert_eq!(simple.query, "mimo 2.6 release date");
        assert_eq!(simple.thinking, None);
        assert_eq!(simple.urls, None);

        let with_flags = parse_search_command_args(
            "--thinking --url https://example.com/doc1 --url https://example.com/doc2 deep dive on specs",
        );
        assert_eq!(with_flags.query, "deep dive on specs");
        assert_eq!(with_flags.thinking, Some(true));
        assert_eq!(
            with_flags.urls.as_deref(),
            Some(
                [
                    "https://example.com/doc1".to_string(),
                    "https://example.com/doc2".to_string()
                ]
                .as_slice()
            )
        );
    }

    #[test]
    fn builds_the_grounding_request() {
        let request = build_search_request(
            &SearchRequestOptions {
                query: "compare models".to_string(),
                instruction: Some("focus on benchmarks".to_string()),
                urls: Some(vec!["https://benchmark.org".to_string()]),
                thinking: Some(true),
            },
            DEFAULT_SEARCH_MODEL,
            "test-project-123",
        );

        assert_eq!(request["project"], "test-project-123");
        assert_eq!(request["model"], DEFAULT_SEARCH_MODEL);
        assert_eq!(DEFAULT_SEARCH_MODEL, "gemini-3.5-flash-lite");
        assert_eq!(
            SEARCH_MODEL_FALLBACKS.join(","),
            "gemini-3.5-flash-lite,gemini-3.1-flash-lite,gemini-3-flash"
        );
        assert!(SEARCH_SYSTEM_INSTRUCTION.contains("## Findings"));
        assert!(SEARCH_SYSTEM_INSTRUCTION.contains("## Gaps"));
        assert!(SEARCH_SYSTEM_INSTRUCTION.contains("## Next checks"));
        assert!(SEARCH_SYSTEM_INSTRUCTION.contains("neighboring entry"));

        let fast = build_search_request(
            &SearchRequestOptions {
                query: "clock.monotonic".to_string(),
                ..SearchRequestOptions::default()
            },
            DEFAULT_SEARCH_MODEL,
            "proj",
        );
        // Off (no `--thinking`) must not send `thinkingBudget: 0` — Cloud Code rejects it.
        assert!(fast["request"]["generationConfig"]["thinkingConfig"]
            .get("thinkingBudget")
            .is_none());
        assert_eq!(
            fast["request"]["generationConfig"]["thinkingConfig"]["includeThoughts"],
            false
        );
        assert_eq!(request["requestType"], "agent");
        assert_eq!(request["userAgent"], "antigravity");
        assert!(!request["requestId"].as_str().unwrap_or("").is_empty());

        let tools = request["request"]["tools"].as_array().unwrap();
        assert!(tools.iter().any(|tool| tool.get("googleSearch").is_some()));
        assert!(tools.iter().any(|tool| tool.get("urlContext").is_some()));
        assert!(request["request"]["contents"][0]["parts"][0]["text"]
            .as_str()
            .unwrap()
            .contains("focus on benchmarks"));
        assert!(request["request"]["systemInstruction"]["parts"][0]["text"]
            .as_str()
            .unwrap()
            .contains("focus on benchmarks"));
        assert_eq!(request["request"]["generationConfig"]["thinkingConfig"]["thinkingBudget"], 4096);
    }

    #[test]
    fn parses_and_formats_a_wrapped_response() {
        let mock = json!({
            "response": {
                "candidates": [{
                    "content": {
                        "role": "model",
                        "parts": [
                            { "thought": true, "text": "Searching internal thoughts..." },
                            { "text": "MiMo-V2.6 was released on September 22, 2026." }
                        ]
                    },
                    "finishReason": "STOP",
                    "groundingMetadata": {
                        "webSearchQueries": ["mimo 2.6 release", "mimo specs"],
                        "groundingChunks": [{
                            "web": { "uri": "https://example.com/article", "title": "Example Article" }
                        }]
                    }
                }]
            }
        });
        let parsed = parse_search_response(&mock);
        assert_eq!(parsed.text, "MiMo-V2.6 was released on September 22, 2026.");
        assert_eq!(parsed.queries, vec!["mimo 2.6 release", "mimo specs"]);
        assert_eq!(parsed.sources.len(), 1);
        assert_eq!(parsed.sources[0].url, "https://example.com/article");
        assert_eq!(parsed.sources[0].title, "Example Article");

        let markdown = format_search_result(&parsed);
        assert!(markdown.contains("MiMo-V2.6 was released on September 22, 2026."));
        assert!(markdown.contains("### Sources"));
        assert!(markdown.contains("[Example Article](https://example.com/article)"));
        assert!(markdown.contains("*Search queries: `mimo 2.6 release`, `mimo specs`*"));
    }

    #[test]
    fn cites_utf8_byte_offsets_and_skips_unsafe_supports() {
        let text = "日本語 clock. More.";
        let first_end = "日本語 clock.".len() as f64;
        let grounded = parse_search_response(&json!({
            "candidates": [{
                "content": { "parts": [
                    { "text": "First part." },
                    { "thought": true, "text": "Hidden" },
                    { "text": text }
                ]},
                "groundingMetadata": {
                    "groundingChunks": [
                        { "web": { "uri": "https://example.com/a", "title": "A" } },
                        { "other": {} },
                        { "web": { "uri": "https://example.com/b", "title": "B" } }
                    ],
                    "groundingSupports": [
                        { "segment": { "text": "First part.", "endIndex": 11 }, "groundingChunkIndices": [0] },
                        { "segment": { "text": "日本語 clock.", "endIndex": first_end, "partIndex": 2 }, "groundingChunkIndices": [0, 2, 99, -1, 0.5, "0"] },
                        { "segment": { "text": "日本語 clock.", "endIndex": first_end, "partIndex": 2 }, "groundingChunkIndices": [2, 0] },
                        { "segment": { "text": text, "endIndex": text.len(), "partIndex": 2 }, "groundingChunkIndices": [2] },
                        { "segment": { "text": "Hidden", "endIndex": 6, "partIndex": 1 }, "groundingChunkIndices": [0] },
                        { "segment": { "text": "Wrong", "endIndex": 5, "partIndex": 2 }, "groundingChunkIndices": [0] },
                        { "segment": { "text": "Too far", "endIndex": 999, "partIndex": 2 }, "groundingChunkIndices": [0] },
                        { "segment": { "text": "No offset" }, "groundingChunkIndices": [0] },
                        { "segment": { "text": "No web source", "endIndex": 13 }, "groundingChunkIndices": [1] },
                        { "segment": null, "groundingChunkIndices": [0] }
                    ]
                }
            }]
        }));

        assert_eq!(
            grounded.parts.as_ref().unwrap().iter().map(|part| part.index).collect::<Vec<_>>(),
            vec![0, 2]
        );
        assert_eq!(
            grounded.sources.iter().map(|source| source.index).collect::<Vec<_>>(),
            vec![Some(0), Some(2)]
        );
        assert_eq!(
            grounded.supports.as_ref().unwrap()[1].source_indices,
            vec![0.0, 2.0]
        );
        assert!(!format!("{grounded:?}").contains("Hidden"));

        let cited = format_search_result(&grounded);
        assert!(cited.starts_with("First part.[1]\n\n日本語 clock.[1][3] More.[3]"));
        assert!(cited.contains("- [3] [B](https://example.com/b)"));
        assert!(!cited.contains("Hidden"));
        assert!(!cited.contains('\u{FFFD}'));

        let edited = SearchResult {
            text: "Edited answer".to_string(),
            ..grounded.clone()
        };
        assert!(format_search_result(&edited).starts_with("Edited answer"));
    }

    #[test]
    fn formatter_rejects_unknown_source_indices() {
        let result = SearchResult {
            text: "Fact.".to_string(),
            sources: vec![SearchSource {
                index: Some(2),
                title: "Source".to_string(),
                url: "https://example.com".to_string(),
            }],
            queries: Vec::new(),
            parts: None,
            supports: Some(vec![SearchSupport {
                text: "Fact.".to_string(),
                start_index: None,
                end_index: Some(5.0),
                part_index: None,
                source_indices: vec![99.0, -1.0, 0.5, f64::NAN, 2.0, 2.0],
            }]),
        };
        assert_eq!(
            format_search_result(&result),
            "Fact.[3]\n\n### Sources\n- [3] [Source](https://example.com)"
        );
    }

    #[test]
    fn preserves_legacy_result_shapes() {
        let legacy = SearchResult {
            text: "Legacy result".to_string(),
            sources: vec![SearchSource {
                index: None,
                title: "Source".to_string(),
                url: "https://example.com".to_string(),
            }],
            queries: Vec::new(),
            parts: None,
            supports: None,
        };
        assert_eq!(
            format_search_result(&legacy),
            "Legacy result\n\n### Sources\n- [Source](https://example.com)"
        );
        assert_eq!(
            format_search_result(&SearchResult {
                text: "No sources".to_string(),
                ..SearchResult::default()
            }),
            "No sources"
        );
    }

    #[tokio::test]
    async fn search_requires_a_query() {
        let http = crate::providers::antigravity_types::ReqwestAntigravityHttp::new();
        let error = execute_antigravity_search(&ExecuteSearchOptions::default(), &http)
            .await
            .unwrap_err();
        assert_eq!(error, "Search query is required.");
    }

    #[test]
    fn tool_spec_is_stable() {
        let parameters = google_search_tool_parameters();
        let mut keys: Vec<&str> = parameters["properties"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["instruction", "query", "thinking", "urls"]);
    }
}
