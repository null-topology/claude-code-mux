## v0.17.1 (2026-11-07)

- Codex token refresh no longer writes over a login it did not start from.
  The proxy now takes `auth.lock` in the directory of `auth.json` while it
  rereads and writes the file, waiting as long as another process holds it,
  and writes only when the file still holds the access token and refresh
  token the refresh was made from. If you log in again, log out or delete
  `auth.json` while a refresh is in flight, the proxy drops its refreshed
  tokens, leaves the file as you left it, and uses the stored token while it
  is still valid; otherwise the request fails and asks you to run
  `codex login`. A file the proxy cannot read, that is not JSON, or that has
  no `tokens` object is left untouched and the refresh fails instead of
  replacing it. The writeback changes only the token fields and
  `last_refresh`, and stores a returned `id_token` only when the Codex CLI
  can read its claims. If the new tokens cannot be written, `auth.lock`
  included, the proxy logs one `codex_token_writeback_failed` warning, serves
  the new token and leaves the file as it was. It keeps those tokens in
  memory and uses them while the file still holds the login they replaced.
  A request writes them again at most once every 30 seconds until a write
  succeeds, also while a failed refresh waits to be tried again. Once they
  are within five minutes of expiring, or have expired, the proxy refreshes
  them with their own refresh token. Once the file holds anything else, they
  are dropped. While
  the file cannot be read, is not JSON or has no `tokens` object, requests
  fail as they would without them, and a write retry that finds such a file
  drops them. If the token endpoint refuses their refresh token, they stay
  in use until their access token expires. A restart forgets them. A
  refresh also finishes, and its tokens are written, when the request that
  started it is cancelled; requests waiting for it use its result, and a
  refresh that falls back to the stored token logs its warning once either
  way.
- `auth.lock` is the Codex CLI's own lock only when `CCP_CODEX_AUTH_FILE` is
  unset or names `$CODEX_HOME/auth.json`; point it at that file. A symlinked
  `auth.json` is now updated at its target, so the link stays a link; a link
  whose target is missing is left alone and never replaced by a file.
- When a refresh in the last five minutes before the token expires fails,
  whatever the reason, the request no longer fails as long as the stored
  token, read again under `auth.lock`, has not expired: the proxy logs one
  `codex_token_refresh_failed_using_stored_token` warning on the `codex`
  logger, with the failure class and HTTP status only, and keeps using that
  token. When that token has expired, or the file holds none or cannot be
  read, the request fails with the refresh error. A refresh that cannot read
  `auth.json`, for instance while another writer is halfway through it,
  reads the file again under `auth.lock`, and uses a token there that is
  not yet in its last five minutes without a warning; otherwise it falls
  back like a failed refresh, and a request that still fails gets the read
  error. After a failed
  refresh, the proxy does not try again for 60 seconds when the failure was
  temporary, and otherwise not until the stored access token changes; it
  uses the stored token meanwhile while it is valid. A refresh that did not
  finish, or whose first read of `auth.json` failed, starts no such wait.
  The forced refresh after the backend rejected a token is always tried and
  never falls back. A token response without `refresh_token`, or with an
  `id_token` or `refresh_token` that is not a string, is accepted; an
  `expires_in` that is not a positive integer counts as absent; the expiry
  is the access token's `exp`, else `expires_in`, else one hour.
- A Codex token refresh that fails for a temporary reason (the token
  endpoint unreachable, a timeout, 429, 5xx, a refresh that did not finish)
  now answers 503 `api_error`, which a client may retry, instead of 401.
  The message is `Codex token refresh is temporarily unavailable`. This
  answer comes only when no valid stored token can be used instead (see
  above); the forced refresh after a 401 never uses one and answers 503
  too. The Codex model listing reports such a failure as unreachable
  rather than as a missing login. Other refresh failures still answer 401,
  without the token endpoint's response body; an error status from the
  token endpoint is named in the message, and 401 and 403 ask you to run
  `codex login`.
- `proxy.log` now keeps at most five rotated files. Past 20 MiB the log is
  still renamed to `proxy.<millis>` in the state dir, and that rotation now
  deletes every `proxy.<millis>` file there except the one it just created
  and the four newest others, so the log just rotated is kept even after the
  system clock went back. Before, rotated files were never removed. There is
  no setting to keep more: copy a rotated log somewhere else if you want to
  keep it.
- Fix: a log field longer than 4000 bytes with a multibyte character across
  the 4000-byte mark no longer panics while being shortened. Such fields
  include upstream error messages, error bodies and the model id of an
  unknown-model warning. The panic dropped the client's connection, and a
  retry hit the same panic. The field
  is now cut at the last whole character, and the `…[N more]` suffix counts
  the bytes left out.
- Fix: an environment variable whose value is not valid Unicode no longer
  makes the proxy panic, at startup or when it reads its configuration.
  - A setting read as text counts as unset with such a value. For `PORT` and
    the `CCP_*` variables other than the path ones below, the setting comes
    from `config.json` or the default, and the startup `config summary`
    warns with the variable's name (never its value). Set the variable to a
    valid value to use it again.
  - Variables that hold a path keep a path that is not valid UTF-8 and apply
    as set: `CCP_CONFIG_DIR`, `XDG_CONFIG_HOME`, `XDG_STATE_HOME`, `HOME` and
    `USERPROFILE` for the config and state directories, and
    `CCP_CODEX_AUTH_FILE` and `CODEX_HOME` for the Codex CLI login file. The
    `config summary` never names them. Stored Kimi, Grok and Cursor logins
    under a config directory that is not valid UTF-8 still use a lossily
    converted path.
  - `HTTP_PROXY`, `HTTPS_PROXY` and `ALL_PROXY` are the exception, as in
    v0.17.0: such a value, in either spelling, is an invalid proxy URL, and
    the proxy stops at startup with, for example,
    `invalid HTTPS_PROXY proxy URL` (the uppercase name, never the value)
    rather than reach the Codex backend around the configured proxy. A valid
    lowercase spelling does not stand in for an uppercase one that is not
    valid Unicode. Set the variable to a valid URL to start the proxy.
