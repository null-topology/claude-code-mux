//! Claude Code's advisor is an Anthropic server tool: Anthropic's servers run
//! it inside one Messages call whose executor is a Claude model, and no other
//! backend can run it. On every other route the proxy removes the advisor
//! tool, a `tool_choice` that names it and the advisor's blocks in the
//! history, so a translated model never calls a tool the client cannot answer.

use std::collections::HashSet;

use crate::anthropic::schema::{Message, MessagesRequest};
use serde_json::{Map, Value};

/// The `type` of every advisor tool version starts with this.
const ADVISOR_TOOL_TYPE_PREFIX: &str = "advisor_";
/// The block that carries the advisor's answer in the history.
const ADVISOR_RESULT_TYPE: &str = "advisor_tool_result";
/// The name of the `server_tool_use` block that calls the advisor.
const ADVISOR_TOOL_NAME: &str = "advisor";

/// Removes the advisor tools from `tools`, a `tool_choice` that only a removed
/// tool could satisfy, and the advisor's blocks from the history. Returns
/// whether anything was removed; a request without the advisor is unchanged.
pub fn strip_advisor(body: &mut MessagesRequest) -> bool {
    let tools_removed = strip_advisor_tools(&mut body.extra);
    let history_removed = strip_advisor_history(&mut body.messages);
    tools_removed || history_removed
}

/// Removes the advisor entries from a `tools` array, the key itself when none
/// is left, and then a `tool_choice` that needs a removed tool. A `tools`
/// value that is not an array is left alone.
fn strip_advisor_tools(extra: &mut Map<String, Value>) -> bool {
    let Some(Value::Array(tools)) = extra.get_mut("tools") else {
        return false;
    };
    let removed_names: Vec<String> = tools
        .iter()
        .filter(|tool| is_advisor_tool(tool))
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
        .map(str::to_owned)
        .collect();
    let before = tools.len();
    tools.retain(|tool| !is_advisor_tool(tool));
    if tools.len() == before {
        return false;
    }
    let no_tool_left = tools.is_empty();
    if no_tool_left {
        extra.remove("tools");
    }
    if choice_needs_a_removed_tool(extra.get("tool_choice"), &removed_names, no_tool_left) {
        extra.remove("tool_choice");
    }
    true
}

fn is_advisor_tool(tool: &Value) -> bool {
    tool.get("type")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind.starts_with(ADVISOR_TOOL_TYPE_PREFIX))
}

/// A choice of one named tool needs it, and with no tool left a choice of a
/// named tool or of `any` has nothing to pick from.
fn choice_needs_a_removed_tool(
    choice: Option<&Value>,
    removed_names: &[String],
    no_tool_left: bool,
) -> bool {
    let Some(choice) = choice else {
        return false;
    };
    match choice.get("type").and_then(Value::as_str) {
        Some("tool") => {
            no_tool_left
                || choice
                    .get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|name| removed_names.iter().any(|removed| removed == name))
        }
        Some("any") => no_tool_left,
        _ => false,
    }
}

/// Removes every `advisor_tool_result` block, and every `server_tool_use`
/// block that is named `advisor` or whose id one of those results answers.
/// A message left with no block goes too; string content and a message that
/// arrived with no block stay as they are.
fn strip_advisor_history(messages: &mut Vec<Message>) -> bool {
    let answered: HashSet<String> = messages
        .iter()
        .filter_map(|message| message.content.as_array())
        .flatten()
        .filter(|block| block_type(block) == Some(ADVISOR_RESULT_TYPE))
        .filter_map(|block| block.get("tool_use_id").and_then(Value::as_str))
        .map(str::to_owned)
        .collect();
    let mut removed = false;
    messages.retain_mut(|message| {
        let Some(blocks) = message.content.as_array_mut() else {
            return true;
        };
        let before = blocks.len();
        blocks.retain(|block| !is_advisor_block(block, &answered));
        if blocks.len() == before {
            return true;
        }
        removed = true;
        !blocks.is_empty()
    });
    removed
}

