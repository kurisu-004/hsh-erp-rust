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
    NEXTEST_FAILED=$?
    exit "$NEXTEST_FAILED"
fi

CID=$(docker run -d \
    --tmpfs /var/lib/postgresql \
    -e POSTGRES_PASSWORD=postgres \
    -p 127.0.0.1:0:5432 \
    postgres:18-alpine \
    -c max_connections=500)
# 2026-09-21 改造：nextest 失败时保留容器供 debug。
# NEXTEST_FAILED 未设置 / 空 → setup 阶段退出 → 删容器（兜底）；
# NEXTEST_FAILED=0 → nextest 成功 → 删容器；
# NEXTEST_FAILED 非 0 → nextest 失败 → 保留容器 + 打印调试指引。
# 注意：cleanup() 内不用 local rc=...，因为 local 内置在 set -u 下有未初始化窗口
# 会触发 unbound variable。直接赋值给全局 rc + 默认值兼容 unset 场景。
cleanup() {
    rc="${NEXTEST_FAILED:-}"
    port=$(docker port "$CID" 5432/tcp 2>/dev/null | head -n1 | awk -F: '{print $NF}')
    if [ "$rc" = "0" ]; then
        docker rm -f "$CID" >/dev/null 2>&1 || true
    elif [ -n "$rc" ]; then
        # 2026-09-21 备注：bash 3.2 (macOS) 把 `$rc` 后接 UTF-8 高字节误并入变量名，
        # 导致 set -u 下报 unbound；用 ${rc} 大括号显式划界。
        echo "warning: nextest 退出码 ${rc}；保留容器 $CID 用于 debug" >&2
        echo "  连接 DB: psql -h 127.0.0.1 -p ${port:-?} -U postgres -d hsh_erp_template" >&2
        echo "  列 test DB: SELECT datname FROM pg_database WHERE datname LIKE 'test_%';" >&2
        echo "  或:    docker exec -it $CID psql -U postgres" >&2
        echo "  清理:   docker rm -f $CID" >&2
    else
        # setup 阶段失败 → 删容器兜底
        docker rm -f "$CID" >/dev/null 2>&1 || true
    fi
    # 2026-09-24 E1 改造：与 session 容器一起清理 RSA keypair tmpdir（避免每跑一次
    # 累积一堆 2048-bit 私钥残留；不在 git 但仍有 disk-usage 噪音）。
    if [ -n "${JWT_KEYS_TMPDIR:-}" ] && [ -d "$JWT_KEYS_TMPDIR" ]; then
        rm -rf "$JWT_KEYS_TMPDIR" 2>/dev/null || true
    fi
}
trap cleanup EXIT

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

# 2026-09-24 E1 改造：跑迁移到 template 之前，先用 openssl 生成一对 2048-bit RSA
# keypair 并 export 到环境，test-support/src/pem.rs 会短路读盘，避免 nextest
# process-per-test 模型下 ~500 进程各自生成两次 RSA keypair（每进程 ~150ms+
# OsRng entropy）。next 仍生成（与现有 middleware 断言对齐：JwtConfig::public_keys
# 装载 current + next 两对；test_public_kids() 首次访问会触发 KEYS_NEXT lazy init）。
#
# env 短路约定：
# - JWT_TEST_PRIVATE_PEM_PATH → 私钥 PKCS#8 PEM 文件
# - JWT_TEST_PUBLIC_PEMS_DIR → 公钥 SPKI PEM 目录，kid = 文件名（current.pem / next.pem）
# 两 env 都设 → test-support::pem 读盘；任一缺失 → 静默回退 OsRng（unit test 兼容）；
# 都设但文件缺失 → fast-fail panic（不静默回退）。
JWT_KEYS_TMPDIR="$(mktemp -d -t hsh_jwt_keys.XXXXXX)"
mkdir -p "$JWT_KEYS_TMPDIR/pub"
openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out "$JWT_KEYS_TMPDIR/priv.pem" 2>/dev/null
openssl rsa -in "$JWT_KEYS_TMPDIR/priv.pem" -pubout -out "$JWT_KEYS_TMPDIR/pub/current.pem" 2>/dev/null
openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out "$JWT_KEYS_TMPDIR/next_priv.pem" 2>/dev/null
openssl rsa -in "$JWT_KEYS_TMPDIR/next_priv.pem" -pubout -out "$JWT_KEYS_TMPDIR/pub/next.pem" 2>/dev/null
export JWT_TEST_PRIVATE_PEM_PATH="$JWT_KEYS_TMPDIR/priv.pem"
export JWT_TEST_PUBLIC_PEMS_DIR="$JWT_KEYS_TMPDIR/pub"

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
# 2026-09-21 备注：cargo nextest run 失败时需要保留 $? 给 cleanup()，
# 所以临时关 set -e 让赋值 NEXTEST_FAILED=$? 能跑到；否则 set -e 会
# 在 cargo nextest 失败那一刻直接退出、跳过赋值，cleanup 拿到空值。
set +e
cargo nextest run "$@"
NEXTEST_FAILED=$?
set -e
exit "$NEXTEST_FAILED"
