# hsh-erp-rust 架构设计

> 本仓库为 Python FastAPI ERP 系统 `/Users/ren/Code/myERP` 的 Rust 重构版。
> **项目阶段**：基础设施（axum 路由 / 错误信封 / JWT+RBAC+session / 雪花 ID / WS 中枢 / DB 迁移）已就绪，19 个核心域（auth / users / applicants / customers / shelves / workers / work_types / processes / production / parts / assemblies / cnc_program / part_file / outsource / delivery_note / delivery_group / process_chain / _e2e + 1 WS stub）已完成业务 handler；仅 1 域（statistics）+ dashboard WS 握手待实施。详见 §7 当前进度。
> 本文档是骨架的"自述手册"，回答**目录为什么这样组织、各模块做什么、关键模式是什么**。

## 1. 技术栈

| 类别 | 选型 | 理由 |
|---|---|---|
| Web 框架 | **axum 0.8** | 与 Tokio 生态契合；tower-http layers 复用；`FromRequestParts` extractor 友好 |
| 异步运行时 | **tokio (full)** | axum 强制；后台任务与 WS 都需 |
| 数据库 | **sqlx 0.9 (postgres)** + 编译期宏 | 与 Python 端 asyncpg/SQLAlchemy 对位；编译期 SQL 检查减少重构漂移 |
| 序列化 | **serde + serde_json** | 标准选择 |
| 时间 | **chrono (FixedOffset)** | 业务统一 Asia/Shanghai（无 chrono-tz 依赖） |
| 认证 | **jsonwebtoken (HS256) + bcrypt** | Python 端 `pyjwt + bcrypt`，直接对应 |
| 会话存储 | **deadpool-redis 0.23 + redis 1** | 服务端 session 真相源；access token 吊销依赖；`session:tok:<sha256_hex>` 主条目 + `sessions:user:<id>` Set 索引 |
| 配置 | **dotenvy + std::env** | 透明、无框架魔法；如未来需要可换 `config` crate |
| 并发集合 | **dashmap** | WebSocket 连接注册表 |
| 优雅退出 | **tokio_util::CancellationToken** | 后台任务 + axum serve 同步退出 |
| 数值精度 | **rust_decimal** | 外协报价 / 装配体价格字段统一 decimal（避免 f64 漂移） |
| HTTP 客户端 | **reqwest** | COS XML API 签名 + e2e profile 健康检查 |
| Excel | **umya-spreadsheet** + **calamine** | 送货单打印模板生成（写）+ 工人导入等读 |
| PDF | **printpdf** + **lopdf** | 装配件多页 PDF 校验（页数匹配 children.len()+1）+ 送货单打印 PDF |
| 条码 | **barcoders** | 工人 badge_code / part 序列号条形码生成 |

