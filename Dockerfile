FROM rust:alpine AS builder
RUN apk add --no-cache musl-dev
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src && echo "fn main() {}" > src/main.rs \
    && cargo build --release \
    && rm -rf src target/release/deps/synology_thumbgen* target/release/synology_photos_thumbgen* target/release/synology-thumbgen*
COPY src ./src
RUN rm -rf target/release/.fingerprint/synology* target/release/deps/synology* target/release/synology* \
    && touch src/main.rs \
    && cargo build --release --locked

FROM alpine:latest
RUN apk add --no-cache ffmpeg libheif-tools tzdata ca-certificates
COPY --from=builder /build/target/release/synology-thumbgen /usr/local/bin/synology-thumbgen
ENV HOME=/tmp
ENTRYPOINT ["synology-thumbgen"]
CMD ["daemon"]
