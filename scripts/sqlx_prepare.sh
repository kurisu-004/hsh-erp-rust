#!/usr/bin/env bash
# 生成 sqlx 离线查询元数据
#
# 前置：
#   1. DATABASE_URL 已设置且指向开发库（参见 .env.example）
#   2. 已执行 migrations（业务实现阶段起效）
#
# 输出：.sqlx/query-*.json（须入版本库）

set -euo pipefail
cd "$(dirname "$0")/.."

if [ -z "${DATABASE_URL:-}" ]; then
  if [ -f .env ]; then
    # shellcheck disable=SC1091
    set -a; . ./.env; set +a
  fi
fi

if [ -z "${DATABASE_URL:-}" ]; then
  echo "✗ DATABASE_URL 未设置"; exit 1
fi

echo "→ cargo sqlx prepare -- --lib"
cargo sqlx prepare -- --lib
echo "✓ .sqlx/query-*.json 已生成"