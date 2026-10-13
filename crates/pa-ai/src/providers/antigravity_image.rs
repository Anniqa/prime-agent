//! Antigravity image generation and the optional `generate_image` model tool.
//!
//! Ported from `pi-antigravity` (extension 0.10.0):
//!
//! - `src/image/image.ts` — request build, direct/SSE response parsing, save-path resolution.
//! - `src/image/native.ts` — the in-memory image API (identity + output assembly).
//! - `src/features/generate-image.ts` — the tool definition (`generate_image`) and its
//!   enable/disable decision; tool registration and progress callbacks are outside pa-ai.
#![allow(dead_code)] // parity surface: tool-registration glue lives outside pa-ai

use std::path::{Component, Path, PathBuf};

use base64::Engine;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::providers::antigravity_types::{
    antigravity_headers, endpoint_candidates, is_extra_tool_enabled, is_record, json_or_text_error,
    parse_api_key, random_uuid_v4, safe_error, sanitize_text, AntigravityHttp,
    AntigravityHttpResponse,
};

/// Default image model.
pub const DEFAULT_IMAGE_MODEL: &str = "gemini-3.1-flash-image";

/// Allowed aspect ratios.
pub const IMAGE_ASPECT_RATIOS: &[&str] = &[
    "1:1", "2:3", "3:2", "3:4", "4:3", "4:5", "5:4", "9:16", "16:9", "21:9",
];

const IMAGE_MODEL_FALLBACKS: &[&str] = &[
    "gemini-3.1-flash-image",
    "gemini-3-pro-image",
    "gemini-3-pro-image-preview",
];

const DEFAULT_IMAGE_DIR: &str = ".pi/generated-images";
const MAX_PROMPT_CHARS: usize = 8000;

/// One generated image (`GeneratedImage` in `types.ts`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GeneratedImage {
    pub data: String,
    pub mime_type: String,
}

/// Arguments for the interactive `/antigravity.image` command.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImageCommandArgs {
    pub prompt: String,
    pub aspect_ratio: Option<String>,
    pub model: Option<String>,
    pub path: Option<String>,
}

/// Options for image generation.
#[derive(Clone, Debug, Default)]
pub struct GenerateImageOptions {
    pub api_key: String,
    pub cwd: String,
    pub prompt: String,
    pub aspect_ratio: Option<String>,
    pub model: Option<String>,
    pub path: Option<String>,
    pub signal: Option<CancellationToken>,
    pub input_images: Vec<GeneratedImage>,
}

/// In-memory generation result (`Omit<GenerateImageResult, "savedPaths">`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GeneratedImageContent {
    pub images: Vec<GeneratedImage>,
    pub text: Vec<String>,
    pub model: String,
}

/// Generation result including the saved file paths.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GenerateImageResult {
    pub images: Vec<GeneratedImage>,
    pub saved_paths: Vec<String>,
    pub text: Vec<String>,
    pub model: String,
}

fn image_extension(mime_type: &str) -> &'static str {
    let lower = mime_type.to_lowercase();
    if lower.contains("jpeg") || lower.contains("jpg") {
        return "jpg";
    }
    if lower.contains("webp") {
        return "webp";
    }
    if lower.contains("gif") {
        return "gif";
    }
    "png"
}

/// Validate an image model id (`assertSafeImageModel`).
pub fn assert_safe_image_model(model_id: &str) -> Result<String, String> {
    let id = model_id.trim();
    if id.is_empty() || id.chars().count() > 80 {
        return Err("Unsupported image model id.".to_string());
    }
    let pattern =
        regex::Regex::new(r"(?i)^(gemini-[a-z0-9.+-]*image[a-z0-9.+-]*|imagen-[a-z0-9.+-]+)$")
            .map_err(|error| error.to_string())?;
    if !pattern.is_match(id) {
        return Err(format!("Unsupported image model: {id}"));
    }
    Ok(id.to_string())
}

/// Validate an aspect ratio against [`IMAGE_ASPECT_RATIOS`].
pub fn assert_safe_aspect_ratio(ratio: &str) -> Result<String, String> {
    let value = ratio.trim();
    if IMAGE_ASPECT_RATIOS.contains(&value) {
        return Ok(value.to_string());
    }
    Err(format!(
        "Unsupported aspect ratio: {value}. Use one of {}.",
        IMAGE_ASPECT_RATIOS.join(", ")
    ))
}

