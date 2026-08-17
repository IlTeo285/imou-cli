# imou-cli

A Rust command-line tool for controlling Imou/Dahua Easy4ip cameras through
the [Imou Open Platform](https://open.imoulife.com/book/start.html) HTTP API.

## Features

- List cameras/channels bound to your account
- Check device online status
- Start a live stream and get an HLS URL
- PTZ control (pan/tilt/zoom) for supported cameras
- `watch`: a foreground service that polls for motion detection and logs
  each event to a file, and can save a local pre/post-roll video clip per
  event straight from each camera's LAN RTSP stream (no cloud involved)
- `listen`: same logging/clip-recording, but driven by Imou's push
  callbacks instead of polling — no polling interval, but a slow warm-up
  after each fresh registration (see below)
- Access tokens are cached locally between runs (tokens last ~3 days)

## Setup

1. Install [Rust](https://www.rust-lang.org/tools/install) (stable toolchain).
2. Create a `.env` file in the repo root:

   ```env
   APP_ID=your_app_id
   APP_SECRET=your_app_secret
   # Data center the app was registered in:
   #   sg = Singapore / East Asia
   #   fk = Frankfurt / Central Europe
   #   or = Oregon / Western America
   DATA_CENTER=or
   ```

   Get `APP_ID`/`APP_SECRET` by [creating an application](https://open.imoulife.com/book/readme/create.html)
   on the Imou Open Platform console. `DATA_CENTER` must match the region the
   app was registered in — calling the wrong region fails auth even with
   valid credentials.

   Optionally, for local clip recording in `watch` (see below), add per-camera
   LAN credentials:

   ```env
   CAM_FRONTDOOR_IP="192.168.1.100"
   CAM_FRONTDOOR_SECURE=your_rtsp_password
   ```

   `<NAME>` is the channel name from `imou devices`, upper-cased. The
   password is the camera's local RTSP/ONVIF password (often called
   "Security Code" in the Imou Life app, under the device's info/QR screen —
   not your cloud account password, and usually different per camera).

3. Install [ffmpeg](https://ffmpeg.org/) and make sure it's on `PATH` — only
   needed if you want local clip recording in `watch`.

4. Build:

   ```sh
   cargo build --release
   ```

## Usage

```sh
# Fetch (or reuse the cached) access token
cargo run -- token

# List devices/channels on the account
cargo run -- devices

# Check whether a device is online
cargo run -- status <DEVICE_ID>

# Start a live stream and print its HLS URL
cargo run -- live <DEVICE_ID> [--channel-id <ID>] [--stream-id 0|1]

# Move a PTZ camera
cargo run -- ptz <DEVICE_ID> <DIRECTION> [--channel-id <ID>] [--duration-ms <MS>]
# DIRECTION: up | down | left | right | upper-left | bottom-left
#          | upper-right | bottom-right | zoom-in | zoom-out | stop

# Watch all cameras for motion, log events, and record local clips, until Ctrl+C
cargo run -- watch [--interval-secs 30] [--events-file motion_events.jsonl]
                    [--clips-dir clips] [--buffer-dir .imou_ring_buffer]
                    [--pre-roll-secs 30] [--post-roll-secs 60]

# Same, but push-driven instead of polling — needs a public HTTPS URL
cargo run -- listen https://your-public-domain/imou-callback
                     [--listen-addr 0.0.0.0:8787] [--max-push-latency-secs 300]
                     [other flags same as watch]
```

Run `cargo run -- --help` or `cargo run -- <command> --help` for full option
details.

> **Note:** PTZ commands physically move a real camera — there's no
> simulation mode.

### `watch`

Polls every camera bound to the account every `--interval-secs` (default 30)
and appends one JSON line per detected event to `--events-file` (default
`motion_events.jsonl`), e.g.:

```json
{"time":"2026-08-17T16:15:04+00:00","device_id":"EXAMPLE0DEVICEID","channel_id":"0","channel_name":"frontdoor","alarm_id":"1171910261101032336","alarm_name":"frontdoor","alarm_type":"34500","label_type":"humanAlarm"}
```

It only reports events from the moment it starts — it doesn't replay
account history. Imou also offers a push/webhook mechanism
(`setMessageCallback`) as an alternative to polling; it does work with a
real, already-live domain, but took about 48 minutes to start actually
delivering events in testing (far more than the docs' stated "~5 minutes"),
and its real payload shape doesn't match what's documented (see CLAUDE.md).
Polling is what's implemented here — its latency is predictable
(`--interval-secs`) and it has no public-reachability requirement.

**Local clip recording**: for any channel with `CAM_<NAME>_IP` /
`CAM_<NAME>_SECURE` set in `.env`, `watch` also connects directly to that
camera's RTSP stream on the LAN (verified live — no Imou cloud API involved
for the video) and keeps a rolling local recording buffer. On each detected
event it saves a `clips/<channel_name>/<timestamp>_<alarm_id>.mp4` covering
`--pre-roll-secs` before the event through `--post-roll-secs` after —
requires `ffmpeg` on `PATH`. Channels without local credentials configured
just get the JSONL log entry, same as before.

### `listen`

Reacts to events as they're pushed — no polling. Same JSONL log format and
local clip recording as `watch` (identical `--clips-dir`/`--pre-roll-secs`/
`--post-roll-secs` flags). `callback_url` must be an HTTPS URL, already
publicly reachable when `listen` starts, ending in `/imou-callback` (fixed
path) — **`listen` does not call `setMessageCallback` itself**, register it
manually via the Imou console before starting (an automatic call on every
startup was found to reset the account's "IoT Device Message" push
subscription, which real events depend on; see CLAUDE.md).

Confirmed live that push does eventually deliver real events — but a fresh
registration took **~48 minutes** to start delivering, far more than the
docs' stated "~5 minutes", and the real payload doesn't match what's
documented (`msgType: "iotEvent"`, not `"videoMotion"`; see CLAUDE.md for
the full list of differences). `--max-push-latency-secs` (default 300)
sizes the local recording buffer's retention against this uncertainty — a
channel's pre-roll can only be recovered if the buffer is still holding it
by the time a delayed push actually arrives.

For production deployment (Docker, behind Caddy, video/logs on mapped host
directories), see [`deploy/README.md`](./deploy/README.md).

## Development

```sh
cargo build              # build
cargo clippy --all-targets   # lint
```

See [CLAUDE.md](./CLAUDE.md) for architecture notes and guidance on adding
new API endpoints.

## License

[MIT](./LICENSE)
