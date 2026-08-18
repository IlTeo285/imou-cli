# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A Rust CLI (`imou-cli`) for controlling Imou/Dahua Easy4ip cameras through the
Imou Open Platform HTTP API. Credentials live in `.env` (gitignored) and are
loaded at startup with `dotenvy`.

Reference docs (fetch these when adding new endpoints — the site has no
sitemap, so guess method-doc paths from the pattern
`device/<category>/<method>.html` under `http/`, e.g.
`http/device/live/bindDeviceLive.html`):
- Platform overview: https://open.imoulife.com/book/start.html
- Auth/signing example: https://open.imoulife.com/book/http/apiExample.html
- Request/response envelope spec: https://open.imoulife.com/book/http/develop.html

## Commands

```
cargo build              # build
cargo run -- <args>      # e.g. cargo run -- devices
cargo clippy --all-targets   # lint — keep this clean before committing
```

There's no local mock of the Imou API, so the practical way to verify a
change to an `api/*` module is to build and run the relevant subcommand
against the real account in `.env` (see Architecture below for why the
response shapes must be verified against the live API rather than trusted
from the docs). A few pure-logic unit tests do exist (`cargo test`) — e.g.
`listen::tests` covers timestamp parsing against a real captured payload —
for the parts that don't need a live account to verify.

## Architecture

**Request flow**: `main.rs` (clap CLI) → `api::{devices,live,ptz,alarm}` (typed
params/response structs + endpoint method names) → `client::ImouClient`
(builds the `{system, id, params}` envelope, signs it, dispatches the HTTP
call, unwraps `result.data` or maps non-`"0"` codes to `ImouError::Api`) →
`config::Config` / `signing` / `token_cache` (supporting pieces). `watch.rs`
sits above `api::alarm` and `event_log::EventLog` for the polling-based
motion-event mode (see below).

**Envelope and signing, uniform across every endpoint** (see `signing.rs`,
`client.rs::dispatch`): every call — including `accessToken` itself — is
POSTed as `{"system": {ver, appId, sign, time, nonce}, "id", "params"}`. The
signature is `md5("time:{time},nonce:{nonce},appSecret:{appSecret}")` and
does **not** cover `params`, so a fresh `time`/`nonce` pair must be generated
per request even when replaying identical params. Endpoint method names in
the URL path are the bare method name (`bindDeviceLive`, `deviceBaseList`,
`controlMovePTZ`, ...) — not the nested path the doc site's URL suggests
(`device/live/bindDeviceLive.html`); double-check the real endpoint against
the "Request Method & URL" section of each doc page, not its own URL.

**Two call shapes on `ImouClient`**, because some endpoints (PTZ control)
return `result.code` with no `result.data` at all:
- `call::<P, R>` — authenticated, expects a `data` payload, used by most reads.
- `call_action::<P>` — authenticated, ignores `data`, used for commands like
  PTZ move where only success/failure matters.
- `call_unauthenticated::<P, R>` — no token merged into params; used only for
  `accessToken` itself.

**HTTP client has an explicit 20s timeout** (`client.rs::ImouClient::new`) —
`reqwest::Client::new()`'s default has none. Found live: `watch`'s poll loop
is a plain sequential `for` over channels awaiting each `poll_channel` call
directly, so one stalled connection (no server response, half-open TCP) hung
the entire loop forever — the process stayed alive but silently stopped
making progress, no crash/error to signal it. Any future long-lived loop
built the same way (await a network call per iteration, no per-call timeout)
has the identical failure mode; don't remove this timeout without addressing
that structurally.

