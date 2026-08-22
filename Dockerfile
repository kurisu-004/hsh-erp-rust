# syntax=docker/dockerfile:1.7
# hsh-erp-rust 生产镜像：musl 静态编译 → alpine 运行时
#
# 单独构建：  DOCKER_BUILDKIT=1 docker build -t hsh-erp-rust:local .
# 编排构建：  cd /Users/ren/Code && docker compose build rust-backend

ARG RUST_VERSION=1.97
ARG ALPINE_VERSION=3.22

# ---------- 阶段 1：构建 ----------
FROM rust:${RUST_VERSION}-alpine AS builder

# Cargo.lock 里 openssl/native-tls/rustls/ring/aws-lc 全部 0 命中，
# 只装 musl-dev。
RUN apk add --no-cache musl-dev

ENV SQLX_OFFLINE=true \
    CARGO_TERM_COLOR=never \
    CARGO_INCREMENTAL=0 \
    RUSTFLAGS="-C strip=symbols"

WORKDIR /build

# ---- 依赖层：清单 + 桩源码 ----
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src \
    && echo 'fn main() {}' > src/main.rs \
    && : > src/lib.rs
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    cargo build --release --locked

# ---- 真实源码（.sqlx 与 migrations 都是编译期宏的输入）----
COPY .sqlx ./.sqlx
COPY migrations ./migrations
COPY src ./src

# 关键：必须 touch 全部 .rs，cargo 的 mtime 缓存才不会复用空 lib.rs
RUN find src -name '*.rs' -exec touch {} + \
    && rm -f target/release/hsh-erp-rust

RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    cargo build --release --locked --bin hsh-erp-rust

# ---------- 阶段 2：运行时 ----------
FROM alpine:${ALPINE_VERSION} AS runtime

RUN apk add --no-cache tini tzdata ca-certificates

ENV TZ=Asia/Shanghai \
    LISTEN_ADDR=0.0.0.0:3000 \
    RUST_LOG=info,sqlx=warn,tower_http=info
RUN ln -snf /usr/share/zoneinfo/$TZ /etc/localtime && echo $TZ > /etc/timezone

RUN addgroup -g 1000 -S app \
    && adduser -u 1000 -S -G app -h /app app

WORKDIR /app
COPY --from=builder --chown=app:app /build/target/release/hsh-erp-rust /app/hsh-erp-rust
USER app

EXPOSE 3000

HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
  CMD wget -q -O /dev/null http://127.0.0.1:3000/api/v2/health || exit 1

ENTRYPOINT ["/sbin/tini", "--"]
CMD ["/app/hsh-erp-rust"]
