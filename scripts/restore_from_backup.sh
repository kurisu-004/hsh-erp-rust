#!/usr/bin/env bash
# 从 db_backup/ 的 .dump 还原数据到本地 dev DB（postgres-dev 容器）
#
# 2026-09-25 sqlx 接管后问题：
#   baseline 合并 schema 时把 dump 老 schema 里的几列删/改名了
#   （t_part.actual_delivery_date 等），裸 pg_restore 会因 COPY 撞缺失列
#   而整张表 rollback = 空表。本脚本自动：
#     1. 补缺失列（nullable + IF NOT EXISTS，幂等）
#     2. 跑 pg_restore --data-only，过滤 owner/privileges/comments
#     3. 过滤已知可忽略错误（alembic_version / t_menu 冲突）
#     4. 统计前后行数，对账
#
# 用法：
#   ./scripts/restore_from_backup.sh                          # 用 db_backup/ 最新 .dump
#   DUMP_FILE=db_backup/foo.dump ./scripts/restore_from_backup.sh
#   RESET=1 ./scripts/restore_from_backup.sh                  # TRUNCATE 已恢复表后重建
#   SKIP_COLUMNS=1 ./scripts/restore_from_backup.sh           # 不补缺失列（schema 已是最新）
#   INCLUDE_MENU=1 ./scripts/restore_from_backup.sh            # 同时灌 dump 里的 t_menu/t_role_menu（默认跳过）
#   POSTGRES_CONTAINER=dev_pg ./scripts/restore_from_backup.sh  # 自定义容器名
#
# 前置：postgres-dev 容器已起（docker compose up -d postgres-dev），
#       baseline migration 已应用（cargo run 一次会自动跑或 sqlx migrate run）。
#
# 已知可忽略错误（脚本已自动吞掉）：
#   - "alembic_version 不存在"        — alembic 是 python 端迁移表，rust 不需要
#   - 其它表的 "duplicate key"        — 上次残留数据，RESET 后干净；非 RESET 模式按现逻辑吞
#   - 缺列错误                         — 本脚本先 ADD COLUMN 再 restore
#   - t_menu duplicate key            — INCLUDE_MENU=1 与 seeds/menu.sql 冲突时按现逻辑吞
#                                       （默认 INCLUDE_MENU=0 不应再出现，menu 数据由 seed 重建）
#
# 关于 t_menu / t_role_menu：
#   seeds/menu.sql 是 t_menu / t_role_menu 的权威源（走 ON CONFLICT (code) DO UPDATE 幂等 upsert）。
#   默认从 dump 跳过这两张表的 DATA 行，restore 后由第 6 步应用 seeds/menu.sql 重建。
#   应急时可用 INCLUDE_MENU=1 灌 dump 原数据。

set -euo pipefail
cd "$(dirname "$0")/.."

# ---------------------------------------------------------------------------- env
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

POSTGRES_CONTAINER="${POSTGRES_CONTAINER:-dev}"

# 容器内 PG 凭据（与 backend-rust/docker-compose.yml 的 POSTGRES_* 默认值对齐）
PG_USER="${POSTGRES_USER:-hsh}"
PG_PASSWORD="${POSTGRES_PASSWORD:-6065161}"
PG_DB="${POSTGRES_DB:-hsh}"

# ---------------------------------------------------------------------------- dump
DUMP_FILE="${DUMP_FILE:-}"
if [ -z "$DUMP_FILE" ]; then
    # 自动选 db_backup/ 下最新的 .dump
    if [ -d db_backup ]; then
        DUMP_FILE="$(ls -t db_backup/*.dump 2>/dev/null | head -1 || true)"
    fi
fi
if [ -z "$DUMP_FILE" ] || [ ! -f "$DUMP_FILE" ]; then
    echo "✗ 找不到 .dump 文件（DUMP_FILE=$DUMP_FILE，db_backup/ 也无 .dump）" >&2
    exit 1
fi
echo "→ dump: $DUMP_FILE ($(du -h "$DUMP_FILE" | cut -f1))"

# ---------------------------------------------------------------------------- 容器检查
if ! command -v docker >/dev/null 2>&1; then
    echo "✗ docker 命令不存在" >&2
    exit 1
fi
if ! docker ps --format '{{.Names}}' | grep -qx "$POSTGRES_CONTAINER"; then
    echo "✗ 容器 $POSTGRES_CONTAINER 没在跑（docker compose up -d postgres-dev）" >&2
    exit 1
fi

docker exec -e PGPASSWORD="$PG_PASSWORD" "$POSTGRES_CONTAINER" \
    psql -U "$PG_USER" -d "$PG_DB" -tAc "SELECT 1" >/dev/null

