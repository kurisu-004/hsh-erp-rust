# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

> 📌 **前端对接**：后端 API 参考见 [`docs/api/`](docs/api/index.md)（[index.md](docs/api/index.md) 为总入口，含通用约定 + 跨域错误码速查；按模块拆分为 `iam.md` / `applicants.md` / `customers.md` / `shelves.md` / `websocket.md` / `delivery-groups.md` / `cnc-programs.md` / `files.md` / `outsource-companies.md` / `outsource-quotes.md` / `outsource-shipments.md` / `_e2e.md`，`parts` 因端点 ≥49 已拆为 `docs/api/parts/` 子目录（`index.md` / `crud.md` / `lifecycle.md` / `inspection.md`），`assemblies` 拆为 `docs/api/assemblies/`，`production` 拆为 `docs/api/production/`（`index.md` + 6 子页：work-types / processes / work-type-process-mapping / process-chain / worker-pool / workers），`delivery-notes` 拆为 `docs/api/delivery-notes/`（4 子页）；2026-09-19 IAM 域合并：原 `auth.md` + `users.md` → `iam.md`；2026-09-19 prod 容器聚合：原 `workers.md` → `production/workers.md`，工种/工序/工艺链/工人池/工人 5 支撑域聚合为 `src/modules/prod/*`，URL 硬切换 `/api/v2/prod/*`）。**后端代码变更（新增 / 修改 / 删除端点，或修改 DTO 字段 / 错误码）必须立即同步更新对应模块文件**。

> 📌 **开发规约**：单文件职责、1000 行上限、SQL 防 N+1、单元测试覆盖（纯函数 100% / 含 IO 不强求）、函数 / 结构体 / 枚举注释规范等硬约定见 [docs/conventions.md](docs/conventions.md)。**新增 / 修改 `src/` 代码前必读**。

## 项目定位

`hsh-erp` monorepo 的 Rust 后端 v2 子模块（对应 `backend-rust/`），承担新功能域（iam / deliveryNote / scanInspect / part / assembly / outsource / cnc_program / part_file / production / _e2e 等 15 域）；历史业务域在兄弟子模块 `backend-python/`（FastAPI v1）。跨后端契约（共享 JWT_SECRET / 共享 PostgreSQL 库 / 雪花 ID 实例号分工等）见根仓库 [`../CLAUDE.md`](../CLAUDE.md) §跨子模块架构，本文件不重复。

> 📌 **2026-09-19 prod 容器聚合**（PR-N）：把 worker / work_type / process / process_chain / worker_pool 5 个支撑域平移至 `src/modules/prod/*`，URL 硬切换 `/api/v2/prod/*`，旧 nest 下线无 alias（前端配套 PR 锁步）。容器聚合采用 com 风格（各子模块保留独立六件套 + router()），非 iam 风格融合。**part / assembly 是 ERP 跨域核心实体（CLAUDE.md §「part 是跨域枢纽」），未并入 prod**；报工端点（worker-scan / pick-up / to-* / complete / 返修闭环）保留在 part 域，文档层在 [`docs/api/production/index.md` §生产全流程端点地图](docs/api/production/index.md) 串联。

**权威文档是 `docs/architecture.md`**——含完整技术栈选型理由、目录结构、Python→Rust 模块映射表、实施路线图。做任何架构决策前先读它；本文件只提炼不动脑就需要遵守的硬约定。