/// Parse arguments for the interactive `/antigravity.image` command.
#[must_use]
pub fn parse_image_command_args(args: &str) -> ImageCommandArgs {
    let tokens: Vec<&str> = args.split_whitespace().collect();
    let mut out = ImageCommandArgs::default();
    let mut rest: Vec<&str> = Vec::new();
    let mut index = 0;
    while index < tokens.len() {
        let token = tokens[index];
        let next = tokens.get(index + 1).copied();
        if token == "--ratio" || token == "--aspect-ratio" {
            if let Some(value) = next {
                out.aspect_ratio = Some(value.to_string());
                index += 2;
                continue;
            }
        }
        if token == "--model" {
            if let Some(value) = next {
                out.model = Some(value.to_string());
                index += 2;
                continue;
            }
        }
        if token == "--path" {
            if let Some(value) = next {
                out.path = Some(value.to_string());
                index += 2;
                continue;
            }
        }
        rest.push(token);
        index += 1;
    }
    out.prompt = rest.join(" ");
    out
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn relative_path(root: &Path, target: &Path) -> Option<PathBuf> {
    let root_components: Vec<Component> = root.components().collect();
    let target_components: Vec<Component> = target.components().collect();
    let mut common = 0;
    while common < root_components.len()
        && common < target_components.len()
        && root_components[common] == target_components[common]
    {
        common += 1;
    }
    if common == 0 {
        return None;
    }
    let mut out = PathBuf::new();
    for _ in common..root_components.len() {
        out.push("..");
    }
    for component in &target_components[common..] {
        out.push(component.as_os_str());
    }
    Some(out)
}

/// Resolve a save path inside the working directory (`resolveImageSavePath`).
pub fn resolve_image_save_path(
    cwd: &str,
    requested: Option<&str>,
    mime_type: &str,
    index: Option<usize>,
) -> Result<String, String> {
    let ext = image_extension(mime_type);
    let stamp = iso_timestamp_utc(crate::utils_inner::diagnostics::now_ms());
    let suffix = index.map_or_else(String::new, |index| format!("-{}", index + 1));
    let default_name = format!("image-{stamp}{suffix}.{ext}");

    let root = normalize(Path::new(cwd));
    let requested = requested.map(str::trim).filter(|value| !value.is_empty());
    let target = match requested {
        Some(requested) => {
            let path = Path::new(requested);
            if path.is_absolute() {
                normalize(path)
            } else {
                normalize(&root.join(path))
            }
        }
        None => normalize(&root.join(DEFAULT_IMAGE_DIR).join(&default_name)),
    };

    let Some(rel) = relative_path(&root, &target) else {
        return Err("Image save path must be inside the working directory.".to_string());
    };
    let rel_text = rel.to_string_lossy().to_string();
    if rel_text.is_empty()
        || rel_text == ".."
        || rel_text.starts_with("../")
        || rel_text.starts_with("..\\")
        || Path::new(&rel_text).is_absolute()
    {
        return Err("Image save path must be inside the working directory.".to_string());
    }

    if target.extension().is_none() {
        return Ok(target.join(&default_name).to_string_lossy().to_string());
    }
    if requested.is_none() || index.is_none() {
        return Ok(target.to_string_lossy().to_string());
    }
    let target_text = target.to_string_lossy().to_string();
    let extension = target
        .extension()
        .map(|value| format!(".{}", value.to_string_lossy()))
        .unwrap_or_default();
    let stem_length = target_text.len().saturating_sub(extension.len());
    Ok(format!(
        "{}{}{}",
        &target_text[..stem_length],
        suffix,
        extension
    ))
}

/// Build the Antigravity wire request payload for image generation (`buildImageGenerateRequest`).
#[must_use]
pub fn build_image_generate_request(
    prompt: &str,
    model: &str,
    project_id: &str,
    aspect_ratio: &str,
    input_images: &[GeneratedImage],
) -> Value {
    let mut parts: Vec<Value> = vec![json!({ "text": sanitize_text(prompt) })];
    for image in input_images {
        parts.push(json!({
            "inlineData": { "mimeType": image.mime_type, "data": image.data }
        }));
    }

    json!({
        "project": project_id,
        "model": model,
        "request": {
            "contents": [
                {
                    "role": "user",
                    "parts": parts,
                },
            ],
            "generationConfig": {
                "imageConfig": { "aspectRatio": aspect_ratio },
                "candidateCount": 1,
            },
        },
        "requestType": "image_gen",
        "userAgent": "antigravity",
        "requestId": format!(
            "image_gen/{}/{}/1",
            crate::utils_inner::diagnostics::now_ms(),
            random_uuid_v4()
        ),
    })
}

fn collect_images_from_parts(
    parts: Option<&Vec<Value>>,
    images: &mut Vec<GeneratedImage>,
    text: &mut Vec<String>,
) {
    let Some(parts) = parts else {
        return;
    };
    for part in parts {
        if let Some(value) = part.get("text").and_then(Value::as_str) {
            if !value.is_empty() {
                text.push(value.to_string());
            }
        }
        let inline = part.get("inlineData");
        if let Some(data) = inline
            .and_then(|inline| inline.get("data"))
            .and_then(Value::as_str)
        {
            if !data.is_empty() {
                let mime_type = inline
                    .and_then(|inline| inline.get("mimeType"))
                    .and_then(Value::as_str)
                    .unwrap_or("image/png");
                images.push(GeneratedImage {
                    data: data.to_string(),
                    mime_type: mime_type.to_string(),
                });
            }
        }
    }
}

/// Incremental `text/event-stream` image collector (`collectImagesFromSse`), fed chunk by
/// chunk so a line split across chunks is reassembled.
#[derive(Clone, Debug, Default)]
pub struct ImageSseParser {
    buffer: String,
    pub images: Vec<GeneratedImage>,
    pub text: Vec<String>,
}

impl ImageSseParser {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one body chunk; returns the first streamed error message, if any.
    pub fn push(&mut self, chunk: &str) -> Result<(), String> {
        self.buffer.push_str(chunk);
        let lines: Vec<String> = self.buffer.split('\n').map(str::to_string).collect();
        let remainder = lines.last().cloned().unwrap_or_default();
        self.buffer = remainder;
        let count = lines.len();
        for line in lines.into_iter().take(count.saturating_sub(1)) {
            self.process_line(&line)?;
        }
        Ok(())
    }

    pub fn finish(&mut self) -> Result<(), String> {
        let remainder = std::mem::take(&mut self.buffer);
        if !remainder.is_empty() {
            self.process_line(&remainder)?;
        }
        Ok(())
    }

    fn process_line(&mut self, line: &str) -> Result<(), String> {
        let Some(payload) = line.strip_prefix("data:") else {
            return Ok(());
        };
        let payload = payload.trim();
        if payload.is_empty() || payload == "[DONE]" {
            return Ok(());
        }
        let Ok(chunk) = serde_json::from_str::<Value>(payload) else {
            return Ok(());
        };
        if let Some(message) = chunk
            .get("error")
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
        {
            return Err(message.to_string());
        }
        let response_data = chunk
            .get("response")
            .filter(|value| is_record(value))
            .unwrap_or(&chunk);
        if let Some(candidates) = response_data.get("candidates").and_then(Value::as_array) {
            for candidate in candidates {
                let parts = candidate
                    .get("content")
                    .and_then(|content| content.get("parts"))
                    .and_then(Value::as_array);
                collect_images_from_parts(parts, &mut self.images, &mut self.text);
            }
        }
        Ok(())
    }
}

/// Parse a full SSE body (the buffered transport seam's counterpart to `collectImagesFromSse`).
pub fn collect_images_from_sse(
    chunks: &[&str],
) -> Result<(Vec<GeneratedImage>, Vec<String>), String> {
    let mut parser = ImageSseParser::new();
    for chunk in chunks {
        parser.push(chunk)?;
    }
    parser.finish()?;
    Ok((parser.images, parser.text))
}

/// Issue the direct image request; only a 404 enables the legacy SSE endpoint
/// (`requestImageResponse`).
pub async fn request_image_response(
    endpoint: &str,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    signal: Option<CancellationToken>,
    http: &dyn AntigravityHttp,
) -> Result<(AntigravityHttpResponse, bool), String> {
    let response = http
        .post(
            &format!("{endpoint}/v1internal:generateContent"),
            headers.clone(),
            body.clone(),
            signal.clone(),
        )
        .await?;
    if response.status != 404 {
        return Ok((response, false));
    }
    // The 404 body is consumed by the seam read; fall back to the legacy SSE endpoint.
    let response = http
        .post(
            &format!("{endpoint}/v1internal:streamGenerateContent?alt=sse"),
            headers,
            body,
            signal,
        )
        .await?;
    Ok((response, true))
}

/// Extract images and notes from a wrapped or unwrapped direct response (`parseImageResponse`).
pub fn parse_image_response(chunk: &Value) -> Result<(Vec<GeneratedImage>, Vec<String>), String> {
    if let Some(message) = chunk
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
    {
        return Err(message.to_string());
    }
    let mut images = Vec::new();
    let mut text = Vec::new();
    let response_data = chunk
        .get("response")
        .filter(|value| is_record(value))
        .unwrap_or(chunk);
    if let Some(candidates) = response_data.get("candidates").and_then(Value::as_array) {
        for candidate in candidates {
            let parts = candidate
                .get("content")
                .and_then(|content| content.get("parts"))
                .and_then(Value::as_array);
            collect_images_from_parts(parts, &mut images, &mut text);
        }
    }
    Ok((images, text))
}

fn take_chars(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

/// Generate in-memory content shared by the native API and the file-saving wrapper
/// (`generateAntigravityImageContent`).
pub async fn generate_antigravity_image_content(
    options: &GenerateImageOptions,
    http: &dyn AntigravityHttp,
) -> Result<GeneratedImageContent, String> {
    let prompt = options.prompt.trim().to_string();
    if prompt.is_empty() && options.input_images.is_empty() {
        return Err("Image prompt or input image is required.".to_string());
    }
    if prompt.encode_utf16().count() > MAX_PROMPT_CHARS {
        return Err(format!(
            "Image prompt is too long (max {MAX_PROMPT_CHARS} characters)."
        ));
    }

    let aspect_ratio = assert_safe_aspect_ratio(options.aspect_ratio.as_deref().unwrap_or("1:1"))?;
    let preferred =
        assert_safe_image_model(options.model.as_deref().unwrap_or(DEFAULT_IMAGE_MODEL))?;
    let mut models = vec![preferred.clone()];
    for model in IMAGE_MODEL_FALLBACKS {
        if *model != preferred {
            models.push((*model).to_string());
        }
    }

    let creds = parse_api_key(Some(options.api_key.as_str()))?;
    let headers = antigravity_headers(&creds.token);
    let endpoints = endpoint_candidates();

    let mut last_error = "no endpoint available".to_string();
    for model in &models {
        let body = build_image_generate_request(
            &prompt,
            model,
            &creds.project_id,
            &aspect_ratio,
            &options.input_images,
        )
        .to_string();
        for endpoint in &endpoints {
            if options
                .signal
                .as_ref()
                .is_some_and(CancellationToken::is_cancelled)
            {
                return Err("Request was aborted".to_string());
            }

            let result = request_image_response(
                endpoint,
                headers.clone(),
                body.clone().into_bytes(),
                options.signal.clone(),
                http,
            )
            .await;

            match result {
                Ok((response, streaming)) => {
                    if !response.ok() {
                        let body_error = take_chars(&json_or_text_error(&response.text()), 400);
                        if response.status == 404
                            || [403, 429, 500, 502, 503, 504].contains(&response.status)
                        {
                            last_error = format!("HTTP {}: {}", response.status, body_error);
                            continue;
                        }
                        return Err(format!(
                            "Antigravity image request failed ({}): {}",
                            response.status,
                            safe_error(&body_error)
                        ));
                    }

                    let body_text = response.text();
                    let parsed = if streaming {
                        collect_images_from_sse(&[body_text.as_str()])
                    } else {
                        parse_image_response(&response.json())
                    };
                    let (images, text) = match parsed {
                        Ok(value) => value,
                        Err(error) => {
                            last_error = safe_error(&error);
                            if options
                                .signal
                                .as_ref()
                                .is_some_and(CancellationToken::is_cancelled)
                            {
                                return Err("Request was aborted".to_string());
                            }
                            continue;
                        }
                    };
                    if images.is_empty() {
                        let joined = text.join(" ").trim().to_string();
                        last_error = if joined.is_empty() {
                            "No image data returned.".to_string()
                        } else {
                            joined
                        };
                        continue;
                    }
                    return Ok(GeneratedImageContent {
                        images,
                        text,
                        model: model.clone(),
                    });
                }
                Err(error) => {
                    last_error = safe_error(&error);
                    if options
                        .signal
                        .as_ref()
                        .is_some_and(CancellationToken::is_cancelled)
                    {
                        return Err("Request was aborted".to_string());
                    }
                }
            }
        }
    }

    Err(format!(
        "Antigravity image generation failed: {}",
        safe_error(&last_error)
    ))
}

/// Save generated images for the tool and slash command (`generateAntigravityImage`).
pub async fn generate_antigravity_image(
    options: &GenerateImageOptions,
    http: &dyn AntigravityHttp,
) -> Result<GenerateImageResult, String> {
    let content = generate_antigravity_image_content(options, http).await?;
    let multiple = content.images.len() > 1;
    let mut saved_paths = Vec::new();
    for (index, image) in content.images.iter().enumerate() {
        let path = resolve_image_save_path(
            &options.cwd,
            options.path.as_deref(),
            &image.mime_type,
            if multiple { Some(index) } else { None },
        )?;
        write_image(&path, image)?;
        saved_paths.push(path);
    }
    Ok(GenerateImageResult {
        images: content.images,
        saved_paths,
        text: content.text,
        model: content.model,
    })
}

fn write_image(file_path: &str, image: &GeneratedImage) -> Result<(), String> {
    let path = Path::new(file_path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&image.data)
        .map_err(|error| error.to_string())?;
    std::fs::write(path, bytes).map_err(|error| error.to_string())
}

// ---------------------------------------------------------------------------------------------
// image/native.ts — the in-memory image API
// ---------------------------------------------------------------------------------------------

/// The `api` identifier the native image API serves (`ANTIGRAVITY_IMAGE_API`).
pub const ANTIGRAVITY_IMAGE_API: &str = "antigravity-images";

/// One image model descriptor (`ANTIGRAVITY_IMAGE_MODELS`).
#[derive(Clone, Debug, PartialEq)]
pub struct AntigravityImageModel {
    pub type_: &'static str,
    pub id: &'static str,
    pub name: &'static str,
    pub api: &'static str,
    pub input: &'static [&'static str],
    pub output: &'static [&'static str],
    pub cost_input: f64,
    pub cost_output: f64,
    pub cost_cache_read: f64,
    pub cost_cache_write: f64,
}