> 所有 7 个追加依赖（`rust_decimal` / `reqwest` / `umya-spreadsheet` / `calamine` / `printpdf` / `lopdf` / `barcoders`）已在 `Cargo.toml` 落地（2026-09 阶段），不再是按需计划项。

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
    │   ├── config.rs              # AppConfig（含 JwtConfig / CosConfig / SnowflakeConfig / AutoCompleteConfig / RedisConfig / E2eConfig）
    │   ├── db.rs                  # sqlx PgPool 构建
    │   ├── redis.rs               # deadpool-redis 连接池构建
    │   ├── cos.rs                 # CosClient trait + NoopCos 占位
    │   ├── snowflake.rs           # 雪花 ID 生成器（位布局对齐 myERP Python `snowflake-id` 包：41+10+12）
    │   ├── serial.rs              # 业务单号/序列号
    │   ├── clock.rs               # Asia/Shanghai 时间
    │   └── ws_hub.rs              # WebSocket 广播中枢（hub 集成所有业务事件，握手 stub 待实装）
    │
    ├── shared/                    # 跨域共享
    │   ├── error.rs               # AppError enum + 错误码常量段 + IntoResponse（已超 1000 行，需拆）
    │   ├── response.rs            # R<T> 统一信封 + Page<T> 分页结构
    │   ├── types.rs               # IdStr：i64↔JSON string 的 serde helper
    │   └── pagination.rs          # PageQuery 通用分页
    │
    ├── state/                     # 应用 kernel
    │   └── mod.rs                 # AppState 全局对象
    │
    ├── task/                      # 后台任务
    │   └── auto_complete.rs       # DELIVERED→COMPLETED 定时循环（已实装，commit `f2e8bf1`）
    │
    ├── util/                      # 通用工具
    │   ├── barcode.rs
    │   ├── excel.rs
    │   └── pdf.rs
    │
    └── modules/                   # 业务域（垂直切片，20 域：18 业务 + 1 _e2e seed hook + 1 WS dashboard）
        ├── mod.rs                 # v2_router()/ws_router() 聚合 + /api/v2/health
        ├── auth/                  # 登录/refresh/改密
        ├── user/                  # 账号+角色+菜单
        ├── customer/              # 客户树
        ├── applicant/             # 申请人
        ├── worker/                # 工人
        ├── work_type/             # 工种
        ├── process/               # 工序
        ├── shelf/                 # 货架
        ├── worker_pool/           # 工人候选池（auto_allocate / refill / remove / take_one）
        ├── part/                  # ★核心：零件工单（含 statemachine.rs）
        │   ├── dto.rs / dto_crud.rs            # 请求/响应 DTO
        │   ├── handler.rs                      # axum handler（49 端点）
        │   ├── model.rs                        # TPart + 域枚举
        │   ├── repo/{part,batch,event}.rs      # sqlx 查询（按读 / 写 / 事件 拆）
        │   ├── service/{crud,inspection,inspection_core,lifecycle,worker_scan,rollup,phase1}.rs
        │   └── statemachine.rs                 # 状态机 + reorder_with_step_size helper（PR-B2）
        ├── assembly/              # 装配件（含 statemachine.rs + 子件 rollup）
        ├── cnc_program/           # CNC 程序（pairs 上传 + 列表，Phase 3）
        ├── part_file/             # 零件文件/图纸（kind 全统一为 t_part_file，Phase 3）
        ├── outsource/             # 外协域（company / quote / shipment 三子域，含 statemachine.rs）
        ├── delivery_note/         # 送货单（已按职责拆到极致）
        │   ├── handler.rs                      # 785 行
        │   ├── dto.rs / model.rs / statemachine.rs
        │   ├── print.rs / print_xml_patch.rs    # 打印模板 + XML 补丁
        │   ├── repo/{mod,query,mutate}.rs      # 读 / 写 分文件
        │   ├── service/{mod,crud,inner,lifecycle,group,scan,print,attach}.rs
        ├── process_chain/         # 工艺链（含 statemachine.rs，reorder_with_step_size helper）
        ├── statistics/            # 生产统计（占位，无 model.rs）
        ├── _e2e/                  # e2e seed hook（11 端点，dev/test 默认 / release 硬关，2026-09-14）
        └── dashboard/             # WebSocket 大屏（handler 骨架已搭，待握手实现）
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
| 200xx~ | 业务域 | 200xx 用户、201xx 零件/客户（PART_NOT_FOUND 20101 / ... / **20114 BIZ_PART_BATCH_NOT_HELD_BY_WORKER** worker-pool-take 新增 / **20115 PART_ALREADY_CANCELLED / 20116 PART_NOT_DELIVERED / 20117 PART_NOT_READY_TO_SHIP / 20118 PART_REPAIR_NOT_TRIGGERED / 20119 PART_NOT_DELETABLE** phase 1 新增）、202xx 工人（**20205 BIZ_WORKER_POOL_EMPTY / 20206 BIZ_WORKER_NO_WORK_TYPE** worker-pool-take 新增）、203xx 装配体（**20305 PDF_INVALID / 20306 CHILD_PRICE_LOCKED / 20307 HAS_SHIPMENT / 20308 CUSTOMER_NO_SERIAL_PREFIX** Phase 3 新增）、204xx/211xx 文件、205xx 货架（**20504 PROCESS_SHELF_NOT_FOUND / 20505 PROCESS_PROCESS_NOT_FOUND / 20506 NO_MATCH_FOR_PROCESS** process-mapping 新增）、206xx 账号、207xx 工艺链（**20701 PROCESS_CHAIN_NOT_FOUND / 20702 PROCESS_CHAIN_STEP_NOT_FOUND / 20703 WORK_TYPE_MAX_HELD_MINUTES_NOT_SET / 20704 AUTO_ALLOCATE_INVALID_RATIO** process_chain 域新增）、208xx 工序、209xx 工种（**20904 BIZ_WORK_TYPE_MAX_HELD_NOT_SET / 20905 BIZ_WORK_TYPE_NO_PROCESS_MAPPING** worker-pool-take 新增）、210xx 申请人、212xx 外协公司（含 **21207 DIRECT_REQUIRES_C2_SHELF / 21208 NO_SHELF**）、213xx 外协报价、214xx 送货单、215xx 外协发货（**21502 INVALID_TRANSITION / 21503 NO_OPEN / 21504 QUANTITY_EXCEEDS** Phase 2 shipment 新增） |

