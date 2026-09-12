//! Deferred tool loading mapped onto the Codex backend's native tool search.
//!
//! Claude Code keeps most tools out of the prompt: they arrive in `tools` with
//! `defer_loading: true`, the model loads one by calling Claude Code's
//! `ToolSearch` function, and the result carries `tool_reference` blocks while
//! the loaded tool is appended to `tools`. Anthropic expands the reference in
//! place, so the cached prompt prefix never changes.
//!
//! The Codex backend has the same mechanism under different names: a
//! `tool_search` tool with client execution, a `tool_search_call` output item,
//! and a `tool_search_output` input item that carries the loaded tool specs at
//! the point of the search. Translating to it keeps the tools head (the
//! lite-lane `additional_tools` item or the full-lane `tools` field) identical
//! before and after a load. Putting the loaded tool into the head instead
//! changes the first bytes of the prompt and costs a full prompt-cache miss on
//! every load.
//!
//! Everything here is derived from the request alone, so the same Claude Code
//! history always translates to the same bytes.

use std::collections::{BTreeMap, HashSet};

use serde_json::{Value, json};

use crate::anthropic::schema::MessagesRequest;
use crate::providers::translate_shared::{ContentBlock, normalize_content};

/// Name of Claude Code's client-side tool search function.
pub const CLAUDE_CODE_TOOL_SEARCH_NAME: &str = "ToolSearch";

/// `execution` value of a tool search the client runs.
pub const TOOL_SEARCH_EXECUTION: &str = "client";

/// Status recorded on replayed tool search items.
pub const TOOL_SEARCH_STATUS: &str = "completed";

/// What the request translator needs to know about tool search in one request.
#[derive(Debug, Default, Clone)]
pub struct ToolSearchPlan {
    /// Ids of Claude Code `ToolSearch` tool uses in the history.
    pub call_ids: HashSet<String>,
    /// Deferred tools from the request's `tools`, by name, as Claude Code sent them.
    pub deferred_tools: BTreeMap<String, Value>,
    /// Deferred tools a `ToolSearch` result in the history has loaded. These
    /// are carried by `tool_search_output` items and stay out of the tools head.
    pub loaded: HashSet<String>,
}

impl ToolSearchPlan {
    pub fn is_tool_search_call(&self, tool_use_id: &str) -> bool {
        self.call_ids.contains(tool_use_id)
    }

    /// Whether a tool belongs in the tools head. Deferred tools loaded by a
    /// search in this history are delivered by that search's output instead;
    /// deferred tools no search references (the placeholder, or a tool whose
    /// search was compacted away) stay in the head so the model can still call them.
    pub fn keeps_in_head(&self, tool: &Value) -> bool {
        if !is_deferred(tool) {
            return true;
        }
        let name = tool.get("name").and_then(Value::as_str).unwrap_or("");
        !self.loaded.contains(name)
    }

    /// Deferred tools referenced by one `ToolSearch` result, in reference order,
    /// without duplicates. References to tools the request does not carry as
    /// deferred are skipped: a non-deferred tool is already in the head.
    pub fn referenced_deferred_tools<'a>(&'a self, content: &Value) -> Vec<&'a Value> {
        let mut seen = HashSet::new();
        referenced_tool_names(content)
            .into_iter()
            .filter(|name| seen.insert(name.clone()))
            .filter_map(|name| self.deferred_tools.get(&name))
            .collect()
    }
}

/// Whether a request tool is Claude Code's `ToolSearch` function.
pub fn is_claude_code_tool_search(tool: &Value) -> bool {
    tool.get("name").and_then(Value::as_str) == Some(CLAUDE_CODE_TOOL_SEARCH_NAME)
        && tool
            .get("input_schema")
            .and_then(|schema| schema.get("properties"))
            .and_then(|properties| properties.get("query"))
            .is_some()
}

pub fn is_deferred(tool: &Value) -> bool {
    tool.get("defer_loading").and_then(Value::as_bool) == Some(true)
}

