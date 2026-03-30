FROM rust:1.90-alpine AS builder
RUN apk add --no-cache musl-dev openssl-dev openssl-libs-static perl
WORKDIR /app
COPY . .
RUN cargo build --release --target x86_64-unknown-linux-musl

FROM scratch
COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/
COPY --from=builder /app/target/x86_64-unknown-linux-musl/release/gitea-ci-autoscaler /
ENTRYPOINT ["/gitea-ci-autoscaler"]
