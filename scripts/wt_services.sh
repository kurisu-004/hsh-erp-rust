#!/usr/bin/env bash
# wt_services.sh — 每 worktree 一套容器（PG dev + Redis dev + app 端口）
#
# 2026-09-30 新增。解决的根因：所有 worktree 的 .env 都写死同一个
# DATABASE_URL（postgres://hsh:6065161@localhost:5430/hsh），而
# `sqlx::migrate!("./migrations")`（src/main.rs:58）路径相对
# CARGO_MANIFEST_DIR，即每个分支只会 apply 自己的 migrations。两者叠加 →
# 任一 worktree 跑一次 app 就往共用的 _sqlx_migrations 写一行，别的工作tree
# 立刻炸 `VersionMissing: migration X was previously applied but is missing
# in the resolved migrations`。本脚本让每个分支拿到独立的 PG/Redis 容器、
# 命名卷与端口，从根上消除该类跨分支污染。
#
# 用法（在主 checkout 仓库根执行）：
#   ./scripts/wt_services.sh up <slug>        # 建/复用该 worktree 的容器并改其 .env
#   ./scripts/wt_services.sh down <slug>      # 销毁容器 + 卷（数据一次性）
#   ./scripts/wt_services.sh ps               # 列出所有受管 worktree 的服务
#   ./scripts/wt_services.sh doctor           # 一致性体检（孤儿容器 / 端口冲突）
#
# 生命周期：normally 由 orchestrator skill 驱动，无需手工调用——
#   setup-worktree.sh    复制 .env 之后 → up <slug>
#   teardown-worktree.sh 移除 worktree 之前 → down <slug>
# 第二条同时根治了「worktree 目录已删、compose 容器还活着并占着端口」的
# 孤儿问题（teardown-worktree.sh 原本完全没有 docker 清理逻辑）。
#
# 端口分配（保留段 + 冲突探测，确定性优先）：
#   PG dev     5431-5499   Redis dev  6381-6449   App HTTP  3001-3099
#   起点 = 段起点 + cksum(slug) % 段长，再线性探测第一个空闲端口。
#   分配结果落盘到 <worktree>/.wt-services，up 重跑时复用 → 端口在整个
#   worktree 生命周期内稳定（容器 restart 不变），前端联调可写死。
#   段全占满则报错退出，不静默降级。
#
# 两个实现要点（改动时务必保持）：
#   1. `docker compose` 从**主 checkout 根**执行，靠 shell 环境变量注入
#      CONTAINER_PREFIX / POSTGRES_PORT / REDIS_PORT。compose 的插值优先级是
#      shell env > .env 文件，因此在主 checkout 跑不会被干扰；反过来若在
#      worktree 根跑，compose 会去读「本脚本正待改写的那个 .env」造成循环。
#      副作用：compose 文件恒取主 checkout 版本，分支若真改 docker-compose.yml
#      不会生效（当前用途无碍，调用方需知）。
#   2. 回写 .env 只做「按 key 定点替换」，绝不整文件重写——.env 里有明文 COS
#      生产凭据（.env:43-44），整文件重写风险不可接受。
#
# macOS bash 3.2 兼容：不用 mapfile / ${var^^} / readlink -f（与
# scripts/test_runner.sh 同一约束）。
# ⚠️ bash 3.2（macOS 自带）另有一处已知解析 bug：`$var` 后紧跟 UTF-8 多字节
#    字符（中文全角括号等）时，多字节首字节被并入变量名，`set -u` 下报
#    `var?: unbound variable`。因此本文件所有「变量 + 中文标点」相邻处一律写成
#    `${var}`。新增 echo 时务必保持。自检：
#      LC_ALL=C grep -n '\$[A-Za-z_][A-Za-z0-9_]*[^ -~]' scripts/wt_services.sh

set -euo pipefail
cd "$(dirname "$0")/.."

# ── 项目约定区 ───────────────────────────────────────────────
WT_ROOT=".claude/worktrees"
STATE_BASENAME=".wt-services"
PROJECT_PREFIX="hshwt-"
CONTAINER_PREFIX="wt-"