# baseline 检查：t_user 必须存在（baseline 创建的核心表）
if ! docker exec -e PGPASSWORD="$PG_PASSWORD" "$POSTGRES_CONTAINER" \
    psql -U "$PG_USER" -d "$PG_DB" -tAc \
    "SELECT 1 FROM information_schema.tables WHERE table_schema='public' AND table_name='t_user'" \
    | grep -q 1; then
    echo "✗ $PG_DB 还没建表（baseline migration 未跑，先 cargo run 或 sqlx migrate run）" >&2
    exit 1
fi

# 已恢复检测：关键表非空 → 大概率是再次运行
HAS_DATA=$(docker exec -e PGPASSWORD="$PG_PASSWORD" "$POSTGRES_CONTAINER" \
    psql -U "$PG_USER" -d "$PG_DB" -tAc \
    "SELECT (SELECT count(*) FROM t_user) + (SELECT count(*) FROM t_part) + (SELECT count(*) FROM t_customer)")
if [ "${HAS_DATA:-0}" -gt 0 ] && [ "${RESET:-0}" != "1" ]; then
    echo "⚠ 检测到 DB 已有数据（t_user + t_part + t_customer 共 $HAS_DATA 行）"
    echo "  → 想重新灌一遍：RESET=1 $0"
    echo "  → 想跳过已存在的表：脚本会自动忽略 duplicate key 错误，仅恢复空表"
fi

# ---------------------------------------------------------------------------- 1) 补缺失列
if [ "${SKIP_COLUMNS:-0}" != "1" ]; then
    echo "→ 补 baseline 与 dump 之间的 schema 漂移列（IF NOT EXISTS，幂等）"

    ALTER_SQL=$(cat <<'SQL'
-- 2026-09-25 sqlx 接管：baseline 把 dump 老 schema 里的几列删/改名了。
-- 这里只 ADD（nullable），不破坏 baseline 契约；新增列不进任何业务查询。
ALTER TABLE t_assembly ADD COLUMN IF NOT EXISTS actual_delivery_date date;
ALTER TABLE t_part ADD COLUMN IF NOT EXISTS actual_delivery_date date;
ALTER TABLE t_part ADD COLUMN IF NOT EXISTS location varchar(20);
ALTER TABLE t_part ADD COLUMN IF NOT EXISTS current_holder_id bigint;
ALTER TABLE t_part ADD COLUMN IF NOT EXISTS placed_at timestamp;
ALTER TABLE t_part ADD COLUMN IF NOT EXISTS delivery_note_id bigint;
ALTER TABLE t_part ADD COLUMN IF NOT EXISTS has_been_repaired boolean;
ALTER TABLE t_part_batch ADD COLUMN IF NOT EXISTS next_process_id bigint;
ALTER TABLE t_part_batch ADD COLUMN IF NOT EXISTS placed_at timestamp;
ALTER TABLE t_part_batch ADD COLUMN IF NOT EXISTS has_been_repaired boolean;
SQL
)

    echo "$ALTER_SQL" | docker exec -i -e PGPASSWORD="$PG_PASSWORD" "$POSTGRES_CONTAINER" \
        psql -U "$PG_USER" -d "$PG_DB" --set ON_ERROR_STOP=1 -v ON_ERROR_STOP=1 >/dev/null
fi

# ---------------------------------------------------------------------------- 2) RESET（可选）
if [ "${RESET:-0}" = "1" ]; then
    echo "→ RESET=1：TRUNCATE 待恢复的表后重建"
    RESET_TABLES=(t_user t_role_menu t_user_role t_customer t_applicant t_delivery_note
                  t_delivery_group t_delivery_group_member t_delivery_note_event
                  t_delivery_note_counter t_process t_work_type t_work_type_process
                  t_worker t_shelf t_shelf_process t_part t_part_batch t_part_event
                  t_part_file t_assembly t_drawing_file t_cnc_program t_outsource_company
                  t_outsource_company_process t_outsource_quote t_outsource_quote_event
                  t_outsource_shipment)
    TBL_LIST=$(printf '%s,' "${RESET_TABLES[@]}")
    TBL_LIST="${TBL_LIST%,}"
    docker exec -e PGPASSWORD="$PG_PASSWORD" "$POSTGRES_CONTAINER" \
        psql -U "$PG_USER" -d "$PG_DB" -c "TRUNCATE ${TBL_LIST} RESTART IDENTITY CASCADE" >/dev/null
fi

