# hsh-erp-rust 架构设计

> 本仓库为 Python FastAPI ERP 系统 `/Users/ren/Code/myERP` 的 Rust 重构版。
> **当前为骨架阶段**——目录结构已搭好、框架级类型已实现，业务模块留待实施。
> 本文档是骨架的"自述手册"，回答**目录为什么这样组织、各模块做什么、关键模式是什么**。

## 1. 技术栈

| 类别 | 选型 | 理由 |
|---|---|---|
| Web 框架 | **axum 0.8** | 与 Tokio 生态契合；tower-http layers 复用；`FromRequestParts` extractor 友好 |
| 异步运行时 | **tokio (full)** | axum 强制；后台任务与 WS 都需 |
| 数据库 | **sqlx 0.8 (postgres)** + 编译期宏 | 与 Python 端 asyncpg/SQLAlchemy 对位；编译期 SQL 检查减少重构漂移 |
| 序列化 | **serde + serde_json** | 标准选择 |
| 时间 | **chrono (FixedOffset)** | 业务统一 Asia/Shanghai（无 chrono-tz 依赖） |
| 认证 | **jsonwebtoken (HS256) + bcrypt** | Python 端 `pyjwt + bcrypt`，直接对应 |
| 会话存储 | **deadpool-redis 0.23 + redis 1** | 服务端 session 真相源；access token 吊销依赖；`session:tok:<sha256_hex>` 主条目 + `sessions:user:<id>` Set 索引 |
| 配置 | **dotenvy + std::env** | 透明、无框架魔法；如未来需要可换 `config` crate |
| 并发集合 | **dashmap** | WebSocket 连接注册表 |
| 优雅退出 | **tokio_util::CancellationToken** | 后台任务 + axum serve 同步退出 |

后续实施按需追加：`rust_decimal`、`reqwest`（COS XML API 签名）、`umya-spreadsheet` / `calamine`（Excel）、`printpdf` / `lopdf`（PDF）、`barcoders`（条码）、`testcontainers-rs`（集成测试）、`rmcp`（MCP 协议）。

## 2. 目录结构

```
hsh-erp-rust/
├── Cargo.toml                     # 依赖清单
├── .env.example                   # 环境变量样板
├── .gitignore
├── .sqlx/                         # sqlx 离线元数据（入版本库）
│   └── .gitkeep
├── migrations/                    # sqlx migrate 迁移
│   └── README.md                  # 迁移规范
├── docs/architecture.md           # 本文档
├── template/                      # 送货单 xlsx 模板（从 myERP 复制）
├── scripts/
│   ├── dev_db.sh                  # 启动本地 PG
│   └── sqlx_prepare.sh            # 生成 .sqlx 元数据
├── tests/                         # 集成测试基建 + README
├── docker-compose.yml             # 本地 PG（5433）
├── Dockerfile                     # 多阶段构建
└── src/
    ├── main.rs                    # 入口：装配 + serve + 优雅退出
    ├── lib.rs                     # 库根（集成测试可见）
    │
    ├── auth/                      # 横切认证授权（对应 myERP/core/security.py + permission.py）
    │   ├── jwt.rs                 # access/refresh 双 token 编解码
    │   ├── password.rs            # bcrypt 散列/校验
    │   ├── rbac.rs                # Role 五角色 + CurrentUser + Claims
    │   ├── session.rs             # SessionStore trait + RedisSessionStore 实现（CachedSession / hash_token）
    │   └── extractor.rs           # FromRequestParts<Arc<AppState>> for CurrentUser + AuthTokenHash
    │
    ├── infra/                     # 外部资源封装（含配置）
    │   ├── config.rs              # AppConfig（含 JwtConfig / CosConfig / SnowflakeConfig / AutoCompleteConfig / RedisConfig）
    │   ├── db.rs                  # sqlx PgPool 构建
    │   ├── redis.rs               # deadpool-redis 连接池构建
    │   ├── cos.rs                 # CosClient trait + NoopCos 占位
    │   ├── snowflake.rs           # 雪花 ID 生成器
    │   ├── serial.rs              # 业务单号/序列号（占位）
    │   ├── clock.rs               # Asia/Shanghai 时间
    │   └── ws_hub.rs              # WebSocket 广播中枢
    │
    ├── shared/                    # 跨域共享
    │   ├── error.rs               # AppError enum + 错误码常量段 + IntoResponse
    │   ├── response.rs            # R<T> 统一信封 + Page<T> 分页结构
    │   ├── types.rs               # IdStr：i64↔JSON string 的 serde helper
    │   └── pagination.rs          # PageQuery 通用分页
    │
    ├── state/                     # 应用 kernel
    │   └── mod.rs                 # AppState 全局对象
    │
    ├── task/                      # 后台任务
    │   └── auto_complete.rs       # DELIVERED→COMPLETED 定时循环
    │
    ├── util/                      # 通用工具（占位）
    │   ├── barcode.rs
    │   ├── excel.rs
    │   └── pdf.rs
    │
    └── modules/                   # 业务域（垂直切片，16 域）
        ├── mod.rs                 # v2_router()/ws_router() 聚合 + /api/v2/health
        ├── auth/                  # 登录/refresh/改密
        ├── user/                  # 账号+角色+菜单
        ├── customer/              # 客户树
        ├── applicant/             # 申请人
        ├── worker/                # 工人
        ├── work_type/             # 工种
        ├── process/               # 工序
        ├── shelf/                 # 货架
        ├── part/                  # ★核心：零件工单（含 statemachine.rs）
        ├── assembly/              # 装配件（含 statemachine.rs，子件 rollup）
        ├── cnc_program/           # CNC 程序
        ├── part_file/             # 零件文件/图纸
        ├── outsource/             # 外协域（含 statemachine.rs）
        ├── delivery_note/         # 送货单（含 statemachine.rs + 打印）
        ├── statistics/            # 生产统计（无 model.rs）
        └── dashboard/             # WebSocket 大屏
```

