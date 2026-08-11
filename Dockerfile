# --- 构建阶段 ---
FROM rust:1.83-alpine AS builder

RUN apk add --no-cache musl-dev pkgconfig openssl-dev

WORKDIR /build

# 利用层缓存：先复制清单
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs && echo "" > src/lib.rs \
    && cargo build --release --bin hsh-erp-rust || true

# 复制真实源码
COPY . .
RUN touch src/main.rs \
    && SQLX_OFFLINE=true cargo build --release --bin hsh-erp-rust

# --- 运行阶段 ---
FROM alpine:3.20

RUN apk add --no-cache tini tzdata ca-certificates
ENV TZ=Asia/Shanghai

WORKDIR /app
COPY --from=builder /build/target/release/hsh-erp-rust /app/hsh-erp-rust

# 创建非 root 运行账号
RUN addgroup -S app && adduser -S app -G app && chown -R app:app /app
USER app

EXPOSE 3000
ENTRYPOINT ["/sbin/tini", "--", "/app/hsh-erp-rust"]