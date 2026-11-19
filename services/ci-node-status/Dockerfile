FROM rust:1.100-alpine@sha256:8d0e82abfdf9a38071e07cabc5afba4ddbe077631a80e4e742a760c38beadc33 AS builder

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src

ENV RUSTFLAGS="-C target-feature=+crt-static"
RUN TARGET="$(rustc -vV | awk '/^host:/{print $2}')" \
 && cargo build --release --locked --target "$TARGET" \
 && cp "target/${TARGET}/release/ci-node-status" /ci-node-status

FROM scratch

LABEL org.opencontainers.image.source="https://github.com/Bafff/github-runners"
LABEL org.opencontainers.image.description="Read-only HTTP status service for the 5950x ARC CI node"

COPY --from=builder /ci-node-status /ci-node-status

USER 1000:1000
EXPOSE 8080
ENTRYPOINT ["/ci-node-status"]
