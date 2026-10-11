#!/usr/bin/env bash
# 手工应用 seeds/ 到当前 $DATABASE_URL
#
# 2026-09-25 sqlx 接管后新增：通常 app 启动钩子自动跑 seeds/menu.sql → seeds/rbac.sql，
# 本脚本用于：
#   1. 紧急手动补跑（生产 deployment 出问题）
#   2. 跳过 main.rs 直接 psql 验证
#   3. 远程 ops 在不重启 app 的情况下应用 seed 更新
#
# 用法：
#   ./scripts/seed_apply.sh                              # 用 .env 的 DATABASE_URL，按序跑两个 seed
#   DATABASE_URL=postgres://... ./scripts/seed_apply.sh  # 显式指定
#   ./scripts/seed_apply.sh seeds/rbac.sql               # 只跑指定文件（可传多个，按传入顺序）

set -euo pipefail
cd "$(dirname "$0")/.."

if [ -z "${DATABASE_URL:-}" ]; then
    if [ -f .env ]; then
        # shellcheck disable=SC1091
        set -a; . ./.env; set +a
    fi
fi

if [ -z "${DATABASE_URL:-}" ]; then
    echo "✗ DATABASE_URL 未设置（无 .env 也未通过 env 注入）" >&2
    exit 1
fi

# 默认与启动钩子 run_seeds 同序：先菜单目录（menu.sql）再授权基线（rbac.sql）。
# rbac.sql §5 的授权清理依赖 menu.sql §3 的菜单软删结果，顺序不可颠倒。
SEED_FILES=("$@")
if [ "${#SEED_FILES[@]}" -eq 0 ]; then
    SEED_FILES=(seeds/menu.sql seeds/rbac.sql)
fi

for SEED_FILE in "${SEED_FILES[@]}"; do
    if [ ! -f "$SEED_FILE" ]; then
        echo "✗ seed 文件不存在: $SEED_FILE" >&2
        exit 1
    fi

    echo "→ psql \$DATABASE_URL -v ON_ERROR_STOP=1 -f $SEED_FILE"
    psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -f "$SEED_FILE"
    echo "✓ $SEED_FILE 应用完成"
done