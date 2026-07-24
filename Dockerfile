# pincerbell -- multi-stage build: static musl binary -> minimal runtime.
#
# The runtime stage stays bare Alpine: TLS roots (Mozilla CA set) are
# compiled into the binary via rustls/webpki-roots, so not even
# ca-certificates is needed.

# --- 1. build ----------------------------------------------------------------
FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src

# Dependency layer: compile all crates against a dummy main so this layer is
# cached until Cargo.toml/Cargo.lock change.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
    && echo 'fn main() {}' > src/main.rs \
    && cargo build --release --locked \
    && rm -r src

COPY src ./src
# touch: make cargo notice the real sources are newer than the dummy build.
RUN touch src/main.rs && cargo build --release --locked

# --- 2. runtime --------------------------------------------------------------
FROM alpine:3.21
RUN adduser -S -D -H pincerbell
COPY --from=build /src/target/release/pincerbell /usr/local/bin/pincerbell

USER pincerbell
EXPOSE 8300

# The mounted config MUST set listen = "0.0.0.0:8300" -- the built-in default
# binds 127.0.0.1, which is unreachable from outside the container. Key files
# referenced by the config (service-account JSON, .p8) are mounted next to it.
ENTRYPOINT ["/usr/local/bin/pincerbell"]
CMD ["-c", "/etc/pincerbell/pincerbell.toml"]
