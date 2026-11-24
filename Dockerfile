# syntax=docker/dockerfile:1
FROM rust:1.100-slim-trixie AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked && cp target/release/ai-gateway /ai-gateway
RUN mkdir /data

FROM gcr.io/distroless/cc-debian13:nonroot
COPY --from=build /ai-gateway /usr/local/bin/ai-gateway
# The nonroot user (65532) must own /data, or a named volume starts root-owned and every save fails
COPY --from=build --chown=65532:65532 /data /data
ENV HOST=0.0.0.0 PORT=8080 STATS_PATH=/data/stats.json
VOLUME ["/data"]
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/ai-gateway"]