- Fix: in a terminal too small for the help (`?`) or setup (`b`) popup, the
  monitor no longer exits with a panic. The popup is now cut to the window.
  Normal sizes look the same as before.
- A Codex `codex.rate_limits` event that reports `limit_reached` with no
  usable credits is now answered as a spent window, the way a
  `usage_limit_reached` error already was, on the live WebSocket and HTTP
  streams and the buffered paths: 429 `rate_limit_error` with
  `x-should-retry: false`, `anthropic-ratelimit-unified-status: rejected`,
  and `-reset` and `-representative-claim` only when exactly one of the
  windows it carries is spent and every one it carries is readable; two
  spent windows name neither. It no longer sends `Retry-After`, which used to carry
  the window's whole countdown (hours on a weekly window) for a client to
  sleep through before retrying; `x-should-retry: false` tells it not to
  retry instead. A snapshot that still has credits keeps streaming as before.
- A spent window after the answer started streaming, as a
  `usage_limit_reached` error or as that snapshot, now ends the stream with
  an SSE `error` of type `rate_limit_error` instead of `api_error`.
- Every spent-window answer that names a window and a reset still ahead now
  records that window as spent until then, and a refusing snapshot updates
  the held reading too, so later responses carry it in their
  `anthropic-ratelimit-unified-5h-*` or `-7d-*` headers. Until the next
  reading those responses can say `unified-status: allowed` beside a
  utilization of 1.0. A spent window that arrives after output on the HTTP
  transport takes its window from the response's `X-Codex-*` headers when
  the event names none.
- Every `codex.rate_limits` reading, healthy or refusing, and every spent
  window now update the held reading one window at a time instead of
  replacing it: a window whose reset has already passed is ignored, and a
  held window that resets more than 60 seconds after the incoming one is
  kept, so a reading that arrives late cannot take a newer window back.
  Within 60 seconds the two count as the same window, so a spent window is
  still recorded when its reset is a second or two off. A window a reading
  does not carry keeps the one held until its reset passes; a reading with
  only one window used to drop the other from the headers.
- Codex `policy_capacity_unavailable` errors (as code or type) are answered
  like `server_is_overloaded`: 529 `overloaded_error` with the native
  message, and `Retry-After` only when the error carries a delay. They used
  to be 502 `api_error`, a server failure.
- Codex `model_not_found` errors (as code or type) that come with no status
  or with 400, 403 or 404 are answered 404 `not_found_error` with the native
  message, or `model_not_found` when it is empty or missing, instead of
  502 `api_error` or a status-based answer. With any other status they keep
  their previous answer.
- A buffered WebSocket request whose `error` event carries a status is now
  answered by the event's error code as well, like an HTTP error body.
- A Codex 401 whose body or `error` event reports a spent usage window or
  names one of the known codes is now answered as that window or code
  without a token refresh, on the live and buffered HTTP and WebSocket paths
  and the standalone search call. Each of them makes one upstream attempt,
  except that under the default `auto` transport a rejected WebSocket
  upgrade still falls back to HTTP, which reads its own answer. A plain 401
  still refreshes the token and sends the request again, at most once per
  client request: under `auto` a live request whose WebSocket attempt
  already refreshed no longer refreshes a second time on the HTTP fallback.
- A rejected WebSocket upgrade and a failed live HTTP start or search call
  now update the held reading with every `codex.rate_limits` window their
  error body carries before the body is turned into a spent window, as the
  buffered paths already did. The refusal itself is answered as before: two
  spent windows still name neither, but later responses now carry both in
  their `anthropic-ratelimit-unified-5h-*` and `-7d-*` headers.
- The held `codex.rate_limits` reading now belongs to the ChatGPT account of
  the request it came with. A reading or spent window under another account
  replaces it instead of merging into it, and a response to a request under
  another account carries none of its `anthropic-ratelimit-unified-5h-*` or
  `-7d-*` headers, so after switching Codex logins the previous account's
  windows are no longer shown. A pooled WebSocket opened under another login
  is no longer reused: the next request opens a new connection, and a
  continuation that depended on the old socket is sent again with its full
  context, as when a pooled socket is gone. A token of the same account
  renewed before it expires keeps the socket.
- A `codex.rate_limits` reading that names a metered limit other than
  `codex` in `metered_limit_name` or `limit_name` no longer updates the held
  reading or records a spent window. A refusing reading of that kind is
  still answered as a spent window.
- On live and buffered HTTP responses and rejected WebSocket upgrades, a
  delay the Codex error body names (`retry_after`, `retry_after_seconds` or
  a `headers` object) now takes precedence over the response's own
  `Retry-After` header. Whichever is used is sent as `Retry-After`: a number
  of seconds rounded up to a whole second, at least 1, and an HTTP date
  unchanged. A buffered 429 without a known code used to
  pass the header on as it came and ignore the body's delay.