PG_PORT_START=5431
PG_PORT_END=5499
REDIS_PORT_START=6381
REDIS_PORT_END=6449
HTTP_PORT_START=3001
HTTP_PORT_END=3099
# ─────────────────────────────────────────────────────────────

usage() {
    cat >&2 <<'EOF'
用法:
  wt_services.sh up <slug>      建/复用该 worktree 的容器并改写其 .env
  wt_services.sh down <slug>    销毁容器 + 卷（数据一次性，先备份）
  wt_services.sh ps             列出所有受管 worktree 的服务
  wt_services.sh doctor         一致性体检
EOF
    exit 2
}

[ $# -ge 1 ] || usage
CMD="$1"
shift || true

# ── 工具函数 ─────────────────────────────────────────────────

# slug 必须是 kebab-case：compose project 名与 container_name 都吃这套字符集
valid_slug() {
    printf '%s' "$1" | grep -qE '^[a-z0-9][a-z0-9-]*$'
}

slug_hash() {
    local sum
    read -r sum _ < <(printf '%s' "$1" | cksum)
    printf '%s' "$sum"
}

# 端口空闲探测：连得上 = 占用
port_free() {
    ! (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null
}

# 在 [start,end] 内按 slug 确定性起点线性探测空闲端口；全占满返回 1
alloc_port() {
    local slug="$1" start="$2" end="$3" span base p
    span=$((end - start + 1))
    base=$((start + ( $(slug_hash "$slug") % span ) ))
    p=$base
    while [ "$p" -le "$end" ]; do
        if port_free "$p"; then
            printf '%s' "$p"
            return 0
        fi
        p=$((p + 1))
    done
    return 1
}

wt_path() { printf '%s/%s' "$WT_ROOT" "$1"; }
state_path() { printf '%s/%s/%s' "$WT_ROOT" "$1" "$STATE_BASENAME"; }
project_of() { printf '%s%s' "$PROJECT_PREFIX" "$1"; }
container_prefix_of() { printf '%s%s-' "$CONTAINER_PREFIX" "$1"; }

state_get() {
    # state_get <slug> <key>；无状态文件 / 无该 key 返回 1
    local sf; sf="$(state_path "$1")"
    [ -f "$sf" ] || return 1
    local v
    v="$(grep -E "^$2=" "$sf" 2>/dev/null | head -n1 | cut -d= -f2-)"
    [ -n "$v" ] || return 1
    printf '%s' "$v"
}

# 从主 checkout .env 定点取值，缺省回落到 docker-compose.yml 的默认值
master_env_get() {
    local key="$1" fallback="$2" v=""
    if [ -f .env ]; then
        v="$(grep -E "^${key}=" .env 2>/dev/null | head -n1 | cut -d= -f2- || true)"
    fi
    if [ -n "$v" ]; then printf '%s' "$v"; else printf '%s' "$fallback"; fi
}

# env_set <file> <KEY> <VALUE>：按 key 定点替换，key 不存在则追加带注释的新块
env_set() {
    local f="$1" key="$2" val="$3" tmp replaced=0
    tmp="${f}.wt.$$"
    if grep -q "^${key}=" "$f" 2>/dev/null; then
        awk -v k="$key" -v v="$val" '
            $0 ~ "^" k "=" { if (!done) { print k "=" v; done = 1 }; next }
            { print }
        ' "$f" > "$tmp"
        replaced=1
    else
        cp "$f" "$tmp"
    fi
    if [ "$replaced" -eq 0 ]; then
        printf '\n# --- wt_services.sh 注入 ---\n%s=%s\n' "$key" "$val" >> "$tmp"
    fi
    mv "$tmp" "$f"
}

compose_wt() {
    # compose_wt <slug> <args...>：在主 checkout 根、带 worktree 插值变量执行
    local slug="$1"; shift
    CONTAINER_PREFIX="$(container_prefix_of "$slug")" \
    POSTGRES_PORT="$(state_get "$slug" pg_port)" \
    REDIS_PORT="$(state_get "$slug" redis_port)" \
        docker compose -p "$(project_of "$slug")" "$@"
}

require_slug() {
    [ $# -ge 1 ] || { usage; exit 2; }
    if ! valid_slug "$1"; then
        echo "✗ slug 非法: '$1'（只允许小写字母/数字/连字符，如 fix-batch-current-process-id）" >&2
        exit 2
    fi
    if [ ! -d "$(wt_path "$1")" ]; then
        echo "✗ worktree 目录不存在: $(wt_path "$1")" >&2
        echo "  请先走 setup-worktree.sh 创建。" >&2
        exit 1
    fi
}

wait_ready() {
    # wait_ready <slug>：等 PG + Redis 健康，最多 30s
    local slug="$1" cname i
    cname="$(container_prefix_of "$slug")dev"
    i=0
    while [ "$i" -lt 60 ]; do
        if docker exec "$cname" pg_isready -q -U "$(master_env_get POSTGRES_USER hsh)" 2>/dev/null; then
            if docker exec "$(container_prefix_of "$slug")redis-dev" redis-cli ping 2>/dev/null | grep -q PONG; then
                return 0
            fi
        fi
        i=$((i + 1))
        sleep 0.5
    done
    echo "✗ 容器 30s 内未就绪（${cname}）" >&2
    docker logs "$cname" >&2 2>&1 | tail -20 || true
    return 1
}

# ── up ───────────────────────────────────────────────────────
cmd_up() {
    require_slug "${1:-}"
    local slug="$1" sf wt pg redis http pg_user pg_pass db_name db_url redis_url
    wt="$(wt_path "$slug")"
    sf="$(state_path "$slug")"

    # 已有状态文件 → 复用端口（幂等）；否则重新分配
    if [ -f "$sf" ]; then
        pg="$(state_get "$slug" pg_port || true)"
        redis="$(state_get "$slug" redis_port || true)"
        http="$(state_get "$slug" http_port || true)"
        if [ -z "$pg" ] || [ -z "$redis" ] || [ -z "$http" ]; then
            echo "✗ 状态文件损坏: $sf" >&2
            exit 1
        fi
        echo "→ 复用已分配端口: pg=$pg redis=$redis http=$http"
    else
        pg="$(alloc_port "$slug" "$PG_PORT_START" "$PG_PORT_END")" || {
            echo "✗ PG 保留段 $PG_PORT_START-$PG_PORT_END 已占满" >&2; exit 1; }
        redis="$(alloc_port "$slug" "$REDIS_PORT_START" "$REDIS_PORT_END")" || {
            echo "✗ Redis 保留段 $REDIS_PORT_START-$REDIS_PORT_END 已占满" >&2; exit 1; }
        http="$(alloc_port "$slug" "$HTTP_PORT_START" "$HTTP_PORT_END")" || {
            echo "✗ HTTP 保留段 $HTTP_PORT_START-$HTTP_PORT_END 已占满" >&2; exit 1; }
    fi

    {
        echo "project=$(project_of "$slug")"
        echo "container_prefix=$(container_prefix_of "$slug")"
        echo "pg_port=$pg"
        echo "redis_port=$redis"
        echo "http_port=$http"
    } > "$sf"

    echo "→ 启动容器 (compose project: $(project_of "$slug"))"
    # 刻意逐服务 up：不用全量 up -d。全量会重建主 checkout 的容器，且踩到
    # docker-compose.yml 写 redis:7-alpine 而在跑的实为 8-alpine 的历史分歧。
    compose_wt "$slug" up -d postgres-dev redis-dev

    wait_ready "$slug"

    # 回写 worktree 的 .env（定点替换 3 个 key）
    pg_user="$(master_env_get POSTGRES_USER hsh)"
    pg_pass="$(master_env_get POSTGRES_PASSWORD 6065161)"
    db_name="$(master_env_get POSTGRES_DB hsh)"
    db_url="postgres://${pg_user}:${pg_pass}@localhost:${pg}/${db_name}"
    redis_url="redis://localhost:${redis}"

    if [ ! -f "$wt/.env" ]; then
        cp .env "$wt/.env"
        echo "→ worktree 缺 .env，已从主 checkout 复制"
    fi
    env_set "$wt/.env" DATABASE_URL "$db_url"
    env_set "$wt/.env" REDIS_URL "$redis_url"
    env_set "$wt/.env" LISTEN_ADDR "0.0.0.0:${http}"

    echo
    echo "✓ worktree 服务就绪: $slug"
    echo "  compose project : $(project_of "$slug")"
    echo "  容器           : $(container_prefix_of "$slug")dev / $(container_prefix_of "$slug")redis-dev"
    echo "  卷             : $(project_of "$slug")_hsh-pdata / $(project_of "$slug")_redis-pdata"
    echo "  DATABASE_URL   : $db_url"
    echo "  REDIS_URL      : $redis_url"
    echo "  LISTEN_ADDR     : 0.0.0.0:${http}"
    echo
    echo "  首次 cargo run 会自动跑本分支的 migrations + seeds（src/main.rs:58-66）。"
    echo "  清理: ./scripts/wt_services.sh down $slug"
}

# ── down ─────────────────────────────────────────────────────
cmd_down() {
    require_slug "${1:-}"
    local slug="$1" sf; sf="$(state_path "$slug")"

    if [ ! -f "$sf" ]; then
        echo "WARN: 无状态文件（可能未 up 过或已清理）: $sf" >&2
    fi
    echo "→ 销毁容器 + 卷 (compose project: $(project_of "$slug"))"
    CONTAINER_PREFIX="$(container_prefix_of "$slug")" \
        docker compose -p "$(project_of "$slug")" down -v --remove-orphans
    rm -f "$sf"
    echo "✓ 已清理 $slug"
    echo "  提示: 卷是一次性的；若需要其中数据，先 pg_dump 再 down。"
}

# ── ps ───────────────────────────────────────────────────────
cmd_ps() {
    local found=0 slug proj cname
    printf '%-38s %-10s %-8s %-8s %s\n' SLUG PROJECT PG REDIS HTTP
    for sf in "$WT_ROOT"/*/"$STATE_BASENAME"; do
        [ -e "$sf" ] || continue
        found=1
        slug="$(basename "$(dirname "$sf")")"
        proj="$(grep -E '^project=' "$sf" | cut -d= -f2-)"
        cname="$(container_prefix_of "$slug")dev"
        local up_pg up_http
        if docker ps --format '{{.Names}}' | grep -qx "$cname"; then
            up_pg="up"
        else
            up_pg="DOWN"
        fi
        up_http="$(state_get "$slug" http_port || echo '?')"
        printf '%-38s %-10s %-8s %-8s %s\n' \
            "$slug" "$proj" "$(state_get "$slug" pg_port)" "$up_pg" "$up_http"
    done
    [ "$found" -eq 1 ] || echo "(暂无受管 worktree；.claude/worktrees/*/.wt-services 为标记文件)"
    return 0
}

# ── doctor ───────────────────────────────────────────────────
cmd_doctor() {
    local fail=0 warn=0 sf slug proj cname pg_port redis_port http_port
    local used_ports="" p wt

    echo "== 1. 受管 worktree 一致性 =="
    local found=0
    for sf in "$WT_ROOT"/*/"$STATE_BASENAME"; do
        [ -e "$sf" ] || continue
        found=1
        slug="$(basename "$(dirname "$sf")")"
        wt="$(wt_path "$slug")"
        proj="$(state_get "$slug" project || echo '?')"
        pg_port="$(state_get "$slug" pg_port || echo '?')"
        redis_port="$(state_get "$slug" redis_port || echo '?')"
        http_port="$(state_get "$slug" http_port || echo '?')"
        cname="$(container_prefix_of "$slug")dev"

        if docker ps --format '{{.Names}}' | grep -qx "$cname"; then
            echo "OK   $slug: 容器在跑 ($cname, pg=$pg_port redis=$redis_port http=$http_port)"
        else
            echo "FAIL $slug: 容器未运行 ($cname) —— 跑 ./scripts/wt_services.sh up $slug" >&2
            fail=$((fail + 1))
        fi

        # .env 是否与状态文件一致
        if [ -f "$wt/.env" ]; then
            if grep -qE "^DATABASE_URL=.*:${pg_port}/" "$wt/.env"; then
                echo "OK   $slug: .env DATABASE_URL 指向 $pg_port"
            else
                echo "FAIL $slug: .env DATABASE_URL 与状态文件 ($pg_port) 不一致" >&2
                fail=$((fail + 1))
            fi
            if grep -qE "^LISTEN_ADDR=.*:${http_port}\$" "$wt/.env"; then
                echo "OK   $slug: .env LISTEN_ADDR 指向 $http_port"
            else
                echo "FAIL $slug: .env LISTEN_ADDR 与状态文件 ($http_port) 不一致" >&2
                fail=$((fail + 1))
            fi
        else
            echo "FAIL $slug: worktree .env 不存在" >&2
            fail=$((fail + 1))
        fi

        for p in "$pg_port" "$redis_port" "$http_port"; do
            case "$used_ports" in
                *" $p "*)
                    echo "FAIL 端口 $p 被多个 worktree 占用（${used_ports}）" >&2
                    fail=$((fail + 1)) ;;
            esac
            used_ports="$used_ports $p "
        done
    done
    [ "$found" -eq 1 ] || echo "(无受管 worktree)"

    echo
    echo "== 2. 孤儿 compose project（working_dir 已不存在）=="
    # ConfigFiles 是 compose 文件路径，判活要看它所在的**目录**（worktree 被
    # 删后文件自然消失，但容器/端口仍被占）。teardown-worktree.sh 原先没有
    # docker 清理，正是这类孤儿的来源。
    local n_orphan=0 cfg_dir
    while IFS= read -r cfg_dir; do
        [ -n "$cfg_dir" ] || continue
        if [ ! -d "$(dirname "$cfg_dir")" ]; then
            echo "FAIL 孤儿: ${cfg_dir}（目录已删，容器与端口仍被占用）" >&2
            echo "     清理: docker compose -p <project> down -v   （project 名见 docker compose ls）" >&2
            n_orphan=$((n_orphan + 1))
            fail=$((fail + 1))
        fi
    done < <(docker compose ls --format json 2>/dev/null \
        | tr '{' '\n' | grep -o '"ConfigFiles":"[^"]*"' | cut -d'"' -f4 | tr ',' '\n')
    [ "$n_orphan" -eq 0 ] && echo "OK   无孤儿 compose project"

    echo
    echo "== 3. 未受管的存量 worktree（信息项，不报错）=="
    local n_legacy=0
    for wt in "$WT_ROOT"/*/; do
        [ -d "$wt" ] || continue
        if [ ! -f "$wt/$STATE_BASENAME" ]; then
            n_legacy=$((n_legacy + 1))
            echo "INFO $(basename "$wt")（未迁移，共用主 checkout 的 PG/Redis 与 3000 端口）"
        fi
    done
    [ "$n_legacy" -eq 0 ] && echo "OK   无未受管 worktree"

    echo
    if [ "$fail" -eq 0 ]; then
        echo "== 体检通过 =="
    else
        echo "== 体检未通过：$fail 项 FAIL，$warn 项 WARN ==" >&2
        exit 1
    fi
}

# ── dispatch ─────────────────────────────────────────────────
case "$CMD" in
    up)     cmd_up "${1:-}" ;;
    down)   cmd_down "${1:-}" ;;
    ps)     cmd_ps ;;
    doctor) cmd_doctor ;;
    -h|--help|help) usage ;;
    *) echo "✗ 未知子命令: $CMD" >&2; usage ;;
esac
