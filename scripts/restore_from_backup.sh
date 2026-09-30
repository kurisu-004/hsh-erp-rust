#!/usr/bin/env bash
# 从 db_backup/ 的 .dump 还原数据到本地 dev DB（postgres-dev 容器）
#
# ============================ 设计原则（2026-10-01 重写）============================
#   **restore 绝不修改 public schema。**
#
#   2026-09-25 sqlx 接管后的老做法：dump 是 Python 端老 schema 的快照，与 baseline
#   有列漂移（027 删过 t_part 的 6 个批次依附列 / 028 删过 t_part_batch 的 2 列 /
#   004 改了列语义），于是脚本先 `ADD COLUMN IF NOT EXISTS` 把老列补回 public，
#   再 `pg_restore --data-only`。数据是灌进去了，但那 10 个已废弃的列从此永久留在
#   schema 里 —— 这就是「重建后字段又恢复了」的根因（业务代码早已不读它们，
#   全仓只剩注释提及）。
#
#   现在改为**数据投影**：dump 独有的历史列先进临时 schema `restore_stage`
#   （用 dump 自己的 DDL 原样建表），再按列投影 `INSERT ... SELECT` 回 public；
#   public 全程零 DDL。已删除的列不再复活，需要语义延续的走显式 rename 映射。
#
# ============================ 配套保障 ============================================
#   1. REBUILD_SCHEMA=1（默认）→ `DROP SCHEMA public CASCADE` + `sqlx migrate run`，
#      目标 schema 严格等于 migrations HEAD（顺带补上从未应用的 004）。
#   2. 漂移自动检测 + 白名单：dump 里出现「未登记的历史列」→ 硬失败，防静默丢数据。
#   3. 灌数据前后对 information_schema 做快照 diff → 断言 restore 零 schema 变更。
#   4. `_sqlx_migrations` / `alembic_version` 从 TOC 排除：dump 里带的是 Python 端
#      旧版本迁移记录，灌进来会让 sqlx 报 VersionMissing，之后 `cargo run` 起不来。
#   5. stage 表逐张对账：灌入行数必须等于 INSERT 后 public 的增量。
#
# ============================ 用法 =================================================
#   ./scripts/restore_from_backup.sh                              # 重建 schema + 灌数据
#   DRY_RUN=1 ./scripts/restore_from_backup.sh                    # 只打印漂移决策表
#   REBUILD_SCHEMA=0 RESET=1 ./scripts/restore_from_backup.sh     # 原地模式（只灌数据）
#   INCLUDE_MENU=1 ./scripts/restore_from_backup.sh               # 同时灌 dump 的 t_menu
#   ALLOW_UNKNOWN_DROP=1 ./scripts/restore_from_backup.sh         # 放行未登记历史列（会丢数据）
#   DUMP_FILE=db_backup/foo.dump ./scripts/restore_from_backup.sh
#   POSTGRES_CONTAINER=hsh-restore-test \
#   RESTORE_DATABASE_URL=postgres://hsh:6065161@localhost:5433/hsh \
#       ./scripts/restore_from_backup.sh                         # 非默认端口的验证容器
#
# 前置：目标容器已起；REBUILD_SCHEMA=1 需要 sqlx-cli（brew install sqlx）；
#       REBUILD_SCHEMA=0 需要 public 已是 migrations HEAD 的 schema。
#
# ============================ 已知可忽略错误 ======================================
#   - 其它表的 "duplicate key"  — 上次残留数据，RESET 后干净；仅原地模式容忍
#   - t_menu duplicate key      — 仅 INCLUDE_MENU=1 且与 seeds/menu.sql 冲突时
#   关于 t_menu / t_role_menu：seeds/menu.sql 是权威源（ON CONFLICT (code) DO UPDATE
#   幂等 upsert）。默认从 dump 跳过这两张表的 DATA 行，由第 9 步 seed 重建。
#
# ============================ 显式列映射表（改 schema 契约先改这里）=================
# ① rename 映射（dump 老列 → baseline 新列；老列因此**不算**丢失）
#    格式：<table>|<dump_col>|<target_col>|<出处>
RENAME_MAP='t_part_batch|next_process_id|current_process_id|004：判断批次属于哪道工序池的唯一权威列'

