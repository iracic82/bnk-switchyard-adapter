# Builds both binaries; select one with the image entrypoint (`adapter` target is default).
FROM rust:1.99-slim-bookworm AS build
WORKDIR /src
RUN apt-get update && apt-get install -y --no-install-recommends git ca-certificates && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p bnk-switchyard-adapter -p mock-llm \
 && cp target/release/bnk-switchyard-adapter target/release/mock-llm /usr/local/bin/

FROM gcr.io/distroless/cc-debian12:nonroot AS mock-llm
COPY --from=build /usr/local/bin/mock-llm /usr/local/bin/mock-llm
EXPOSE 8000
ENTRYPOINT ["/usr/local/bin/mock-llm"]

FROM gcr.io/distroless/cc-debian12:nonroot AS adapter
COPY --from=build /usr/local/bin/bnk-switchyard-adapter /usr/local/bin/bnk-switchyard-adapter
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/bnk-switchyard-adapter"]
CMD ["/etc/switchyard-adapter/adapter.toml"]
