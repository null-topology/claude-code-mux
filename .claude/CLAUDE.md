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
   `/v1/messages/count_tokens`, `/v1/models`. Optional OpenAI-compatible
   surfaces gated by `AppFeatures`: `/v1/responses` and `/v1/chat/completions`
   (`CCP_CODEX_RESPONSES_API`), `/v1/images/*` (`CCP_CODEX_IMAGES_API`),
   `/v1/audio/transcriptions` (`CCP_CODEX_TRANSCRIPTIONS_API`).
   `dispatch_request` handles the Anthropic routes: read the body, normalize
   the model (strip `[1m]`), look up the session by `x-claude-code-session-id`,
   pick a provider via the registry, apply the auto-review override (Claude
   Code's non-streaming, tool-free security classifier is rerouted to
   `CCP_AUTO_REVIEW_MODEL`, default `gpt-5.6-luna` on codex), then call the
   provider and record log, monitor and traffic capture. Every response it
   returns, early rejections and the local agent-summary answer (with
   `CCP_AGENT_SUMMARY=local`) included,
   carries a `request-id` header with the proxy's `req_id` unless the upstream
   already set one (the passthrough relays Anthropic's); Claude Code records it
   as `requestId` in transcripts. The stamping lives in `dispatch_request`, not
   in `monitor_response_body`, so the OpenAI-compatible surfaces do not get it.
   `handler_models` (`/v1/models`) calls `Provider::list_models` on every
   registered provider (or the one named by `?provider=`), tags each row with
   `provider`, and adds a top-level `providers[]` block with `auth`, `source`,
   `status`, `detail`, `fetched_at` per provider. The filtered form fails with
   502 when that provider cannot list; the aggregate form is always 200.
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

The Codex lane setting has a chain of its own in `config.rs`:
`CCP_CODEX_LANE_POLICY` (`full|inventory`, default `full`), then the legacy
`CCP_CODEX_FULL_LANE`, then `codex.lanePolicy`, then the legacy
`codex.fullLane`. The first source that parses wins. For both env keys and for
`codex.lanePolicy`, a value that does not parse is invalid rather than unset:
it is skipped and reported by `config_override_summary_lines`, which names the
source without echoing what was set. `codex.lanePolicy` is deserialized as a
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
backend's own counts can arrive arbitrarily late. The price is a map that grows
with the number of requests served until the process restarts. Records hold
numbers and small metadata only — no prompt, no request or response body, no
output text. Every mutation goes through `Ledger::update`, which detaches a
record's contribution from the rows it feeds, applies the change, and attaches
it again, so a row is always the sum of the records attached to it; one signed
`UsageDelta` path carries the numbers, and a change worth nothing numerically
still counts, because a missing count arriving as a reported zero moves the
evidence behind a total. Metadata is ordered by `(started_at, rank)` and a row
shows the metadata of the request with the greatest order, so a late report for
an older request cannot take a row's model or status back; a session's project
and a conversation's parent have independent watermarks (`project_order`,
`parent_order`) because a request states them separately from being routed. In
`note_metadata` a `None` never erases a value already known.

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
as `Σ <id>` with `mixed N` when the rollups name more than one model, lists the
`models` rollups, the `unattributed` row and an `evidence` line in the session
detail, prints the 5m/1h buckets as parts reported separately, marks a
conversation whose `raw_parent` was never resolved with `^`, and keys the
selection by row identity so a re-render keeps it (`(selection reset)` in the
pane title when the row is gone). Meaning is never carried by color alone.
The `demo` command shows all of it from `src/monitor/mock.rs`.

`session_summaries` orders sessions by activity: sessions with a request in
flight first, then by `last_seen` descending, then by `first_seen_rank`
descending as the tie-break, whichever model or conversation made the latest
request; the conversations under a session keep `order_conversations`' tree
order. The bottom pane is tabbed (`BottomTab`: `Events`, `Stats`); Tab cycles
`FocusPane` Sessions → Recent → Bottom, and while Bottom has focus Left/Right
switch the tab (`MonitorApp::navigate`) and Up/Down/j/k scroll it, with no
row identity to keep. The title brackets the shown tab (`[Events] Stats`).
The Stats tab renders `MonitorState::model_stats`: one `ModelStats` per
(provider, effective model), the session `models` rollups summed across every
session, with `local` rows left out. Its cache miss tally is `CacheMissTally`
on `ModelUsage`, fed by `Contribution.miss` from the `CacheMiss` a request's
evaluation stored on its record, so a per-model miss is counted exactly once
and detached with the record like every other number; the judging itself is
unchanged. Prompt is `input + read + write` (no `reported_prompt_tokens` at
this level), hit % is `totals_cache_hit_ratio`, a Codex row's cache write stays
`Missing` (`n/a`) rather than zero, and `Lat(rec)` / `tok/s(rec)` are medians
over the row's completed requests still in `recent` (the mean of the two
middle values for an even count), so they cover the recent window only and
the header says so. Rows sort by prompt tokens descending.

Deferred and non-blocking: `RequestRecord::model_key` and the requested-model
histogram allocate `String`s on every ledger update.

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
and Codex's is `gpt-5.6-luna`; a provider without an entry in
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
  refresh token on use).

## Codex rate limits