> `/api/mcp` 不在本仓库——AI 只读入口由独立 MCP 服务器承载（用户决策）。

### 2.1 为什么垂直切片而非水平分层

本 ERP 的核心依赖是 **part**（被 delivery_note、assembly、outsource、part_file、statistics、shelves 几乎所有模块直接依赖）。纯水平分层（api/service/repository/model 一字铺开）虽简单，但迁移对照清晰；垂直切片在 part 单点拆开后，其他域共同依赖 part 而无横向圈依赖。

**两种风格的边界同样清晰**，本项目选垂直的原因：
1. 与 myERP `api/v1/<mod>.py`、`service/<mod>.py` 等命名一一对应，**逐域迁移**（先 part、再 assembly、再 delivery_note……）可成批进行。
2. part 是跨域枢纽但本身边界清晰：垂直切片让 part 的内部复杂度（500+ 行 service、50  行 api）隔离在 `modules/part/`。
3. 同一域内的 handler/service/repo/model/dto **放在一起**，IDE 跳转无需跨层。

每个域的标准六件套（部分域简化，详见下表）：

| 文件 | 职责 | 对应 myERP |
|---|---|---|
| `handler.rs` | axum handler + 路由注册 | `api/v1/<mod>.py` |
| `service.rs` | 业务逻辑（签名接收 `&mut PgConnection`） | `service/<mod>_service.py` |
| `repo.rs` | sqlx 查询（签名 `impl PgExecutor<'_>`） | `repository/<mod>_repository.py` |
| `model.rs` | 表行模型 + 域枚举 | `model/<mod>.py` |
| `dto.rs` | 请求/响应 DTO | `schema/<mod>.py` |
| `statemachine.rs` | 仅 part/assembly/delivery_note/outsource 四域 | `statemachines/<mod>_state_machine.py` |
| `mod.rs` | 模块声明 + `pub fn router()` | — |

特殊域文件结构：

| 域 | 文件数 | 说明 |
|---|---|---|
| statistics | 5（无 model.rs） | 纯读聚合，无表 |
| auth | 4（无 model/repo） | 登录/refresh 业务；JWT/RBAC 在顶层 `crate::auth` |
| dashboard | 4（无 model/repo） | WebSocket handler + snapshot 聚合 |

## 3. 关键架构模式

### 3.1 事务边界在 handler

```rust
// handler.rs 伪代码
async fn create_part(
    State(state): State<Arc<AppState>>,
    user: CurrentUser,
    Json(req): Json<CreatePartReq>,
) -> Result<Json<R<PartOut>>, AppError> {
    user.require_role(Role::Manager)?;
    let mut tx = state.pool.begin().await?;        // ← tx 起点
    let part = PartService::create(&mut tx, req, &user).await?;
    tx.commit().await?;                            // ← 显式 commit
    Ok(Json(R::ok(PartOut::from(part))))
}
```