> 20113 BIZ_CUSTOMER_IN_USE 原与 20109 同号；worker-pool-take 阶段确认 20109 = BIZ_PART_BATCH_NOT_FOUND 后，20113 单作 BIZ_CUSTOMER_IN_USE 使用，20114 留给 BIZ_PART_BATCH_NOT_HELD_BY_WORKER。
> 21207 BIZ_OUTSOURCE_DIRECT_REQUIRES_C2_SHELF 在 `src/shared/error.rs::174` 已定义（不是 phantom code），direct 派外协校验触发；删除该码会破坏对外契约，**保留**。

业务域 2xxxx 段由各域实现阶段自行定义，**前置要求是给前端契约兼容**。

### 3.4 DI 与权限

- `Arc<AppState>` 作为 axum Router 的 state，含 `pool / config / snowflake / ws_hub / cos / session / shutdown` 七个字段。
- `CurrentUser` 实现 `FromRequestParts<Arc<AppState>>`：从 `Authorization` 头解析 Bearer JWT → 解码 Claims → 当 `REDIS_SESSION_CHECK_ENABLED=true`（默认）时用 sha256(token) 查 Redis（`session:tok:<hash>`）→ 用 `CachedCurrentUser` 构造 CurrentUser → EXPIRE 续期。查不到返回 40105 SESSION_REVOKED。当该开关关闭时跳过整段 Redis 查询，直接从 Claims 构造 CurrentUser——适用于借 Python 后端 JWT 的迁移过渡期（详见 `.env.example` 与 `docs/api/auth.md`）。
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

`state.shutdown.cancel()` 是单一退出点：

- `auto_complete::run` 在 `select!` 收到 `token.cancelled()` 后跳出循环、释放连接池；
- `axum::serve` 在 `with_graceful_shutdown(token.cancelled())` 钩子里停止接收新连接、等待 in-flight 请求完成后退出。
- 二者通过同一 `CancellationToken` 共享一个 cancel 语义，**保证 Ctrl-C 后无半截事务、无僵尸 WS 客户端**。

## 4. Python → Rust 模块映射

| Python myERP | Rust hsh-erp-rust |
|---|---|
| `main.py` | `src/main.rs` + `lib.rs` |
| `core/config.py` / `database.py` | `src/infra/config.rs` / `src/infra/db.rs` |
| `core/response.py` / `exception*.py` / `error_code.py` | `src/shared/response.rs` / `src/shared/error.rs` |
| `core/security.py` + `permission.py` | `src/auth/{jwt,password,rbac,session,extractor}.rs` |
| `core/cos.py` / `time.py` | `src/infra/{cos,clock}.rs` |
| `core/serial.py` | `src/infra/serial.rs`（业务单号 F1000/L1234） |
| `utils/id_gen.py`（→ `snowflake-id` PyPI 包 v1.0.2） | `src/infra/snowflake.rs`（位布局 `ts<<22 \| instance<<12 \| seq`，跨语言 ID 互解） |
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
# 1. 起本地 PG（dev / test 双库由 docker compose 启动；rust 子模块无内置脚本）
docker compose -f ../docker-compose-local.yml up -d postgres-dev postgres-test

