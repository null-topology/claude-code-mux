pub mod error;
pub mod schema;
pub mod sse;

/// Largest request body the Anthropic routes read. A Claude Code history
/// carrying images grows well past what a text-only conversation needs, and
/// the whole history is resent every turn, so a limit that fits the
/// OpenAI-compatible surfaces cuts those conversations off entirely.
pub const MAX_ANTHROPIC_REQUEST_BYTES: usize = 64 * 1024 * 1024;

pub use self::error::{ErrorDetail, ErrorEnvelope, json_error};
pub use self::schema::{CountTokensResponse, Message, MessagesRequest};
pub use self::sse::{
    SseEvent, SseParseStats, encode_sse_event, parse_sse_events, parse_sse_events_with_stats,
};
