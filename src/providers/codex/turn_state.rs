//! Echoing the Codex `x-codex-turn-state` token within a turn.
//!
//! A Codex response may carry an opaque `x-codex-turn-state` value: as an HTTP
//! response header, on the WebSocket handshake response, or in the `headers`
//! of a stream event. The Codex CLI keeps the first value of a turn and sends
//! it back on every later request of that same turn, as a request header over
//! HTTP and inside `client_metadata` of `response.create` over WebSocket. It
//! never carries a value into the next turn. This module does the same, per
//! conversation.
//!
//! The turn comes from Claude Code's history: a request whose last user
//! message holds a `tool_result` continues the turn in progress, and any other
//! request starts a new one. Claude Code puts reminder text next to tool
//! results, so text alone does not mark a boundary.
//!
//! Only a conversation's own turns take part. A request with no conversation
//! identity, a side request without client tools (a title, a search call) and
//! a subagent progress label neither read nor reset the state, so a label sent
//! in the middle of a subagent's turn does not end it. State lives in memory,
//! one slot per conversation, and a restart clears it.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Mutex, OnceLock};

use serde_json::Value;

use crate::anthropic::schema::MessagesRequest;
use crate::request_identity::ConversationIdentity;

pub(crate) const TURN_STATE_HEADER: &str = "x-codex-turn-state";

/// A slot left alone this long belongs to a conversation that has ended; it
/// matches how long continuation state is kept.
const IDLE_MS: u64 = 30 * 60 * 1000;

#[derive(Debug, Default)]
struct Slot {
    /// The request that took the slot last. Only it may echo from the slot or
    /// fill it, so a request that finished late cannot write into a turn that
    /// has moved on.
    req_id: String,
    /// The first value seen in the current turn.
    value: Option<String>,
    /// The value that request sends, fixed when it took the slot so every
    /// resend of the request carries the same one.
    outgoing: Option<String>,
    touched_at: u64,
}

static SLOTS: OnceLock<Mutex<HashMap<ConversationIdentity, Slot>>> = OnceLock::new();

tokio::task_local! {
    /// The request a WebSocket connect runs for, so the handshake response can
    /// be read without threading the request through the connect helpers.
    static HANDSHAKE_REQUEST: String;
}

fn slots() -> &'static Mutex<HashMap<ConversationIdentity, Slot>> {
    SLOTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

/// What a request was planned with, for the request log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TurnStatePlan {
    pub(crate) new_turn: bool,
    pub(crate) sent: bool,
}

/// Whether `body` starts a new turn: its last user message holds no
/// `tool_result` block.
pub(crate) fn starts_new_turn(body: &MessagesRequest) -> bool {
    let Some(last_user) = body
        .messages
        .iter()
        .rev()
        .find(|message| message.role == "user")
    else {
        return true;
    };
    !last_user.content.as_array().is_some_and(|blocks| {
        blocks
            .iter()
            .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
    })
}

fn takes_part(body: &MessagesRequest) -> bool {
    crate::monitor::has_client_tools(&body.extra)
        && !crate::agent_summary::is_agent_summary_request(body)
}

/// Make `req_id` the current request of its conversation. A new turn drops the
/// stored value first; a continuing one sends what the turn has stored.
pub(crate) fn plan_request(
    req_id: &str,
    identity: Option<&ConversationIdentity>,
    body: &MessagesRequest,
) -> TurnStatePlan {
    let new_turn = starts_new_turn(body);
    let mut plan = TurnStatePlan {
        new_turn,
        sent: false,
    };
    let Some(identity) = identity.filter(|_| takes_part(body)) else {
        return plan;
    };
    let Ok(mut slots) = slots().lock() else {
        return plan;
    };
    let now = now_ms();
    slots.retain(|_, slot| now.saturating_sub(slot.touched_at) <= IDLE_MS);
    let slot = slots.entry(identity.clone()).or_default();
    if new_turn {
        slot.value = None;
    }
    slot.req_id = req_id.to_string();
    slot.outgoing = slot.value.clone();
    slot.touched_at = now;
    plan.sent = slot.outgoing.is_some();
    plan
}

/// The value `req_id` sends, if it is the current request of a turn that has
/// stored one.
pub(crate) fn outgoing(req_id: &str) -> Option<String> {
    let slots = slots().lock().ok()?;
    slots
        .values()
        .find(|slot| slot.req_id == req_id)?
        .outgoing
        .clone()
}

/// Keep `value` for the turn `req_id` is the current request of, unless the
/// turn already has one.
fn capture(req_id: &str, value: Option<&str>) {
    let Some(value) = value.filter(|value| http::HeaderValue::from_str(value).is_ok()) else {
        return;
    };
    let Ok(mut slots) = slots().lock() else {
        return;
    };
    if let Some(slot) = slots.values_mut().find(|slot| slot.req_id == req_id)
        && slot.value.is_none()
    {
        slot.value = Some(value.to_string());
    }
}

/// Read the value from HTTP response headers.
pub(crate) fn observe_response_headers(req_id: &str, headers: &[(String, String)]) {
    let value = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(TURN_STATE_HEADER))
        .map(|(_, value)| value.as_str());
    capture(req_id, value);
}

