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
#   → 按字典序逐个 apply migrations/*.sql + seeds/menu.sql 到 template
#   → 注入 TEST_DATABASE_BASE_URL →
#   cargo nextest run（不要 exec：exec 会替换 shell 让 EXIT trap 失效）
# - nextest 调每个测试时 .cargo/config.toml 的 runner (test_runner.sh) 触发转义
#   口 1（TEST_DATABASE_BASE_URL 已注入）→ 直接 exec binary → 不再起新容器
# - trap EXIT 在 wrapper 退出时清理 session 容器
#
# 2026-09-25 改造历史：旧的 29 个 DDL+菜单 DML 迁移曾合并进 baseline 单文件、
#   菜单 seed 抽到 seeds/。当时的 SKIP_RE（015/018/021/023/024）随之失效：
#   015 DML 在 baseline 中是 CREATE SEQUENCE + 空 INSERT（no-op），其它全在
#   baseline；菜单种子走 seeds/。**注意：baseline 早已不是唯一文件** —— 之后按
#   append-only 又追加了 7 个迁移（2026-09-29 wx identity / is_cnc / gcode index /
#   2026-09-30 batch current_process_id，以及 2026-10-01 的 is_repairing /
#   REPAIRING→IN_PROCESS / 序列号释放；见下方 2026-09-30 修复）。
#
# 2026-09-30 修复：template 库迁移从「断言 migrations/ 有且仅有 1 个 .sql」改成
#   「按字典序逐个 apply」。原断言自第 2 个迁移文件（2026-09-29 wx identity）加入
#   起就必然失败（master 上同样坏），导致本脚本从没人能跑通——集成测试实际一直
#   靠 CLAUDE.md:37 的 `TEST_DATABASE_BASE_URL=... cargo nextest run` 快速路兜底。
#   根因是脚本把「baseline 单文件」这一 2026-09-25 的**阶段性**状态写成了断言；
#   但 migrations/README.md 明确「新 schema 变更走 append-only 追加新迁移」，
#   文件数必然增长。
#
#   字典序 == 时间序的前提：迁移命名规范是 `<13位时间戳>_<顺序>_<描述>.sql`
#   （migrations/README.md「命名」节），13 位时间戳定长且十进制零填充，13 位上限
#   远超现有值（2026 年 ≈ 2.0e12 < 1e13），故定长前缀的字典序等价于时间序。
#   追加新迁移时只要继续遵守该命名规范，apply 顺序就与 sqlx::migrate! 扫描序一致。
#
#   仍用裸 `psql` apply（**不写** `_sqlx_migrations` 账本）：template 库只作
#   CREATE DATABASE ... TEMPLATE 的模板，账本缺失不影响派生库；改成调
#   `cargo sqlx migrate run` 反而会因为账本缺失而重跑 baseline 并失败。
#
#   已知竞态（review 第 1 轮记录，本轮不修）：`pg_isready` 轮询可能在
#   docker-entrypoint 的 initdb 阶段命中**只监听 unix socket 的临时 server**，
#   随后 `psql` 走同一 socket 时可能报
#   `connection to server on socket ... No such file or directory`。影响仅限
#   首跑偶发失败（重跑即过），且是 fail-loud —— `set -e` + EXIT trap 下
#   NEXTEST_FAILED 未赋值会走「setup 失败 → 删容器」分支并 exit 1，不会产生假
#   PASS。低成本修法：把轮询探针换成真连接（`psql -c 'SELECT 1'`）或给
#   CREATE DATABASE 包一层重试。

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

# 2026-09-20 plan 2 + 2026-09-25 改造：在容器内 CREATE DATABASE hsh_erp_template，
# 跑全部迁移 + seeds/menu.sql，让测试 DB 共享同一份 schema + 菜单 baseline。
TEMPLATE_DB=hsh_erp_template
docker exec -e PGPASSWORD=postgres "$CID" \
    psql -U postgres -c "CREATE DATABASE \"$TEMPLATE_DB\"" \
    >/dev/null

# 2026-09-30 修复：按字典序逐个 apply migrations/*.sql（原先断言「有且仅有 1 个
# 文件」，从第 2 个迁移加入起就必然失败）。字典序 == 时间序的前提见文件头注释
# （迁移命名规范 `<13位时间戳>_<顺序>_<描述>.sql`，见 migrations/README.md）。
shopt -s nullglob
migration_files=(migrations/*.sql)
if [ "${#migration_files[@]}" -eq 0 ]; then
    echo "error: migrations/ 下没有任何 .sql 文件" >&2
    exit 1
fi
for f in "${migration_files[@]}"; do
    echo "[migrate] $f" >&2
    docker exec -i -e PGPASSWORD=postgres "$CID" \
        psql -U postgres -d "$TEMPLATE_DB" -v ON_ERROR_STOP=1 -f - \
        < "$f" >/dev/null
done

# 跑菜单种子到 template（幂等；test 路径下也走同一份声明式菜单树，
# 与 production 一致；测试自身的 seed_* helper 不受 seed IDs 干扰，
# 因为 seed ID 段 9000000000xxx 与 fixture/snowflake 物理不相交）
docker exec -i -e PGPASSWORD=postgres "$CID" \
    psql -U postgres -d "$TEMPLATE_DB" -v ON_ERROR_STOP=1 -f - \
    < seeds/menu.sql >/dev/null

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