- Service 方法签名：`&mut PgConnection`（不是 `&mut Transaction`——`&mut Transaction` 自动 deref 到 `&mut PgConnection`，传递更灵活）。
- Repository 函数签名：`impl PgExecutor<'_>`——同时接受 `&PgPool` / `&mut PgConnection` / `&mut Transaction`，由调用方决定。
- 失败时 `tx` Drop 自动回滚，无需显式 `tx.rollback()`。

### 3.2 统一响应信封

handler 返回 `Result<Json<R<T>>, AppError>`：

- `Ok` 分支：axum 序列化 `R { code: 0, message: "ok", data: Some(T) }`。
- `Err` 分支：`AppError::into_response()` 把错误装入 `R { code, message, data: None }`，并按错误码设置 HTTP 状态。

不做 middleware 后置包装（Python 的 `UnifiedResponseMiddleware` 在 Rust 里需要缓冲全响应，性能差且破坏流式响应）。

### 3.3 错误码分段契约

`src/shared/error.rs::code` 模块定义全局错误码常量，与 Python 端对齐：

| 段 | 范围 | 示例 |
|---|---|---|
| 0 | SUCCESS | `code::SUCCESS = 0` |
| 40000~41xxx | HTTP 语义 | `BAD_REQUEST=40000`、`VALIDATION_ERROR=40001`、`UNAUTHORIZED=40100`、`FORBIDDEN=40300`、`NOT_FOUND=40400`、`VERSION_CONFLICT=40901`、`REQUEST_TOO_LARGE=41301` |
| 50000~ | 系统 | `INTERNAL=50000`、`DATABASE=50001` |
| 200xx~ | 业务域 | 200xx 用户、201xx 零件/客户（**20114 BIZ_PART_BATCH_NOT_HELD_BY_WORKER** worker-pool-take 新增）、202xx 工人（**20205 BIZ_WORKER_POOL_EMPTY / 20206 BIZ_WORKER_NO_WORK_TYPE** worker-pool-take 新增）、203xx 装配体、204xx/211xx 文件、205xx 货架、206xx 账号、208xx 工序、209xx 工种（**20904 BIZ_WORK_TYPE_MAX_HELD_NOT_SET / 20905 BIZ_WORK_TYPE_NO_PROCESS_MAPPING** worker-pool-take 新增）、210xx 申请人、212xx 外协公司、213xx 外协报价、214xx 送货单、215xx 外协发货 |

> 20113 BIZ_CUSTOMER_IN_USE 原与 20109 同号；worker-pool-take 阶段确认 20109 = BIZ_PART_BATCH_NOT_FOUND 后，20113 单作 BIZ_CUSTOMER_IN_USE 使用，20114 留给 BIZ_PART_BATCH_NOT_HELD_BY_WORKER。

业务域 2xxxx 段由各域实现阶段自行定义，**前置要求是给前端契约兼容**。

### 3.4 DI 与权限

- `Arc<AppState>` 作为 axum Router 的 state，含 `pool / config / snowflake / ws_hub / cos / session / shutdown` 七个字段。
- `CurrentUser` 实现 `FromRequestParts<Arc<AppState>>`：从 `Authorization` 头解析 Bearer JWT → 解码 Claims → sha256(token) 查 Redis（`session:tok:<hash>`）→ 用 `CachedCurrentUser` 构造 CurrentUser → EXPIRE 续期。查不到返回 40105 SESSION_REVOKED。
- `AuthTokenHash`（同 impl）给需要原始 token 哈希的端点（如 logout）使用。
- **角色守卫**：服务层调用 `user.require_role(Role::Manager)?`（Command 守卫）。

```rust
// usage
async fn handler(user: CurrentUser, ...) -> Result<...> {
    user.require_role(Role::Manager)?;  // 任意角色：require_any_role(&[Role::Clerk, Role::Manager])
    user.require_role(Role::ShelfAccount)?;  // 货架一体机
    // ... 业务
}
```

`SHELF_ACCOUNT` 用户被 scope 到 `shelf_ids` + `shelf_wildcard`，业务侧用 `user.can_access_shelf(id)` 校验。

### 3.5 状态机

四域 `statemachine.rs` 手写 enum + `match` 迁移表（`can_transition_to`）：