- A Codex retry delay is now used only when, with surrounding whitespace
  removed, it is a finite number of seconds or at most 64 bytes of printable
  ASCII, such as an HTTP date. Any other value (empty, longer, non-ASCII,
  with a control character, or a number like `inf`) is ignored wherever it
  is read: stream events, error bodies, and the `Retry-After` header of a
  failed HTTP answer, a rejected WebSocket upgrade or a refused proxy
  tunnel. The next source then gives the delay: the next field the error
  names, then the response's header where there is one, and for `slow_down`
  and `rate_limit_exceeded` the `try again in` delay of the message. Such a
  value used to be passed on as it came, to the client's `Retry-After` and
  to the `auto` fallback log line.
- The body of a failed Codex HTTP answer (live and buffered requests, the
  compaction request and the search call) is now read only up to 64 KiB, as
  a rejected WebSocket upgrade's already was, and the whole read has to end
  within the body idle timeout (5 minutes) instead of each chunk. The error
  code, a spent usage window and the traffic capture are read from those
  64 KiB. A body still arriving at the deadline fails the request with a
  timeout of its own, which is neither refreshed nor sent again; it used to
  hold the request for as long as the body kept coming.
- A `codex.rate_limits` reading with a non-finite, negative or out-of-range
  window length, percentage, reset time or countdown now drops only that
  window; the other window is still published. A reset time or countdown
  that is present but unusable drops its window even when the other one is
  usable. A huge countdown no longer wraps the reset time.
- Codex over WebSocket: writing a request to a peer that stopped reading no
  longer hangs the request or leaves its socket task running. Writing the
  request frame now has a 300 s timeout, and the wait for the response
  starts its own 300 s timeout once the frame is written. When the write
  times out, the client receives the same response-start timeout error as
  when the response never starts, and the request is neither resent on a
  new socket nor replayed over HTTP, even under the default `auto`
  transport. A Pong sent during a response gets the time left of that
  wait. When it runs out, the response ends with that wait's timeout and
  is not resent either. The Ping that checks a
  pooled socket before reuse now counts toward the 15 s check. A check that
  runs out discards the socket, like any other failed check. There is no
  setting for these deadlines and no way back to unbounded writes.

## v0.17.0 (2026-11-05)

- Breaking: `GET /v1/models` now answers in the Anthropic List Models shape
  and carries nothing else. The body has exactly `data`, `first_id`,
  `has_more` and `last_id` (both ids null on an empty page); the top-level
  `object`, the `providers[]` block and the per-row `provider` field are gone.
  Every row the proxy builds is an Anthropic `ModelInfo` with exactly
  `type: "model"`, `id`, `display_name`, `created_at` (always
  `1970-01-01T00:00:00Z`, the API's value for an unknown release date),
  `capabilities`, `deprecated_at: null`, `lifecycle: "active"`, `line`
  (null except on Claude rows from the public catalog), `max_input_tokens`,
  `max_tokens` and `retires_at: null`; Claude rows relayed from Anthropic are
  Anthropic's own (see below). Each id appears once: when two backends list
  the same id, the row of the backend routing sends it to is kept. There is
  no switch back to the old shape. What `providers[]` said about each backend
  (who holds the login, where the rows came from, whether the backend
  answered) is still printed by `claude-code-mux models [--full]`, whose
  output is unchanged except for the Anthropic line, and `?provider=<name>`
  fails with 502 and the reason when that backend has no saved login or
  cannot list.
- Breaking: Codex rows no longer carry the backend's catalog fields
  (`description`, `visibility`, `priority`, `supported_in_api`,
  `use_responses_lite`, `context_window`, `max_context_window`,
  `default_reasoning_level`, `supported_reasoning_levels`,
  `input_modalities`, `service_tiers`, `default_service_tier`). Hidden models
  are still listed. The Codex CLI's own `models_cache.json`, next to its
  `auth.json`, still holds those fields. Routing, the lane table under
  `CCP_CODEX_LANE_POLICY=inventory` and the catalog service tier keep reading
  the listing as before.
- Codex rows report the model's context window as `max_input_tokens`: the
  listing's `max_context_window` (872000 for every current Codex model), else
  its `context_window`, else null. `max_tokens` is null, because the proxy
  sends no output cap to Codex, and `display_name` is the listing's, else the
  id. Claude Code does not read this window from the endpoint, since gateway
  model discovery stays off; a `modelPicker` row still gets its window from
  settings.
- Codex rows carry the full Anthropic `capabilities` tree, and a leaf is true
  only when the Codex translator carries that request feature to the backend:
  the effort levels the model lists; `thinking.supported` when it lists any,
  with `thinking.types.enabled` and `adaptive` false (the request's `thinking`
  field is not read, the effort drives reasoning) and `disabled` and
  `between_tools` true (never rejected); image input as the listing says;
  structured outputs through `output_config.format`; and hosted web search
  with the `web_search_20250305` tool, except on `gpt-5.6-luna` and
  `gpt-6-luna`, whose searches the proxy runs on another model. PDF input,
  batch, citations, code execution and context management are false. Kimi,
  Grok and Cursor rows have `capabilities: null` and the `display_name`
  `"<id> (<provider>)"`; Kimi rows report `max_tokens: 32000`, the cap the
  translator enforces, except `k3` and `kimi-k3`, and every other limit on
  these rows is null.
- `GET /v1/models` and `GET /v1/models/{model_id}` now list Claude models,
  first in the answer. When the request carries a non-empty `x-api-key` or
  `authorization` header, the proxy relays the listing to Anthropic's own
  `GET /v1/models` (at `CCP_ANTHROPIC_BASE_URL`) with the caller's headers as
  sent, minus the ones the messages relay drops (`host`, `content-length`,
  `accept-encoding` and the hop-by-hop headers) and `content-type`, plus
  `anthropic-version: 2023-06-01` only when the request has none. It asks for
  `limit=1000`, follows `has_more` for at most 10 pages, and passes the
  request's `lifecycle` parameters on as written. A relay that stops with
  rows still to come, at the tenth page or on a page that says `has_more`
  without a usable cursor, keeps the rows it has and logs a warning. Those
  rows are Anthropic's, untouched, `capabilities` and `lifecycle` included.
  Because the `lifecycle` pairs go to Anthropic as spelled, on this path a
  cursor resolves over the rows Anthropic returned for that filter, and a
  Get Model with a credential sees the rows Anthropic lists by default. The
  relay follows no redirect, so a 3xx is a failed relay. Without a
  credential, or when the relay fails in any way (an error status such as
  401, a timeout, a body without a `data` array, a failing later page, whose
  earlier rows are then dropped), the rows come from the public Claude model
  catalog, `https://downloads.claude.ai/model-catalog/v1/catalog.json`: the
  models it offers on Anthropic's own API (an `offered_on` that is absent or
  names `first_party`; a null or empty one leaves the model out), with
  `display_name` from the catalog's name, `max_input_tokens` and
  `max_tokens` from its runtime limits, `line` from its model family and
  `capabilities: null`. The catalog is fetched with no credential and no
  client header, follows redirects, and is kept for 10 minutes from a
  successful fetch. Callers that arrive while a fetch of the same URL is in
  flight wait for it and all get its result, a failure included; a failed
  fetch is not kept, so a request after it tries again, and there is no
  stale copy to fall back on. A relay page or a catalog body over 4 MiB is
  a failure. The relay, all its pages together, and each catalog fetch,
  waiting for one in flight included, get 4 seconds. A fetch whose callers
  stopped waiting still runs to its end, and a catalog it reads is kept for
  the requests after it. The catalog's
  signature is not checked: its rows are informational. Routing of
  `claude-*` ids and the aliases does not read any Claude listing, and no
  Claude listing enters the routing catalog.
