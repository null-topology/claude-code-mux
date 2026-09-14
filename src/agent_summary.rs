//! Claude Code's background-agent status line, answered without an upstream call.
//!
//! While a subagent runs, Claude Code re-sends that subagent's whole context
//! every half minute and asks for a three-to-five word label for its progress
//! line. The label never reaches the model's actual work, but on a subscription
//! backend the request costs a full context pass: in a measured capture a
//! quarter of all Codex requests were these labels, each carrying tens of
//! thousands of tokens. The proxy recognises the prompt and answers it from the
//! transcript instead, so the label still appears and no tokens are spent.
//!
//! `CCP_AGENT_SUMMARY=upstream` sends them to the model again.

use crate::anthropic::schema::MessagesRequest;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

/// The instruction Claude Code appends when it wants a progress label. Matching
/// the prompt text is the only reliable marker: these requests otherwise look
/// like an ordinary turn of the subagent, with its tools and its history.
pub const SUMMARY_PROMPT_MARKER: &str = "Describe your most recent action in 3-5 words";

/// Where a label goes when it is not answered locally: the provider's junior
/// model that still holds the context the subagent has grown to. Anthropic's is
/// Sonnet rather than Haiku, whose 200k window would leave a long-running
/// subagent without a status line; on Codex, Luna holds a million tokens.
/// A provider with no entry here keeps the request's own model.
pub const ANTHROPIC_SUMMARY_MODEL: &str = "claude-sonnet-5";
pub const CODEX_SUMMARY_MODEL: &str = "gpt-5.6-luna";

pub fn summary_model_for(provider: &str) -> Option<&'static str> {
    match provider {
        "anthropic" => Some(ANTHROPIC_SUMMARY_MODEL),
        "codex" => Some(CODEX_SUMMARY_MODEL),
        _ => None,
    }
}

/// Point a label request at the cheap model and stop it from reasoning about a
/// three-word answer.
pub fn apply_summary_route(body: &mut MessagesRequest, model: &str) {
    body.model = Some(model.to_string());
    body.bypass_provider_model_override = true;
    body.extra
        .insert("output_config".to_string(), json!({"effort": "low"}));
}

/// Claude Code puts the label prompt in the closing text block of the last user
/// message, on its own. Other callers quote a subagent's transcript into the
/// message they send, so the same sentence turns up inside a much longer block
/// that asks for something else; matching it there answers the wrong request.
/// The prompt has to open the block, not merely appear somewhere in it.
pub fn is_agent_summary_request(body: &MessagesRequest) -> bool {
    let Some(last) = body.messages.last() else {
        return false;
    };
    if last.role != "user" {
        return false;
    }
    last_text_block(&last.content)
        .is_some_and(|text| text.trim_start().starts_with(SUMMARY_PROMPT_MARKER))
}

/// A label for what the agent is doing, taken from the last tool call it made.
pub fn summary_text(body: &MessagesRequest) -> String {
    for message in body.messages.iter().rev() {
        if message.role != "assistant" {
            continue;
        }
        let Some(blocks) = message.content.as_array() else {
            continue;
        };
        for block in blocks.iter().rev() {
            if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                continue;
            }
            let name = block
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("a tool");
            let input = block.get("input");
            return label_for_tool(name, input);
        }
    }
    "Working on the task".to_string()
}

fn label_for_tool(name: &str, input: Option<&Value>) -> String {
    let target = |key: &str| {
        input
            .and_then(|input| input.get(key))
            .and_then(Value::as_str)
            .map(short_target)
    };
    match name {
        "Read" | "NotebookRead" => match target("file_path") {
            Some(file) => format!("Reading {file}"),
            None => "Reading a file".to_string(),
        },
        "Edit" | "Write" | "NotebookEdit" => match target("file_path") {
            Some(file) => format!("Editing {file}"),
            None => "Editing a file".to_string(),
        },
        "Bash" | "BashOutput" => "Running a shell command".to_string(),
        "Grep" | "Glob" => match target("pattern") {
            Some(pattern) => format!("Searching for {pattern}"),
            None => "Searching the code".to_string(),
        },
        "WebFetch" | "WebSearch" => "Searching the web".to_string(),
        "Agent" | "Task" => "Delegating to a subagent".to_string(),
        "TodoWrite" => "Updating the plan".to_string(),
        "Skill" => "Running a skill".to_string(),
        other => format!("Using {}", short_target(other)),
    }
}

