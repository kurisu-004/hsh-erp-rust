#!/usr/bin/env bash
# 2026-09-20 新增：nextest session 级 PG 容器 wrapper
#
# 背景：cargo-nextest 是 process-per-test 模型 —— runner 被每个测试调一次，
# 「binary 级容器」语义失效。改用 session 级共享 1 个 PG 容器，每测试 fresh
# database（fresh_database_url() 仍负责）。性能：避免 8-16 个临时容器反复
# 起停；正确性：fresh database 提供 per-test 隔离。
#
# 与 test_runner.sh 的关系：
# - test_nextest.sh 起 1 个 session 容器 → 注入 TEST_DATABASE_BASE_URL → cargo nextest run
#   （不要 exec：exec 会替换 shell 让 EXIT trap 失效）
# - nextest 调每个测试时 .cargo/config.toml 的 runner (test_runner.sh) 触发转义
#   口 1（TEST_DATABASE_BASE_URL 已注入）→ 直接 exec binary → 不再起新容器
# - trap EXIT 在 wrapper 退出时清理 session 容器

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

PORT=$(docker port "$CID" 5432/tcp | head -n1 | awk -F: '{print $NF}')
export TEST_DATABASE_BASE_URL="postgres://postgres:postgres@127.0.0.1:${PORT}"

# 不要 `exec` —— exec 会替换 shell 进程导致 EXIT trap 失效（2026-09-20 修复 bug：
# cargo nextest 退出后 shell 已不在，容器不会被 trap 清理）。改用普通调用让
# wrapper 自然走到末尾，trap 在脚本退出时清理 CID。
cargo nextest run "$@"
exit $?