- New `CCP_ANTHROPIC_CATALOG_URL` fetches the catalog from another URL, such
  as a mirror or a copy served over HTTP on a machine without access to the
  default one. It is environment-only, and an empty value means the default.
  There is no switch that leaves the Claude rows out; `?provider=<name>` for
  another backend asks only that backend and lists no Claude row.
- Breaking: a backend without a saved login is no longer asked and adds no
  rows. Kimi, Grok and Cursor used to list their bundled ids with or without
  a login; they now appear only once that backend has one (`claude-code-mux
  <provider> auth login`), and Codex rows only while the Codex CLI's
  `auth.json` exists. `?provider=<name>` for such a backend answers 502
  `api_error` with `<name> model listing unavailable (no saved login)`
  instead of its rows. Anthropic is asked on every call, though the proxy
  holds no login for it. `claude-code-mux models` still prints every
  backend's list, login or not.
- Breaking: the aggregate `GET /v1/models` now fails with 502 `api_error`,
  naming the backend and its status, when a backend it asks cannot list:
  Anthropic, or a backend the proxy holds a saved login for. It used to answer
  200 and report the failure in `providers[]`; without that block a partial
  answer would read as that backend's models being gone. For Anthropic that
  means no relay succeeded and the catalog could not be fetched, which
  answers `anthropic model listing unavailable (unreachable): public model
  catalog unreachable` (or `answered <status>`, `timed out`, `body is not a
  catalog`, `names no model`, `fetch did not complete`). A proxy that cannot
  reach the catalog
  therefore answers the aggregate list and every `/v1/models/{model_id}` with
  502 unless the request carries a credential Anthropic accepts;
  `?provider=codex` still answers, and `CCP_ANTHROPIC_CATALOG_URL` can point
  at a reachable copy. The backends are asked at the same time, each one as
  soon as its own login check passes; when more than one fails, the first in
  the answer's order names the error, whichever failed first.
- New `GET /v1/models/{model_id}` (Anthropic Get Model) answers with the row
  the list would show for that id, from the same listing pass, request
  headers and 502 rule, or 404 `not_found_error` with the message
  `model: <id>`. With a credential, a Claude id gets Anthropic's relayed row,
  else the catalog's. No lifecycle filter applies. An id containing `/` works
  written literally or as `%2F`.
- `GET /v1/models` takes the Anthropic query parameters. `after_id` and
  `before_id` are exclusive cursors over the answer's own order: both at once
  is a 400, an empty value counts as absent, and a cursor that names no row
  is a 400 naming it. A cursor is looked up among all rows before the
  lifecycle filter, so one naming a row the filter leaves out still works.
  `limit` must be an integer from 1 to 1000, else 400; without it every row
  is returned in one page, unlike the Anthropic API's default of 20.
  `lifecycle` (repeated, as `lifecycle[]`, or comma-separated) accepts
  `active`, `deprecated` and `retired` and answers anything else with a 400.
  It filters each row by its own `lifecycle`, a row without one or with any
  other value counting as active; without the parameter, active and
  deprecated rows are listed and
  retired ones left out. Every row the proxy builds is active, so only
  relayed Anthropic rows can be anything else. `?provider=<name>` stays as
  the proxy's own filter, with a 400 for an unknown name. Other parameters
  are ignored. Request headers such as `anthropic-version`,
  `anthropic-beta` and `anthropic-workspace-id` matter only to the relay,
  which passes them on; without a credential they are ignored. Every error,
  including a malformed `?limit=abc` that used to get a plain-text 400, now
  uses the Anthropic envelope `{"type": "error", "error": {"type": ...,
  "message": ...}}`.
