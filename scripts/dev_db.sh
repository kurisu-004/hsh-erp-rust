#!/usr/bin/env bash
# 启动本地开发 PostgreSQL（端口 5433，避开 myERP Python 项目的测试库 5434）
set -euo pipefail
cd "$(dirname "$0")/.."

if ! command -v docker >/dev/null 2>&1; then
  echo "✗ docker 未安装"; exit 1
fi

docker compose up -d postgres

# 等 PG 就绪
for i in $(seq 1 30); do
  if docker compose exec -T postgres pg_isready -U erp -d hsh_erp >/dev/null 2>&1; then
    echo "✓ postgres 已就绪：postgres://erp:erp@localhost:5433/hsh_erp"
    exit 0
  fi
  sleep 1
done

echo "✗ postgres 启动超时"; exit 1