# Pinned to the same Debian release as the runtime stage below — a newer
# builder (e.g. plain `rust:1-slim`, which tracks whatever Debian release is
# current) links against a glibc the older runtime image can't satisfy
# ("GLIBC_2.38/2.39 not found"), hit live deploying this.
FROM rust:1-slim-bookworm AS builder
WORKDIR /app

# Cache dependency compilation separately from source changes.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release \
    && rm -rf src

COPY src ./src
RUN touch src/main.rs && cargo build --release

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ffmpeg ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/imou-cli /usr/local/bin/imou-cli
WORKDIR /app
ENTRYPOINT ["imou-cli"]