/// Keep a label short: the file name rather than its path, and never a
/// sentence's worth of text.
fn short_target(value: &str) -> String {
    let tail = value.rsplit('/').next().unwrap_or(value);
    let tail: String = tail.chars().take(40).collect();
    tail.split_whitespace()
        .take(3)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Answer the label request in the shape the client asked for. Usage is
/// reported as zero input, because nothing was sent upstream.
pub fn local_response(body: &MessagesRequest, text: &str) -> (Response, u64) {
    let model = body.model.clone().unwrap_or_default();
    let output_tokens = text.split_whitespace().count().max(1) as u64;
    let message_id = format!("msg_{}", uuid::Uuid::new_v4().simple());
    if !body.stream {
        let payload = json!({
            "id": message_id,
            "type": "message",
            "role": "assistant",
            "model": model,
            "content": [{"type": "text", "text": text}],
            "stop_reason": "end_turn",
            "stop_sequence": null,
            "usage": {"input_tokens": 0, "output_tokens": output_tokens}
        });
        return (axum::Json(payload).into_response(), output_tokens);
    }

    let mut sse = Vec::new();
    for (event, payload) in [
        (
            "message_start",
            json!({"type": "message_start", "message": {
                "id": message_id, "type": "message", "role": "assistant", "model": model,
                "content": [], "stop_reason": null, "stop_sequence": null,
                "usage": {"input_tokens": 0, "output_tokens": 0}
            }}),
        ),
        (
            "content_block_start",
            json!({"type": "content_block_start", "index": 0,
                   "content_block": {"type": "text", "text": ""}}),
        ),
        (
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "text_delta", "text": text}}),
        ),
        (
            "content_block_stop",
            json!({"type": "content_block_stop", "index": 0}),
        ),
        (
            "message_delta",
            json!({"type": "message_delta",
                   "delta": {"stop_reason": "end_turn", "stop_sequence": null},
                   "usage": {"input_tokens": 0, "output_tokens": output_tokens}}),
        ),
        ("message_stop", json!({"type": "message_stop"})),
    ] {
        sse.extend_from_slice(&crate::anthropic::sse::encode_sse_event(
            Some(event),
            &payload.to_string(),
        ));
    }
    let response = (
        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
        sse,
    )
        .into_response();
    (response, output_tokens)
}

