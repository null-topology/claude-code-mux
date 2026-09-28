//! Echoing the Codex `x-codex-turn-state` token within a turn.
//!
//! The echo goes where the Codex CLI puts it: a request header over HTTP and a
//! key of `client_metadata` in `response.create` over WebSocket. The value is
//! the first one a turn receives, from the `x-codex-turn-state` header of an
//! HTTP response or from the `headers` object of a stream event whose type is
//! `response.metadata` or `codex.response.metadata`. The CLI reads only
//! `response.metadata`; Codex WebSocket streams carry the value in
//! `codex.response.metadata`, so reading that type as well goes beyond the
//! CLI. Events of any other type, `error` included, are never read, and
//! neither is the WebSocket handshake response.
//!
//! The turn comes from Claude Code's history. A request continues the turn in
//! progress when its last user message holds a `tool_result` and every other
//! block in it is a `tool_result` or reminder text (a text block whose trimmed
//! text starts with `<system-reminder>`, which Claude Code puts next to tool
//! results). Any
//! other request starts a new turn, which drops the stored value before the
//! request is sent: a prompt typed after an interrupt, or feedback sent next
//! to a tool result, starts one. Misjudging toward a new turn only loses the
//! echo. A value is never carried into the next turn.
//!
//! Only a conversation's own turns take part. A request with no conversation
//! identity, a side request without client tools (a title, a search call) and
//! a subagent progress label neither read nor reset the state, so a label sent
//! in the middle of a subagent's turn does not end it. State lives in memory,
//! one slot per conversation, and a restart clears it.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use serde_json::Value;

use crate::anthropic::schema::MessagesRequest;
use crate::request_identity::ConversationIdentity;

pub(crate) const TURN_STATE_HEADER: &str = "x-codex-turn-state";

/// The stream events whose `headers` are read.
const METADATA_EVENT_TYPES: [&str; 2] = ["response.metadata", "codex.response.metadata"];

/// A slot expires after this long with no tracked activity: a request planned
/// on it, a value stored in it or a value sent from it. Idle time does not
/// prove that the turn ended; it only bounds how long a slot is kept. Expired
/// slots are dropped when the next request is planned. The first request of a
/// turn planned after its slot expired goes out without a value, and a value
/// its response carries is sent from then on. The bound is the one
/// continuation state uses.
const IDLE_MS: u64 = 30 * 60 * 1000;

const REMINDER_PREFIX: &str = "<system-reminder>";

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
    /// The last tracked activity on the slot.
    touched_at: u64,
}

static SLOTS: OnceLock<Mutex<HashMap<ConversationIdentity, Slot>>> = OnceLock::new();

fn slots() -> &'static Mutex<HashMap<ConversationIdentity, Slot>> {
    SLOTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

/// Whether `slot` has had no tracked activity for longer than `IDLE_MS` at
/// `now`.
fn expired(slot: &Slot, now: u64) -> bool {
    now.saturating_sub(slot.touched_at) > IDLE_MS
}

/// What a request was planned with, for the request log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TurnStatePlan {
    pub(crate) new_turn: bool,
    pub(crate) sent: bool,
}

/// Whether `body` starts a new turn. It continues the turn in progress only
/// when its last user message holds at least one `tool_result` and every other
/// block in it is a `tool_result` or reminder text.
pub(crate) fn starts_new_turn(body: &MessagesRequest) -> bool {
    let Some(last_user) = body
        .messages
        .iter()
        .rev()
        .find(|message| message.role == "user")
    else {
        return true;
    };
    let Some(blocks) = last_user.content.as_array() else {
        return true;
    };
    let mut has_tool_result = false;
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("tool_result") => has_tool_result = true,
            Some("text") if is_reminder_text(block) => {}
            _ => return true,
        }
    }
    !has_tool_result
}

