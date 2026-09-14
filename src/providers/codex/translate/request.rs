use std::collections::HashSet;

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::anthropic::schema::MessagesRequest;
use crate::config;
use crate::providers::translate_shared::{
    ContentBlock, flatten_system_text, image_source_to_url, normalize_content, parallel_tool_calls,
    read_effort, wrap_reasoning,
};

use super::read_rewrite::{ReadOffsetRewrite, read_offset_rewrite};
use super::reasoning_signature::decode_reasoning_signature;
use super::tool_search::{
    self, CLAUDE_CODE_TOOL_SEARCH_NAME, TOOL_SEARCH_EXECUTION, TOOL_SEARCH_STATUS, ToolSearchPlan,
};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Effort {
    None,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl std::fmt::Display for Effort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Effort::None => write!(f, "none"),
            Effort::Low => write!(f, "low"),
            Effort::Medium => write!(f, "medium"),
            Effort::High => write!(f, "high"),
            Effort::Xhigh => write!(f, "xhigh"),
            Effort::Max => write!(f, "max"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ServiceTier {
    Priority,
    Flex,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponsesToolChoiceMode {
    Auto,
    None,
    Required,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponsesToolChoice {
    Mode(ResponsesToolChoiceMode),
    Function {
        r#type: String,
        name: String,
    },
    WebSearch {
        r#type: String,
    },
    AllowedTools {
        r#type: String,
        mode: String,
        tools: Vec<Value>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesRequest {
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    pub input: Vec<ResponsesInputItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ResponsesTool>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ResponsesToolChoice>,
    pub store: bool,
    pub stream: bool,
    pub parallel_tool_calls: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_metadata: Option<std::collections::HashMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<ServiceTier>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
    pub text: ResponsesText,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ResponsesReasoning>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesReasoning {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesText {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verbosity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<ResponsesTextFormat>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
pub enum ResponsesTextFormat {
    Text,
    JsonObject,
    JsonSchema {
        name: String,
        schema: Value,
        #[serde(default)]
        strict: Option<bool>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ResponsesInputItem {
    #[serde(rename = "additional_tools")]
    AdditionalTools {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        role: String,
        tools: Vec<Value>,
    },
    #[serde(rename = "message")]
    Message {
        role: String,
        content: Vec<ResponsesContentPart>,
    },
    #[serde(rename = "function_call")]
    FunctionCall {
        #[serde(default)]
        call_id: String,
        name: String,
        arguments: String,
    },
    #[serde(rename = "function_call_output")]
    FunctionCallOutput {
        #[serde(default)]
        call_id: String,
        output: ResponsesFunctionCallOutput,
    },
    /// A replayed search for deferred tools (Claude Code's `ToolSearch` call).
    #[serde(rename = "tool_search_call")]
    ToolSearchCall {
        #[serde(default)]
        call_id: String,
        execution: String,
        status: String,
        arguments: Value,
    },
    /// The tools a search loaded, carried at the point of the search so the
    /// tools head stays unchanged.
    #[serde(rename = "tool_search_output")]
    ToolSearchOutput {
        #[serde(default)]
        call_id: String,
        status: String,
        execution: String,
        tools: Vec<Value>,
    },
    #[serde(rename = "reasoning")]
    Reasoning {
        id: String,
        summary: Vec<Value>,
        encrypted_content: String,
    },
    #[serde(rename = "compaction")]
    Compaction { encrypted_content: String },
    #[serde(rename = "compaction_trigger")]
    CompactionTrigger,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponsesFunctionCallOutput {
    Text(String),
    ContentItems(Vec<ResponsesFunctionCallOutputContentPart>),
}

impl ResponsesFunctionCallOutput {
    #[cfg(test)]
    fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(text) => Some(text),
            Self::ContentItems(_) => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponsesFunctionCallOutputContentPart {
    InputText {
        text: String,
    },
    InputImage {
        image_url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ResponsesContentPart {
    #[serde(rename = "input_text")]
    InputText { text: String },
    #[serde(rename = "output_text")]
    OutputText { text: String },
    #[serde(rename = "input_image")]
    InputImage {
        image_url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponsesTool {
    Function(ResponsesFunctionTool),
    WebSearch(ResponsesWebSearchTool),
    ToolSearch(ResponsesToolSearchTool),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesFunctionTool {
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub parameters: Value,
    #[serde(default)]
    pub strict: bool,
    /// Set only on tools delivered by a `tool_search_output` item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defer_loading: Option<bool>,
}

/// The backend's native deferred-tool search, executed by the client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesToolSearchTool {
    #[serde(rename = "type")]
    pub kind: String,
    pub execution: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub parameters: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesWebSearchTool {
    #[serde(rename = "type")]
    pub kind: String,
    pub external_web_access: bool,
    pub search_content_types: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filters: Option<ResponsesWebSearchFilters>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesWebSearchFilters {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_domains: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_domains: Option<Vec<String>>,
}

pub struct TranslateOptions {
    /// What the backend keys its prompt cache on: the conversation's cache
    /// scope (`ConversationIdentity::cache_scope`), which is the session id for
    /// a main thread and a derived id for a subagent. It becomes
    /// `prompt_cache_key` and the routing headers of the upstream request.
    pub session_id: Option<String>,
    pub service_tier: Option<ServiceTier>,
    pub model: String,
    pub use_responses_lite: bool,
}

// ---------------------------------------------------------------------------
// Translation entry point
// ---------------------------------------------------------------------------

pub(crate) fn to_codex_effort(effort: Option<&str>) -> Option<Effort> {
    match effort {
        Some("max") => Some(Effort::Max),
        Some("xhigh") => Some(Effort::Xhigh),
        Some("low") => Some(Effort::Low),
        Some("medium") => Some(Effort::Medium),
        Some("high") => Some(Effort::High),
        _ => None,
    }
}

fn resolve_effort(effort: Option<Effort>) -> Result<Option<Effort>, anyhow::Error> {
    resolve_effort_override(effort, config::codex_effort().as_deref())
}

pub(crate) fn resolve_effort_override(
    effort: Option<Effort>,
    override_effort: Option<&str>,
) -> Result<Option<Effort>, anyhow::Error> {
    if let Some(val) = override_effort {
        let valid = ["none", "low", "medium", "high", "xhigh", "max"];
        if !valid.contains(&val) {
            anyhow::bail!(
                "Invalid effort override: \"{val}\". Must be one of: none, low, medium, high, xhigh, max"
            );
        }
        return Ok(Some(match val {
            "max" => Effort::Max,
            "xhigh" => Effort::Xhigh,
            "high" => Effort::High,
            "medium" => Effort::Medium,
            "low" => Effort::Low,
            _ => Effort::None,
        }));
    }
    Ok(effort)
}

fn reasoning_summary_requested(summary: Option<&str>) -> bool {
    !matches!(summary, Some("off" | "none"))
}

// ---------------------------------------------------------------------------
// Compaction fast path
// ---------------------------------------------------------------------------

const COMPACT_SYSTEM_MARKER: &str =
    "You are a helpful AI assistant tasked with summarizing conversations";
const COMPACT_MESSAGE_PREFIX: &str = "CRITICAL: Respond with TEXT ONLY. Do NOT call any tools.";
const COMPACT_MESSAGE_TASK: &str =
    "Your task is to create a detailed summary of the conversation so far";

pub(crate) fn is_compact_request(instructions: Option<&str>) -> bool {
    instructions.is_some_and(|text| text.contains(COMPACT_SYSTEM_MARKER))
}

pub(crate) fn is_compact_message_text(text: &str) -> bool {
    text.contains(COMPACT_MESSAGE_PREFIX) && text.contains(COMPACT_MESSAGE_TASK)
}

fn is_compact_message_content(content: &Value) -> bool {
    match content {
        Value::String(text) => is_compact_message_text(text),
        Value::Array(blocks) => blocks.iter().any(|block| {
            block.get("type").and_then(Value::as_str) == Some("text")
                && block
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(is_compact_message_text)
        }),
        _ => false,
    }
}

pub(crate) fn is_compact_messages_request(request: &MessagesRequest) -> bool {
    is_compact_request(flatten_system_text(request.extra.get("system")).as_deref())
        || request.messages.last().is_some_and(|message| {
            message.role == "user" && is_compact_message_content(&message.content)
        })
}

/// Reasoning-effort cap applied to compaction requests, or None when the
/// fast path is disabled. Summarization is extraction, not problem solving:
/// native Claude Code compacts without extended thinking, so burning
/// medium/high reasoning on a 200k-token summary only adds latency. The cap
/// never raises effort — a request already below it is left alone.
fn compact_effort_cap() -> Option<Effort> {
    compact_effort_cap_from(std::env::var("CCP_COMPACT_EFFORT").ok().as_deref())
}

fn compact_effort_cap_from(raw: Option<&str>) -> Option<Effort> {
    match raw {
        None | Some("") => Some(Effort::Low),
        Some("off") => None,
        Some("none") => Some(Effort::None),
        Some(other) => to_codex_effort(Some(other)).or(Some(Effort::Low)),
    }
}

const VALID_SERVICE_TIERS: &[&str] = &["fast", "priority", "flex"];

fn normalize_service_tier(tier: &str) -> Result<ServiceTier, anyhow::Error> {
    if !VALID_SERVICE_TIERS.contains(&tier) {
        anyhow::bail!(
            "Invalid service tier override: \"{tier}\". Must be one of: {}",
            VALID_SERVICE_TIERS.join(", ")
        );
    }
    match tier {
        "flex" => Ok(ServiceTier::Flex),
        _ => Ok(ServiceTier::Priority),
    }
}

fn resolve_service_tier(
    model_tier: Option<ServiceTier>,
) -> Result<Option<ServiceTier>, anyhow::Error> {
    let tier = config::codex_service_tier();
    match tier {
        Some(ref val) => Ok(Some(normalize_service_tier(val)?)),
        None => Ok(model_tier),
    }
}

pub fn normalize_strict_json_schema(schema: &Value) -> Value {
    match schema {
        Value::Array(arr) => Value::Array(arr.iter().map(normalize_strict_json_schema).collect()),
        Value::Object(map) => {
            let mut out = map.clone();
            if let Some(properties) = out.get("properties").and_then(|v| v.as_object()) {
                let keys: Vec<String> = properties.keys().cloned().collect();
                out.insert(
                    "required".into(),
                    Value::Array(keys.into_iter().map(Value::String).collect()),
                );
            }
            for (key, val) in out.clone().iter() {
                out.insert(key.clone(), normalize_strict_json_schema(val));
            }
            Value::Object(out)
        }
        _ => schema.clone(),
    }
}

/// Hosted tools (web_search) are rejected by the Responses Lite lane, which
/// only supports function and custom tools. Requests carrying them must use
/// the full Responses API.
pub fn has_hosted_web_search(req: &MessagesRequest) -> bool {
    req.extra
        .get("tools")
        .and_then(|v| v.as_array())
        .is_some_and(|tools| {
            tools.iter().any(|tool| {
                tool.get("type").and_then(|v| v.as_str()) == Some("web_search_20250305")
            })
        })
}

pub fn translate_request(
    req: &MessagesRequest,
    opts: TranslateOptions,
) -> Result<ResponsesRequest, anyhow::Error> {
    translate_request_inner(req, opts, true)
}

pub fn translate_openai_compatible_request(
    req: &MessagesRequest,
    model: String,
    session_id: Option<String>,
) -> Result<ResponsesRequest, anyhow::Error> {
    translate_request_inner(
        req,
        TranslateOptions {
            session_id,
            service_tier: None,
            model,
            use_responses_lite: false,
        },
        false,
    )
}

fn translate_request_inner(
    req: &MessagesRequest,
    opts: TranslateOptions,
    apply_codex_config: bool,
) -> Result<ResponsesRequest, anyhow::Error> {
    let instructions = flatten_system_text(req.extra.get("system"));
    let is_compact = is_compact_messages_request(req);
    let tool_search_plan = tool_search::plan(req);
    let input = build_input(req, tool_search_plan.as_ref());
    let tools = read_tools(req, tool_search_plan.as_ref())?;
    let tool_choice = map_tool_choice(req)?;
    let parallel_tool_calls = parallel_tool_calls(req).unwrap_or(true);

    let mut text = ResponsesText {
        verbosity: Some("low".to_string()),
        format: None,
    };

    if let Some(fmt) = read_output_format(req) {
        text.format = Some(fmt);
    }

    let mut out = ResponsesRequest {
        model: opts.model,
        instructions,
        input,
        store: false,
        stream: true,
        parallel_tool_calls,
        tool_choice,
        text,
        tools: None,
        include: None,
        client_metadata: None,
        service_tier: None,
        prompt_cache_key: None,
        reasoning: None,
    };

    if opts.use_responses_lite {
        out.client_metadata = Some(std::collections::HashMap::from([(
            "ws_request_header_x_openai_internal_codex_responses_lite".to_string(),
            "true".to_string(),
        )]));
        // The lite lane hard-requires this: the backend answers a lite request
        // that sets it to `true` with 400 `unsupported_value`
        // ("X-OpenAI-Internal-Codex-Responses-Lite requires
        // `parallel_tool_calls` to be false"). Escaping the restriction means
        // leaving the lane, which is what `codex.fullLane` does.
        out.parallel_tool_calls = false;

        let mut prefix = Vec::new();
        if let Some(ref tools) = tools
            && !tools.is_empty()
        {
            let tools = tools
                .iter()
                .map(serde_json::to_value)
                .collect::<Result<Vec<_>, _>>()?;
            prefix.push(ResponsesInputItem::AdditionalTools {
                id: None,
                role: "developer".to_string(),
                tools,
            });
        }
        if let Some(instructions) = out.instructions.take()
            && !instructions.is_empty()
        {
            prefix.push(ResponsesInputItem::Message {
                role: "developer".to_string(),
                content: vec![ResponsesContentPart::InputText { text: instructions }],
            });
        }
        if !prefix.is_empty() {
            prefix.extend(out.input);
            out.input = prefix;
        }
    } else if let Some(tools) = tools
        && !tools.is_empty()
    {
        out.tools = Some(tools);
    }

    // Never force a web_search tool_choice the request didn't register —
    // upstream 502s instead of ignoring it.
    if matches!(
        out.tool_choice,
        Some(ResponsesToolChoice::WebSearch { .. } | ResponsesToolChoice::AllowedTools { .. })
    ) {
        let has_web_search = out.tools.as_ref().is_some_and(|t| {
            t.iter()
                .any(|tool| matches!(tool, ResponsesTool::WebSearch(_)))
        });
        if !has_web_search {
            out.tool_choice = Some(ResponsesToolChoice::Mode(ResponsesToolChoiceMode::Auto));
        }
    }

    if let Some(sid) = opts.session_id {
        out.prompt_cache_key = Some(sid);
    }

    if apply_codex_config {
        let service_tier = resolve_service_tier(opts.service_tier)?;
        if let Some(ref tier) = service_tier {
            out.service_tier = Some(tier.clone());
        }
    }

    let effort = read_effort(req)?;
    let codex_effort = to_codex_effort(effort);
    let mut resolved_effort = if apply_codex_config {
        resolve_effort(codex_effort)?
    } else {
        codex_effort
    };
    if apply_codex_config
        && is_compact
        && let Some(cap) = compact_effort_cap()
        && resolved_effort.as_ref().is_some_and(|e| *e > cap)
    {
        resolved_effort = Some(cap);
    }
    if resolved_effort.is_some() || opts.use_responses_lite {
        let summary = if resolved_effort.is_some()
            && (!apply_codex_config
                || reasoning_summary_requested(config::codex_reasoning_summary().as_deref()))
        {
            Some("auto".to_string())
        } else {
            None
        };
        out.reasoning = Some(ResponsesReasoning {
            effort: resolved_effort.clone(),
            summary,
            context: opts.use_responses_lite.then_some("all_turns".to_string()),
        });
    }
    if resolved_effort.is_some() {
        out.include = Some(vec!["reasoning.encrypted_content".to_string()]);
    }

    Ok(out)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn read_output_format(req: &MessagesRequest) -> Option<ResponsesTextFormat> {
    let output_config = req.extra.get("output_config")?.as_object()?;
    let format = output_config.get("format")?.as_object()?;
    let kind = format.get("type")?.as_str()?;
    match kind {
        "json_schema" => {
            let name = format
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("response")
                .to_string();
            let schema = format.get("schema")?;
            let normalized = normalize_strict_json_schema(schema);
            Some(ResponsesTextFormat::JsonSchema {
                name,
                schema: normalized,
                strict: Some(true),
            })
        }
        "json_object" => Some(ResponsesTextFormat::JsonObject),
        _ => Some(ResponsesTextFormat::Text),
    }
}

fn read_tools(
    req: &MessagesRequest,
    tool_search_plan: Option<&ToolSearchPlan>,
) -> Result<Option<Vec<ResponsesTool>>, anyhow::Error> {
    let Some(tools) = req.extra.get("tools") else {
        return Ok(None);
    };
    let tools_arr = match tools {
        Value::Array(a) => a,
        _ => return Ok(None),
    };
    let mut out = Vec::new();
    for tool in tools_arr {
        if let Some(plan) = tool_search_plan {
            if tool_search::is_claude_code_tool_search(tool) {
                out.push(ResponsesTool::ToolSearch(tool_search_spec(tool)));
                continue;
            }
            if !plan.keeps_in_head(tool) {
                continue;
            }
        }
        let tool_type = tool
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("function");
        if tool_type == "web_search_20250305" {
            let mut filters = ResponsesWebSearchFilters {
                allowed_domains: None,
                blocked_domains: None,
            };
            let allowed = tool.get("allowed_domains").and_then(|v| v.as_array());
            if allowed.is_some_and(|a| !a.is_empty()) {
                filters.allowed_domains = allowed.map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                });
            }
            let blocked = tool.get("blocked_domains").and_then(|v| v.as_array());
            if blocked.is_some_and(|a| !a.is_empty()) {
                filters.blocked_domains = blocked.map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                });
            }
            let has_filters =
                filters.allowed_domains.is_some() || filters.blocked_domains.is_some();
            out.push(ResponsesTool::WebSearch(ResponsesWebSearchTool {
                kind: "web_search".to_string(),
                external_web_access: true,
                search_content_types: vec!["text".to_string(), "image".to_string()],
                filters: if has_filters { Some(filters) } else { None },
            }));
        } else {
            out.push(ResponsesTool::Function(function_tool(tool)));
        }
    }
    if out.is_empty() {
        Ok(None)
    } else {
        Ok(Some(out))
    }
}

fn function_tool(tool: &Value) -> ResponsesFunctionTool {
    let name = tool
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let description = tool
        .get("description")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let parameters = tool
        .get("input_schema")
        .cloned()
        .unwrap_or(serde_json::json!({}));
    let description = codex_tool_description(&name, description);
    let parameters = codex_tool_parameters(&name, parameters);
    ResponsesFunctionTool {
        kind: "function".to_string(),
        name,
        description,
        parameters,
        strict: false,
        defer_loading: None,
    }
}

/// Claude Code's `ToolSearch` function as the backend's client-executed
/// `tool_search`: same description, same parameters, so the model's
/// arguments are exactly what `ToolSearch` accepts.
fn tool_search_spec(tool: &Value) -> ResponsesToolSearchTool {
    ResponsesToolSearchTool {
        kind: "tool_search".to_string(),
        execution: TOOL_SEARCH_EXECUTION.to_string(),
        description: tool
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string),
        parameters: tool
            .get("input_schema")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({"type": "object"})),
    }
}

/// A deferred tool as delivered inside `tool_search_output`.
fn loaded_tool_spec(tool: &Value) -> Value {
    let mut spec = function_tool(tool);
    spec.defer_loading = Some(true);
    serde_json::to_value(spec).unwrap_or(Value::Null)
}

fn codex_tool_description(name: &str, description: Option<String>) -> Option<String> {
    if name != "Read" {
        return description;
    }

    let base = description.unwrap_or_else(|| "Reads a file from the local filesystem.".to_string());
    Some(format!("{base}\n\n{}", read_offset_guidance()))
}

fn codex_tool_parameters(name: &str, mut parameters: Value) -> Value {
    if name != "Read" {
        return parameters;
    }

    let Some(props) = parameters
        .get_mut("properties")
        .and_then(Value::as_object_mut)
    else {
        return parameters;
    };

    if let Some(offset) = props.get_mut("offset").and_then(Value::as_object_mut) {
        offset.insert(
            "description".to_string(),
            Value::String(
                "Optional continuation index. Use only after a prior Read of the same file returned content and more lines are needed. Compute as prior offset plus returned line count. Displayed line numbers, grep line numbers, byte counts, token counts, file sizes, and guessed positions are invalid offsets. Omit when unsure.".to_string(),
            ),
        );
    }

    if let Some(limit) = props.get_mut("limit").and_then(Value::as_object_mut) {
        limit.insert(
            "description".to_string(),
            Value::String(
                "Optional number of lines to read. Omit when opening a file. Use with offset only when continuing a large file."
                    .to_string(),
            ),
        );
    }

    parameters
}

fn map_tool_choice(req: &MessagesRequest) -> Result<Option<ResponsesToolChoice>, anyhow::Error> {
    let choice = match req.extra.get("tool_choice") {
        Some(Value::Object(m)) => m,
        Some(Value::String(s)) => {
            return Ok(Some(match s.as_str() {
                "auto" => ResponsesToolChoice::Mode(ResponsesToolChoiceMode::Auto),
                "none" => ResponsesToolChoice::Mode(ResponsesToolChoiceMode::None),
                "any" | "required" => ResponsesToolChoice::Mode(ResponsesToolChoiceMode::Required),
                _ => ResponsesToolChoice::Mode(ResponsesToolChoiceMode::Auto),
            }));
        }
        _ => return Ok(None),
    };

    let choice_type = choice
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("auto");
    match choice_type {
        "auto" => Ok(Some(ResponsesToolChoice::Mode(
            ResponsesToolChoiceMode::Auto,
        ))),
        "none" => Ok(Some(ResponsesToolChoice::Mode(
            ResponsesToolChoiceMode::None,
        ))),
        "any" | "required" => Ok(Some(ResponsesToolChoice::Mode(
            ResponsesToolChoiceMode::Required,
        ))),
        "tool" => {
            let name = choice.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let tools = req.extra.get("tools").and_then(|v| v.as_array());
            let is_web_search = tools.is_some_and(|t| {
                t.iter().any(|tool| {
                    (tool.get("type").and_then(|v| v.as_str()) == Some("web_search_20250305"))
                        && tool.get("name").and_then(|v| v.as_str()) == Some(name)
                })
            });
            if is_web_search {
                Ok(Some(ResponsesToolChoice::AllowedTools {
                    r#type: "allowed_tools".to_string(),
                    mode: "required".to_string(),
                    tools: vec![serde_json::json!({"type": "web_search"})],
                }))
            } else {
                Ok(Some(ResponsesToolChoice::Function {
                    r#type: "function".to_string(),
                    name: name.to_string(),
                }))
            }
        }
        _ => Ok(None),
    }
}

fn build_input(
    req: &MessagesRequest,
    tool_search_plan: Option<&ToolSearchPlan>,
) -> Vec<ResponsesInputItem> {
    let mut out: Vec<ResponsesInputItem> = Vec::new();
    let mut read_tool_uses_with_offset = HashSet::new();

    for msg in &req.messages {
        let blocks = normalize_content(&msg.content, Value::Null);
        match msg.role.as_str() {
            "user" => {
                let mut parts: Vec<ResponsesContentPart> = Vec::new();
                for block in &blocks {
                    match block {
                        ContentBlock::Text { text } => {
                            parts.push(ResponsesContentPart::InputText { text: text.clone() });
                        }
                        ContentBlock::Image { source } => {
                            parts.push(ResponsesContentPart::InputImage {
                                image_url: image_source_to_url(source),
                                detail: None,
                            });
                        }
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } => {
                            if !parts.is_empty() {
                                out.push(ResponsesInputItem::Message {
                                    role: "user".to_string(),
                                    content: std::mem::take(&mut parts),
                                });
                            }
                            if let Some(plan) = tool_search_plan
                                && plan.is_tool_search_call(tool_use_id)
                            {
                                // Text-only results (nothing found, an error) load no
                                // tools; the search output then carries none.
                                out.push(ResponsesInputItem::ToolSearchOutput {
                                    call_id: tool_use_id.clone(),
                                    status: TOOL_SEARCH_STATUS.to_string(),
                                    execution: TOOL_SEARCH_EXECUTION.to_string(),
                                    tools: plan
                                        .referenced_deferred_tools(content)
                                        .into_iter()
                                        .map(loaded_tool_spec)
                                        .collect(),
                                });
                                continue;
                            }
                            let mut rendered = render_tool_result(content);
                            if is_error.unwrap_or(false) {
                                rendered.prepend_text("[tool execution error]".to_string());
                            }
                            if let Some(note) =
                                rewritten_read_offset_note(&rendered.joined_text(), tool_use_id)
                            {
                                rendered.push_text(format!("\n{note}"));
                            }
                            if should_append_read_offset_guidance(
                                &rendered.joined_text(),
                                read_tool_uses_with_offset.contains(tool_use_id),
                                is_error.unwrap_or(false),
                            ) {
                                rendered.push_text(format!("\n{}", read_offset_guidance()));
                            }
                            out.push(ResponsesInputItem::FunctionCallOutput {
                                call_id: tool_use_id.clone(),
                                output: function_call_output(rendered),
                            });
                        }
                        _ => {}
                    }
                }
                if !parts.is_empty() {
                    out.push(ResponsesInputItem::Message {
                        role: "user".to_string(),
                        content: parts,
                    });
                }
            }
            "system" => {
                let parts: Vec<ResponsesContentPart> = blocks
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => {
                            Some(ResponsesContentPart::InputText { text: text.clone() })
                        }
                        _ => None,
                    })
                    .collect();
                if !parts.is_empty() {
                    out.push(ResponsesInputItem::Message {
                        role: "developer".to_string(),
                        content: parts,
                    });
                }
            }
            _ => {
                let mut text_parts: Vec<ResponsesContentPart> = Vec::new();
                let flush_text =
                    |out: &mut Vec<ResponsesInputItem>,
                     text_parts: &mut Vec<ResponsesContentPart>| {
                        if !text_parts.is_empty() {
                            out.push(ResponsesInputItem::Message {
                                role: "assistant".to_string(),
                                content: std::mem::take(text_parts),
                            });
                        }
                    };
                for block in &blocks {
                    match block {
                        ContentBlock::Text { text } => {
                            text_parts
                                .push(ResponsesContentPart::OutputText { text: text.clone() });
                        }
                        ContentBlock::ToolUse { id, name, input }
                            if name == CLAUDE_CODE_TOOL_SEARCH_NAME
                                && tool_search_plan
                                    .is_some_and(|plan| plan.is_tool_search_call(id)) =>
                        {
                            flush_text(&mut out, &mut text_parts);
                            out.push(ResponsesInputItem::ToolSearchCall {
                                call_id: id.clone(),
                                execution: TOOL_SEARCH_EXECUTION.to_string(),
                                status: TOOL_SEARCH_STATUS.to_string(),
                                arguments: input.clone(),
                            });
                        }
                        ContentBlock::ToolUse { id, name, input } => {
                            flush_text(&mut out, &mut text_parts);
                            if is_read_tool_use_with_offset(name, input) {
                                read_tool_uses_with_offset.insert(id.clone());
                            }
                            let args =
                                serde_json::to_string(input).unwrap_or_else(|_| "{}".to_string());
                            out.push(ResponsesInputItem::FunctionCall {
                                call_id: id.clone(),
                                name: name.clone(),
                                arguments: args,
                            });
                        }
                        ContentBlock::Thinking {
                            thinking,
                            signature,
                        } => {
                            if let Some(replay) =
                                signature.as_deref().and_then(decode_reasoning_signature)
                            {
                                flush_text(&mut out, &mut text_parts);
                                out.push(ResponsesInputItem::Reasoning {
                                    id: replay.id,
                                    summary: Vec::new(),
                                    encrypted_content: replay.encrypted_content,
                                });
                            } else if !thinking.is_empty() {
                                text_parts.push(ResponsesContentPart::OutputText {
                                    text: wrap_reasoning(thinking),
                                });
                            }
                        }
                        _ => {}
                    }
                }
                flush_text(&mut out, &mut text_parts);
            }
        }
    }

    out
}

fn is_read_tool_use_with_offset(name: &str, input: &Value) -> bool {
    name == "Read" && input.get("offset").is_some()
}

fn rewritten_read_offset_note(output: &str, tool_use_id: &str) -> Option<String> {
    if output.contains("Proxy Read offset note:") {
        return None;
    }
    read_offset_rewrite(tool_use_id)
        .as_ref()
        .map(read_offset_rewrite_note)
}

fn read_offset_rewrite_note(rewrite: &ReadOffsetRewrite) -> String {
    let file = rewrite
        .file_path
        .as_deref()
        .map(|path| format!(" for {path}"))
        .unwrap_or_default();
    format!(
        "Proxy Read offset note:\n\
         - Requested Read offset {}{} exceeds the proxy rewrite threshold of 1000000.\n\
         - This Read starts at the beginning of the file.\n\
         - For continuation reads, use offset after a prior Read of the same file returned content and more lines are needed.\n\
         - Compute offset as prior offset plus the number of lines returned by that prior Read.",
        rewrite.offset, file
    )
}

fn should_append_read_offset_guidance(
    output: &str,
    read_call_had_offset: bool,
    is_error: bool,
) -> bool {
    read_call_had_offset
        && !output.contains("Codex Read guidance:")
        && looks_like_read_offset_result(output)
        && (is_error || looks_like_read_offset_warning(output))
}

fn looks_like_read_offset_result(output: &str) -> bool {
    let lower = output.to_ascii_lowercase();
    lower.contains("offset")
        && (lower.contains("file has")
            || lower.contains("out of range")
            || (lower.contains("line") && lower.contains("requested")))
}

fn looks_like_read_offset_warning(output: &str) -> bool {
    let lower = output.to_ascii_lowercase();
    lower.contains("warning") || lower.contains("system-reminder")
}

fn read_offset_guidance() -> &'static str {
    "Codex Read guidance:\n\
     - offset is an optional zero based continuation index, not a line number lookup.\n\
     - Use offset only after a prior Read of the same file returned content and more lines are needed.\n\
     - Compute offset as prior offset plus the number of lines returned by that prior Read.\n\
     - Displayed line numbers, grep line numbers, byte counts, token counts, file sizes, and guessed positions are invalid offsets.\n\
     - Omit offset and limit when opening a file or when unsure."
}

// ---------------------------------------------------------------------------
// Tool result rendering
// ---------------------------------------------------------------------------

enum RenderedToolResultPart {
    Text(String),
    Image(String),
}

struct RenderedToolResult {
    parts: Vec<RenderedToolResultPart>,
}

impl RenderedToolResult {
    fn has_images(&self) -> bool {
        self.parts
            .iter()
            .any(|part| matches!(part, RenderedToolResultPart::Image(_)))
    }

    fn joined_text(&self) -> String {
        self.parts
            .iter()
            .filter_map(|part| match part {
                RenderedToolResultPart::Text(text) => Some(text.as_str()),
                RenderedToolResultPart::Image(_) => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn prepend_text(&mut self, text: String) {
        self.parts.insert(0, RenderedToolResultPart::Text(text));
    }

    fn push_text(&mut self, text: String) {
        self.parts.push(RenderedToolResultPart::Text(text));
    }
}

fn render_tool_result(content: &Value) -> RenderedToolResult {
    let parts = match content {
        Value::String(text) => vec![RenderedToolResultPart::Text(text.clone())],
        Value::Array(blocks) => blocks.iter().map(render_tool_result_block).collect(),
        _ => Vec::new(),
    };
    RenderedToolResult { parts }
}

fn render_tool_result_block(block: &Value) -> RenderedToolResultPart {
    match block.get("type").and_then(Value::as_str) {
        Some("text") => block
            .get("text")
            .and_then(Value::as_str)
            .map(|text| RenderedToolResultPart::Text(text.to_string()))
            .unwrap_or_else(|| {
                RenderedToolResultPart::Text(unsupported_tool_result_block_to_string(block))
            }),
        Some("image") => render_tool_result_image(block),
        Some(other) => {
            RenderedToolResultPart::Text(format!("[unsupported content block omitted: {other}]"))
        }
        None => RenderedToolResultPart::Text(unsupported_tool_result_block_to_string(block)),
    }
}

fn render_tool_result_image(block: &Value) -> RenderedToolResultPart {
    let Some(source) = block.get("source").and_then(Value::as_object) else {
        return RenderedToolResultPart::Text(unsupported_tool_result_block_to_string(block));
    };
    match source.get("type").and_then(Value::as_str) {
        Some("url") if source.get("url").and_then(Value::as_str).is_some() => {
            RenderedToolResultPart::Text("[image omitted: url]".to_string())
        }
        Some("base64") => {
            let media_type = source.get("media_type").and_then(Value::as_str);
            let data = source.get("data").and_then(Value::as_str);
            match media_type
                .zip(data)
                .and_then(|(media_type, data)| validated_image_data_url(media_type, data))
            {
                Some(image_url) => RenderedToolResultPart::Image(image_url),
                None => {
                    RenderedToolResultPart::Text(unsupported_tool_result_block_to_string(block))
                }
            }
        }
        _ => RenderedToolResultPart::Text(unsupported_tool_result_block_to_string(block)),
    }
}

fn validated_image_data_url(media_type: &str, data: &str) -> Option<String> {
    if !matches!(
        media_type,
        "image/jpeg" | "image/png" | "image/gif" | "image/webp"
    ) {
        return None;
    }

    let compact: String = data
        .chars()
        .filter(|character| !character.is_ascii_whitespace())
        .collect();
    if compact.is_empty() {
        return None;
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(&compact)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(&compact))
        .ok()?;
    let canonical = base64::engine::general_purpose::STANDARD.encode(decoded);
    Some(format!("data:{media_type};base64,{canonical}"))
}

fn function_call_output(rendered: RenderedToolResult) -> ResponsesFunctionCallOutput {
    if !rendered.has_images() {
        return ResponsesFunctionCallOutput::Text(rendered.joined_text());
    }

    ResponsesFunctionCallOutput::ContentItems(
        rendered
            .parts
            .into_iter()
            .map(|part| match part {
                RenderedToolResultPart::Text(text) => {
                    ResponsesFunctionCallOutputContentPart::InputText { text }
                }
                RenderedToolResultPart::Image(image_url) => {
                    ResponsesFunctionCallOutputContentPart::InputImage {
                        image_url,
                        detail: None,
                    }
                }
            })
            .collect(),
    )
}

fn unsupported_tool_result_block_to_string(block: &Value) -> String {
    let kind = block
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    format!("[unsupported content block omitted: {kind}]")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const PNG_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";

    fn opts() -> TranslateOptions {
        TranslateOptions {
            session_id: None,
            service_tier: None,
            model: "gpt-5.5".to_string(),
            use_responses_lite: false,
        }
    }

    #[test]
    fn responses_tool_choice_modes_serialize_as_openai_strings() {
        for (mode, expected) in [
            (ResponsesToolChoiceMode::Auto, json!("auto")),
            (ResponsesToolChoiceMode::None, json!("none")),
            (ResponsesToolChoiceMode::Required, json!("required")),
        ] {
            assert_eq!(
                serde_json::to_value(ResponsesToolChoice::Mode(mode)).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn translate_tool_choice_preserves_wire_and_parallel_semantics() {
        for (tool_choice, expected_choice, expected_parallel) in [
            (json!({"type":"auto"}), json!("auto"), true),
            (
                json!({"type":"auto","disable_parallel_tool_use":false}),
                json!("auto"),
                true,
            ),
            (json!({"type":"none"}), json!("none"), true),
            (json!({"type":"any"}), json!("required"), true),
            (
                json!({"type":"any","disable_parallel_tool_use":true}),
                json!("required"),
                false,
            ),
            (
                json!({
                    "type":"tool",
                    "name":"test",
                    "disable_parallel_tool_use":true
                }),
                json!({"type":"function","name":"test"}),
                false,
            ),
        ] {
            let req: MessagesRequest = serde_json::from_value(json!({
                "model": "gpt-5.5",
                "messages": [{"role":"user", "content":"use the tool"}],
                "tools": [{
                    "name":"test",
                    "input_schema":{"type":"object","properties":{}}
                }],
                "tool_choice": tool_choice
            }))
            .unwrap();
            let wire = serde_json::to_value(translate_request(&req, opts()).unwrap()).unwrap();

            assert_eq!(wire["tool_choice"], expected_choice);
            assert_eq!(wire["parallel_tool_calls"], expected_parallel);
        }
    }

    #[test]
    fn responses_lite_keeps_parallel_tool_calls_disabled() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-luna",
            "messages": [{"role":"user", "content":"use the tool"}],
            "tools": [{
                "name":"test",
                "input_schema":{"type":"object","properties":{}}
            }],
            "tool_choice": {
                "type":"any",
                "disable_parallel_tool_use":false
            }
        }))
        .unwrap();
        let wire = serde_json::to_value(
            translate_request(
                &req,
                TranslateOptions {
                    model: "gpt-5.6-luna".to_string(),
                    use_responses_lite: true,
                    ..opts()
                },
            )
            .unwrap(),
        )
        .unwrap();

        assert_eq!(wire["tool_choice"], json!("required"));
        assert_eq!(wire["parallel_tool_calls"], false);
    }

    #[test]
    fn translate_web_search_tool_to_codex_tool() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content":"find it"}],
            "tools": [{
                "type":"web_search_20250305",
                "name":"web_search",
                "allowed_domains":["example.com"]
            }],
            "tool_choice": {"type":"tool", "name":"web_search"}
        }))
        .unwrap();
        let out = translate_request(
            &req,
            TranslateOptions {
                session_id: Some("s".into()),
                service_tier: None,
                model: "gpt-5.5".to_string(),
                use_responses_lite: false,
            },
        )
        .unwrap();
        assert_eq!(out.prompt_cache_key.as_deref(), Some("s"));
        assert!(matches!(
            out.tool_choice,
            Some(ResponsesToolChoice::AllowedTools { .. })
        ));
        let tool_choice = serde_json::to_value(out.tool_choice.as_ref().unwrap()).unwrap();
        assert_eq!(tool_choice["type"], "allowed_tools");
        assert_eq!(tool_choice["mode"], "required");
        assert_eq!(tool_choice["tools"], json!([{"type":"web_search"}]));
        let ResponsesTool::WebSearch(tool) = &out.tools.as_ref().unwrap()[0] else {
            panic!("expected web_search tool");
        };
        assert!(tool.external_web_access);
        assert_eq!(
            tool.filters.as_ref().unwrap().allowed_domains.as_deref(),
            Some(&["example.com".to_string()][..])
        );
        assert!(out.instructions.is_none());
    }

    #[test]
    fn automatic_filtered_web_search_keeps_native_filters() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content":"find it"}],
            "tools": [{
                "type":"web_search_20250305",
                "name":"web_search",
                "allowed_domains":["example.com"],
                "blocked_domains":["spam.example"]
            }],
            "tool_choice": {"type":"auto"}
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        let ResponsesTool::WebSearch(tool) = &out.tools.as_ref().unwrap()[0] else {
            panic!("expected web_search tool");
        };
        assert!(tool.external_web_access);
        let filters = tool.filters.as_ref().unwrap();
        assert_eq!(
            filters.allowed_domains.as_deref(),
            Some(&["example.com".to_string()][..])
        );
        assert_eq!(
            filters.blocked_domains.as_deref(),
            Some(&["spam.example".to_string()][..])
        );
        assert!(out.instructions.is_none());
    }

    #[test]
    fn forced_filtered_web_search_keeps_native_filters() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content":"find it"}],
            "system": "Be brief.",
            "tools": [{
                "type":"web_search_20250305",
                "name":"web_search",
                "allowed_domains":["a.example", "b.example"],
                "blocked_domains":["spam.example"]
            }],
            "tool_choice": {"type":"tool", "name":"web_search"}
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        let ResponsesTool::WebSearch(tool) = &out.tools.as_ref().unwrap()[0] else {
            panic!("expected web_search tool");
        };
        let filters = tool.filters.as_ref().unwrap();
        assert_eq!(
            filters.allowed_domains.as_deref(),
            Some(&["a.example".to_string(), "b.example".to_string()][..])
        );
        assert_eq!(
            filters.blocked_domains.as_deref(),
            Some(&["spam.example".to_string()][..])
        );
        assert_eq!(out.instructions.as_deref(), Some("Be brief."));
        assert!(matches!(
            out.tool_choice,
            Some(ResponsesToolChoice::AllowedTools { .. })
        ));
    }

    #[test]
    fn unfiltered_web_search_adds_no_domain_instructions() {
        for tool_choice in [None, Some(json!({"type":"tool", "name":"web_search"}))] {
            let mut body = json!({
                "model": "gpt-5.5",
                "messages": [{"role":"user", "content":"find it"}],
                "tools": [{"type":"web_search_20250305", "name":"web_search"}]
            });
            if let Some(tool_choice) = tool_choice {
                body["tool_choice"] = tool_choice;
            }
            let req: MessagesRequest = serde_json::from_value(body).unwrap();
            let out = translate_request(&req, opts()).unwrap();
            let ResponsesTool::WebSearch(tool) = &out.tools.as_ref().unwrap()[0] else {
                panic!("expected web_search tool");
            };
            assert!(tool.external_web_access);
            assert!(tool.filters.is_none());
            assert!(out.instructions.is_none());
        }
    }

    #[test]
    fn has_hosted_web_search_detects_web_search_tool() {
        let with: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-sol",
            "messages": [{"role":"user", "content":"find it"}],
            "tools": [
                {"name":"Bash", "input_schema":{}},
                {"type":"web_search_20250305", "name":"web_search"}
            ]
        }))
        .unwrap();
        assert!(has_hosted_web_search(&with));

        let without: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-sol",
            "messages": [{"role":"user", "content":"run it"}],
            "tools": [{"name":"Bash", "input_schema":{}}]
        }))
        .unwrap();
        assert!(!has_hosted_web_search(&without));
    }

    #[test]
    fn responses_lite_downgrades_unregistered_web_search_tool_choice() {
        // On the lite lane tools travel in the AdditionalTools developer
        // prefix, so a top-level web_search tool_choice would reference a
        // tool upstream doesn't know about and 502.
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-sol",
            "messages": [{"role":"user", "content":"find it"}],
            "tools": [{
                "type":"web_search_20250305",
                "name":"web_search"
            }],
            "tool_choice": {"type":"tool", "name":"web_search"}
        }))
        .unwrap();
        let out = translate_request(
            &req,
            TranslateOptions {
                session_id: None,
                service_tier: None,
                model: "gpt-5.6-sol".to_string(),
                use_responses_lite: true,
            },
        )
        .unwrap();
        assert!(out.tools.is_none());
        assert!(matches!(
            out.tool_choice,
            Some(ResponsesToolChoice::Mode(ResponsesToolChoiceMode::Auto))
        ));
        assert_eq!(
            serde_json::to_value(&out).unwrap()["tool_choice"],
            json!("auto")
        );
    }

    #[test]
    fn full_lane_keeps_web_search_tool_choice_registered() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-sol",
            "messages": [{"role":"user", "content":"find it"}],
            "tools": [{
                "type":"web_search_20250305",
                "name":"web_search"
            }],
            "tool_choice": {"type":"tool", "name":"web_search"}
        }))
        .unwrap();
        let out = translate_request(
            &req,
            TranslateOptions {
                session_id: None,
                service_tier: None,
                model: "gpt-5.6-sol".to_string(),
                use_responses_lite: false,
            },
        )
        .unwrap();
        assert!(out.tools.as_ref().is_some_and(|t| {
            t.iter()
                .any(|tool| matches!(tool, ResponsesTool::WebSearch(_)))
        }));
        assert!(matches!(
            out.tool_choice,
            Some(ResponsesToolChoice::AllowedTools { .. })
        ));
    }

    #[test]
    fn translate_read_tool_adds_codex_offset_guidance() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content":"read it"}],
            "tools": [{
                "name": "Read",
                "description": "Reads a file from the local filesystem.",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "file_path": {"type": "string"},
                        "offset": {"type": "integer", "description": "old offset"},
                        "limit": {"type": "integer", "description": "old limit"}
                    },
                    "required": ["file_path"]
                }
            }]
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        let tools = out.tools.as_ref().unwrap();
        let ResponsesTool::Function(tool) = &tools[0] else {
            panic!("expected function tool");
        };
        let description = tool.description.as_deref().unwrap();
        assert!(description.contains("Codex Read guidance"));
        assert!(description.contains("zero based continuation index"));
        assert!(description.contains("guessed positions are invalid offsets"));

        let props = tool
            .parameters
            .get("properties")
            .and_then(Value::as_object)
            .unwrap();
        assert_eq!(
            props
                .get("offset")
                .and_then(|v| v.get("description"))
                .and_then(Value::as_str),
            Some(
                "Optional continuation index. Use only after a prior Read of the same file returned content and more lines are needed. Compute as prior offset plus returned line count. Displayed line numbers, grep line numbers, byte counts, token counts, file sizes, and guessed positions are invalid offsets. Omit when unsure."
            )
        );
        assert_eq!(
            props
                .get("limit")
                .and_then(|v| v.get("description"))
                .and_then(Value::as_str),
            Some(
                "Optional number of lines to read. Omit when opening a file. Use with offset only when continuing a large file."
            )
        );
    }

    #[test]
    fn translate_non_read_tool_preserves_tool_metadata() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content":"search"}],
            "tools": [{
                "name": "Search",
                "description": "Find matching records.",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "offset": {"type": "integer", "description": "record offset"}
                    }
                }
            }]
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        let tools = out.tools.as_ref().unwrap();
        let ResponsesTool::Function(tool) = &tools[0] else {
            panic!("expected function tool");
        };
        assert_eq!(tool.description.as_deref(), Some("Find matching records."));
        assert!(!tool.strict);
        assert_eq!(
            serde_json::to_value(tool).unwrap()["strict"],
            Value::Bool(false)
        );
        assert_eq!(
            tool.parameters
                .get("properties")
                .and_then(|v| v.get("offset"))
                .and_then(|v| v.get("description"))
                .and_then(Value::as_str),
            Some("record offset")
        );
    }

    #[test]
    fn translate_omits_reasoning_when_not_enabled() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content":"hello"}]
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        assert!(out.reasoning.is_none());
        assert!(out.include.is_none());
    }

    #[test]
    fn translate_includes_reasoning_when_enabled() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content":"hello"}],
            "output_config": {"effort": "medium"}
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        let reasoning = out.reasoning.unwrap();
        assert!(matches!(reasoning.effort, Some(Effort::Medium)));
        assert_eq!(reasoning.summary.as_deref(), Some("auto"));
        assert_eq!(
            out.include,
            Some(vec!["reasoning.encrypted_content".to_string()])
        );
    }

    #[test]
    fn translate_effort_max_maps_to_max() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content":"hello"}],
            "output_config": {"effort": "max"}
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        assert!(matches!(out.reasoning.unwrap().effort, Some(Effort::Max)));
    }

    #[test]
    fn translate_effort_override_max_maps_to_max() {
        let effort = resolve_effort_override(Some(Effort::Low), Some("max")).unwrap();
        assert!(matches!(effort, Some(Effort::Max)));
    }

    #[test]
    fn compact_request_detected_from_system_marker() {
        assert!(is_compact_request(Some(
            "You are a helpful AI assistant tasked with summarizing conversations."
        )));
        assert!(!is_compact_request(Some("You are Claude Code.")));
        assert!(!is_compact_request(None));
    }

    #[test]
    fn compact_request_detected_from_final_user_message() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-sol",
            "messages": [
                {"role": "user", "content": "prior turn"},
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "tool_result",
                            "tool_use_id": "tool-1",
                            "content": "result"
                        },
                        {
                            "type": "text",
                            "text": concat!(
                                "CRITICAL: Respond with TEXT ONLY. Do NOT call any tools.\n\n",
                                "Your task is to create a detailed summary of the conversation so far, ",
                                "paying close attention to the user's explicit requests."
                            )
                        }
                    ]
                }
            ],
            "system": "You are Claude Code."
        }))
        .unwrap();

        assert!(is_compact_messages_request(&req));
    }

    #[test]
    fn compact_message_markers_must_be_in_final_user_message() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-sol",
            "messages": [
                {
                    "role": "user",
                    "content": concat!(
                        "CRITICAL: Respond with TEXT ONLY. Do NOT call any tools.\n",
                        "Your task is to create a detailed summary of the conversation so far."
                    )
                },
                {"role": "user", "content": "continue normally"}
            ],
            "system": "You are Claude Code."
        }))
        .unwrap();

        assert!(!is_compact_messages_request(&req));
    }

    #[test]
    fn compact_message_requires_both_markers() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-sol",
            "messages": [{
                "role": "user",
                "content": "Your task is to create a detailed summary of the conversation so far."
            }],
            "system": "You are Claude Code."
        }))
        .unwrap();

        assert!(!is_compact_messages_request(&req));
    }

    #[test]
    fn compact_effort_cap_parses_env_values() {
        assert!(matches!(compact_effort_cap_from(None), Some(Effort::Low)));
        assert!(matches!(
            compact_effort_cap_from(Some("")),
            Some(Effort::Low)
        ));
        assert!(compact_effort_cap_from(Some("off")).is_none());
        assert!(matches!(
            compact_effort_cap_from(Some("none")),
            Some(Effort::None)
        ));
        assert!(matches!(
            compact_effort_cap_from(Some("medium")),
            Some(Effort::Medium)
        ));
        // Unrecognized values fall back to the safe default.
        assert!(matches!(
            compact_effort_cap_from(Some("bogus")),
            Some(Effort::Low)
        ));
    }

    #[test]
    fn compact_request_downgrades_effort_to_cap() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content":"summarize"}],
            "system": "You are a helpful AI assistant tasked with summarizing conversations.",
            "output_config": {"effort": "medium"}
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        assert!(matches!(out.reasoning.unwrap().effort, Some(Effort::Low)));
    }

    #[test]
    fn compact_cap_never_raises_effort() {
        // A compact request already at or below the cap is left alone.
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content":"summarize"}],
            "system": "You are a helpful AI assistant tasked with summarizing conversations.",
            "output_config": {"effort": "low"}
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        assert!(matches!(out.reasoning.unwrap().effort, Some(Effort::Low)));
    }

    #[test]
    fn non_compact_request_keeps_requested_effort() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content":"hello"}],
            "system": "You are Claude Code.",
            "output_config": {"effort": "high"}
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        assert!(matches!(out.reasoning.unwrap().effort, Some(Effort::High)));
    }

    #[test]
    fn effort_ordering_matches_variant_order() {
        assert!(Effort::None < Effort::Low);
        assert!(Effort::Low < Effort::Medium);
        assert!(Effort::Medium < Effort::High);
        assert!(Effort::High < Effort::Xhigh);
        assert!(Effort::Xhigh < Effort::Max);
    }

    #[test]
    fn max_tokens_is_not_serialized_for_codex() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "max_tokens": 4096,
            "messages": [{"role":"user", "content":"hello"}]
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        let value = serde_json::to_value(out).unwrap();
        assert!(value.get("max_output_tokens").is_none());
    }

    #[test]
    fn translate_effort_xhigh_maps_to_xhigh() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content":"hello"}],
            "output_config": {"effort": "xhigh"}
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        assert!(matches!(out.reasoning.unwrap().effort, Some(Effort::Xhigh)));
        assert_eq!(
            out.include,
            Some(vec!["reasoning.encrypted_content".to_string()])
        );
    }

    #[test]
    fn reasoning_summary_override_values() {
        assert!(reasoning_summary_requested(None));
        assert!(reasoning_summary_requested(Some("auto")));
        assert!(reasoning_summary_requested(Some("detailed")));
        assert!(!reasoning_summary_requested(Some("off")));
        assert!(!reasoning_summary_requested(Some("none")));
    }

    #[test]
    fn translate_user_text_and_image() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content": [
                {"type":"text", "text":"describe"},
                {"type":"image", "source": {"type":"base64", "media_type":"image/jpeg", "data":"xyz"}}
            ]}]
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        assert_eq!(out.input.len(), 1);
        if let ResponsesInputItem::Message { role, content } = &out.input[0] {
            assert_eq!(role, "user");
            assert_eq!(content.len(), 2);
        } else {
            panic!("expected Message");
        }
    }

    #[test]
    fn translate_assistant_with_text_and_tool_use() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"assistant", "content": [
                {"type":"text", "text":"answer"},
                {"type":"tool_use", "id":"tu_1", "name":"search", "input": {"q":"rust"}}
            ]}]
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        assert_eq!(out.input.len(), 2);
    }

    #[test]
    fn translate_assistant_thinking_becomes_tagged_reasoning() {
        // Symmetric with the anthropic passthrough: on an opus->codex switch a replayed
        // thinking block has no Responses container, so it is carried as tagged text
        // rather than dropped.
        use crate::providers::translate_shared::{REASONING_CLOSE, REASONING_OPEN};
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"assistant", "content": [
                {"type":"thinking", "thinking":"opus reasoning", "signature":"sig"},
                {"type":"text", "text":"the answer"}
            ]}]
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        assert_eq!(out.input.len(), 1);
        let ResponsesInputItem::Message { role, content } = &out.input[0] else {
            panic!("expected Message");
        };
        assert_eq!(role, "assistant");
        assert_eq!(content.len(), 2);
        let ResponsesContentPart::OutputText { text: reasoning } = &content[0] else {
            panic!("expected reasoning OutputText");
        };
        assert!(reasoning.starts_with(REASONING_OPEN), "{reasoning}");
        assert!(reasoning.contains("opus reasoning"), "{reasoning}");
        assert!(reasoning.ends_with(REASONING_CLOSE), "{reasoning}");
        let ResponsesContentPart::OutputText { text: answer } = &content[1] else {
            panic!("expected answer OutputText");
        };
        assert_eq!(answer, "the answer");
    }

    #[test]
    fn translate_strict_json_schema_normalization() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content":"hi"}],
            "output_config": {"format": {
                "type": "json_schema",
                "schema": {
                    "type": "object",
                    "properties": {"ok": {"type": "boolean"}, "reason": {"type": "string"}},
                    "required": ["ok"]
                }
            }}
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        if let Some(ResponsesTextFormat::JsonSchema { schema, .. }) = &out.text.format {
            let required = schema.get("required").and_then(|v| v.as_array()).unwrap();
            assert!(required.iter().any(|v| v == "ok"));
            assert!(required.iter().any(|v| v == "reason"));
        } else {
            panic!("expected JsonSchema format");
        }
    }

    #[test]
    fn translate_tool_result_content() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content": [{
                "type": "tool_result",
                "tool_use_id": "tu_1",
                "content": [{"type":"text", "text":"result"}]
            }]}]
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        assert_eq!(out.input.len(), 1);
        if let ResponsesInputItem::FunctionCallOutput { call_id, output } = &out.input[0] {
            assert_eq!(call_id, "tu_1");
            assert_eq!(output.as_text(), Some("result"));
        } else {
            panic!("expected FunctionCallOutput");
        }
    }

    #[test]
    fn translate_read_offset_error_adds_guidance() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [
                {"role":"assistant", "content": [{
                    "type": "tool_use",
                    "id": "tu_1",
                    "name": "Read",
                    "input": {"file_path": "/tmp/a", "offset": 2952, "limit": 200}
                }]},
                {"role":"user", "content": [{
                    "type": "tool_result",
                    "tool_use_id": "tu_1",
                    "is_error": true,
                    "content": [{"type":"text", "text":"File has 331 lines, but offset 2952 was requested."}]
                }]}
            ]
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        assert_eq!(out.input.len(), 2);
        if let ResponsesInputItem::FunctionCallOutput { output, .. } = &out.input[1] {
            let output = output.as_text().expect("text tool output");
            assert!(output.contains("[tool execution error]"));
            assert!(output.contains("File has 331 lines"));
            assert!(output.contains("Codex Read guidance:"));
            assert!(output.contains("zero based continuation index"));
        } else {
            panic!("expected FunctionCallOutput");
        }
    }

    #[test]
    fn translate_read_unrelated_error_keeps_original_output() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [
                {"role":"assistant", "content": [{
                    "type": "tool_use",
                    "id": "tu_1",
                    "name": "Read",
                    "input": {"file_path": "/tmp/a", "offset": 10, "limit": 20}
                }]},
                {"role":"user", "content": [{
                    "type": "tool_result",
                    "tool_use_id": "tu_1",
                    "is_error": true,
                    "content": [{"type":"text", "text":"File does not exist."}]
                }]}
            ]
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        assert_eq!(out.input.len(), 2);
        if let ResponsesInputItem::FunctionCallOutput { output, .. } = &out.input[1] {
            assert_eq!(
                output.as_text(),
                Some("[tool execution error]\nFile does not exist.")
            );
        } else {
            panic!("expected FunctionCallOutput");
        }
    }

    #[test]
    fn translate_rewritten_read_result_adds_proxy_note() {
        crate::providers::codex::translate::read_rewrite::sanitize_read_args(
            "Read",
            r#"{"file_path":"/tmp/a","offset":1300000,"limit":20}"#,
            Some("tu_rewritten_read"),
        );
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [
                {"role":"assistant", "content": [{
                    "type": "tool_use",
                    "id": "tu_rewritten_read",
                    "name": "Read",
                    "input": {"file_path": "/tmp/a", "limit": 20}
                }]},
                {"role":"user", "content": [{
                    "type": "tool_result",
                    "tool_use_id": "tu_rewritten_read",
                    "content": [{"type":"text", "text":"1\tcontent"}]
                }]}
            ]
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        assert_eq!(out.input.len(), 2);
        if let ResponsesInputItem::FunctionCallOutput { output, .. } = &out.input[1] {
            let output = output.as_text().expect("text tool output");
            assert!(output.contains("1\tcontent"));
            assert!(output.contains("Proxy Read offset note:"));
            assert!(output.contains("1300000"));
            assert!(output.contains("/tmp/a"));
        } else {
            panic!("expected FunctionCallOutput");
        }
    }

    #[test]
    fn translate_read_success_with_offset_words_keeps_original_output() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [
                {"role":"assistant", "content": [{
                    "type": "tool_use",
                    "id": "tu_1",
                    "name": "Read",
                    "input": {"file_path": "/tmp/a", "offset": 10, "limit": 20}
                }]},
                {"role":"user", "content": [{
                    "type": "tool_result",
                    "tool_use_id": "tu_1",
                    "content": [{"type":"text", "text":"File has 331 lines, and the requested offset is shown in this fixture."}]
                }]}
            ]
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        assert_eq!(out.input.len(), 2);
        if let ResponsesInputItem::FunctionCallOutput { output, .. } = &out.input[1] {
            assert_eq!(
                output.as_text(),
                Some("File has 331 lines, and the requested offset is shown in this fixture.")
            );
        } else {
            panic!("expected FunctionCallOutput");
        }
    }

    #[test]
    fn translate_tool_result_preserves_mixed_content_order() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content": [{
                "type": "tool_result",
                "tool_use_id": "tu_image",
                "content": [
                    {"type": "text", "text": "before"},
                    {"type": "image", "source": {
                        "type": "base64",
                        "media_type": "image/png",
                        "data": PNG_BASE64
                    }},
                    {"type": "text", "text": "after"}
                ]
            }]}]
        }))
        .unwrap();

        let out = translate_request(&req, opts()).unwrap();
        assert_eq!(
            serde_json::to_value(&out.input[0]).unwrap(),
            json!({
                "type": "function_call_output",
                "call_id": "tu_image",
                "output": [
                    {"type": "input_text", "text": "before"},
                    {"type": "input_image", "image_url": format!("data:image/png;base64,{PNG_BASE64}")},
                    {"type": "input_text", "text": "after"}
                ]
            })
        );
    }

    #[test]
    fn translate_tool_result_preserves_image_then_text_order() {
        let rendered = render_tool_result(&json!([
            {"type": "image", "source": {
                "type": "base64",
                "media_type": "image/png",
                "data": PNG_BASE64
            }},
            {"type": "text", "text": "caption"}
        ]));

        assert_eq!(
            serde_json::to_value(function_call_output(rendered)).unwrap(),
            json!([
                {"type": "input_image", "image_url": format!("data:image/png;base64,{PNG_BASE64}")},
                {"type": "input_text", "text": "caption"}
            ])
        );
    }

    #[test]
    fn unsupported_tool_result_images_become_in_place_text_placeholders() {
        let rendered = render_tool_result(&json!([
            {"type": "text", "text": "before"},
            {"type": "image", "source": {
                "type": "url",
                "url": "https://example.invalid/a.png"
            }},
            {"type": "image", "source": {
                "type": "base64",
                "media_type": "text/plain",
                "data": "aGVsbG8="
            }},
            {"type": "image", "source": {
                "type": "base64",
                "media_type": "image/png",
                "data": "not base64"
            }},
            {"type": "text", "text": "after"}
        ]));

        assert_eq!(
            serde_json::to_value(function_call_output(rendered)).unwrap(),
            json!(
                "before\n[image omitted: url]\n[unsupported content block omitted: image]\n[unsupported content block omitted: image]\nafter"
            )
        );
    }

    #[test]
    fn supported_tool_result_image_media_types_pass_validation() {
        for media_type in ["image/jpeg", "image/png", "image/gif", "image/webp"] {
            assert_eq!(
                validated_image_data_url(media_type, "YQ"),
                Some(format!("data:{media_type};base64,YQ=="))
            );
        }
        assert!(validated_image_data_url("image/svg+xml", "YQ==").is_none());
        assert!(validated_image_data_url("image/png", "").is_none());
    }

    #[test]
    fn text_only_tool_result_keeps_string_wire_format() {
        let rendered = render_tool_result(&json!([
            {"type": "text", "text": "first"},
            {"type": "text", "text": "second"}
        ]));

        assert_eq!(
            serde_json::to_value(function_call_output(rendered)).unwrap(),
            json!("first\nsecond")
        );
    }

    #[test]
    fn tool_result_error_prefix_precedes_image_content() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content": [{
                "type": "tool_result",
                "tool_use_id": "tu_error_image",
                "is_error": true,
                "content": [{"type": "image", "source": {
                    "type": "base64",
                    "media_type": "image/png",
                    "data": PNG_BASE64
                }}]
            }]}]
        }))
        .unwrap();

        let out = translate_request(&req, opts()).unwrap();
        assert_eq!(
            serde_json::to_value(&out.input[0]).unwrap()["output"],
            json!([
                {"type": "input_text", "text": "[tool execution error]"},
                {"type": "input_image", "image_url": format!("data:image/png;base64,{PNG_BASE64}")}
            ])
        );
    }

    #[test]
    fn malformed_tool_result_blocks_still_become_text_placeholders() {
        let rendered = render_tool_result(&json!([
            {"type": "text"},
            {"type": "image"},
            {}
        ]));

        assert_eq!(
            rendered.joined_text(),
            "[unsupported content block omitted: text]\n[unsupported content block omitted: image]\n[unsupported content block omitted: unknown]"
        );
        assert!(!rendered.has_images());
    }

    #[test]
    fn luna_preserves_high_effort() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-luna",
            "messages": [{"role":"user", "content":"hello"}],
            "output_config": {"effort": "high"}
        }))
        .unwrap();
        let out = translate_request(
            &req,
            TranslateOptions {
                model: "gpt-5.6-luna".to_string(),
                use_responses_lite: true,
                ..opts()
            },
        )
        .unwrap();
        assert!(matches!(out.reasoning.unwrap().effort, Some(Effort::High)));
    }

    #[test]
    fn sol_preserves_high_effort() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-sol",
            "messages": [{"role":"user", "content":"hello"}],
            "output_config": {"effort": "high"}
        }))
        .unwrap();
        let out = translate_request(
            &req,
            TranslateOptions {
                model: "gpt-5.6-sol".to_string(),
                use_responses_lite: true,
                ..opts()
            },
        )
        .unwrap();
        assert!(matches!(out.reasoning.unwrap().effort, Some(Effort::High)));
    }

    #[test]
    fn responses_lite_moves_instructions_and_tools_into_input() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-luna",
            "messages": [{"role":"user", "content":"hello"}],
            "system": "be helpful",
            "tools": [{"name":"test","input_schema":{"type":"object"}}]
        }))
        .unwrap();
        let out = translate_request(
            &req,
            TranslateOptions {
                model: "gpt-5.6-luna".to_string(),
                use_responses_lite: true,
                ..opts()
            },
        )
        .unwrap();
        assert!(out.instructions.is_none());
        assert!(out.tools.is_none());
        assert!(!out.parallel_tool_calls);
        assert!(out.client_metadata.is_some());
        assert_eq!(out.input.len(), 3);
        assert!(matches!(
            out.input[0],
            ResponsesInputItem::AdditionalTools { .. }
        ));
        if let ResponsesInputItem::Message { role, content } = &out.input[1] {
            assert_eq!(role, "developer");
            assert!(matches!(content[0], ResponsesContentPart::InputText { .. }));
        } else {
            panic!("expected developer message");
        }
    }

    #[test]
    fn responses_lite_without_effort_uses_all_turns_context() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "claude-haiku-4-5",
            "messages": [{"role":"user", "content":"hello"}]
        }))
        .unwrap();
        let out = translate_request(
            &req,
            TranslateOptions {
                model: "gpt-5.6-luna".to_string(),
                use_responses_lite: true,
                ..opts()
            },
        )
        .unwrap();
        let reasoning = out.reasoning.unwrap();
        assert!(reasoning.effort.is_none());
        assert!(reasoning.summary.is_none());
        assert_eq!(reasoning.context.as_deref(), Some("all_turns"));
        assert!(out.include.is_none());
    }

    #[test]
    fn responses_lite_reasoning_uses_all_turns_context() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-luna",
            "messages": [{"role":"user", "content":"hello"}],
            "output_config": {"effort": "medium"}
        }))
        .unwrap();
        let out = translate_request(
            &req,
            TranslateOptions {
                model: "gpt-5.6-luna".to_string(),
                use_responses_lite: true,
                ..opts()
            },
        )
        .unwrap();
        assert_eq!(out.reasoning.unwrap().context.as_deref(), Some("all_turns"));
    }

    #[test]
    fn translate_returns_only_expected_top_level_fields() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "claude-sonnet-4-6",
            "messages": [{"role":"user", "content":"hello"}],
            "system": "be helpful",
            "tools": [{"name":"test","input_schema":{"type":"object"}}],
            "tool_choice": {"type":"tool", "name":"test"}
        }))
        .unwrap();
        let out = translate_request(
            &req,
            TranslateOptions {
                model: "gpt-5.4".to_string(),
                ..opts()
            },
        )
        .unwrap();
        assert_eq!(out.model, "gpt-5.4");
        let out_value = serde_json::to_value(&out).unwrap();
        let keys: std::collections::BTreeSet<String> =
            out_value.as_object().unwrap().keys().cloned().collect();
        for key in &[
            "model",
            "input",
            "store",
            "stream",
            "parallel_tool_calls",
            "text",
        ] {
            assert!(keys.contains(*key), "missing key: {key}");
        }
    }

    #[test]
    fn assistant_thinking_signature_replays_codex_reasoning_item() {
        let replay = super::super::reasoning_signature::ReasoningReplay {
            id: "rs_1".to_string(),
            encrypted_content: "opaque".to_string(),
        };
        let signature =
            super::super::reasoning_signature::encode_reasoning_signature(&replay).unwrap();
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [
                {"role":"user","content":"start"},
                {"role":"assistant","content":[
                    {"type":"thinking","thinking":"visible summary","signature":signature},
                    {"type":"text","text":"done"}
                ]},
                {"role":"user","content":"continue"}
            ]
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        let reasoning_index = out
            .input
            .iter()
            .position(|item| matches!(item, ResponsesInputItem::Reasoning { .. }))
            .unwrap();
        let ResponsesInputItem::Reasoning {
            id,
            summary,
            encrypted_content,
        } = &out.input[reasoning_index]
        else {
            unreachable!();
        };
        assert_eq!(id, "rs_1");
        assert!(summary.is_empty());
        assert_eq!(encrypted_content, "opaque");
        assert!(matches!(
            out.input.get(reasoning_index + 1),
            Some(ResponsesInputItem::Message { role, .. }) if role == "assistant"
        ));
    }

    /// Claude Code's tools around a deferred tool load: `ToolSearch`, the
    /// deferred placeholder, and after the load the loaded tool, which Claude
    /// Code inserts in name order with `defer_loading` still set.
    fn claude_code_tools(with_loaded_tool: bool) -> Value {
        let mut tools = vec![
            json!({
                "name": "Read",
                "description": "Read a file.",
                "input_schema": {"type": "object", "properties": {"file_path": {"type": "string"}}, "required": ["file_path"]}
            }),
            json!({
                "name": "ToolSearch",
                "description": "Fetches full schema definitions for deferred tools.",
                "input_schema": {
                    "type": "object",
                    "properties": {"query": {"type": "string"}, "max_results": {"type": "number"}},
                    "required": ["query", "max_results"]
                }
            }),
            json!({
                "name": "DeferredToolPlaceholder",
                "description": "Reserved placeholder that keeps deferred tool loading active; never call this tool.",
                "defer_loading": true,
                "input_schema": {"type": "object", "properties": {}}
            }),
        ];
        if with_loaded_tool {
            tools.insert(
                0,
                json!({
                    "name": "CronList",
                    "description": "List scheduled jobs.",
                    "defer_loading": true,
                    "input_schema": {"type": "object", "properties": {}}
                }),
            );
        }
        Value::Array(tools)
    }

    fn tool_search_request(after_load: bool, search_result: Value) -> MessagesRequest {
        let mut messages =
            vec![json!({"role": "user", "content": "How many cron jobs are scheduled?"})];
        if after_load {
            messages.push(json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "call_search", "name": "ToolSearch",
                 "input": {"query": "select:CronList", "max_results": 1}}
            ]}));
            messages.push(json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "call_search", "content": search_result}
            ]}));
        }
        serde_json::from_value(json!({
            "model": "gpt-5.6-sol",
            "system": [{"type": "text", "text": "You are a coding agent."}],
            "messages": messages,
            "tools": claude_code_tools(after_load),
        }))
        .unwrap()
    }

    fn cron_reference() -> Value {
        json!([{"type": "tool_reference", "tool_name": "CronList"}])
    }

    fn lane_opts(use_responses_lite: bool) -> TranslateOptions {
        TranslateOptions {
            session_id: Some("s".into()),
            service_tier: None,
            model: "gpt-5.6-sol".to_string(),
            use_responses_lite,
        }
    }

    fn items_json(out: &ResponsesRequest) -> Vec<Value> {
        out.input
            .iter()
            .map(|item| serde_json::to_value(item).unwrap())
            .collect()
    }

    #[test]
    fn tool_search_keeps_lite_tools_head_identical_across_a_load() {
        let before =
            translate_request(&tool_search_request(false, Value::Null), lane_opts(true)).unwrap();
        let after = translate_request(
            &tool_search_request(true, cron_reference()),
            lane_opts(true),
        )
        .unwrap();
        let before_items = items_json(&before);
        let after_items = items_json(&after);

        // The additional_tools head and the instructions do not change, so the
        // prompt prefix cached for the first request still matches.
        assert_eq!(before_items[0], after_items[0]);
        assert_eq!(before_items[1], after_items[1]);

        let head = before_items[0]["tools"].as_array().unwrap();
        let kinds: Vec<(&str, Option<&str>)> = head
            .iter()
            .map(|tool| (tool["type"].as_str().unwrap(), tool["name"].as_str()))
            .collect();
        assert_eq!(
            kinds,
            vec![
                ("function", Some("Read")),
                ("tool_search", None),
                ("function", Some("DeferredToolPlaceholder")),
            ]
        );
        assert_eq!(head[1]["execution"], "client");
        assert_eq!(
            head[1]["description"],
            "Fetches full schema definitions for deferred tools."
        );
        assert!(head[1]["parameters"]["properties"]["query"].is_object());
        assert!(head.iter().all(|tool| tool.get("defer_loading").is_none()));

        let tail = &after_items[before_items.len()..];
        assert_eq!(
            tail[0],
            json!({
                "type": "tool_search_call",
                "call_id": "call_search",
                "execution": "client",
                "status": "completed",
                "arguments": {"max_results": 1, "query": "select:CronList"}
            })
        );
        assert_eq!(tail[1]["type"], "tool_search_output");
        assert_eq!(tail[1]["call_id"], "call_search");
        assert_eq!(tail[1]["status"], "completed");
        assert_eq!(tail[1]["execution"], "client");
        assert_eq!(
            tail[1]["tools"],
            json!([{
                "type": "function",
                "name": "CronList",
                "description": "List scheduled jobs.",
                "parameters": {"type": "object", "properties": {}},
                "strict": false,
                "defer_loading": true
            }])
        );
        assert_eq!(tail.len(), 2);

        let serialized = serde_json::to_string(&after).unwrap();
        assert!(!serialized.contains("unsupported content block omitted"));
        assert!(!serialized.contains("\"name\":\"ToolSearch\""));
    }

    #[test]
    fn tool_search_keeps_full_lane_tools_identical_across_a_load() {
        let before =
            translate_request(&tool_search_request(false, Value::Null), lane_opts(false)).unwrap();
        let after = translate_request(
            &tool_search_request(true, cron_reference()),
            lane_opts(false),
        )
        .unwrap();

        assert_eq!(
            serde_json::to_value(&before.tools).unwrap(),
            serde_json::to_value(&after.tools).unwrap()
        );
        assert_eq!(before.instructions, after.instructions);
        let types: Vec<String> = items_json(&after)
            .iter()
            .map(|item| item["type"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            types,
            vec!["message", "tool_search_call", "tool_search_output"]
        );
    }

    #[test]
    fn tool_search_result_without_references_loads_no_tools() {
        let req = tool_search_request(
            true,
            json!([{"type": "text", "text": "No matching deferred tools found"}]),
        );
        let out = translate_request(&req, lane_opts(true)).unwrap();
        let items = items_json(&out);
        let output = items
            .iter()
            .find(|item| item["type"] == "tool_search_output")
            .expect("search output");
        assert_eq!(output["tools"], json!([]));

        // Nothing was loaded, so the deferred tool Claude Code still lists stays
        // callable from the head.
        let head_names: Vec<&str> = items[0]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect();
        assert!(head_names.contains(&"CronList"));
    }

    #[test]
    fn deferred_tool_without_a_search_in_history_stays_in_head() {
        // After a compaction Claude Code keeps the loaded tool in `tools` but the
        // search that loaded it is no longer in the history.
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-sol",
            "messages": [{"role": "user", "content": "Summary of earlier work. Continue."}],
            "tools": claude_code_tools(true),
        }))
        .unwrap();
        let out = translate_request(&req, lane_opts(true)).unwrap();
        let items = items_json(&out);
        let cron = items[0]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "CronList")
            .expect("CronList in head");
        assert!(cron.get("defer_loading").is_none());
    }

    #[test]
    fn without_tool_search_tool_deferred_loading_translates_as_before() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-sol",
            "messages": [
                {"role": "user", "content": "go"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "call_search", "name": "ToolSearch", "input": {"query": "cron"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "call_search", "content": cron_reference()}
                ]}
            ],
            "tools": [
                {"name": "CronList", "defer_loading": true, "input_schema": {"type": "object"}}
            ],
        }))
        .unwrap();
        let out = translate_request(&req, lane_opts(false)).unwrap();
        let items = items_json(&out);
        assert_eq!(items[1]["type"], "function_call");
        assert_eq!(items[1]["name"], "ToolSearch");
        assert_eq!(items[2]["type"], "function_call_output");
        assert_eq!(
            items[2]["output"],
            "[unsupported content block omitted: tool_reference]"
        );
        let tools = serde_json::to_value(&out.tools).unwrap();
        assert_eq!(tools[0]["name"], "CronList");
    }

    fn directed_tool_choice(mut req: MessagesRequest, name: &str) -> MessagesRequest {
        req.extra.insert(
            "tool_choice".to_string(),
            json!({"type": "tool", "name": name}),
        );
        req
    }

    /// The tools head as the lane carries it: the full lane's `tools` field, the
    /// lite lane's `additional_tools` item.
    fn head_tools(out: &ResponsesRequest) -> Vec<Value> {
        match out.tools {
            Some(ref tools) => tools
                .iter()
                .map(|tool| serde_json::to_value(tool).unwrap())
                .collect(),
            None => items_json(out)
                .into_iter()
                .find(|item| item["type"] == "additional_tools")
                .and_then(|item| item["tools"].as_array().cloned())
                .unwrap_or_default(),
        }
    }

    fn head_tool_names(out: &ResponsesRequest) -> Vec<String> {
        head_tools(out)
            .iter()
            .filter_map(|tool| tool["name"].as_str().map(str::to_string))
            .collect()
    }

    /// The prompt prefix a cache hit depends on: the tools head plus the system
    /// text, wherever the lane puts them.
    fn head_prefix(out: &ResponsesRequest) -> String {
        let tools = serde_json::to_string(&head_tools(out)).unwrap();
        let text = match out.instructions {
            Some(ref instructions) => instructions.clone(),
            None => items_json(out)
                .into_iter()
                .find(|item| item["type"] == "message" && item["role"] == "developer")
                .map(|item| item["content"].to_string())
                .unwrap_or_default(),
        };
        format!("{tools}\n{text}")
    }

    /// The tool names each `tool_search_output` carries, outputs in wire order.
    fn search_output_tool_names(out: &ResponsesRequest) -> Vec<Vec<String>> {
        items_json(out)
            .into_iter()
            .filter(|item| item["type"] == "tool_search_output")
            .map(|item| {
                item["tools"]
                    .as_array()
                    .expect("search output tools")
                    .iter()
                    .map(|tool| tool["name"].as_str().expect("tool name").to_string())
                    .collect()
            })
            .collect()
    }

    fn function_tool_choice(out: &ResponsesRequest) -> Option<String> {
        match out.tool_choice {
            Some(ResponsesToolChoice::Function {
                ref r#type,
                ref name,
            }) if r#type.as_str() == "function" => Some(name.clone()),
            _ => None,
        }
    }

    #[test]
    fn tool_search_directed_tool_choice_names_a_loaded_deferred_tool() {
        for lite in [true, false] {
            let req = directed_tool_choice(tool_search_request(true, cron_reference()), "CronList");
            let out = translate_request(&req, lane_opts(lite)).unwrap();

            // Directing the model at a tool a search loaded stays a plain function
            // choice by name on either lane.
            assert_eq!(
                function_tool_choice(&out).as_deref(),
                Some("CronList"),
                "lite={lite}: {:?}",
                serde_json::to_value(&out.tool_choice).unwrap()
            );
            assert_eq!(
                serde_json::to_value(&out.tool_choice).unwrap(),
                json!({"type": "function", "name": "CronList"}),
                "lite={lite}"
            );

            // Its schema is on the wire where a loaded deferred tool lives: in the
            // search output, not in the head.
            let loaded = items_json(&out)
                .into_iter()
                .find(|item| item["type"] == "tool_search_output")
                .expect("search output");
            assert_eq!(
                loaded["tools"],
                json!([{
                    "type": "function",
                    "name": "CronList",
                    "description": "List scheduled jobs.",
                    "parameters": {"type": "object", "properties": {}},
                    "strict": false,
                    "defer_loading": true
                }]),
                "lite={lite}"
            );
            assert!(
                !head_tool_names(&out).contains(&"CronList".to_string()),
                "lite={lite}"
            );
        }
    }

    #[test]
    fn tool_search_directed_tool_choice_is_forwarded_when_nothing_loaded_it() {
        // A deferred tool no search in this history loaded stays in the head, and
        // the directed choice names it there.
        let mut req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-sol",
            "messages": [{"role": "user", "content": "How many cron jobs are scheduled?"}],
            "tools": claude_code_tools(true),
        }))
        .unwrap();
        req = directed_tool_choice(req, "CronList");
        for lite in [true, false] {
            let out = translate_request(&req, lane_opts(lite)).unwrap();
            assert_eq!(
                function_tool_choice(&out).as_deref(),
                Some("CronList"),
                "lite={lite}"
            );
            assert!(
                head_tool_names(&out).contains(&"CronList".to_string()),
                "lite={lite}"
            );
            assert!(search_output_tool_names(&out).is_empty(), "lite={lite}");
        }

        // A name the request carries no tool for is forwarded verbatim too:
        // `map_tool_choice` runs no presence check, and only the hosted
        // web_search branch is special-cased (see the allowed_tools reset above,
        // which rewrites an unregistered web search choice to auto).
        let unknown =
            directed_tool_choice(tool_search_request(true, cron_reference()), "NeverListed");
        let out = translate_request(&unknown, lane_opts(true)).unwrap();
        assert_eq!(function_tool_choice(&out).as_deref(), Some("NeverListed"));
        // The choice is the only place that name occurs: no schema is invented
        // for it, in the head or in a search output.
        assert!(!head_tool_names(&out).contains(&"NeverListed".to_string()));
        assert!(
            search_output_tool_names(&out)
                .iter()
                .all(|names| !names.contains(&"NeverListed".to_string()))
        );
    }

    const CATALOG_SIZE: usize = 500;

    fn catalog_tool_name(index: usize) -> String {
        format!("Mcp_Tool_{index:03}")
    }

    /// The stable text catalog of every deferred tool: what the model reads to
    /// know the tools exist without their schemas being in the prompt.
    fn catalog_text() -> String {
        let mut text = String::from("Available tools (load them with ToolSearch):\n");
        for index in 0..CATALOG_SIZE {
            let name = catalog_tool_name(index);
            text.push_str(&format!("- {name}: tool number {index}\n"));
        }
        text
    }

    /// A request out of a catalog of `CATALOG_SIZE` deferred tools after the
    /// given searches, each one a list of tool indices its result referenced in
    /// that order. Claude Code appends a loaded tool to `tools` in name order
    /// with `defer_loading` still set, so the loaded tools lead the array here.
    fn catalog_request(searches: &[Vec<usize>]) -> MessagesRequest {
        let mut loaded: Vec<usize> = searches.iter().flatten().copied().collect();
        loaded.sort_unstable();
        let mut tools: Vec<Value> = loaded
            .iter()
            .map(|index| {
                json!({
                    "name": catalog_tool_name(*index),
                    "description": format!("tool number {index}"),
                    "defer_loading": true,
                    "input_schema": {"type": "object", "properties": {"index": {"type": "number"}}}
                })
            })
            .collect();
        tools.extend(claude_code_tools(false).as_array().unwrap().iter().cloned());

        let mut messages = vec![json!({"role": "user", "content": "Work through the catalog."})];
        for (turn, references) in searches.iter().enumerate() {
            let call_id = format!("call_search_{turn}");
            let query = references
                .iter()
                .map(|index| catalog_tool_name(*index))
                .collect::<Vec<_>>()
                .join(",");
            messages.push(json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": call_id, "name": "ToolSearch",
                 "input": {"query": format!("select:{query}"), "max_results": references.len()}}
            ]}));
            let blocks: Vec<Value> = references
                .iter()
                .map(|index| json!({"type": "tool_reference", "tool_name": catalog_tool_name(*index)}))
                .collect();
            messages.push(json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": call_id, "content": blocks}
            ]}));
        }

        serde_json::from_value(json!({
            "model": "gpt-5.6-sol",
            "system": [{"type": "text", "text": catalog_text()}],
            "messages": messages,
            "tools": tools,
        }))
        .unwrap()
    }

    #[test]
    fn tool_search_large_catalog_appends_only_the_loaded_schemas() {
        // Neither the searches nor the references inside one result are in name
        // order, so an implementation that sorted by name would show here.
        let searches = vec![vec![137, 4], vec![250], vec![499, 12]];
        let expected: Vec<Vec<String>> = searches
            .iter()
            .map(|references| references.iter().map(|i| catalog_tool_name(*i)).collect())
            .collect();

        for lite in [true, false] {
            let before = translate_request(&catalog_request(&[]), lane_opts(lite)).unwrap();
            let after = translate_request(&catalog_request(&searches), lane_opts(lite)).unwrap();

            assert!(
                head_prefix(&before).contains(&catalog_tool_name(CATALOG_SIZE - 1)),
                "lite={lite}: the catalog text is the fixture's point"
            );
            // Five loads out of five hundred leave the cached prefix byte for byte.
            assert_eq!(head_prefix(&before), head_prefix(&after), "lite={lite}");

            let head = head_tools(&after);
            let kinds: Vec<(&str, Option<&str>)> = head
                .iter()
                .map(|tool| (tool["type"].as_str().unwrap(), tool["name"].as_str()))
                .collect();
            assert_eq!(
                kinds,
                vec![
                    ("function", Some("Read")),
                    ("tool_search", None),
                    ("function", Some("DeferredToolPlaceholder")),
                ],
                "lite={lite}"
            );

            // Only the referenced schemas travel: one output per search, searches
            // in history order, references in the order the result listed them.
            assert_eq!(search_output_tool_names(&after), expected, "lite={lite}");

            // The 495 nothing referenced never reach the wire as a schema; their
            // names live in the catalog text only.
            let wire = serde_json::to_string(&after).unwrap();
            for index in 0..CATALOG_SIZE {
                let name = catalog_tool_name(index);
                let is_loaded = searches.iter().any(|refs| refs.contains(&index));
                assert_eq!(
                    wire.contains(&format!("\"name\":\"{name}\"")),
                    is_loaded,
                    "lite={lite}: {name}"
                );
                assert!(wire.contains(&format!("- {name}: tool number {index}")));
            }

            // Same request in, same bytes out: nothing on this path iterates a
            // hash map.
            let again = translate_request(&catalog_request(&searches), lane_opts(lite)).unwrap();
            assert_eq!(wire, serde_json::to_string(&again).unwrap(), "lite={lite}");
        }
    }
}
