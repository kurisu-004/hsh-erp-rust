#!/usr/bin/env bash
# 2026-09-20 新增：cargo test 自定义 runner（修复 ephemeral-postgres 容器泄漏）
#
# 背景：原 ephemeral-postgres 把 Cluster 放在 `static OnceCell`，Rust static 永
# 不 Drop → docker 容器永不释放（用户实测已堆 128 个 postgres:18-alpine）。
# 本 runner 把容器生命周期绑到「test binary 进程」上，由 shell `trap EXIT`
# 强制清理，不依赖 Rust Drop / tokio runtime / 信号 watchdog。
#
# 调用方式：cargo 通过 `.cargo/config.toml` 的 `target.<cfg>.runner` 字段调本
# 脚本，把 binary 路径作为 $1、后续 args 作为 $2..。stdout 必须纯净（nextest
# list 阶段会解析 stdout），所有诊断走 stderr。
#
# 4 个转义口 + 主路径：
#   1. TEST_DATABASE_BASE_URL 已设置  → exec binary（外部注入或手动 export）
#   2. args 含 --list                  → exec binary（list 阶段不起容器）
#   3. binary 路径不含 /deps/          → exec binary（cargo run 主 binary）
#   4. binary 名 = hsh_erp_rust-*      → exec binary（lib/bin 单测纯函数）
#   主路径：43 个 integration test binary → 起容器 → trap 清理 → 跑 binary

set -euo pipefail

BINARY="${1:-}"
shift || true

# 转义口 1：外部已注入（如指向 postgres-test:5429 的快速路）
if [ -n "${TEST_DATABASE_BASE_URL:-}" ]; then
    exec "$BINARY" "$@"
fi

# 转义口 2：nextest list 阶段（不起容器）
for arg in "$@"; do
    if [ "$arg" = "--list" ]; then
        exec "$BINARY" "$@"
    fi
done

# 转义口 3：cargo run 主 binary 路径不含 /deps/
case "$BINARY" in
    */deps/*) ;;
    *)
        exec "$BINARY" "$@"
        ;;
esac

# 转义口 4：lib/bin 单元测试（binary 名 = hsh_erp_rust-*）
bin_name="$(basename "$BINARY")"
case "$bin_name" in
    hsh_erp_rust-*)
        exec "$BINARY" "$@"
        ;;
esac

# ----------------------------------------------------------------------
# 主路径：起 postgres:18-alpine 容器 → trap EXIT 清理 → 注入 URL → 跑 binary
# ----------------------------------------------------------------------

# nextest session 模式：未走 test_nextest.sh 启动容器 → 每测试一容器（慢但正
# 确）。stderr 提示用户走 wrapper 拿 session 级共享。
if [ -n "${NEXTEST:-}" ] || env | grep -q '^NEXTEST_'; then
    echo "warning: NEXTEST_* env detected 但 TEST_DATABASE_BASE_URL 未注入；" >&2
    echo "         每测试将拉起一个临时容器（慢）。全量跑请用 scripts/test_nextest.sh" >&2
fi

CONTAINER_ID=$(docker run -d \
    --tmpfs /var/lib/postgresql \
    -e POSTGRES_PASSWORD=postgres \
    -p 127.0.0.1:0:5432 \
    postgres:18-alpine)
# 注意：不能用 exec —— exec 会替换 shell 进程导致 trap 失效。子进程 + 透传 exit code。
trap 'docker rm -f "$CONTAINER_ID" >/dev/null 2>&1 || true' EXIT

# pg_isready 轮询 ≤30s（0.5s 间隔）
ready=0
for _ in $(seq 1 60); do
    if docker exec "$CONTAINER_ID" pg_isready -U postgres -q 2>/dev/null; then
        ready=1
        break
    fi
    sleep 0.5
done
if [ "$ready" -ne 1 ]; then
    echo "error: postgres container 未在 30s 内就绪 (id=$CONTAINER_ID)" >&2
    docker logs "$CONTAINER_ID" >&2 || true
    exit 1
fi

# 解析动态端口
PORT=$(docker port "$CONTAINER_ID" 5432/tcp | head -n1 | awk -F: '{print $NF}')
export TEST_DATABASE_BASE_URL="postgres://postgres:postgres@127.0.0.1:${PORT}"

"$BINARY" "$@"
exit $?
