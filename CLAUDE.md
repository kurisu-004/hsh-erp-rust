# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

> 📌 **前端对接**：后端 API 参考见 [`docs/api/`](docs/api/index.md)（[index.md](docs/api/index.md) 为总入口，含通用约定 + 跨域错误码速查；按模块拆分为 `iam.md` / `applicants.md` / `customers.md` / `shelves.md` / `websocket.md` / `delivery-groups.md` / `cnc-programs.md` / `files.md` / `outsource-companies.md` / `outsource-quotes.md` / `outsource-shipments.md` / `_e2e.md`，`parts` 因端点 ≥49 已拆为 `docs/api/parts/` 子目录（`index.md` / `crud.md` / `lifecycle.md` / `inspection.md`），`assemblies` 拆为 `docs/api/assemblies/`，`production` 拆为 `docs/api/production/`（`index.md` + 6 子页：work-types / processes / work-type-process-mapping / process-chain / worker-pool / workers），`delivery-notes` 拆为 `docs/api/delivery-notes/`（4 子页）；2026-09-19 IAM 域合并：原 `auth.md` + `users.md` → `iam.md`；2026-09-19 prod 容器聚合：原 `workers.md` → `production/workers.md`，工种/工序/工艺链/工人池/工人 5 支撑域聚合为 `src/modules/prod/*`，URL 硬切换 `/api/v2/prod/*`）。**后端代码变更（新增 / 修改 / 删除端点，或修改 DTO 字段 / 错误码）必须立即同步更新对应模块文件**。

## 常用命令

```bash
docker compose up -d postgres-dev    # 开发库（localhost:5430，库 hsh）：cargo run 与 query! 编译期校验依赖
docker compose up -d postgres-test   # 测试库（localhost:5429，库 postgres_rust_test）：集成测试依赖，首次自动建库+迁移

cargo check                 # 已有 query! 宏：编译期经 .env 的 DATABASE_URL 连开发库校验；无库时用 SQLX_OFFLINE=true（.sqlx 已提交）
cargo clippy --all-targets
cargo test                  # 需先起 postgres-test；跑单个测试：cargo test <name>
cargo run                   # 需先 cp .env.example .env 并起 postgres-dev

./scripts/sqlx_prepare.sh   # 每次 query! 宏改动后必须重跑，生成 .sqlx/query-*.json 并提交
SQLX_OFFLINE=true cargo build --release   # CI/Docker 用离线元数据构建

# 一次性安装 nextest
cargo install cargo-nextest --locked

# 全量并行测试（推荐；走 test_nextest.sh 起 session 级容器 + per-test database）
scripts/test_nextest.sh

# 单个 binary 调试（runner 自动起容器、用完即删）
cargo test --test <name>

# 快速路：复用 postgres-test 服务（:5429，跳过容器)
TEST_DATABASE_BASE_URL=postgres://hsh_test:6065161test@localhost:5429 cargo nextest run

# 手工应用 seeds（菜单等配置数据；通常 app 启动钩子自动跑）
./scripts/seed_apply.sh
```

## Schema 迁移与 seeds 分离（2026-09-25 sqlx 接管后）

- `migrations/20260925000000_001_baseline.sql` —— 全量 schema 基线（合并原 001-029）
- `migrations/archive/` —— 原 29 个 migration 文件归档，仅历史参考，**不参与** sqlx::migrate! 扫描
- `seeds/menu.sql` —— 菜单树声明式种子（t_menu + t_role_menu），幂等可反复跑
- `seeds/README.md` —— seed 编写规范

**新 schema 变更走追加新 migration**（append-only，永不修改已有文件）。
**菜单变更 = 改 `seeds/menu.sql` + 重启 app**（启动钩子自动跑，无需新 migration）。

### 初始管理员账号（2026-09-26 新增）

`seeds/admin.sql` 是**可选**初始管理员 seed（`username=admin / password=changeme / role=MANAGER`），由环境变量 `BOOTSTRAP_ADMIN_ENABLED` 门控（默认 `false`）。在 `src/main.rs` 启动钩子、`src/infra/seed.rs::run_seeds(pool, bootstrap_admin_enabled)` 处执行。开启流程：env=`true` → cargo run / docker compose up → 用 admin/changeme 登录 `/api/v2/iam/login` → 改密 → env=`false` → 重启。生产默认关，避免无意中创建初始账号。明文密码与 `src/modules/iam/service/account.rs::DEFAULT_RESET_PASSWORD` 同源，bcrypt 哈希复用 `test-support/fixtures/iam.sql` 同款字面值。

## 领域结构（垂直切片）

`src/modules/<域>/` 标准六件套，与 `backend-python/` 文件一一对应（迁移时逐域对照）：

