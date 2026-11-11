# Agent Deck 推送采集器（Rust）的 Linux 镜像。构建上下文为仓库根目录（需要 avatars/roster.json 内嵌名字表）。
FROM rust:1.97-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY avatars/roster.json avatars/roster.json
COPY collector collector
RUN cargo build --release --locked --manifest-path collector/Cargo.toml

# 运行镜像无需 ca-certificates：ureq 的 rustls 使用编译期内嵌的 webpki-roots 根证书
FROM alpine:3.22
COPY --from=build /src/collector/target/release/deck-push /usr/local/bin/deck-push
ENTRYPOINT ["/usr/local/bin/deck-push"]
