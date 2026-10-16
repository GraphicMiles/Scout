# Scout (Rust) — multi-stage build.
#
# NOTE: this image was written but NOT built or run: the authoring sandbox has no
# Docker daemon, so nothing here has been verified end to end. Treat the first
# `docker build` as an untested step and report what it says.

FROM rust:1.82-slim-bookworm AS build
WORKDIR /src

# Dependency layer: compile the tree once with a stub main so that a source-only
# change does not re-download and re-compile every crate.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src \
 && printf 'fn main() {}\n' > src/main.rs \
 && cargo build --release --locked \
 && rm -rf src

COPY src ./src
COPY tests ./tests
RUN touch src/main.rs && cargo build --release --locked

FROM debian:bookworm-slim AS runtime
# rustls needs a trust store; the binary itself has no OpenSSL dependency.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --create-home scout

COPY --from=build /src/target/release/scout /usr/local/bin/scout

USER scout
ENV RUST_LOG=info \
    SCOUT_HOST=0.0.0.0 \
    SCOUT_PORT=8080 \
    SCOUT_MEM_SOFT_MB=300 \
    SCOUT_MEM_HARD_MB=400

EXPOSE 8080

# /health is public and returns 503 while shutting down, so it is a truthful
# liveness+readiness probe. SCOUT_API_KEYS is required: without it, outside
# dev mode, the process refuses to start rather than serving unauthenticated.
HEALTHCHECK --interval=15s --timeout=3s --start-period=5s --retries=3 \
  CMD curl -fsS http://127.0.0.1:8080/health || exit 1

ENTRYPOINT ["/usr/local/bin/scout"]