> 📌 **2026-09-17 PR-1/2/3/4 串联重大重构已完成**：
> - **PR-1**（migration 026）工艺链 FK 翻转：`t_part.process_chain_id` 承载 1:1 归属
> - **PR-2**（migration 027）t_part 瘦身 + t_assembly 删 `actual_delivery_date`：6 列从 t_part 迁至 t_part_batch 真相源；「位置 / 持有人 / 实际交付日期 / 返修事实」4 类派生走 service 层 + event-driven
> - **PR-3**（migration 028）批次 step 化：`t_part_batch.next_process_id` → `current_process_step_id`（逻辑 FK → t_process_chain_step.id）
> - **PR-4**（migration 029）卫生项：3 索引补齐 + 孤儿 `t_assembly_id_seq` 清理 + split_batch 合并 + assembly model/DDL 对齐 + locations/holder_ids 过滤 + chain step 引用计数
>
> 详见 [`docs/audit-5-tables-2026-09-16.md`](docs/audit-5-tables-2026-09-16.md) §附录 + [`docs/architecture.md` §7.5](docs/architecture.md)。**改 part / assembly / process_chain / part_batch 域前先读 PR-1/2/3/4 的契约变更，避免回退到已废弃字段（如 `t_part.location` / `t_part_batch.next_process_id` 列）。**

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
```

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

1. **事务边界在 service**：service 持 `Arc<dyn UowProvider>`，调用 `provider.begin().await?` 得 `Box<dyn UnitOfWork>`，所有跨 repo 操作经 UoW 访问器（`uow.user_repo().xxx()`），写端点 `uow.commit().await?`、读端点 drop（隐式回滚）。handler 薄壳化不接触 `pool.begin/commit`。
   - 例外清单（仍走 handler 边界）：
     - `_e2e` 直调方（测试 fixture 自管 tx）
     - 既有 `tests/iam_api.rs` 等 HTTP 契约测试（不改测试代码）
2. **统一响应信封**：handler 返回 `Result<Json<R<T>>, AppError>`。`R { code: 0, message: "ok", data }`；错误由 `AppError::into_response()` 装入同一信封。不做 middleware 后置包装。
3. **错误码分段契约**（`src/shared/error.rs::code`，与 Python 前端对齐）：0 成功、4xxxx HTTP 语义、5xxxx 系统、2xxxx 业务域（每域一个段，如 201xx 零件/客户、214xx 送货单，新增域错误码先入对应段）。
4. **权限在服务层**：`CurrentUser` 经 `FromRequestParts` 从 Bearer JWT 解析；JWT 验签后额外查 Redis（`session:tok:<sha256_hex>`）确认 session 仍有效，查不到 → 40105 SESSION_REVOKED。service 调 `user.require_role(Role::Manager)?` 守卫。五角色见 `src/auth/rbac.rs`；`ShelfAccount` 用 `can_access_shelf(id)` 校验货架范围；如需 token 哈希（如 logout），注入 `AuthTokenHash` extractor。
5. **状态机不写 DB**：`statemachine.rs` 只做内存 enum + `can_transition_to` 迁移表；事件日志由 service 在事务内统一插入。
6. **WS 广播在 commit 之后**（对齐 Python 延迟广播模式），用 `state.ws_hub.broadcast(...)`。
7. **路由挂载**：业务 REST 统一 `/api/v2`（与 Python `/api/v1` 并行），WS 在 `/ws/dashboard`。`/api/mcp` 不在本仓库。

## UoW 范式（service 控制事务）

- **按域各写各的 UoW**：user 域有 `user/uow.rs`（4 访问器），delivery_note 域应有 `delivery_note/uow.rs`，**禁止 17 域访问器堆进一个全局 UoW trait**
- **repo trait 独立 + automock**：4 个 repo trait 各自 `#[cfg_attr(test, automock)]`，方法名回归 `repo.rs` 固有命名（无前缀）
- **访问器模式**：UoW = 4 访问器 + `commit(self: Box<Self>)` / `rollback(self: Box<Self>)`，**不含 begin**
- **跨域原子写**：用 supertrait 组合 `trait SalesUoW: UnitOfWork + DeliveryNoteUoW {}` + blanket impl，**不**为跨域引入新 UoW trait 而污染单域接口
- **SharedTx 解法**：单域内多 repo 共享 `Arc<tokio::sync::Mutex<Option<Transaction<'static, Postgres>>>>`；访问器要 `&mut self`，同一 UoW 调用天然串行，锁只是内部可变性通道；Drop 即回滚

## DB 约定（迁移与查询必须沿用）

- 无物理外键（`bigint` + 索引，存在性由 service 校验）、无 DB ENUM（`varchar` + Rust enum 校验）
- 乐观锁：`version` 列，UPDATE 带 `WHERE id=$1 AND version=$2`，0 行 → 409 / `VERSION_CONFLICT`
- 软删除：`deleted_at IS NULL`；审计字段 `created_at/by`、`updated_at/by`
- 雪花主键：`SnowflakeIdGenerator::next_id()` App 侧生成
- i64 主键序列化为 JSON string（`shared/types.rs` 的 serde helper），防 JS 精度截断
- 时间列存 naive `timestamp`，写入用 `infra::clock::now_naive()`（Asia/Shanghai）
- 迁移命名：`<13位时间戳>_<顺序>_<描述>.sql`，见 `migrations/README.md`
- `repo.rs` 固有静态方法签名收 `impl PgExecutor<'_>`，与 Sqlx*Repo 实现委托兼容；`repo.rs` 是 UoW 的唯一真源，**零 diff 是硬 gate**
- service 持有 `Arc<dyn UowProvider>`，不持 `Arc<dyn Repo>`，不持 `PgPool`（pool 仅 `AppState` 与 `SqlxUowProvider` 持有）

## 环境要点

- 双 PG 容器（docker compose 分服务启动）：开发库 `postgres-dev` 在 **5430**（库 `hsh`），测试库 `postgres-test` 在 **5429**（库 `postgres_rust_test`）
- 配置全部走 `.env`（`infra/config.rs`）：`DATABASE_URL` 优先，缺省回退 `POSTGRES_*` 拆分变量拼接；测试库 URL 由 `build_test_database_url()` 构建（`DATABASE_TEST_URL` / `POSTGRES_TEST_*`）
- 优雅退出：`AppState.shutdown`（CancellationToken）同时通知 axum serve 与 `task/auto_complete` 后台循环