- Every response of `/v1/models` and `/v1/models/{model_id}`, errors
  included, carries a `request-id` header.
- Ids containing `claude` or `anthropic` are no longer left out of the
  listing; the earlier rule that dropped them is gone. Claude Code's gateway
  model discovery (`CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1`) keeps only
  such ids, so a client that turns it on now gets this listing's Claude rows
  as picker rows. `modelPicker` rows are not affected. The Codex listing feeds
  routing as before.
- `proxy.log` has four new events on the `anthropic` logger: a warning
  `anthropic model listing relay failed; using the public catalog`, a
  warning `anthropic model listing truncated` with the number of `pages`
  read, an info `anthropic model catalog fetched` with the catalog's
  `version` and the number of `models` kept, and a warning `anthropic model
  catalog fetch failed`. The two failure warnings carry `status` (0 when
  none came) and a short `class` (`status`, `timeout`, `network`, `body`,
  also for a body over 4 MiB, `empty` for a catalog that names no model,
  and `task` for a catalog fetch that ended without a result, such as one
  cut short by shutdown). None of them logs a header, a body or a query.
- `claude-code-mux models` now prints the catalog's Claude ids for
  Anthropic, as `anthropic: <ids> [public catalog, not verified]`, or
  `anthropic: unavailable (unreachable: <reason>)` when the catalog cannot be
  fetched, instead of a line saying credentials are forwarded from the
  client. The command now reaches the catalog URL to do so.

## v0.16.0 (2026-10-23)

- New `config.json` key `codex.serviceTierByModel` sets the Codex service tier
  per model: an object from model id to tier, with the `serviceTier` values
  (`fast`, `priority`, `flex`, `default`). The key must match the id that goes
  to Codex exactly (after `codex.model` and the web search lane upgrade,
  without `-fast` or `[1m]`); there are no wildcards and no environment
  variable for it. A request now takes the first of
  `CCP_CODEX_SERVICE_TIER`, the model's entry, `codex.serviceTier`, a `-fast`
  suffix and the listing's default, so an entry overrides `serviceTier` and
  the suffix for its model, and `default` there sends no tier for it. Like
  `serviceTier`, the map applies to Anthropic Messages requests routed to
  Codex: `/v1/messages`, the `count_tokens` check and Claude Code's
  auto-review classifier, whose entry is keyed by its model (`gpt-6-luna` by
  default); the OpenAI-compatible `/v1/responses` and `/v1/chat/completions`
  surfaces take neither. A `null` entry counts as absent, and so does a `null`
  map. An entry that is not a valid tier name answers that model's requests
  with a 400 naming the entry, such as `Invalid service tier in
  codex.serviceTierByModel["gpt-6-astra"]: "priorty". Must be one of: ...`.
  A value that is not an object is ignored and logged at startup as a
  `config summary` warning:
  `codex.serviceTierByModel: invalid value; expected an object; ignored`.
  Without the key nothing changes.
- `codex_upstream_request_started` in `proxy.log` can now carry
  `serviceTierSource: "config_model"`, for a tier taken from
  `codex.serviceTierByModel`. `config` still means `CCP_CODEX_SERVICE_TIER` or
  `codex.serviceTier`. A log reader that matches the known sources needs the
  new value.
- The proxy now keeps the parsed `config.json` in memory for 30 seconds
  instead of reading the file again on every request. An edit to the file
  reaches the settings read per request within 30 seconds rather than on the
  next one; a missing file stays missing for up to 30 seconds after it is
  created. A file that cannot be read or does not parse, such as one caught
  mid-write, is not kept: only the request that saw it runs without the file.
  There is no setting to turn this off; to apply an edit at once, restart the
  proxy.
- `serve` now writes the config override summary to `proxy.log` at startup,
  one `config summary` event per line with the line in its `line` field. Each
  line names a setting and its source (`port (env)`, `port (config)`), never
  the value it carried. A source whose value was unusable and skipped, such as
  `codex.lanePolicy: invalid value; expected full|inventory; ignored`, is a
  warning, which also reaches stderr when the monitor is off; every other line
  is info. Until now this summary was not emitted anywhere, although an
  unusable `codex.lanePolicy` or lane env value was documented as reported at
  startup.

## v0.15.0 (2026-10-07)

- A request that belongs to a Claude Code message thread (a body with a
  `thread` field, `"type": "create"` or `"continue"`, under the
  `message-threads-2026-08-12` beta) is now refused with a 400 when its model
  goes to Codex, Kimi, Grok or Cursor, instead of being forwarded. A thread
  lives on Anthropic's side: a continuation carries only the messages after
  the one it names, so a translated route would answer from part of the
  conversation (the upstream report describes Codex failing on a
  continuation that answers a tool call, and a plain message losing its
  context), and a thread created on another backend would be anchored on a
  message id Anthropic never issued. The refusal's message starts with
  `capability_rejected: beta_header:message-threads-2026-08-12`, on which
  Claude Code turns threading off for the rest of the session and sends the
  full conversation from then on. That costs one extra round trip in a
  session that threads. The proxy also never reroutes a threaded request away
  from Anthropic: the auto-review override (`CCP_AUTO_REVIEW_MODEL`) and the
  `upstream` progress-label route leave it on the model the client named, so
  a side request cannot turn threading off for the main conversation. The
  Anthropic route keeps threading and relays these requests unchanged. Each
  refusal is logged as `message_thread_rejected` with the provider and model.
  There is no setting to forward threaded requests again. Reported in
  raine/claude-code-proxy#148.