# 2. 业务实现阶段：编写迁移并 apply
cargo run -- migrations run   # 或 main.rs 启动时自动 migrate!().run()

# 3. 在开发库上生成离线元数据（每次 query! 改动后必须重跑）
./scripts/sqlx_prepare.sh
# → 写入 .sqlx/query-*.json，必须 commit

# 4. CI / Docker 构建
SQLX_OFFLINE=true cargo build --release
```

开发库就绪后，`cargo check` 经 `query!` 宏在编译期连库校验 SQL；本地无库时设 `SQLX_OFFLINE=true` 用 `.sqlx/` 元数据构建。

## 7. 当前进度（2026-09-14）

> 阶段说明：✅ 已上线业务 handler；⏳ 占位待实装。

### 7.1 已上线（19 域 ~162 端点 + 1 WS stub）

| 域 | 端点 | 上线日期 | 关键变化 |
|---|---:|---|---|
| auth | 5 | 2026-08-26 | login / refresh / logout / 改密 / me |
| users | 9 | 2026-08-26 | 账号 CRUD + role / 菜单绑定 |
| applicants | 5 | 2026-08-26 | applicant CRUD + L1 customer 校验 |
| customers | 5 | 2026-08-26 | L1/L2 CRUD + OCC |
| shelves | 11 | 2026-08-26 | CRUD + picker + shelf-process mapping |
| workers | 7 | 2026-08-26 | CRUD + verify-badge + activate/deactivate |
| work_types / processes | 19（含生产管理子目录） | 2026-09-12 | 工种 + 工序 + 工序映射 + 工艺链 + 工人候选池（5 子域合 19 端点） |
| parts | **49** | 2026-09-14（Phase 1+2） | 19 CRUD/lifecycle/inspection 端点 + 30 Phase 1/2 状态机扩展 + 扫码/批量端点 |
| assemblies | **8** | 2026-09-14（Phase 3） | CRUD + multipart PDF + 子件派生 + start + files |
| cnc_program | 2 | 2026-09-14（Phase 3） | 配对上传 + 列表 |
| part_file | 3 | 2026-09-14（Phase 3） | 上传 + 列表 + 下载 URL |
| outsource | **16** | 2026-09-13（Phase 2） | company 7 + quote 8 + shipment 1 |
| delivery_notes | 18 | 2026-08-31 | P1–P4 业务 + 扫码 + 打印 + 分组 |
| delivery_groups | 4 | 2026-08-31 | P1 域（独立 nest） |
| process_chain | 2 | 2026-09-12 | GET/PUT by-part；含 reorder_with_step_size helper |
| _e2e | 11 | 2026-09-14 | seed hook（dev/test 默认；release profile 硬关） |
| worker_pool | 5 | 2026-09-12 | state / admin refill / remove + auto_allocate + admin 占位 |
| dashboard（WS） | 1（stub） | — | handler 骨架已搭，握手未实装 |
| **合计** | **~162** | — | **+1 WS stub** |

**关键里程碑**：

- ✅ `auto_complete` 后台循环已实装（commit `f2e8bf1`）：`DELIVERED→COMPLETED` 按 `t_delivery_note.delivery_date` 定时扫描；与 axum serve 共用 `state.shutdown` 优雅退出。
- ✅ WS hub 已注册全部业务事件（`DELIVERY_NOTE_*` / `PART_TO_*` / `BATCH_TO_*` / `WORKER_SCAN_*` / `WORKER_POOL_*` / `PART_DELIVERED` / `PART_BATCH_*` / `ASSEMBLY_*` / `WORKER_POOL_AUTO_ALLOCATE_DONE`），仅 dashboard handler 未做真实握手，前端连 `ws://.../ws/dashboard` 当前会卡到超时。

### 7.2 横切待完成（2 项）

