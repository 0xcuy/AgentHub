# --- build stage ---
FROM rust:1.93-slim-bookworm AS build
WORKDIR /build
# Dependencies first for layer caching
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY migrations ./migrations
COPY static ./static
RUN cargo build --release

# --- runtime stage ---
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates sqlite3 curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --create-home --home-dir /data aghub
COPY --from=build /build/target/release/aghub /usr/local/bin/aghub
# Migrations and the static UI are compiled into the binary (include_str!).
USER aghub
ENV AGHUB_DB=/data/aghub.db \
    PORT=3000
EXPOSE 3000
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD curl -fsS http://127.0.0.1:3000/healthz || exit 1
ENTRYPOINT ["/usr/local/bin/aghub"]
