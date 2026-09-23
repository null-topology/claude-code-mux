## Unreleased

- The monitor's Sessions pane is ordered by activity: sessions with a request
  in flight first, then by their latest request, newest first, whichever model
  or subagent conversation made it. Conversations keep their tree order under
  the session and the selected row stays selected when the order changes.
  Rows were sorted by session id before, which buried a new session among
  the one-off ones.
- The bottom pane of the monitor is tabbed: `Events` as before and a new
  `Stats` tab with one row per backend and model over every session since
  the proxy started (requests and failures, prompt tokens, cache hit rate,
  uncached input, cache write, cache misses split within-lifetime / expired,
  output, and median latency and output rate over the recent-request window).
  `Tab` now reaches the bottom pane; `Left`/`Right` switch its tabs while it
  has focus and keep their meaning elsewhere. Every count carries its quality
  mark the way the other panes do, and a Codex row shows its cache write as
  `n/a`, since that backend never reports one.
- A Codex model that only the backend lists, such as `gpt-6-sol` or
  `gpt-6-luna`, routes from the first request. A freshly started proxy used to
  answer `Unknown model` for it until something called `/v1/models`. `serve`
  now asks every backend it holds a login for which models it serves as it
  starts, in the background, and a request for an unrecognised model asks once
  more before the 400; that second ask runs at most once every 30 seconds.
  Once a backend has answered, its list is what routes, in place of the list
  built into the proxy. An answer that names no model is ignored and the last
  good list keeps routing, a model the built-in lists place with one backend
  stays with it whatever another backend lists, and every list taken over is
  logged with its size and what changed.

## v0.10.0 (2026-09-23)

- Claude Code's subagent progress label is forwarded natively by default
  (`CCP_AGENT_SUMMARY=native`): the request is routed and relayed like any
  other for its model, with no rewrite beyond the Codex tool choice described
  below. A native label reports its real usage
  to the client; the local answer reported zero input tokens, so anything
  reading usage saw a request of the wrong size. `CCP_AGENT_SUMMARY=local`
  (or `agentSummary: "local"` in `config.json`) brings back the previous
  behavior, and `upstream` still pins the provider's junior model.
- A natively forwarded label can fail like any request (429, 5xx, a spent
  Codex window), which it never did while it was answered locally.
- A label request whose prompt is followed by a trailing system message, as
  Claude Code sends mid-conversation to carry reminders, is now recognized.
  Before, such requests went upstream unrecognized even in `local` mode.
- An empty or unrecognized `CCP_AGENT_SUMMARY` is skipped in favor of
  `config.json` and then `native`. Values are trimmed and case-sensitive.
- In `upstream` mode the junior model is now kept when a Codex model override
  (`CCP_CODEX_MODEL`) is set; before, the override replaced it.
- A progress label routed to Codex, in `native` and `upstream` mode both, now
  goes out with `tool_choice` set to `none`. The client attaches the
  subagent's tools and only asks in prose not to use them, and Codex models
  often answered a label with a tool call, which the client discards. Measured
  on the ChatGPT backend, the setting is accepted by every listed Codex
  model, enforced, and leaves the cached prefix intact because the tools stay
  in the request. The Anthropic route is untouched: a `tool_choice` change
  invalidates Anthropic's messages cache
  while the passthrough stays byte-exact.
- Label detection now also consults Claude Code's `x-claude-code-request-class`
  header: a request classed as anything other than `auxiliary` is not a label
  however its prompt reads. The header is a guard, not the detector, because
  Claude Code sends `auxiliary` on every side request, and a request with no
  header stays eligible, as older clients send none.

## v0.9.2 (2026-09-22)

- Deferred tools stay out of the Codex tools head, the placeholder included.
  When a compaction or a context manager summarized the turns holding a
  `ToolSearch` exchange, the tool it had loaded used to be rendered into the
  head as a plain function. That changed the first bytes of the prompt and
  cost a full prompt-cache miss, over 100k uncached tokens at a time; the
  prefix before the rewritten turns now stays cached. A deferred tool that a
  directed `tool_choice` names while no search loaded it still goes into the
  head. The head changes once with this release, so a conversation cached
  under an older build misses the cache on its first request after the
  upgrade.
- Every `/v1/messages` and `count_tokens` response carries a `request-id`
  header: the proxy's own id, unless the backend already sent one. Claude
  Code records it as `requestId`, and transcript consumers that de-duplicate
  on it no longer count repeated records twice.
- Tool schemas sent to Codex lose their JSON Schema `pattern` keywords,
  because OpenAI rejects some patterns Claude Code sends, such as Unicode
  property escapes. Literal values and property names are kept.
- The compaction effort cap (`CCP_COMPACT_EFFORT`, default `low`) also
  applies when a compaction request names no effort, and it still never
  raises an effort the request named. An effort of `none` goes on the wire
  as is but asks for no reasoning summary and no encrypted reasoning.
- On the live HTTP stream a spent Codex window takes its length and reset
  time from the response headers when the limit event carries only a
  relative reset, as the WebSocket path already did.
- JSON traffic captures replace `encrypted_content` and the proxy's own
  reasoning signatures with their length. Raw SSE and byte captures are
  unchanged.
