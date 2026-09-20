#!/usr/bin/env bash
# 2026-09-20 新增：nextest session 级 PG 容器 wrapper（plan 2：单容器 + TEMPLATE 克隆）
#
# 背景：cargo-nextest 是 process-per-test 模型 —— runner 被每个测试调一次，
# 「binary 级容器」语义失效。改用 session 级共享 1 个 PG 容器，每测试 fresh
# database（test_pool() 内部 CREATE DATABASE ... TEMPLATE hsh_erp_template 派生）。
# 性能：避免 8-16 个临时容器反复起停；正确性：fresh database 提供 per-test 隔离。
#
# 与 test_runner.sh 的关系：
# - test_nextest.sh 起 1 个 session 容器 → 在容器内 CREATE DATABASE hsh_erp_template
#   → 跑 24 个 schema 迁移到 template → 注入 TEST_DATABASE_BASE_URL → cargo nextest run
#   （不要 exec：exec 会替换 shell 让 EXIT trap 失效）
# - nextest 调每个测试时 .cargo/config.toml 的 runner (test_runner.sh) 触发转义
#   口 1（TEST_DATABASE_BASE_URL 已注入）→ 直接 exec binary → 不再起新容器
# - trap EXIT 在 wrapper 退出时清理 session 容器
#
# 跳过 5 个 INSERT 迁移（015/018/021/023/024）—— seed 数据由 test fixture helper
# 显式插入，避免与 UNIQUE 约束撞键；plan §Phase 1 step 3 决策。

set -euo pipefail

# 转义：外部已注入（如指向 postgres-test:5429 的快速路）→ 直通
if [ -n "${TEST_DATABASE_BASE_URL:-}" ]; then
    cargo nextest run "$@"
    exit $?
fi

CID=$(docker run -d \
    --tmpfs /var/lib/postgresql \
    -e POSTGRES_PASSWORD=postgres \
    -p 127.0.0.1:0:5432 \
    postgres:18-alpine \
    -c max_connections=500)
trap 'docker rm -f "$CID" >/dev/null 2>&1 || true' EXIT

# pg_isready 轮询 ≤30s（0.5s 间隔）
ready=0
for _ in $(seq 1 60); do
    if docker exec "$CID" pg_isready -U postgres -q 2>/dev/null; then
        ready=1
        break
    fi
    sleep 0.5
done
if [ "$ready" -ne 1 ]; then
    echo "error: postgres container 未在 30s 内就绪 (id=$CID)" >&2
    docker logs "$CID" >&2 || true
    exit 1
fi

# 2026-09-20 plan 2：在容器内 CREATE DATABASE hsh_erp_template + 跑 24 个 schema 迁移
TEMPLATE_DB=hsh_erp_template
docker exec -e PGPASSWORD=postgres "$CID" \
    psql -U postgres -c "CREATE DATABASE \"$TEMPLATE_DB\"" \
    >/dev/null

# 跳过的 5 个 INSERT 迁移（plan §Phase 1 step 3）：015/018/021/023/024
SKIP_RE='^20260[0-9]+_(015|018|021|023|024)_'

# 按文件名顺序逐个跑 schema 迁移到 template；失败立即退出（容器 exit trap 负责清理）
shopt -s nullglob
migrations_run=0
for f in $(ls migrations/*.sql | sort); do
    base=$(basename "$f")
    if [[ "$base" =~ $SKIP_RE ]]; then
        continue
    fi
    docker exec -i -e PGPASSWORD=postgres "$CID" \
        psql -U postgres -d "$TEMPLATE_DB" -v ON_ERROR_STOP=1 -f - \
        < "$f" >/dev/null
    migrations_run=$((migrations_run + 1))
done

if [ "$migrations_run" -ne 24 ]; then
    echo "error: 预期跑 24 个 schema 迁移，实际跑了 $migrations_run（INSERT 迁移跳过规则可能有误）" >&2
    exit 1
fi

# 注入 TEST_DATABASE_BASE_URL（指向 template；caller 用 TEMPLATE 派生 fresh DB）
PORT=$(docker port "$CID" 5432/tcp | head -n1 | awk -F: '{print $NF}')
export TEST_DATABASE_BASE_URL="postgres://postgres:postgres@127.0.0.1:${PORT}/${TEMPLATE_DB}"

# 不要 `exec` —— exec 会替换 shell 进程导致 EXIT trap 失效（2026-09-20 修复 bug：
# cargo nextest 退出后 shell 已不在，容器不会被 trap 清理）。改用普通调用让
# wrapper 自然走到末尾，trap 在脚本退出时清理 CID。
cargo nextest run "$@"
exit $?