/// Read the value from the `headers` object of a stream event.
pub(crate) fn observe_event(req_id: &str, payload: &Value) {
    let value = payload
        .get("headers")
        .and_then(Value::as_object)
        .and_then(|headers| {
            headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(TURN_STATE_HEADER))
        })
        .and_then(|(_, value)| match value {
            Value::Array(items) => items.first().and_then(Value::as_str),
            value => value.as_str(),
        });
    capture(req_id, value);
}

/// Read the value from a buffered response: its headers first, then its
/// events in order.
pub(crate) fn observe_buffered_response(req_id: &str, headers: &[(String, String)], body: &[u8]) {
    observe_response_headers(req_id, headers);
    for event in crate::anthropic::sse::parse_sse_events(body) {
        if let Ok(payload) = serde_json::from_str::<Value>(&event.data) {
            observe_event(req_id, &payload);
        }
    }
}

/// Run a WebSocket connect for `req_id`, so its handshake response is read.
pub(crate) async fn connect_for_request<F: Future>(req_id: &str, connect: F) -> F::Output {
    HANDSHAKE_REQUEST.scope(req_id.to_string(), connect).await
}

/// Read the value from a WebSocket handshake response.
pub(crate) fn observe_handshake(headers: &http::HeaderMap) {
    let value = headers
        .get(TURN_STATE_HEADER)
        .and_then(|value| value.to_str().ok());
    if value.is_some() {
        let _ = HANDSHAKE_REQUEST.try_with(|req_id| capture(req_id, value));
    }
}