- A Codex answer stopped by the backend's content filter now reaches Claude
  Code with `stop_reason: "refusal"` instead of `"max_tokens"`. Until now
  every response that ended incomplete was reported as running out of output,
  so Claude Code took a filtered answer for one cut off by the output limit.
  Now it applies its own handling for a content refusal. A response that ended
  incomplete for any other reason, or without one, still reports
  `max_tokens`. This covers streaming and non-streaming requests on every
  Codex transport. On the non-streaming and buffered paths, a response that
  ended incomplete in the middle of an output item now reaches the client
  with the text it had so far and its stop reason, instead of a 502. The
  proxy still makes one attempt and leaves any retry to the client. The
  OpenAI-compatible routes are unchanged. There is no setting to get the old
  stop reason back.
- The proxy now reads Codex `Retry-After` from the `headers` of a stream
  error, rounds the value of a stream error, a rejected WebSocket upgrade and a
  refused WebSocket proxy tunnel up to whole seconds like every classified
  error, and recognises a spent usage window in a rejected upgrade's body:
  - a `response.failed` or `error` stream event that carries `Retry-After` in
    its error's `headers` now passes it on. It wins over a "try again in"
    delay the message names, while a `retry_after` field or the event's own
    `headers` still win over it;
  - a `Retry-After` number from a stream error without a recognised code, a
    rejected upgrade or a refused proxy tunnel is now rounded up to whole
    seconds, at least 1, as classified errors already were; an HTTP date
    passes as it came. Until now a fractional or zero value went through
    unchanged;
  - a rejected upgrade whose body reports a spent usage window
    (`usage_limit_reached`) is answered like a spent window on every other
    transport: a 429 with `x-should-retry: false`,
    `anthropic-ratelimit-unified-status: rejected`, the reset time and the
    exhausted window when the body or the `X-Codex-*` headers name them, and
    no `Retry-After`. Until now it was a plain 429 that passed on any
    `Retry-After` the upgrade carried. Under `auto` transport such a rejection
    still falls back to HTTP first;
  - a rejected upgrade's body is read up to 64 KiB, up from 1 KiB, so a
    longer error body is still recognised; the error detail stays at 1 KiB.

  There is no setting to get the previous behaviour back.
- The Codex transport now defaults to `auto` instead of `websocket`, adapted
  from raine/claude-code-proxy#173. Requests still go over a WebSocket first.
  When the WebSocket cannot be set up before the request is sent, for example
  because the handshake is refused or answered with a 429, the request goes
  over HTTP instead of failing. A request already sent over the WebSocket is
  never sent again over HTTP, so a connection that drops in the middle of an
  answer still reports an error. Each fallback is logged to `proxy.log` as a
  `codex_websocket_fallback_to_http` warning with the handshake status, so
  you can see how often the WebSocket is refused. An unrecognised transport
  value now falls back to `auto` as well, and the `transport` field of the
  Codex request events in `proxy.log` says `auto`. A transport you have
  already set is unchanged. `CCP_CODEX_TRANSPORT=websocket` (or `"transport": "websocket"`
  under `codex` in `config.json`) brings back the WebSocket-only behaviour,
  in which a refused handshake reaches Claude Code as an error, except for
  compaction, which always goes over HTTP (see the next entry).
- Claude Code's compaction summary request now always goes to Codex over
  HTTP, whatever `CCP_CODEX_TRANSPORT` says, adapted from
  raine/claude-code-proxy#168. The summary request is large, and over a
  WebSocket its connection can close before the answer is complete. With
  `CCP_CODEX_SERVER_COMPACTION` on, the proxy's own compaction call goes over
  HTTP too. Every other request keeps the configured transport, including the
  requests after a compaction. The summary request is now also recognised
  when Claude Code sends system reminders after the summary prompt: the
  prompt counts when it sits in a user message among the last eight messages
  and every message after it is a user message holding only
  `<system-reminder>` text, or a system message holding only text. A prompt
  followed by anything else, such as an answer, a tool result or other text,
  is not a summary request, so an ordinary turn that quotes the prompt is
  left alone. A recognised request
  gets the compaction effort cap (`CCP_COMPACT_EFFORT`) as well, and server
  compaction when `CCP_CODEX_SERVER_COMPACTION` is on; server compaction now
  removes the summary prompt from the message that holds it rather than from
  the last message, so the prompt stays out of the stored history when
  reminders follow it. The HTTP summary request sends the full context and
  leaves no WebSocket continuation state, so, with
  `CCP_CODEX_PREVIOUS_RESPONSE_ID` on, the next request also sends its full
  context without `previous_response_id`. The `transport` field of the
  summary's Codex request events in `proxy.log` says `http`. There is no
  setting to send the summary over the WebSocket again.
