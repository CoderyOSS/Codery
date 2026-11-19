# GitAuthGate image (GATE-009): single static musl binary; no git, no shell.
# Alpine builder: musl is the native libc (no cross toolchain needed) and
# the x86_64-unknown-linux-musl target defaults to crt-static, so the
# result runs on scratch.
FROM rust:1-alpine AS build
WORKDIR /src
# musl-dev: ring/sqlite compile C via the native cc.
# ca-certificates: runtime trust bundle copied into the scratch stage.
RUN apk add --no-cache musl-dev ca-certificates
# Manifests first so the dependency layers cache; no cargo config or
# toolchain pin is committed — the image uses its own default toolchain.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
    && echo 'fn main() {}' > src/main.rs \
    && cargo build --release --target x86_64-unknown-linux-musl
COPY src ./src
RUN touch src/main.rs \
    && cargo build --release --target x86_64-unknown-linux-musl \
    && mkdir -p /data \
    && chown 65532:65532 /data

# Distroless-style runtime: static binary + CA certs only.
# /data is baked empty but gate-owned (65532): a fresh named volume seeds
# from image content on first mount, ownership included — no manual
# volume chown step at deploy time.
FROM scratch
COPY --from=build /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/
COPY --from=build /src/target/x86_64-unknown-linux-musl/release/gate /usr/local/bin/gate
COPY --from=build /data /data
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/gate"]