| 项 | 位置 | 说明 |
|---|---|---|
| `statistics` 域占位 | `src/modules/statistics/` | 5 个聚合读端点（overview / workers / work_types / customers / throughput）空 router；可复用 part / assembly / delivery_note repo |
| `dashboard` WS 握手 | `src/modules/dashboard/handler.rs` | stub 7 行注释；JWT 验签 + Redis session 校验 + 注册到 hub + 首连 `DashboardSnapshot` 下发 |

### 7.3 集成测试文件清单（按模块）

`tests/` 下当前 24 个黑盒 spec：

| 文件 | 覆盖范围 |
|---|---|
| `tests/auth_api.rs` | login / refresh / logout / 改密 / 40105 SESSION_REVOKED |
| `tests/applicant_api.rs` | applicant CRUD + L1 customer 校验 + OCC |
| `tests/customer_api.rs` | L1/L2 CRUD + OCC + 软删引用校验 |
| `tests/shelf_api.rs` | shelves CRUD + picker + mapping |
| `tests/worker_api.rs` | workers CRUD + verify-badge + 40901 |
| `tests/work_type_api.rs` | 工种 CRUD + 工序映射 |
| `tests/process_api.rs` | 工序 CRUD |
| `tests/work_type_process_mapping_api.rs` | 工种-工序映射 |
| `tests/process_chain_api.rs` | 工艺链 GET/PUT by-part + reorder helper |
| `tests/worker_pool_api.rs` | state / refill / remove + take_one_from_pool CTE |
| `tests/worker_pool_auto_allocate_api.rs` | auto_allocate 按比例分配 |
| `tests/part_api.rs` + `part_crud.rs` + `part_lifecycle_api.rs` + `part_repair_api.rs` | part 全 49 端点 happy / 拆批 / OCC / 状态机守卫 |
| `tests/part_api_inspection_batches.rs` + `tests/part_api_to_inspection.rs` + `tests/part_api_to_ship.rs` + `tests/part_api_to_process.rs` | inspection 拆分 spec |
| `tests/part_batch_api.rs` + `tests/part_api_helpers.rs` | part_batches 子表 + helpers |
| `tests/part_file_api.rs` | part_file 上传 + 列表 + 下载 URL |
| `tests/cnc_program_api.rs` | CNC 配对上传 + 列表 |
| `tests/outsource_company_api.rs` + `tests/outsource_quote_api.rs` + `tests/outsource_send_receive_api.rs` | 外协三子域 |
| `tests/assembly_api.rs` + `tests/assembly_files_api.rs` + `tests/assembly_status_sync.rs` | assembly 8 端点 + 子件 auto-rollup 同步 |
| `tests/delivery_note_api.rs` + `tests/delivery_attach_batches_api.rs` + `tests/delivery_scan_api.rs` + `tests/delivery_print_api.rs` + `tests/delivery_group_api.rs` | delivery_notes 全套 |
| `tests/auto_complete_api.rs` | auto_complete 后台循环 |
| `tests/serial_api.rs` | 业务单号 / 序列号派发 |
| `tests/_e2e_api.rs` | e2e seed hook 11 端点（受 profile toggle 控制） |
| `tests/common/` | DB / Redis / AppState 构造基建 |

### 7.4 数据库迁移

22 个 SQL 文件，最新 `20260914100000_022_create_e2e_seeded_table.sql`（2026-09-14）。
后续 2026-09-16/17 追加 7 个：023-029（含 PR-1 工艺链 FK 翻转 026 + PR-2 t_part 瘦身 027 + PR-3 批次 step 化 028 + PR-4 索引补齐 + 孤儿序列清理 029）。迁移最新编号：`20260916140000_029_add_missing_indexes_and_drop_orphan_seq.sql`。

迁移命名：`<13位时间戳>_<顺序>_<描述>.sql`，详见 `migrations/README.md`。

### 7.5 2026-09-17 重大重构（PR-1/2/3/4 串联）

2026-09-16/17 完成 3 项重大重构 + 1 项卫生项，工艺链 / 工单 / 批次数据模型均经历结构调整：