- The installer recognizes an installed binary whose version has no leading
  `v`.

The request-id, capture, schema, compaction effort, quota header and
installer changes are adapted from the original project, with their authors
credited as co-authors.

## v0.9.1 (2026-09-21)

- A Codex completion with no output is a normal end of turn. The backend
  answers this way when the model has nothing to add after a tool result, and
  the answer is the same however often the request is repeated. It used to be
  resent and then reported as a 503, which the client retried in turn, so one
  such turn became dozens of full-context requests. It now reaches the client
  as `200` with `stop_reason: end_turn` after exactly one upstream request.
- The Codex route no longer retries a failed request. A 429, a 5xx, a stream
  that drops before its first output and a transport error are reported after
  one attempt, with the status and `Retry-After` the backend sent, and the
  client applies its own retry policy. Still repaired by the proxy, because
  they concern its own state: one full-context resend when the backend no
  longer knows the previous response, the token refresh after a 401, and the
  WebSocket to HTTP fallback in `auto` transport.
- Errors on `/v1/messages` are worded the way the Anthropic API words them. A
  text that names the backend or the transport becomes `Internal server
  error`, `Overloaded` or `Rate limited` by error type, in the JSON body and
  in a mid-stream `event: error`; the native reason is what `proxy.log`, the
  error capture and the monitor record. Sign-in errors, the spent-window
  response, context-window overflow and any other backend 4xx keep their own
  text.

## v0.9.0 (2026-09-20)

- A Codex completion that ends without any output is no longer re-issued
  ten times. Those retries added up to almost 200 seconds during which the
  client received no bytes at all and usually gave up on response headers
  before the 503 was sent. Both the live-stream and the buffered paths now
  stop after two retries, so the 503 reaches the client within seconds and
  its own retry policy applies. Other retryable errors keep their budget.
- The server accepts `POST /messages` and `POST /messages/count_tokens`
  next to the `/v1` spellings, for gateways that address a custom upstream
  with either form. The path is folded back to the canonical `/v1` path
  before the Anthropic relay, with the query string kept verbatim, so the
  alias never reaches the upstream.

## v0.8.1 (2026-09-14)

- A spent Codex window is answered once on every transport. The buffered
  HTTP and WebSocket paths and the live HTTP stream, including a non-2xx
  startup status whose body or `X-Codex-*` headers carry the limit, used to
  retry a `usage_limit_reached` four times and then answer a bare 429 with no
  reset time; they now answer once with `x-should-retry: false`, the
  `anthropic-ratelimit-unified-*` status, reset and representative claim, as
  the live WebSocket path already did. Only the explicit `usage_limit_reached`
  type is terminal: a transient 429 that happens to carry a reset clock stays
  retryable instead of becoming a final refusal.
- The exhausted window is identified from the reset time in the body when no
  countdown is present. When the five-hour and weekly readings stay
  ambiguous, no reset clock is invented from the headers and no
  representative claim is published. The claim is published only for a window
  duration the proxy recognises, around 300 minutes or around 10080.
- A server-side compaction request that hits the limit stops at once and
  clears its pending state, instead of letting the normal request spend the
  same exhausted quota again.
- These corrections were written by the upstream maintainer, Raine Virta,
  when merging this fork's pull request raine/claude-code-proxy#139, and are
  carried here with his authorship.

## v0.8.0 (2026-09-14)

- Auto mode's security classifier is never answered as a progress label any
  more. The classifier carries the subagent label prompt inside the transcript
  it evaluates, and the local label detector matched that text anywhere in the
  last message, so the classifier received a four-token label, Claude Code
  retried ten times and reported that it could not evaluate the action. The
  classifier is now recognised first, and the detector requires the label
  instruction to open the last text block of the last user message. This
  closes the known limitation of v0.7.0.
- The Codex lane is an explicit policy. `CCP_CODEX_LANE_POLICY=full|inventory`
  (or `codex.lanePolicy`) replaces the boolean full-lane switch, which stays
  accepted as a legacy source. `full`, the default, sends every model through
  the full Responses lane; `inventory` follows the `use_responses_lite` flag
  from the backend's listing, else the compiled-in table. The first source
  that parses wins, an unparsable value is reported without being echoed, and
  a bad `codex.lanePolicy` no longer fails the whole config file.
- Every token count in the monitor carries its quality: missing, an opening
  estimate, or an exact closing report, with a reported zero distinct from no
  report. The TUI marks an estimate with `~`, a count nobody reported with
  `n/a`, and shows an exact one plain. The Anthropic 5m and 1h cache-write
  buckets keep a quality of their own and are never inferred from the
  aggregate write; a prompt total a backend measured itself is kept apart from
  the four categories.
- A response that fails mid-stream is recorded as failed. An SSE `error` event
  or a body that stops before its terminal event marks the request Failed even
  though the client already received HTTP 200, and the first cause wins.
  Monitor usage comes from the counts the backend reported rather than from a
  synthetic closing event, and a salvaged tool call no longer closes an
  estimate with a zero.
