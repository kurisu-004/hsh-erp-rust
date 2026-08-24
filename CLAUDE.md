# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

> 📌 **前端对接**：后端 API 参考见 [`docs/api/`](docs/api/index.md)（[index.md](docs/api/index.md) 为总入口，含通用约定 + 跨域错误码速查；按模块拆分为 `auth.md` / `users.md` / `delivery-notes.md` / `delivery-groups.md` / `websocket.md`）。**后端代码变更（新增 / 修改 / 删除端点，或修改 DTO 字段 / 错误码）必须立即同步更新对应模块文件**。

## 项目定位

Python FastAPI ERP（`/Users/ren/Code/myERP`）的 Rust 重构版。**当前为骨架阶段**：目录结构与框架级类型（错误、响应信封、JWT/RBAC、雪花 ID、WS 中枢、AppState）已实现，16 个业务域的 handler/service/repo/model/dto 均为占位。

**权威文档是 `docs/architecture.md`**——含完整技术栈选型理由、目录结构、Python→Rust 模块映射表、实施路线图。做任何架构决策前先读它；本文件只提炼不动脑就需要遵守的硬约定。

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

`src/modules/<域>/` 标准六件套，与 myERP 文件一一对应（迁移时逐域对照）：

| 文件 | 职责 | 对应 myERP |
|---|---|---|
| `handler.rs` | axum handler + 路由 | `api/v1/<mod>.py` |
| `service.rs` | 业务逻辑，签名收 `&mut PgConnection` | `service/<mod>_service.py` |
| `repo.rs` | sqlx 查询，签名收 `impl PgExecutor<'_>` | `repository/<mod>_repository.py` |
| `model.rs` / `dto.rs` | 表行模型+域枚举 / 请求响应 DTO | `model/` / `schema/` |
| `statemachine.rs` | 仅 part / assembly / delivery_note / outsource 四域 | `statemachines/` |

**part 是跨域枢纽**（delivery_note、assembly、outsource、part_file、statistics、shelf 均依赖它），实施顺序见 architecture.md 第 7 节。

## 必须遵守的架构约定

1. **事务边界在 handler**：handler 里 `state.pool.begin()` → 传 `&mut tx` 给 service → 显式 `tx.commit()`；Drop 自动回滚。repo 用 `impl PgExecutor<'_>` 以同时接受 pool/conn/tx。
2. **统一响应信封**：handler 返回 `Result<Json<R<T>>, AppError>`。`R { code: 0, message: "ok", data }`；错误由 `AppError::into_response()` 装入同一信封。不做 middleware 后置包装。
3. **错误码分段契约**（`src/shared/error.rs::code`，与 Python 前端对齐）：0 成功、4xxxx HTTP 语义、5xxxx 系统、2xxxx 业务域（每域一个段，如 201xx 零件/客户、214xx 送货单，新增域错误码先入对应段）。
4. **权限在服务层**：`CurrentUser` 经 `FromRequestParts` 从 Bearer JWT 解析；JWT 验签后额外查 Redis（`session:tok:<sha256_hex>`）确认 session 仍有效，查不到 → 40105 SESSION_REVOKED。service 调 `user.require_role(Role::Manager)?` 守卫。五角色见 `src/auth/rbac.rs`；`ShelfAccount` 用 `can_access_shelf(id)` 校验货架范围；如需 token 哈希（如 logout），注入 `AuthTokenHash` extractor。
5. **状态机不写 DB**：`statemachine.rs` 只做内存 enum + `can_transition_to` 迁移表；事件日志由 service 在事务内统一插入。
6. **WS 广播在 commit 之后**（对齐 Python 延迟广播模式），用 `state.ws_hub.broadcast(...)`。
7. **路由挂载**：业务 REST 统一 `/api/v2`（与 Python `/api/v1` 并行），WS 在 `/ws/dashboard`。`/api/mcp` 不在本仓库。

## DB 约定（迁移与查询必须沿用）

- 无物理外键（`bigint` + 索引，存在性由 service 校验）、无 DB ENUM（`varchar` + Rust enum 校验）
- 乐观锁：`version` 列，UPDATE 带 `WHERE id=$1 AND version=$2`，0 行 → 409 / `VERSION_CONFLICT`
- 软删除：`deleted_at IS NULL`；审计字段 `created_at/by`、`updated_at/by`
- 雪花主键：`SnowflakeIdGenerator::next_id()` App 侧生成
- i64 主键序列化为 JSON string（`shared/types.rs` 的 serde helper），防 JS 精度截断
- 时间列存 naive `timestamp`，写入用 `infra::clock::now_naive()`（Asia/Shanghai）
- 迁移命名：`<13位时间戳>_<顺序>_<描述>.sql`，见 `migrations/README.md`

## 环境要点

- 双 PG 容器（docker compose 分服务启动）：开发库 `postgres-dev` 在 **5430**（库 `hsh`），测试库 `postgres-test` 在 **5429**（库 `postgres_rust_test`）
- 配置全部走 `.env`（`infra/config.rs`）：`DATABASE_URL` 优先，缺省回退 `POSTGRES_*` 拆分变量拼接；测试库 URL 由 `build_test_database_url()` 构建（`DATABASE_TEST_URL` / `POSTGRES_TEST_*`）
- 优雅退出：`AppState.shutdown`（CancellationToken）同时通知 axum serve 与 `task/auto_complete` 后台循环