**Token handling** (`client.rs::get_access_token`, `token_cache.rs`): access
tokens last ~3 days per the docs, so they're persisted to
`.imou_token_cache.json` (gitignored) between CLI invocations rather than
re-fetched every run. `call`/`call_action` merge `token` into the *params*
object (confirmed against `deviceBaseList.html`'s example, not just assumed)
— it does not go in `system` alongside the signature.

**Data centers** (`config.rs::DataCenter`): an app's `appId`/`appSecret` pair
is only valid against the data center it was registered in
(`sg`/`fk`/`or` → Singapore/Frankfurt/Oregon base URLs). `DATA_CENTER` is
required in `.env`; there is no default, because guessing wrong doesn't fail
loudly at signing time, only once a call actually reaches the wrong region's
API. The current `.env` is set to `or`, which is verified working with the
live account it was set up for.

**Response field trustworthiness**: the doc site's example JSON has been
wrong/incomplete more than once during development (e.g. `deviceBaseList`
required an undocumented-as-mandatory `needApInfo` field; `deviceOnline`'s
status field is `onLine`, not `status`; `getAlarmMessage`'s `nextAlarmId` is
absent from the response entirely when `alarms` is empty, not just `0`/`-1`;
`getAlarmMessage`'s documented `type` code table — `0`=human infrared,
`1`=motion, etc. — does not match reality, every alarm observed live came
back as `type: "34500"`, and the actually-useful field is the undocumented
`labelType`, e.g. `"humanAlarm"`). Treat doc examples as a starting point,
not ground truth — when adding a new `api/*` endpoint, run it against the
real account and adjust the struct fields to match the actual response
before trusting it.

**`watch` (motion polling)**: `watch.rs` polls `api::alarm::get_alarm_message`
per device/channel on an interval and appends a JSON line to an events file
via `event_log::EventLog` for every alarm returned (see the `type`/`labelType`
note above — there's no reliable code to filter on, so everything
`getAlarmMessage` returns for a channel is treated as a motion/security
event by construction of the endpoint, and the raw `alarm_type`/`label_type`
are passed through in the output for future refinement). Each channel has a
`ChannelCursor` (`begin_time` + `last_alarm_id` high-water mark) so restarts
of `watch` start from "now", not from account history, and so alarms
straddling a poll boundary (same-second granularity on `beginTime`/`endTime`)
aren't double-logged. Motion events also print to stdout (`motion detected:
device=... channel=... (...)`), not just to the events file.

**Critical, undocumented gotcha — `getAlarmMessage` filters on local time,
not UTC**: an `Alarm`'s `time` field and the request's `beginTime`/`endTime`
are the account's local wall-clock time, encoded as a Unix epoch *as if it
were UTC* (confirmed live: `time`'s digits match `localDate` exactly, and
differ from the genuinely-UTC `utcTime` field by the account's UTC offset —
observed 2 hours, i.e. CEST/W. Europe). `watch.rs` therefore builds its query
windows with `chrono::Local`, not `Utc` — using `Utc::now()` there silently
drops every event less than the account's UTC offset old, because the
window's upper bound trails the account's real "now" by that offset. This
assumes the machine's local timezone matches the account's configured one;
if `watch` is ever run somewhere that doesn't hold, this breaks again in the
same silent way. `MotionEvent.time` in the output uses `utc_time` (genuinely
UTC) for correctness; `local_time` is `time` formatted naively for human
reading.

**Local RTSP recording (`recorder.rs`)**: verified live that these cameras
(Dahua/Imou `IPC-K7C` family) expose standard RTSP directly on the LAN —
`rtsp://admin:<password>@<ip>:554/cam/realmonitor?channel=1&subtype=0` with
Digest auth — completely bypassing the Open Platform cloud. Per-channel local
credentials live in `.env` as `CAM_<NAME>_IP` / `CAM_<NAME>_SECURE` (`<NAME>`
= `channel_name` upper-cased, e.g. `CAM_INGRESSO_IP`); `recorder::local_config_for`
reads them, and a channel without both is simply not recorded (motion still
gets logged as before). The RTSP username is hardcoded `admin` — verified
live for this device family, not configurable. **External dependency**:
`ffmpeg` must be on `PATH`; `watch::run` checks this upfront (only if at
least one channel has local config) rather than failing on first trigger.

Design (ring buffer + concat, not a real-time circular buffer in memory):
- `recorder::start_ring_buffer` spawns one long-running, auto-restarting
  `ffmpeg ... -f segment -segment_time 2 -strftime 1 seg_%Y%m%dT%H%M%S.ts`
  per channel (`-c copy`, stream copy only — negligible CPU even at the
  cameras' native HD/H.265 resolution) into `<buffer_dir>/<channel_name>/`,
  plus a periodic sweep deleting segments older than `retention`.
- `retention` is **not** just the pre-roll — it's computed in `watch.rs` as
  `pre_roll + interval + margin`, because `watch` only discovers an alarm via
  polling, up to a full `--interval-secs` after it actually happened. A
  buffer that only kept the raw pre-roll window could have already evicted
  the footage by the time an alarm is discovered.
- `recorder::extract_clip` (spawned per alarm from `watch.rs`'s poll loop,
  not blocking it) computes `[alarm.utc_time - pre_roll, alarm.utc_time +
  post_roll]`, **converted to local time** to compare against segment
  filenames — ffmpeg's `-strftime 1` timestamps segments using the process's
  *local* time, so this mirrors the same local-vs-UTC handling already
  documented above for `getAlarmMessage` windows, for the same reason. It
  sleeps until the window has fully landed on disk, selects overlapping
  segments, and concatenates them (`-f concat -c copy`, lossless) into
  `<clips_dir>/<channel_name>/<local-time>_<alarm_id>.mp4`.
- Verified live end-to-end (real ~490KB/segment H.265 2560×1440 footage,
  concat producing a valid playable multi-segment clip) — see git history /
  session notes for the exact test; not kept as an automated test since
  there's no fixture buffer checked into the repo.
- Shutdown: `watch::run` sends a `tokio::sync::watch` shutdown signal and
  awaits the recorder tasks (which kill their ffmpeg child) with a timeout,
  so no orphaned ffmpeg processes survive Ctrl+C. In-flight clip-extraction
  tasks that haven't reached their post-roll window yet are simply dropped
  on exit — no partial-clip handling, documented as intentional.

**Push/webhook callbacks (`setMessageCallback`) — works, but not what the
docs describe, and not currently wired into the CLI**: earlier testing in
this project (with an ngrok URL and a few throwaway domains) got `OP1003`
rejections and led to a wrong conclusion that only mainland-China domains
are accepted. Re-tested later against a real, legitimately-owned domain
fronted by Cloudflare (not China-hosted) and it eventually worked — so the
rejection is **not** a China/ICP domain allowlist. What actually seems to
matter, unconfirmed: some combination of domain reputation/age and needing
the registered `callbackUrl` to already be live and responding before
`setMessageCallback` is called (ngrok/throwaway domains in the earlier test
weren't actually serving anything yet at registration time). Toy/test
domains and tunel-of-the-moment URLs (ngrok, freshly-spun-up subdomains)
should still be expected to fail; a real, already-serving domain is the
one combination confirmed to work.

Two more corrections once it did work, live:
- End-to-end latency from `setMessageCallback` registration to the first
  real delivered event was **~48 minutes**, not the "~5 minutes" the docs
  state for config changes to take effect. Don't assume push is broken from
  a short test window — the account also received a couple of no-op
  verification pings (empty `{}` body, `User-Agent: Java/...`) well before
  any real event arrived, which is a reasonable "endpoint is reachable"
  signal while waiting.
- The real payload doesn't match `push/event.html`'s documented shape at
  all: `msgType` is `"iotEvent"`, not `"videoMotion"`; there's no `cid`; the
  alarm subtype lives in `content.event` (matches `Alarm.alarm_type` from
  `getAlarmMessage`, e.g. `"34500"`); device name is `dname`, not `cname`;
  timestamps are `"yyyyMMddTHHmmss"` strings (`time`, and a separate,
  genuinely-UTC `utcTime`), not the unix-epoch numbers docs show; there's
  also `thumbUrl`/`picUrlArr`/`userId`/`pid`. Any future webhook receiver
  must be built against this real shape, not the doc's example.

`watch` uses polling (`api::alarm::get_alarm_message`) — simple, no public
reachability requirement, predictable latency. `listen.rs` is the push-based
alternative, now implemented, for when a public HTTPS endpoint is available
and near-real-time delivery (once warmed up) matters more than predictable
latency.

**Shared logic between `watch` and `listen`** (`motion_event.rs`,
`recorder::start_all`/`shutdown_all`/`spawn_clip_extraction`): both modes
ultimately produce an `api::alarm::Alarm` (one from polling
`getAlarmMessage`, one synthesized from a push payload in `listen.rs`) and
do the exact same three things with it — log a `MotionEvent`
(`motion_event::MotionEvent::from_alarm`), start/stop the per-channel
ring-buffer recorders (`recorder::{start_all,shutdown_all}`), and spawn clip
extraction (`recorder::spawn_clip_extraction`). Keep it that way — if the
two modes' handling of an `Alarm` ever needs to diverge, that's a sign the
shared code needs a parameter, not a fork.

**`listen` (push-based, `listen.rs`)**: serves `POST /imou-callback` with
axum, always returning 200 (Imou disables the callback after repeated
non-200s). **Does not call `setMessageCallback`** — that used to be done
automatically on every startup (`api::push::set_callback`, now deleted) but
was found to reset the account's "IoT Device Message" push subscription
(the one real motion events, `msgType: "iotEvent"`, actually depend on) —
`setMessageCallback`'s documented `callbackFlag` values (`alarm`,
`deviceStatus`, `numberstat`, `faceAnalysis`) don't even include an
IoT-specific flag, so that subscription lives outside this API entirely,
most likely as a toggle in the Imou Open Platform console itself. The
callback URL must now be registered **manually via the Imou console** —
`listen` just assumes it's already pointed at `--callback-url` and prints a
reminder of that at startup. `PushEvent`/`PushContent` model the **real**
payload shape (see
the push/webhook note above) — every field `Option`, because Imou also
sends empty `{}` verification pings that must be silently accepted, not
treated as errors. A `PushEvent` is converted into a synthetic `Alarm` via
`parse_push_timestamp` (parses `"yyyyMMddTHHmmss"` strings — `time` and
`utcTime` are BOTH this format in push, unlike `getAlarmMessage`'s numeric
epoch `time` — treating the digits as UTC either way, same local-vs-genuine
convention as `Alarm`'s own doc note). `channel_id` is hardcoded `"0"`
since the push payload has no `cid` and these devices are single-channel;
`channel_name` prefers the payload's `dname`, falling back to a
`device_id -> channel_name` map built once at startup via
`api::devices::list` (only used for this fallback and for populating
`recorded_channels` — `listen` does not poll for detection). Retention for
the ring buffer uses an explicit `--max-push-latency-secs` (default 300)
in place of `watch`'s polling `interval`, since there's no polling cadence
to derive it from — sized generously given the observed ~48-minute cold
start, but a truly slow first delivery after a restart can still outrun it
and lose pre-roll; this is a real, accepted limitation of push, not a bug.
`listen.rs` has unit tests (`cargo test listen::`) covering the timestamp
conversion against a real captured payload — pure logic, no live API
needed, unlike most other verification in this project.

## Google Drive clip upload (`src/gdrive/`)

Both `watch` and `listen` optionally upload each extracted clip to Google
Drive right after a successful local save, deleting the local `.mp4` only
if the upload succeeds — the local copy stays as the fallback on any
upload failure (network down, bad token, etc.), matching this project's
general "detection/recording must never depend on a remote call
succeeding" posture. `recorder::extract_clip` now returns the written
`PathBuf` (previously `Result<()>`) so `recorder::spawn_clip_extraction`
has something to hand to `gdrive::upload_and_replace`; the upload itself
runs inside the same detached per-alarm task extraction already used
(fire-and-forget, errors only `eprintln!`'d — see the ring-buffer section
above), so a slow or failed upload never blocks `watch`'s poll loop or
`listen`'s push HTTP handler. The feature is entirely opt-in: if
`gdrive::config_from_env()` returns `None` (no `GDRIVE_CLIENT_ID`/
`GDRIVE_CLIENT_SECRET` in the environment), `watch`/`listen` pass `None`
through to every `spawn_clip_extraction` call and behavior is byte-for-byte
identical to before this feature existed — same "no-op if unconfigured"
convention as `recorder::local_config_for` for per-camera RTSP recording.

**A plain service account does not work here — verified live, not assumed
from docs.** The first implementation attempt used a Google service
account (JWT-bearer OAuth, no interactive login) since it's the more
natural fit for an unattended server process. It authenticates fine, but
the real Drive API rejects the actual upload with `403
storageQuotaExceeded`: *"Service Accounts do not have storage quota.
Leverage shared drives, or use OAuth delegation instead."* Service accounts
can only write into a Shared Drive or via domain-wide delegation — both
Google Workspace-only features, unavailable for uploading into a personal
Google account's ordinary "My Drive" folder. The shipped implementation
instead uses **real user OAuth** via Google's device-code flow
(`gdrive::device_flow::run_login`, wired to the `imou gdrive-login`
subcommand) — no local redirect URI or browser needed on the server itself
(the user opens the printed `verification_url` and enters `user_code` on
any other device), and uploads count against the authorizing user's own,
normal storage quota. This is a case worth remembering for this project's
established "verify against the real API, don't trust what looks like the
obviously-correct design" practice — service-account auth being simpler to
implement and more idiomatic for a headless deployment did not make it
correct here.

`gdrive::auth` persists `{refresh_token, access_token, expires_at}` to
`.gdrive_token_cache.json` via a generalized `token_cache::{load,save}`
(now generic over any `Serialize`/`DeserializeOwned` type + an explicit
path, rather than hardcoded to Imou's access token — `client.rs` passes
`.imou_token_cache.json` explicitly and keeps its own private `CachedToken`
type). Unlike Imou's access token, which re-fetches itself automatically
from `appId`/`appSecret` with no user interaction, losing this file means
the user has to redo the manual OAuth consent — so **it must be
bind-mounted** in the Docker deploy (`deploy/docker-compose.yaml`), unlike
`.imou_token_cache.json`, which isn't mounted at all because it doesn't
need to be.

`gdrive::upload` uses Drive API v3's **resumable** upload (a metadata-only
POST to open a session, then a single PUT of the whole file to the
returned session `Location`) rather than a one-shot multipart request —
Google's recommendation for anything beyond trivially small files, and
more resilient to a flaky home uplink given clips can be tens of MB. The
whole file is read into memory (`tokio::fs::read`) rather than streamed —
an accepted v1 simplification, not a hard limitation of the resumable
protocol itself; revisit if clip sizes grow substantially.
`gdrive::GDriveClient` uses an explicit 180s `reqwest` timeout (longer than
`ImouClient`'s 20s, since uploads move real file bytes instead of small
JSON payloads) — see the ring-buffer section's note on why an explicit
timeout matters structurally for any network call inside a long-lived
process.

**Per-day folders + retention (`src/gdrive/folders.rs`,
`src/gdrive/retention.rs`)**: every clip is uploaded into a `YYYY-MM-DD`
Drive folder (the alarm's *local* date — `recorder::alarm_local_time`,
shared with `extract_clip` so both always agree on which day a clip
belongs to) directly under the configured `GDRIVE_FOLDER_ID` root,
created via `folders::ensure_day_folder` if it doesn't exist yet. Folder
lookup/creation results are cached in-memory on `GDriveClient` (`HashMap`
behind a `Mutex`, keyed by the `YYYY-MM-DD` name) — this doubles as a lock
held across the whole "check cache, else list-or-create Drive folder, then
cache" sequence, so two clips from different channels finishing upload at
nearly the same moment can't race into creating two folders for the same
day. `retention::start_retention_sweep` runs once at startup and then
every 24h for as long as the process runs (`watch`/`listen`'s
`--gdrive-retention-days`, default 30, `0` = keep forever): lists the day
folders directly under the root, parses each name as a date, and
permanently **deletes** (not trashes, same semantics as
`recorder::cleanup_old_segments` for local segments) any older than the
retention window — deleting a Drive folder cascades to everything inside
it that has no other parent, so this alone reclaims the clips too, no need
to enumerate files individually. Folders whose name doesn't parse as
`YYYY-MM-DD` are left untouched, so the sweep only ever acts on folders
this app could plausibly have created itself. The sweep task is a plain
detached `tokio::spawn` with no shutdown signal — unlike the ring-buffer
recorders, it doesn't hold an OS resource (no child process to kill), so
there's no orphan risk in just letting tokio drop it at process exit.
Verified live: day-folder creation, reuse of an existing day folder on a
second upload the same day, and the retention sweep actually deleting an
old (synthetically dated) folder.

## Production deployment

See [`deploy/README.md`](../deploy/README.md) — `imou listen` as a
standalone Docker Compose project (own network, `Dockerfile` at repo root,
`ffmpeg` baked into the image), fronted by a dedicated Caddy container that
reuses the deployment host's existing TLS cert files. Mount the individual
resolved cert/key files, not the `letsencrypt/live/<domain>/` directory
itself — that directory's symlinks are relative and break across a bind
mount into a container (hit live while building the initial webhook test).

## Adding a new endpoint

1. Fetch the doc page for the method to get the real params/response field
   names and the URL path's bare method name.
2. Add a params struct (only the fields you need, `#[serde(rename = ...)]` to
   match the API's camelCase) and a response struct in the relevant
   `src/api/*.rs` module (or a new module + `pub mod` line in `src/api/mod.rs`
   if it's a new category).
3. Call `client.call(...)` or `client.call_action(...)` depending on whether
   the endpoint returns a `data` payload.
4. Wire a `clap` subcommand in `main.rs` if it should be user-facing.
5. Build and run it against the real account to confirm the response struct
   actually matches — see the trustworthiness note above.

## Safety note

PTZ commands (`imou ptz ...`) physically move a real camera. There's no
simulation/dry-run mode — treat these calls as having real-world side
effects, same as any other physically-actuating command.
