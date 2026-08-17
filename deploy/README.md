# Production deployment

Deploys `imou listen` (push-based motion detection + local clip recording)
as a **standalone** Docker Compose project on a server you control — its
own network, independent of any other compose project on the same host —
fronted by a dedicated Caddy container that terminates TLS with your own
domain's certificate.

`setMessageCallback`'s `callbackUrl` must be an HTTPS URL that's already
publicly reachable when `listen` starts (see the main README/CLAUDE.md for
what's been observed about registration/delivery behavior). That means,
before deploying:
- A domain (or subdomain) pointing at this host's public IP.
- A TLS certificate for it (any ACME-issued cert works — Caddy here is
  configured to use existing cert files directly rather than provisioning
  its own, so it doesn't need port 80).
- Port forwarding on your router/firewall for whatever port you expose
  Caddy on (`8443` by default in `docker-compose.yaml`) to this host.

## Steps

1. Create the project directory and data subdirectories (bind-mounted, so
   they survive container restarts/recreates):
   ```sh
   mkdir -p ~/imou-cli/data/{clips,logs,ring_buffer}
   ```

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

## Verification

1. `docker compose ps` (from `~/imou-cli`) — both containers `Up`.
2. **Confirm the container can reach your cameras** — if they're on a
   different subnet than the Docker host, that routing has to already work
   at the host level; verify from inside the container itself:
   ```sh
   docker exec imou-cli sh -c 'cat < /dev/null > /dev/tcp/<camera-ip>/554' && echo OK
   ```
3. `docker logs imou-cli` — shows the push callback registered and, for
   each channel with local RTSP config, a ring-buffer recorder starting.
4. Generate real motion, then check:
   - `~/imou-cli/data/logs/motion_events.jsonl` for a new line.
   - `~/imou-cli/data/clips/<channel>/...mp4` for the clip.
   Don't be alarmed if the very first event after a fresh registration
   takes several minutes — Imou's push delivery has a slow warm-up after
   `setMessageCallback` is called (observed ~48 minutes once, live — see
   CLAUDE.md).
5. `docker compose restart imou-cli` — confirm the callback re-registers
   cleanly and `data/` contents survive the restart (bind mounts, not
   anonymous volumes, so they should).