| 文件 | 职责 | 对应 `backend-python` |
|---|---|---|
| `handler.rs` | axum handler + 路由 | `api/v1/<mod>.py` |
| `service.rs` | 业务逻辑，签名收 `&mut PgConnection` | `service/<mod>_service.py` |
| `repo.rs` | sqlx 查询，签名收 `impl PgExecutor<'_>` | `repository/<mod>_repository.py` |
| `model.rs` / `dto.rs` | 表行模型+域枚举 / 请求响应 DTO | `model/` / `schema/` |
| `statemachine.rs` | 仅 part / assembly / delivery_note / outsource / process_chain 五域 | `statemachines/` |

**part 是跨域枢纽**（delivery_note、assembly、outsource、part_file、statistics、shelf 均依赖它），实施顺序见 architecture.md 第 7 节。

## 必须遵守的架构约定

1. **事务边界在 handler（2026-09-21 重构 + 2026-09-22 删 `PgIamRepo` 转发壳后 iam 与其余 20 个 handler 文件一致）**：handler 显式 `state.pool.begin()` / `tx.commit()`，错误路径 tx drop 隐式回滚。service 不知事务——所有跨 repo 操作经 `repo: R`（by-value；`IamRepo` / 域内对应 trait 已直接 `impl for &mut PgConnection`，handler/service 借 `&mut *tx` / `&mut *conn` 即可）参数传入。
   - 例外清单（仍走 handler 边界）：
     - `_e2e` 直调方（测试 fixture 自管 tx）
     - 既有 `tests/iam/{api.rs,middleware.rs}`（2026-09-23 PR13 拆分；原 `tests/iam_api.rs` / `tests/auth_middleware.rs` 已合到 `tests/iam/`）等 HTTP 契约测试（不改测试代码）
     - 读端点（`me` / `list_users` / `get_user` 等）`pool.acquire()` 不开事务，service 借 `&mut PgConnection` 跑查询，连接用完即 drop。
2. **统一响应信封**：handler 返回 `Result<Json<R<T>>, AppError>`。`R { code: 0, message: "ok", data }`；错误由 `AppError::into_response()` 装入同一信封。不做 middleware 后置包装。
3. **错误码分段契约**（`src/shared/error.rs::code`，与 Python 前端对齐）：0 成功、4xxxx HTTP 语义、5xxxx 系统、2xxxx 业务域（每域一个段，如 201xx 零件/客户、214xx 送货单，新增域错误码先入对应段）。
5. **状态机不写 DB**：`statemachine.rs` 只做内存 enum + `can_transition_to` 迁移表；事件日志由 service 在事务内统一插入。
6. **WS 广播在 commit 之后**（对齐 Python 延迟广播模式），用 `state.ws_hub.broadcast(...)`。
7. **路由挂载**：业务 REST 统一 `/api/v2`（与 Python `/api/v1` 并行），WS 在 `/ws/dashboard`。`/api/mcp` 不在本仓库。

## 集成测试目录结构（2026-09-23 PR13 重构后）

51 个 integration test binary 已重组为 **20 binary**（9 多文件 domain 子目录化 + 11 single-file 保留 + 1 域内拆 3）：

| 新结构 | 拆前 binary 数 | 拆后 binary 名（nextest filter） |
|---|---:|---|
| `tests/delivery/{main,group,attach_batches,print,scan,note}.rs` | 5 | `delivery` |
| `tests/part/{main,helpers,crud,lifecycle,batch,file,list_enrichment,repair,to_ship,to_inspection,to_process,inspection_batches,serial}.rs` | 12 | `part` |
| `tests/assembly/{main,api,files,status_sync}.rs` | 3 | `assembly` |
| `tests/iam/{main,api,middleware}.rs` | 2 | `iam`（redis-flush group）|
| `tests/shelf/{main,api,deactivate}.rs` | 2 | `shelf` |
| `tests/statistics/{main,api,event_driven}.rs` | 2 | `statistics` |
| `tests/production/{main,work_type,process,process_chain,worker,worker_pool,worker_pool_auto_allocate}.rs` | 6 | `production`（按 `src/modules/prod/*` 对齐）|
| `tests/outsource/{main,company,quote,send_receive}.rs` | 3 | `outsource` |
| `tests/user_repo/{main,basic,role,password}.rs` | 1 → 3 sub-file | `user_repo` |
| 单文件保留：applicant_api / customer_api / _e2e_api / cos_opendal_api / cos_real_smoke / auto_complete_api / dashboard_ws_api / idempotency_api / guard_dn_in_use_api / cnc_program_api | 10 | （各自原 binary 名）|
| **合计** | 51 → | **20 binary** |

**关键约定**：
- cargo 1.98.1 **不识别** `tests/<dir>/mod.rs`，只识别 `tests/<dir>/main.rs`（binary 名 = `<dir>`）。新 domain 一律用 `main.rs`。
- 共享基建从 monolith `tests/common/mod.rs`（1393 行）迁到独立 dev-only crate **`test-support/`**（`hsh-erp-test-support`）；`tests/common/mod.rs` 保留为 facade re-export 兼容期，单文件 binary 通过 `mod common;` 仍可用，未来逐步移除。
- **修改 tests 内 query! 宏后必须重跑 `./scripts/sqlx_prepare.sh`** 生成 `.sqlx/query-*.json` 并提交；拆分 sub-file 会引入新 cache hash（路径变化）。