fn is_reminder_text(block: &Value) -> bool {
    block
        .get("text")
        .and_then(Value::as_str)
        .is_some_and(|text| text.trim().starts_with(REMINDER_PREFIX))
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
    slots.retain(|_, slot| !expired(slot, now));
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
/// stored one. Sending counts as activity on the slot.
pub(crate) fn outgoing(req_id: &str) -> Option<String> {
    let mut slots = slots().lock().ok()?;
    let slot = slots.values_mut().find(|slot| slot.req_id == req_id)?;
    let value = slot.outgoing.clone()?;
    slot.touched_at = now_ms();
    Some(value)
}

/// Keep `value` for the turn `req_id` is the current request of, unless the
/// turn already has one. Storing it counts as activity on the slot.
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
        slot.touched_at = now_ms();
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

/// Read the value from the `headers` object of a metadata stream event. Any
/// other event is ignored, whatever `headers` it carries.
pub(crate) fn observe_event(req_id: &str, payload: &Value) {
    let is_metadata = payload
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|kind| METADATA_EVENT_TYPES.contains(&kind));
    if !is_metadata {
        return;
    }
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
/// metadata events in order.
pub(crate) fn observe_buffered_response(req_id: &str, headers: &[(String, String)], body: &[u8]) {
    observe_response_headers(req_id, headers);
    for event in crate::anthropic::sse::parse_sse_events(body) {
        if let Ok(payload) = serde_json::from_str::<Value>(&event.data) {
            observe_event(req_id, &payload);
        }
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

    /// A request whose last user message holds `blocks`.
    fn last_user_blocks(blocks: Value) -> MessagesRequest {
        request(json!([
            {"role": "user", "content": "start"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_1", "name": "read_chunk", "input": {}}
            ]},
            {"role": "user", "content": blocks}
        ]))
    }

    fn header(value: &str) -> Vec<(String, String)> {
        vec![("X-Codex-Turn-State".to_string(), value.to_string())]
    }

    fn metadata_event(kind: &str, value: &str) -> Value {
        json!({"type": kind, "headers": {"x-codex-turn-state": value}})
    }

    /// Move the slot's last activity `by_ms` into the past and return the new
    /// time.
    fn shift_touched_at(identity: &ConversationIdentity, by_ms: u64) -> u64 {
        let mut slots = slots().lock().unwrap();
        let slot = slots.get_mut(identity).unwrap();
        slot.touched_at -= by_ms;
        slot.touched_at
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
    fn anything_but_reminders_next_to_a_tool_result_starts_a_new_turn() {
        let tool_result = json!({
            "type": "tool_result",
            "tool_use_id": "toolu_1",
            "content": "[Request interrupted by user for tool use]"
        });
        // A prompt typed after an interrupt arrives next to the tool result.
        assert!(starts_new_turn(&last_user_blocks(json!([
            tool_result.clone(),
            {"type": "text", "text": "stop and look at the tests instead"}
        ]))));
        // Reminder text only counts when it opens the block.
        assert!(starts_new_turn(&last_user_blocks(json!([
            tool_result.clone(),
            {"type": "text", "text": "wait\n<system-reminder>note</system-reminder>"}
        ]))));
        assert!(starts_new_turn(&last_user_blocks(json!([
            tool_result.clone(),
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AA=="}}
        ]))));
        // Reminders alone are not a tool round trip.
        assert!(starts_new_turn(&last_user_blocks(json!([
            {"type": "text", "text": "<system-reminder>note</system-reminder>"}
        ]))));
        // Several tool results with reminders around them, leading whitespace
        // included, continue the turn.
        assert!(!starts_new_turn(&last_user_blocks(json!([
            {"type": "text", "text": "\n  <system-reminder>before</system-reminder>"},
            tool_result.clone(),
            {"type": "tool_result", "tool_use_id": "toolu_2", "content": "chunk"},
            {"type": "text", "text": "<system-reminder>after</system-reminder>\n"}
        ]))));
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
            &metadata_event("codex.response.metadata", "token-b"),
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
    fn both_metadata_event_types_carry_the_value() {
        for kind in ["response.metadata", "codex.response.metadata"] {
            let identity = identity();
            let (first, second) = (format!("{kind}-1"), format!("{kind}-2"));
            plan_request(&first, Some(&identity), &new_turn_request());
            observe_event(&first, &metadata_event(kind, kind));
            plan_request(&second, Some(&identity), &continuing_request());
            assert_eq!(outgoing(&second).as_deref(), Some(kind), "type {kind}");
        }
    }

    #[test]
    fn other_events_never_carry_the_value() {
        let identity = identity();
        plan_request("other-1", Some(&identity), &new_turn_request());
        observe_event(
            "other-1",
            &json!({
                "type": "error",
                "error": {"type": "server_error", "message": "boom"},
                "headers": {"x-codex-turn-state": "from-error"}
            }),
        );
        for kind in [
            "response.created",
            "codex.rate_limits",
            "response.completed",
        ] {
            observe_event("other-1", &metadata_event(kind, kind));
        }
        observe_event(
            "other-1",
            &json!({"headers": {"x-codex-turn-state": "untyped"}}),
        );
        plan_request("other-2", Some(&identity), &continuing_request());
        assert_eq!(outgoing("other-2"), None);

        // The slot is still open to the metadata event that follows.
        observe_event(
            "other-2",
            &metadata_event("response.metadata", "from-metadata"),
        );
        plan_request("other-3", Some(&identity), &continuing_request());
        assert_eq!(outgoing("other-3").as_deref(), Some("from-metadata"));
    }

    #[test]
    fn storing_or_sending_a_value_keeps_the_slot_past_the_plan_time_window() {
        // Each slot's planning is moved 20 minutes back, still inside the
        // window, so pruning by a test running alongside cannot drop it.
        const EARLIER: u64 = 20 * 60 * 1000;

        let filled = identity();
        plan_request("keep-fill-1", Some(&filled), &new_turn_request());
        let filled_planned_at = shift_touched_at(&filled, EARLIER);
        observe_response_headers("keep-fill-1", &header("filled"));

        let sent = identity();
        plan_request("keep-send-1", Some(&sent), &new_turn_request());
        observe_response_headers("keep-send-1", &header("sent"));
        plan_request("keep-send-2", Some(&sent), &continuing_request());
        let sent_planned_at = shift_touched_at(&sent, EARLIER);
        assert_eq!(outgoing("keep-send-2").as_deref(), Some("sent"));

        let untouched = identity();
        plan_request("keep-idle-1", Some(&untouched), &new_turn_request());
        let untouched_planned_at = shift_touched_at(&untouched, EARLIER);
        // A value the slot already holds is not stored again.
        let refilled = identity();
        plan_request("keep-refill-1", Some(&refilled), &new_turn_request());
        observe_response_headers("keep-refill-1", &header("first"));
        plan_request("keep-refill-2", Some(&refilled), &new_turn_request());
        observe_response_headers("keep-refill-2", &header("second"));
        let refilled_planned_at = shift_touched_at(&refilled, EARLIER);
        observe_response_headers("keep-refill-2", &header("third"));

        // Just past the window that opened when each request was planned, the
        // pruning `plan_request` runs keeps the slots with later activity.
        let slots = slots().lock().unwrap();
        assert!(!expired(&slots[&filled], filled_planned_at + IDLE_MS + 1));
        assert!(!expired(&slots[&sent], sent_planned_at + IDLE_MS + 1));
        assert!(expired(
            &slots[&untouched],
            untouched_planned_at + IDLE_MS + 1
        ));
        assert!(expired(
            &slots[&refilled],
            refilled_planned_at + IDLE_MS + 1
        ));
        assert!(!expired(&slots[&untouched], untouched_planned_at + IDLE_MS));
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
            &metadata_event("codex.response.metadata", "turn-two"),
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
    fn buffered_responses_are_read_headers_first_then_metadata_events() {
        let identity = identity();
        plan_request("buffered-1", Some(&identity), &new_turn_request());
        let body = concat!(
            "data: {\"type\":\"error\",\"headers\":{\"x-codex-turn-state\":\"from-error\"}}\n\n",
            "data: {\"type\":\"codex.response.metadata\",\"headers\":{\"x-codex-turn-state\":\"from-event\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\"}}\n\n"
        );
        observe_buffered_response("buffered-1", &[], body.as_bytes());
        plan_request("buffered-2", Some(&identity), &continuing_request());
        assert_eq!(outgoing("buffered-2").as_deref(), Some("from-event"));
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
