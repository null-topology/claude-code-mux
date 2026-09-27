//! Claude Code's side requests: the calls it makes without client tools. They
//! do not extend a transcript, so each goes to a lane of its own under the
//! conversation it was made from, labelled by what it is for. Consecutive ones
//! share little beyond the system prompt, so no side lane of any kind is judged
//! for cache misses.

use serde_json::{Map, Value};

/// What a side request is for, read from the markers its body carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SideKind {
    /// The auto-mode security classifier.
    Classifier,
    /// The request that names the session.
    Title,
    /// The isolated call that carries the hosted web search tool.
    Search,
    /// The recap of an agent left running.
    Recap,
    /// WebFetch's processing of a fetched page.
    Fetch,
    /// Any other side request.
    Side,
}

impl SideKind {
    const ALL: [Self; 6] = [
        Self::Classifier,
        Self::Title,
        Self::Search,
        Self::Recap,
        Self::Fetch,
        Self::Side,
    ];

    /// The suffix appended to the label of the conversation the request was
    /// made from.
    pub fn suffix(self) -> &'static str {
        match self {
            Self::Classifier => "/classifier",
            Self::Title => "/title",
            Self::Search => "/search",
            Self::Recap => "/recap",
            Self::Fetch => "/fetch",
            Self::Side => "/side",
        }
    }
}

/// The conversation a side lane was made from and the kind of its requests, or
/// `None` for a conversation of its own.
pub fn split_side_conversation(conversation: &str) -> Option<(&str, SideKind)> {
    SideKind::ALL.into_iter().find_map(|kind| {
        conversation
            .strip_suffix(kind.suffix())
            .map(|base| (base, kind))
    })
}

/// Whether a conversation label names a side lane of any kind.
pub fn is_side_conversation(conversation: &str) -> bool {
    split_side_conversation(conversation).is_some()
}

/// Whether a request body carries a client tool, one with an `input_schema`.
/// A request without one is a side request.
pub fn has_client_tools(body: &Map<String, Value>) -> bool {
    body.get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| tools.iter().any(|tool| tool.get("input_schema").is_some()))
}

/// Whether a request body is Claude Code's session title request: a side
/// request whose `output_config` asks for a JSON schema whose only property is
/// `title`. The `/title` label and the passthrough's capture of the title
/// reply both go by this.
pub fn is_session_title_side_request(body: &Map<String, Value>) -> bool {
    !has_client_tools(body) && body.get("output_config").is_some_and(asks_for_title)
}

fn asks_for_title(output_config: &Value) -> bool {
    let Some(format) = output_config.get("format") else {
        return false;
    };
    format.get("type").and_then(Value::as_str) == Some("json_schema")
        && format
            .pointer("/schema/properties")
            .and_then(Value::as_object)
            .is_some_and(|properties| properties.len() == 1 && properties.contains_key("title"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_side_label_splits_into_its_base_and_kind() {
        assert_eq!(
            split_side_conversation("main/title"),
            Some(("main", SideKind::Title))
        );
        assert_eq!(
            split_side_conversation("agent-7/classifier"),
            Some(("agent-7", SideKind::Classifier))
        );
        assert_eq!(
            split_side_conversation("main/side"),
            Some(("main", SideKind::Side))
        );
        assert_eq!(split_side_conversation("main"), None);
        assert_eq!(split_side_conversation("agent-7"), None);
        assert!(!is_side_conversation("main/other"));
    }

    fn body(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap()
    }

    #[test]
    fn only_a_schema_with_a_single_title_property_asks_for_the_session_title() {
        let title = json!({"format": {"type": "json_schema", "schema": {
            "type": "object",
            "properties": {"title": {"type": "string"}},
            "required": ["title"],
            "additionalProperties": false
        }}});
        assert!(is_session_title_side_request(&body(
            json!({"output_config": title})
        )));

        let wider = json!({"format": {"type": "json_schema", "schema": {
            "properties": {"title": {"type": "string"}, "summary": {"type": "string"}}
        }}});
        assert!(!is_session_title_side_request(&body(
            json!({"output_config": wider})
        )));
        let other = json!({"format": {"type": "json_schema", "schema": {
            "properties": {"verdict": {"type": "string"}}
        }}});
        assert!(!is_session_title_side_request(&body(
            json!({"output_config": other})
        )));
        assert!(!is_session_title_side_request(&body(
            json!({"output_config": {"effort": "low"}})
        )));
    }

    /// A turn that carries client tools is never the title request, whatever
    /// its `output_config` asks for; a hosted tool alone does not count.
    #[test]
    fn a_title_schema_on_a_request_with_client_tools_is_not_the_title_request() {
        let title = json!({"format": {"type": "json_schema", "schema": {
            "properties": {"title": {"type": "string"}}
        }}});
        let with_tools = body(json!({
            "output_config": title,
            "tools": [{"name": "Read", "input_schema": {"type": "object"}}]
        }));
        assert!(has_client_tools(&with_tools));
        assert!(!is_session_title_side_request(&with_tools));

        let hosted_only = body(json!({
            "output_config": title,
            "tools": [{"type": "web_search_20250305", "name": "web_search"}]
        }));
        assert!(!has_client_tools(&hosted_only));
        assert!(is_session_title_side_request(&hosted_only));
    }
}