```rust
pub enum PartState {
    Pending, Programming, OnShelf, WithWorker, Inspection,
    ReadyToShip, Delivered, Repairing, Outsource,
    Completed, Cancelled,
}

impl PartState {
    pub fn can_transition_to(&self, next: &PartState) -> bool {
        use PartState::*;
        match (self, next) {
            (Pending, Programming | OnShelf | Cancelled | Outsource) => true,
            (Programming, OnShelf | Repairing | Cancelled) => true,
            // ... 共 ~30 条迁移规则
            _ => false,
        }
    }
}
```

状态机**不写 DB**——只操作内存中的 domain 对象；事件日志由 service 在事务内统一插入。

### 3.6 DB 约定（迁移时沿用）

- **无物理外键**：`cross_table_id bigint` + 索引，存在性由 service 校验。
- **无 DB ENUM**：`status varchar(N)`，合法性由 Rust enum 在 service 层校验。
- **乐观锁**：`version integer NOT NULL DEFAULT 0`；UPDATE 时 `WHERE id=$1 AND version=$2`，影响 0 行返回 409。
- **软删除**：`deleted_at timestamp NULL`；查询统一 `WHERE deleted_at IS NULL`。
- **审计字段**：`created_at`、`created_by`、`updated_at`、`updated_by`。
- **雪花主键**：`id bigint NOT NULL`，App 侧 `SnowflakeIdGenerator::next_id()` 生成。
- **JSON 输出**：`#[serde(serialize_with = "crate::shared::types::serialize_i64")]`，避免 `Number.MAX_SAFE_INTEGER` 精度截断。

### 3.7 WebSocket

`infra/ws_hub.rs` 提供：

```rust
let hub: Arc<WsHub> = ...;

hub.broadcast(WsEvent::DashboardEvent { kind: "PICKED_UP".into(), payload: json!({...}) });
hub.send_to(user_id, WsEvent::Notification { user_id, content: "...".into() });
```

实施阶段在 commit 成功后再调 `broadcast`（对齐 Python 的 `session.info` 延迟广播模式），避免慢 WS 拖慢 HTTP 响应。

### 3.8 后台任务

```rust
let token = CancellationToken::new();
tokio::spawn(task::auto_complete::run(state.clone(), token.clone()));
```

`run` 函数用 `tokio::select!` 等待定时器 tick 与 `token.cancelled()` 竞态。Ctrl-C 触发后：

```rust
async move {
    tokio::signal::ctrl_c().await.ok();
    state.shutdown.cancel();  // 同时通知 auto_complete 任务和 axum serve
}
```

## 4. Python → Rust 模块映射

| Python myERP | Rust hsh-erp-rust |
|---|---|
| `main.py` | `src/main.rs` + `lib.rs` |
| `core/config.py` / `database.py` | `src/infra/config.rs` / `src/infra/db.rs` |
| `core/response.py` / `exception*.py` / `error_code.py` | `src/shared/response.rs` / `src/shared/error.rs` |
| `core/security.py` + `permission.py` | `src/auth/{jwt,password,rbac,session,extractor}.rs` |
| `core/cos.py` / `serial.py` / `time.py` | `src/infra/{cos,serial,clock}.rs` + `snowflake.rs` |
| `state` 聚合 | `src/state/mod.rs`（AppState kernel） |
| `api/deps.py` | `src/auth/extractor.rs` + `Arc<AppState>` |
| `api/v1/<mod>.py`（19 个，drawing→part_file、ws→dashboard、print 并入 delivery_note） | `src/modules/<域>/handler.rs`（**路由暴露在 `/api/v2`**，与 v1 并行） |
| `api/mcp/*` | **不在本仓库**——独立 MCP 服务器承载 |
| `service/<mod>.py`（20 个） | `src/modules/<域>/service.rs` |
| `service/auto_complete.py` | `src/task/auto_complete.rs` |
| `repository/<mod>.py`（26 个） | `src/modules/<域>/repo.rs` |
| `model/<mod>.py` + `enums.py` | `src/modules/<域>/model.rs`（枚举就近入域） |
| `schema/<mod>.py` + `_types.py` | `src/modules/<域>/dto.rs` + `src/shared/types.rs` |
| `statemachines/`（4 个） | `src/modules/<域>/statemachine.rs`（part/assembly/delivery_note/outsource） |
| `utils/*` | `src/util/` |
| `alembic/` | `migrations/`（sqlx migrate） |
| `template/*.xlsx` | `template/`（直接复制） |
| `.env.example` | `.env.example` |
| `docker-compose.yml` | `docker-compose.yml`（PG 端口 5433） |

