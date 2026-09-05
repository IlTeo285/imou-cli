# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A Rust CLI (`imou-cli`) for controlling Imou/Dahua Easy4ip cameras through the
Imou Open Platform HTTP API. Credentials live in `.env` (gitignored) and are
loaded at startup with `dotenvy`.

**This repo is public — before every commit, check every changed/new file
for personal or account-specific information**, not just secrets in the
obvious `.env`-shaped sense. This has actually slipped in before: doc
comments and `CLAUDE.md` notes written while narrating real verification
work ended up describing the real content of a real user's camera footage
(e.g. what specifically was seen walking through frame) — technically not
a credential, but not something that belongs in a public repo either.
Also watch for real IPs/hostnames/domains, device IDs, and RTSP
passwords — this project's own history shows these leak easily into
"verified live against the real thing" narration, precisely because that
kind of verification is this project's normal working style. When
documenting a live-verification finding, describe the mechanism and the
result in the abstract ("a real motion event," "the correct frame was
picked"), not the specific real-world content that happened to be in
frame.

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
Drive right after a successful local save. `recorder::extract_clip` now
returns the written `PathBuf` (previously `Result<()>`) so
`recorder::spawn_clip_extraction` has something to hand to
`gdrive::upload_clip`; the upload itself runs inside the same detached
per-alarm task extraction already used (fire-and-forget, errors only
`eprintln!`'d — see the ring-buffer section above), so a slow or failed
upload never blocks `watch`'s poll loop or `listen`'s push HTTP handler.
The feature is entirely opt-in: if `gdrive::config_from_env()` returns
`None` (no `GDRIVE_CLIENT_ID`/`GDRIVE_CLIENT_SECRET` in the environment),
`watch`/`listen` pass `None` through to every `spawn_clip_extraction` call
and behavior is byte-for-byte identical to before this feature existed —
same "no-op if unconfigured" convention as `recorder::local_config_for`
for per-camera RTSP recording.

**Local and Drive copies have independent retention, not a fallback
relationship.** Earlier versions deleted the local `.mp4` as soon as its
Drive upload succeeded (keeping it only as a fallback on upload failure).
`gdrive::upload_clip` no longer touches the local file either way — a
successful upload and the local file's deletion are now fully decoupled.
Instead, `recorder::start_local_retention_sweep` (`--local-retention-days`,
default 30, `0` = keep forever) periodically deletes `clips_dir` entries
older than its window, on the same "detached task, no shutdown signal
needed" reasoning as `gdrive::retention::start_retention_sweep` below,
parsing the `<YYYYmmddTHHMMSS>_<alarm_id>.mp4` filename the same way
`parse_segment_time` reads ring-buffer segment names. This exists because
a deployment may reasonably want a longer/shorter local retention window
than its Drive retention window (e.g. cheap local disk kept for a month,
Drive storage trimmed to a few days) — the two flags are set independently
per deployment, in `deploy/docker-compose.yaml`'s `command` block.

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

## MQTT motion event publishing (`src/mqtt.rs`)

Both `watch` and `listen` optionally publish each `MotionEvent` — the exact
same JSON already written to `motion_events.jsonl`, no second schema — to an
MQTT broker, entirely opt-in: `mqtt::config_from_env()` returns `None`
unless `MQTT_BROKER_HOST`/`MQTT_BROKER_PORT` are both set, and `watch::run`/
`listen::run` each independently build their own `Option<Arc<MqttPublisher>>`
from it (same convention as `gdrive::config_from_env()` — behavior is
unchanged when unconfigured, no CLI flag gates it).

**Topic shape**: one topic per camera, `{MQTT_TOPIC_PREFIX}/<channel_name>/motion`
(default prefix `imou`), meant to be consumed via a wildcard subscription
like `imou/+/motion`. `channel_name` (from Imou's own per-camera naming,
already trusted as-is elsewhere, e.g. in `recorder.rs`'s file paths) isn't
sanitized against MQTT-reserved topic characters (`/`, `+`, `#`) — an
accepted limitation for a single-operator CLI, not a real risk given who
names the channels.

**Fire-and-forget, same posture as Drive upload**: `MqttPublisher::publish`
is a sync method that builds the topic/payload and then `tokio::spawn`s a
task to actually await the publish, rather than awaiting it inline. This
isn't just defensive — `rumqttc`'s own docs note `AsyncClient::publish(...)
.await` can block on internal backpressure if the event loop isn't keeping
up (broker down/slow), which is exactly the same "one stalled network call
hangs a long-lived loop forever" failure class this project already hit
once with a timeout-less `reqwest::Client` (see the ring-buffer section
above). QoS is `AtLeastOnce`, not retained: at-least-once gives real
delivery assurance without QoS 2's extra handshake, and *not* retained
because this is an event stream, not a state topic — a retained "last
motion" value would misrepresent old motion as fresh to a newly-connecting
subscriber. An MQTT publish failure never affects `event_log` or clip
recording, same as a Drive upload failure never rolls back the local clip.

**The event loop must be driven, or nothing happens at all — not even the
initial connect**: `rumqttc::AsyncClient::new` returns a client handle and
a separate `EventLoop`; nothing (connect, publish, ping, reconnect) actually
happens until something calls `eventloop.poll().await` in a loop.
`MqttPublisher::connect` spawns exactly one detached `tokio::spawn` task to
own and drive this for the life of the process — same "no shutdown signal
needed, no OS resource owned" reasoning as `gdrive::start_retention_sweep`.
Reconnection itself is automatic as long as `poll()` keeps being called
after an `Err` rather than abandoned; there's a flat 1s `sleep` between
retries in that error branch specifically because the crate documents no
built-in backoff, and a bare retry loop against a broker that's down for a
while would otherwise busy-spin.

**Non-obvious explicit-timeout gotcha, worth remembering**: the initial
connect timeout is *not* a `MqttOptions` setting — `set_keep_alive` governs
an already-established connection's ping cadence, not how long to wait for
the first connect. The actual knob is `EventLoop.network_options` (a public
field on the `EventLoop` returned by `AsyncClient::new`, not `MqttOptions`)
via `.set_connection_timeout(secs)`, called explicitly in
`MqttPublisher::connect` right after construction — same explicit-timeout
rule as `ImouClient::new`'s 20s `reqwest` timeout and `GDriveClient::new`'s
180s one, applied here because this codebase has a documented incident
where a network call with no timeout silently hung a long-lived loop
forever with no crash and no error to signal it.

## AI video-content analysis (`crates/imou-vision`)

Both `watch` and `listen` optionally classify each extracted clip's content
(human/animal/vehicle/package/empty) and get a free-text description from a
**local** vision-language model served by Ollama, then use that to enrich
the event log/MQTT stream and, opt-in, gate the existing Google Drive
upload / MQTT publish. Same "opt-in, no-op if unconfigured" convention as
`gdrive`/`mqtt`: `imou_vision::config_from_env()` returns `None` unless
`AI_OLLAMA_URL`/`AI_MODEL_NAME` are both set, and behavior is unchanged
from before this feature existed if they aren't.

**Why a separate workspace crate, not another `src/` module**: the repo's
root `Cargo.toml` is now both a package manifest and a workspace root
(`[package]` + `[workspace] members = ["crates/imou-vision"]` in the same
file — Cargo supports this directly, avoiding a `src/` move and the
resulting churn to every existing `use crate::...` path). `imou-vision`
depends on nothing from `imou-cli` and could be reused standalone; the
dependency only runs one direction (`imou-cli` depends on `imou-vision`,
declared as a normal path dependency in the root `[dependencies]`).
Internally the crate is split into `reqwest`-free modules (`response.rs`
parsing, `relevance.rs`, `frames.rs`'s timestamp math) plus the actual
HTTP-calling `client.rs`, so most of it is unit-testable without a live
Ollama instance — same precedent as `listen::tests` verifying timestamp
parsing against a captured real payload with no live API needed.

**Frame extraction happens inside `imou-vision`, not `recorder.rs`**:
`ffmpeg`/`ffprobe` are already a hard runtime dependency of the Docker
image (installed for the ring-buffer/clip-concat code), so `imou_vision::
extract_frames` invokes them directly via `tokio::process::Command`, same
idiom as `recorder.rs`. `imou_vision::analyze_clip` (the crate's main
entry point) probes the clip's duration, picks 1–3 evenly-spaced
timestamps via `frames::pick_timestamps` (pure function, falls back to the
clip's midpoint for very short clips), extracts one JPEG per timestamp
into a per-call scratch dir under `std::env::temp_dir()`, reads them, and
POSTs them (base64-encoded) to Ollama's `/api/generate` with `format:
"json"` and a fixed prompt asking for `{"category": ..., "description":
...}`. The scratch dir is removed (best-effort) on both the success and
error path — callers own no cleanup.

**Non-obvious Ollama gotcha, worth remembering**: the outer response's
`response` field holds the model's structured output as a JSON-encoded
**string**, not a nested JSON object — `response.rs`'s
`parse_generate_response` is therefore a genuine double-decode (outer
envelope, then the inner string), tested explicitly against a response
captured from a real `ollama run moondream` call.

**`format` must be a JSON Schema, not the bare string `"json"` — verified
live, this one actually broke output correctness, not just parsing
elegance**: `client.rs::response_schema` sends a JSON Schema object as
`format` (Ollama's "structured outputs" feature, a grammar constraint
applied at decode time regardless of model). The bare string `format:
"json"` ("produce some valid JSON, shape unconstrained") was tried first
and failed live: `moondream` reliably degenerated into repeating a garbage
key (`"distance_elapsed"`) hundreds of times until it hit its generation
limit, ~31 seconds later, leaving the inner `response` string genuinely
**unterminated** (an open string, no closing brace) despite starting with
a perfectly good `category`/`description` — not just verbose, actually
invalid JSON that fails to parse at all. Passing the schema instead
constrains generation to exactly the two fields being asked for, so the
object closes as soon as they're filled: same model, same test image,
under 1 second, clean `done_reason: "stop"`. `response.rs` still degrades
gracefully (falls back to `Category::Unknown`, raw text kept as the
description) if a differently-behaving model or a future Ollama version
ever produces a malformed body again — that fallback is a safety net for
a failure mode this project has directly observed, not a hypothetical.

**Relevance is computed locally, not asserted by the model**:
`relevance::is_relevant` maps `Category` -> `bool` in Rust
(`Empty`/`Unknown` are not relevant, everything else is) rather than
asking the model to also judge relevance itself — a local
vision-language model's judgment of "what is this" is far more reliable
than its judgment of "is this worth caring about", and a fixed mapping is
easy to reason about/adjust later. A model response that isn't valid JSON,
or names a category outside the fixed set, degrades to `Category::Unknown`
with the raw text kept as the description — analysis quality degrades
gracefully, it must never error out and block the clip pipeline.

**Integration into the existing pipeline — the MQTT/log timing tension**:
today, per alarm, the immediate `MotionEvent` log line and `.../motion`
MQTT publish both fire *before* clip extraction even starts (extraction
has to wait for the post-roll window to land on disk); AI analysis can
only run once the clip exists, so gating the fast notification path on it
would add real, unacceptable latency to every motion alert. Instead, AI
analysis is a **second, deferred, additive** event:
`motion_event::MotionAnalysisEvent` (`kind: "motion_analyzed"`, correlated
to the original line by `alarm_id`) is appended to the *same*
`motion_events.jsonl` via the existing `EventLog::append`, and (if MQTT is
configured) published to a *second* topic, `{prefix}/<channel_name>/
motion-analyzed`, via `MqttPublisher::publish_analysis`. Both happen
inside `recorder::spawn_clip_extraction`'s `Ok(out_path)` arm, after
`extract_clip` succeeds and (if `vision` is configured)
`imou_vision::analyze_clip` runs.

**Gating is fail-open and opt-in**: `spawn_clip_extraction` gained
`ai_gate_gdrive_upload`/`ai_gate_mqtt_analyzed` bool parameters
(`--ai-gate-gdrive-upload`/`--ai-gate-mqtt-analyzed` CLI flags on
`watch`/`listen`, both default `false`). An AI call that errors, times
out, or isn't configured at all never suppresses the existing Drive
upload / MQTT publish — only a confident, successfully-parsed `relevant ==
false` verdict does, and only when the corresponding flag is explicitly
set. The local `.mp4` itself is **never** gated on the AI verdict — it
always stays on disk regardless, same "local retention is independent of
upload outcome" philosophy as the Drive-upload section above.
`Alarm.label_type` (Imou's own on-device classification) is deliberately
*not* used to skip the AI call — it's documented elsewhere in this file as
undocumented/unreliable, and using it to suppress inference would risk
silently dropping real events on exactly the accounts where it's wrong.

**Runtime**: Ollama runs as its own sidecar Docker container
(`imou-ollama` in `deploy/docker-compose.yaml`), not embedded in the
`imou-cli` process — keeps the `imou-cli` image free of
ONNX/CUDA-class dependencies, and `imou-vision` just speaks plain HTTP to
it (`reqwest`, explicit timeout, same rule as every other network client
in this codebase — see the MQTT section above for why that timeout is
never optional). Default model: `moondream` (1.8B, CPU-friendly, fits a
home-server/small-VPS deployment target); `AI_MODEL_NAME=llava` is a
documented heavier opt-in for a GPU host. Model weights are pulled once
(`ollama pull <model>`) into a bind-mounted volume so they survive
container recreates — see `deploy/README.md`.

**Verified live on the real production homeserver: not viable on that
specific hardware, currently disabled there.** Even `moondream` (the
smallest capable model) pegged all 4 cores of that host's Intel Apollo
Lake CPU (no GPU) at 300%+ for multiple minutes per single-image call —
observed still running past 400s in one test — on a box that also runs
unrelated production services (other projects' backends/databases) with
under 300MB of free RAM to begin with. `AI_OLLAMA_URL`/`AI_MODEL_NAME` are
commented out in that deployment's `.env` and the `imou-ollama` container
was removed; the code path itself is untouched and fail-open, so it's a
one-line `.env` change (plus redeploying an `imou-ollama` service) to
re-enable on a host that can actually carry it — but don't assume a
"small" local VLM is automatically cheap enough for whatever box `watch`/
`listen` happen to be running on; verify like this was verified. See
`snapshot` below for what replaced it on that deployment.

## Frame-diff snapshot extraction (`imou-vision::snapshot`)

Both `watch` and `listen` always (no config/opt-in — see below) extract a
representative JPEG snapshot per motion clip using pixel-level frame
differencing via ffmpeg — the practical replacement for the Ollama path
above on hardware too weak to run even a small VLM, born directly from
that finding.

**Algorithm** (`crates/imou-vision/src/snapshot.rs::extract_snapshots`): a
single ffmpeg analysis pass —
```
select='not(mod(n\,DECIMATE))',tblend=all_mode=difference,signalstats,metadata=print:file=<tmp>
```
`tblend=all_mode=difference` outputs the per-pixel `|frame_n - frame_n-1|`
image; `signalstats`' `YAVG` on that is the average magnitude of that
difference — a direct, continuous motion score, 0 for identical
consecutive frames. `select='not(mod(n,DECIMATE))'` (default `DECIMATE=4`)
compares every 4th frame instead of every consecutive one — verified live
this cuts analysis time roughly 4x (31s → 8s on a real ~97s clip) with no
loss of detection, since real motion spans many frames regardless. The
metadata is written to a temp file (`file=...`), not stdout — avoids
fighting with `-f null -`'s own output on the same stream, same pattern as
`recorder::extract_clip`'s temp concat-list file. `parse_diff_scores`
(pure function) parses `(pts_time, YAVG)` pairs from it;
`select_top_timestamps` (pure function) greedily picks the `max_count`
(default 1) highest-scoring timestamps at least `min_gap_secs` (default
2.0) apart, then one `ffmpeg -ss <t> -frames:v 1` call per pick (the same
single-frame-extraction helper `frames.rs` uses for the Ollama path,
factored out as `extract_frame_at` and shared by both).

**Rejected first attempt, worth remembering**: ffmpeg's `select=gt(scene,X)`
— a hard scene-CUT detector built for edited video, the obvious-looking
first choice — was tried first and verified live to be unusable for CCTV
motion: on one real clip its score never exceeded its own ~0.06-0.08 noise
floor (never a real spike, just compression noise), and on a second real
clip it scored exactly 0 for the ENTIRE clip despite real motion partway
through. There's no threshold that fixes this — it's the wrong metric for
gradual real-world motion, not a tuning problem. The `tblend=difference` +
`signalstats` approach used instead is a literal per-pixel motion
magnitude, and correctly picked out that same moment on the second clip
(confirmed by eye against the extracted JPEG, score roughly 3x the
surrounding baseline) — this is why `SnapshotConfig`
has no threshold field: every clip has *some* highest-scoring frame, and
that's always the one taken, regardless of its absolute value.

**Always on, unlike `gdrive`/`mqtt`/`vision`**: no `config_from_env`, no
env var gate. It's local pixel arithmetic against a file already on disk —
no network call, no credential, no external service — so there's nothing
to be "not configured." `--snapshots-dir` (default `snapshots`) and
`--snapshot-count` (default `1`) are the only knobs, same style as
`--clips-dir`. Filenames match `extract_clip`'s own convention exactly
(`<local-time>_<alarm_id>[_n].jpg` under
`<snapshots_dir>/<channel_name>/`), so a clip and its snapshot(s) are
trivially correlated and `recorder::start_local_retention_sweep`
(generalized to take a file extension parameter — `"mp4"` for clips,
`"jpg"` for snapshots, same `--local-retention-days`) sweeps both trees
with the exact same filename parser.

**Wired into `recorder::spawn_clip_extraction` unconditionally**, right
alongside (not gated by) the AI analysis block — a snapshot failure only
logs a warning, same fail-open posture as everything else in that
function; it never affects clip extraction, upload, or AI analysis.

Verified live end-to-end on a real homeserver clip with real motion in it
(downloaded via the `/mnt/wd/imou-video` share, analyzed with a
`rust:1-slim-bookworm` + `ffmpeg` container matching the production build
image exactly, to avoid a glibc/codec mismatch with this dev machine's own
Fedora `ffmpeg-free` package, which lacks HEVC decode entirely — these
cameras' clips are H.265): the snapshot picked out the moment something
actually entered frame, correctly ranked above two other picks from the
same clip that were genuinely uneventful (an empty scene) — confirmed by
eye, not just by the score numbers.

## Continuous recording (`src/recorder.rs`)

Both `watch` and `listen` can optionally record the **entire** video
stream (not just motion pre/post-roll) for every channel with local RTSP
config, into a rolling window — e.g. "keep the last 2 days." Opt-in via
`--continuous-retention-hours` (default `0`): **unlike every other
`--*-retention-*` flag in this CLI, `0` here means the feature is off, not
"keep forever"** — continuous recording is the one feature costly enough
(tens of GB per camera per day) to need an explicit opt-in via its own
retention value rather than being always-on with just the sweep
disableable. `--continuous-dir` (default `continuous`) and
`--continuous-segment-minutes` (default `15`) are the other two knobs.

**One ffmpeg process per camera, two independent segment outputs** — not
two separate RTSP connections. `build_recorder_args`
(`src/recorder.rs`, pure function, unit-tested against the exact command
verified live) adds a second, independent `-c copy -f segment` branch to
the same ffmpeg invocation that already writes the short-lived `.ts`
pre-roll ring buffer:
```
ffmpeg -i rtsp://... -c copy \
  -f segment -segment_time 2    -strftime 1 -reset_timestamps 1 <buffer>/seg_%Y%m%dT%H%M%S.ts \
  -c copy -f segment -segment_time <secs> -strftime 1 -reset_timestamps 1 <continuous>/seg_%Y%m%dT%H%M%S.mp4
```
Verified live (synthetic real-time-paced source, `-re`) that ffmpeg
accepts two independent `segment` muxers — different `segment_time`,
different container format — from one input in a single process. Reusing
the connection this way, rather than opening a second RTSP session to the
same camera, was a deliberate choice: how many concurrent RTSP sessions
these cameras tolerate has never been tested, so avoiding the question
entirely is the safer default. Both outputs are `-c copy` (no re-encode),
so the CPU cost of the second branch is negligible — same reasoning
already documented above for the ring buffer itself.

**Format is `.mp4`, not `.ts`, unlike the ring buffer** — the ring buffer
is only ever consumed internally (concatenated into a clip), but the
continuous archive is meant to be opened directly by a person. The
`segment` muxer finalizes each file (writes its `moov` atom) when it
rotates to the next one, so every *completed* segment is a normal, fully
playable file. **The currently-recording segment is the one real caveat**:
it has no `moov` atom yet, so a player can fail to open it or report a
wrong duration/seek range until it rotates — not a corruption risk, just
an mp4-format-with-a-segment-muxer limitation, worth knowing before
assuming "the newest file won't open" means something is broken. This is
also why the default segment length is 15 minutes, not something larger
like an hour: it bounds how stale "the most recent watchable footage" can
be.

**Naming and retention reuse the ring buffer's own machinery** — same
`seg_<YYYYmmddTHHMMSS>.<ext>` convention, so `parse_segment_time` and the
now-generalized `cleanup_old_segments` (extension is a parameter — `"ts"`
for the ring buffer, `"mp4"` for the continuous archive; same refactor
pattern already applied once to the snapshot feature's
`sweep_local_clips`→`sweep_local_files`) are shared verbatim, just pointed
at a different directory/extension/retention window. `start_ring_buffer`
spawns a third cleanup task (alongside the recorder and the existing
ring-buffer sweep) only when continuous recording is configured.

**No per-day subfolders** — considered and rejected: ffmpeg's `segment`
muxer does not create missing directories on the fly, so a pattern like
`continuous/%Y-%m-%d/seg_%H%M%S.mp4` would silently break at every
midnight rollover (the new day's directory doesn't exist yet). A flat
per-channel directory, sortable by filename, avoids this entirely — same
layout already used for the ring buffer and for clips.

**Intended viewing path is the existing network share, not a new UI**:
`/mnt/wd` on the homeserver is already reachable as a network share from
the user's own machines (confirmed live this session — mounted locally at
`/mnt/wd_share`), so pointing `--continuous-dir` there means completed
segments are directly double-clickable from a normal file browser or VLC,
no additional web UI needed. Completed segments are safe to read
concurrently with ffmpeg writing the *next* one — no locking concern,
since it's a plain sequential write to a different, already-closed file;
the only real limitation is the in-progress-segment one described above,
which is inherent to the file format, not the filesystem or network
share.

**A second, related caveat found live, not just theoretical**: a
container restart/redeploy *while a continuous segment is mid-write*
leaves that specific segment permanently corrupt — `kill_on_drop` kills
ffmpeg mid-write (same as any other restart, e.g. an auto-restart after a
stalled RTSP connection), so that file never gets its `moov` atom and the
new ffmpeg process that starts afterward begins an entirely new segment
file rather than resuming the old one. Confirmed live: redeploying to add
the `TZ` fix (see below) killed a segment ~9 minutes into its 15-minute
window, permanently unplayable, while the segment before it (which
reached a full natural rotation first) played back fine at the expected
~900s duration. No fix applied for this — same "no partial-clip handling
on exit, documented as intentional" posture already accepted for
motion-triggered clip extraction — but worth knowing before assuming a
broken file means the recording pipeline itself is broken: check whether
a restart happened to land in that segment's window first.

**Non-Imou, continuous-only cameras**: `recorder::LocalCameraConfig` is an
enum (`Dahua { ip, secure }` — the original template, or `CustomUrl(String)`
— a full RTSP URL used verbatim, no credentials injected). `local_config_for`
checks `CAM_<NAME>_URL` before falling back to `CAM_<NAME>_IP`/`_SECURE`, so
a camera whose RTSP shape/auth doesn't fit the Dahua pattern (e.g. a
different vendor with no auth at all) can still be recorded. Since such a
camera has no Imou device/channel, its name can't come from
`api::devices::list` the way every other channel's does — `watch.rs`/
`listen.rs` build a *separate* `recording_channels` list (real Imou channels
+ synthetic entries from `recorder::extra_continuous_channels`, which reads
`EXTRA_CONTINUOUS_CHANNELS`, a comma-separated list of channel names) and
pass only that to `recorder::start_all`; the original Imou-only `channels`
list still drives alarm polling (`watch.rs`) and the push device-name map
(`listen.rs`) unchanged, since a channel-less camera can never produce an
`Alarm`. Its ring buffer still gets built alongside the continuous archive
(no continuous-only code path) — simply never consumed, self-cleans via the
existing retention sweep, negligible cost.

## Grid continuous recording (`--continuous-mode grid`, `src/grid.rs`)

`--continuous-mode grid` (default `separate`, i.e. unchanged behavior)
replaces per-camera continuous archives with a single video showing every
configured camera at once, in a fixed 2x2 grid, synchronized on time.
Still gated by `--continuous-retention-hours > 0` like plain continuous
recording — `watch.rs`/`listen.rs` warn at startup if `grid` is selected
with retention left at `0`, since that combination is otherwise a silent
no-op (easy to miss: the enable/disable knob and the mode knob are two
different flags).

**Batch post-process, not a live multi-input ffmpeg process** — deliberate
choice, not a first cut to be replaced later. Each camera keeps writing
its own continuous `.mp4` segments exactly as in `separate` mode (same
`-c copy`, same single ffmpeg process/RTSP connection piggybacked on the
ring buffer — see the "Continuous recording" section above), just into an
internal, non-user-facing staging tree
(`<continuous_dir>/.grid-staging/<channel_name>/`) instead of a retained
per-camera archive. A separate periodic task (`grid::start_grid_composer`,
spawned once per group from `recorder::start_all`, not per channel — it
doesn't fit `start_ring_buffer`'s per-channel loop) picks up completed
staging segments and composes them into one grid segment with
`ffmpeg -filter_complex xstack` under `<continuous_dir>/grid/`. This is
the **one** unavoidably CPU-costly re-encode step in this whole codebase —
everywhere else is stream-copy — which matters because the production
deployment target is documented above (see the AI vision section) as a
weak, GPU-less Apollo Lake CPU that already can't run even a small vision
model. `--grid-tile-size` (default `960x540`) and `--grid-fps` (default
`8`) are the CPU-cost knobs; `-preset ultrafast -crf 28` is hardcoded, not
exposed, for v1.

**Sync granularity is per-segment-file, not frame-accurate — and requires
a producer-side fix to even get that far.** Each camera's continuous-branch
ffmpeg process starts at an independent wall-clock moment and rotates
`segment_minutes` after *its own* start, so two cameras' "latest segment"
are not otherwise aligned to the same window at all. The fix is
`-segment_atclocktime 1` added to the continuous branch only (not the
`.ts` ring-buffer branch) in `build_recorder_args` — this makes the
`segment` muxer cut at wall-clock multiples of `segment_time`
(`:00/:15/:30/:45` for 15-minute segments) regardless of when a camera's
ffmpeg process actually started or last auto-restarted, leaving only a few
seconds of keyframe-boundary jitter between cameras rather than up to a
full `segment_minutes` of drift.

**Verified live** (synthetic real-time-paced sources, `-re`, same
methodology as the original two-branch continuous-recording discovery
above — no real cameras needed for this part, since it's a property of
the `segment` muxer itself, not of the video content): two independent
ffmpeg processes, each with the same two-branch shape `build_recorder_args`
produces (a plain `.ts` ring-buffer branch plus a `.mp4` continuous branch
with `-segment_atclocktime 1`), started 4 seconds apart against a
`testsrc` lavfi source. The `.ts` branch (no `atclocktime`) rotated on its
own process-relative schedule as before, unaffected. The `.mp4` branch on
*both* processes converged onto the exact same absolute wall-clock
boundaries once past each process's own irregular first (partial) segment
— e.g. one process's segments landed at `:40` (partial), `:42`, `:48`,
`:54`, `:00`, `:06` and the other's (started at `:44`) landed at `:44`
(partial), `:48`, `:54`, `:00`, `:06`, `:12` — `:48/:54/:00/:06` identical
across both despite the different start times. This confirms the flag is
correctly honored on the *second* muxer branch of a two-output
single-process invocation, not just the well-trodden single-output case.

**Matching algorithm** (`grid::next_window`/`bucket_window`/
`is_completed`/`match_window`, all pure and unit-tested): a single shared
cursor advances the whole group through wall-clock windows (not one cursor
per camera — the point is one composed output per window). Per window,
per configured slot: list that camera's staging segments, keep only ones
provably `is_completed` (a newer sibling exists, proving ffmpeg rotated
past it, **or** enough wall-clock time has elapsed since its nominal start
that it must have rotated regardless — the latter guards a camera that
dies right after writing its last segment, which would otherwise never
satisfy the first condition and strand that segment forever), then pick
whichever completed segment's start time is closest to the window within a
tolerance (`grid::alignment_tolerance`, **not** `recorder::FLUSH_MARGIN` —
see the live-verification note below for why those two needed to be
different constants). A slot with no match in range becomes `None` —
composed as a black tile via an `-f lavfi -i
color=c=black:...` source, **not** a reason to fail the whole window; only
when *all four* slots come back `None` does that window get skipped
entirely (logged once, cursor still advances — a permanently-down camera
set must not wedge the composer or spam all-black files forever). If the
composer falls behind by more than one window, `next_window` jumps
straight to the newest fully-elapsed boundary rather than replaying every
missed one.

**`xstack`, not pairwise `hstack`+`vstack`**: tiles are pre-scaled to
identical size first, making the fixed 4-slot
`layout=0_0|w0_0|0_h0|w0_h0` trivial, and `xstack=...:shortest=1` handles
"the real inputs present this window have slightly different durations"
for the whole grid in one place — the composed segment's length is simply
`min` of the real inputs, with no separate placeholder-duration
computation needed (an infinite `lavfi` black source needs no `-t`/`d=`
at all). Requires ffmpeg 4.1+ (`xstack` filter). **Verified live**: the
actual deploy image's ffmpeg (Debian bookworm's apt package, 5.1.9-0
+deb12u1 — the same one `Dockerfile` installs) has both `xstack` and
`libx264`; `recorder::start_all` also calls `grid::check_xstack_available`
before spawning the composer, so a future ffmpeg swap missing the filter
fails loudly at startup instead of per-window inside the detached
composer task. The exact `build_grid_args` command shape was run
end-to-end against two real segment files (from the synthetic test above)
plus two black placeholders: output correctly came back as 1920x1080
(`2 * 960x540`), duration `6.000000s` matching `shortest=1`'s contract
(the shortest real input present), and a frame pulled from the middle of
it visually confirmed the TL/TR/BL/BR layout — the two real sources in the
top corners, both bottom corners solid black, no cross-contamination
between slots.

**Camera order** (`grid::resolve_grid_order`, TL/TR/BL/BR): `--grid-order`
flag, else `GRID_ORDER` env var, else an alphabetically-sorted default —
deliberately never a direct `HashSet<String>` iteration (`recorded_channels`
has no stable order across runs). A configured name with no local RTSP
config becomes a permanent black tile (warned once, not an error) — same
fail-open convention as `local_config_for` returning `None` elsewhere.
More than 4 names is a hard startup error (ambiguous), not a silent
truncation.

**Staging retention is deliberately NOT the user's `--continuous-retention-hours`
window.** In `grid` mode, `start_ring_buffer` skips spawning its normal
per-channel continuous-cleanup task entirely (which would otherwise sweep
staging with the same — potentially large, e.g. 48h — retention meant for
a *final* artifact, letting unconsumed raw per-camera footage pile up per
camera and defeating "replace, don't retain per-camera"). Instead,
`grid::staging_ttl` (`segment_minutes * 3` — roughly two windows of slack
past the composer's own cadence) bounds staging independently, swept once
for the whole `.grid-staging` tree by the composer task itself. This also
doubles as cleanup for a camera later removed from `--grid-order`/config —
its old staging subdirectory has no other path to ever being reaped.

**Fail-open on a failed compose, same posture as everywhere else in this
pipeline** (gdrive upload, AI gating, snapshot extraction): if the `ffmpeg
xstack` call itself fails for one window, the staging inputs for that
window are deliberately **not** deleted (left for the TTL sweep to reap
later, in case someone wants to hand-compose that window manually) and the
cursor still advances past it — retrying the same window forever is
explicitly rejected, since a stuck window would wedge the composer the
same way a restart-corrupted continuous segment is already documented
(above) to be unrecoverable, not retried.

**Real-camera finding that broke the first cut of the matching tolerance —
worth remembering**: the synthetic-source test above proved
`-segment_atclocktime` alignment *works*, but running the actual composer
against 4 real cameras (2 Imou/Dahua, 2 plain `CAM_<NAME>_URL` ones) showed
every window coming back "no available camera footage from any configured
slot" past the initial warm-up, even with all 4 RTSP connections alive and
producing segments. Root cause: `-segment_atclocktime`'s cut point is
bounded by *keyframe availability*, not wall-clock precision — it cuts at
the first keyframe at-or-after the boundary, and real consumer cameras'
keyframe interval is far sparser than the synthetic `testsrc` encoder's.
Observed live, with `segment_time=60` for a fast test cycle: real cut
points landing up to ~29s past the nominal minute boundary, consistently,
not just as occasional jitter. `recorder::FLUSH_MARGIN` (5s) — calibrated
against the synthetic, frequent-keyframe source — was nowhere near
generous enough once real keyframe-driven drift entered the picture; used
as `match_window`'s tolerance, it rejected every real segment as "too far
from the boundary" to match. Fixed with a dedicated `grid::alignment_tolerance`
(60s, capped at half the segment length so a short test `segment_minutes`
doesn't get an oversized tolerance relative to its own window) used for
`match_window`'s tolerance, `is_completed`'s "enough time elapsed" branch,
and the composer's own per-window readiness wait — `recorder::FLUSH_MARGIN`
itself is untouched and still governs the ring-buffer/clip-extraction
flush timing it was originally sized for elsewhere in this codebase; grid
matching needed a materially larger, separately-named constant, not a
reused one. **Re-verified live after the fix**, same 4 real cameras, 3
consecutive windows: every one found real footage on all 4 slots (only the
very first 1-2 windows during startup warm-up still legitimately have
nothing yet, which is correct, not a bug).

**Compose command verified live end-to-end against real camera footage**,
not just synthetic sources: took 4 real staging segments (one per camera,
same matched window) produced by the run above, ran the exact
`build_grid_args` shape through the actual deploy image's ffmpeg (Debian
bookworm, `libx264`) — output came back as a correctly-dimensioned,
correctly-durationed (`shortest=1` honored against real, slightly
differing real segment lengths), playable grid video; a frame pulled from
the middle of it confirmed by eye all 4 real camera feeds tiled into the
correct TL/TR/BL/BR corners with no cross-contamination between slots (per
this project's "describe the mechanism and result in the abstract, not the
specific real-world content" privacy rule — see the top of this file — the
verification frame itself was not kept).

**Genuinely still not verified, because it requires the specific
production host**: real compose throughput on the actual Apollo Lake
production target at the default tile size/fps — the real go/no-go for
whether `960x540`/`8fps`/`ultrafast`/`crf 28` are sufficient there, or
whether a VAAPI (`h264_vaapi`) escape hatch becomes necessary sooner than
"eventually" (that hardware does have QuickSync, per the AI vision section
above, but building that escape hatch before measuring the plain-CPU path
first is out of scope for this feature). Whether the real RTSP source
carries an audio track was also not directly checked, but doesn't matter
either way — the compose filtergraph maps only `[N:v]` per input and the
output uses `-an`, sidestepping the question by design. Check both during
the deploy verification steps in `deploy/README.md`.

## Filename timezone (`--filename-timezone`, `src/recorder.rs`)

**Real bug found live, root cause was container configuration, not
application logic**: filenames (ring buffer/continuous segments, clips,
snapshots) were observed on the production homeserver showing UTC instead
of the account's real local time, even though the code was already
written to use local time everywhere in filenames (`chrono::Local` in
Rust, ffmpeg's `-strftime 1` which uses the *process's* clock). Root
cause: `docker exec imou-cli date` showed UTC while the host itself was
CEST — the container simply had no `TZ`/`/etc/localtime` configured (a
bare Docker container defaults to UTC), so "local" was accidentally UTC
by omission. Fixed independently of the feature below by setting `TZ` in
`.env` (see `.env.example`, `deploy/.env.production.example`) — the
runtime image already has `tzdata` installed as a transitive dependency
(confirmed live), so no `Dockerfile` change was needed, just the env var.

On top of that fix, `--filename-timezone <utc|local>` (default `local`)
lets this be an explicit choice rather than only "whatever the container's
`TZ` happens to be." Scope is deliberately narrow — **filenames only**:
- Ring buffer/continuous archive segments (ffmpeg `-strftime`), clip and
  snapshot filenames (`extract_clip`, `spawn_clip_extraction`'s snapshot
  `file_prefix`), and the retention sweeps that later parse those same
  names back (`cleanup_old_segments`, `sweep_local_files`) — the sweep's
  cutoff computation MUST use the same convention the names were written
  in, or it compares against the wrong "now" and evicts at the wrong
  time; both go through the same `FilenameTimezone` value for exactly
  this reason.
- **Deliberately excluded**: `MotionEvent`/`MotionAnalysisEvent`'s
  `time`/`local_time` fields in the JSON event log (already a
  well-defined, always-UTC/always-local pair — unrelated to filenames);
  `watch.rs`'s polling windows (`ChannelCursor`, tied to
  `getAlarmMessage`'s own local-as-if-UTC filtering behavior documented
  above in Architecture, not a display preference — changing this would
  break the actual API filter); the Google Drive day-folder grouping
  (`spawn_clip_extraction`'s `alarm_local_time(&alarm).date_naive()`,
  kept as the real local calendar day always, regardless of this flag —
  "which day did this happen" is a different question from "how is the
  filename formatted").

**Mechanism, two different techniques for two different clocks**:
- ffmpeg's `-strftime` uses the *child process's* clock, not something
  passed as a CLI arg — `run_recorder_supervisor` sets `TZ=UTC` on the
  spawned ffmpeg's environment only for `FilenameTimezone::Utc`
  (`ffmpeg_tz_env`, a pure function so this is unit-tested without
  spawning ffmpeg); for `Local` it sets nothing and inherits the
  container's own `TZ` (which is why the fix above matters independent of
  this flag — `local` mode still depends on it being correct). This
  avoids ever needing to know/hardcode the real IANA zone name for "local"
  mode.
- Rust-side filenames go through two small helpers,
  `alarm_filename_time`/`now_naive`, which resolve straight to
  `NaiveDateTime` once the convention is chosen — replacing what used to
  be a single hardcoded `alarm_local_time`/`Local::now()` call each.
  `extract_clip` previously used `alarm_local_time` for two things at
  once: matching the pre/post-roll window against ring-buffer segment
  filenames, and building the clip's own output filename — both had to
  move to the mode-aware helper together, since the window-matching side
  must stay consistent with whatever convention the segments were
  actually named in, not just the final filename.

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
