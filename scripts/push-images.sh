#!/usr/bin/env bash
# ----------------------------------------------------------------------------
# 本地构建并推送 hsh-erp-rust（v2 Axum）镜像到腾讯云 TCR
#
# 用法:
#   ./scripts/push-images.sh                        # 推 prod- 前缀,打 SHA + latest
#   TAG_PREFIX=staging ./scripts/push-images.sh
#   DOCKER_REPO=ccr.ccs.tencentyun.com/foo ./scripts/push-images.sh
#   VERSION=0.1.0 ./scripts/push-images.sh          # 发布版:只打 0.1.0 tag（不打 SHA / latest）
#
# 凭据: 自动从 .env 读取 DOCKER_USERNAME / DOCKER_PASSWORD / DOCKER_REPO
#       也可以在 shell 里 export 覆盖
#
# Dockerfile 在本仓根（rust:1.97-alpine builder → alpine:3.22 runtime），
# 多阶段构建；.sqlx/ 已提交，build 时 SQLX_OFFLINE=true 不需 live DB。
# ----------------------------------------------------------------------------
set -euo pipefail

cd "$(dirname "$0")/.."

# 载入 .env（如果存在）—— 把所有 K=V 导出到当前 shell
if [ -f .env ]; then
  set -a
  # shellcheck disable=SC1091
  . .env
  set +a
fi

: "${DOCKER_USERNAME:?DOCKER_USERNAME 未设置（在 .env 里配或 export）}"
: "${DOCKER_PASSWORD:?DOCKER_PASSWORD 未设置（在 .env 里配或 export）}"
: "${DOCKER_REPO:?DOCKER_REPO 未设置（默认 ccr.ccs.tencentyun.com/hsh-erp）}"

TAG_PREFIX="${TAG_PREFIX:-prod}"
SHA="$(git rev-parse --short HEAD 2>/dev/null || echo 'local')"
REGISTRY_HOST="$(echo "$DOCKER_REPO" | cut -d/ -f1)"

# 发布模式:VERSION 设置时只打 VERSION 一个 tag（无 SHA / latest）。
# 默认模式:打 SHA + latest 两个 tag,便于滚动部署与回滚。
if [ -n "${VERSION:-}" ]; then
  TAGS=("$VERSION")
  echo "==> 仓库: $DOCKER_REPO"
  echo "==> 标签前缀: $TAG_PREFIX  (镜像: $TAG_PREFIX-rust-backend)"
  echo "==> 发布版本: $VERSION  (Git SHA: $SHA, 只打 $VERSION tag)"
else
  TAGS=("$SHA" "latest")
  echo "==> 仓库: $DOCKER_REPO"
  echo "==> 标签前缀: $TAG_PREFIX  (镜像: $TAG_PREFIX-rust-backend)"
  echo "==> Git SHA: $SHA  (打 SHA + latest 两个 tag)"
fi
echo

echo "==> 1/3 登录 TCR ($REGISTRY_HOST)"
echo "$DOCKER_PASSWORD" | docker login "$REGISTRY_HOST" -u "$DOCKER_USERNAME" --password-stdin

# 把所有 tag 拼成 docker build 的 -t 参数
RUST_TAG_ARGS=()
for t in "${TAGS[@]}"; do
  RUST_TAG_ARGS+=("-t" "$DOCKER_REPO/$TAG_PREFIX-rust-backend:$t")
done

echo
echo "==> 2/3 构建 rust-backend ($TAG_PREFIX-rust-backend:${TAGS[*]})"
# --platform linux/amd64：Mac Apple Silicon 本地 build 默认出 arm64，
# CVM 是 amd64，必须显式指定。push 之后 CVM 才能正常拉取。
# context = .  （rust 仓库根，Dockerfile 在根目录）
docker build \
  --platform linux/amd64 \
  "${RUST_TAG_ARGS[@]}" \
  --label "org.opencontainers.image.revision=$SHA" \
  --label "org.opencontainers.image.source=$(git config --get remote.origin.url 2>/dev/null || echo 'local')" \
  .

echo
echo "==> 3/3 推送 tag 到 TCR"
for t in "${TAGS[@]}"; do
  docker push "$DOCKER_REPO/$TAG_PREFIX-rust-backend:$t"
done

echo
echo "==> 登出（清理凭据）"
docker logout "$REGISTRY_HOST" >/dev/null

echo
echo "✓ 推送完成"
for t in "${TAGS[@]}"; do
  echo "  - $DOCKER_REPO/$TAG_PREFIX-rust-backend:$t"
done