# ② 已删除列白名单（dump 里还在、baseline 已删；允许丢弃，超出白名单即失败）
#    格式：<table>|<dump_col>|<出处>
KNOWN_DROP='t_part|actual_delivery_date|027 part 瘦身：交付日改由 t_part_event DELIVERED 事件派生
t_part|location|027 part 瘦身：位置真相源是 t_part_batch.location
t_part|current_holder_id|027 part 瘦身：持位真相源是 t_part_batch.current_holder_id
t_part|placed_at|027 part 瘦身：上架时间改由 t_part_event 派生
t_part|delivery_note_id|027 part 瘦身：送货单真相源是 t_part_batch.delivery_note_id
t_part|has_been_repaired|027 part 瘦身：拆批后语义已失真
t_part_batch|placed_at|028 batch step 化：placed_at 随 step 化一并移除
t_part_batch|has_been_repaired|027 part 瘦身：拆批后无法确定哪一批返修
t_assembly|actual_delivery_date|027 part 瘦身：装配体交付日改由事件派生'

set -euo pipefail
cd "$(dirname "$0")/.."

# ---------------------------------------------------------------------------- env
if [ -z "${DATABASE_URL:-}" ]; then
    if [ -f .env ]; then
        set -a
        # shellcheck disable=SC1091
        . ./.env
        set +a
    fi
fi
if [ -z "${DATABASE_URL:-}" ]; then
    echo "✗ DATABASE_URL 未设置（无 .env 也未通过 env 注入）" >&2
    exit 1
fi

POSTGRES_CONTAINER="${POSTGRES_CONTAINER:-dev}"

# 容器内 PG 凭据（与 docker-compose.yml 的 POSTGRES_* 默认值对齐）
PG_USER="${POSTGRES_USER:-hsh}"
PG_PASSWORD="${POSTGRES_PASSWORD:-6065161}"
PG_DB="${POSTGRES_DB:-hsh}"

# sqlx migrate 走「主机可达」的 URL：默认沿用 DATABASE_URL 的 host:port，只换凭据与库名。
# 跑在非默认端口的容器上（如验证用 hsh-restore-test:5433）时用 RESTORE_DATABASE_URL 覆盖。
if [ -n "${RESTORE_DATABASE_URL:-}" ]; then
    HOST_DB_URL="$RESTORE_DATABASE_URL"
else
    _sch_cred_host="${DATABASE_URL%/*}"        # postgres://user:pass@host:port
    _hostport="${_sch_cred_host#*://}"         # user:pass@host:port
    _hostport="${_hostport#*@}"                # host:port
    HOST_DB_URL="postgres://${PG_USER}:${PG_PASSWORD}@${_hostport}/${PG_DB}"
fi

INCLUDE_MENU="${INCLUDE_MENU:-0}"
REBUILD_SCHEMA="${REBUILD_SCHEMA:-1}"
DRY_RUN="${DRY_RUN:-0}"
ALLOW_UNKNOWN_DROP="${ALLOW_UNKNOWN_DROP:-0}"

# ---------------------------------------------------------------------------- helpers
# PGOPTIONS 压掉 NOTICE（DROP SCHEMA ... CASCADE 会刷几十行 "drop cascades to ..."）
psql_q() {  # 单条查询（-tA 紧凑输出）
    docker exec -e PGPASSWORD="$PG_PASSWORD" -e PGOPTIONS="-c client_min_messages=warning" \
        "$POSTGRES_CONTAINER" \
        psql -U "$PG_USER" -d "$PG_DB" -v ON_ERROR_STOP=1 -tAc "$1"
}
psql_in() {  # 从 stdin 读 SQL 执行（-c 由调用方追加）
    docker exec -i -e PGPASSWORD="$PG_PASSWORD" -e PGOPTIONS="-c client_min_messages=warning" \
        "$POSTGRES_CONTAINER" \
        psql -U "$PG_USER" -d "$PG_DB" -v ON_ERROR_STOP=1 "$@"
}

csv_has() {  # csv_has <item> <csv>：item 是否在逗号分隔串内
    case ",$2," in
        *",$1,"*) return 0 ;;
        *) return 1 ;;
    esac
}

csv_pick() {  # csv_pick <src csv> <other csv> <keep|drop>
    local src="$1" other="$2" mode="$3" out="" item hit old_ifs="$IFS"
    IFS=','
    # shellcheck disable=SC2086
    for item in $src; do
        if [ -n "$item" ]; then
            if csv_has "$item" "$other"; then hit=1; else hit=0; fi
            if { [ "$mode" = "keep" ] && [ "$hit" = 1 ]; } ||
               { [ "$mode" = "drop" ] && [ "$hit" = 0 ]; }; then
                out="${out:+$out,}$item"
            fi
        fi
    done
    IFS="$old_ifs"
    printf '%s' "$out"
}