| PR | migration | 核心改动 | 关键端点 / 错误码 |
|---|---|---|---|
| **PR-1**（2026-09-16） | 026 | 工艺链 FK 翻转：`t_part_process_chain` 删 `part_id`，`t_part` 加 `process_chain_id`（1:1 binding） | 新增 `GET /api/v2/process-chains/{chain_id}`；新增错误码 `20705 BIZ_PROCESS_CHAIN_PART_NOT_PENDING`（part 非 PENDING 禁 PUT 工艺链） |
| **PR-2**（2026-09-16） | 027 | t_part 瘦身：删 `actual_delivery_date` / `location` / `current_holder_id` / `placed_at` / `delivery_note_id` / `has_been_repaired` 6 列（真相源迁至 `t_part_batch`）；t_part_batch 删 `has_been_repaired`；t_assembly 删 `actual_delivery_date` | 前端 `PartListItem` 加 `location` / `holder_name` 派生字段（service 层 min-progress 活跃批次派生） |
| **PR-3**（2026-09-16/17） | 028 | 批次 step 化：`t_part_batch.next_process_id` → `current_process_step_id`（逻辑 FK → t_process_chain_step.id）；删 `placed_at` | 新增错误码 `20706 BIZ_PROCESS_CHAIN_REQUIRED`（to_process / place_on_shelf / send_to_outsource 等"进入生产流"端点要求 part 已绑链）；`PartListItem.next_process_name` 派生 |
| **PR-4**（2026-09-17） | 029 | 卫生项：B1 补 3 索引（`ix_t_part_batch_parent_batch_id` / `ix_t_part_system_delivery_date` / `ix_t_assembly_name`）；B4 清理孤儿 `t_assembly_id_seq` | A1 工序软删守卫补 `t_process_chain_step` 引用计数；A2 `GET /parts` 加 `locations` / `holder_ids` 过滤参数；B2 split_batch 公共函数合并；B3 `TAssembly.request_date` / `planned_delivery_date` 与 DDL NOT NULL 对齐去 Option |

跨 PR 联动要点：
- **「下一步工序」概念统一**：part → chain → step 是新工艺引用通道（PR-1 翻转）；batch 持有 `current_process_step_id`（PR-3 替换 `next_process_id` 列）；service 层按 `min-progress 活跃 batch.current_process_step_id JOIN step.process_id` 派生 `PartListItem.next_process_name`（PR-3）
- **「位置 / 持有人」统一派生**：t_part 已无 location / current_holder_id 列（PR-2 删），service 层从 t_part_batch 派生（PR-2 列表 + PR-4 locations/holder_ids 过滤）
- **「实际交付日期」统一派生**：t_part / t_assembly 已无 actual_delivery_date 列（PR-2 删），statistics 域统一查 `t_part_event.event_type='DELIVERED'`
- **「返修事实」统一派生**：t_part / t_part_batch 已无 has_been_repaired 列（PR-2 删），由 `t_part_event.event_type='REPAIR_STARTED'` + `t_part_batch.status='REPAIRING'` 体现

详细修复路线图见 [`docs/audit-5-tables-2026-09-16.md`](audit-5-tables-2026-09-16.md) §附录。

---

## 8. 验证

骨架当前状态（`hsh-erp/backend-rust/` 子模块根目录执行；PG 由 monorepo 根的 docker compose 拉起，子模块内无 `scripts/dev_db.sh`）：

```bash
cd /Users/ren/Code/hsh-erp/backend-rust

# 1. 编译检查（要求 query! 元数据已 commit 进 .sqlx/）
cargo check --offline                       # 或 SQLX_OFFLINE=true cargo check
cargo clippy --all-targets --offline
cargo test --no-run --offline               # 仅编译测试，不连 DB

# 2. 跑测试（需要 postgres-test 5432 端口可达；推荐 docker compose 起）
docker compose -f ../docker-compose-local.yml up -d postgres-test
cargo test                                  # 24 个集成 spec
```

实际运行全栈（用根仓库 docker compose 编排）：

```bash
cp .env.example .env
# 编辑 .env 填入 DATABASE_URL / JWT_SECRET / COS_* 等
docker compose -f ../docker-compose-local.yml up -d postgres redis backend rust-backend frontend
```