# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A small local proxy that lets Claude Code talk to more than one backend at once,
chosen per request by the model name. Claude Code already speaks the Anthropic
Messages API, so the proxy speaks it too and forwards or translates each request:

- `claude-*` models (and the `opus`/`sonnet`/`haiku`/`fable` aliases) go to
  Anthropic as a transparent passthrough that reuses Claude Code's own
  subscription login. No API key, no translation.
- `gpt-5.6-*` and the other codex ids go to the Codex backend using the ChatGPT
  subscription that the Codex CLI already logged in.
- `kimi-*`, `grok-*`, and `cursor:*` ids go to their own translators.

The headline use is running the opus slot on a Claude subscription and the
sonnet slot on a ChatGPT/Codex subscription in the same session, switching
freely mid-conversation.

## Fork, upstream, and the trap between them

This is a fork of `fcakyon/claude-code-with-codex`, itself a fork of
`raine/claude-code-proxy`. Four remotes are configured:

| remote | repository | role |
| --- | --- | --- |
| `origin` | `null-topology/claude-code-mux` | this repository; pushes and `v*` tags go here |
| `fcakyon` | `fcakyon/claude-code-with-codex` | the parent fork; crate `claude-codex` on crates.io; fetch only |
| `upstream` | `raine/claude-code-proxy` | the original; crate `claude-code-proxy`; fetch only |
| `mine` | `null-topology/claude-code-proxy` | GitHub fork of upstream that holds branches for upstream pull requests; leave untouched until they merge |

`origin` is a plain repository, not a GitHub fork: GitHub allows one fork per
network per account and `mine` already occupies that slot.

`src/providers/anthropic/` and `AliasProvider::Anthropic` came from the parent
fork `fcakyon`, which has both and defaults the aliases to Anthropic just as
this fork does. `upstream` (raine) is the one with no passthrough at all: its
`AliasProvider` is `{Codex, Kimi}` and its registry routes `claude-*` to another
backend (codex by default). A "Claude through the proxy" test against an
upstream build silently goes to Codex. If a Claude-route test ever returns a
Codex-looking answer or a Codex rate limit, check which build is running first.
This fork's own additions to the passthrough are `UsageObserver`, the
`list_models` override and stream-error classification, not the provider itself.

Other differences when moving code between the trees:

- Upstream additionally has `src/providers/opencode/`, its own Codex
  browser/device/PKCE login under `codex/auth/`, an Astro docs site, and a grok
  hosted-search text projection. In this fork Codex sign-in is `codex login`
  from the Codex CLI. `fcakyon` has none of those either, except the Nix flake
  it still shares with upstream.
- Crate names differ in imports and test helpers: `claude_codex::` vs
  `claude_code_proxy::`, `Command::cargo_bin("claude-code-mux")` vs
  `Command::cargo_bin("claude-code-proxy")`. Ported test files need that edit.
- Never copy one `Cargo.lock` over the other.

`AGENTS.md` at the repo root lists the fork practices (stay close to upstream,
keep Claude aliases on Anthropic, Codex CLI auth, toy credentials in tests).
Changes adapted from upstream are credited in the `CHANGELOG.md` entry of the
release that carries them.

## Toolchain on this machine

`cargo` is not on the non-interactive shell PATH. The rustup shims live in
`/opt/homebrew/opt/rustup/bin` (stable, rustc 1.94.x); prepend that directory
to `PATH` before running cargo. `just`, `checkle` and `cargo-release` are not
installed, so run the underlying cargo commands directly instead of the
`justfile` recipes. There is no Nix build: it fetched crates from crates.io
inside the sandbox and was removed; install is `cargo install --git` or the
prebuilt release binaries.

## Commands

```sh
export PATH="/opt/homebrew/opt/rustup/bin:$PATH"
cargo build
cargo test -- --test-threads=1                                 # full suite
cargo test --test server -- --test-threads=1                   # one integration binary (tests/server.rs)
cargo test --test smoke_cutover websocket -- --test-threads=1  # tests whose name contains a substring
cargo test --lib providers::codex::rate_limits                 # unit tests in src/, by module path
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo run -- serve --no-monitor --port 19000                   # plain server, no TUI
cargo run -- models --full
scripts/debug-proxy                                            # isolated instance: random port, verbose log, traffic capture, temp state dir
```

Run tests single-threaded. A few config tests mutate process-wide environment
variables and race under the parallel runner; `tests/smoke_cutover.rs` also
serializes its env-mutating tests with a file-local `env_lock()`. This is
pre-existing and unrelated to product behavior.

CI (`.github/workflows/ci.yml`) runs `just check-ci`, which is `checkle run all`
(`cargo fmt --check`, `cargo clippy --all-targets -D warnings`, `cargo build
--all`, `cargo test --all`) and then fails if the checks left uncommitted
changes. The pre-commit hook (`just install-hooks`) runs `checkle pre-commit`.

## Changelog and releases

`CHANGELOG.md` is the release notes, and keeping it is part of every change.
A pull request that changes anything a user can notice (routing, what goes on
the wire, configuration keys and defaults, CLI, monitor, `proxy.log` events,
install) adds its entry under `## Unreleased` in the same pull request. Write
each entry for someone upgrading: what changed, what it means for them, and
how to get the previous behavior back when there is a way. Refactors, tests
and CI-only changes need no entry.

A release, from an up-to-date `main` after its pull requests are merged:

1. One `build: release X.Y.Z` commit that bumps `version` in `Cargo.toml`,
   refreshes `Cargo.lock` with a build, and renames `## Unreleased` to
   `## vX.Y.Z (YYYY-MM-DD)`. A feature bumps the minor version, a fix the
   patch.
2. An annotated tag `vX.Y.Z` on that commit. Push `main`, then the tag by
   name. Never `git push --tags`: the local clone also holds upstream's tags.
3. The tag runs `.github/workflows/release.yml`. It takes the tag's section
   out of `CHANGELOG.md` and fails before anything is built when there is
   none, tests with `--test-threads=1`, builds prebuilt binaries for six
   targets, verifies `--version` matches the tag, and publishes a GitHub
   release whose notes are that section followed by the generated list of
   merged pull requests. It uses the default `GITHUB_TOKEN` and needs no
   secrets. Nothing is published to crates.io.

`just release` (cargo-release) is not installed here, so the bump is done by
hand.

Running a build without touching an installed one:

```sh
./target/release/claude-code-mux serve --port 18766 --no-monitor
CCP_TRAFFIC_LOG=1 ./target/release/claude-code-mux serve --port 18766 --no-monitor
```

## Architecture

Request path:

1. `src/main.rs`: clap CLI. `serve` is the default command and opens the
   ratatui monitor when stdout is a TTY; `--no-monitor` gives a plain server.
   Other commands: `models [--full]`, `<provider> auth {login,device,status,logout}`,
   hidden `demo`.