/// Build the plan for a request, or `None` when the request has no
/// `ToolSearch` tool; the translation then stays as it was.
pub fn plan(req: &MessagesRequest) -> Option<ToolSearchPlan> {
    let tools = req.extra.get("tools")?.as_array()?;
    if !tools.iter().any(is_claude_code_tool_search) {
        return None;
    }

    let deferred_tools: BTreeMap<String, Value> = tools
        .iter()
        .filter(|tool| is_deferred(tool))
        .filter_map(|tool| {
            let name = tool.get("name").and_then(Value::as_str)?;
            Some((name.to_string(), tool.clone()))
        })
        .collect();

    let mut plan = ToolSearchPlan {
        deferred_tools,
        ..ToolSearchPlan::default()
    };

    for msg in &req.messages {
        for block in normalize_content(&msg.content, Value::Null) {
            match block {
                ContentBlock::ToolUse { id, name, .. }
                    if msg.role == "assistant" && name == CLAUDE_CODE_TOOL_SEARCH_NAME =>
                {
                    plan.call_ids.insert(id);
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } if msg.role == "user" && plan.call_ids.contains(&tool_use_id) => {
                    for name in referenced_tool_names(&content) {
                        if plan.deferred_tools.contains_key(&name) {
                            plan.loaded.insert(name);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    Some(plan)
}

/// Names from the `tool_reference` blocks of a tool result's content.
pub fn referenced_tool_names(content: &Value) -> Vec<String> {
    let Some(blocks) = content.as_array() else {
        return Vec::new();
    };
    blocks
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_reference"))
        .filter_map(|block| block.get("tool_name").and_then(Value::as_str))
        .map(str::to_string)
        .collect()
}

/// Rewrite a `tool_search_call` output item event into the `function_call`
/// events the stream translators already handle, named as Claude Code's
/// `ToolSearch`. The backend sends the item once with empty arguments when it
/// starts and once complete when it is done, with no argument deltas, so the
/// done event becomes one arguments delta followed by the done item.
///
/// Returns `None` for every other event.
pub fn normalize_stream_event(payload: &Value) -> Option<Vec<Value>> {
    let kind = payload.get("type").and_then(Value::as_str)?;
    let item = payload.get("item")?;
    if item.get("type").and_then(Value::as_str) != Some("tool_search_call") {
        return None;
    }
    let output_index = payload.get("output_index").cloned().unwrap_or(json!(0));
    let call_id = item.get("call_id").cloned().unwrap_or(json!(""));

    match kind {
        "response.output_item.added" => Some(vec![json!({
            "type": "response.output_item.added",
            "output_index": output_index,
            "item": {
                "type": "function_call",
                "call_id": call_id,
                "name": CLAUDE_CODE_TOOL_SEARCH_NAME,
                "arguments": "",
            }
        })]),
        "response.output_item.done" => {
            let arguments = item
                .get("arguments")
                .filter(|arguments| arguments.is_object())
                .map(|arguments| serde_json::to_string(arguments).unwrap_or_default())
                .unwrap_or_else(|| "{}".to_string());
            Some(vec![
                json!({
                    "type": "response.function_call_arguments.delta",
                    "output_index": output_index,
                    "delta": arguments,
                }),
                json!({
                    "type": "response.output_item.done",
                    "output_index": output_index,
                    "item": {
                        "type": "function_call",
                        "call_id": call_id,
                        "name": CLAUDE_CODE_TOOL_SEARCH_NAME,
                        "arguments": arguments,
                        "status": item.get("status").cloned().unwrap_or(json!(TOOL_SEARCH_STATUS)),
                    }
                }),
            ])
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(value: Value) -> MessagesRequest {
        serde_json::from_value(value).expect("valid messages request")
    }

    fn tool_search_tool() -> Value {
        json!({
            "name": "ToolSearch",
            "description": "Fetches full schema definitions for deferred tools.",
            "input_schema": {
                "type": "object",
                "properties": {"query": {"type": "string"}, "max_results": {"type": "number"}},
                "required": ["query", "max_results"]
            }
        })
    }

    #[test]
    fn no_plan_without_tool_search_tool() {
        let req = request(json!({
            "model": "gpt-5.6-sol",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"name": "Read", "input_schema": {"type": "object"}}]
        }));
        assert!(plan(&req).is_none());
    }

    #[test]
    fn plan_tracks_search_calls_and_loaded_deferred_tools() {
        let req = request(json!({
            "model": "gpt-5.6-sol",
            "tools": [
                tool_search_tool(),
                {"name": "DeferredToolPlaceholder", "defer_loading": true, "input_schema": {"type": "object"}},
                {"name": "CronList", "defer_loading": true, "input_schema": {"type": "object"}},
                {"name": "Read", "input_schema": {"type": "object"}}
            ],
            "messages": [
                {"role": "user", "content": "load cron"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "call_s", "name": "ToolSearch", "input": {"query": "select:CronList,Read,Missing", "max_results": 3}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "call_s", "content": [
                        {"type": "tool_reference", "tool_name": "CronList"},
                        {"type": "tool_reference", "tool_name": "Read"},
                        {"type": "tool_reference", "tool_name": "Missing"}
                    ]}
                ]}
            ]
        }));
        let plan = plan(&req).expect("plan");
        assert!(plan.is_tool_search_call("call_s"));
        assert_eq!(plan.loaded, HashSet::from(["CronList".to_string()]));

        let placeholder = json!({"name": "DeferredToolPlaceholder", "defer_loading": true});
        let cron = json!({"name": "CronList", "defer_loading": true});
        let read = json!({"name": "Read"});
        assert!(plan.keeps_in_head(&placeholder));
        assert!(!plan.keeps_in_head(&cron));
        assert!(plan.keeps_in_head(&read));

        let content = json!([
            {"type": "tool_reference", "tool_name": "CronList"},
            {"type": "tool_reference", "tool_name": "CronList"},
            {"type": "tool_reference", "tool_name": "Read"}
        ]);
        let names: Vec<&str> = plan
            .referenced_deferred_tools(&content)
            .into_iter()
            .filter_map(|tool| tool.get("name").and_then(Value::as_str))
            .collect();
        assert_eq!(names, vec!["CronList"]);
    }

    #[test]
    fn references_in_other_tool_results_do_not_load_tools() {
        let req = request(json!({
            "model": "gpt-5.6-sol",
            "tools": [
                tool_search_tool(),
                {"name": "CronList", "defer_loading": true, "input_schema": {"type": "object"}}
            ],
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "call_b", "name": "Bash", "input": {"command": "true"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "call_b", "content": [
                        {"type": "tool_reference", "tool_name": "CronList"}
                    ]}
                ]}
            ]
        }));
        let plan = plan(&req).expect("plan");
        assert!(plan.loaded.is_empty());
    }

    #[test]
    fn added_tool_search_call_becomes_function_call_start() {
        let events = normalize_stream_event(&json!({
            "type": "response.output_item.added",
            "output_index": 2,
            "item": {"id": "tsc_1", "type": "tool_search_call", "status": "in_progress",
                     "arguments": {}, "call_id": "call_s", "execution": "client"}
        }))
        .expect("normalized");
        assert_eq!(
            events,
            vec![json!({
                "type": "response.output_item.added",
                "output_index": 2,
                "item": {"type": "function_call", "call_id": "call_s", "name": "ToolSearch", "arguments": ""}
            })]
        );
    }

    #[test]
    fn done_tool_search_call_becomes_arguments_delta_and_done() {
        let events = normalize_stream_event(&json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {"type": "tool_search_call", "status": "completed", "call_id": "call_s",
                     "execution": "client", "arguments": {"query": "select:CronList", "max_results": 1}}
        }))
        .expect("normalized");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["type"], "response.function_call_arguments.delta");
        assert_eq!(events[0]["output_index"], 0);
        let delta: Value = serde_json::from_str(events[0]["delta"].as_str().unwrap()).unwrap();
        assert_eq!(delta, json!({"query": "select:CronList", "max_results": 1}));
        assert_eq!(events[1]["type"], "response.output_item.done");
        assert_eq!(events[1]["item"]["type"], "function_call");
        assert_eq!(events[1]["item"]["name"], "ToolSearch");
        assert_eq!(events[1]["item"]["call_id"], "call_s");
        assert_eq!(events[1]["item"]["arguments"], events[0]["delta"]);
    }

    #[test]
    fn other_events_are_left_alone() {
        assert!(
            normalize_stream_event(&json!({
                "type": "response.output_item.done",
                "item": {"type": "function_call", "call_id": "c", "name": "Read", "arguments": "{}"}
            }))
            .is_none()
        );
        assert!(normalize_stream_event(&json!({"type": "response.completed"})).is_none());
    }
}