# ---------------------------------------------------------------------------- 3) 取 baseline 行数（对账用）
BASELINE_COUNTS=$(docker exec -e PGPASSWORD="$PG_PASSWORD" "$POSTGRES_CONTAINER" \
    psql -U "$PG_USER" -d "$PG_DB" -tAc "
SELECT 't_user='||count(*) FROM t_user
UNION ALL SELECT 't_role_menu='||count(*) FROM t_role_menu
UNION ALL SELECT 't_user_role='||count(*) FROM t_user_role
UNION ALL SELECT 't_customer='||count(*) FROM t_customer
UNION ALL SELECT 't_applicant='||count(*) FROM t_applicant
UNION ALL SELECT 't_delivery_note='||count(*) FROM t_delivery_note
UNION ALL SELECT 't_process='||count(*) FROM t_process
UNION ALL SELECT 't_work_type='||count(*) FROM t_work_type
UNION ALL SELECT 't_worker='||count(*) FROM t_worker
UNION ALL SELECT 't_shelf='||count(*) FROM t_shelf
UNION ALL SELECT 't_part='||count(*) FROM t_part
UNION ALL SELECT 't_part_batch='||count(*) FROM t_part_batch
UNION ALL SELECT 't_assembly='||count(*) FROM t_assembly
UNION ALL SELECT 't_part_event='||count(*) FROM t_part_event
UNION ALL SELECT 't_part_file='||count(*) FROM t_part_file
UNION ALL SELECT 't_menu='||count(*) FROM t_menu;")

echo "→ 恢复前："
echo "$BASELINE_COUNTS" | sed 's/^/    /'

# ---------------------------------------------------------------------------- 4) pg_restore
echo "→ pg_restore --data-only --no-owner --no-privileges ..."

# 把 dump 拷进容器（容器内 pg_restore 才能直接读）
DUMP_BASENAME="$(basename "$DUMP_FILE")"
docker cp "$DUMP_FILE" "$POSTGRES_CONTAINER:/tmp/$DUMP_BASENAME"

# 2026-09-30 新增：默认跳过 t_menu / t_role_menu 的 DATA 行（seeds/menu.sql 是权威源）。
# pg_restore -l 输出格式示例：
#   4030; 0 16401 TABLE DATA public t_menu postgres
# 这里精确匹配 `TABLE DATA public <table>` 行，注释行以 `;` 开头不会被误删。
TOC_LIST_HOST="$(mktemp)"
docker exec -e PGPASSWORD="$PG_PASSWORD" "$POSTGRES_CONTAINER" \
    pg_restore -l "/tmp/$DUMP_BASENAME" > "$TOC_LIST_HOST"

if [ "${INCLUDE_MENU:-0}" != "1" ]; then
    grep -vE ' TABLE DATA public (t_menu|t_role_menu) ' "$TOC_LIST_HOST" \
        > "${TOC_LIST_HOST}.filtered"
    mv "${TOC_LIST_HOST}.filtered" "$TOC_LIST_HOST"
    echo "→ 默认跳过 t_menu / t_role_menu DATA 行（seeds/menu.sql 会重建；INCLUDE_MENU=1 恢复 dump 数据）"
else
    echo "→ INCLUDE_MENU=1：同时灌 dump 里的 t_menu / t_role_menu（可能与 seeds/menu.sql 冲突）"
fi

# 过滤后的 TOC 列表拷进容器，pg_restore -L 应用
docker cp "$TOC_LIST_HOST" "$POSTGRES_CONTAINER:/tmp/toc.list"

# 跑 restore，过滤已知可忽略错误
RESTORE_LOG="$(mktemp)"
set +e
docker exec -e PGPASSWORD="$PG_PASSWORD" "$POSTGRES_CONTAINER" pg_restore \
    --data-only \
    --no-owner \
    --no-privileges \
    --no-comments \
    --no-publications \
    --no-subscriptions \
    --no-security-labels \
    -L /tmp/toc.list \
    -h localhost -U "$PG_USER" -d "$PG_DB" \
    "/tmp/$DUMP_BASENAME" 2>"$RESTORE_LOG"
RESTORE_RC=$?
set -e

# 容器里清理临时 dump / toc.list
docker exec "$POSTGRES_CONTAINER" rm -f "/tmp/$DUMP_BASENAME" "/tmp/toc.list"
# 主机 mktemp 清理
rm -f "$TOC_LIST_HOST"

# 过滤已知错误后剩余的算致命
FILTERED_LOG="$(mktemp)"
grep -Ev \
    -e 'relation "public.alembic_version" does not exist' \
    -e 'COPY failed for table "t_menu".*duplicate key' \
    -e 'COPY failed for table "[^"]+": ERROR:  duplicate key value violates unique constraint' \
    -e '^DETAIL:  Key .* already exists\.$' \
    -e '^CONTEXT:  COPY .*, line [0-9]+$' \
    -e '^Command was: ' \
    -e '^pg_restore: warning: errors ignored on restore: ' \
    "$RESTORE_LOG" > "$FILTERED_LOG" || true

if [ -s "$FILTERED_LOG" ]; then
    echo "✗ pg_restore 出现未预期错误（已过滤 alembic_version / t_menu / duplicate key）：" >&2
    cat "$FILTERED_LOG" >&2
    rm -f "$RESTORE_LOG" "$FILTERED_LOG"
    exit 1
fi

# 把被吞掉的已知错误数算出来给用户看
IGNORED=$(grep -cE \
    -e 'COPY failed for table "[^"]+": ERROR:  duplicate key value violates unique constraint' \
    -e 'relation "public.alembic_version" does not exist' \
    "$RESTORE_LOG" 2>/dev/null || echo 0)
if [ "${IGNORED:-0}" -gt 0 ]; then
    echo "  (ignored ${IGNORED} known errors: alembic_version / duplicate key, see comments)"
fi

rm -f "$RESTORE_LOG" "$FILTERED_LOG"

# ---------------------------------------------------------------------------- 5) 对账
FINAL_COUNTS=$(docker exec -e PGPASSWORD="$PG_PASSWORD" "$POSTGRES_CONTAINER" \
    psql -U "$PG_USER" -d "$PG_DB" -tAc "
SELECT 't_user='||count(*) FROM t_user
UNION ALL SELECT 't_role_menu='||count(*) FROM t_role_menu
UNION ALL SELECT 't_user_role='||count(*) FROM t_user_role
UNION ALL SELECT 't_customer='||count(*) FROM t_customer
UNION ALL SELECT 't_applicant='||count(*) FROM t_applicant
UNION ALL SELECT 't_delivery_note='||count(*) FROM t_delivery_note
UNION ALL SELECT 't_process='||count(*) FROM t_process
UNION ALL SELECT 't_work_type='||count(*) FROM t_work_type
UNION ALL SELECT 't_worker='||count(*) FROM t_worker
UNION ALL SELECT 't_shelf='||count(*) FROM t_shelf
UNION ALL SELECT 't_part='||count(*) FROM t_part
UNION ALL SELECT 't_part_batch='||count(*) FROM t_part_batch
UNION ALL SELECT 't_assembly='||count(*) FROM t_assembly
UNION ALL SELECT 't_part_event='||count(*) FROM t_part_event
UNION ALL SELECT 't_part_file='||count(*) FROM t_part_file
UNION ALL SELECT 't_menu='||count(*) FROM t_menu;")

echo "→ 恢复后："
echo "$FINAL_COUNTS" | sed 's/^/    /'

# ---------------------------------------------------------------------------- 6) 补灌 menu seed（idempotent；RESET 之后 seed 也被清了）
# seeds/menu.sql 走 ON CONFLICT (code) DO UPDATE，反复跑无副作用
# 2026-09-30 新增：默认（INCLUDE_MENU=0）情况下 pg_restore 已跳过 t_menu / t_role_menu DATA 行，
# 此时 t_menu / t_role_menu 行数 = 0 + seed 量；INCLUDE_MENU=1 时 seed 的 ON CONFLICT 仍兜底。
if [ "${INCLUDE_MENU:-0}" != "1" ]; then
    echo "→ 跳过 dump 里的 t_menu / t_role_menu（已在上一步 TOC 过滤）；由下方 seeds/menu.sql 重建"
fi
SEED_FILE="${SEED_FILE:-seeds/menu.sql}"
if [ -f "$SEED_FILE" ]; then
    echo "-> apply seed: $SEED_FILE (idempotent)"
    docker exec -i -e PGPASSWORD="$PG_PASSWORD" "$POSTGRES_CONTAINER" \
        psql -U "$PG_USER" -d "$PG_DB" -v ON_ERROR_STOP=1 < "$SEED_FILE" >/dev/null
    FINAL_TMENU=$(docker exec -e PGPASSWORD="$PG_PASSWORD" "$POSTGRES_CONTAINER" \
        psql -U "$PG_USER" -d "$PG_DB" -tAc "SELECT count(*) FROM t_menu")
    FINAL_RMENU=$(docker exec -e PGPASSWORD="$PG_PASSWORD" "$POSTGRES_CONTAINER" \
        psql -U "$PG_USER" -d "$PG_DB" -tAc "SELECT count(*) FROM t_role_menu")
    echo "   t_menu=$FINAL_TMENU, t_role_menu=$FINAL_RMENU"
fi

echo "✓ restore 完成"
