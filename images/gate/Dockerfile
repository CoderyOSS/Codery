# GitAuthGate image (GATE-009): single static musl binary; no git, no shell.
FROM rust:1-slim AS build
WORKDIR /src
RUN rustup target add x86_64-unknown-linux-musl
# Manifests first so the dependency layers cache; no cargo config or
# toolchain pin is committed — the image uses its own default toolchain.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
    && echo 'fn main() {}' > src/main.rs \
    && cargo build --release --target x86_64-unknown-linux-musl
COPY src ./src
RUN touch src/main.rs \
    && cargo build --release --target x86_64-unknown-linux-musl

# Distroless-style runtime: static binary + CA certs only.
FROM scratch
COPY --from=build /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/
COPY --from=build /src/target/x86_64-unknown-linux-musl/release/gate /usr/local/bin/gate
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/gate"]