fn is_advisor_block(block: &Value, answered: &HashSet<String>) -> bool {
    match block_type(block) {
        Some(ADVISOR_RESULT_TYPE) => true,
        Some("server_tool_use") => {
            block.get("name").and_then(Value::as_str) == Some(ADVISOR_TOOL_NAME)
                || block
                    .get("id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| answered.contains(id))
        }
        _ => false,
    }
}

fn block_type(block: &Value) -> Option<&str> {
    block.get("type").and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::strip_advisor;
    use crate::anthropic::schema::MessagesRequest;
    use serde_json::{Value, json};

    fn request(value: Value) -> MessagesRequest {
        serde_json::from_value(value).unwrap()
    }

    /// Strips `value` and returns what `strip_advisor` said with the body as
    /// it would be serialized afterwards.
    fn stripped(value: Value) -> (bool, Value) {
        let mut body = request(value);
        let removed = strip_advisor(&mut body);
        (removed, serde_json::to_value(&body).unwrap())
    }

    fn advisor_tool() -> Value {
        json!({"type": "advisor_20260301", "name": "advisor", "model": "claude-fable-5-2"})
    }

    fn client_tool(name: &str) -> Value {
        json!({
            "name": name,
            "description": "A client tool.",
            "input_schema": {"type": "object", "properties": {}}
        })
    }

    fn web_search_tool() -> Value {
        json!({"type": "web_search_20250305", "name": "web_search", "max_uses": 5})
    }

    #[test]
    fn strips_advisor_tools_and_keeps_the_others_in_order() {
        let (removed, body) = stripped(json!({
            "model": "gpt-5.5",
            "messages": [],
            "tools": [
                client_tool("Read"),
                advisor_tool(),
                web_search_tool(),
                {
                    "type": "advisor_20260301",
                    "name": "advisor",
                    "model": "claude-fable-5-2",
                    "defer_loading": true
                },
                client_tool("Bash"),
                {
                    "type": "advisor_20260301",
                    "name": "advisor",
                    "model": "claude-fable-5-2",
                    "max_uses": 3,
                    "max_tokens": 2048
                }
            ]
        }));
        assert!(removed);
        assert_eq!(
            body["tools"],
            json!([client_tool("Read"), web_search_tool(), client_tool("Bash")])
        );
    }

    #[test]
    fn keeps_a_client_tool_named_advisor() {
        for tool in [
            json!({"name": "advisor", "input_schema": {"type": "object"}}),
            json!({"type": "custom", "name": "advisor", "input_schema": {"type": "object"}}),
            json!({"type": "advisor", "name": "advisor", "input_schema": {"type": "object"}}),
        ] {
            let sent = json!({
                "model": "gpt-5.5",
                "messages": [],
                "tools": [tool.clone()],
                "tool_choice": {"type": "tool", "name": "advisor"}
            });
            let before = serde_json::to_value(request(sent.clone())).unwrap();
            let (removed, body) = stripped(sent);
            assert!(!removed, "{tool}");
            assert_eq!(body, before, "{tool}");
        }
    }

    #[test]
    fn removes_the_tools_key_when_only_the_advisor_was_there() {
        let (removed, body) = stripped(json!({
            "model": "gpt-5.5",
            "messages": [],
            "tools": [
                advisor_tool(),
                {
                    "type": "advisor_20260301",
                    "name": "advisor",
                    "model": "claude-fable-5-2",
                    "defer_loading": true
                }
            ]
        }));
        assert!(removed);
        assert_eq!(body.get("tools"), None);
    }

    #[test]
    fn drops_a_tool_choice_that_names_the_removed_advisor() {
        let (removed, body) = stripped(json!({
            "model": "gpt-5.5",
            "messages": [],
            "tools": [client_tool("Read"), advisor_tool()],
            "tool_choice": {"type": "tool", "name": "advisor", "disable_parallel_tool_use": true}
        }));
        assert!(removed);
        assert_eq!(body["tools"], json!([client_tool("Read")]));
        assert_eq!(body.get("tool_choice"), None);
    }

    #[test]
    fn drops_any_when_no_tool_is_left() {
        for choice in [
            json!({"type": "any"}),
            json!({"type": "any", "disable_parallel_tool_use": true}),
            json!({"type": "tool", "name": "Read"}),
        ] {
            let (removed, body) = stripped(json!({
                "model": "gpt-5.5",
                "messages": [],
                "tools": [advisor_tool()],
                "tool_choice": choice.clone()
            }));
            assert!(removed, "{choice}");
            assert_eq!(body.get("tools"), None, "{choice}");
            assert_eq!(body.get("tool_choice"), None, "{choice}");
        }
    }

    #[test]
    fn keeps_auto_and_a_choice_naming_another_tool() {
        for choice in [
            json!({"type": "auto"}),
            json!({"type": "any"}),
            json!({"type": "none"}),
            json!({"type": "tool", "name": "Read"}),
        ] {
            let (removed, body) = stripped(json!({
                "model": "gpt-5.5",
                "messages": [],
                "tools": [client_tool("Read"), advisor_tool()],
                "tool_choice": choice.clone()
            }));
            assert!(removed, "{choice}");
            assert_eq!(body["tools"], json!([client_tool("Read")]), "{choice}");
            assert_eq!(body["tool_choice"], choice);
        }
        // With no tool left, only a choice that requires one goes.
        for choice in [json!({"type": "auto"}), json!({"type": "none"})] {
            let (removed, body) = stripped(json!({
                "model": "gpt-5.5",
                "messages": [],
                "tools": [advisor_tool()],
                "tool_choice": choice.clone()
            }));
            assert!(removed, "{choice}");
            assert_eq!(body.get("tools"), None, "{choice}");
            assert_eq!(body["tool_choice"], choice);
        }
    }

    #[test]
    fn strips_advisor_blocks_from_the_history() {
        let web_search_use = json!({
            "type": "server_tool_use",
            "id": "srvtoolu_02",
            "name": "web_search",
            "input": {"query": "release notes"}
        });
        let web_search_result = json!({
            "type": "web_search_tool_result",
            "tool_use_id": "srvtoolu_02",
            "content": []
        });
        let (removed, body) = stripped(json!({
            "model": "gpt-5.5",
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "Plan the change."}]},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "Let me ask."},
                    {"type": "server_tool_use", "id": "srvtoolu_01", "name": "advisor", "input": {}},
                    {
                        "type": "advisor_tool_result",
                        "tool_use_id": "srvtoolu_01",
                        "content": {"type": "advisor_result", "text": "Split it in two."}
                    },
                    web_search_use.clone(),
                    web_search_result.clone(),
                    {"type": "text", "text": "Here is the plan."}
                ]},
                {"role": "user", "content": [{"type": "text", "text": "Go on."}]},
                {"role": "assistant", "content": [
                    {"type": "server_tool_use", "id": "srvtoolu_03", "name": "consult", "input": {}},
                    {
                        "type": "advisor_tool_result",
                        "tool_use_id": "srvtoolu_03",
                        "content": {"type": "advisor_redacted_result", "encrypted_content": "opaque"}
                    },
                    {"type": "text", "text": "Done."},
                    {
                        "type": "advisor_tool_result",
                        "tool_use_id": "srvtoolu_04",
                        "content": {"type": "advisor_tool_result_error", "error_code": "unavailable"}
                    }
                ]}
            ]
        }));
        assert!(removed);
        assert_eq!(
            body["messages"],
            json!([
                {"role": "user", "content": [{"type": "text", "text": "Plan the change."}]},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "Let me ask."},
                    web_search_use,
                    web_search_result,
                    {"type": "text", "text": "Here is the plan."}
                ]},
                {"role": "user", "content": [{"type": "text", "text": "Go on."}]},
                {"role": "assistant", "content": [{"type": "text", "text": "Done."}]}
            ])
        );
    }

    #[test]
    fn drops_a_message_the_strip_empties_and_keeps_string_content() {
        let (removed, body) = stripped(json!({
            "model": "gpt-5.5",
            "messages": [
                {"role": "user", "content": "Plan the change."},
                {"role": "assistant", "content": [
                    {"type": "server_tool_use", "id": "srvtoolu_01", "name": "advisor", "input": {}},
                    {
                        "type": "advisor_tool_result",
                        "tool_use_id": "srvtoolu_01",
                        "content": {"type": "advisor_result", "text": "Split it in two."}
                    }
                ]},
                {"role": "assistant", "content": []},
                {"role": "assistant", "content": "Here is the plan."},
                {"role": "user", "content": "Go on."}
            ]
        }));
        assert!(removed);
        assert_eq!(
            body["messages"],
            json!([
                {"role": "user", "content": "Plan the change."},
                {"role": "assistant", "content": []},
                {"role": "assistant", "content": "Here is the plan."},
                {"role": "user", "content": "Go on."}
            ])
        );
    }

    #[test]
    fn leaves_a_request_without_the_advisor_unchanged() {
        for sent in [
            json!({
                "model": "gpt-5.5",
                "max_tokens": 1024,
                "stream": true,
                "system": [{"type": "text", "text": "You may consult the advisor."}],
                "tools": [client_tool("Read"), web_search_tool()],
                "tool_choice": {"type": "auto"},
                "messages": [
                    {"role": "user", "content": "Find the release notes."},
                    {"role": "assistant", "content": [
                        {"type": "text", "text": "Searching."},
                        {
                            "type": "server_tool_use",
                            "id": "srvtoolu_02",
                            "name": "web_search",
                            "input": {"query": "release notes"}
                        },
                        {"type": "web_search_tool_result", "tool_use_id": "srvtoolu_02", "content": []},
                        {"type": "tool_use", "id": "toolu_01", "name": "advisor", "input": {}}
                    ]},
                    {"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": "toolu_01", "content": "done"}
                    ]},
                    {"role": "assistant", "content": []}
                ]
            }),
            json!({"model": "gpt-5.5", "tools": [], "messages": []}),
            json!({"model": "gpt-5.5", "tools": {"type": "advisor_20260301"}, "messages": []}),
            json!({
                "model": "gpt-5.5",
                "tool_choice": {"type": "tool", "name": "advisor"},
                "messages": [{"role": "user", "content": "Plan the change."}]
            }),
        ] {
            let before = serde_json::to_value(request(sent.clone())).unwrap();
            let (removed, body) = stripped(sent);
            assert!(!removed, "{before}");
            assert_eq!(body, before);
        }
    }
}