## 5. 接口版本策略

**重构版业务 REST 接口统一 `/api/v2`**——与原 Python 版 `/api/v1` 并行共存，前端可灰度切换。

```
Nginx 反代（不变）
   ├── /api/v1/*  → Python 后端（不变）
   └── /api/v2/*  → Rust 后端（本仓库）
```

非版本化入口：

- `/ws/dashboard` — WebSocket 大屏（两版本需独立部署）
- `/api/mcp/*` — **不在本仓库**，由独立 MCP 服务器承载

实施阶段需在 nginx 侧显式分流，避免 v1/v2 路径冲突。

## 6. sqlx 离线模式工作流

```bash
# 1. 起本地 PG
./scripts/dev_db.sh

# 2. 业务实现阶段：编写迁移并 apply
cargo run -- migrations run   # 或 main.rs 启动时自动 migrate!().run()

# 3. 在开发库上生成离线元数据（每次 query! 改动后必须重跑）
DATABASE_URL=... ./scripts/sqlx_prepare.sh
# → 写入 .sqlx/query-*.json，必须 commit

# 4. CI / Docker 构建
SQLX_OFFLINE=true cargo build --release
```

骨架阶段无 `query!` 宏调用，`cargo check` 不依赖数据库。

## 7. 实施路线图（建议）

按"先核心后外圈"的顺序：

1. **数据层**
   - sqlx migrate 初始化（squash myERP alembic 31 个迁移为 ~10 个 Rust SQL）
   - 21 张表 + `query!`/`FromRow` 模型到 `modules/part/model.rs` 试水
2. **认证域**
   - `modules/user/` + `modules/auth/`（最高 ROI：解耦前能并行开发其他域）
3. **零件工单核心**
   - `modules/part/`（包括 `part_batch` / `part_event` / `pickup_skip_event` / `serial_counter`）
   - 同步实施 `modules/customer/` `modules/shelf/` `modules/worker/` `modules/process/` `modules/work_type/`（part 强依赖）
4. **装配体、送货单、外协、worker-pool**
   - `modules/assembly/`（含 rollup）
   - `modules/delivery_note/`（含打印）

   > 送货单域的扫码建单 / 规则分类 / 勾选打印标签的完整设计见 `docs/delivery-note-redesign.md`。

   - `modules/outsource/`（company/quote/shipment）
   - **`modules/worker_pool/`**（✅ **worker-pool-take 已完成**：`POST /parts/worker-scan` 同事务联动 `refill_for_worker`、`POST /admin/worker-pool/refill`、`POST /admin/worker-pool/remove`、`GET /worker-pool/state`；错误码 20114/20205/20206/20904/20905；WS 5 类 `WORKER_*` 事件已注册）。详细文档见 [`docs/api/worker-pool.md`](api/worker-pool.md) + [`docs/api/parts.md`](api/parts.md#post-apiv2partsworker-scan)。
5. **文件、CNC、统计**
   - `modules/part_file/` + COS trait 实现（`infra/cos.rs`）
   - `modules/cnc_program/`
   - `modules/statistics/`
6. **横切补完**
   - `task/auto_complete.rs` 业务查询
   - `modules/dashboard/` WebSocket 完整实现（worker-pool WS 事件已注册，待 hub 真实握手后即可下发）
   - `modules/mcp/` MCP tool 暴露**（本骨架不含——MCP 由独立服务器承载）**
   - 集成测试 `tests/common/mod.rs` + 各域场景
7. **运维**
   - `infra/serial.rs` 业务单号
   - 文档、CI、监控

## 8. 验证

骨架当前状态：

```bash
cd /Users/ren/Code/hsh-erp-rust
cargo check                # ✓ 通过（无 query! 调用，不依赖 DB）
cargo clippy --all-targets # ✓ 通过
cargo test                 # ✓ 通过（含 snowflake 单测）
```

实际运行需：

```bash
cp .env.example .env
# 编辑 .env 填入 DATABASE_URL / JWT_SECRET / COS_* 等
./scripts/dev_db.sh
cargo run
```