rename_target() {  # rename_target <table> <dump_col> → 命中的 target 列名（无则空）
    local a b tgt
    while IFS='|' read -r a b tgt _rest; do
        if [ -n "$a" ] && [ "$a" = "$1" ] && [ "$b" = "$2" ]; then
            printf '%s' "$tgt"
            return 0
        fi
    done <<EOF
$RENAME_MAP
EOF
    return 0
}

known_drop_reason() {  # known_drop_reason <table> <dump_col> → 出处（未登记则空）
    local a b reason
    while IFS='|' read -r a b reason; do
        if [ -n "$a" ] && [ "$a" = "$1" ] && [ "$b" = "$2" ]; then
            printf '%s' "$reason"
            return 0
        fi
    done <<EOF
$KNOWN_DROP
EOF
    return 0
}

classify_table() {  # classify_table <table> <dump cols csv> <target cols csv>
    # 输出：table|common|insert_cols|select_cols|dropped|target_only|unknown
    local tbl="$1" dcols="$2" tcols="$3"
    local common="" insert="" select="" dropped="" newonly="" unknown=""
    local mapped="" mapped_targets=""
    local dc tgt reason old_ifs="$IFS"
    IFS=','
    # shellcheck disable=SC2086
    for dc in $dcols; do
        if [ -n "$dc" ]; then
            if csv_has "$dc" "$tcols"; then
                common="${common:+$common,}$dc"
                insert="${insert:+$insert,}$dc"
                select="${select:+$select,}$dc"
            else
                tgt="$(rename_target "$tbl" "$dc")"
                reason="$(known_drop_reason "$tbl" "$dc")"
                if [ -n "$tgt" ] && csv_has "$tgt" "$tcols"; then
                    insert="${insert:+$insert,}$tgt"
                    select="${select:+$select,}$dc"
                    mapped="${mapped:+$mapped,}${dc}→$tgt"
                    mapped_targets="${mapped_targets:+$mapped_targets,}$tgt"
                elif [ -n "$reason" ]; then
                    dropped="${dropped:+$dropped,}$dc"
                else
                    unknown="${unknown:+$unknown,}$dc"
                fi
            fi
        fi
    done
    IFS="$old_ifs"
    newonly="$(csv_pick "$tcols" "$dcols" drop)"
    if [ -n "$mapped_targets" ]; then
        newonly="$(csv_pick "$newonly" "$mapped_targets" drop)"
    fi
    printf '%s|%s|%s|%s|%s|%s|%s|%s\n' \
        "$tbl" "$common" "$insert" "$select" "$dropped" "$newonly" "$unknown" "$mapped"
}

# ---------------------------------------------------------------------------- dump
DUMP_FILE="${DUMP_FILE:-}"
if [ -z "$DUMP_FILE" ]; then
    if [ -d db_backup ]; then
        DUMP_FILE="$(ls -t db_backup/*.dump 2>/dev/null | head -1 || true)"
    fi
fi
if [ -z "$DUMP_FILE" ] || [ ! -f "$DUMP_FILE" ]; then
    echo "✗ 找不到 .dump 文件（DUMP_FILE=${DUMP_FILE}，db_backup/ 也无 .dump）" >&2
    exit 1
fi
echo "→ dump: $DUMP_FILE ($(du -h "$DUMP_FILE" | cut -f1))"

# ---------------------------------------------------------------------------- 容器检查
if ! command -v docker >/dev/null 2>&1; then
    echo "✗ docker 命令不存在" >&2
    exit 1
fi
if ! docker ps --format '{{.Names}}' | grep -qx "$POSTGRES_CONTAINER"; then
    echo "✗ 容器 $POSTGRES_CONTAINER 没在跑" >&2
    exit 1
fi
docker exec -e PGPASSWORD="$PG_PASSWORD" "$POSTGRES_CONTAINER" \
    psql -U "$PG_USER" -d "$PG_DB" -tAc "SELECT 1" >/dev/null

RESTORE_LOG="$(mktemp)"
TMP_FILES="$RESTORE_LOG"
cleanup() {
    docker exec "$POSTGRES_CONTAINER" rm -f "/tmp/$DUMP_BASENAME" >/dev/null 2>&1 || true
    # shellcheck disable=SC2086
    rm -f $TMP_FILES
}
trap cleanup EXIT