/// The closing text of a message: the whole thing when the content is a plain
/// string, otherwise the last text block, past the tool results that precede it.
fn last_text_block(content: &Value) -> Option<&str> {
    match content {
        Value::String(text) => Some(text.as_str()),
        Value::Array(blocks) => blocks
            .iter()
            .rev()
            .find_map(|block| block.get("text").and_then(Value::as_str)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request(last_role: &str, last_text: &str, tool: Option<Value>) -> MessagesRequest {
        let mut messages = vec![];
        if let Some(tool) = tool {
            messages.push(json!({"role": "assistant", "content": [tool]}));
        }
        messages.push(json!({
            "role": last_role,
            "content": [{"type": "text", "text": last_text}]
        }));
        serde_json::from_value(json!({
            "model": "gpt-6-astra",
            "max_tokens": 64000,
            "stream": true,
            "messages": messages
        }))
        .unwrap()
    }

    #[test]
    fn recognises_the_progress_label_prompt() {
        let asking = request(
            "user",
            "Describe your most recent action in 3-5 words using present tense (-ing).",
            None,
        );
        assert!(is_agent_summary_request(&asking));

        // An ordinary turn, and the same text from the wrong side.
        assert!(!is_agent_summary_request(&request(
            "user",
            "Please review this diff",
            None
        )));
        assert!(!is_agent_summary_request(&request(
            "assistant",
            "Describe your most recent action in 3-5 words",
            None
        )));
        assert!(!is_agent_summary_request(&request("user", "", None)));
    }

    /// Other callers quote a transcript into the message they send, and a
    /// transcript of a subagent contains the label prompt verbatim. Answering
    /// one of those with a three-word label gives the caller nothing it can
    /// parse. The prompt has to be what the message asks for, not a line
    /// somewhere inside it.
    #[test]
    fn the_prompt_must_be_the_message_rather_than_a_line_quoted_inside_it() {
        let quoted = format!(
            "Here is the transcript to judge:\n\
             assistant: I will read a file\n\
             user: {SUMMARY_PROMPT_MARKER} using present tense (-ing).\n\
             assistant: Reading server.rs\n\
             Answer with a verdict."
        );
        assert!(!is_agent_summary_request(&request("user", &quoted, None)));

        // The genuine request is the prompt itself, and stays recognised even
        // when the client indents it or adds its own trailing sentence.
        assert!(is_agent_summary_request(&request(
            "user",
            "  Describe your most recent action in 3-5 words using present tense (-ing).",
            None
        )));

        // Claude Code sends the prompt as the last block of a message that also
        // carries the tool result the label should describe.
        let with_tool_result: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-6-astra",
            "max_tokens": 64000,
            "stream": true,
            "messages": [{"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "..."},
                {"type": "text", "text": SUMMARY_PROMPT_MARKER}
            ]}]
        }))
        .unwrap();
        assert!(is_agent_summary_request(&with_tool_result));
    }

    #[test]
    fn each_provider_gets_a_junior_model_that_still_holds_the_context() {
        // Sonnet, not Haiku: a subagent past 200k tokens must keep its label.
        assert_eq!(summary_model_for("anthropic"), Some("claude-sonnet-5"));
        assert_eq!(summary_model_for("codex"), Some("gpt-5.6-luna"));
        // Unknown provider: keep the request's own model rather than guess.
        assert_eq!(summary_model_for("kimi"), None);
    }

    #[test]
    fn routing_a_label_pins_the_cheap_model_and_lowest_effort() {
        let mut body = request("user", SUMMARY_PROMPT_MARKER, None);
        body.extra
            .insert("output_config".to_string(), json!({"effort": "xhigh"}));
        apply_summary_route(&mut body, CODEX_SUMMARY_MODEL);

        assert_eq!(body.model.as_deref(), Some(CODEX_SUMMARY_MODEL));
        assert_eq!(body.extra["output_config"], json!({"effort": "low"}));
        // The configured provider override must not pull it back to a big model.
        assert!(body.bypass_provider_model_override);
    }

    #[test]
    fn labels_come_from_the_last_tool_call() {
        let read = request(
            "user",
            SUMMARY_PROMPT_MARKER,
            Some(json!({
                "type": "tool_use", "id": "t1", "name": "Read",
                "input": {"file_path": "/Users/leo/PROJECTS/proxy/src/monitor.rs"}
            })),
        );
        assert_eq!(summary_text(&read), "Reading monitor.rs");

        let bash = request(
            "user",
            SUMMARY_PROMPT_MARKER,
            Some(json!({"type": "tool_use", "id": "t2", "name": "Bash", "input": {}})),
        );
        assert_eq!(summary_text(&bash), "Running a shell command");

        let unknown = request(
            "user",
            SUMMARY_PROMPT_MARKER,
            Some(json!({"type": "tool_use", "id": "t3", "name": "mcp__jira__get_issue"})),
        );
        assert_eq!(summary_text(&unknown), "Using mcp__jira__get_issue");

        // Nothing to go on: still a label, never an empty line.
        assert_eq!(
            summary_text(&request("user", SUMMARY_PROMPT_MARKER, None)),
            "Working on the task"
        );
    }
}
