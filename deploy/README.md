# Production deployment

Deploys `imou listen` (push-based motion detection + local clip recording)
as a **standalone** Docker Compose project on a server you control — its
own network, independent of any other compose project on the same host —
fronted by a dedicated Caddy container that terminates TLS with your own
domain's certificate.

`listen`'s `callbackUrl` argument must be an HTTPS URL that's already
publicly reachable when `listen` starts (see the main README/CLAUDE.md for
what's been observed about registration/delivery behavior). That means,
before deploying:
- A domain (or subdomain) pointing at this host's public IP.
- A TLS certificate for it (any ACME-issued cert works — Caddy here is
  configured to use existing cert files directly rather than provisioning
  its own, so it doesn't need port 80).
- Port forwarding on your router/firewall for whatever port you expose
  Caddy on (`8443` by default in `docker-compose.yaml`) to this host.
- **The callback registered manually via the Imou console** (Console →
  message push settings → `setMessageCallback`, pointed at that domain's
  `/imou-callback`) — `listen` no longer calls `setMessageCallback` itself;
  doing so automatically on every startup was found to reset the account's
  "IoT Device Message" push subscription that real events depend on. See
  CLAUDE.md's `listen` section.

## Steps

1. Create the project directory and data subdirectories (bind-mounted, so
   they survive container restarts/recreates):
   ```sh
   mkdir -p ~/imou-cli/data/{clips,snapshots,logs,ring_buffer,ollama}
   touch ~/imou-cli/data/gdrive_token_cache.json
   ```
   The `touch` matters even if you're not using Google Drive upload: Docker
   creates a bind-mounted path that doesn't exist yet as a **directory**,
   which then breaks `gdrive-login` later if you enable it.

2. Copy `deploy/docker-compose.yaml` and `deploy/Caddyfile` into
   `~/imou-cli/`, and edit both: replace `your-domain.example.com` with your
   real domain, and the `callbackUrl` in `docker-compose.yaml`'s `command:`
   with your actual registered callback URL.

3. Create `~/imou-cli/.env` from `deploy/.env.production.example`, filled in
   with real credentials. **Never commit this file** — same rule as the
   repo's own `.env`.

4. Build the image directly on the target host (avoids any
   cross-compilation/architecture mismatch) — from a copy/clone of this repo:
   ```sh
   docker build -t imou-cli:latest .
   ```

5. Start the stack:
   ```sh
   cd ~/imou-cli
   docker compose up -d
   ```

6. **Optional — Google Drive clip upload**: if `GDRIVE_CLIENT_ID`/
   `GDRIVE_CLIENT_SECRET` are set in `.env`, authorize once (the device-code
   flow just prints a URL + code to the terminal — no browser needed on the
   server itself, complete it on your phone or laptop):
   ```sh
   docker compose run --rm imou-cli gdrive-login
   ```
   This writes the refresh token to `./data/gdrive_token_cache.json`
   (bind-mounted into the container — see `docker-compose.yaml`), so it
   survives future `docker compose up -d` recreates and doesn't need to be
   redone on every deploy.

7. **Optional — local AI video analysis**: classifies each extracted clip's
   content (human/animal/vehicle/package/empty) using a local
   vision-language model run by the `imou-ollama` sidecar container
   already started in step 5 — see CLAUDE.md's "AI video-content analysis"
   section for the full design. Set `AI_OLLAMA_URL=http://imou-ollama:11434`
   and `AI_MODEL_NAME=moondream` in `.env`, then pull the model once (first
   pull downloads ~1.7GB, stored in the bind-mounted `./data/ollama`, so
   this survives future recreates):
   ```sh
   docker compose exec imou-ollama ollama pull moondream
   ```
   `moondream` is a small (1.8B), CPU-friendly model chosen for a
   home-server-class deployment; if this host has a GPU and you want
   better accuracy, `AI_MODEL_NAME=llava` is a heavier drop-in alternative
   (`ollama pull llava` instead). Restart `imou-cli` (`docker compose
   restart imou-cli`) after changing `.env` so it picks up the new
   variables. By default, AI results are only logged/published
   (enrichment) — pass `--ai-gate-gdrive-upload`/`--ai-gate-mqtt-analyzed`
   in `docker-compose.yaml`'s `command:` block to also have the AI verdict
   filter what gets uploaded/published.

8. **Optional — continuous recording**: records the entire video stream
   (not just motion clips) into a rolling window, e.g. "last 2 days" — see
   CLAUDE.md's "Continuous recording" section. Costly (tens of GB per
   camera per day), so it's off unless you uncomment both the
   `./data/continuous` volume and the two `--continuous-*` command lines in
   `docker-compose.yaml`. Consider pointing that volume at a larger/external
   disk instead of `./data` if your deployment host doesn't have room —
   `--continuous-retention-hours=0` (the default if the flag is omitted)
   means the feature is off, not "keep forever" like this project's other
   retention flags. Completed segments are ordinary `.mp4` files, directly
   playable from wherever you mount `./data/continuous` (or the external
   disk) — no extra viewer needed.

## Verification

1. `docker compose ps` (from `~/imou-cli`) — both containers `Up`.
2. **Confirm the container can reach your cameras** — if they're on a
   different subnet than the Docker host, that routing has to already work
   at the host level; verify from inside the container itself:
   ```sh
   docker exec imou-cli sh -c 'cat < /dev/null > /dev/tcp/<camera-ip>/554' && echo OK
   ```
3. `docker logs imou-cli` — shows a reminder that the callback must already
   be registered manually (not "registered", since this process no longer
   does that) and, for each channel with local RTSP config, a ring-buffer
   recorder starting.
4. Generate real motion, then check:
   - `~/imou-cli/data/logs/motion_events.jsonl` for a new line.
   - `~/imou-cli/data/clips/<channel>/...mp4` for the clip.
   - `~/imou-cli/data/snapshots/<channel>/...jpg` for the frame-diff
     snapshot (always on, no configuration needed — see CLAUDE.md).
   Don't be alarmed if the very first event after a fresh manual
   registration takes several minutes — Imou's push delivery has a slow
   warm-up after `setMessageCallback` is called (observed ~48 minutes once,
   live — see CLAUDE.md).
5. `docker compose restart imou-cli` — confirm `data/` contents survive the
   restart (bind mounts, not anonymous volumes, so they should); the
   callback registration itself is untouched by this, since `listen` never
   calls `setMessageCallback`.
6. If Google Drive upload is configured: after step 4's real motion event,
   confirm the clip also appears in the target Drive folder. Unlike
   earlier versions, the local `~/imou-cli/data/clips/<channel>/...mp4`
   copy is *not* deleted on a successful upload — local and Drive copies
   now age out independently via `--local-retention-days` and
   `--gdrive-retention-days` respectively (see CLAUDE.md). A `docker
   compose up -d --force-recreate` afterward should not require re-running
   `gdrive-login`, confirming the token cache bind mount actually persists.
7. If AI analysis is configured: after step 4's real motion event, wait for
   the deferred analysis to complete (extraction's post-roll wait plus
   inference time), then confirm `~/imou-cli/data/logs/motion_events.jsonl`
   gained a *second* line for the same `alarm_id` with
   `"kind":"motion_analyzed"`, a `category`, and a `description`. If MQTT
   is also configured, confirm a message arrived on
   `imou/<channel>/motion-analyzed`. If `--ai-gate-gdrive-upload` is set,
   confirm an intentionally empty/irrelevant clip is *not* uploaded to
   Drive while a clip with a person/animal in frame still is.
8. If continuous recording is configured: after `--continuous-segment-minutes`
   has elapsed, confirm `~/imou-cli/data/continuous/<channel>/seg_...mp4`
   exists and plays — the *most recent* file may not open yet (see
   CLAUDE.md: it isn't finalized until it rotates), but any earlier one
   should. Confirm `docker logs imou-cli` printed the "recording continuous
   archive to..." startup line.