## 集成测试 fixture 范本（2026-09-23 PR13 Phase F 引入）

`test-support` 提供三类共享资产，新 integration test binary 一律走下列入口，**禁止在测试文件内重新声明本地 `send` / `json_request` / `setup` / `login_*` / `insert_*` / `seed_*`**：

### `test-support::http` —— HTTP 客户端 helper

| 函数 | 用途 |
|---|---|
| `send(app: axum::Router, req: Request<Body>) -> (StatusCode, Value)` | oneshot 驱动 Router 并解析信封 JSON |
| `json_request(method: &str, uri: &str, body: Option<Value>, bearer: Option<&str>) -> Request<Body>` | 构造带 JSON body / Bearer 头的请求 |
| `login_token(app: &axum::Router, username: &str, password: &str) -> String` | POST `/iam/login` 拿 bearer token |

签名与原 27+ 重复实现**逐字一致**，便于后续按 binary 批量替换。

### `test-support::fixture` —— 按域预制 fixture 加载

约定三段式（**新域遵循**）：
1. **SQL 文件**：`test-support/fixtures/<domain>.sql`
   - 所有 ID 走常量 `9_000_000_000_000_000_001+` 区段（雪花 ID epoch=2020-01-01 × instance≤1023 × 12bit seq ≈ 6×10¹⁶ 上限，物理不相交）
   - bcrypt 哈希预生成嵌入 SQL（cost=12，明文由 `ProcessChainFixture::PASSWORD` 公开），省每测试现场 hash ~250ms
   - 时间列：审计字段用 `now()`，业务日期按 fixtures 字面
2. **Fixture struct**：`test-support/src/fixture.rs::ProcessChainFixture`（字段 + 常量 ID `pub const`），`Default` 实现给出 `manager_username` / `clerk_username` 等字符串常量
3. **Loader 函数**：`load_<domain>_fixture(pool: &PgPool) -> <Domain>Fixture`，走 `include_str!` 编译期嵌入 + `sqlx::raw_sql` 一次性执行（multi-statement）；与 [`fixtures`](test-support/src/fixtures.rs)（动态 helper）分工：前者批量差异跨域共享，后者单条参数化差异

### 范本文件

`tests/production/process_chain.rs` 是首个按 fixture 范本改写的 integration test binary，10 个场景的字面请求 / 断言**逐字保留**，仅替换本地 helper 为 test-support 引入 + 抽出 `bootstrap_as_manager` / `bootstrap_as_clerk` 两个样板函数。新 binary 改造时可参照此模式。

### 后续 27+ 文件改造顺序（按 cargo test nextest filter）

| 优先级 | 域 | 备注 |
|---|---|---|
| Phase F 已完成 | process_chain | 范本 |
| Phase G | part（11 sub-file）/ delivery（5 sub-file） | 涉及 process_chain / worker_pool 引用最多 |
| Phase H | production 其余（work_type / process / worker / worker_pool / worker_pool_auto_allocate）/ assembly / shelf / statistics / outsource | worker_pool fixture 复用度高 |
| Phase I | iam / user_repo / applicant / customer / dashboard_ws / _e2e / cnc_program / auto_complete / guard_dn_in_use / idempotency / cos_opendal / cos_real_smoke | 单 binary 不拆 |


## DB 约定（迁移与查询必须沿用）

- 无物理外键（`bigint` + 索引，存在性由 service 校验）、无 DB ENUM（`varchar` + Rust enum 校验）
- 乐观锁：`version` 列，UPDATE 带 `WHERE id=$1 AND version=$2`，0 行 → 409 / `VERSION_CONFLICT`
- 软删除：`deleted_at IS NULL`；审计字段 `created_at/by`、`updated_at/by`
- 雪花主键：`SnowflakeIdGenerator::next_id()` App 侧生成
- i64 主键序列化为 JSON string（`shared/types.rs` 的 serde helper），防 JS 精度截断
- 时间列存 naive `timestamp`，写入用 `infra::clock::now_naive()`（Asia/Shanghai）
- 迁移命名：`<13位时间戳>_<顺序>_<描述>.sql`，见 `migrations/README.md`
- 菜单 / 角色等配置数据走 `seeds/*.sql`，不走 migration

## 环境要点

- 双 PG 容器（docker compose 分服务启动）：开发库 `postgres-dev` 在 **5430**（库 `hsh`），测试库 `postgres-test` 在 **5429**（库 `postgres_rust_test`）
- 配置全部走 `.env`（`infra/config.rs`）：`DATABASE_URL` 优先，缺省回退 `POSTGRES_*` 拆分变量拼接；测试库 URL 由 `build_test_database_url()` 构建（`DATABASE_TEST_URL` / `POSTGRES_TEST_*`）
- 优雅退出：`AppState.shutdown`（CancellationToken）同时通知 axum serve 与 `task/auto_complete` 后台循环