/// The single published image model.
pub const ANTIGRAVITY_IMAGE_MODELS: &[AntigravityImageModel] = &[AntigravityImageModel {
    type_: "image",
    id: DEFAULT_IMAGE_MODEL,
    name: "Gemini 3.1 Flash Image (Antigravity)",
    api: ANTIGRAVITY_IMAGE_API,
    input: &["text", "image"],
    output: &["text", "image"],
    cost_input: 0.0,
    cost_output: 0.0,
    cost_cache_read: 0.0,
    cost_cache_write: 0.0,
}];

/// The `{api, provider, model}` identity of an image request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageModelIdentity {
    pub id: String,
    pub api: String,
    pub provider: String,
}

/// One input item for the native image API.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImageInput {
    Text(String),
    Image(GeneratedImage),
}

/// The native image context (`ImagesContext`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImagesContext {
    pub input: Vec<ImageInput>,
}

/// Options for the native image API (`ImagesOptions`); `aspect_ratio` mirrors
/// `metadata.aspectRatio` and may be any JSON value so the "must be a string" guard applies.
#[derive(Clone, Debug, Default)]
pub struct ImagesOptions {
    pub api_key: Option<String>,
    pub aspect_ratio: Option<Value>,
    pub signal: Option<CancellationToken>,
}