# ---------------------------------------------------------------------------- 1) 重建 schema
if [ "$REBUILD_SCHEMA" = "1" ]; then
    echo "→ REBUILD_SCHEMA=1：DROP SCHEMA public CASCADE，按 migrations/ 重建"
    echo "  ⚠ 会清空库内全部数据；请确认没有 app 正连着这个库"
    psql_q "DROP SCHEMA IF EXISTS public CASCADE" >/dev/null
    psql_q "CREATE SCHEMA public" >/dev/null
    psql_q "GRANT ALL ON SCHEMA public TO public" >/dev/null
    if ! command -v sqlx >/dev/null 2>&1; then
        echo "✗ 找不到 sqlx-cli，无法跑 migrations（brew install sqlx）；" >&2
        echo "  或改用 REBUILD_SCHEMA=0 + 手工 cargo run 一次让应用自己迁移" >&2
        exit 1
    fi
    sqlx migrate run --source migrations --database-url "$HOST_DB_URL"
    MIG_FILES=$(ls migrations/*.sql 2>/dev/null | wc -l | tr -d ' ')
    APPLIED=$(psql_q "SELECT count(*) FROM _sqlx_migrations WHERE success")
    if [ "$MIG_FILES" != "$APPLIED" ]; then
        echo "✗ migration 应用数不符：migrations/ 下 $MIG_FILES 个，" \
             "_sqlx_migrations 记 $APPLIED 条" >&2
        exit 1
    fi
    echo "  ✓ $APPLIED 个 migration 全部应用，目标 schema == migrations HEAD"
else
    echo "→ REBUILD_SCHEMA=0：沿用当前 schema（只灌数据，不动 DDL）"
    if [ "$(psql_q "SELECT count(*) FROM information_schema.tables
                   WHERE table_schema='public' AND table_name='t_user'")" != "1" ]; then
        echo "✗ $PG_DB 还没建表（先 cargo run 或 sqlx migrate run）" >&2
        exit 1
    fi
    APPLIED=$(psql_q "SELECT count(*) FROM _sqlx_migrations WHERE success")
    MIG_FILES=$(ls migrations/*.sql 2>/dev/null | wc -l | tr -d ' ')
    if [ "$APPLIED" != "$MIG_FILES" ]; then
        echo "⚠ REBUILD_SCHEMA=0 但 migration 只应用了 $APPLIED/$MIG_FILES 个，" \
             "目标 schema 落后于 HEAD（建议 REBUILD_SCHEMA=1）"
    fi
fi

# ---------------------------------------------------------------------------- 2) dump 进容器
DUMP_BASENAME="$(basename "$DUMP_FILE")"
docker cp "$DUMP_FILE" "$POSTGRES_CONTAINER:/tmp/$DUMP_BASENAME"

if [ "$REBUILD_SCHEMA" != "1" ] && [ "${RESET:-0}" != "1" ]; then
    HAS_DATA=$(psql_q "SELECT (SELECT count(*) FROM t_user) + (SELECT count(*) FROM t_part)
                       + (SELECT count(*) FROM t_customer)")
    if [ "${HAS_DATA:-0}" -gt 0 ]; then
        echo "⚠ 检测到 DB 已有数据（t_user + t_part + t_customer 共 $HAS_DATA 行）"
        echo "  → 想重新灌一遍：REBUILD_SCHEMA=1 $0"
        echo "  → 想原地重灌：RESET=1 REBUILD_SCHEMA=0 $0"
    fi
fi

# ---------------------------------------------------------------------------- 3) 漂移检测
echo "→ 检测 dump 与目标 schema 的列漂移"
COPY_HEADERS="$(mktemp)"; TMP_FILES="$TMP_FILES $COPY_HEADERS"
docker exec -e PGPASSWORD="$PG_PASSWORD" "$POSTGRES_CONTAINER" \
    pg_restore --data-only --file=- "/tmp/$DUMP_BASENAME" 2>/dev/null \
    | grep -E '^COPY public\.[A-Za-z0-9_]+ \(' > "$COPY_HEADERS" || true
if [ ! -s "$COPY_HEADERS" ]; then
    echo "✗ dump 里没解析出任何 COPY 表头（pg_restore 输出格式异常？）" >&2
    exit 1
fi

TARGET_SCHEMA="$(psql_q "SELECT table_name || '|' ||
                               string_agg(column_name, ',' ORDER BY ordinal_position)
                          FROM information_schema.columns
                          WHERE table_schema = 'public'
                          GROUP BY table_name")"

lookup_target_cols() {
    local want="$1" tn tc
    while IFS='|' read -r tn tc; do
        if [ "$tn" = "$want" ]; then printf '%s' "$tc"; return 0; fi
    done <<EOF
$TARGET_SCHEMA
EOF
    return 0
}

DECISIONS="$(mktemp)"; TMP_FILES="$TMP_FILES $DECISIONS"
SKIP_TABLES=""      # dump 有、目标库无（不进 restore）
STAGE_TABLES=""      # 有历史列 / rename 映射 → 必须走 stage 投影
DROPPED_ASSERT=""    # "tbl:col,col" —— restore 后断言这些列**不存在**
UNKNOWN_TABLES=""

while IFS= read -r line; do
    # pg_dump 写成 `COPY public.x (a, b, c) FROM stdin;`（逗号后有空格），先归一化
    tbl="${line#COPY public.}"; tbl="${tbl%% (*}"
    dcols="${line#*\(}"; dcols="${dcols%\) FROM stdin;}"; dcols="${dcols// /}"
    case "$tbl" in
        _sqlx_migrations|alembic_version) continue ;;   # 见文件头说明④
    esac
    tcols="$(lookup_target_cols "$tbl")"
    if [ -z "$tcols" ]; then
        SKIP_TABLES="${SKIP_TABLES}${SKIP_TABLES:+ }$tbl"
        continue
    fi
    classify_table "$tbl" "$dcols" "$tcols" >> "$DECISIONS"
done < "$COPY_HEADERS"

echo "  表 / 处理方式："
while IFS='|' read -r tbl common insert select dropped newonly unknown mapped; do
    if [ -n "$dropped" ] || [ -n "$mapped" ]; then
        STAGE_TABLES="${STAGE_TABLES}${STAGE_TABLES:+ }$tbl"
        DROPPED_ASSERT="${DROPPED_ASSERT}${DROPPED_ASSERT:+ }$tbl:$dropped"
        if [ -n "$unknown" ]; then
            UNKNOWN_TABLES="${UNKNOWN_TABLES}${UNKNOWN_TABLES:+ }$tbl"
        fi
        printf '    %-24s stage 投影：丢弃历史列 [%s]' "$tbl" "$dropped"
        if [ -n "$mapped" ]; then printf '；映射 %s' "$mapped"; fi
        printf '\n'
        if [ -z "$select" ]; then
            echo "✗ $tbl 没有可映射的公共列" >&2
            exit 1
        fi
    else
        printf '    %-24s 直灌' "$tbl"
        if [ -n "$newonly" ]; then printf '（目标新列置 NULL: %s）' "$newonly"; fi
        printf '\n'
    fi
done < "$DECISIONS"

if [ -n "$SKIP_TABLES" ]; then
    echo "  目标库无此表、跳过：$(printf '%s ' $SKIP_TABLES)"
fi
if [ -n "$UNKNOWN_TABLES" ]; then
    if [ "$ALLOW_UNKNOWN_DROP" = "1" ]; then
        echo "  ⚠ ALLOW_UNKNOWN_DROP=1：未登记的历史列将被丢弃，涉及 $UNKNOWN_TABLES"
    else
        echo "✗ 以下表含未登记的历史列（不在脚本顶部 KNOWN_DROP / RENAME_MAP 里），" >&2
        echo "  无法判定该丢弃还是该映射，已中止。确认后三选一：" >&2
        echo "    1) 在 KNOWN_DROP 登记为「027/028 已删，允许丢弃」" >&2
        echo "    2) 在 RENAME_MAP 登记为「老列 → 新列」" >&2
        echo "    3) 确实要丢：ALLOW_UNKNOWN_DROP=1 重跑" >&2
        awk -F'|' 'BEGIN{OFS=""} $7 != "" { print "    " $1 ": " $7 "\n" }' "$DECISIONS" >&2
        exit 1
    fi
fi

if [ "$DRY_RUN" = "1" ]; then
    echo "→ DRY_RUN=1：只做检测，不改库"
    exit 0
fi

# ---------------------------------------------------------------------------- 4) schema 快照（restore 前）
psql_q "DROP SCHEMA IF EXISTS _restore_verify CASCADE" >/dev/null
psql_q "CREATE SCHEMA _restore_verify" >/dev/null
psql_q "CREATE TABLE _restore_verify.columns_before AS
        SELECT table_name, column_name, data_type
        FROM information_schema.columns WHERE table_schema = 'public'" >/dev/null

# ---------------------------------------------------------------------------- 5) RESET（原地模式可选）
if [ "${RESET:-0}" = "1" ]; then
    echo "→ RESET=1：TRUNCATE 待恢复的表后重建"
    RESET_TABLES=$(psql_q "SELECT coalesce(string_agg(table_name, ',' ORDER BY table_name), '')
                           FROM information_schema.tables
                           WHERE table_schema = 'public' AND table_type = 'BASE TABLE'
                             AND table_name <> '_sqlx_migrations'")
    if [ -n "$RESET_TABLES" ]; then
        psql_q "TRUNCATE $RESET_TABLES RESTART IDENTITY CASCADE" >/dev/null
    fi
fi

# ---------------------------------------------------------------------------- 6) stage 投影
if [ -n "$STAGE_TABLES" ]; then
    psql_q "DROP SCHEMA IF EXISTS restore_stage CASCADE" >/dev/null
    psql_q "CREATE SCHEMA restore_stage" >/dev/null
    for tbl in $STAGE_TABLES; do
        row="$(awk -F'|' -v t="$tbl" '$1 == t { print; exit }' "$DECISIONS")"
        insert_cols="$(printf '%s' "$row" | cut -d'|' -f3)"
        select_cols="$(printf '%s' "$row" | cut -d'|' -f4)"
        echo "  → ${tbl}：建 restore_stage.${tbl}（用 dump 自己的 DDL 原样建）"
        DDL="$(docker exec -e PGPASSWORD="$PG_PASSWORD" "$POSTGRES_CONTAINER" \
                 pg_restore --schema-only --file=- -t "$tbl" "/tmp/$DUMP_BASENAME" 2>/dev/null \
               | sed -n "/^CREATE TABLE public\\.$tbl (/,/^);/p" \
               | sed "s/^CREATE TABLE public\\.$tbl (/CREATE TABLE restore_stage.$tbl (/")"
        if [ "$(printf '%s\n' "$DDL" | grep -c "^CREATE TABLE restore_stage\\.$tbl (")" != "1" ]; then
            echo "✗ 没能从 dump 里切出 $tbl 的 CREATE TABLE（-t 匹配到 0 或多张表）" >&2
            exit 1
        fi
        printf '%s\n' "$DDL" | psql_in >/dev/null

        echo "  → ${tbl}：灌 restore_stage 数据"
        docker exec -e PGPASSWORD="$PG_PASSWORD" "$POSTGRES_CONTAINER" \
            pg_restore --data-only --file=- -t "$tbl" "/tmp/$DUMP_BASENAME" 2>>"$RESTORE_LOG" \
          | sed -e "s/^COPY public\\.$tbl (/COPY restore_stage.$tbl (/" \
                 -e '/^SELECT pg_catalog\.setval(/d' \
          | psql_in >/dev/null

        stage_n=$(psql_q "SELECT count(*) FROM restore_stage.$tbl")
        before_n=$(psql_q "SELECT count(*) FROM public.$tbl")
        psql_in -c "INSERT INTO public.$tbl ($insert_cols) SELECT $select_cols FROM restore_stage.$tbl" \
                >/dev/null
        after_n=$(psql_q "SELECT count(*) FROM public.$tbl")
        if [ $((after_n - before_n)) -ne "$stage_n" ]; then
            echo "✗ $tbl 行数对不上：stage $stage_n 行，public 增量 $((after_n - before_n)) 行" >&2
            exit 1
        fi
        echo "    ✓ ${tbl}：投影 $stage_n 行（public ← ${select_cols}）"
        psql_q "DROP TABLE restore_stage.$tbl" >/dev/null
    done
    psql_q "DROP SCHEMA IF EXISTS restore_stage CASCADE" >/dev/null
fi

# ---------------------------------------------------------------------------- 7) pg_restore 主流程
echo "→ pg_restore --data-only --no-owner --no-privileges ..."
TOC_LIST_HOST="$(mktemp)"; TMP_FILES="$TMP_FILES $TOC_LIST_HOST"
docker exec -e PGPASSWORD="$PG_PASSWORD" "$POSTGRES_CONTAINER" \
    pg_restore -l "/tmp/$DUMP_BASENAME" > "$TOC_LIST_HOST"

# 7a) 排除目标库不存在的 sequence（否则 setval 报 relation does not exist）
seq_list="$(sed -n 's/.* SEQUENCE SET public \([A-Za-z0-9_]*\).*/\1/p' "$TOC_LIST_HOST" \
           | sort -u | tr '\n' ',')"
seq_list="${seq_list%,}"
if [ -n "$seq_list" ]; then
    seq_array=""
    IFS=','; for s in $seq_list; do
        if [ -n "$s" ]; then seq_array="${seq_array}${seq_array:+,}'$s'"; fi
    done; unset IFS
    missing_seqs=$(psql_q "SELECT coalesce(string_agg(s, ','), '') FROM unnest(ARRAY[$seq_array]) AS s
                            WHERE NOT EXISTS (SELECT 1 FROM information_schema.sequences
                                              WHERE sequence_schema = 'public'
                                                AND sequence_name = s)")
    if [ -n "$missing_seqs" ]; then
        echo "  → 目标库没有的 sequence，TOC 里跳过：$(echo "$missing_seqs" | tr ',' ' ')"
        grep -vE " SEQUENCE SET public (${missing_seqs})( |$)" "$TOC_LIST_HOST" \
            > "$TOC_LIST_HOST.f"
        mv "$TOC_LIST_HOST.f" "$TOC_LIST_HOST"
    fi
fi

# 7b) 排除不进 restore 的表：迁移表 / dump 有目标无 / 已走 stage / t_menu（可选）
if [ "$INCLUDE_MENU" = "1" ]; then
    EXCLUDE_DATA="_sqlx_migrations alembic_version $SKIP_TABLES $STAGE_TABLES"
    echo "  → INCLUDE_MENU=1：同时灌 dump 里的 t_menu / t_role_menu（可能与 seeds/menu.sql 冲突）"
else
    EXCLUDE_DATA="_sqlx_migrations alembic_version t_menu t_role_menu $SKIP_TABLES $STAGE_TABLES"
    echo "  → 默认跳过 t_menu / t_role_menu DATA 行（seeds/menu.sql 会重建）"
fi
data_group=""
for t in $EXCLUDE_DATA; do
    if [ -n "$t" ]; then data_group="${data_group:+$data_group|}$t"; fi
done
if [ -n "$data_group" ]; then
    grep -vE " TABLE DATA public ($data_group)( |$)" "$TOC_LIST_HOST" > "$TOC_LIST_HOST.f"
    mv "$TOC_LIST_HOST.f" "$TOC_LIST_HOST"
fi

docker cp "$TOC_LIST_HOST" "$POSTGRES_CONTAINER:/tmp/toc.list"

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
    "/tmp/$DUMP_BASENAME" 2>>"$RESTORE_LOG"
set -e
docker exec "$POSTGRES_CONTAINER" rm -f "/tmp/$DUMP_BASENAME" "/tmp/toc.list"
rm -f "$TOC_LIST_HOST"

# 过滤已知错误后剩余的算致命
FILTERED_LOG="$(mktemp)"; TMP_FILES="$TMP_FILES $FILTERED_LOG"
grep -Ev \
    -e 'relation "public.alembic_version" does not exist' \
    -e 'COPY failed for table "[^"]+": ERROR:  duplicate key value violates unique constraint' \
    -e '^DETAIL:  Key .* already exists\.$' \
    -e '^CONTEXT:  COPY .*, line [0-9]+$' \
    -e '^Command was: ' \
    -e '^pg_restore: warning: errors ignored on restore: ' \
    "$RESTORE_LOG" > "$FILTERED_LOG" || true

if [ -s "$FILTERED_LOG" ]; then
    echo "✗ pg_restore 出现未预期错误（已过滤 alembic_version / duplicate key）：" >&2
    cat "$FILTERED_LOG" >&2
    exit 1
fi

IGNORED=$(grep -cE 'duplicate key value violates unique constraint' "$RESTORE_LOG" 2>/dev/null || true)
if [ "${IGNORED:-0}" -gt 0 ]; then
    echo "  (ignored ${IGNORED} duplicate key：上次残留数据；RESET=1 可避免)"
fi

# ---------------------------------------------------------------------------- 8) 校验
psql_q "CREATE TABLE _restore_verify.columns_after AS
        SELECT table_name, column_name, data_type
        FROM information_schema.columns WHERE table_schema = 'public'" >/dev/null
COL_DIFF=$(psql_q "
    SELECT coalesce(string_agg(x, ' ; '), '') FROM (
        (SELECT table_name||'.'||column_name||' '||data_type AS x FROM _restore_verify.columns_before
         EXCEPT ALL
         SELECT table_name||'.'||column_name||' '||data_type FROM _restore_verify.columns_after)
        UNION ALL
        (SELECT table_name||'.'||column_name||' '||data_type FROM _restore_verify.columns_after
         EXCEPT ALL
         SELECT table_name||'.'||column_name||' '||data_type FROM _restore_verify.columns_before)
    ) d")
if [ -n "$COL_DIFF" ]; then
    echo "✗ restore 过程改动了 public schema（违反设计原则）：$COL_DIFF" >&2
    exit 1
fi
echo "  ✓ schema 快照 diff 为空：restore 未改动 public 的任何列"

# 判定为「丢弃」的已删除列，必须仍然不存在
if [ -n "$DROPPED_ASSERT" ]; then
    values_sql=""
    for pair in $DROPPED_ASSERT; do
        t="${pair%%:*}"
        for c in $(echo "${pair#*:}" | tr ',' ' '); do
            values_sql="${values_sql}${values_sql:+,}('$t','$c')"
        done
    done
    RESURRECTED=$(psql_q "SELECT coalesce(string_agg(t||'.'||c, ' '), '')
                           FROM (VALUES $values_sql) AS v(t,c)
                           WHERE EXISTS (SELECT 1 FROM information_schema.columns
                                         WHERE table_schema='public'
                                           AND table_name=v.t AND column_name=v.c)")
    if [ -n "$RESURRECTED" ]; then
        echo "✗ 已删除的列又被建回来了：$RESURRECTED" >&2
        exit 1
    fi
    echo "  ✓ 已删除列仍不存在：$(printf '%s\n' $DROPPED_ASSERT \
                                 | awk -F: '{ printf "%s(%s) ", $1, $2 }')"
fi

# 目标库存在但 dump 里没有的表 → 恢复后必然为空，明确告警（别静默当成"本来就没数据"）
dump_tables=""
while IFS= read -r line; do
    t="${line#COPY public.}"; t="${t%% (*}"
    dump_tables="${dump_tables}${dump_tables:+,}'$t'"
done < "$COPY_HEADERS"
NO_DUMP_TABLES=$(psql_q "SELECT coalesce(string_agg(table_name, ' '), '') FROM information_schema.tables
                         WHERE table_schema = 'public' AND table_type = 'BASE TABLE'
                           AND table_name <> '_sqlx_migrations'
                           AND table_name NOT IN ($dump_tables)")
if [ -n "$NO_DUMP_TABLES" ]; then
    echo "  ⚠ dump 里没有这些表的数据（恢复后为空）：$(echo "$NO_DUMP_TABLES" | tr ' ' ' ')"
fi

echo "→ 恢复后行数："
psql_q "
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
UNION ALL SELECT 't_menu='||count(*) FROM t_menu;" | sed 's/^/    /'

# ---------------------------------------------------------------------------- 9) menu seed
SEED_FILE="${SEED_FILE:-seeds/menu.sql}"
if [ -f "$SEED_FILE" ]; then
    echo "-> apply seed: $SEED_FILE (idempotent)"
    psql_in -f - < "$SEED_FILE" >/dev/null
    echo "   t_menu=$(psql_q 'SELECT count(*) FROM t_menu'), t_role_menu=$(psql_q 'SELECT count(*) FROM t_role_menu')"
fi

# ---------------------------------------------------------------------------- 10) 目标新列告警 + 清理
GAPS=$(awk -F'|' '$6 != "" { printf "    %-24s %s\n", $1, $6 }' "$DECISIONS")
if [ -n "$GAPS" ]; then
    echo "⚠ dump 早于这些列，恢复后为空（正常，非故障）："
    printf '%s\n' "$GAPS"
fi

psql_q "DROP SCHEMA IF EXISTS restore_stage CASCADE" >/dev/null
psql_q "DROP SCHEMA IF EXISTS _restore_verify CASCADE" >/dev/null

echo "✓ restore 完成（public schema 未被改动；已删除的列保持删除）"