- Codex requests now take the `service_tier` body field from the model's
  `default_service_tier` in the Codex model listing the proxy already
  fetches, as the Codex CLI fills that field, but only when the default is
  `priority` or `flex` and the model's own `service_tiers` lists it. Any
  other value, `fast`, `ultrafast` and `default` included, sends no tier and
  is not mapped to one. This covers the `service_tier` field only: the proxy
  sends no `x-codex-routing-hint` header. A model whose listing names no
  default gets no tier, as before; when this was released, no model's
  listing named one. When a listing names a default for a model, requests to
  that model carry it from that successful listing on. A model the proxy has
  not seen in a successful listing, such as one known only from the
  compiled-in list or any model before the first listing after a start, gets
  no default.
- Claude Code's auto-review classifier, when it is sent to Codex, never takes
  the catalog default; a configured tier or a `-fast` id still applies to it
  as before.
- The order is: `CCP_CODEX_SERVICE_TIER` or `codex.serviceTier`, then a
  `-fast` model id, then the listing's default. To send no tier whatever the
  listing says, set `CCP_CODEX_SERVICE_TIER=default` or
  `"codex": {"serviceTier": "default"}`; like any configured tier, it also
  overrides a `-fast` id. To pin a tier instead, set `fast`, `priority` or
  `flex` as before.
- Codex rows in `/v1/models` now include the listing's `service_tiers` and
  `default_service_tier` when they are set; empty or unset fields are
  omitted.
- The `codex_upstream_request_started` event in `proxy.log` now records
  `serviceTier` (the tier sent, or null) and `serviceTierSource` (`config`,
  `suffix`, `catalog`, or null when none applies). A configured `default`
  records `serviceTier: null` with `serviceTierSource: "config"`.

## v0.14.1 (2026-09-30)

- The monitor's aggregate cache hit % (the `Hit` column of sessions and
  conversations, the Stats tab and the session detail) now leaves out
  requests that carry no history: a request whose body holds no assistant
  turn, such as the first request of a session or a subagent, a one-shot side
  request or the first turn after a compaction. Until now a cold first request
  lowered the ratio for the rest of the session: with two prompts of the same
  size, a cold first request followed by one that read 99% from cache showed
  49.5%, and now shows 99%. A session resumed later
  with a cold cache still carries its history, so that cold request still
  counts against the ratio. The tokens of the requests left out stay in every
  other total, the `Hit` cell of a single request still shows its own ratio,
  and a row made only of such requests shows `n/a`. There is no setting to
  get the old ratio back.

## v0.14.0 (2026-09-28)

- Codex errors that carry a known error code now reach Claude Code with a
  status that says whether trying again can help, so Claude Code no longer
  retries a request the backend will refuse again. Until now most of them
  arrived as a 502 server error or a plain 429. Now:
  - a spent credit balance or spend limit (`insufficient_quota`,
    `credit_balance_exhausted`, `organization_spend_limit_exceeded`,
    `project_spend_limit_exceeded`, `organization_usage_limit_exceeded`) is a
    429 with `x-should-retry: false` and the backend's message. Unlike a spent
    usage window it names no reset time, because the backend gives none;
  - `usage_not_included`, a plan without Codex access, is a 403;
  - a prompt the backend refuses (`invalid_prompt`, `cyber_policy`,
    `bio_policy`, `misalignment_policy_violation`) is a 400 with the backend's
    message;
  - `context_length_exceeded` is the same 413 that a context window message
    already produced;
  - `slow_down` and `rate_limit_exceeded` are a 429 that may be retried, with
    `Retry-After` taken from the error or from the "try again in" delay its
    message names;
  - `server_is_overloaded` is a 529, with `Retry-After` only when the error
    carries one.

  As with every error, a backend message that names the backend or its
  transport is replaced with a generic one. A `Retry-After` given in seconds
  is rounded up to whole seconds, at least 1: a delay of `0.25` or `0` is sent
  as `1`, and a date is passed on as it came. With opt-in server compaction
  (`CCP_CODEX_SERVER_COMPACTION`), a compaction refused for spent credits or
  `usage_not_included` is answered with that error and the normal request is
  not sent, as a spent usage window already was.

  An error that arrives after the answer has started streaming keeps the
  status already sent and now carries the matching error type. A code the
  proxy does not know is handled as before, and so is a spent usage window
  (`usage_limit_reached`). There is no setting to get the old answers back.

- The Codex defaults move to the gpt-6 models, following the same change in
  raine/claude-code-proxy#165:
  - Claude Code's security classifier, when it goes to Codex, now runs on
    `gpt-6-luna` instead of `gpt-5.6-luna`. `CCP_AUTO_REVIEW_MODEL=gpt-5.6-luna`
    brings the old model back, but a set value applies to the classifier on
    every route, not only on Codex; there is no Codex-only way back;
  - in the `upstream` progress-label mode the Codex junior model is now
    `gpt-6-luna`. `CCP_AGENT_SUMMARY_MODEL=gpt-5.6-luna` brings the old model
    back, and it too applies to labels on every route;
  - with `CCP_ALIAS_PROVIDER=codex`, `haiku` and the Claude Haiku ids resolve
    to `gpt-6-luna`, and `opus`, `fable`, the Claude Opus and Fable ids and the
    new `claude-opus-5-5` to `gpt-6-sol`; Sonnet stays on `gpt-5.6-terra`.
    There is no setting for the old mapping; name the Codex model directly
    instead;
  - `gpt-6-luna` and `gpt-6-sol` are known before the first model listing
    arrives, sit in the built-in Lite table that the `inventory` lane policy
    falls back on, and a hosted web search on `gpt-6-luna` is sent as
    `gpt-6-sol`, as one on `gpt-5.6-luna` already goes to `gpt-5.6-sol`;
  - the monitor's setup hint names `gpt-6-sol` and `gpt-6-luna`;
  - the model listing call sends `client_version` 0.157.1 when none of
    `CCP_CODEX_CLIENT_VERSION`, `codex.clientVersion` in `config.json` and the
    Codex CLI's `models_cache.json` names one. Set either of the first two to
    send a different version.