/// One output item (`AssistantImages["output"]`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImageOutput {
    Text { text: String },
    Image { data: String, mime_type: String },
}

/// The native image result (`AssistantImages`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssistantImages {
    pub api: String,
    pub provider: String,
    pub model: String,
    pub output: Vec<ImageOutput>,
    pub timestamp: u64,
    pub stop_reason: String,
    pub error_message: Option<String>,
}

async fn native_image_request(
    model: &ImageModelIdentity,
    context: &ImagesContext,
    options: &ImagesOptions,
    http: &dyn AntigravityHttp,
) -> Result<(String, Vec<ImageOutput>), String> {
    let prompt = context
        .input
        .iter()
        .filter_map(|item| match item {
            ImageInput::Text(text) => Some(text.clone()),
            ImageInput::Image(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    let input_images: Vec<GeneratedImage> = context
        .input
        .iter()
        .filter_map(|item| match item {
            ImageInput::Image(image) => Some(image.clone()),
            ImageInput::Text(_) => None,
        })
        .collect();
    let aspect_ratio = options
        .aspect_ratio
        .as_ref()
        .and_then(Value::as_str)
        .map(str::to_string);

    let content = generate_antigravity_image_content(
        &GenerateImageOptions {
            api_key: options.api_key.clone().unwrap_or_default(),
            cwd: String::new(),
            prompt,
            aspect_ratio,
            model: Some(model.id.clone()),
            path: None,
            signal: options.signal.clone(),
            input_images,
        },
        http,
    )
    .await?;

    let mut output = Vec::new();
    for text in &content.text {
        output.push(ImageOutput::Text { text: text.clone() });
    }
    for image in &content.images {
        output.push(ImageOutput::Image {
            data: image.data.clone(),
            mime_type: image.mime_type.clone(),
        });
    }
    Ok((content.model, output))
}

/// Implements the in-memory image API using the existing Antigravity credentials and transport
/// (`generateAntigravityImages`).
pub async fn generate_antigravity_images(
    model: &ImageModelIdentity,
    context: &ImagesContext,
    options: Option<&ImagesOptions>,
    http: &dyn AntigravityHttp,
) -> AssistantImages {
    let aborted = options
        .and_then(|options| options.signal.as_ref())
        .is_some_and(CancellationToken::is_cancelled);

    let outcome = if aborted {
        Err("Request was aborted".to_string())
    } else {
        match options.and_then(|options| {
            options
                .api_key
                .as_deref()
                .filter(|key| !key.is_empty())
                .map(|_| options)
        }) {
            None => Err("No Antigravity credentials. Run /login antigravity first.".to_string()),
            Some(options) => match &options.aspect_ratio {
                Some(ratio) if !ratio.is_string() => {
                    Err("aspectRatio must be a string.".to_string())
                }
                _ => native_image_request(model, context, options, http).await,
            },
        }
    };

    match outcome {
        Ok((model_id, output)) => AssistantImages {
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model_id,
            output,
            timestamp: crate::utils_inner::diagnostics::now_ms(),
            stop_reason: "stop".to_string(),
            error_message: None,
        },
        Err(message) => AssistantImages {
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            output: Vec::new(),
            timestamp: crate::utils_inner::diagnostics::now_ms(),
            stop_reason: if aborted
                || options
                    .and_then(|options| options.signal.as_ref())
                    .is_some_and(CancellationToken::is_cancelled)
            {
                "aborted"
            } else {
                "error"
            }
            .to_string(),
            error_message: Some(safe_error(&message)),
        },
    }
}

// ---------------------------------------------------------------------------------------------
// features/generate-image.ts — the optional model-facing tool
// ---------------------------------------------------------------------------------------------

/// Tool name.
pub const GENERATE_IMAGE_TOOL_NAME: &str = "generate_image";
/// Human-readable label.
pub const GENERATE_IMAGE_TOOL_LABEL: &str = "Generate image";
/// Tool description.
pub const GENERATE_IMAGE_TOOL_DESCRIPTION: &str = "Generate an image via Antigravity using the signed-in Google account. Saves under .pi/generated-images/ unless path is set.";
/// One-line prompt snippet.
pub const GENERATE_IMAGE_TOOL_PROMPT_SNIPPET: &str =
    "Generate images via Antigravity OAuth (Gemini image models)";
/// Extra guidance lines.
pub const GENERATE_IMAGE_TOOL_PROMPT_GUIDELINES: &[&str] =
    &["Use generate_image when the user asks to create, draw, or generate an image."];

/// Whether the `generate_image` tool should be registered (`isExtraToolEnabled("IMAGE")`).
#[must_use]
pub fn generate_image_tool_enabled() -> bool {
    is_extra_tool_enabled("IMAGE")
}

/// JSON Schema for the `generate_image` parameters (`Type.Object` in the TS extension).
#[must_use]
pub fn generate_image_tool_parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "prompt": { "type": "string", "description": "Image description." },
            "aspectRatio": {
                "type": "string",
                "enum": IMAGE_ASPECT_RATIOS,
            },
            "model": {
                "type": "string",
                "description": format!("Image model id. Default: {DEFAULT_IMAGE_MODEL}."),
            },
            "path": {
                "type": "string",
                "description": "Project-relative file or directory to save the image.",
            },
        },
        "required": ["prompt"],
        "additionalProperties": false,
    })
}