/// Put the value `req_id` sends into a `response.create` message's
/// `client_metadata`, where the Codex CLI sends it over WebSocket.
pub(crate) fn apply_to_websocket_request(req_id: &str, request: &mut Value) {
    let Some(value) = outgoing(req_id) else {
        return;
    };
    let Some(request) = request.as_object_mut() else {
        return;
    };
    let metadata = request
        .entry("client_metadata")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    if !metadata.is_object() {
        *metadata = Value::Object(serde_json::Map::new());
    }
    if let Some(metadata) = metadata.as_object_mut() {
        metadata.insert(TURN_STATE_HEADER.to_string(), Value::String(value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn identity() -> ConversationIdentity {
        ConversationIdentity::Main(uuid::Uuid::new_v4().to_string())
    }

    fn request(messages: Value) -> MessagesRequest {
        serde_json::from_value(json!({
            "model": "gpt-6-sol",
            "max_tokens": 64,
            "tools": [{
                "name": "read_chunk",
                "description": "Read a chunk",
                "input_schema": {"type": "object", "properties": {}}
            }],
            "messages": messages
        }))
        .unwrap()
    }

    fn new_turn_request() -> MessagesRequest {
        request(json!([{"role": "user", "content": "start"}]))
    }

    fn continuing_request() -> MessagesRequest {
        request(json!([
            {"role": "user", "content": "start"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_1", "name": "read_chunk", "input": {}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": "chunk"},
                {"type": "text", "text": "<system-reminder>keep going</system-reminder>"}
            ]}
        ]))
    }

    fn header(value: &str) -> Vec<(String, String)> {
        vec![("X-Codex-Turn-State".to_string(), value.to_string())]
    }

    #[test]
    fn a_tool_result_in_the_last_user_message_continues_the_turn() {
        assert!(starts_new_turn(&new_turn_request()));
        assert!(!starts_new_turn(&continuing_request()));
        assert!(starts_new_turn(&request(json!([
            {"role": "user", "content": [{"type": "text", "text": "next question"}]}
        ]))));
        // The last user message decides, whatever follows it.
        assert!(!starts_new_turn(&request(json!([
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": "chunk"}
            ]},
            {"role": "assistant", "content": "partial"}
        ]))));
        // An earlier tool result does not carry over a new user message.
        assert!(starts_new_turn(&request(json!([
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": "chunk"}
            ]},
            {"role": "assistant", "content": "done"},
            {"role": "user", "content": "next"}
        ]))));
        assert!(starts_new_turn(&request(json!([]))));
    }

    #[test]
    fn the_first_value_of_a_turn_wins_and_is_echoed_within_it() {
        let identity = identity();
        let first = plan_request("ts-first", Some(&identity), &new_turn_request());
        assert_eq!(
            first,
            TurnStatePlan {
                new_turn: true,
                sent: false
            }
        );
        assert_eq!(outgoing("ts-first"), None);
        observe_response_headers("ts-first", &header("token-a"));
        observe_event(
            "ts-first",
            &json!({"type": "codex.response.metadata", "headers": {"x-codex-turn-state": "token-b"}}),
        );

        let second = plan_request("ts-second", Some(&identity), &continuing_request());
        assert_eq!(
            second,
            TurnStatePlan {
                new_turn: false,
                sent: true
            }
        );
        assert_eq!(outgoing("ts-second").as_deref(), Some("token-a"));
        observe_response_headers("ts-second", &header("token-c"));

        let third = plan_request("ts-third", Some(&identity), &continuing_request());
        assert!(third.sent);
        assert_eq!(outgoing("ts-third").as_deref(), Some("token-a"));
    }

    #[test]
    fn a_new_turn_never_sends_the_previous_turns_value() {
        let identity = identity();
        plan_request("nt-1", Some(&identity), &new_turn_request());
        observe_response_headers("nt-1", &header("turn-one"));
        assert!(plan_request("nt-2", Some(&identity), &continuing_request()).sent);

        let next_turn = plan_request("nt-3", Some(&identity), &new_turn_request());
        assert!(next_turn.new_turn);
        assert!(!next_turn.sent);
        assert_eq!(outgoing("nt-3"), None);
        observe_event(
            "nt-3",
            &json!({"type": "codex.response.metadata", "headers": {"x-codex-turn-state": "turn-two"}}),
        );
        plan_request("nt-4", Some(&identity), &continuing_request());
        assert_eq!(outgoing("nt-4").as_deref(), Some("turn-two"));
    }

    #[test]
    fn a_request_without_a_conversation_identity_never_echoes() {
        let plan = plan_request("no-identity", None, &continuing_request());
        assert!(!plan.new_turn);
        assert!(!plan.sent);
        observe_response_headers("no-identity", &header("orphan"));
        assert_eq!(outgoing("no-identity"), None);
    }

    #[test]
    fn side_requests_and_progress_labels_leave_the_turn_alone() {
        let identity = identity();
        plan_request("side-1", Some(&identity), &new_turn_request());
        observe_response_headers("side-1", &header("main-turn"));

        let title: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-6-sol",
            "max_tokens": 64,
            "messages": [{"role": "user", "content": "name this session"}]
        }))
        .unwrap();
        assert!(!plan_request("side-title", Some(&identity), &title).sent);
        observe_response_headers("side-title", &header("title-token"));
        let label = request(json!([
            {"role": "user", "content": "start"},
            {"role": "user", "content": crate::agent_summary::SUMMARY_PROMPT_MARKER}
        ]));
        assert!(!plan_request("side-label", Some(&identity), &label).sent);

        plan_request("side-2", Some(&identity), &continuing_request());
        assert_eq!(outgoing("side-2").as_deref(), Some("main-turn"));
    }

    #[test]
    fn a_late_request_cannot_fill_a_turn_that_moved_on() {
        let identity = identity();
        plan_request("late-1", Some(&identity), &new_turn_request());
        plan_request("late-2", Some(&identity), &new_turn_request());
        observe_response_headers("late-1", &header("stale"));
        observe_response_headers("late-2", &header("current"));
        plan_request("late-3", Some(&identity), &continuing_request());
        assert_eq!(outgoing("late-3").as_deref(), Some("current"));
    }

    #[test]
    fn buffered_responses_are_read_headers_first_then_events() {
        let identity = identity();
        plan_request("buffered-1", Some(&identity), &new_turn_request());
        let body = concat!(
            "data: {\"type\":\"codex.response.metadata\",\"headers\":{\"x-codex-turn-state\":\"from-event\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\"}}\n\n"
        );
        observe_buffered_response("buffered-1", &[], body.as_bytes());
        plan_request("buffered-2", Some(&identity), &continuing_request());
        assert_eq!(outgoing("buffered-2").as_deref(), Some("from-event"));
    }

    #[tokio::test]
    async fn the_handshake_response_is_read_for_the_request_that_connects() {
        let identity = identity();
        plan_request("handshake-1", Some(&identity), &new_turn_request());
        let mut headers = http::HeaderMap::new();
        headers.insert(TURN_STATE_HEADER, "from-handshake".parse().unwrap());
        // Outside a connect scope there is no request to read it for.
        observe_handshake(&headers);
        plan_request("handshake-probe", Some(&identity), &continuing_request());
        assert_eq!(outgoing("handshake-probe"), None);

        plan_request("handshake-2", Some(&identity), &new_turn_request());
        connect_for_request("handshake-2", async { observe_handshake(&headers) }).await;
        plan_request("handshake-3", Some(&identity), &continuing_request());
        assert_eq!(outgoing("handshake-3").as_deref(), Some("from-handshake"));
    }

    #[test]
    fn the_websocket_echo_goes_into_client_metadata() {
        let identity = identity();
        plan_request("ws-1", Some(&identity), &new_turn_request());
        let mut untouched = json!({"type": "response.create", "model": "gpt-6-sol"});
        apply_to_websocket_request("ws-1", &mut untouched);
        assert_eq!(
            untouched,
            json!({"type": "response.create", "model": "gpt-6-sol"})
        );

        observe_response_headers("ws-1", &header("ws-token"));
        plan_request("ws-2", Some(&identity), &continuing_request());
        let mut lite = json!({
            "type": "response.create",
            "client_metadata": {"ws_request_header_x_openai_internal_codex_responses_lite": "true"}
        });
        apply_to_websocket_request("ws-2", &mut lite);
        assert_eq!(
            lite["client_metadata"],
            json!({
                "ws_request_header_x_openai_internal_codex_responses_lite": "true",
                "x-codex-turn-state": "ws-token"
            })
        );
    }
}
