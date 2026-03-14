FROM rust:1.85-alpine AS builder
RUN apk add --no-cache musl-dev
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
# Cache dependencies
RUN mkdir src && echo 'fn main() {}' > src/main.rs && cargo build --release && rm -rf src
COPY src ./src
RUN touch src/main.rs && cargo build --release

FROM alpine:3.21
RUN apk add --no-cache ca-certificates
COPY --from=builder /app/target/release/upnp-controller /usr/local/bin/upnp-controller
ENTRYPOINT ["/usr/local/bin/upnp-controller"]