- Codex requests now name their conversation the way the Codex CLI does. The
  `session_id` header is replaced by `session-id` and `thread-id`, both with
  the value `session_id` had (the session id for a main thread, the derived
  id for a subagent), on HTTP and on the WebSocket handshake alike. Within a
  turn the proxy also sends back the first `x-codex-turn-state` value the
  backend returned in that turn. Over HTTP it goes out as a request header
  and the request body is unchanged. Over WebSocket, `response.create`
  messages gain a `client_metadata` object holding the `x-codex-turn-state`
  key when a value is stored; the handshake carries none. The value is taken
  from the HTTP response header or from a stream event of type
  `response.metadata` or `codex.response.metadata`, never from another event
  type or the WebSocket handshake response. A request continues the turn
  when its last user message holds a tool result and otherwise only tool
  results and reminder text; any other request, a prompt typed after an
  interrupt included, starts a new one and sends no value. A request without
  a Claude Code session header, a title request and a subagent progress
  label take no part. The store is process-local: it lives in memory, a
  restart clears it, and a conversation's value is dropped after 30 minutes
  with no activity on it. `prompt_cache_key`, `x-client-request-id` and
  `x-codex-window-id` are unchanged. `codex_upstream_request_started` in
  `proxy.log` gains `newTurn` and `turnStateSent`, never the value itself;
  opt-in traffic captures (`CCP_TRAFFIC_LOG=1`) can contain it. There is no
  setting for this: to send the old `session_id` header and no turn state,
  stay on 0.13.0.

## v0.13.0 (2026-09-27)

- The monitor's Stats tab no longer counts token count requests. The local
  Codex estimate used to add a `codex/-` row of requests with every token
  column `n/a`, and Anthropic's relayed `count_tokens` inflated `Reqs` (and
  could feed `Fail` and the latency median) of the Claude model it named.
  These requests still show in the Sessions pane, the recent list and the
  events as before.
- The monitor now forgets idle sessions. A session with nothing in flight and
  no request for an hour is hidden from the Sessions pane, whose title says
  how many are hidden (`Sessions (idle hidden: N)`), and its next request
  brings it back with its history. After a day without a request it is dropped
  from memory, so a long-running proxy no longer keeps every request it ever
  served; a later request under the same session id starts a new session from
  zero, which gets its transcript name back. The header's session count covers
  the visible sessions only. The Stats
  tab still covers every request since the proxy started, dropped sessions
  included. There is no setting for either period and no switch back to
  keeping every session.
- Sessions in the monitor have names. The Sessions pane's `Project` column is
  now `Session` and shows the name given with `/rename`, else the title Claude
  Code generated for the session, both read from its transcript under
  `$CLAUDE_CONFIG_DIR/projects` (default `~/.claude/projects`), else the title
  seen in Claude Code's title request on the Anthropic route, else the project
  (as `project · worktree` when the session runs in a git worktree), else the
  session id. Transcripts are read in the background every few seconds, from
  where the previous read stopped, and never modified; names stay in memory
  and are not written to `proxy.log` (a traffic capture holds the title reply
  as it holds any response). The session detail
  says where the name came from and shows the worktree beside the project. A
  checkout under `.claude/worktrees/<name>` on another machine used to show
  the worktree name as its project; it now shows the repository, and a local
  git worktree shows its worktree name as well.
- Side calls are labelled by kind. Conversations that used to end in `/side`
  now end in `/classifier` (the auto-mode classifier), `/title` (the session
  title), `/search` (the isolated web search), `/recap` (the recap of a
  subagent left running) or `/fetch` (a WebFetch summary), and in `/side` for
  any other request without client tools. They are placed and counted exactly
  as before: under the conversation that made them and never judged for cache
  misses.

## v0.12.0 (2026-09-23)

- Client headers can now reach the Codex backend. `CCP_CODEX_FORWARD_HEADERS`
  (config key `codex.forwardHeaders`) names the headers to pass on, for
  example `x-gateway-token` set through Claude Code's
  `ANTHROPIC_CUSTOM_HEADERS`, so one gateway credential works on the Claude
  and the Codex route alike. Until now the Codex route built its own headers
  and dropped every client header. A named header goes on the WebSocket
  handshake, HTTP requests, web search, and the model listing a client
  request triggers (an unknown model, `/v1/models`); the listing at start
  has no client request and goes without it, and the OpenAI-compatible
  routes forward nothing. A pooled WebSocket keeps the values it was opened
  with. The proxy's own headers are never replaced, the client's connection
  headers (`host`, `content-length`, hop-by-hop) are never forwarded, and
  forwarded values are redacted in traffic captures. Nothing is forwarded
  unless a name is listed.
- The Codex HTTP client no longer follows redirects, as the WebSocket and
  native Responses clients already did, so a forwarded header cannot reach
  another host. A redirect now reaches the client as that status.

## v0.11.0 (2026-09-23)

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
