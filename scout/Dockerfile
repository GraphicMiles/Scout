# Scout (Rust) — multi-stage build.
#
# The toolchain pin matters: `cargo build --release --locked` needs rustc 1.88+
# because the lock resolves `cookie_store` 0.22.1, `time` 0.3.55 and `icu_*`
# 2.3.0, which declare `rust-version = "1.88"`. The image is built and
# smoke-tested on every push by CI (.github/workflows/ci.yml).

FROM rust:1.88-slim-bookworm AS build
WORKDIR /src

# rquickjs-sys patches the QuickJS sources at build time, so `patch` must be
# present; slim images do not ship it.
RUN apt-get update \
 && apt-get install -y --no-install-recommends patch \
 && rm -rf /var/lib/apt/lists/*

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
# The binary reads HOST and PORT (not SCOUT_HOST/SCOUT_PORT, which earlier
# revisions of this file set by mistake; they were silently ignored).
ENV RUST_LOG=info \
    HOST=0.0.0.0 \
    PORT=8080 \
    SCOUT_MEM_SOFT_MB=300 \
    SCOUT_MEM_HARD_MB=400

EXPOSE 8080

# /health is public and returns 503 while shutting down, so it is a truthful
# liveness+readiness probe. SCOUT_API_KEYS is required: without it, outside
# dev mode, the process refuses to start rather than serving unauthenticated.
HEALTHCHECK --interval=15s --timeout=3s --start-period=5s --retries=3 \
  CMD curl -fsS http://127.0.0.1:8080/health || exit 1

ENTRYPOINT ["/usr/local/bin/scout"]