2. `src/server.rs`: axum router. Always `/healthz`, `/v1/messages`,
   `/v1/messages/count_tokens`, `/v1/models`, `/v1/models/{model_id}`.
   Optional OpenAI-compatible
   surfaces gated by `AppFeatures`: `/v1/responses` and `/v1/chat/completions`
   (`CCP_CODEX_RESPONSES_API`; a buffered chat completion reads at most
   `MAX_PROVIDER_STREAM_BYTES`, 32 MiB, of the Codex answer; a streaming one
   (`chat_completions/stream.rs`) has already sent its 200 status line, so
   it ends the stream with an SSE error event of type `api_error` followed
   by `data: [DONE]` and reads no further once the bytes it holds without a
   complete SSE frame pass `MAX_HTTP_SSE_FRAME_BYTES` (8 MiB) or a text
   delta would take the output text it keeps (`CompletionState::text`) past
   `MAX_PROVIDER_STREAM_BYTES`, that delta not appended; neither
   surface's error text names the request URL), `/v1/images/*` (`CCP_CODEX_IMAGES_API`),
   `/v1/audio/transcriptions` (`CCP_CODEX_TRANSCRIPTIONS_API`).
   `dispatch_request` handles the Anthropic routes: read the body, normalize
   the model (strip `[1m]`), look up the session by `x-claude-code-session-id`,
   pick a provider via the registry, apply the auto-review override (Claude
   Code's non-streaming, tool-free security classifier is rerouted to
   `CCP_AUTO_REVIEW_MODEL`, default `gpt-6-luna` on codex), then call the
   provider and record log, monitor and traffic capture. A `/v1/messages`
   body with a `thread` object whose `type` is a string (message threads
   beta, `create` or `continue`) and whose final provider is not anthropic is
   refused before the provider runs with a 400 whose message starts
   `capability_rejected: beta_header:message-threads-2026-08-12`, on which
   Claude Code drops threading for the session and resends the full history:
   a `continue` holds only the newest messages, and a `create` served off
   Anthropic would anchor the thread on an id Anthropic never issued. The
   refusal calls `provider_selected` for the monitor and records no session
   state. A threaded request whose model routes to anthropic is never
   rerouted (`apply_auto_review_model` and the `upstream` label route skip
   it) and is relayed untouched. Every response `dispatch_request`
   returns, early rejections and the local agent-summary answer (with
   `CCP_AGENT_SUMMARY=local`) included,
   carries a `request-id` header with the proxy's `req_id` unless the upstream
   already set one (the passthrough relays Anthropic's); Claude Code records it
   as `requestId` in transcripts. The stamping lives in `dispatch_request`, not
   in `monitor_response_body`, so the OpenAI-compatible surfaces do not get it.
   `handler_models` (`/v1/models`) answers Anthropic List Models: exactly
   `data`, `first_id`, `has_more` and `last_id`, each row an Anthropic
   `ModelInfo`. `collect_model_rows` takes every registered provider (or the
   one named by `?provider=`, unknown name 400) and asks it only when
   `listing_required()` holds (anthropic) or `has_credentials()` does, the
   check running on tokio's blocking pool. A provider not asked adds
   no rows; when it was named, the answer is 502 `api_error` `<name> model
   listing unavailable (no saved login)`. Each asked provider gets
   `list_models_for` with a `ListingRequest` (the client headers and the raw
   `lifecycle` / `lifecycle[]` query pairs from `lifecycle_pairs`). Each
   provider's check and listing are one future, and those run concurrently
   (`join_all`), so a slow check delays only its own provider; the listings
   are consumed in registry order
   once all have finished: each goes to `record_listing` in that order, and a
   listing that is not ok answers 502 `api_error` `<provider> model listing
   unavailable (<status>): <detail>`, the first failure in that order
   winning. `dedupe_model_rows` keeps one row per id (the
   one of the provider routing picks when two list it, else the first);
   Claude ids are listed like any other. The aggregate goes in
   `list_provider_names` order, sorted by name, so anthropic's rows come
   first. `ModelsQuery::parse` reads the raw query string, so every rejection
   is an Anthropic JSON error: `limit` 1-1000 (absent means every row, not
   Anthropic's 20), `after_id` and `before_id` never together (an empty value
   counts as absent), `lifecycle` repeated, `lifecycle[]` or comma-separated
   over `active|deprecated|retired`; other parameters are ignored.
   `page_model_rows` resolves the exclusive cursors over every deduped row
   (400 for a cursor that names no row), keeps the rows past the cursor whose
   own `lifecycle` string `ModelsQuery::keeps` accepts (`model_row_lifecycle`:
   absent, non-string or any string other than `active`, `deprecated` or
   `retired` means `active`; without the parameter `active` and `deprecated`
   pass), then
   applies `limit`; `has_more` means kept rows remain in that direction.
   `handler_model` (`/v1/models/{model_id}`) runs the same listing pass with
   the client headers, no lifecycle pairs and no lifecycle filter, and the
   same failure rule, and returns the row the list would show, or 404
   `not_found_error` with `model: <id>`; an id containing `/` works literally
   or as `%2F`. It is served by two routes, `/v1/models/` and the catch-all
   `/v1/models/{*model_id}` (axum's catch-all matches no empty remainder),
   and takes the path as `Result<Path<String>, PathRejection>`: an empty id or
   one that does not percent-decode to UTF-8 answers that 404 with the path
   as sent and runs no listing pass. Both handlers stamp a fresh `request-id`
   through `with_request_id`, errors included.
3. `src/registry.rs`: model to provider. `ANTHROPIC_STYLE_ALIASES` and any
   `claude-*` id go to the alias provider (`CCP_ALIAS_PROVIDER`, default
   anthropic); `cursor:` prefixes go to cursor; anything else must match a
   provider's catalog exactly: the ids its last successful listing named
   (`LISTED_MODELS`), else its compiled-in list until a listing has arrived;
   a `-fast` suffix on a listed Codex id also routes. An id a compiled-in
   list names belongs to that provider whatever another backend lists; an id
   only listings name goes to the provider that has listed it the longest,
   never by map order. An id nothing routes
   refreshes the listings once (`provider_for_model_or_refresh`, see "Codex
   model inventory"), then unknown ids return 400 listing the catalog.
   The `CODEX_MODELS`, `KIMI_MODELS`, `GROK_MODELS` lists here are duplicated
   in each provider's `translate/model_allowlist.rs`. Keep them in sync.
4. `src/provider.rs`: the `Provider` trait, `RequestContext` (request id,
   session, traffic, monitor, and `Passthrough` with the raw bytes and headers
   for byte-exact relays), `ProviderError`, `CliHandlers`, and `ModelListing`
   (what `list_models` returns: `auth`, `source`, `status`, `detail`,
   `fetched_at`, which the `models` command and the 502 messages read, and
   the rows; a row the proxy builds comes from `model_info` or
   `model_info_in_line`, which also sets `line`, the constructors of an
   Anthropic `ModelInfo`, with `ModelCapabilities` writing the whole
   capabilities tree; the default impl advertises `supported_models()` as
   `source: bundled` with `capabilities: null` (`ModelListing::bundled`; kimi
   uses `bundled_with_limits` for its `max_tokens` cap), anthropic overrides
   to its relayed or catalog rows (`source: relayed` or `catalog`, `auth:
   client`), codex overrides to ask its backend). `list_models_for` takes a
   `ListingRequest` and defaults to `list_models(headers)`; anthropic
   overrides it to pass the lifecycle pairs on. `listing_required` (default
   false, true for anthropic) makes `/v1/models` ask a provider that holds
   no saved login and fail when its listing is not ok.
5. `src/providers/<name>/`: one directory per backend with the same shape:
   `auth/`, `client.rs`, `translate/` (`request.rs` Anthropic to native,
   `stream.rs` and `reducer.rs` native events to typed events,
   `accumulate.rs` for non-streaming, `model_allowlist.rs`), and
   `count_tokens.rs` (local estimate, no upstream call).
   `translate_shared.rs` holds the shared `ContentBlock` model and the
   `REASONING_OPEN` / `REASONING_CLOSE` tags used when replaying reasoning
   across backends.
   - `anthropic/`: transparent reverse proxy to `api.anthropic.com`. Forwards
     the `Passthrough` bytes verbatim; the only rewrite is turning a
     signature-less `thinking` block into tagged text. Holds no credentials.
     `models.rs` builds the Claude rows of `/v1/models` (see "Codex model
     inventory").
   - `codex/`: by far the largest. Maps Anthropic Messages onto the OpenAI
     Responses API. A response that ended incomplete finishes with
     `stop_reason: max_tokens`, or `refusal` when its
     `incomplete_details.reason` is `content_filter` (`incomplete_stop_reason`
     in `translate/reducer.rs`, shared by the buffered and live paths).
     Transport (`CCP_CODEX_TRANSPORT` = `auto|websocket|http`)
     defaults to `auto`: a WebSocket from a per-conversation pool in
     `websocket.rs`, falling back to HTTP only when the handshake failed
     before anything was sent, or when the request was refused for its size
     (`should_fallback_to_http` in `client.rs`). A request already sent over
     the WebSocket is never replayed over HTTP, with two exceptions below,
     and `websocket` never falls back. The size refusals follow the Codex
     CLI (`codex-rs/codex-api/src/endpoint/responses_websocket.rs` at
     `ed7ca22bb6`, the HTTP fallback at `bc3ccba6e1`); whether the backend
     sends the header is not established. A successful handshake's
     `x-codex-websocket-max-message-bytes` (a trimmed positive integer;
     empty, zero or anything else means no limit) is kept on the
     `PoolEntry` (`advertised_max_message_bytes`) and logged at info as
     `codex_websocket_message_limit` with `reqId` and `maxMessageBytes` for
     a new socket that carries one. A request frame longer than the limit
     is never written (`PoolEntry::check_message_size`). A close with code
     1009 (message too big) seen before any text frame of the response is
     the same refusal, and the first exception: under `auto` that request
     goes out again over HTTP, as the Codex CLI treats that close. Both refusals
     carry status 413 and one of `WEBSOCKET_MESSAGE_TOO_LARGE_DETAIL` and
     `WEBSOCKET_CLOSED_MESSAGE_TOO_LARGE_DETAIL`
     (`is_message_size_refusal`); neither is a continuation retry. Either
     drops the socket like any failed attempt, so the conversation's next
     request opens a new socket and sends its full context. Under `auto`
     the request is sent over HTTP with its full context, which like any
     HTTP answer leaves no continuation state, and every later attempt of
     that client request (a 401 refresh) stays on HTTP
     (`post_codex_with_transport`); the next client request starts on the
     WebSocket again. The HTTP replay goes out only under a credential shown
     to be the one of the WebSocket attempt
     (`refuse_size_fallback_under_another_account`,
     `SizeRefusalFallback::same_credential`): the account id of that attempt
     and the one the HTTP attempt holds (the fresh credential read of the
     live path, a 401 refresh on either path) are both known and equal, or,
     with an id unknown on either side, the HTTP attempt's access token is
     the very string the WebSocket attempt went out with. The live stream
     publishes each attempt's token beside its account
     (`CodexWebSocketEventStream::access_token`), held in memory for this
     comparison only, in types without `Debug`. Otherwise nothing more is
     sent over HTTP, the size refusal itself is answered, and one
     `codex_websocket_size_fallback_refused` warning carries `reqId` only
     (`size_fallback_refused_fields`); a renewed token of the same account
     goes ahead. Under `websocket` it is answered
     once with 413 `request_too_large`, never resent, and the message adds
     that the `auto` transport sends such a request over HTTP; any other 413 of a
     size refusal (buffered `auto` whose HTTP fallback is off behind a
     WebSocket proxy) states only what happened
     (`map_codex_transport_error_to_response` in `mod.rs`). A 1009 after
     response text and any other close keep their handling, including the
     full-context resend that repairs a continuation
     (`should_retry_without_continuation`), for example after a delta
     (`previous_response_id`) ended with no terminal event. The second
     exception is a resend the proxy makes anyway, such as that repair: it
     is a new attempt, its frame is checked against the limit of the socket
     it would go to, and under `auto` one over that limit goes over HTTP
     instead of the WebSocket. In every other case a request already
     written to the WebSocket is not sent over HTTP. A close frame
     that ends a response logs `codex_websocket_closed` at info with
     `reqId`, `closeCode` (0 without a frame) and `textStarted`, never the
     reason text. WebSocket writes share the idle deadline
     of their path: the request frame (`send_request_frame`) and Pong
     replies that of the response wait, and the validation Ping that of the
     pooled-socket validation. A timed-out request frame or Pong answers as
     the response wait's own timeout (a request frame as a response that
     never started) with `WEBSOCKET_WRITE_TIMEOUT_DETAIL`, which
     `is_continuation_retry_error` and `should_fallback_to_http` leave out,
     so it is neither resent nor replayed over HTTP. Each fallback, live and
     buffered, logs a `codex_websocket_fallback_to_http` warning
     (`log_websocket_fallback_to_http`) with `reqId`, the handshake `status`
     (0 when none came), `class`, `usageLimit`, `retryAfter` and a `detail`
     holding the rejection text or connect error cut to 300 characters, or
     for a size refusal a fixed text naming only the frame length and the
     limit or the 1009 close, never anything from the request. For a size
     refusal `status` is the proxy's own 413, not a status the backend
     sent. A buffered request that fails logs a
     `codex_upstream_request_failed` warning whose `detail` is the error's
     `CodexError.detail` cut to 300 characters on a character boundary
     (`failed_request_log_detail`), null when it has none. A connect that
     got no answer (`ConnectAttemptError::Unreachable`:
     before the connect timeout the upgrade's response head, a proxy's
     status line or the tunneled destination's handshake response was not
     complete, or the connection or the TLS handshake failed) made for a
     request that falls back
     to HTTP pauses new connections to its target (scheme, host and port,
     `connect_pause_key`) for `WEBSOCKET_CONNECT_PAUSE` (5 min) in the
     process-wide `WS_CONNECT_PAUSES`, and logs one
     `codex_websocket_connect_paused` warning with `reqId`, `seconds` and
     the connect error as `detail`. Whether something answered is the
     `response_head` flag `connect_with_policy` hands each attempt: the
     upgrade sets it once reqwest returned the response head, the CONNECT
     tunnel once the proxy's status line is complete, cleared again when the
     tunnel opened, after which only a complete handshake response from the
     destination ends the attempt. So a timeout while a rejection body or
     the rest of a proxy's response head is still being read starts no
     pause, and the caller gets the same timeout error. Failing to reach a configured proxy (its TCP
     connection or TLS handshake, or a SOCKS connection, which reqwest
     hands to `reqwest_handshake_error` as a connect error) counts as no
     answer and pauses the backend's target; a proxy that answers with a
     status (a 407, a refused CONNECT tunnel) does not. While it runs, such a request that would
     open a new connection there, a reconnect after a pooled socket failed
     validation included, makes no connect attempt and fails as a handshake
     with status 0 and `WEBSOCKET_CONNECT_PAUSED_DETAIL`, which the usual
     fallback sends to HTTP at once (`connect_with_timeout`,
     `OnConnectFailure::FallBackToHttp`); a usable pooled socket still
     serves its conversation. The pause is checked before the connect gate
     and again after the gate's wait, right before the attempt
     (`ConnectAttemptError::Paused`, which leaves the gate unstamped), so a
     request that waited while another one started the pause is skipped
     the same way. Those requests are the live `auto` path until its task
     has sent the client wrapper an item, and the buffered one whose HTTP
     fallback is on. The wrapper falls back only on its first item, so a
     later live attempt of a request that already sent one (the
     full-context resend that repairs a continuation, the resend after a
     token refresh) connects with `OnConnectFailure::Fail`
     (`coordinate_live_websocket_events`). Such an attempt, `websocket`
     transport and a buffered `auto` with the fallback off connect every
     time a new connection is needed and start no pause. A rejected
     upgrade with a status, a 407, a refused proxy tunnel, a write timeout
     and a size refusal start none either, and a pause already running is
     left as it is, with no second warning. A successful connect to the
     target, from any of those paths, ends its pause
     (`end_connect_pause`); between that and an unanswered connect the one
     that finishes last decides. Once the pause ends the next request tries
     again. The Codex CLI instead keeps its
     fallback for the rest of the session (`codex-rs/core/src/client.rs` at
     `4460449921`). Claude Code's compaction summary goes over
     HTTP whatever the setting (`transport_for_request` in `mod.rs`), and so
     does the opt-in server compaction call (`request_compaction` in
     `compaction.rs`).
     `continuation.rs` keeps `previous_response_id` state keyed by
     `ConversationIdentity` (session plus agent headers) so subagents do not
     clobber each other (a live stream whose events, or whose output held
     for translation, went past `MAX_BUFFERED_RESPONSE_BYTES` leaves none);
     `compaction.rs` does
     server-side compaction;
     `events.rs` classifies stream failures and recognises the
     `usage_limit_reached` error; `rate_limits.rs` keeps the newest
     `codex.rate_limits` reading of one ChatGPT account and publishes it as
     `anthropic-ratelimit-unified-*` headers (see below). Auth reads the Codex
     CLI's `~/.codex/auth.json` (`CCP_CODEX_AUTH_FILE` overrides the path).
   - `kimi/`, `grok/`: Chat Completions / Responses translators with their own
     OAuth. `cursor/`: Connect protocol over protobuf. These three and the
     Anthropic relay never pass on a `reqwest::Error`'s own text, which ends
     with ` for url (...)` and may hold a credential from a configured URL:
     they use `upstream_http::error_text`, or `url_free_error` where `?`
     carries the error on; both keep only reqwest's top-level text, never its
     source chain. Each body they hold is read under a cap named in its
     file, enforced while reading: 1 MiB for a token, device, login poll or
     refresh answer, 64 KiB for an error body, 8 MiB for a Kimi or Cursor
     response held whole, `MAX_METADATA_BYTES` (256 KiB) for Grok's OIDC
     metadata. A body over its cap fails as an unreadable one does at that
     place, except a Kimi response: over the cap it answers 502 `api_error`
     naming the cap, while an unreadable one is still taken as empty. A
     Cursor answer of status 400 or more keeps its status mapping and loses
     only its message when its body is over the cap or unreadable
     (`read_run_response`). A blocking body read (`read_blocking_body_capped`
     and the helpers over it) takes a `deadline`, the timeout of the client
     that sent the request, from one constant per client file
     (`REQUEST_TIMEOUT`, `HTTP_TIMEOUT`) that the builder uses too: reqwest's
     blocking `Read` restarts its timeout on every read, so the helper starts
     no read past the deadline and fails with `BODY_READ_TIMEOUT_TEXT`.
     Cursor's gzip frames are inflated through `connect::GzipFrames`, at
     most `MAX_GZIP_FRAME_BYTES` (32 MiB) per frame and
     `MAX_GZIP_RESPONSE_BYTES` (64 MiB) for the frames of one response, since
     the events built from them are held together; past either the frame
     fails as an undecodable one. The
     passthrough's relayed body is streamed, not held, and has no cap. Their
     login and status commands print provider-supplied text (verification
     URL, user code, account id, email, error code or body) through
     `upstream_http::escape_control_chars`, which escapes what
     `char::is_control` matches (Cc only, so not U+202E); text the proxy
     composes itself is printed as is.
6. Cross-cutting: `session.rs` (in-memory sessions, 30 min idle TTL, provider
   affinity), `request_identity.rs` (Claude Code session and agent headers),
   `retry.rs` (backoff on 429 and 5xx), `monitor.rs` + `tui.rs`, `traffic.rs`
   (`CCP_TRAFFIC_LOG=1` writes full captures under the state dir),
   `logging.rs` (JSONL `proxy.log`, redacts known keys, cuts a string field
   over 4000 bytes on a character boundary unless verbose; past
   `MAX_LOG_BYTES` (20 MiB) the log is renamed to `proxy.<millis>`, and that
   file and the `MAX_ROTATED_LOGS - 1` (4) newest other rotated files of it
   are kept, the rest deleted), `paths.rs` (config
   dir `${XDG_CONFIG_HOME:-~/.config}/claude-code-proxy`, state dir
   `${XDG_STATE_HOME:-~/.local/state}/claude-code-proxy`, macOS included; on
   Windows, keyed on the `std::env::consts::OS` value `windows`,
   `%APPDATA%\claude-code-proxy` and `%LOCALAPPDATA%\claude-code-proxy`, else,
   for each variable unset or empty on its own,
   `<home>\AppData\Roaming\claude-code-proxy` and
   `<home>\AppData\Local\claude-code-proxy`, every separator added there a
   `\`, and `\AppData\...` when the home is empty or only separators (the `/`
   that stands in for no `HOME` or `USERPROFILE`); `CCP_CONFIG_DIR`
   overrides only the config dir; `CCP_CONFIG_DIR`, `APPDATA`,
   `XDG_CONFIG_HOME`, `LOCALAPPDATA`, `XDG_STATE_HOME`, `HOME` /
   `USERPROFILE` are read with `var_os`: a
   non-UTF-8 path applies as set), `config.rs` (precedence: `CCP_*` env, then
   `config.json` in the config dir, then default; settings read as text come
   from `env_vars`, where a variable whose name or value is not valid
   Unicode reads as unset, and a `PORT` or `CCP_*` one with such a value is a
   `config summary` warning that names it, except the path variables read
   with `var_os`, `paths::PATH_ENV_VARS` and `OS_PATH_ENV_VARS`
   (`CCP_CODEX_AUTH_FILE`); the Codex client's `ProxyEnvironment::from_env`
   reads `HTTP_PROXY`, `HTTPS_PROXY` and `ALL_PROXY` with `var_os`, the
   uppercase spelling when set, else the lowercase one, and panics with
   `invalid <NAME> proxy URL` on a value that is not valid Unicode rather
   than connect around the proxy; it skips a `NO_PROXY` or `no_proxy`
   spelling that is not valid Unicode, so the other one or none applies).
   `read_file_config` keeps
   the parsed file per path in a process-wide cache for `CONFIG_FILE_TTL`
   (30 s), a missing file (`NotFound`) included as `None`, so an edit lands
   within 30 s. Any other read error and a file that does not parse are not
   cached: that call gets `None` and the next one reads the file again, so a
   file caught mid-write costs one request. A test that rewrites a config it
   has already read calls `clear_config_file_cache_for_tests` (the
   `write_config` helper does).

`serve` logs `config::config_override_summary` once at startup
(`log_config_summary` in `main.rs`, after `suppress_stderr` in monitor mode):
one `config summary` event per line on the `server` logger, the line in the
`line` field. `ConfigSummaryLine::ignored` (a source whose value was unusable
and skipped) picks warn, everything else info. The logged form is
`ConfigSummaryLine::logged`, which names a setting and its source and never a
value (`port (config)`); `text`, which `config_override_summary_lines` returns
and the config tests read, may carry a file value (`port: 8080`).

The Codex lane setting has a chain of its own in `config.rs`:
`CCP_CODEX_LANE_POLICY` (`full|inventory`, default `full`), then the legacy
`CCP_CODEX_FULL_LANE`, then `codex.lanePolicy`, then the legacy
`codex.fullLane`. The first source that parses wins. For both env keys and for
`codex.lanePolicy`, a value that does not parse is invalid rather than unset:
it is skipped and reported by `config_override_summary_lines`, which names the
source without echoing what was set, and logged at startup as a `config
summary` warning. `codex.lanePolicy` is deserialized as a
raw `Value`, so an unusable value there is ignored on its own instead of
failing the whole file; the legacy `codex.fullLane` keeps its boolean grammar
and a wrong type there still fails the whole file in serde, which is why its
invalid arm is unreachable and reports nothing.

All conversation state (sessions, continuations, compaction, WebSocket pool)
lives in process-wide statics. A restart clears it.

## Monitor token accounting

Usage is tracked in the Anthropic shape: `input_tokens` is uncached, cache
read and cache write are separate (`RequestCache` on each request), and the
prompt size is their sum. Providers report usage in one of two ways:

- Legacy `stream_progress(input, output)` / `usage_updated(input, output)` /
  `request_completed(input, output)` values only ever raise a count. Upstream
  added that rule (a369bd3) to ignore stale updates; kimi, grok and cursor
  still use it.
- `stream_progress_usage` / `usage_reported` carry a `UsageReport`
  (`src/monitor/usage.rs`). Its `opening` values raise a count until a
  `closing` value arrives, which replaces it. The Codex translator's
  `message_start` input is an estimate of the whole prompt (upstream #86 keeps
  it on the wire for Claude Code's live counters), so the Codex provider parses
  SSE with `usage_report_from_anthropic_sse`, which treats `message_start` as
  opening and `message_delta` as closing. The Anthropic passthrough wraps the
  relayed body in `UsageObserver` (bytes untouched) and treats
  `message_start` prompt counts as closing, because Anthropic's are exact.

Each count carries its own quality, derived from the value held and whether a
closing observation produced it (`quality_of` in `src/monitor/usage.rs`,
`usage_quality` in `src/monitor.rs`): `Missing` with no value, `Opening` while
only an estimate has arrived, `Exact` once a closing value did, a reported zero
included. A missing count is not a zero, and a status never upgrades a quality:
a request that completes with no closing report stays `Opening`. The two
lifetime buckets of a cache write have their own `CacheWriteQuality` and are
closed only on their own reports — a bucket is never inferred from the write it
belongs to, from the other bucket, or from a TTL — so the buckets may not
reconcile with the aggregate write, which the backend reports independently;
they are also never added to the prompt or to the aggregate. Codex reports
neither cache writes nor buckets, so those stay `Missing` rather than zero.

`reported_prompt_tokens` keeps a full prompt total that a backend measured
itself, apart from the four categories: `prompt_tokens()` prefers it over their
sum, it is never added to them as a fifth category, and it says nothing about
the cached split, which stays unknown when only the total arrived.

Session totals apply signed deltas and skip `count_tokens` requests. Cache
misses are judged per lane (session, conversation from
`MonitorEvent::ConversationResolved`, provider, model) in
`MonitorStore::evaluate_cache` once a request's cache read is closed.
`detect_cache_miss` takes the expected prefix as min(previous prompt, this
prompt) and counts a shortfall of at least clamp(10% of it, 1024, 20k); a
prompt that shrank by more than 1024 tokens is a client rewrite (compaction,
clear, rewind) and only resets the baseline. The cause is `expired` when the
start-to-start gap exceeds the lifetime of the previous request's entries
(Anthropic `cache_creation.ephemeral_1h/5m`, else 5m; Codex 30m), `within ttl`
otherwise. The store skips judging:

- side lanes: when a request has no tool with `input_schema`,
  `server::monitor_conversation_label` appends the suffix of its kind
  (`side_request_kind`, `SideKind` in `src/monitor/side.rs`), first match
  wins: `/classifier` (the auto-mode classifier, by
  `is_claude_auto_review_request` or its system prefix), `/title` (an
  `output_config` JSON schema whose only property is `title`), `/search` (a
  hosted `web_search_*` tool), `/recap` (the walked-away recap system
  prefix), `/fetch` (no tools and a last user text ending with the WebFetch
  lyrics sentence, the weakest marker), else `/side`. Every consumer reads
  the kind through `split_side_conversation` / `is_side_conversation`, never
  a literal suffix, and every kind is a side lane alike: not judged, parented
  to its base conversation, ranked after it in the tree. Without this, web
  search calls on the main model reset the main lane's baseline and set the
  context size to a few thousand tokens;
- a request that started before the lane baseline's response began
  (`readable_from`), since Anthropic entries are readable only from then;
- a request that started before the baseline itself (it finished late; it
  also must not replace the baseline or the session's context size);
- a lane with no cache read or write so far, unless `caches_implicitly`
  (Codex never reports writes).

A model switch is not labelled: the new model's lane has no baseline. Doing it
reliably needs the previous request of the same conversation and a guard for
side calls on other models. Do not switch cursor to the report API: it sends
`cache_read_input_tokens: 0` always and would produce false misses.

Whether a response counts as a failure is the shared `ResponseOutcome`
(`src/provider.rs`). The status line leaves before the body does, so a producer
records a mid-stream failure there and the server reads it when the body ends.
A protocol that ends with a terminal event uses `requiring_terminal`: for an
observed Anthropic Messages SSE stream — the passthrough's relayed body and the
Codex live stream both — an `error` event or a body that stopped before
`message_stop` is recorded as Failed even though the client already received
HTTP 200. The first failure wins, so a semantic cause is not replaced by a
later transport error, nor erased by a healthy-looking finish.

`src/monitor/accounting.rs` is the persistent ledger behind the recent list.
The recent list holds the last few hundred requests in full; the ledger keeps
one compact numeric record plus small metadata per request id, so a report
arriving after a request was evicted still lands on its record and corrects
every session, conversation and model total it fed. The live Codex path needs
that: it hands the response to the client before the stream ends, so the
backend's own counts can arrive arbitrarily late. A record lives as long as
its session: `MonitorStore::drop_idle_sessions` removes a session with no
request in flight and no activity for `SESSION_DROPPED_AFTER` (24 h), with its
conversations, its records and its cache lanes, through
`Ledger::drop_sessions_idle_since`. It runs on every `snapshot()` and at every
`RequestStarted` before the ledger sees the request, so a request for a session
id idle that long starts a new session (fresh counters, a new rank, no old
cache baseline) whether or not a snapshot ran in between. A later usage report
for a dropped request finds no record and changes nothing. A terminal event for
an unknown id still starts a record, because `Ledger::finish` calls `start`
first; that is the terminal-first path, kept on purpose. So the map grows with
the requests of the sessions still in use, not with the whole run. Records hold
numbers and small metadata only — no prompt, no request or response body. The
one piece of output text they keep is a session title taken from a title reply
(`RequestRecord::session_title`, `SessionRecord::wire_title`), beside the names
read from transcripts. Every mutation goes through `Ledger::update`, which detaches a
record's contribution from the rows it feeds, applies the change, and attaches
it again, so a row is always the sum of the records attached to it; one signed
`UsageDelta` path carries the numbers, and a change worth nothing numerically
still counts, because a missing count arriving as a reported zero moves the
evidence behind a total. Metadata is ordered by `(started_at, rank)` and a row
shows the metadata of the request with the greatest order, so a late report for
an older request cannot take a row's model or status back; a session's project,
its wire title and a conversation's parent have independent watermarks
(`project_order`, `wire_title_order`, `parent_order`) because a request states
them separately from being routed. In `note_metadata` a `None` never erases a
value already known, with one exception: the worktree is part of the project
statement and rides on `project_order`, so a newer project statement replaces
both and one without a worktree clears it.

A session's name (`SessionRecord::name`) is, first available: the last
`custom-title` line of its Claude Code transcript (`/rename`), the last
`ai-title` line, then a title seen on the wire; `SessionSummary::display_name`
falls back to the project, as `project · worktree` when the session runs in a
worktree, and the TUI to the session id (`project.rs` only ever names a
worktree together with a project). Every name is normalised to one line (each
run of whitespace and control characters becomes one space), an empty one is
rejected, and it is cut to 120 characters (`clean_session_name`).
The transcript is `$CLAUDE_CONFIG_DIR` (else `~/.claude`)
`/projects/*/<session id>.jsonl`, the one with the newest mtime when several
project directories hold it, read by `TranscriptReader`
(`src/monitor/naming.rs`) in a task `serve_listener` spawns when a monitor
exists: every 5 s, on tokio's blocking pool, for the sessions the ledger holds
(`session_generations`), from the offset after the last complete line, keeping
no line over 16 KiB and parsing only lines that mention a title. The reader
forgets a session a poll no longer sees; a session dropped and started again
under the same id between two polls stays, and the new session's rank (its
generation) tells the reader to hand the names it holds over again. A poll that
panics loses the reader's state: the loop logs one warning with no name, title
or path and goes on with a fresh reader. A missing file is looked for again
after a minute; a file that shrank is read again from the start. Names reach
the ledger as `TranscriptNamesRead`, which only fills a session that exists and
never erases. The wire title comes from Claude Code's session-title request (the
`/title` kind): `UsageObserver` in the Anthropic passthrough keeps up to 4 KiB
of the reply text, only for a request `sanitize_anthropic_request` flagged
with `is_session_title_side_request` (in `side.rs`, the predicate the `/title`
label uses too: no client tools and a title-only `output_config` schema),
and publishes `SessionTitleObserved` once the reply completed with valid
`{"title": ...}` JSON. It reads the relayed bytes only; the relay stays
byte-exact. The Codex route observes no title. Names live in memory only and
the proxy logs none; a traffic capture holds the title reply only as part of
the response it records anyway.

`src/project.rs` returns a `ProjectName { project, worktree }`. On disk, a
`.git` file whose `gitdir` sits under `<main>/.git/worktrees/` names the main
repository as the project and the checkout's directory as the worktree (a
submodule's `.git` file gets no worktree). A path that is not on this machine
and contains `.claude/worktrees/<name>` names the directory before `.claude`
as the project and `<name>` as the worktree.

Requested and effective model are tracked separately. `ModelRequested` captures
the id the client's body named, before the agent-summary and auto-review
overrides, the one-hour suffix and any provider alias; the first naming wins and
a request that never reached a provider still says what it asked for.
`ModelResolved` is published only by a producer that saw the outgoing request
built, so it names the model of a genuine upstream request — which is not proof
that the backend accepted it. The `requested → effective` string is display for
the visible row; no total is keyed on it. The rollups are keyed on provider plus
effective model (`SessionSummary.models`, `ModelKey`) and carry a histogram of
the requested ids that fed each row. Paths that build no upstream request name
no effective model: the local Codex, Kimi and Cursor `count_tokens` estimates,
the
locally answered agent summary (with `CCP_AGENT_SUMMARY=local`; provider
`local`, whose synthetic counts stay
out of every token total), and Cursor's tool-bridge and auth-failure early
returns. Anthropic's `count_tokens` is a genuine relay and may name one. Kimi
names one only once the translated request exists, so a body rejected in
translation has none. A `[1m]` suffix is reported as the string the client sent,
as evidence of what went on the wire rather than as a one-hour cache marker.

The TUI renders this. `src/tui.rs` marks every count with its quality (`~`
opening, `n/a` missing, plain exact, a reported zero included), names the
executed model on request rows and marks a routed-only one with `?`, labels a
locally answered request `local answer` in every cell (an agent summary is
answered locally only with `CCP_AGENT_SUMMARY=local`), shows the session root
as `Σ <id>` with `mixed N` when the rollups name more than one model, shows
the session's display name in the `Session` column, lists the name with its
source spelled out (`rename`, `auto`, `wire`), the project with its worktree,
the `models` rollups, the `unattributed` row and an `evidence` line in the
session detail, prints the 5m/1h buckets as parts reported separately, marks a
conversation whose `raw_parent` was never resolved with `^`, and keys the
selection by row identity so a re-render keeps it (`(selection reset)` in the
pane title when the row is gone). Meaning is never carried by color alone.
The `demo` command shows all of it from `src/monitor/mock.rs`.

`session_summaries` orders sessions by activity: sessions with a request in
flight first, then by `last_seen` descending, then by `first_seen_rank`
descending as the tie-break, whichever model or conversation made the latest
request; the conversations under a session keep `order_conversations`' tree
order. `MonitorStore::snapshot` then leaves out a session with no request in
flight and no activity for `SESSION_HIDDEN_AFTER` (1 h, `hide_idle_sessions`)
and counts it in `MonitorState::idle_sessions`, which the Sessions pane title
shows as `idle hidden: N`; its data stays in the ledger and its next request
brings it back whole. The header's session count is the visible sessions. The
store's clock is `MonitorStore::now`, which tests move with `clock_offset`.
`apply_window_rate` skips a recent request older than its session's
`first_seen`, which belonged to an earlier session dropped under the same id.
The bottom pane is tabbed (`BottomTab`: `Events`, `Stats`); Tab cycles
`FocusPane` Sessions → Recent → Bottom, and while Bottom has focus Left/Right
switch the tab (`MonitorApp::navigate`) and Up/Down/j/k scroll it, with no
row identity to keep. The title brackets the shown tab (`[Events] Stats`).
The Stats tab renders `MonitorState::model_stats`: one `ModelStats` per
(provider, effective model), summed over `MonitorState::models`, with `local`
rows left out. Those come from `Ledger::measured_usage`: the ledger's own
per-model rows (`measured`, fed in `apply_contribution` beside the session
rollups) plus `retired`, where `drop_sessions_idle_since` moves a dropped
record's contribution, so Stats covers every request since the proxy started
whether its session is shown, hidden or dropped. A `count_tokens` request runs
no model and is left out of Stats entirely (`RequestRecord::ran_on_model`, and
the median window skips the endpoint): the local Codex estimate names no
effective model and used to make a `codex/-` row, and Anthropic's relayed one
inflated its model's `Reqs`. It stays in the session rollups, so a session's
`models` still add up to its figures. Its cache miss tally is `CacheMissTally`
on `ModelUsage`, fed by `Contribution.miss` from the `CacheMiss` a request's
evaluation stored on its record, so a per-model miss is counted exactly once
and detached with the record like every other number; the judging itself is
unchanged. Prompt is `input + read + write` (no `reported_prompt_tokens` at
this level), hit % is `totals_cache_hit_ratio` over the row's `hit_basis`,
a Codex row's cache write stays `Missing` (`n/a`) rather than zero, and
`Lat(rec)` / `tok/s(rec)` are medians over the row's completed requests
still in `recent` (the mean of the two middle values for an even count), so
they cover the recent window only and the header says so. Rows sort by
prompt tokens descending.

Every aggregate hit % (session, conversation, Stats row) is read off
`HitBasis` (`hit_basis` on `RowCounts` and on the summaries), never off the
token totals. It sums input, read and write over the requests that carried
history: `dispatch_request` publishes `RequestWithoutHistory` when the parsed
body holds no `assistant` message (a new session or subagent, a one-shot side
call, the first turn after a compaction), whatever provider serves it, and
`Ledger::note_without_history` sets `RequestRecord::without_history`. What
such a request found in cache says nothing about how the conversation's cache
holds. Its tokens stay in every other total, and a row holding only such
requests has no ratio (`n/a`). A request not flagged as lacking history
counts as before: a body that did not parse, the OpenAI-compatible surfaces,
a terminal-first record. The per-request hit ratio and cache-miss judging are
unchanged.

Deferred and non-blocking: `RequestRecord::model_key` and the requested-model
histogram allocate `String`s on every ledger update.

The proxy sends `ConversationIdentity::cache_scope()` as the request's
`prompt_cache_key` and in the `session-id` and `thread-id` headers: the session
id for a main thread, a uuid v5 of session plus agent id for a subagent. Claude
Code gives a subagent its parent's session id, so without this every subagent
shared the main thread's key and its routing bucket, and OpenAI documents
overflow routing above about 15 requests per minute on one key.
`build_codex_headers` takes the scope from the translated body's
`prompt_cache_key`, so every transport and retry path sends the same value; a
request routed without a conversation identity (the auto-review classifier)
falls back to the session id. `session-id` plus `thread-id` with the same value
is the Codex CLI's spelling (`codex-rs/codex-api/src/requests/headers.rs:8-11`
at rust-v0.157.1); 0.13.0 and earlier sent one `session_id` header instead.
Whether the backend's cache affinity follows these spellings as it follows
`session_id` has not been established.

Turn state (`providers/codex/turn_state.rs`) uses the Codex CLI's transport
locations. The CLI keeps the first `x-codex-turn-state` value of a turn in a
per-turn `OnceLock` and sends it on every later request of that turn, never
across turns (`codex-rs/core/src/client.rs:270-298` at rust-v0.157.1): as an
HTTP header (`build_responses_headers`, :2224-2242) and on WebSocket as a key
of `response.create`'s `client_metadata` (:1903-1910). The proxy keeps one slot
per `ConversationIdentity`, in memory, and fills it with the first value of the
turn from one of two sources:

- the HTTP response header;
- `headers["x-codex-turn-state"]` of a stream event whose `type` is exactly
  `response.metadata` or `codex.response.metadata`. Any other event, `error`
  included, is ignored even when it carries `headers`. The CLI reads only
  `response.metadata` (`codex-rs/codex-api/src/sse/responses.rs` 64-70 for
  the header, 219-227 for the event) and uses `codex.response.metadata` only
  for `x-models-etag`
  (`codex-rs/codex-api/src/endpoint/responses_websocket.rs:745-763`). Codex
  WebSocket streams carry the value in `codex.response.metadata`, so reading
  it there goes beyond the CLI.

The WebSocket handshake response is not read, as the CLI's core passes no turn
state to the handshake (`codex-rs/core/src/client.rs:1234` and :1301), and the
handshake carries none. A request continues the turn only when its last user
message holds at least one `tool_result` and every other block in it is a
`tool_result` or a text block whose trimmed text starts with
`<system-reminder>` (Claude Code puts reminders next to tool results). Any
other text or content block there, such as a prompt typed after an interrupt
and merged next to the tool result, starts a new turn, which clears the slot
before the request is sent (`starts_new_turn`). Misjudging toward a new turn
only drops the echo. A request with no conversation identity, no client tool
with `input_schema` (titles and other side requests) or the progress-label
marker takes no part. A slot expires after 30 min with no tracked activity:
planning a request on it, storing a value in it or sending its value, each of
which refreshes `touched_at`. That is a bound, not proof that the turn ended:
expired slots are dropped when the next request is planned, the first request
of a turn planned after its slot expired goes out without the echo, and a
value its response carries is echoed from then on. Only the request
the slot was planned for (`req_id`) fills or sends it, so a late reply of an
earlier request cannot. The echo is added to the outgoing JSON after
`build_websocket_request`, never to `ResponsesRequest::client_metadata`, whose
presence marks the Lite lane, and never to the handshake.
`codex_upstream_request_started` logs `newTurn` and `turnStateSent`, never the
value. What the backend does with the echo is not established.

On the ChatGPT Codex backend the session header (then `session_id`) drives cache affinity:
byte-identical requests repeated 5 seconds apart hit the cache 4 times out of 4
with it and 1 time out of 4 without it (measured 2026-09-11). Even with it,
some re-sends miss at random. With gpt-5.6-sol, re-sends missed after 2, 6,
29 and 61 minutes and hit after 11, 20, 21 and 35 minutes, and after 45
minutes when read again at 20. With gpt-5.5 they hit at 6, 11, 21 and 35
minutes. A single Codex miss inside the lifetime
is not proof that the prompt changed.
Explicit cache controls (`prompt_cache_breakpoint`, `prompt_cache_retention`)
are rejected by the subscription backend for GPT-5.6 models (openai/codex
#35300, #39397).

## What the Codex subscription actually meters

Read the allowance window, never assume it. A `codex.rate_limits` event opens
every healthy Codex stream and names its own windows, and
`GET /backend-api/wham/usage` reports the same thing. Which slot holds which
window varies by plan: a weekly window (`window_minutes: 10080`,
`limit_window_seconds: 604800`) has been seen as `primary` with `secondary:
null`, and a 5h/weekly pair has been seen too. Match on the window length, not
on the slot. On the stream only the wire spelling `reset_at` /
`reset_after_seconds` occurs; `resets_at` / `resets_in_seconds` is a Codex CLI
log reserialization of the same data.

What the meter counts, fitted over a capture corpus of roughly 680 requests
against the integer `used_percent` readings that open each stream:

- **Total input, with cached tokens billed rather than free.** A model in which
  cached input costs nothing fits this corpus badly. How much cheaper a cached
  token is than an uncached one is not pinned down: the corpus is uniformly
  cached, so the two terms are barely separable and the fitted ratio spans from
  a large discount to none at all. Treat the discount as unmeasured; separating
  the terms needs traffic with a deliberately low hit rate.
- **A per-model weight, which is the largest single factor.** Normalised on
  `gpt-5.6-sol` = 1.00, `gpt-5.6-terra` fits near 0.7 and `gpt-6-astra` near
  3.5-3.9. A fit that ignores this returns nonsense, so any further measurement
  has to model it first.
- **Output and reasoning were negligible in this corpus** beside input, a
  fraction of a percent of the billed total and not separately resolvable.
  That is what these captures show, not a guarantee about how the meter bills.

For example, at one observed model mix a single percentage point of the weekly
window cost on the order of 5-6M input tokens counted this way.

So the cost tracks context size times request count times the model's weight.
Which model to run stays the user's choice, and keeping a stable cached prefix
is still a legitimate lever: this corpus prices neither a hit nor a miss, so
nothing here says cache stability is worthless. The proxy preserves cache
stability and avoids redundant model requests.

`src/agent_summary.rs` handles the progress label Claude Code asks for
periodically for each running background subagent, resending the subagent's
whole context. `CCP_AGENT_SUMMARY` (config key `agentSummary`) picks the mode:
the first source that parses wins, env then `config.json`, else `native`; an
empty or unrecognised value is skipped, and values are trimmed and
case-sensitive. `native`, the default, routes and relays the request like any
other for its model, with no rewrite beyond `tool_choice: none` on the Codex
route described below (`agent_summary_forwarded`), so the client
sees the label's real usage; the local answer reported zero input, so anything
reading usage saw a request of the wrong size. A native label can fail like any
request (429, 5xx, a spent Codex window). `local` answers it from the
transcript, built from the last tool call (provider `local`,
`agent_summary_answered_locally`). `upstream` (also `model`, `remote`) pins the
provider's junior model at `effort: low`, never the subagent's own
(`CCP_AGENT_SUMMARY_MODEL` overrides it in this mode only;
`agent_summary_routed`); on the Anthropic route the relayed bytes stay the
client's own, so the request Anthropic receives is unchanged and only the
proxy's own record names the junior model. The junior model
must still hold the subagent's context, which is why Anthropic's is
`claude-sonnet-5` and not Haiku (200k would drop the label on a long subagent)
and Codex's is `gpt-6-luna`; a provider without an entry in
`summary_model_for` keeps the request's model. Detection is the prompt text,
`SUMMARY_PROMPT_MARKER`: these requests otherwise look like a normal subagent
turn, with its tools and history. Detection skips trailing `role: "system"`
messages after the prompt, which Claude Code sends mid-conversation to carry
reminders. It also consults `x-claude-code-request-class`
(`request_class_allows_label`): any present value other than `auxiliary` rules
a request out, and the header alone is not enough because Claude Code sends
`auxiliary` on every side request (titles, prompt suggestions, the isolated web
search call); an absent header stays eligible. A label routed to codex, in
native and upstream mode both, gets `tool_choice: {"type": "none"}`
(`forbid_tool_calls`, applied in `dispatch_request` once the provider is
known) with its tools kept: the client only asks in prose, and measured on the
ChatGPT backend Codex models often answered a label with a tool call;
`none` is accepted and enforced by every listed Codex model and cache-neutral
because the tools stay. The Anthropic route is untouched: a `tool_choice`
change invalidates Anthropic's messages cache and the passthrough is
byte-exact.

## Invariants to preserve

- The Anthropic passthrough must stay byte-exact for normal traffic. The only
  rewrite it performs is converting a signature-less `thinking` block into a
  tagged `text` block, and it reserializes only when such a block is present.
  Anything that reserializes every request would evict Anthropic's prompt cache.
- Reasoning stays portable across a model switch. A `thinking` block written by
  one backend cannot be replayed to the other in native form (Anthropic rejects
  a signature-less `thinking` block; the Responses API has no `thinking`
  container). Both translators convert it to text wrapped in the shared
  `REASONING_OPEN` and `REASONING_CLOSE` tags via `wrap_reasoning`. Keep this
  deterministic so the rewritten prefix is byte-stable turn to turn.
- Codex credentials come only from the Codex CLI's `~/.codex/auth.json`. The
  proxy has no Codex login of its own and must never delete that file. Token
  refresh writes back to it so the Codex CLI keeps working (OpenAI rotates the
  refresh token on use). The writeback (`CodexCliAuthStore::save_refreshed` in
  `providers/codex/auth/token_store.rs`) holds `auth.lock` in the directory of
  the configured path (an exclusive `File::lock` waited for without a deadline
  on tokio's blocking pool, never removed or truncated) across reload, compare
  and write only. That is the Codex CLI's own lock only when the path is
  `$CODEX_HOME/auth.json`, so `CCP_CODEX_AUTH_FILE` must name that file. A
  symlinked `auth.json` is resolved under the lock, and the temp file and the
  rename go to the target's directory, so the link stays a link; a link whose
  target is missing is a changed login to `save_refreshed` and refused by the
  whole-login `save`, and stays a link either way. It writes
  only into a file that still holds the access token and the refresh token it
  refreshed from (and the same `account_id` when both are known). It changes
  only `tokens.access_token`, `tokens.refresh_token` and `tokens.id_token` when
  the response carried them (an `id_token` only when its claims parse with the
  types the Codex CLI reads), `tokens.account_id` when it was absent, and
  `last_refresh`, through a uuid temp file, `sync_all` and a rename. Of the
  token response only a non-empty `access_token` is required: an `expires_in`
  that is not a positive integer and an `id_token` or `refresh_token` that is
  not a string count as absent, and the expiry is the access token's `exp`,
  else `expires_in`, else 3600 s (`TokenResponse::expires_at_ms`). Outcomes:
  - a file it cannot read, that is not JSON, or whose root or `tokens` is not
    an object is left untouched and the refresh fails, with no stored-token
    fallback;
  - a missing file fails with the `codex login` message;
  - another login (a different access token, refresh token or `account_id`)
    drops the refreshed tokens, and the request uses the stored token while
    it is unexpired and, on the forced refresh, differs from the rejected
    token (`usable_after_login_change`);
  - a write that fails for an I/O reason (`WritebackError::Io`: `auth.lock`
    that cannot be opened or locked, encode, temp file, write, rename) logs
    one `codex_token_writeback_failed` warning with no fields and serves the
    refreshed token, leaving the file unchanged.
  Tokens whose write failed that way stay in `UNWRITTEN_REFRESHES` in
  `manager.rs`, in memory only and never logged, keyed by the store path (an
  `InMemoryAuthStore` path names that store and its clones) and matched on
  the login they replaced as their writeback matches it: the access and
  refresh token and, when both are known, the `account_id`
  (`RefreshedTokens::same_login`). While the file still holds that login,
  `load` and `load_locked` serve them and a due refresh exchanges their own
  refresh token and writes over the file's pair; once the file holds
  anything else, another `account_id` or a missing file included, they are
  dropped. They are
  written again at most once per `WRITEBACK_RETRY_INTERVAL` (30 s), due or
  not and before any backoff applies (`retry_write_back`). Between retries
  `get_auth` serves them without the single-flight guard or `auth.lock` while
  they are not due; once they are due it takes the single-flight guard for
  the exchange or the backoff check, and no `auth.lock` while that refresh
  backs off. A retry that fails for an I/O reason keeps them for another
  interval and logs nothing, one that finds another login drops them, and one
  that finds a document it cannot use (unreadable, not JSON, no `tokens`
  object) drops them and the request fails as it would without them. After a
  retry the store is read again under `auth.lock` before any exchange. When
  the token endpoint refuses their refresh token (401 or 403 for the same
  login, or any other status that is not retried), they stay while their
  access token is unexpired and are dropped once it has expired
  (`keep_unwritten_after_refusal`). The exchange, the writeback and the retry
  run in a `tokio::spawn`ed task that owns the single-flight guard
  (`lock_owned`), so a caller dropped mid-request neither frees the lock early
  nor loses the tokens; no `std` mutex is held across an await. A writeback
  that finds a changed login reads the store again with `load_locked`.
  `load_locked` itself falls back to a plain read when `auth.lock` cannot be
  taken, as the Codex CLI's own read takes no lock
  (`codex-rs/login/src/auth/storage.rs` at rust-v0.166.0) and a read-only home
  must keep working.
  The same rule for another login covers a stored token that changed before
  the request went out and the re-read under `auth.lock` after the token
  endpoint answered 401 or 403. A proactive refresh that fails for any reason
  while the stored token, read again under the lock, is unexpired keeps using
  it with one `codex_token_refresh_failed_using_stored_token` warning carrying
  only `class` and `status`, decided in the spawned task so that a dropped
  caller still logs it once. The two failures outside that task, a file the
  unlocked reload before it cannot read and a task that did not finish
  (`refresh_task_answer`), fall back the same way in the caller without
  recording a failure in `PROACTIVE_REFRESH_FAILURES`; after the failed
  reload, a token the re-read under `auth.lock` finds not due is served with
  no warning, as no refresh was tried (`after_failed_unlocked_reload`). The
  forced refresh after an upstream 401 never falls back.
  `PROACTIVE_REFRESH_FAILURES` remembers the last failed proactive refresh per
  store path and access token: the proactive path then serves the unexpired
  stored token without an exchange for `TRANSIENT_REFRESH_BACKOFF` (60 s)
  after a transient failure, and after any other failure until the access
  token changes; the forced path neither reads nor writes it. A transient
  failure (the endpoint unreachable, a timeout, 429, 5xx, a refresh task that
  did not finish, `TransientRefreshError`) answers the client 503 `api_error`
  with the fixed message `Codex token refresh is temporarily unavailable`
  (`auth_error` at every `get_auth` site in `client.rs`, `auth_refresh_error`
  at every `force_refresh` site there); any other failure answers 401. The
  model listing maps the same split to `ModelsError::Unreachable` and
  `Unauthorized` (`auth_models_error` in `models.rs`). No refresh error
  carries the token endpoint's response body. That answer is read up to
  `MAX_TOKEN_RESPONSE_BYTES` (1 MiB); a longer one fails the refresh as an
  unreadable one does, as a transient failure.

## Codex rate limits

Codex ends a stream with an `error` event of type `usage_limit_reached` when a
window is spent, and emits a `codex.rate_limits` event during healthy streams.
Every transport answers a spent window once, without retrying, with
`x-should-retry: false` plus `anthropic-ratelimit-unified-status: rejected`,
`-reset` and `-representative-claim`: the live WebSocket and live HTTP
streams, a rejected WebSocket upgrade whose body reports the limit
(`handshake_rejection_error` in `websocket.rs`, direct upgrade and CONNECT
tunnel alike), a non-2xx startup status whose body or `X-Codex-*` response headers
carry the limit, the buffered paths (`usage_limit_from_response` runs ahead of
`first_reportable_failure`, so a buffered WebSocket relay shares the branch),
and an opt-in server compaction request, which aborts instead of spending the
normal request as well. The live HTTP stream hands its response headers to
`usage_limit_from_event_with_headers`, so a limit event that carries only a
relative reset still gets its window from the `X-Codex-*` headers; a header
inside the event payload wins over the HTTP one; the committed branch after
output reads them too (`response_headers` threaded from
`stream_codex_http_events_for_owner`; a WebSocket stream has none). Only the
explicit `usage_limit_reached` type and the terminal snapshot below are
terminal; a transient 429 that happens to carry a reset clock stays retryable.
`classify_event_failure` does not classify the snapshot at all, so no retry
path sees it; the usage-limit readers answer it. A body holding both an
explicit `usage_limit_reached` and a terminal snapshot is answered from the
explicit one (`usage_limit_from_response` reads them in two passes), and the
buffered WebSocket collector stops on a terminal snapshot as it does on a
terminal event.
A `codex.rate_limits` event reporting `limit_reached` with no usable credits
(`is_terminal_rate_limit_event`: neither `credits.has_credits` nor
`credits.unlimited` true) is answered as a spent window too, on every one of
those paths. It carries no error object, so its reset and claim come from its
own windows (`rate_limits::limiting_window`): a window is named only when
exactly one window the event carries is at or past 100% and every window it
carries is readable (`window_state`); otherwise none is, with no reset, no claim and
nothing recorded. The snapshot itself still updates the held reading
(`rate_limits::observe_event`) on the live HTTP stream and the buffered paths
(`observe_rate_limits_in_body`), as on the live WebSocket start, before the
limit is answered, and so does every window of an error body:
`codex_status_error` (a failed live HTTP start, the search call, the buffered
fallthrough) and `handshake_rejection_error` (a rejected upgrade) observe the
whole body before reducing it to a spent window. Every spent-window answer
(`usage_limit_response`), when the limit names a window and a reset, records
that window as spent until its reset (`rate_limits::observe_usage_limit`,
through the pure `mark_spent`).
The reading is held for one ChatGPT account (`Held` in `rate_limits.rs`):
each reading and spent window carries the request's account id (the login's
`account_id`; on the live paths `CodexWebSocketEventStream::account_id`, on a
rejected upgrade the request's `ChatGPT-Account-Id` header). A live WebSocket
reading is recorded by `coordinate_live_websocket_events` under the account
of the attempt that read it before it is queued, and the reader skips it
(`rate_limits_observed`), because a 401 refresh can move the stream to
another login while the reading waits in the queue; the response headers
still follow the stream's final account. Since that task records them, the
bare receivers of `stream_codex_websocket_events` and
`stream_codex_auto_events` (the latter on its WebSocket branch) feed the
gauge too, which no reader did for them before. A reading or spent window
under another account replaces the held reading whole, and
`with_rate_limit_headers` publishes it only for a request under the same
account. A pooled WebSocket keeps the `ChatGPT-Account-Id` of its handshake
(`PoolEntry::account_id`), since the backend answers every request on it
under that account, and the handshake's access token
(`PoolEntry::access_token`, in memory only, in a type without `Debug`, never
logged). `take_pool_entry_for_request` hands it only to a request it serves
(`PoolEntry::serves_request`): both account ids are known and equal, or
neither is known and the request's access token is the very string of the
handshake. An account known on one side only is another login. Otherwise it
drops the socket as gone, so a
continuation that needed it takes the missing-socket path
(`continuation_socket_missing_error`) and is sent again with full context,
and any other request opens its own connection. A token of the same known
account renewed before it expires keeps the socket, while a renewed token
with no account id does not (a refresh forced by a 401 still
drops the conversation's socket, as before), whereas the Codex CLI resets its
connection on any auth change (`codex-rs/core/src/client.rs` at
rust-v0.166.0). The HTTP replay of a WebSocket size refusal likewise goes
out only under the account of the WebSocket attempt it replaces, and, when
an account id is unknown, only under that attempt's own access token
(`refuse_size_fallback_under_another_account`). When the request's account
id is unknown, the pool rule makes its token the one its socket was opened
with (a pooled socket serves it only under the same token, else it opens
its own), so the token that comparison reads is the socket's. A reading whose
`metered_limit_name`, else `limit_name`, names a limit other than `codex`
updates nothing, and its refusal marks no window (`reads_codex_limit`,
`CodexUsageLimit::feeds_gauge`); one naming neither reads `codex`, as the
Codex CLI files it (`codex-rs/codex-api/src/rate_limits.rs:124-167` at
rust-v0.166.0). `is_terminal_rate_limit_event` does not read the name, so
such a refusal is still answered as a spent window.
Under one account, every reading, healthy or refusing, and every spent window
are merged window by window (`merge_snapshot`, `merge_window`), never swapped
in whole: an
incoming window whose reset has passed is ignored, a held window whose reset
is more than `SAME_WINDOW_TOLERANCE_SECONDS` (60 s) after the incoming one is
kept, and within that tolerance the incoming one replaces it as the same
window, so a reset derived from a countdown a second or two off still marks
it spent. A window the incoming reading does not carry, or carries unusable,
keeps the held one, which `latest` drops once its reset passes.
Healthy responses can then carry `unified-status: allowed` beside a
utilization of 1.0 until the next reading of that window replaces it. A
spent window after output started, as an error event or as that snapshot,
ends the stream with an SSE `error` of type `rate_limit_error` and the same
message, and records the window the same way; the status line has already
left, so it carries no headers. Only live stream starts carry the gauge
headers; a buffered answer has none. Tests that touch the reading take
`rate_limits::lock_for_tests`, and a smoke test that leaves one behind clears
it with `clear_codex_rate_limits_for_tests`.
The representative claim is published only for a window duration that
`claimable_window` in `rate_limits.rs` recognises (around 300 minutes for the
five hour window, around 10080 for the weekly one; Codex has reported 299),
and an ambiguous reset clock yields neither a substituted header clock nor a
claim. `Retry-After` is deliberately not sent
because clients sleep for its full value, which here is hours. Healthy
readings become `-5h-utilization` / `-5h-reset` / `-7d-*`, and a window past
its threshold (`CCP_CODEX_QUOTA_WARN_AT`, defaults 0.9 session / 0.75 weekly)
adds `-surpassed-threshold`; readings whose reset time has passed are dropped.
A reading drops a window, not the whole reading, when its `window_minutes`,
`used_percent`, reset moment or countdown is not a finite, non-negative number
in range (numeric strings are still read), and a countdown is added to the
clock with saturating arithmetic (`window_state`). A reset moment or countdown
that is present but unusable drops the window even when the other is usable;
an absent or `null` one is not unusable.

Other failures are classified by their error code first (`error_class` in
`events.rs`, `CodexErrorClass`), in event payloads (`error`,
`/response/error`) and in error bodies of any status (`error_class_from_body`:
JSON, else the SSE events a buffered WebSocket relay builds under the status
its `error` event named), on the live WebSocket and HTTP starts, the WebSocket
handshake and the buffered paths. The buffered class lookup runs before the
status-only arms. A 401's body or `error` event is read before any token
refresh, on the live and buffered HTTP and WebSocket paths and `post_search`:
a spent window (`usage_limit_reached` or a terminal snapshot) is answered
first, then a known class, both with no refresh and one upstream attempt per
transport: under `auto` a refused upgrade still falls back to HTTP, which
reads its own answer. Only a 401 that names neither refreshes the token, at
most once per client request (`should_refresh_after_unauthorized`,
`unauthorized_needs_refresh`, and `unauthorized_event_needs_refresh` for a
live WebSocket event): the buffered loop keeps one flag across both
transports, and the live WebSocket task marks its refresh on the stream
(`used_auth_refresh`), which the live `auto` fallback hands to the HTTP
attempt (`auth_refresh_spent`). A terminal
snapshot also closes the live WebSocket retry window
(`event_closes_live_retry_window`), so no later 401 of that request refreshes.
The code decides over the status and the message heuristics:

- quota (`insufficient_quota`, `credit_balance_exhausted`,
  `organization_spend_limit_exceeded`, `project_spend_limit_exceeded`,
  `organization_usage_limit_exceeded`, as code or type): 429
  `rate_limit_error` with `x-should-retry: false`, no `Retry-After` and no
  unified headers (`with_rate_limit_headers` leaves alone any response that
  carries `x-should-retry`);
- `usage_not_included` (code or type): 403 `permission_error`;
- `invalid_prompt`, `cyber_policy`, `bio_policy`,
  `misalignment_policy_violation`: 400 `invalid_request_error`, with a short
  fallback text when the message is empty;
- `context_length_exceeded`: the 413 `request_too_large` answer of the
  context window message path (`context_overflow_response`), native text kept;
- `slow_down`, `rate_limit_exceeded`: 429 with `Retry-After` from the payload,
  else from the message's `try again in N s|ms|seconds`;
- `server_is_overloaded`, and `policy_capacity_unavailable` as code or type:
  529 `overloaded_error`, native text kept, with `Retry-After` only when the
  payload carries one;
- `model_not_found` (code or type) with no status or a 400, 403 or 404
  (`class_with_status`, after `codex-rs/codex-api/src/error.rs:83-94` at
  rust-v0.166.0): 404 `not_found_error` with the native text, else
  `model_not_found`, also on the committed stream branch and a rejected
  upgrade; any other status keeps its old handling;
- the terminal codes of `TERMINAL_CODES` in `events.rs` (after
  `parse_terminal_provider_error` in `codex-rs/codex-api/src/error.rs` at
  rust-v0.169.0-alpha.9), read by `code` only and only with no status or the
  one listed; under any other status the failure is read as if it carried no
  such code. `content_policy_violation`, `invalid_access_program` and
  `unsupported_access_program` (no status or 400) are 400
  `invalid_request_error`; `budget_reached` (no status or 400) is quota, 429
  with `x-should-retry: false`; `inference_hook_denied` and
  `access_program_not_enabled` (no status or 403) are the `PermissionDenied`
  class, 403 `permission_error` with no `Retry-After`. One of these codes
  whose message is missing or blank takes the CLI's text for the code,
  whether it came as a stream event (`classify_event_failure`), a JSON error
  body (`codex_status_error`) or a rejected upgrade
  (`handshake_rejection_error`), both of the latter through
  `terminal_code_text_from_body`; `budget_reached` has no text of its own and
  gets the quota class's short text. A message that is not blank is kept,
  subject to the same client text rule as every other class. The CLI itself
  reads the access program codes only with no status.

`usage_limit_reached` keeps precedence over every class.

A classified `Retry-After` is normalised (`normalize_retry_after`, on the
trimmed value): a finite number is rounded up to whole seconds, at least 1;
other text (an HTTP date) passes as it came when it is at most
`MAX_RETRY_AFTER_TEXT_BYTES` (64) of printable ASCII. Anything else, empty,
longer, non-ASCII, with a control character, or `NaN`/`inf`, is not usable
and counts as absent at that source, so the next source applies and the
value reaches no header and no log line. In a stream event (`classify_event_failure`, where the
error is the event's `error` or else `/response/error`) the first of these
gives the value: `error.retry_after`, `error.retry_after_seconds`, the event's
`retry_after_seconds`, the event's `headers` (`retry-after`, then
`Retry-After`), then the error's own `headers` the same way, which is where
`response.failed` carries them (`codex-rs/codex-api/src/sse/responses_error.rs`
at rust-v0.162.0-alpha.17). Any of them wins over the message's delay. A
Codex error body, classified or not, and a rejected upgrade's body are read
the same way (`retry_advice`, `retry_after_from_body`; `response_retry_after`
on the live and buffered HTTP paths), with the response's normalised
`Retry-After` header as the fallback. That
value, a rejected upgrade's `Retry-After` header and a refused CONNECT
tunnel's are normalised when they are read, so an unclassified 429 or 5xx
passes them on in that form too. `CodexError.class` carries the class to `map_codex_error_to_response`
(`classified_error_response`); a mid-stream error keeps its status and takes
the class's error type. An unknown code keeps the old handling, and
`usage_limit_reached` keeps precedence over all of these. A live failure
event before output, on the WebSocket or inside an HTTP stream, that is not
a spent window, is not retryable by `classify_event_failure`
(`retryable_live_start_payload`), has no class and names a 400-499 status of
its own (the event's `status` or `status_code`, else its error's `status`)
other than 408, 409 and 425, and is not a missing previous response
(`websocket::is_previous_response_missing`, by code or by message), goes
through `map_codex_error_to_response` as an HTTP error answer with that
status does (`live_stream_response_once`): a context-window message is 413
`request_too_large`, else 401 is `authentication_error`, 403
`permission_error` and any other status `api_error`, each with the
backend's message; only the live answer can also carry the gauge headers.
The error it builds has origin `Http`, so a backend message equal to the
proxy's own WebSocket size refusal text is not read as that refusal. Every
other failure that reaches that point (not a spent window, not retryable,
no class) gets `map_codex_failure_to_response`: 502 `api_error`, or 413 for
a context-window message; a retryable one goes through `provider_retry` and
keeps its own status. An opt-in server
compaction refused as quota, `usage_not_included` or `PermissionDenied`
answers the client with that class and does not send the normal request; any
other compaction failure, a rejected one included, falls back to it. The
WebSocket handshake reads the rejection body on both the direct upgrade and
the HTTP CONNECT tunnel (on the tunnel only the bytes that arrived with the
response head), never for a 407. It keeps up to
`MAX_ERROR_BODY_BYTES` (64 KiB, in `client.rs`) of the body to read the class
and a spent window from, and at most 1 KiB of its message as the detail. A
non-2xx HTTP answer (live and buffered Messages requests, the compaction
request, the search call) is read the same way, up to `MAX_ERROR_BODY_BYTES`
and within one deadline of `body_idle_timeout_ms` for the whole read
(`read_error_body`); the classification, a spent window and the traffic
capture see only those bytes, and a read past the deadline fails with
`detail: http_response_body` and is neither refreshed nor resent. A
successful buffered Messages answer is held up to
`MAX_BUFFERED_RESPONSE_BYTES` (64 MiB of SSE, in `client.rs`; no setting,
`with_buffered_response_limit` is for tests), over HTTP
(`collect_http_response`) and WebSocket (`collect_ws_events`) alike; one
byte more fails it with a 502 `api_error` whose detail is
`BUFFERED_RESPONSE_TOO_LARGE_DETAIL`, drops the socket, and is neither
refreshed, resent nor replayed over HTTP. The exception is a refusal the
proxy holds at that point (`capped_body_names_refusal`: a spent window from
`usage_limit_from_response`, else a classified failure from
`first_reportable_failure` for a 2xx or `error_class_from_body` otherwise):
the collector ends the body there and returns it, so the readers of
`post_codex_with_transport` answer it as without the limit, the spent window
recorded and the readings merged; the socket is still dropped and nothing
is resent. On HTTP the bytes examined are those held, the crossing chunk
cut at the limit, so a refusal that arrives wholly after the limit is not
seen and the answer is the 502, and a refusal held is answered from what is
held even when a more specific one follows past the limit. On WebSocket the
event that crosses the
limit is examined when its `type` is `error`, `response.failed` or
`codex.rate_limits` and the collector would stop on it as a failure
(`failure_past_limit`: a terminal event or a terminal snapshot, not a
missing previous response); it joins the body as one compact `data:` line.
A live stream is relayed as it
arrives, and the copies of its events kept beside the relay stop at the
client's same limit (`CodexHttpClient::buffered_response_limit`). The copy
its continuation state is read from (`LiveUpstreamCopy` in `mod.rs`) is
released for good by the event that would take it past the limit, with one
`codex_live_stream_copy_dropped` warning (`reqId`, `limitBytes`); at the
terminal event such a stream records no continuation state
(`abort_request_state`), so the conversation's next request sends its full
context without `previous_response_id`. The WebSocket reader's copy for the
response-body capture (`LiveCaptureBody` in `websocket.rs`) exists only
while a traffic capture is on and stops before the event that would pass
the limit, so `032-upstream-response-body.sse` then holds the events up to
that point. The client receives every event either way. No copy is built
before it is known to fit: `encoded_sse_len` gives `encode_sse`'s framed
length without allocating, and `append_upstream_sse_payload` serialises into
a writer that stops at the room left. The live HTTP
reader keeps no copy of the stream; it captures event by event. What a live
stream holds is bounded by the same limit too. The translator
(`LiveStreamTranslator`, built `with_retained_limit` from it in
`live_stream_response_once`) keeps a running count: the bytes of every
string it appends or stores (text, tool call arguments and the sanitized
replacement of a finished call's arguments when it differs from what was
held, reasoning ids and `encrypted_content`, message and call ids, tool
names, web search ids, queries and result titles and URLs; a replacement
adds its length and subtracts nothing, a move into `deferred_text` adds
nothing), plus a fixed `RETAINED_ENTRY_BYTES` for every entry it inserts
into a collection it keeps (a block, a message id lookup, a pending
reasoning item, the `deferred_text` slot of a deferred text block, a web
search, a web search result), charged before the entry goes in, so entries
that hold no string still move the count. `emit_web_searches` emits every
held result under every search, so each pair of a search and a result is
charged once, when the later of the two arrives: a new search adds the
results already held, a new result its own bytes once for itself and once
more for every search already held. It is a count, not a measure of process
memory. The translator refuses the string or entry that would take the
count past the limit before keeping it (`reserve_retained`), with
`RETAINED_OUTPUT_TOO_LARGE`.
After output that refusal ends the stream with an SSE `error` of type
`api_error`, before output it answers a plain 502 `api_error`. Either way
`abort_refused_live_stream` then takes three steps in order. It drops the
upstream stream, which closes the channel the WebSocket task sends on, so
the task can no longer hand its answer on. It removes the socket the turn
has pooled (`invalidate_live_continuation_pool`): the task pools it on a
`response.completed` that arrived before the refusal, while the turn is
still current (`pool_insert_for_turn`). It aborts the request state, which
drops the turn's continuation, so the turn is no longer current and the
task can pool nothing more for it. A socket the task pools between the
removal and the abort belongs to a turn the backend completed and carries
no continuation; the task removes it again when its final send finds the
channel closed, and one whose final send was still accepted stays pooled.
Before
output, the translated framing held in `pending_chunk` (`message_start` and
pings) answers `buffered_response_too_large_error` the same way once the
next chunk would pass the limit, and `spawn_http_event_stream` counts the
SSE data length (`DecodedHttpSseEvent::data_bytes`) of the events it holds
in `pending_events` and, when the next one would pass the limit, drops them
and fails the stream with that error, which is neither resent, refreshed
nor replayed. The live WebSocket coordinator holds nothing before output: it
forwards each event as it arrives over a bounded channel.
Text built from a `reqwest::Error` in these paths goes through
`upstream_http::error_text`, which leaves out the request URL. Under `auto` transport
(the default) a handshake failure, classified or not, falls back to HTTP unless the
WebSocket proxy refused it (`should_fallback_to_http` in `client.rs`), and
that HTTP answer is classified on its own. The codes and their shapes follow
the Codex CLI's own error handling; no test here runs them against the
backend.

Field-name trap: the wire format says `reset_at` / `reset_after_seconds`, while
Codex CLI session logs reserialise the same data as `resets_at` /
`resets_in_seconds`. Both spellings are read. If quota headers ever stop
appearing, compare against a fresh traffic capture before anything else.

The proxy does not retry a failed Codex request: one attempt, then the error
goes to the client, which owns the retry policy. The only resends left repair
the proxy's own state (a forgotten `previous_response_id`, a 401 token refresh).
An `auto` fallback to HTTP after a failed handshake, before the request went
out, or for a frame that was never written because it is over the limit the
handshake advertised, sends nothing twice. Under `auto` a request already
written to the socket goes out over HTTP in two cases only: a WebSocket seen
closing with code 1009 before any text frame of the response, as the Codex CLI
treats that close, and a resend the proxy makes anyway, such as the
full-context one that repairs a continuation, when its frame is over the limit
of the socket it would go to. After a connect that got no answer (nothing
answered in full before the connect timeout, or the connection failed), a
request whose HTTP fallback is on and that would open a new connection to
that target goes to HTTP at once for five minutes, while a pooled socket
still serves its conversation. A live `auto` request that already sent the
client an event, `websocket` transport and a buffered `auto` request whose
fallback is off still connect every time they need a new connection (the
connect pause in the transport paragraph names the paths). With
`websocket` transport either refusal is one 413 answer, and a 429 on the
WebSocket handshake reaches the client as a plain 429 unless its body names
one of the codes above; under `auto` it falls back to HTTP.

## Naming and distribution

The crate, the installed command, the library target (`claude_code_mux`), and
the GitHub repository are all `claude-code-mux`. Releases are `v*` tags with
prebuilt binaries; nothing is published to crates.io.

Some strings deliberately keep the old `claude-code-proxy` name because they are
compatibility contracts, not the user-facing name. Do not rename them:

- The on-disk config and data directory and the macOS Keychain service, in
  `paths.rs`, `providers/kimi/auth`, and `providers/cursor/auth.rs`. Renaming
  these orphans any saved kimi, grok, or cursor login. `paths.rs` already has
  `legacy_config_dir` as the migration hook if this is ever changed on purpose.
- The Codex `ORIGINATOR` and `User-Agent` in `providers/codex`. These go to the
  ChatGPT backend, so keep them stable to avoid changing what the server sees.

## Tests

- Integration tests in `tests/` drive `server::app*` with tower `oneshot` and
  in-process mock upstreams. `smoke_cutover.rs` starts mock Codex HTTP and
  WebSocket servers and a mock Kimi server; `codex_auth.rs` and `cli.rs` run
  the binary through `assert_cmd` with `CCP_CONFIG_DIR` and
  `CCP_CODEX_AUTH_FILE` pointing at temp dirs. Tests use toy credentials only.
- The static registries expose reset helpers: `clear_all_continuations_for_tests`,
  `clear_all_compactions_for_tests`, `clear_codex_websocket_pool_for_tests`,
  `retry::set_zero_retry_delay_for_tests`. Call them at the start of a test
  that depends on clean state.
- Unit tests sit next to the code under `#[cfg(test)]` in most modules.
- Fixtures: `tests/fixtures/anthropic-message.json`, `tests/fixtures/sse-basic.txt`.
- A unit test that mirrors a wire format can pass while being wrong about the
  format. Verify stream-shape changes end to end against a real capture.
- The suite must never touch a real credential, a real backend or the
  developer's own state. Run it from a cleared environment (`env -i` with a
  minimal `PATH`), with `HOME`, `CCP_CONFIG_DIR` and the XDG config, data and
  state dirs pointed at temp directories, `CCP_CODEX_AUTH_FILE` pointed at a
  file that does not exist, every provider base URL pointed at a loopback mock
  server, client versions pinned to fixed strings, and `--test-threads=1`. A
  test that reaches outside that perimeter is a bug in the test.

## Codex model inventory

`/v1/models` and the `models` command do not read a compiled-in Codex list.
`providers/codex/models.rs` calls `GET {api root}/models?client_version=X` on
the Codex CLI's bearer (the completions URL with `/responses` replaced by
`/models`), turns each entry into an Anthropic `ModelInfo` row
(`UpstreamModel::to_listing_row`: `display_name` from the entry, else the
slug; `max_input_tokens` from `max_context_window`, else `context_window`,
else null; `max_tokens` null, as the translator sends no output cap;
`capabilities` from `UpstreamModel::capabilities`, where a leaf is true only
when the Codex translator carries that feature), keeps hidden entries, and
remembers the slugs so
routing and `assert_allowed_model` accept them. At most
`MAX_MODELS_BODY_BYTES` (4 MiB) of the answer is read; a longer one fails the
listing as an unreadable one does (`ModelsError::Unreachable`), and no
listing error text names the request URL. The other catalog fields stay
in `UpstreamModel` for the proxy's own use and are not in the rows. Under the
`inventory` lane policy `use_responses_lite` from the listing overrides the compiled-in
lane table, while under the default `full` policy the proxy forces the full
lane for every model and ignores that flag entirely
(`uses_responses_lite_with_full_lane` in
`providers/codex/translate/model_allowlist.rs`). Nothing of the Codex
listing is cached across calls or restarts, deliberately: a consumer that
reads the listing as the truth about available models must see changes, not
a stale fallback.

The remembered listing also keeps, per slug, the default for the request's
`service_tier` body field (`UpstreamModel::catalog_default_service_tier`):
the entry's `default_service_tier` when its `service_tiers` lists that id,
never the `default` sentinel, which is how the Codex CLI fills that field.
`resolve_service_tier` in `providers/codex/translate/request.rs` decides a
request's tier on the model that goes on the wire, after the lane upgrade:
`CCP_CODEX_SERVICE_TIER`, then that model's entry in
`codex.serviceTierByModel` (exact id, no `-fast` or `[1m]`, no globs; source
`config_model`), then `codex.serviceTier` (`default` in any of the three
means no tier at all), then a `-fast` suffix (priority), then that catalog
default, then none. `config::codex_service_tier_for_model` applies the first
three. The map is a raw `Value`: one that is not an object is ignored and
named in `config_override_summary_lines` without its contents (a `config
summary` warning at startup), a `null` map or `null` entry counts as absent,
and any other entry that is not a valid tier, a non-string one passed on as
its JSON text, gives a 400 that names it
(`Invalid service tier in codex.serviceTierByModel["<model>"]: ...`, from
`normalize_service_tier` when the source is `ConfigModel`); the env variable
and `serviceTier` keep `Invalid service tier override: ...`. The map and
`serviceTier` apply only where `resolve_service_tier` runs, the Codex
provider's Anthropic Messages path: `/v1/messages`, `count_tokens` and the
auto-review classifier, whose entry is keyed by its effective model
(`gpt-6-luna` by default). The OpenAI-compatible `/v1/responses` (a native
relay) and `/v1/chat/completions` take neither. Only `priority` and `flex` from the catalog go out; any other id,
`fast` and `ultrafast` included, sends nothing and is never mapped to a
tier. Claude Code's auto-review classifier sent to Codex skips the catalog
step (`MessagesRequest::bypass_catalog_service_tier`, set in
`dispatch_request`), as the CLI's own review requests carry no
`service_tier`; a configured tier or `-fast` still applies to it. The CLI
drops an explicit tier the model does not list; the proxy sends a configured
or suffix tier as is. Neither applies a tier of its own when the listing
names none. The match covers the `service_tier` body field only: the proxy
sends no `x-codex-routing-hint` header. The OpenAI-compatible surfaces do not
apply the catalog default. `codex_upstream_request_started` logs
`serviceTier` and `serviceTierSource`.

Routing reads a process-wide catalog in `src/registry.rs` (`LISTED_MODELS`):
per provider, the ids of its last successful listing from a backend
(`source: upstream`), which replaces that provider's compiled-in list; a
provider that has not listed yet routes on its compiled-in list, and bundled
listings (kimi, grok, cursor today) are not recorded, nor are anthropic's
(`source: relayed` or `catalog`): `record_listing` records only `source:
upstream`. Being listed by `/v1/models` and automatically, at start and on a
miss, both need `has_credentials`, which defaults to false, so a provider
gains them by implementing both (`/v1/models` also asks a provider whose
`listing_required` holds). Ownership never depends on
name order: an id a compiled-in list names stays with that provider, and its
own listing only says whether it still serves it; an id only listings name
goes to the provider whose current listing has named it the longest (the
catalog numbers each recorded listing). The catalog is filled at three
points, all through `record_listing`:

- every `/v1/models` and `/v1/models/{model_id}` call, for the providers it
  asks (`collect_model_rows`);
- `serve` start: `main.rs` spawns `Registry::refresh_listings` beside the
  server, so binding never waits and a failed listing is only logged;
- a routing miss: a `/v1/messages` or `count_tokens` request for an id that is
  not a Claude id, an alias or a `cursor:` id and that nothing routes runs
  `refresh_listings` once, routes again, and otherwise answers the same 400 as
  before (`provider_for_model_or_refresh`). The refresh is single-flight (a
  per-registry mutex held across the listing; requests that waited route
  again first) and at most one starts per `MISS_LISTING_INTERVAL` (30 s),
  whatever its outcome, so a typo does not reach the backends on every
  request.

`refresh_listings` asks only providers whose `has_credentials()` holds, a
local check of the saved login with no network call (the Codex CLI's
`auth.json`, the kimi and grok token files, the cursor store, which on macOS
with `CCP_CONFIG_DIR` unset can mean a Keychain read that blocks for up to
10 s, so every check runs on tokio's blocking pool); anthropic has none and
is never asked. A restart forgets the catalog.

A successful listing that names no model, such as a 2xx body without
`models`, counts as no listing: it is not recorded, the Codex provider's own
remembered set (`remember_discovered`, which `assert_allowed_model` and the
lane table read) keeps its previous slugs, and the previous listing or the
compiled-in list keeps routing. It is logged as a warning, `model listing
named no models; routing keeps the previous one`. Every recorded listing is
logged at info as `model listing recorded` with `provider`, `models`,
`previous_models` (null for the first), `added` and `removed`, and no model
bodies. `/v1/models` still takes such an answer as a successful listing: that
provider adds no rows and no error.

Hosted web search sits outside the policy: a request carrying the hosted
`web_search_20250305` tool is forced onto the full lane, and there a non-forced
`gpt-5.6-luna` is rewritten to `gpt-5.6-sol` and `gpt-6-luna` to `gpt-6-sol`
(`apply_model_lane_for_request`, `full_lane_web_search_model`). The compiled-in
lane table lists `gpt-6-luna`, `gpt-6-sol`, `gpt-6.1-sol` and `gpt-6.1-terra`
beside the gpt-5.6 family and `gpt-6-astra`. The forced standalone `/alpha/search` path
(`is_standalone_search_request`) builds a request of its own and keeps the
model that was asked for. Its answer is read up to `MAX_SEARCH_BODY_BYTES`
(8 MiB); a longer one fails as an unreadable body does (`detail:
http_response_body`) and is not sent again. The rewrite rests on no reproducible capture of a
Luna refusal on the full lane, and no live capability check has been run
either way.

Verified live 2026-09-11: the call answers 200 with the proxy's own
`originator`, with or without `ChatGPT-Account-Id`, and while the weekly
window is at 100% (`/backend-api/wham/usage` reported `limit_reached`); it
answers 400 without `client_version`. The version comes from
`CCP_CODEX_CLIENT_VERSION`, else `codex.clientVersion` in `config.json`, else
the Codex CLI's `models_cache.json` next to `auth.json`, else
`CODEX_CLIENT_VERSION` in `auth/constants.rs`.

The compiled-in `CODEX_MODELS` / `ALLOWED_MODELS` lists still exist for
routing until the first successful listing and for the OpenAI-compatible
surfaces' error text. They are no longer what `/v1/models` advertises. Adding
a model there is optional; when done, keep `src/registry.rs` and
`src/providers/codex/translate/model_allowlist.rs` in sync and update the
`assert_allowed_model` test.

The Claude rows of `/v1/models` come from `providers/anthropic/models.rs`
on every call (`listing_required`), though the proxy holds no Anthropic
credential; anthropic's `has_credentials()` stays false, so `serve` start and
a routing miss never fetch anything for it. A client request with a
non-empty `x-api-key` or `authorization` header is relayed to
`{CCP_ANTHROPIC_BASE_URL}/v1/models?limit=1000` with the client's headers,
filtered by `is_stripped_request_header` like the messages relay and without
`content-type`, plus `anthropic-version: 2023-06-01` only when absent, with
the client's `lifecycle` pairs appended as spelled and `after_id=<last_id>`
followed while `has_more` holds, at most `RELAY_MAX_PAGES` (10) pages. When
the tenth page still says `has_more`, or a page says it with no cursor that
differs from the one just sent (`last_id`, else the last row's id), the relay
keeps the rows so far and logs the warn `anthropic model listing truncated`
with `pages`. Because the `lifecycle` pairs go to Anthropic as spelled,
cursors on the relayed path resolve over the rows Anthropic returned for that
filter, and a credentialed `/v1/models/{model_id}` (no pairs) sees the rows
Anthropic lists by default. Every page must be a 2xx with a `data` array; the
rows then come untouched (those without a string `id` skipped), `source:
relayed`. Anything else on any page (a non-2xx, a 3xx included since the
relay uses the passthrough client with `redirect::Policy::none()`, a network
error, a timeout, a body without `data`, a body over `MAX_BODY_BYTES`) drops
the rows of earlier pages, logs the warn `anthropic model listing relay
failed; using the public catalog` and falls back to the catalog. Without a
credential, the rows come from `config::anthropic_catalog_url()`:
`CCP_ANTHROPIC_CATALOG_URL` (env only, blank means unset), else
`DEFAULT_CATALOG_URL`
(`https://downloads.claude.ai/model-catalog/v1/catalog.json`), fetched with a
plain GET and no client header on `AnthropicProvider::catalog_client`, a
client of its own that never carries client headers and follows redirects
with reqwest's default bounded policy. Relay pages and the catalog body are
read in chunks up to `MAX_BODY_BYTES` (4 MiB); more is a failure of class
`body` (`read_capped`). `catalog_rows` reads
`surfaces.cc.model_selector_config[*].models[*]`, keeps a model whose
`offered_on` is absent or an array naming `first_party` (a `null`, an empty
array or any other value drops it) and the first occurrence of an id, and
maps `name` to `display_name` (else the id),
`runtime.max_input_tokens` and `max_output_tokens` to `max_input_tokens` and
`max_tokens`, and `runtime.family` to `line` when it is one of `MODEL_LINES`,
with `capabilities: null` and `source: catalog`; a catalog that yields no row
is a failure. `CATALOG_CACHE` keeps the parsed rows per URL for
`CATALOG_TTL` (10 min) from a successful fetch. `CATALOG_FLIGHTS` makes the
fetch single-flight per URL: the first caller spawns the fetch as a task and
registers its shared result; every caller that arrives while it is in flight
awaits that same result, a failure included. The entry leaves the map when
the task ends (`LeaveFlight`, matched by flight id), so a failure is never
cached: a request after the flight ended starts a new fetch, and there is no
stale fallback. A task dropped before its first poll (a runtime shutting
down) never creates `LeaveFlight`, so two more rules keep such an entry from
staying: a caller never joins a flight whose shared result is already in
(`Shared::peek`) and starts a new one in its place, and a `JoinError` from
the task removes the entry of that flight id and logs the failure warn with
class `task`. With no successful relay the listing is
`unavailable("anthropic", "unreachable", ...)`, and `/v1/models` and every
`/v1/models/{model_id}` answer 502. `LISTING_TIMEOUT` (4 s) bounds the whole
relay, each caller's wait for the catalog, whether it started the fetch
or joined one, and, separately, the fetch itself, which therefore runs on
and stores its result when every caller stopped waiting.
`set_anthropic_listing_timeout_for_tests` shortens the relay and the waits,
not the fetch's own budget. The info `anthropic model catalog fetched` carries the
catalog's `version` and the `models` count; the two failure warns carry
`status` (0 when none) and `class` (`status`, `timeout`, `network`, `body`,
`empty`, and `task` on the catalog warn), the truncation warn carries
`pages`. None logs a header, a body or
the query. The catalog's signature is not
verified; routing never reads these rows. Tests point
`CCP_ANTHROPIC_CATALOG_URL` at a loopback mock and use
`clear_anthropic_catalog_cache_for_tests`,
`expire_anthropic_catalog_cache_for_tests`,
`set_anthropic_listing_timeout_for_tests` and the flight-map hooks
`insert_ended_anthropic_catalog_flight_for_tests`,
`insert_unstarted_anthropic_catalog_flight_for_tests` and
`anthropic_catalog_flight_pending_for_tests`. The `models` command calls
`list_models(None)` on every provider without the login gate, so it prints
the catalog ids as `[public catalog, not verified]`, or `anthropic:
unavailable (...)`.

## How Codex models get into Claude Code's picker

Applies to Claude Code 2.1.266; the FABLE variable below to 2.1.269:

- `modelPicker` in user settings (`options[]` of `model`, `label`,
  `description`, `behavesAs`) is the supported way. Rows use bare ids, need no
  credential, and keep the subscription path. Present since 2.1.261.
- Gateway discovery (`CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1` fetching
  `/v1/models`) runs only with `ANTHROPIC_AUTH_TOKEN`, `apiKeyHelper`, or an
  API key, keeps only ids matching `/(claude|anthropic)/i`, and any of those
  credentials makes Claude Code drop the `RateLimitEvent` for every route and
  disable claude.ai connectors. Do not build on it. `/v1/models` lists Claude
  rows, so a client that turns it on gets those as picker rows and none of
  the Codex rows; the discovery request carries the configured credential,
  so the rows are Anthropic's relayed listing, or the catalog's when the
  relay fails.
- The bootstrap call that carries Anthropic's own extra picker rows goes
  straight to `api.anthropic.com`, never through `ANTHROPIC_BASE_URL`.
- Env-only alternatives exist but are narrower: `ANTHROPIC_DEFAULT_*_MODEL`
  remaps a built-in row (with `..._MODEL_DESCRIPTION` for its subtitle), and
  `ANTHROPIC_CUSTOM_MODEL_OPTION` (+ `_NAME`, `_DESCRIPTION`) adds one row.
  The slot variables are `ANTHROPIC_DEFAULT_OPUS_MODEL`, `..._SONNET_MODEL`,
  `..._HAIKU_MODEL` and `..._FABLE_MODEL`; the fable one is known from
  2.1.269, and the CLI ships new builds quickly, so check the version in use
  before relying on it.

Those four variables give a whole-picker recipe — haiku to `gpt-6-luna`,
sonnet to `gpt-5.6-terra`, opus to `gpt-6-sol`, fable to `gpt-6-astra`. It is
client configuration, not a proxy feature: the proxy has no subscription
detection, no provider selector, no launcher and no failover when a backend's
auth fails. Starting Codex-only, with no Claude credentials present, is not
covered by a live client test.

`~/.codex/models_cache.json` is the authoritative inventory of what Codex
currently serves, including `visibility` and `use_responses_lite` per model.

## Gotchas

- In the dual setup, `ANTHROPIC_AUTH_TOKEN` and `ANTHROPIC_API_KEY` must be
  unset. Claude Code forwards its subscription login for `claude-*` only when no
  explicit token is present. Setting either one sends that value instead and the
  Anthropic route returns 401. A non-empty `ANTHROPIC_API_KEY` also puts Claude
  Code on the API-billing path, where it ignores the structured rate-limit
  headers entirely.
- Claude Code disables lazy tool loading when `ANTHROPIC_BASE_URL` is not a
  first-party host. `ENABLE_TOOL_SEARCH=true` restores it, and the flag is safe
  to set on both routes. The Anthropic passthrough forwards the `tool_reference`
  blocks untouched. The Codex route maps deferred loading onto the backend's
  native tool search (`providers/codex/translate/tool_search.rs`): Claude Code's
  `ToolSearch` becomes a client-executed `tool_search` tool, its call a
  `tool_search_call`, its result a `tool_search_output` carrying the loaded
  tools' specs, and deferred tools a search loaded stay out of the tools head.
  Putting a loaded tool into the head instead changes the first bytes of the
  prompt and costs a full prompt-cache miss on every load (measured: 0 cached
  tokens on the request after a load, versus the whole prefix with the native
  mapping). The mapping is derived from the request alone, so it is
  byte-stable turn to turn. A deferred tool no search in the history names
  (the placeholder, or a tool whose search a compaction or a context manager
  removed) stays out of the head too, as Anthropic leaves it out of the
  prompt; only a deferred tool a directed `tool_choice` names goes in
  (`ToolSearchPlan::keeps_in_head`). Measured live on gpt-5.6-sol: the backend
  accepts a `function_call` in the history for a tool absent from `tools`, and
  a request whose search pair was replaced by a summary kept 9216 of 12834
  prompt tokens cached this way, against 0 when the orphan went into the head.
- Claude Code's web search is a client-side `WebSearch` function tool. The
  hosted `web_search_20250305` tool only appears inside an isolated, history-free
  inner call, so its `server_tool_use` and `web_search_tool_result` blocks never
  enter the outer transcript. The passthrough logs `hosted_web_search_in_history`
  if that ever changes, which would mean this assumption needs rechecking.
- `MODEL_ALIASES` in the codex allowlist still maps Claude alias names to codex
  models. That only matters when `CCP_ALIAS_PROVIDER=codex`; the default keeps
  Claude names on Anthropic.
- Tool schemas sent to Codex lose every JSON Schema `pattern` keyword
  (`strip_tool_schema_patterns` in `providers/codex/translate/request.rs`),
  because OpenAI rejects some patterns Claude Code sends, such as Unicode
  property escapes. Only schema positions are touched: `default`, `enum`,
  `const`, examples, extensions and property names stay. It runs in
  `codex_tool_parameters` and `tool_search_spec`, so head tools, tools loaded
  by a search and the ToolSearch tool are all covered. The OpenAI-compatible
  routes are not: Codex `/v1/responses` is relayed natively and Codex chat
  completions refuse `tools`.
- The compaction cap (`CCP_COMPACT_EFFORT`, default `low`) also applies when a
  compaction request names no effort (`apply_compact_effort_cap`); it still
  never raises an effort the request named. An effort of `none`, from that cap
  or from `CCP_CODEX_EFFORT=none`, goes on the wire as `effort: none` but
  requests no reasoning summary and no `reasoning.encrypted_content`
  (`reasoning_requested`). That last rule applies to every request, not only
  compaction.
- A request is Claude Code's compaction summary when its system prompt holds
  the summarizer marker, or when a user message among the newest eight holds
  both prompt markers and every message after it is injected context
  (`is_injected_context_message`): a user message made only of text that
  starts with `<system-reminder>` once trimmed, or a `role: "system"` message
  made only of text, which Claude Code sends mid-conversation to carry
  reminders (`is_agent_summary_request` skips those too)
  (`is_compact_messages_request`, `compact_prompt_message_index`,
  `COMPACT_DETECTION_TAIL_MESSAGES` in `translate/request.rs`). The prompt as
  the last message is the plain case. A prompt followed by anything else (an
  assistant turn, a tool result, other text) was quoted or answered, and a
  prompt further back is history; neither counts. The effort cap, the opt-in
  server compaction and the HTTP routing all read this one detector. Server
  compaction strips the prompt from the prompt's own message
  (`without_compaction_instruction`, given the number of injected messages
  after it; each becomes exactly one input item, a system one a `developer`
  message), so the instruction never enters the stored native history. The
  HTTP summary request sends the full context and leaves no WebSocket
  continuation state, so the next request also sends its full context without
  `previous_response_id`.

## Known limitation

Switching backends in the middle of an active tool call (for example pressing
Esc during a codex tool use, then switching to a Claude model and continuing)
can fail. Anthropic requires a leading signed `thinking` block in that position
and no valid signature can be produced for reasoning that came from another
backend. This is rare and not worked around.

## Sensitive data

Traffic captures and the `errors/` directory under the state dir contain
prompts, tool input, tool output and file contents in the clear. Keep them
local, never paste them into an issue, and delete them after a debugging
session. Never print or commit `auth.json` contents, tokens or account ids.

JSON captures (`write_json`, `write_json_event`) pass through
`redact_traffic_with_depth` in `traffic.rs`, which replaces `encrypted_content`
and proxy-owned `ccp:codex:v1:` reasoning signatures with `[redacted len=N]`
and keeps any other signature. The raw SSE and raw byte captures
(`write_text`, `write_bytes`) are not redacted at all.

With the monitor on, the proxy reads Claude Code's local session transcripts
under `$CLAUDE_CONFIG_DIR/projects` every 5 s, incrementally, to name
sessions. It keeps only titles and read offsets, never modifies the files,
sends nothing from them upstream and writes no name to `proxy.log`. A traffic
capture may still contain a title as part of a recorded title reply.

## Style

Match the surrounding code. `reqwest` is built without gzip or brotli so bodies
are never auto-decompressed, and with rustls so no OS keychain is touched. Keep
new code in the same shape as the module it lives in. Stay close to upstream
and avoid style-only divergence so ports in either direction stay cheap.

## Where the rest is

- `.claude/precompact/` holds session handoffs: what happened, what is open,
  and what misled. Read the newest one at the start of a session.
- `.claude/probes/sdk_limit_probe.py` is an SDK-level rate-limit probe:
  `uv run --with claude-agent-sdk --no-project python .claude/probes/sdk_limit_probe.py`
  with `PROBE_BASE_URL` and `PROBE_MODEL` set.
- Upstream documentation is published at https://claude-code-proxy.raine.dev/
  (with `/llms.txt`); it describes configuration, the HTTP API, and file
  locations that this fork shares.
