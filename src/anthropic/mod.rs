pub mod error;
pub mod schema;
pub mod sse;

/// Largest request body the Anthropic routes read. A Claude Code history
/// carrying images grows well past what a text-only conversation needs, and
/// the whole history is resent every turn, so a limit that fits the
/// OpenAI-compatible surfaces cuts those conversations off entirely. Anthropic
/// documents a 128 MB request size limit for the Messages and Token Counting
/// endpoints; 128 MiB is above it, so the proxy never refuses a body the
/// backend would take.
pub const MAX_ANTHROPIC_REQUEST_BYTES: usize = 128 * 1024 * 1024;

pub use self::error::{ErrorDetail, ErrorEnvelope, json_error};
pub use self::schema::{CountTokensResponse, Message, MessagesRequest};
pub use self::sse::{
    SseEvent, SseParseStats, encode_sse_event, parse_sse_events, parse_sse_events_with_stats,
};