Codex ends a stream with an `error` event of type `usage_limit_reached` when a
window is spent, and emits a `codex.rate_limits` event during healthy streams.
Every transport answers a spent window once, without retrying, with
`x-should-retry: false` plus `anthropic-ratelimit-unified-status: rejected`,
`-reset` and `-representative-claim`: the live WebSocket and live HTTP
streams, a non-2xx startup status whose body or `X-Codex-*` response headers
carry the limit, the buffered paths (`usage_limit_from_response` runs ahead of
`first_retryable_failure`, so a buffered WebSocket relay shares the branch),
and an opt-in server compaction request, which aborts instead of spending the
normal request as well. The live HTTP stream hands its response headers to
`usage_limit_from_event_with_headers`, so a limit event that carries only a
relative reset still gets its window from the `X-Codex-*` headers; a header
inside the event payload wins over the HTTP one. Only the explicit
`usage_limit_reached` type is
terminal; a transient 429 that happens to carry a reset clock stays retryable.
The representative claim is published only for a window duration that
`claimable_window` in `rate_limits.rs` recognises (around 300 minutes for the
five hour window, around 10080 for the weekly one; Codex has reported 299),
and an ambiguous reset clock yields neither a substituted header clock nor a
claim. `Retry-After` is deliberately not sent
because clients sleep for its full value, which here is hours. Healthy
readings become `-5h-utilization` / `-5h-reset` / `-7d-*`, and a window past
its threshold (`CCP_CODEX_QUOTA_WARN_AT`, defaults 0.9 session / 0.75 weekly)
adds `-surpassed-threshold`; readings whose reset time has passed are dropped.

Field-name trap: the wire format says `reset_at` / `reset_after_seconds`, while
Codex CLI session logs reserialise the same data as `resets_at` /
`resets_in_seconds`. Both spellings are read. If quota headers ever stop
appearing, compare against a fresh traffic capture before anything else.

The proxy does not retry a failed Codex request: one attempt, then the error
goes to the client, which owns the retry policy. The only resends left repair
the proxy's own state (a forgotten `previous_response_id`, a 401 token refresh).
A 429 on the WebSocket handshake reaches the client as a plain 429.

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
`/models`), forwards the entries as the backend sent them, and remembers the
slugs so routing and `assert_allowed_model` accept them; under the `inventory`
lane policy `use_responses_lite` from the listing overrides the compiled-in
lane table, while under the default `full` policy the proxy forces the full
lane for every model and ignores that flag entirely
(`uses_responses_lite_with_full_lane` in
`providers/codex/translate/model_allowlist.rs`). Nothing is cached
across calls or restarts, deliberately: a consumer that reads the listing as
the truth about available models must see changes, not a stale fallback.

Routing reads a process-wide catalog in `src/registry.rs` (`LISTED_MODELS`):
per provider, the ids of its last successful listing from a backend
(`source: upstream`), which replaces that provider's compiled-in list; a
provider that has not listed yet routes on its compiled-in list, and bundled
listings (kimi, grok, cursor today) are not recorded. `/v1/models` calls
`list_models` on every provider regardless; being listed automatically, at
start and on a miss, also needs `has_credentials`, which defaults to false,
so a provider gains that by implementing both. Ownership never depends on
name order: an id a compiled-in list names stays with that provider, and its
own listing only says whether it still serves it; an id only listings name
goes to the provider whose current listing has named it the longest (the
catalog numbers each recorded listing). The catalog is filled at three
points, all through `record_listing`:

- every `/v1/models` call (`handler_models`);
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
bodies. `/v1/models` still reports such an empty answer as it came.

Hosted web search sits outside the policy: a request carrying the hosted
`web_search_20250305` tool is forced onto the full lane, and there a non-forced
`gpt-5.6-luna` is rewritten to `gpt-5.6-sol` (`apply_model_lane_for_request`,
`full_lane_web_search_model`). The forced standalone `/alpha/search` path
(`is_standalone_search_request`) builds a request of its own and keeps the
model that was asked for. The rewrite rests on no reproducible capture of a
Luna refusal on the full lane, and no live capability check has been run
either way.

Verified live 2026-09-11: the call answers 200 with the proxy's own
`originator`, with or without `ChatGPT-Account-Id`, and while the weekly
window is at 100% (`/backend-api/wham/usage` reported `limit_reached`); it
answers 400 without `client_version`. The version comes from
`CCP_CODEX_CLIENT_VERSION`, else the Codex CLI's `models_cache.json` next to
`auth.json`, else `CODEX_CLIENT_VERSION` in `auth/constants.rs`.

The compiled-in `CODEX_MODELS` / `ALLOWED_MODELS` lists still exist for
routing until the first successful listing and for the OpenAI-compatible
surfaces' error text. They are no longer what `/v1/models` advertises. Adding
a model there is optional; when done, keep `src/registry.rs` and
`src/providers/codex/translate/model_allowlist.rs` in sync and update the
`assert_allowed_model` test.

`/v1/models` deliberately emits no Anthropic rows: the proxy holds no
Anthropic credential, and Claude Code lists its own models. The anthropic
entry in `providers[]` says so (`auth: client`). No id in `data[]` may contain
`claude` or `anthropic`, because Claude Code's gateway discovery keeps such
ids as picker rows.

## How Codex models get into Claude Code's picker

Verified against Claude Code 2.1.266 by reading the binary and by live runs;
the FABLE variable below on 2.1.269:

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
  The slot variables are `ANTHROPIC_DEFAULT_OPUS_MODEL`, `..._SONNET_MODEL`,
  `..._HAIKU_MODEL` and `..._FABLE_MODEL`; the fable one was read from a
  2.1.269 binary and the CLI ships new builds quickly, so check the version in
  use before relying on it.

Those four variables give a whole-picker recipe — haiku to `gpt-5.6-luna`,
sonnet to `gpt-5.6-terra`, opus to `gpt-5.6-sol`, fable to `gpt-6-astra`. It is
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