- The monitor keeps a compact ledger of every request past its eviction from
  the recent list, so a late usage report still corrects every session,
  conversation and model total it fed. Requested and executed models are
  tracked separately: the id the client named before any override, and the
  model of a request actually built for a backend. Request rows name the model
  that ran and mark a routed-only one with `?`; locally answered requests read
  `local answer`; the session root is an aggregate row with per-model rollups,
  an unattributed row and an evidence line in its detail; a conversation whose
  parent was never seen is marked `^`. Selection survives a re-render, and
  meaning is never carried by colour alone. The `demo` command shows all of it.
- A progress label answered from the transcript reports its usage as exact
  (zero input, zero cache, the label's tokens out) instead of as an estimate,
  and no longer touches its conversation's context size.
- The Anthropic routes accept request bodies up to 64 MiB, so image-heavy
  Claude Code histories get through; an oversized body is answered with the
  Anthropic-shaped 413 `request_too_large` instead of a misleading invalid-JSON
  400. The OpenAI-compatible routes keep their 16 MiB limit. Ported from
  upstream.
- The README describes the whole proxy: a feature overview, what this fork
  adds, a code-verified comparison with `raine/claude-code-proxy` and
  `fcakyon/claude-code-with-codex`, every HTTP route and header, the complete
  `CCP_*` and `config.json` surface, file locations per platform, the TUI panes
  and key bindings, which backend runs which Claude Code slot, troubleshooting
  and sensitive data. Stale statements about the curated catalog, the
  classifier limitation, quota thresholds and install coverage are corrected.
- The test suite stays away from real credentials and backends: routing smokes
  assert the registry only, logout checks run in a temporary home, and the
  native tool search mapping and the lane wire shape are covered by tests that
  pin the policy explicitly.

## v0.7.0 (2026-09-12)

- Codex models answer with several tool calls at once again. Codex marks the
  gpt-5.6 family and `gpt-6-astra` for the Responses Lite lane, which rejects
  `parallel_tool_calls: true`, so a model served through it answered at most one
  tool call per turn and Claude Code's batched tool use turned into one request
  per call, each carrying the whole conversation. The proxy now uses the full
  Responses lane for them. `CCP_CODEX_FULL_LANE=0` (or `codex.fullLane: false`)
  restores the previous behaviour.
- The Sessions view groups requests into conversations, with the main thread,
  nested subagents and side calls shown as a tree. Each conversation has its
  own model, context and cache figures.
- Each conversation gets its own prompt cache scope on the Codex backend.
  Claude Code gives a subagent its parent's session id, so a session and all of
  its subagents sent one `prompt_cache_key` and one set of routing headers,
  putting unrelated prompts under one cache accounting key and one affinity.
  A subagent now derives an id of its own, the way the Codex CLI keeps one id
  per conversation.
- Claude Code's background-agent status line no longer costs a model call. While
  a subagent runs, Claude Code resends that subagent's whole context every half
  minute for a three-word progress label; in a measured capture those requests
  were a quarter of all Codex traffic, tens of thousands of tokens each. The
  proxy now recognises the prompt and answers it from the transcript, on every
  route, since a setup may have no Anthropic subscription at all.
  `CCP_AGENT_SUMMARY=upstream` sends them to a model again, and then to the
  provider's junior model at the lowest effort rather than to the subagent's
  own: `claude-sonnet-5` on Anthropic, because Haiku's 200k window would leave a
  long-running subagent without a label, and `gpt-5.6-luna` on Codex.
  `CCP_AGENT_SUMMARY_MODEL` overrides both. Known limitation of this release:
  that request is recognised by its instruction text, so a request that merely
  quotes those instructions can be answered with a progress label instead of
  being routed. Auto mode's security classifier sends the action it reviews as
  text and can be taken for one, leaving that action unevaluated. A narrower
  detector is a separate change.
- Loading a deferred tool on a Codex model no longer throws away the prompt
  cache. Claude Code's `ToolSearch` is sent to Codex as the backend's own
  client-executed tool search: the loaded tool's schema travels in a
  `tool_search_output` item at the point of the search, and the tools list at
  the start of the prompt stays unchanged. Before, the loaded tool was added
  to that list, so the request right after every load was served with no
  cached tokens. The `tool_reference` placeholder text is gone as well.

## v0.6.0 (2026-09-11)

- The monitor shows the prompt cache: cache reads and writes next to the
  uncached input, the hit rate per request and per session, the session's
  context size with how long its cache stays warm, and cache misses with the
  tokens reprocessed and whether the cache had expired or was lost while it
  should have been alive. Misses are judged per conversation and model, so
  subagents, side calls such as titles and web search, compaction and
  overlapping requests do not show up as misses.
- Claude models report their token usage to the monitor, read from the
  relayed response without changing it.
- Codex token counts in the monitor end at the backend's figures instead of
  the estimate of the whole prompt sent when the stream starts, and
  `count_tokens` estimates no longer add to session totals.

## v0.5.0 (2026-09-11)

- `/v1/models` and `claude-code-mux models` list Codex models from the Codex
  backend's own inventory, on the Codex CLI login, instead of a list compiled
  into the proxy. The call spends no completion quota and answers while a
  usage window is spent. Every row carries a `provider` field and a top-level
  `providers` block reports, per backend, whether the proxy holds the login
  (`auth`), where the rows came from (`source`: `upstream`, `bundled`, or
  `none`), and whether the backend answered (`status`). `?provider=<name>`
  narrows the answer to one backend and fails with 502 when it cannot list.
- A model the Codex backend lists routes to codex without a proxy release,
  `-fast` variant included, and its Responses Lite lane follows the backend's
  flag.
- The Anthropic passthrough is reported in `providers` as `auth: client` with
  no rows; the curated Codex catalog with picker labels is gone.
- `CCP_CODEX_CLIENT_VERSION` (or `codex.clientVersion` in `config.json`) sets
  the `client_version` the listing call requires; by default it is read from
  the Codex CLI's `models_cache.json`.

## v0.4.0 (2026-09-09)

First release under the `claude-code-mux` name, continuing
`fcakyon/claude-code-with-codex` 0.3.1. Entries below v0.4.0 are the upstream
`raine/claude-code-proxy` history this fork was built on.

- The crate, binary, and repository are renamed to `claude-code-mux`. On-disk
  config and state directories, the keychain service, and the Codex originator
  keep their previous names so existing logins keep working.
- A spent Codex window is reported to the client once, with the reset time,
  instead of being retried until the proxy gives up; Claude Code shows its
  native session-limit message and the Agent SDK receives a `RateLimitEvent`.
- Codex quota readings are forwarded as the `anthropic-ratelimit-unified-*`
  headers Claude Code reads, so its usage warning works on Codex models.
  `CCP_CODEX_QUOTA_WARN_AT` sets the warning threshold.
- `/v1/models` no longer repeats the Claude models Claude Code already knows
  and lists Codex through a curated catalog with picker labels and
  descriptions. `gpt-6-astra` is accepted.
- README documents the `modelPicker` setup for Claude Code, the Agent SDK,
  and the authentication modes that keep rate-limit events working.

## v0.1.32 (2026-08-03)

- Kimi subagents and multimodal messages with mixed text and images work instead
  of failing with an invalid content-part error.
  ([#98](https://github.com/raine/claude-code-proxy/issues/98),
  [#99](https://github.com/raine/claude-code-proxy/pull/99))

## v0.1.31 (2026-08-02)

- OpenCode Go subscriptions can power Claude Code with supported OpenAI, Google,
  and Anthropic models through the new OpenCode Go provider.
- Codex HTTP responses stream as they arrive, remain active during quiet periods,
  and recover safely from temporary failures before model output begins.
  ([#51](https://github.com/raine/claude-code-proxy/pull/51))
- OpenAI-compatible requests preserve the caller's parallel tool-call setting
  across Codex, Kimi, and Grok routes.
- Claude Code agents sharing a session keep independent Codex continuation state,
  preventing one agent from consuming or replacing another agent's context.
  ([#96](https://github.com/raine/claude-code-proxy/pull/96))
- Overlapping Codex compaction requests preserve the correct conversation history
  and summary. ([#94](https://github.com/raine/claude-code-proxy/pull/94))
- Monitor session token totals remain accurate as older requests leave the recent
  request list or receive stale usage updates.

## v0.1.30 (2026-07-31)

- OpenAI-compatible clients can use Kimi, Grok, Cursor, or Codex through the
  optional `POST /v1/chat/completions` and `POST /v1/responses` endpoints, with
  support for streaming, reasoning, function tools, usage, and provider routing.
- Codex users can transcribe audio through the optional OpenAI-compatible
  `POST /v1/audio/transcriptions` endpoint using the existing Codex sign-in.
  Enable it with `codex.transcriptionsApi` or
  `CCP_CODEX_TRANSCRIPTIONS_API=1`.
- Codex token estimates accurately count CJK text, long identifiers, minified
  code, and base64-like content, improving context and compaction decisions.
  ([#90](https://github.com/raine/claude-code-proxy/pull/90))
- Codex WebSocket sessions remain reliable under high concurrency instead of
  failing with 403 upgrade rejections.
  ([#87](https://github.com/raine/claude-code-proxy/issues/87),
  [#88](https://github.com/raine/claude-code-proxy/pull/88))

## v0.1.29 (2026-07-30)

- Codex honors required, disabled, and single-tool choices from Claude Code, and
  disables parallel tool calls when requested.
  ([#89](https://github.com/raine/claude-code-proxy/pull/89))

## v0.1.28 (2026-07-29)

- OpenAI-compatible clients can generate and edit images with `gpt-image-2`
  through optional Codex Images API routes using the existing ChatGPT sign-in.
  ([#85](https://github.com/raine/claude-code-proxy/pull/85))
- Codex streams show estimated input usage from the start and exact usage at
  completion, keeping Claude Code's live token counters useful.
  ([#86](https://github.com/raine/claude-code-proxy/pull/86))

## v0.1.27 (2026-07-29)

- Grok streams remain reliable during long responses, keepalive events, and
  output-token truncation instead of failing after partial output.
- Forced Codex web searches keep the selected model, so Luna searches no longer
  switch to Sol, while preserving domain filters and search usage reporting.
  ([#53](https://github.com/raine/claude-code-proxy/pull/53))
- Codex WebSocket connections honor `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`,
  and `NO_PROXY`, restoring standard non-TUN HTTP proxy support.
  ([#83](https://github.com/raine/claude-code-proxy/pull/83))

## v0.1.26 (2026-07-28)

- Standard OpenAI clients can use Codex through the optional
  `POST /v1/chat/completions` endpoint, with streaming, reasoning effort, and
  structured output support.
- Cursor Agent works with current client versions and restores text, thinking,
  usage, model mode, and fast-mode handling.
- Claude Code's `claude-opus-5` model name routes correctly through Codex and
  Kimi.
- Claude Code's automatic security-review classifier can use a dedicated model
  configured with `CCP_AUTO_REVIEW_MODEL` or `autoReviewModel`.
  ([#72](https://github.com/raine/claude-code-proxy/pull/72))
- Codex retries empty successful completions and returns a clear error if no
  usable response arrives. ([#70](https://github.com/raine/claude-code-proxy/pull/70),
  [#71](https://github.com/raine/claude-code-proxy/pull/71))
- Grok handles images in user messages and tool results without failing requests.
  Images are omitted by default, with opt-in vision through
  `CCP_GROK_TOOL_IMAGE`. Traffic captures redact image payloads.
  ([#69](https://github.com/raine/claude-code-proxy/pull/69))
- Nix builds use vendored dependencies for reproducible sandboxed builds.
  ([#80](https://github.com/raine/claude-code-proxy/pull/80))

## v0.1.25 (2026-07-24)

- Kimi users can select Kimi K3 with the `kimi-k3` or `k3` model name, including
  its one-million-token context window and `max` reasoning effort.
  ([#79](https://github.com/raine/claude-code-proxy/pull/79))
- The monitor shows active native Codex compaction requests with a dedicated
  `compacting` status.

## v0.1.24 (2026-07-23)

- Codex optionally preserves conversation continuity across Claude Code
  compaction boundaries with native encrypted compaction artifacts. Enable it
  with `codex.serverCompaction` or `CCP_CODEX_SERVER_COMPACTION`.
- Native OpenAI Responses clients can use `POST /v1/responses` with existing
  Codex authentication, including JSON responses, SSE streaming, and automatic
  token refresh. Enable the endpoint with `codex.responsesApi` or
  `CCP_CODEX_RESPONSES_API=1`; it is disabled by default.

## v0.1.23 (2026-07-22)

- Codex WebSocket streaming handles pooled connections and HTTP fallback more
  reliably, preventing concurrent requests from blocking each other or sending
  the same request twice.
- Codex errors preserve upstream status codes and optional retry timing, with
  clearer permission failures and safer WebSocket handshake diagnostics.
- Codex streaming limits oversized events and error responses, preventing
  malformed or stalled upstream responses from consuming unbounded memory.

## v0.1.22 (2026-07-20)

- Grok accepts request metadata and tool-result references sent by current Claude
  Code versions while continuing to reject malformed and unknown fields.
  ([#56](https://github.com/raine/claude-code-proxy/pull/56))
- Gateway model discovery lists every configured provider model and Claude-style
  alias through `GET /v1/models`, making supported aliases available in Claude
  Code's model picker. ([#60](https://github.com/raine/claude-code-proxy/issues/60),
  [#61](https://github.com/raine/claude-code-proxy/pull/61))
- Codex preserves supported base64 images in tool results while keeping mixed
  text, image, error, and fallback content in its original order.
  ([#59](https://github.com/raine/claude-code-proxy/pull/59))
- Codex requests continue after the included usage limit when account credits
  remain available. ([#68](https://github.com/raine/claude-code-proxy/pull/68))
- Codex compaction requests cap reasoning effort at `low` to reduce latency and
  reasoning-token usage. `CCP_COMPACT_EFFORT` can choose a different cap or
  disable the behavior. ([#67](https://github.com/raine/claude-code-proxy/pull/67))

## v0.1.21 (2026-07-15)

- The monitor shows session token activity trends at common terminal widths,
  making throughput history visible without an extra-wide window.

## v0.1.20 (2026-07-15)

- The monitor reliably shows project names for Claude Code sessions and keeps
  them visible as requests are sequenced.
- Keyboard navigation scrolls session and recent-request tables to keep the
  selected row visible.
- Pressing `q` asks for confirmation before gracefully shutting down the proxy.
- Compact monitor layouts show more project, provider, model, effort, and token
  details without requiring a wider terminal.

## v0.1.19 (2026-07-15)

- The monitor shows project and session context at more terminal widths while
  preserving key request details in narrower layouts.

## v0.1.18 (2026-07-15)

- Codex preserves encrypted reasoning across turns, improving continuity when
  conversation history is replayed. ([#52](https://github.com/raine/claude-code-proxy/pull/52))
- The new `demo` command opens the interactive monitor with simulated traffic,
  without starting a proxy server or requiring provider credentials.
- Session rows show project names and output-token activity over time, making
  concurrent sessions and usage bursts easier to identify.
- Monitor tables adapt more consistently across terminal sizes and keep important
  request details readable in compact layouts.
- The monitor stays visible during graceful shutdown and shows progress until the
  proxy finishes draining connections.

## v0.1.17 (2026-07-14)

- The proxy can listen on a configurable IP address through `CCP_BIND_ADDRESS`
  or `bindAddress`, enabling protected access from containers and remote hosts.
  ([#48](https://github.com/raine/claude-code-proxy/pull/48))
- Model names with context-window hints such as `[1m]` route correctly across
  providers. ([#50](https://github.com/raine/claude-code-proxy/pull/50))
- The monitor reports more accurate output rates by measuring generation time
  and excluding requests without complete usage and timing data.

## v0.1.16 (2026-07-13)

- GPT-5.6 Luna requests work without a custom User-Agent instead of failing with
  a model unavailable error.
  ([#45](https://github.com/raine/claude-code-proxy/issues/45))
- Canceled or replaced Codex prompts cannot interrupt later turns with stale
  continuation state.
- GPT-5.6 setup examples use a 272K compaction window to stay within the current
  ChatGPT context limit.
- Homebrew installations can run the proxy at login as a background service with
  `brew services start claude-code-proxy`.
  ([#44](https://github.com/raine/claude-code-proxy/pull/44))

## v0.2.0 (2026-07-13)

- Claude Code can now run on your Claude subscription and your ChatGPT (Codex)
  subscription at the same time, routed per request by model name. Claude models
  relay straight to Anthropic reusing Claude Code's own login, and gpt-5.6 models
  go to the ChatGPT plan through the Codex login.
- Reasoning is carried across a mid-conversation model switch. Earlier thinking
  is kept as tagged text so moving a conversation between the two plans does not
  lose context.
- The Codex login is read from the Codex CLI's own auth.json, so the proxy and
  the Codex CLI no longer invalidate each other's session.
- The command and crate are now named claude-codex. Install it from crates.io
  with `cargo install claude-codex`, or download a prebuilt binary from the
  releases page with no Rust needed.

## v0.1.15 (2026-07-12)

- Codex function tools preserve optional parameters, preventing unintended tool
  arguments and incorrect agent isolation choices.
  ([#43](https://github.com/raine/claude-code-proxy/issues/43))
- Forced Codex web searches return live results while preserving allowed and
  blocked domain filters.
  ([#26](https://github.com/raine/claude-code-proxy/issues/26))
- Codex credentials are stored and refreshed independently from the native Codex
  CLI, preventing either application from invalidating the other's login. Users
  who relied on the native Codex login must sign in to the proxy once after
  upgrading.
- [Expanded guidance](https://github.com/raine/claude-code-proxy/#switching-models-and-backends)
  explains how to switch models within the proxy and how to switch between the
  proxy and direct Anthropic.

## v0.1.14 (2026-07-12)

- Codex hosted web searches work when Claude Code routes them through the Luna
  small model. ([#26](https://github.com/raine/claude-code-proxy/issues/26),
  [#35](https://github.com/raine/claude-code-proxy/pull/35))
- Codex context-window errors trigger Claude Code's compaction flow instead of
  ending the request. ([#29](https://github.com/raine/claude-code-proxy/pull/29))
- Codex requests fall back to HTTP after WebSocket handshake failures while
  preserving live streaming for established connections.
  ([#39](https://github.com/raine/claude-code-proxy/pull/39))
- Codex HTTP and WebSocket failures retain upstream status codes and error
  details, making failures clearer and more actionable.
  ([#40](https://github.com/raine/claude-code-proxy/pull/40))

## v0.1.13 (2026-07-12)

- Grok users can sign in on headless hosts with `grok auth device`.
  ([#38](https://github.com/raine/claude-code-proxy/pull/38))
- Grok tool calls accept Claude Code's prompt-cache markers, preventing errors
  when switching to Grok during a tool-using session.
  ([#37](https://github.com/raine/claude-code-proxy/pull/37))
- Codex hosted web searches return their result links and citations to Claude
  Code instead of appearing to produce zero results.
  ([#10](https://github.com/raine/claude-code-proxy/issues/10))
- Codex authentication refresh is coordinated across concurrent requests and
  automatically recovers live WebSocket requests after credentials expire.
- Codex requests recover more reliably from temporary upstream failures,
  connection resets, overloads, and long-running responses.

## v0.1.12 (2026-07-12)

- Codex hosted web searches work with GPT-5.6 models instead of failing with an
  unsupported tool error. ([#26](https://github.com/raine/claude-code-proxy/issues/26),
  [#35](https://github.com/raine/claude-code-proxy/pull/35))
- Codex WebSocket connection timeouts are retried automatically, reducing
  interrupted requests.

## v0.1.11 (2026-07-11)

- Grok subscriptions can power Claude Code through browser login, with support for
  Grok 4.5 and Composer 2.5 Fast, streaming, thinking, tools, and token counts.
- Codex WebSocket requests recover from handshake failures and stay marked active
  until the full response body finishes streaming.
- The monitor shows local timestamps, clearer request status and detail indicators,
  more compact columns, arrow-key pane navigation, and an uncluttered display.
- Forward Claude Code's `max` effort as Codex `reasoning.effort: "max"` so
  GPT-5.6 can use its highest supported reasoning level instead of silently
  receiving `xhigh`. ([#28](https://github.com/raine/claude-code-proxy/pull/28))

## v0.1.10 (2026-07-10)

- Claude Code requests using Opus 4.8, Sonnet 5, and Fable 5 model names can
  route through Codex

## v0.1.9 (2026-07-10)

- Claude model aliases use the matching GPT-5.6 tier through Codex: Haiku uses
  Luna, Sonnet uses Terra, and Opus uses Sol.
- GPT-5.6 Codex requests preserve reasoning context and support system guidance
  and tools through the Responses Lite API.
- The dashboard shows requested effort and resolved upstream models, making
  routing decisions easier to inspect.

## v0.1.8 (2026-07-09)

- Codex requests can use `gpt-5.6-sol`, `gpt-5.6-terra`, and `gpt-5.6-luna`,
  including `-fast` variants.
- The default Codex setup uses `gpt-5.6-sol` with `gpt-5.6-luna` as the small
  fast model and a 372K compaction window.

## v0.1.7 (2026-07-06)

- Codex `Read` tool calls get clearer offset guidance and recover from clearly
  invalid large offsets, reducing stalled sessions caused by mistaken
  line-number reads.
- The monitor keeps request lists accurate when a client disconnects or abandons
  a request.

## v0.1.5 (2026-07-03)

- Claude Code's `xhigh` and `max` effort settings now work with Codex and Kimi
  requests instead of being rejected or downgraded unexpectedly.
  ([#20](https://github.com/raine/claude-code-proxy/pull/20))
- Codex receives clearer `Read` tool guidance for line offsets, reducing
  incorrect follow-up reads on large files.
  ([#22](https://github.com/raine/claude-code-proxy/pull/22))

## v0.1.4 (2026-07-01)

- Codex WebSocket streams recover when a pooled continuation connection closes
  before the final response, retrying the turn with full context instead of
  failing the session.

## v0.1.3 (2026-07-01)

- Codex WebSocket streams deliver live text and reasoning progress while reusing
  pooled session continuations to reduce repeated upstream input.
- Codex stream recovery handles retryable startup failures, context-window
  errors, stale continuations, completed tool-call disconnects, stalled `Read`
  arguments, quiet upstream turns, and completed-turn stop reasons.
- Codex gateway requests and tool result translation use accepted payload shapes
  and preserve omitted-block markers for malformed text and image result
  content.

## v0.1.2 (2026-06-30)

- Codex WebSocket continuations recover from streams that only deliver rate
  limit or control events, preventing Claude Code sessions from waiting
  indefinitely on a stalled upstream response.

## v0.1.1 (2026-06-30)

- Codex reasoning summaries are now surfaced as thinking blocks in the response
  stream, so you can see the model's reasoning in your Claude Code session
  when reasoning effort is enabled. Set `codex.reasoningSummary` or
  `CCP_CODEX_REASONING_SUMMARY` to `off` or `none` to suppress summary display
  while keeping reasoning effort active. (Thanks @samot-gc!)
- Codex transport errors (WebSocket connection failures, etc.) now show the
  actual error message instead of a generic "Upstream error", making
  connection issues easier to diagnose.

## v0.1.0 (2026-06-30)

- Ships the native Rust implementation as the release binary.
- Adds the default monitor TUI for `serve`.
- Improves diagnostics with failed-response captures and clearer monitor
  request details.

## v0.0.22 (2026-06-24)

- Codex requests now retry more transient stream and overload failures, making temporary upstream errors less likely to interrupt Claude Code sessions. ([#15](https://github.com/raine/claude-code-proxy/issues/15))
- Codex can now recover stalled `Read` tool calls that previously left Claude Code waiting on incomplete streamed arguments.
- Cursor tool calls are recovered more reliably when Cursor returns XML-style tool use, improving compatibility with Claude Code tools.
- Cursor auth can now be isolated with `CCP_CONFIG_DIR`, so separate proxy configs can keep separate Cursor logins.
- Cursor `composer-2.5` requests now stay in non-fast mode unless fast mode is explicitly requested. ([#17](https://github.com/raine/claude-code-proxy/issues/17), [#18](https://github.com/raine/claude-code-proxy/pull/18))

## v0.0.21 (2026-06-15)

- Forced Codex web search requests now use hosted web search correctly, fixing repeated upstream `Tool choice 'function' not found in 'tools' parameter.` errors. ([#10](https://github.com/raine/claude-code-proxy/issues/10))

## v0.0.20 (2026-06-15)

- Cursor's generic `cursor`, `cursor-agent`, `cursor-plan`, and `cursor-ask` aliases now use Cursor default model selection instead of forcing Composer 2.5 fast mode.

## v0.0.19 (2026-06-14)

- Codex now supports Claude Code hosted web search through Codex's native web search, including domain filters and search usage accounting. ([#10](https://github.com/raine/claude-code-proxy/issues/10))

## v0.0.18 (2026-06-09)

- Cursor sessions now stop heartbeat traffic after streams close, reducing stray connection errors.
- Codex now treats runtime system messages as developer guidance instead of assistant output, preventing Claude Code reminders from being repeated.

## v0.0.17 (2026-06-08)

- Added Cursor Agent as a provider, including login, model selection, ask mode, plan mode, and session continuation.
- Cursor users can select models from the Cursor catalog with `cursor:<model-id>`, `cursor-plan:<model-id>`, and `cursor-ask:<model-id>` aliases.

## v0.0.16 (2026-06-02)

- Codex now uses WebSocket transport by default
- Codex sessions can opt in to append-only continuation with `previous_response_id`, reducing repeated upload size on compatible turns.
- `CCP_TRAFFIC_LOG=1` writes redacted per-request traffic captures to help debug sessions.
- Codex request logging now includes size summaries and image warnings to make compaction and large requests easier to diagnose.
- README guidance for Codex context limits and `[1m]` model suffixes is clearer.

## v0.0.15 (2026-05-30)

- Anthropic requests that omit `stream` now receive JSON responses, fixing Claude Code `/model` validation through the proxy.

## v0.0.14 (2026-05-30)

- Codex streaming now stays responsive during long `Read` tool calls by sending keepalive pings while tool arguments are buffered.
- Truncated Codex streams now return a clear error instead of appearing to finish successfully with incomplete tool calls.
- Stalled Codex requests now time out and retry when response headers never arrive, with clearer diagnostics for slow upstream responses.

## v0.0.13 (2026-05-14)

- Windows users can now download prebuilt `windows-amd64` and `windows-arm64` release archives.

## v0.0.12 (2026-05-12)

- Codex requests can now use `gpt-5.3-codex-spark` as a supported model. ([#14](https://github.com/raine/claude-code-proxy/pull/14))

## v0.0.11 (2026-05-12)

- Claude-style aliases such as `haiku`, `sonnet`, and `opus` now default to Codex while still following the provider already active in the current Claude Code session.
- Mixed Codex and Kimi sessions now keep background alias and token-count requests on the right provider instead of unexpectedly switching providers.
- Tool results with images, errors, or unsupported blocks are handled more safely, reducing malformed upstream requests.

## v0.0.10 (2026-05-06)

- Codex requests can now use `codex.serviceTier` or `CCP_CODEX_SERVICE_TIER` to request a service tier; `fast` is sent upstream as `priority`.
- Codex model names can now include `-fast`, such as `gpt-5.4-fast[1m]`, to request fast mode per request without restarting the proxy.
- Codex's upstream endpoint can now be overridden with `codex.baseUrl` or `CCP_CODEX_BASE_URL`.

## v0.0.9 (2026-05-03)

- Kimi debugging overrides now use `CCP_KIMI_OAUTH_HOST` and `CCP_KIMI_BASE_URL`, matching the proxy's `CCP_` environment variable naming.

## v0.0.8 (2026-04-30)

- Added exponential backoff retry on upstream 429 errors, respecting
  `Retry-After` headers when present
- Added `config.json` as an alternative to environment variables (read from
  `~/.config/claude-code-proxy/config.json` on macOS, XDG-compliant on Linux)
- Made the `originator` and `User-Agent` headers configurable via new env vars
  (`CCP_CODEX_ORIGINATOR`, `CCP_CODEX_USER_AGENT`, `CCP_KIMI_USER_AGENT`,
  `CCP_ORIGINATOR`, `CCP_USER_AGENT`) and the config file
- Codex now sends a default `User-Agent: claude-code-proxy/<version>` header

## v0.0.7 (2026-04-25)

- Some security hardening inspired by [#5](https://github.com/raine/claude-code-proxy/pull/5)

## v0.0.6 (2026-04-25)

- Added support for `gpt-5.5`, and `opus`/`claude-opus-4-7` aliases now map to
  `gpt-5.5` instead of `gpt-5.4`
- Model names with a `[1m]` context suffix (e.g. `gpt-5.4[1m]`) are now
  accepted and stripped before routing, so Claude Code's larger-context model
  variants work without errors
- Documented how to switch between the proxy and direct Anthropic in the README

## v0.0.5 (2026-04-22)

- Added `CCP_CODEX_MODEL` and `CCP_CODEX_EFFORT` environment variables to
  override the model and reasoning effort for Codex requests
  ([#2](https://github.com/raine/claude-code-proxy/pull/2))
- Added `claude-sonnet-4-6` and additional model aliases so more Claude-style
  model names resolve correctly
- Improved request logging with usage summaries, time-to-first-byte metrics, and
  stream completion details for easier debugging
- Client disconnections during streaming are now handled gracefully

## v0.0.4 (2026-04-20)

- Kimi: reasoning content is now preserved across turns as Anthropic thinking
  blocks, so Claude Code sees the model's thinking and multi-turn reasoning
  stays coherent
- Kimi: thinking is always enabled

## v0.0.3 (2026-04-20)

- Renamed to `claude-code-proxy` to reflect multi-provider support
- Added Kimi (kimi.com) as a provider, with device-code login via the install
  script and support for Kimi's chat models
- Requests are now routed to providers based on the requested model, so a single
  proxy can serve both Codex and Kimi models simultaneously
- Improved token counting accuracy and fixed cached token usage reporting
- Added MIT license

## v0.0.2 (2026-04-19)

- Accept Claude-style model aliases (`haiku`, `sonnet`, `opus`, and `claude-*`
  names), resolving them to the appropriate upstream model so portable configs
  and subagents work without edits
- Fix malformed streamed Read tool arguments that Claude Code would reject when
  upstream emitted an empty `pages` field

## v0.0.1 (2026-04-19)

Initial release.
