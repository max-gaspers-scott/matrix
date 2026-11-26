# syntax=docker/dockerfile:1
# Build: docker compose build bridge   (compose supplies the `shared` context)
FROM rust:1-slim-trixie AS build
RUN apt-get update && apt-get install -y --no-install-recommends cmake clang pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*
# Cargo.toml points at the shared crate by absolute path; recreate that path.
COPY --from=shared . /home/mgs/bens_chat/shared
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY migrations migrations
COPY src src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked && cp target/release/bc-bridge /bc-bridge

FROM gcr.io/distroless/cc-debian13:nonroot
COPY --from=build /bc-bridge /bc-bridge
WORKDIR /data
ENV DATABASE_URL=sqlite:///data/bridge.db
EXPOSE 29400
ENTRYPOINT ["/bc-bridge"]
CMD ["run"]
