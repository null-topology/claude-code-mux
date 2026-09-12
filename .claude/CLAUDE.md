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

Only this fork has `src/providers/anthropic/` and `AliasProvider::Anthropic`.
Upstream has no passthrough at all: its registry routes `claude-*` to another
backend (codex by default). A "Claude through the proxy" test against an
upstream build silently goes to Codex. If a Claude-route test ever returns a
Codex-looking answer or a Codex rate limit, check which build is running first.

Other differences when moving code between the two:

- Upstream additionally has `src/providers/opencode/`, its own Codex
  browser/device/PKCE login under `codex/auth/`, and an Astro docs site. In
  this fork Codex sign-in is `codex login` from the Codex CLI.
- Crate names differ in imports and test helpers: `claude_codex::` vs
  `claude_code_proxy::`, `Command::cargo_bin("claude-code-mux")` vs
  `Command::cargo_bin("claude-code-proxy")`. Ported test files need that edit.
- Never copy one `Cargo.lock` over the other.

The fork's `CHANGELOG.md` top entry records the last upstream release that was
integrated. `AGENTS.md` at the repo root lists the fork practices (stay close
to upstream, keep Claude aliases on Anthropic, Codex CLI auth, toy credentials
in tests).

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

Release: `just release` bumps a patch version with cargo-release and skips
publish; pushing a `vX.Y.Z` tag runs `.github/workflows/release.yml`, which
tests with `--test-threads=1`, builds prebuilt binaries for six targets, and
verifies `--version` matches the tag. It uses the default `GITHUB_TOKEN` and
needs no secrets. crates.io publishing is separate.

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
   `/v1/messages/count_tokens`, `/v1/models`. Optional OpenAI-compatible
   surfaces gated by `AppFeatures`: `/v1/responses` and `/v1/chat/completions`
   (`CCP_CODEX_RESPONSES_API`), `/v1/images/*` (`CCP_CODEX_IMAGES_API`),
   `/v1/audio/transcriptions` (`CCP_CODEX_TRANSCRIPTIONS_API`).
   `dispatch_request` handles the Anthropic routes: read the body, normalize
   the model (strip `[1m]`), look up the session by `x-claude-code-session-id`,
   pick a provider via the registry, apply the auto-review override (Claude
   Code's non-streaming, tool-free security classifier is rerouted to
   `CCP_AUTO_REVIEW_MODEL`, default `gpt-5.6-luna` on codex), then call the
   provider and record log, monitor and traffic capture.
   `handler_models` (`/v1/models`) calls `Provider::list_models` on every
   registered provider (or the one named by `?provider=`), tags each row with
   `provider`, and adds a top-level `providers[]` block with `auth`, `source`,
   `status`, `detail`, `fetched_at` per provider. The filtered form fails with
   502 when that provider cannot list; the aggregate form is always 200.
3. `src/registry.rs`: model to provider. `ANTHROPIC_STYLE_ALIASES` and any
   `claude-*` id go to the alias provider (`CCP_ALIAS_PROVIDER`, default
   anthropic); `cursor:` prefixes go to cursor; anything else must match a
   provider's model list exactly, or be a slug the Codex backend named in its
   last successful listing (`providers::codex::models::is_discovered_model`,
   `-fast` included); unknown ids return 400 listing the catalog.
   The `CODEX_MODELS`, `KIMI_MODELS`, `GROK_MODELS` lists here are duplicated
   in each provider's `translate/model_allowlist.rs`. Keep them in sync.
4. `src/provider.rs`: the `Provider` trait, `RequestContext` (request id,
   session, traffic, monitor, and `Passthrough` with the raw bytes and headers
   for byte-exact relays), `ProviderError`, `CliHandlers`, and `ModelListing`
   (what `list_models` returns; the default impl advertises `supported_models()`
   as `source: bundled`, anthropic overrides to `auth: client` with no rows,
   codex overrides to ask its backend).
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
   - `codex/`: by far the largest. Maps Anthropic Messages onto the OpenAI
     Responses API. Transport defaults to WebSocket (`CCP_CODEX_TRANSPORT` =
     `http|websocket|auto`) with a per-conversation pool in `websocket.rs`.
     `continuation.rs` keeps `previous_response_id` state keyed by
     `ConversationIdentity` (session plus agent headers) so subagents do not
     clobber each other; `compaction.rs` does server-side compaction;
     `events.rs` classifies stream failures and recognises the
     `usage_limit_reached` error; `rate_limits.rs` keeps the newest
     `codex.rate_limits` reading and publishes it as
     `anthropic-ratelimit-unified-*` headers (see below). Auth reads the Codex
     CLI's `~/.codex/auth.json` (`CCP_CODEX_AUTH_FILE` overrides the path).
   - `kimi/`, `grok/`: Chat Completions / Responses translators with their own
     OAuth. `cursor/`: Connect protocol over protobuf.
6. Cross-cutting: `session.rs` (in-memory sessions, 30 min idle TTL, provider
   affinity), `request_identity.rs` (Claude Code session and agent headers),
   `retry.rs` (backoff on 429 and 5xx), `monitor.rs` + `tui.rs`, `traffic.rs`
   (`CCP_TRAFFIC_LOG=1` writes full captures under the state dir),
   `logging.rs` (JSONL `proxy.log`, redacts known keys), `paths.rs` (config
   dir `~/.config/claude-code-proxy`, state dir
   `${XDG_STATE_HOME:-~/.local/state}/claude-code-proxy`, `CCP_CONFIG_DIR`
   overrides only the config dir), `config.rs` (precedence: `CCP_*` env, then
   `config.json` in the config dir, then default).

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