// ---------------------------------------------------------------------------------------------
// date helpers (JS `new Date().toISOString()` with `[:.]` replaced by `-`)
// ---------------------------------------------------------------------------------------------

fn iso_timestamp_utc(ms: u64) -> String {
    let seconds = (ms / 1000) as i64;
    let millis = ms % 1000;
    let seconds_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(seconds.div_euclid(86_400));
    let hour = seconds_of_day / 3600;
    let minute = (seconds_of_day % 3600) / 60;
    let second = seconds_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}-{minute:02}-{second:02}-{millis:03}Z")
}

/// Howard Hinnant's `civil_from_days`: days since 1970-01-01 to (year, month, day).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    (year, month as u32, day as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// A scripted transport: records calls/bodies and serves a queue of responses.
    struct ScriptedHttp {
        calls: Mutex<Vec<String>>,
        bodies: Mutex<Vec<Vec<u8>>>,
        responses: Mutex<VecDeque<Result<AntigravityHttpResponse, String>>>,
    }

    impl ScriptedHttp {
        fn new(responses: Vec<Result<AntigravityHttpResponse, String>>) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                bodies: Mutex::new(Vec::new()),
                responses: Mutex::new(responses.into_iter().collect()),
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

        fn bodies(&self) -> Vec<Vec<u8>> {
            self.bodies.lock().unwrap().clone()
        }
    }

    impl AntigravityHttp for ScriptedHttp {
        fn post<'a>(
            &'a self,
            url: &'a str,
            _headers: Vec<(String, String)>,
            body: Vec<u8>,
            _signal: Option<CancellationToken>,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<AntigravityHttpResponse, String>>
                    + Send
                    + 'a,
            >,
        > {
            self.calls.lock().unwrap().push(url.to_string());
            self.bodies.lock().unwrap().push(body);
            let response = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Err("no scripted response".to_string()));
            Box::pin(async move { response })
        }
    }

    fn sse(payload: &Value) -> String {
        format!("data: {payload}\n")
    }

    #[test]
    fn parses_image_command_args() {
        let parsed = parse_image_command_args(
            "--ratio 16:9 --model gemini-3-pro-image a sunset over mountains",
        );
        assert_eq!(parsed.prompt, "a sunset over mountains");
        assert_eq!(parsed.aspect_ratio.as_deref(), Some("16:9"));
        assert_eq!(parsed.model.as_deref(), Some("gemini-3-pro-image"));

        let with_path = parse_image_command_args("--path out/cat.png --ratio 1:1 a cat");
        assert_eq!(with_path.path.as_deref(), Some("out/cat.png"));
        assert_eq!(with_path.prompt, "a cat");
        assert_eq!(parse_image_command_args("").prompt, "");
    }

    #[test]
    fn guards_image_model_and_aspect_ratio() {
        assert_eq!(
            assert_safe_image_model("gemini-3-pro-image").unwrap(),
            "gemini-3-pro-image"
        );
        assert_eq!(
            assert_safe_image_model("imagen-3.0-generate-002").unwrap(),
            "imagen-3.0-generate-002"
        );
        assert!(assert_safe_image_model("claude-opus-4-6")
            .unwrap_err()
            .contains("Unsupported image model"));
        assert!(assert_safe_image_model("https://evil.example/x").is_err());

        assert_eq!(assert_safe_aspect_ratio("16:9").unwrap(), "16:9");
        assert!(assert_safe_aspect_ratio("99:1")
            .unwrap_err()
            .contains("Unsupported aspect ratio"));
    }

    #[test]
    fn builds_the_image_request() {
        let request = build_image_generate_request(
            "a lighthouse",
            "gemini-3-pro-image",
            "proj-1",
            "16:9",
            &[],
        );
        assert_eq!(request["model"], "gemini-3-pro-image");
        assert_eq!(request["project"], "proj-1");
        assert_eq!(
            request["request"]["generationConfig"]["imageConfig"]["aspectRatio"],
            "16:9"
        );
        assert_eq!(
            request["request"]["contents"][0]["parts"][0]["text"],
            "a lighthouse"
        );
        assert_eq!(DEFAULT_IMAGE_MODEL, "gemini-3.1-flash-image");
        assert_eq!(request["requestType"], "image_gen");
        assert!(request["request"].get("systemInstruction").is_none());
        let request_id = request["requestId"].as_str().unwrap();
        let pattern = regex::Regex::new(
            r"^image_gen/\d+/[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}/1$",
        )
        .unwrap();
        assert!(pattern.is_match(request_id), "request id {request_id}");
    }

    #[tokio::test]
    async fn only_a_404_enables_the_sse_endpoint() {
        for status in [200_u16, 400, 403, 429, 500, 503] {
            let http = ScriptedHttp::new(vec![ScriptedHttp::status(status, "note")]);
            let (response, streaming) =
                request_image_response("https://example.test", vec![], b"{}".to_vec(), None, &http)
                    .await
                    .unwrap();
            assert!(!streaming);
            assert_eq!(response.status, status);
            assert_eq!(
                http.calls(),
                vec!["https://example.test/v1internal:generateContent"]
            );
        }

        let http = ScriptedHttp::new(vec![
            ScriptedHttp::status(404, "not found"),
            ScriptedHttp::status(200, "data: [DONE]\n"),
        ]);
        let (_response, streaming) =
            request_image_response("https://example.test", vec![], b"{}".to_vec(), None, &http)
                .await
                .unwrap();
        assert!(streaming);
        assert_eq!(
            http.calls(),
            vec![
                "https://example.test/v1internal:generateContent",
                "https://example.test/v1internal:streamGenerateContent?alt=sse",
            ]
        );

        let http = ScriptedHttp::new(vec![Err("aborted".to_string())]);
        let error =
            request_image_response("https://example.test", vec![], b"{}".to_vec(), None, &http)
                .await
                .unwrap_err();
        assert!(error.contains("aborted"));
    }

    #[test]
    fn parses_direct_and_wrapped_image_responses() {
        let candidate = json!({
            "content": { "parts": [
                { "text": "note" },
                { "inlineData": { "mimeType": "image/jpeg", "data": "AQID" } },
            ]}
        });
        let expected = (
            vec![GeneratedImage {
                data: "AQID".to_string(),
                mime_type: "image/jpeg".to_string(),
            }],
            vec!["note".to_string()],
        );
        assert_eq!(
            parse_image_response(&json!({ "candidates": [candidate] })).unwrap(),
            expected
        );
        assert_eq!(
            parse_image_response(&json!({ "response": { "candidates": [candidate] } })).unwrap(),
            expected
        );
        assert_eq!(
            parse_image_response(
                &json!({ "candidates": [{ "content": { "parts": [{ "text": "no image" }] } }] })
            )
            .unwrap(),
            (Vec::new(), vec!["no image".to_string()])
        );
    }

    #[test]
    fn parses_sse_split_across_chunks() {
        let png = base64::engine::general_purpose::STANDARD.encode([0x89, 0x50, 0x4e, 0x47]);
        let body = format!(
            "{}{}{}",
            sse(
                &json!({ "response": { "candidates": [{ "content": { "parts": [{ "text": "ok" }] } }] } })
            ),
            sse(
                &json!({ "response": { "candidates": [{ "content": { "parts": [{ "inlineData": { "mimeType": "image/png", "data": png } }] } }] } })
            ),
            "data: [DONE]\n",
        );
        let bytes = body.as_bytes();
        let first = String::from_utf8_lossy(&bytes[..40.min(bytes.len())]).into_owned();
        let second = String::from_utf8_lossy(&bytes[40.min(bytes.len())..]).into_owned();
        let (images, text) = collect_images_from_sse(&[first.as_str(), second.as_str()]).unwrap();
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].mime_type, "image/png");
        assert_eq!(images[0].data, png);
        assert_eq!(text.join(""), "ok");
    }

    #[test]
    fn resolves_save_paths_and_rejects_traversal() {
        let tmp = std::env::temp_dir().join(format!("pi-antigravity-image-{}", random_uuid_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let root = tmp.to_string_lossy().to_string();

        let saved = resolve_image_save_path(&root, Some("out/cat.png"), "image/png", None).unwrap();
        assert_eq!(saved, tmp.join("out/cat.png").to_string_lossy().to_string());

        let dir_saved =
            resolve_image_save_path(&root, Some("images"), "image/jpeg", Some(0)).unwrap();
        assert!(dir_saved.ends_with("-1.jpg"), "dir save {dir_saved}");
        let images_dir = tmp.join("images").to_string_lossy().to_string();
        assert!(dir_saved.starts_with(&images_dir));

        let default_multi = resolve_image_save_path(&root, None, "image/png", Some(0)).unwrap();
        assert!(
            default_multi.ends_with("-1.png"),
            "default multi {default_multi}"
        );
        assert!(
            !default_multi.ends_with("-1-1.png"),
            "must not double suffix: {default_multi}"
        );
        let default_multi_2 = resolve_image_save_path(&root, None, "image/png", Some(1)).unwrap();
        assert!(
            default_multi_2.ends_with("-2.png"),
            "default multi 2 {default_multi_2}"
        );

        assert!(
            resolve_image_save_path(&root, Some("../escape.png"), "image/png", None)
                .unwrap_err()
                .contains("inside the working directory")
        );

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn sse_parser_finish_handles_unterminated_trailing_chunk() {
        let png = base64::engine::general_purpose::STANDARD.encode([0x89, 0x50, 0x4e, 0x47]);
        let payload = format!(
            "data: {}",
            json!({
                "response": {
                    "candidates": [{
                        "content": {
                            "parts": [{
                                "inlineData": {
                                    "data": png,
                                    "mimeType": "image/png"
                                }
                            }]
                        }
                    }]
                }
            })
        );
        let (images, _text) = collect_images_from_sse(&[&payload]).unwrap();
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].data, png);
    }

    #[tokio::test]
    async fn request_fails_fast_on_client_error_and_retries_transient() {
        let http_400 = ScriptedHttp::new(vec![ScriptedHttp::status(400, "bad prompt argument")]);
        let result_400 = generate_antigravity_image_content(
            &GenerateImageOptions {
                api_key: r#"{"token":"tok","projectId":"proj"}"#.to_string(),
                prompt: "a cat".to_string(),
                ..GenerateImageOptions::default()
            },
            &http_400,
        )
        .await;
        assert!(result_400.is_err());
        let err = result_400.unwrap_err();
        assert!(err.contains("Antigravity image request failed (400)"));
        assert_eq!(http_400.calls().len(), 1);

        // Every model x endpoint attempt retries transiently; script enough 429s to
        // cover the full matrix so the final error keeps the HTTP status.
        let http_429 = ScriptedHttp::new(
            std::iter::repeat_with(|| ScriptedHttp::status(429, "quota exceeded"))
                .take(12)
                .collect(),
        );
        let result_429 = generate_antigravity_image_content(
            &GenerateImageOptions {
                api_key: r#"{"token":"tok","projectId":"proj"}"#.to_string(),
                prompt: "a cat".to_string(),
                ..GenerateImageOptions::default()
            },
            &http_429,
        )
        .await;
        assert!(result_429.is_err());
        let err_429 = result_429.unwrap_err();
        assert!(
            err_429.contains("HTTP 429"),
            "expected HTTP 429 in error: {err_429}"
        );
    }

    #[tokio::test]
    async fn native_images_parse_inputs_and_errors() {
        let model = ImageModelIdentity {
            id: DEFAULT_IMAGE_MODEL.to_string(),
            api: ANTIGRAVITY_IMAGE_API.to_string(),
            provider: "antigravity".to_string(),
        };
        let context = ImagesContext {
            input: vec![ImageInput::Text("a square".to_string())],
        };

        let no_key =
            generate_antigravity_images(&model, &context, None, &ScriptedHttp::new(vec![])).await;
        assert_eq!(no_key.stop_reason, "error");

        let bad_key = generate_antigravity_images(
            &model,
            &context,
            Some(&ImagesOptions {
                api_key: Some("bad".to_string()),
                ..ImagesOptions::default()
            }),
            &ScriptedHttp::new(vec![]),
        )
        .await;
        assert!(bad_key
            .error_message
            .unwrap_or_default()
            .contains("Invalid Antigravity credentials"));

        let abort = CancellationToken::new();
        abort.cancel();
        let aborted = generate_antigravity_images(
            &model,
            &context,
            Some(&ImagesOptions {
                api_key: Some("bad".to_string()),
                signal: Some(abort),
                ..ImagesOptions::default()
            }),
            &ScriptedHttp::new(vec![]),
        )
        .await;
        assert_eq!(aborted.stop_reason, "aborted");

        let http = ScriptedHttp::new(vec![ScriptedHttp::json(
            &json!({ "response": { "candidates": [{ "content": { "parts": [
                { "text": "done" },
                { "inlineData": { "data": "AQID", "mimeType": "image/jpeg" } },
            ] } }] } })
            .to_string(),
        )]);
        let result = generate_antigravity_images(
            &model,
            &ImagesContext {
                input: vec![
                    ImageInput::Text("a square".to_string()),
                    ImageInput::Image(GeneratedImage {
                        data: "AQID".to_string(),
                        mime_type: "image/png".to_string(),
                    }),
                ],
            },
            Some(&ImagesOptions {
                api_key: Some(r#"{"token":"t","projectId":"p"}"#.to_string()),
                aspect_ratio: Some(json!("16:9")),
                signal: None,
            }),
            &http,
        )
        .await;
        assert_eq!(result.stop_reason, "stop");
        assert_eq!(result.model, DEFAULT_IMAGE_MODEL);
        assert_eq!(
            result.output,
            vec![
                ImageOutput::Text {
                    text: "done".to_string()
                },
                ImageOutput::Image {
                    data: "AQID".to_string(),
                    mime_type: "image/jpeg".to_string(),
                },
            ]
        );
        assert_eq!(http.calls().len(), 1);

        let body: Value = serde_json::from_slice(&http.bodies()[0]).unwrap();
        assert_eq!(body["model"], DEFAULT_IMAGE_MODEL);
        assert_eq!(
            body["request"]["generationConfig"]["imageConfig"]["aspectRatio"],
            "16:9"
        );
        assert_eq!(
            body["request"]["contents"][0]["parts"][1],
            json!({ "inlineData": { "data": "AQID", "mimeType": "image/png" } })
        );

        let invalid = generate_antigravity_images(
            &model,
            &context,
            Some(&ImagesOptions {
                api_key: Some(r#"{"token":"t","projectId":"p"}"#.to_string()),
                aspect_ratio: Some(json!("99:1")),
                signal: None,
            }),
            &ScriptedHttp::new(vec![]),
        )
        .await;
        assert_eq!(invalid.stop_reason, "error");

        let empty = generate_antigravity_images(
            &model,
            &ImagesContext { input: Vec::new() },
            Some(&ImagesOptions {
                api_key: Some(r#"{"token":"t","projectId":"p"}"#.to_string()),
                ..ImagesOptions::default()
            }),
            &ScriptedHttp::new(vec![]),
        )
        .await;
        assert_eq!(empty.stop_reason, "error");
    }

    #[test]
    fn tool_spec_is_stable() {
        let parameters = generate_image_tool_parameters();
        let mut keys: Vec<&str> = parameters["properties"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["aspectRatio", "model", "path", "prompt"]);
        assert_eq!(ANTIGRAVITY_IMAGE_MODELS.len(), 1);
        assert_eq!(ANTIGRAVITY_IMAGE_MODELS[0].id, DEFAULT_IMAGE_MODEL);
    }
}