- side lanes: `server::monitor_conversation_label` appends `/side` when a
  request has no tool with `input_schema` (titles, the auto-mode classifier,
  the isolated web search call carrying only the hosted `web_search_*` tool).
  Without this, web search calls on the main model reset the main lane's
  baseline and set the context size to a few thousand tokens;
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

What the Codex backend keys its prompt cache on is the request's
`prompt_cache_key` plus the `session_id` header, and the proxy sends
`ConversationIdentity::cache_scope()` in both: the session id for a main
thread, a uuid v5 of session plus agent id for a subagent. Claude Code gives a
subagent its parent's session id, so without this every subagent shared the
main thread's key and its routing bucket, and OpenAI documents overflow routing
above about 15 requests per minute on one key. `build_codex_headers` takes the
scope from the translated body's `prompt_cache_key`, so every transport and
retry path sends the same value; a request routed without a conversation
identity (the auto-review classifier) falls back to the session id.

On the ChatGPT Codex backend the `session_id` header drives cache affinity:
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
  refresh token on use).

## Codex rate limits

Codex ends a stream with an `error` event of type `usage_limit_reached` when a
window is spent, and emits a `codex.rate_limits` event during healthy streams.
The live WebSocket path answers a spent window once, without retrying, with
`x-should-retry: false` plus `anthropic-ratelimit-unified-status: rejected`,
`-reset` and `-representative-claim`. `Retry-After` is deliberately not sent
because clients sleep for its full value, which here is hours. Healthy
readings become `-5h-utilization` / `-5h-reset` / `-7d-*`, and a window past
its threshold (`CCP_CODEX_QUOTA_WARN_AT`, defaults 0.9 session / 0.75 weekly)
adds `-surpassed-threshold`; readings whose reset time has passed are dropped.

Field-name trap: the wire format says `reset_at` / `reset_after_seconds`, while
Codex CLI session logs reserialise the same data as `resets_at` /
`resets_in_seconds`. Both spellings are read. If quota headers ever stop
appearing, compare against a fresh traffic capture before anything else.

Not covered: the buffered HTTP path (`client.rs`, `first_retryable_failure`)
and a 429 on the WebSocket handshake.

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

## Codex model inventory

`/v1/models` and the `models` command do not read a compiled-in Codex list.
`providers/codex/models.rs` calls `GET {api root}/models?client_version=X` on
the Codex CLI's bearer (the completions URL with `/responses` replaced by
`/models`), forwards the entries as the backend sent them, and remembers the
slugs so routing and `assert_allowed_model` accept them; `use_responses_lite`
from the listing overrides the compiled-in lane table. Nothing is cached
across calls or restarts, deliberately: a consumer that reads the listing as
the truth about available models must see changes, not a stale fallback.

Verified live 2026-09-11: the call answers 200 with the proxy's own
`originator`, with or without `ChatGPT-Account-Id`, and while the weekly
window is at 100% (`/backend-api/wham/usage` reported `limit_reached`); it
answers 400 without `client_version`. The version comes from
`CCP_CODEX_CLIENT_VERSION`, else the Codex CLI's `models_cache.json` next to
`auth.json`, else `CODEX_CLIENT_VERSION` in `auth/constants.rs`.

The compiled-in `CODEX_MODELS` / `ALLOWED_MODELS` lists still exist for
routing before the first listing and for the OpenAI-compatible surfaces'
error text. They are no longer what `/v1/models` advertises. Adding a model
there is optional; when done, keep `src/registry.rs` and
`src/providers/codex/translate/model_allowlist.rs` in sync and update the
`assert_allowed_model` test.

`/v1/models` deliberately emits no Anthropic rows: the proxy holds no
Anthropic credential, and Claude Code lists its own models. The anthropic
entry in `providers[]` says so (`auth: client`). No id in `data[]` may contain
`claude` or `anthropic`, because Claude Code's gateway discovery keeps such
ids as picker rows.

## How Codex models get into Claude Code's picker

Verified against Claude Code 2.1.266 by reading the binary and by live runs:

- `modelPicker` in user settings (`options[]` of `model`, `label`,
  `description`, `behavesAs`) is the supported way. Rows use bare ids, need no
  credential, and keep the subscription path. Present since 2.1.261.
- Gateway discovery (`CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1` fetching
  `/v1/models`) runs only with `ANTHROPIC_AUTH_TOKEN`, `apiKeyHelper`, or an
  API key, keeps only ids matching `/(claude|anthropic)/i`, and any of those
  credentials makes Claude Code drop the `RateLimitEvent` for every route and
  disable claude.ai connectors. Do not build on it.
- The bootstrap call that carries Anthropic's own extra picker rows goes
  straight to `api.anthropic.com`, never through `ANTHROPIC_BASE_URL`.
- Env-only alternatives exist but are narrower: `ANTHROPIC_DEFAULT_*_MODEL`
  remaps a built-in row (with `..._MODEL_DESCRIPTION` for its subtitle), and
  `ANTHROPIC_CUSTOM_MODEL_OPTION` (+ `_NAME`, `_DESCRIPTION`) adds one row.

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
  first-party host. `ENABLE_TOOL_SEARCH=true` restores it; this proxy forwards
  the `tool_reference` blocks, so the flag is safe to set.
- Claude Code's web search is a client-side `WebSearch` function tool. The
  hosted `web_search_20250305` tool only appears inside an isolated, history-free
  inner call, so its `server_tool_use` and `web_search_tool_result` blocks never
  enter the outer transcript. The passthrough logs `hosted_web_search_in_history`
  if that ever changes, which would mean this assumption needs rechecking.
- `MODEL_ALIASES` in the codex allowlist still maps Claude alias names to codex
  models. That only matters when `CCP_ALIAS_PROVIDER=codex`; the default keeps
  Claude names on Anthropic.

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
