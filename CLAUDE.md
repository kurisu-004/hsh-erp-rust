# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

> 📌 **前端对接**：后端 API 参考见 [`docs/api/`](docs/api/)
## 常用命令

```bash
docker compose up -d postgres-dev    # 开发库（localhost:5430，库 hsh）：cargo run 与 query! 编译期校验依赖
#    集成测试请直接走 scripts/test_nextest.sh（自带容器）

cargo check                 # 已有 query! 宏：编译期经 .env 的 DATABASE_URL 连开发库校验；无库时用 SQLX_OFFLINE=true（.sqlx 已提交）
cargo clippy --all-targets
cargo run                   # 需先 cp .env.example .env 并起 postgres-dev

./scripts/sqlx_prepare.sh   # 每次 query! 宏改动后必须重跑，生成 .sqlx/query-*.json 并提交
SQLX_OFFLINE=true cargo build --release   # CI/Docker 用离线元数据构建

# 一次性安装 nextest
cargo install cargo-nextest --locked

# 全量并行测试（推荐；走 test_nextest.sh 起 session 级容器 + per-test database）
scripts/test_nextest.sh

# ⚠️ 2026-09-30 起 5429 被孤儿容器占着，此路当前不可用 → 走 scripts/test_nextest.sh
TEST_DATABASE_BASE_URL=postgres://hsh_test:6065161test@localhost:5429 cargo nextest run

# 手工应用 seeds（菜单等配置数据；通常 app 启动钩子自动跑）
./scripts/seed_apply.sh

# worktree 的独立容器（见「worktree 服务」一节；日常无需手工调用，skill 自动调）
./scripts/wt_services.sh up <slug>    # 建/复用该 worktree 的 PG+Redis 容器并改写其 .env
./scripts/wt_services.sh down <slug>  # 销毁容器 + 卷
./scripts/wt_services.sh ps           # 列出所有受管 worktree 的服务
./scripts/wt_services.sh doctor       # 体检：孤儿 compose project / 端口冲突 / .env 与状态不一致
```

## Schema 迁移与 seeds 分离（2026-09-25 sqlx 接管后）

- `migrations/20260925000000_001_baseline.sql` —— 全量 schema 基线（合并原 001-029）
- `migrations/archive/` —— 原 29 个 migration 文件归档，仅历史参考，**不参与** sqlx::migrate! 扫描
- `seeds/menu.sql` —— 菜单目录声明式种子（t_menu），幂等可反复跑
- `seeds/rbac.sql` —— RBAC 基线种子（6 个内置角色 + 权限点 + 系统角色授权基线：t_role_permission / t_role_menu），幂等可反复跑；与 `seeds/menu.sql` 关注点分离（菜单目录 vs 角色 / 权限点 / 授权基线；角色→菜单授权由 RBAC 面板持续管理，seed 只补首次基线）
- `seeds/README.md` —— seed 编写规范

**新 schema 变更走追加新 migration**（append-only，永不修改已有文件）。
**菜单变更 = 改 `seeds/menu.sql` + 重启 app**（启动钩子自动跑，无需新 migration）。
**角色 / 权限点 / 授权基线变更 = 改 `seeds/rbac.sql` + 重启 app**（同上，`run_seeds` 按 menu → rbac → [admin] 顺序无条件加载；`t_role_menu` 基线只补「从未有过的组合」，面板的增 / 删重启后保留）。

### 初始管理员账号（2026-09-26 新增）

`seeds/admin.sql` 是**可选**初始管理员 seed（`username=admin / password=changeme / role=ADMIN`），由环境变量 `BOOTSTRAP_ADMIN_ENABLED` 门控（默认 `false`）。在 `src/main.rs` 启动钩子、`src/infra/seed.rs::run_seeds(pool, bootstrap_admin_enabled)` 处执行。开启流程：env=`true` → cargo run / docker compose up → 用 admin/changeme 登录 `/api/v2/iam/login` → 改密 → env=`false` → 重启。生产默认关，避免无意中创建初始账号。明文密码与 `src/modules/iam/service/account/mod.rs::DEFAULT_RESET_PASSWORD` 同源，bcrypt 哈希复用 `test-support/fixtures/iam.sql` 同款字面值。

## 领域结构（垂直切片）

`src/modules/<域>/` 标准六件套：

| 文件 | 职责 | 对应 `backend-python` |
|---|---|---|
| `handler.rs` | axum handler + 路由 | `api/v1/<mod>.py` |
| `service.rs` | 业务逻辑，签名收 `&mut PgConnection` | `service/<mod>_service.py` |
| `repo.rs` | sqlx 查询，签名收 `impl PgExecutor<'_>` | `repository/<mod>_repository.py` |
| `model.rs` / `dto.rs` | 表行模型+域枚举 / 请求响应 DTO | `model/` / `schema/` |
| `statemachine.rs` | 仅 assembly / outsource / process_chain 三域（`delivery_note` 于 2026-10-08 随事件子系统下线移除；`part` 那份于 2026-10-13 整文件上提到 `shared::batch::status_machine`、不再是域内文件） | `statemachines/` |

## 必须遵守的架构约定

1. **事务边界在 handler**
2. **统一响应信封**：handler 返回 `Result<Json<R<T>>, AppError>`。`R { code: 0, message: "ok", data }`；错误由 `AppError::into_response()` 装入同一信封。不做 middleware 后置包装。
3. **错误码分段契约**（`src/shared/error.rs::code`，与 Python 前端对齐）：0 成功、4xxxx HTTP 语义、5xxxx 系统、2xxxx 业务域（每域一个段，如 201xx 零件/客户、214xx 送货单，新增域错误码先入对应段）。
5. **状态机不写 DB**：`statemachine.rs` 只做内存 enum + `can_transition_to` 迁移表；事件日志由 service 在事务内统一插入。
6. **WS 广播在 commit 之后**（对齐 Python 延迟广播模式），用 `state.ws_hub.broadcast(...)`。
7. **路由挂载**：业务 REST 统一 `/api/v2`（与 Python `/api/v1` 并行），WS 在 `/ws/dashboard`。`/api/mcp` 不在本仓库。
8. **OCC（乐观锁）**
   - **所有对带 `version` 列的表的写端点，body 必须显式收客户端 `version` 且必填**（无 `#[serde(default)]`）。缺失 → HTTP **422 纯文本**（axum `Json` 提取器），**不是**业务信封。
   - **禁止「读-再-比」式的隐式 OCC**。service 内 `get_batch_by_id` 拿到的 `version` 只能用来做**兜底对账**（0 行时区分归因），不能**替代**客户端传值。理由：看板数据是 30s 缓存的快照，隐式 OCC 下「用户看到 5 件 → 实际移动 3 件」（期间他人改动）会**静默成功**。
   - **`40901 VERSION_CONFLICT` 只表达 OCC**，不得用于业务唯一键冲突（如 worker 的 `badge_code` create 撞车 
9. **路由声明规约**
   **转发式：`mod.rs` 只放 `pub fn`，路由表在 `handler`。**

   ```rust
   // mod.rs —— 纯转发，每个工厂带 doc 注释说明挂载点
   pub fn queue_router() -> Router<Arc<AppState>> {
       handler::queue_router()
   }
   ```
   ```rust
   // handler.rs（或 handler/<子域>.rs）—— 路由表 + handler 函数
   pub fn queue_router() -> Router<Arc<AppState>> {
       Router::new()
           .route("/snapshot", get(board::snapshot))
           .route("/processes/{process_id}", get(board::process_detail))
           .route("/move", post(move::move_batch))
   }
   ```
   - **一个前缀一个 router 工厂**。多个独立顶层前缀时用多个 `xxx_router()`（`outsource` 有 4 个），而不是一个工厂返回合并 router —— 合并后 matchit 的注册顺序约束会跨前缀纠缠，排查困难。
   - **`handler.rs` > 600 行或工厂数 > 4 时拆 `handler/<子域>.rs`**，`handler.rs` 退化为聚合器。`prod/queue/handler/`、`prod/batch/handler/` 是范本。
   - **`mod.rs` 的模块 doc 逐条列出端点表与硬切记录**；域改名/URL 硬切必须写「旧路径 404，无 alias」。
   - **matchit 注册顺序是硬约束**：同段数时静态段必须先于 `Path<i64>` catch-all，否则静态路径被吞成 **400**（不是 404）。历史踩坑：`outsource` 旧 `pool_router` 的 `/counts`、`quote_router` 的 `/quotable-parts`，`prod/shelf_process` 的 `/by-process/{id}`。段数不同则天然无冲突（`/move` 1 段 vs `/processes/{id}` 2 段）。
   - **顶层 nest 集中登记在 `src/modules/mod.rs::v2_router()`**，一个前缀一行 + 注释说明归属域与硬切日期。跨域共用的端点也在这里登记，并在**拥有它的域**的 `mod.rs` doc 里注明「本域有 N 处挂载」。

## 集成测试目录结构（2026-09-23 PR13 重构后）

integration test binary 已重组为 **28 binary**（17 多文件 domain 子目录化 + 11 single-file 保留；**2026-10-10** 原 `shelf` binary 并入 `iam` binary；**2026-10-13** 原 `cnc_program_api` 与 `files_sts_tmp_keys` 两个单文件 binary 并成新 `files` 目录域 binary，同日新增 `mail_digest`；**2026-10-16** 新增 `scheduled_task` 与 `print_template`；**2026-10-18** 新增 `wecom` 与 `wecom_daily`、**2026-10-19** 新增 `serial` 与 `hr`）—— 数字以 `cargo metadata --no-deps` 列出的 test target 为准逐个复核过（**2026-10-13** 复核、**2026-10-16** 增量；⚠️ 2026-10-16 新增 `print_template` 后实测为 23，★ **2026-10-18 订正**：那次的单文件枚举**漏登了 `tests/customer_tree_depth.rs`**（2026-10-17 客户树无限层级化那批新增，一直漏在本表之外），故实测基线本就是 **24** 而非 23；再加 `wecom` + `wecom_daily` 为 26、★ **2026-10-19 订正拆分口径**：那次只改了总数、没改拆分列，逐 `ls` 实测为 **16 目录型 + 11 单文件** ⇒ 合计 **27**、★ **2026-10-19 再加 `hr`** ⇒ **17 目录型 + 11 单文件** = **28**（`cargo metadata --no-deps` 实测 28 个 test target）；⚠️ 早年的 25 与本表的拆分列曾长期自相矛盾，现各处口径一致）：

| 新结构 | 拆前 binary 数 | 拆后 binary 名（nextest filter） |
|---|---:|---|
| `tests/com/{main,union_list,delivery_note_in_use}.rs` | 5 | `com`（送货单域 2026-10-08 平移进 com 容器时目录随之更名 `delivery` → `com`；`group` / `attach_batches` / `scan` 三个 sub-file 已随端点收敛删除）|
| `tests/part/{main,create_serial_price,batch,crud,inspection_batches,lifecycle,list_enrichment,purchase_order_import,repair,rollup_derived,serial,to_inspection,to_process,to_ship}.rs` | 12 | `part`（sub-file 穷举，**14 个文件**，2026-10-13 逐 `ls` 核对 + 本次 `rollup_recompute.rs` 换成 `rollup_derived.rs` 后**仍是 14**；`main.rs` 为 binary 入口。★ `purchase_order_import.rs` 2026-10-06 新增：采购订单 Excel 导入两端点 —— `match-by-excel-items` 分档匹配 + `batch-update-order-info` 三态回填 / skip。★ `rollup_derived.rs` **2026-10-13 新增**（随 admin 域删除而来）：直调 `shared::batch::status::rollup_part_derived` 的 8 条**非 HTTP** 集成测试，守派生修正 + 幂等 + 序列号归档释放 + 级联父装配件 + 终态守卫上抛 `TerminalSkip`；它**取代**（而非补充）原 `rollup_recompute.rs`（8 条 HTTP 用例，走已下线的 `POST /api/v2/admin/recompute-rollup`）。⚠️ **两个 sub-file 已迁出、枚举里不再有**：`file.rs` → `tests/files/part_file.rs`（**2026-10-13**，随 part_file 域收编进 files 域，与 `tests/part/main.rs` 的拆分映射同批记）；`pickable_by_work_type.rs` → `tests/production/scan_listing.rs`（**2026-10-10**，报工台两条 list 端点迁往 `prod::scan` 时带走））|
| `tests/assembly/{main,api,lifecycle,by_part,children,create_serial_price,force_complete,status_sync}.rs` | 3 | `assembly`（**2026-10-11** 新增 `force_complete.rs`：装配件级强制完成端点 `POST /prod/assemblies/{id}/force-complete`，含「子件非取消批次为 0 条」的边角回归；⚠️ **2026-10-13 PR1b 两个 sub-file 已不在枚举里**：`files.rs` → **更名 `lifecycle.rs`**（其中 2 条文件上传用例迁往 `tests/files/upload.rs`，剩下 4 条生命周期用例留在本 binary —— 是更名不是搬家）；`files_list.rs`（D-09 `GET /assemblies/{id}/files` 的 4 条用例）→ **整文件删除**，端点已下线）|
| `tests/iam/{main,api,middleware,bootstrap_admin_seed,menu_seed,wx_bind,shelf,shelf_deactivate}.rs` | 2 | `iam`（**2026-10-09**：原 `redis-flush` 串行组已整体删除 —— Redis 隔离改由 key 前缀承担、触发该组的 `clean_redis`（FLUSHDB）已零调用方随函数删除、两个 binary 的并行度已恢复；删除理由留档在 `.config/nextest.toml` 的注释里。**2026-10-10**：`tests/shelf/` 整体并入本 binary，用例改名 `shelf.rs`（← `api.rs`，`api.rs` 同名冲突）/ `shelf_deactivate.rs`（← `deactivate.rs`），与货架子模块迁入 iam 域同批）|
| `tests/statistics/{main,api,event_driven}.rs` | 2 | `statistics` |
| `tests/production/{main,work_type,process,process_chain,worker,queue,queue_auto_allocate,queue_dispatch,queue_board,pending_programming,shelf_process,pickup,process_design,inspection,scan_badge,scan_listing,scan_worker_scan}.rs` | 6 | `production`（按 `src/modules/prod/*` 对齐；`shelf_process.rs` 2026-10-02 自当时的 `tests/shelf/api.rs` 迁入；**`process_design.rs` 2026-10-05 新增**，★ 核心回归是「装配件子件可见」；**`inspection.rs` 2026-10-05 新增** 13 场景，★ 核心回归是「扫子件 → 返回整棵装配件树」；**2026-10-08** `worker_pool.rs` → `queue.rs`、`worker_pool_auto_allocate.rs` → `queue_auto_allocate.rs`、`batch.rs` → `queue_dispatch.rs`，并新增 `queue_board.rs`（队列板聚合 9 场景）；**2026-10-10** 新增报工台三 sub-file：`scan_badge.rs`（`verify-badge`，★ 核心回归是出参恰好 4 个 key）/ `scan_listing.rs`（取件 / 持有两页列表）/ `scan_worker_scan.rs`（放回顺链推进 + 触发 refill 联动））|
| `tests/mail_digest/{main,config,preview,send,logs,batches,n_plus_one_guard}.rs` | — | `mail_digest`（**2026-10-13 新建**，与新域同批；**6 个 sub-file**：按端点切分的 4 个（`config.rs` 读/写配置与校验矩阵、`preview.rs` 渲染与统计口径、`send.rs` 三条失败路径 + 「失败也落流水」、`logs.rs` 分页与快照语义）+ **2026-10-16 第二轮新增 2 个**（`batches.rs` 工单行下的批次子行 + 位置四档 + 空 id 数组；`n_plus_one_guard.rs` **源码级**护栏：`fetch_rows` 循环体内禁 `repo::` / 顶层钉 3 个调用点）。★ 核心回归是「装配件只出一行」「`COMPLETED` 也算已交」「CR/LF 收件人被拒」「XSS 转义」「holder 软删退化成 `—`」；⚠️ **「批次子行紧跟工单行」不由本 binary 守**（`batches.rs` 的子行用例按**子串存在性**断言「渲没渲出来」，位置关系要按**顺序**断言才可靠）—— 由 lib `src/modules/mail_digest/template.rs::tests::batch_sub_rows_follow_their_own_order_row` 按位置守，同一取舍登记在 `tests/mail_digest/batches.rs` 文件头；⚠️ 本 binary 走 HTTP 登录，**需要 Redis**（同全仓其它 HTTP 契约测试））|
| `tests/scheduled_task/{main,accounts,delete}.rs` | — | `scheduled_task`（**2026-10-16 新建**，与新域同批；2 个 sub-file 按端点切分：`accounts.rs` 预设表 / 列表 / CRUD / 默认唯一性 / 路由纪律，`delete.rs` 软删 + 被引用禁删 + 删后同名可再建。★ 核心回归是「授权码加密入库 + 出参一个字都不回显」「`secret` 缺省沿用旧密文」「PUT/PATCH/DELETE 不被路由」；⚠️ 本 binary 需要 Redis，且**必须在进程里自设测试主密钥** —— `crypto::seal/open` 每次重读 env 而集成测试不走 `AppConfig::from_env`，见 `tests/scheduled_task/main.rs::ensure_mail_secret_env`）|
| `tests/outsource/{main,company,quote,quotable,send_receive,shipment,pool}.rs` | 3 | `outsource` |
| `tests/user_repo/{main,basic,role,password}.rs` | 1 → 3 sub-file | `user_repo` |
| `tests/files/{main,part_file,upload,cnc,sts}.rs` | 2 → 1 binary | `files`（**2026-10-13 新建**：由 2 个原单文件 binary `tests/cnc_program_api.rs`（→ `cnc.rs`）/ `tests/files_sts_tmp_keys.rs`（→ `sts.rs`）合并而来；⚠️ 另有 2 个 sub-file 自别的 binary 迁入 —— `tests/part/file.rs` → `part_file.rs`（它从来不是独立 binary，在 `part` 里只是 sub-file）与 `tests/assembly/files.rs` 的 2 条文件上传用例 → `upload.rs`（**2026-10-13 PR1b**：端点 `POST /assemblies/{id}/files` 下线后改测 `PartFileService::upload_file_for_owner(owner_kind=ASSEMBLY)`），两者均不计入本列的 binary 数；★ 核心回归是 `part_file.rs` 的硬切 404 守卫 —— PR1a 3 条（`old_part_files_prefix_returns_404` 等，含旧前缀整段 / 数字段 / 旧 CNC 前缀）+ **PR1b 4 条**（`old_part_upload_drawing_returns_404` / `old_part_upload_3d_model_returns_404` / `old_part_files_confirm_returns_404` / `old_assembly_files_returns_404`），共 7 条）|
| `tests/wx/{main,part_list,production}.rs` | 1 → 1 binary | `wx`（**2026-10-13 补记**：2026-10-11「BFF 按页面切子模块」时建的 binary，此前本表漏登记；本次一并补上并按 `cargo metadata --no-deps` 复核计数）|
| 单文件保留：_e2e_api / applicant_api / auto_complete_api / cos_opendal_api / cos_real_smoke / customer_api / customer_tree_depth / dashboard_ws_api / idempotency_api / print_forward / wecom_login | 11 | （各自原 binary 名；★ `customer_tree_depth` **2026-10-19 补登**：随客户树无限层级化那批（2026-10-17）新增，一直漏在本表之外 —— 本表与开头段的拆分口径就是被它错出来的；⚠️ 原清单里的 `cnc_program_api` / `files_sts_tmp_keys` 已于 2026-10-13 并入上表 `files` binary）|
| `tests/wecom/{main,accounts,guards,probe}.rs` | — | `wecom`（**2026-10-18 新建**，企业微信推送**通道**；列表排序 / CRUD / 置默认唯一性 / 动词纪律、失败矩阵（422 / 40901 / 21803 / 21806 / 21807 / 21808）、探针**不写任何日志表**。⚠️ `ensure_mail_secret_env` 用 OnceLock 范式，**每个 test binary 各设各的**（不同 binary = 不同进程、不共享 env）—— `tests/wecom_daily` 同样要设；⚠️ `probe.rs` 真打外网） |
| `tests/wecom_daily/{main,config,preview,send,logs,n_plus_one_guard,timeout_guard}.rs` | — | `wecom_daily`（**2026-10-18 新建**，企业微信**内容**：每日总结；5 个端点 / 6 条路由。`config.rs` 覆盖 422 纯文本 / 40901 / 21804 校验矩阵 / 21806 / `meta` 六键；`preview.rs` 覆盖窗口半开含今天 / 出货不写 status 谓词 / 节开关 / 分片（≥2 片、逐片 ≤4096 字节、首行带「（i/N）」）/ **绝不落流水**；`send.rs` 覆盖 21802 + FAILED 流水 / `SCHEDULED` 准备阶段失败落 `chunk_count=0` / CLERK 403；`logs.rs` 覆盖分页 + 快照列；`n_plus_one_guard.rs` 是**源码级** N+1 护栏；★ `timeout_guard.rs` 是**跨域纯函数**护栏：`/send` 必须命中 `middleware::timeout::is_long_timeout_path` 长档（否则请求级超时会 drop 掉整个 handler future，已推送的分片变「零流水」）、`/preview` 必须不命中。⚠️ **覆盖空洞**：`/send` 成功路径无集成测试（无真实可达的群机器人地址，且刻意不引入 webhook mock transport）—— 见 `docs/api/wecom-daily.md` §8.3 第 9 条） |
| `tests/print_template/{main,upload,resolve,delete}.rs` | — | `print_template`（**2026-10-16 新建**，与新域同批；3 个 sub-file 按端点切分：`upload.rs` 上传 / 替换语义 / 四道轻校验 / CLERK 403，`resolve.rs` `21109` + HTTP **422** / 内容字节逐字节相等 / 权限点守卫 / **两条通用写入口 + 三条通用读端点的越权面** / 行 VO 出参形状（`customer_name` 命中 / `null`，2026-10-16 review 第 2 轮 C-1）/ 旧路径 404，`delete.rs` OCC（缺 `version` ⇒ 422 纯文本、陈旧 ⇒ 40901）+ 列表分页与 kind 隔离。**45 条用例**（2026-10-16 review 第 1 轮 +6、第 2 轮 +3）。⚠️ **列表端点的「无 N+1」不由本 binary 守**（逐行查名的写法照样全绿）—— 由 `src/modules/print_template/service.rs` 末尾 `mod query_guard` 的**源码级** lib 单测守，登记在 `docs/api/print_template.md` §8.3 第 16 条。★ 核心回归是「`t_part_file` 行四列逐项对上」「同 L1 二次上传 = 替换」「绑二级客户 ⇒ 本域端点 20104、经 `POST /files/objects` 21105（review 第 2 轮 MAJOR-A：L1 判据下沉进 `customer_owner_exists`）」「`.xlsx` 但非 ZIP ⇒ 21102」「CLERK 经 `POST /files/objects` **与** `POST /files/objects/confirm` 两条通用写入口都造不出模板行」「`GET /files/objects/{id}/url` 与 `GET /files/objects` 都不漏模板」「行 VO 带 `customer_name`（前端 `.strict()` 必填，缺它三条路径全废）」。⚠️ **本 binary 不再自带 COS 替身**：原先的 `MemoryCos`（第 3 个内存 COS 替身）已删除，能力上提到 `test-support` 的 `MockCos`（`put` 真存字节 + `delete` 真删字节，新增 `stored_bytes`）—— 残余的替身偏差（真实 COS 的错误分类 / etag 语义 / 「CAS 命中跳过 PUT」零覆盖）登记在 `docs/api/print_template.md` §8.3 第 13 条；⚠️ 需要 Redis）|
| `tests/serial/{main,release}.rs` | — | `serial`（**2026-10-19 新建**，与新域同批；1 个 sub-file 覆盖 `POST /serials/release-terminal`：`dry_run` 零写入 + 计数 / 两表清空 + 子件归档事件（`note` 含原序列号、`to_status` 对得上）/ 幂等 / MANAGER 单角色 403 + 零副作用 / **截断与计数解耦**（种 201 行、断言实跑后残留持号行 = 0 ⇒ 明细上限不得连带截断写入）。★ 每个用例都埋一行 **IN_PROCESS** 工单当对照组，守「守卫漂移会抽走生产中工单的号」这条**静默**故障。⚠️ 走 HTTP 登录、需要 Redis）|
| `tests/hr/{main,shifts,calendar,guards}.rs` | — | `hr`（**2026-10-19 新建**，人事排班域 10 端点；3 个 sub-file 按端点切分：`shifts.rs` 端点 4/5/6/7（班次 CRUD + OCC + 时段 V1–V6 的失败矩阵，钉 `21903` 与 `21908` **分码**）、`calendar.rs` 端点 1/2/3/8/9/10（★ **推导规则 6 条**全覆盖：覆盖优先 / 继承全局 / 星期兜底 / 清覆盖回退 / 停用不级联软删 / 区间 366 天）、`guards.rs` 权限矩阵（MANAGER 写、CLERK 只读：7 条写端点全 40300 + **零副作用**、3 条读端点全 200、**守卫先于入参校验**）+ 动词纪律（`PUT`/`PATCH`/`DELETE` 逐条 405）。**47 条用例**（逐文件 `rg -c '#\[tokio::test\]' tests/hr/*.rs` 实测：`shifts.rs` 15 + `calendar.rs` 26 + `guards.rs` 6）。★ 核心回归是「日型出参逐字 `REST_DAY` 而不是变体名 `RestDay`」「`overflow_minutes` 用**分钟**（`26:30` → 150 而不是 2）」「`daily_minutes` 是各段之和无折算」「雪花 id 出参是 JSON string」「整批 upsert 有一条非法就全回滚」。⚠️ 测试日期用**固定远期基准日**（2030-03-04，周一）而不是 `now()` —— 推导规则要断言星期，漂了就红。⚠️ 走 HTTP 登录、需要 Redis。⚠️ nextest filter 是 `binary(hr)`（不是 `test(/^hr::/)`：本仓 test 名是 `<sub-file>::<fn>`，没有 `hr::` 前缀）|
| **合计** | — | **28 binary**（17 子目录 + 11 单文件；逐目录 `ls` + `cargo metadata --no-deps` 实测，2026-10-13 复核、2026-10-16 增量、**2026-10-18 新增两域、2026-10-19 新增 `serial` 与 `hr` 并订正拆分口径**——实测 `ls tests/*.rs` = 11 个单文件、带 `main.rs` 的目录 = 17 个 ⇒ 17 + 11 = 28；⚠️ `tests/com/delivery_note/` 只有 `mod.rs` 没有 `main.rs`，按本仓「cargo 不识别 `tests/<dir>/mod.rs`」的约定不构成 binary，不计入子目录数）|

**关键约定**：
- cargo 1.98.1 **不识别** `tests/<dir>/mod.rs`，只识别 `tests/<dir>/main.rs`（binary 名 = `<dir>`）。新 domain 一律用 `main.rs`。
- 共享基建已全部在独立 dev-only crate **`test-support/`**（`hsh-erp-test-support`）：原 monolith 测试共享目录与其 facade re-export 均已删除、**全仓零 `mod common;` 调用点**，新测试一律直接 `use hsh_erp_test_support::*`。⚠️ 但 `rg 'mod common'` 仍会命中 **14 处**，全部是**注释里的历史溯源文字**、不是活指针：`test-support/src/lib.rs` 2 处 + `tests/**` 下 11 个文件 12 处（`cnc_program_api` / `applicant_api` / `customer_api` / `_e2e_api` / `idempotency_api`（2 处）/ `auto_complete_api` / `assembly/{api,files}` / `user_repo/{basic,password,role}`）—— 这些注释统一是「本文件原 `mod common;` + `use common::{...};` 已改成 `use hsh_erp_test_support::*`」的来路说明，属有意保留。
- **修改 tests 内 query! 宏后必须重跑 `./scripts/sqlx_prepare.sh`** 生成 `.sqlx/query-*.json` 并提交；拆分 sub-file 会引入新 cache hash（路径变化）。

### 测试数据两条硬约定（2026-10-10 新增）

**1）共享 template DB 已灌 `seeds/menu.sql` + `seeds/rbac.sql`** —— `scripts/test_nextest.sh` 建 template 库时会按序 apply 它们（与生产启动钩子 `src/infra/seed.rs` 同一份 SQL），因此每个测试用例 clone 出来的库**都带着**那份授权基线给 `MANAGER` / `CLERK` / `INSPECTOR` / `CNC_PROGRAMMER` / `SHELF_ACCOUNT` 五个真实角色授予的若干菜单。**角色数 5 是结构事实、不随 seed 内容变；菜单条数随 `seeds/menu.sql` 的目录与 `seeds/rbac.sql` 的基线改动而变，故此处不写数字**（要确切条数就查库，别把它抄进注释或文档 —— 那正是会立刻过期的陈旧数字）。

⇒ **任何断言 `t_menu` / `t_role_menu` 条数或内容的测试，必须用 `seeds/menu.sql` / `seeds/rbac.sql` 永不授予的合成角色串**（`t_role_menu.role` 是 `varchar(20)`、无 DB ENUM、无 `t_role` 表，任意 ≤20 字符字符串都合法），不能用 `MANAGER` 等真实角色反查 —— 否则会把共享 seed 的菜单一起数进来。范本见 `tests/user_repo/role.rs` 的 `SYNTHETIC_MENU_ROLE` / `SYNTHETIC_MENU_ROLE_B`，同文件 `menu_seed_never_grants_synthetic_menu_roles` 自锁「两份 seed 文本不得出现这两个串」（护的是「合成角色」这个前提本身，一旦被破坏就在 seed 文本上当场红，而不是稀释掉某条断言）。

**2）插入行用 `now_naive()` 时，查询窗口不得硬编码绝对日期** —— 否则每月 1 号再次变红（`2026-09` 写的测试在 `2026-10-01` 起查不到行）。两条合规写法：照 `tests/statistics/event_driven.rs` 的 `insert_delivered_event(…, at_date)` 把插入时间戳一起钉死，或让查询窗口由 `now_naive()` 推导。

## 集成测试 fixture 范本（2026-09-23 PR13 Phase F 引入）

`test-support` 提供三类共享资产，新 integration test binary 一律走下列入口，**禁止在测试文件内重新声明本地 `send` / `json_request` / `setup` / `login_*` / `insert_*` / `seed_*` / snowflake 取号入口**：

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
2. **Fixture struct**：`test-support/src/fixture/process_chain.rs::ProcessChainFixture`（字段 + 常量 ID `pub const`），`Default` 实现给出 `manager_username` / `clerk_username` 等字符串常量
3. **Loader 函数**：`load_<domain>_fixture(pool: &PgPool) -> <Domain>Fixture`，走 `include_str!` 编译期嵌入 + `sqlx::raw_sql` 一次性执行（multi-statement）。原先配套的 `test-support/src/fixtures.rs` 动态 INSERT helper 已随 2026-09-24 PR-C.Final 全部迁出（多数复制为各测试文件底部的本地 async fn，零调用的直接删除），故本节只剩本目录一条腿

### `test-support::snowflake` —— 进程内**唯一**取号入口（2026-10-09）

| 进程域 | 入口 | 备注 |
|---|---|---|
| `tests/**`（集成测试 binary） | `hsh_erp_test_support::shared_test_snowflake()` | `pool_snowflake()` 是**兼容薄壳**（同一把锁 + 内层多包一层 `Arc`，靠 `Deref` 照旧 `lock().unwrap().next_id()`），存量调用点未改 |
| `src/**` 的 `#[cfg(test)]`（lib 单测 binary） | `crate::shared::test_snowflake::shared_test_snowflake()` | ⚠️ **不能**用 test-support 的同名函数 —— dev-dependency 环让该二进制里链进两份 `hsh_erp_rust`，两个 `SnowflakeIdGenerator` 是**不同类型**，传参即 `E0308` |

两者为什么必须并存（成因、被否决的两个替代方案、根治方向）见「测试取号：进程内唯一 generator」一节；CI 护栏见「状态派生契约」一节登记的两条 `shared::snowflake_guard::tests::*`。**测试文件里不得出现第三处 generator**。


## DB 约定（迁移与查询必须沿用）

- 无物理外键（`bigint` + 索引，存在性由 service 校验）、无 DB ENUM（`varchar` + Rust enum 校验）
- jsonb 列（**2026-10-19 新增，本仓第一例**）：只有当那一列是**内嵌值对象**才用 —— 无独立生命周期（离开宿主无意义）、不被别域引用、无单独查询口径、宿主详情一次 SELECT 完。当前唯一的 jsonb 列是 `t_hr_shift.segments`（班次时段，论证与被否决的两种替代形态见 `docs/api/hr.md` §8.2）。⚠️ 连带后果：jsonb + `query!` 宏在本仓是**零先例组合**（`.sqlx/` 离线元数据里 0 个 `JSONB` type_info），故该域走运行时 `sqlx::query_as`、**不产生** `.sqlx/` 缓存 diff。需要被 JOIN / 需要 partial unique 索引 / 需要按日期分片区间查询的「看起来也像内嵌数据」的东西（如 `t_hr_shift_calendar`）仍是**真表**，不要 jsonb 化
- 乐观锁：`version` 列，UPDATE 带 `WHERE id=$1 AND version=$2`，0 行 → 409 / `VERSION_CONFLICT`
- 软删除：`deleted_at IS NULL`；审计字段 `created_at/by`、`updated_at/by`
- 雪花主键：`SnowflakeIdGenerator::next_id()` App 侧生成
  - ⚠️ **测试期例外（2026-10-09）**：测试代码**不得**自己 `new` generator，进程内各有一个
    唯一源 —— `tests/**` 走 `hsh_erp_test_support::shared_test_snowflake()`（兼容薄壳
    `pool_snowflake()`）、`src/**` 的 `#[cfg(test)]` 走
    `crate::shared::test_snowflake::shared_test_snowflake()`；由
    `src/shared/snowflake_guard.rs` 的两道 CI 护栏强制，详见「测试取号：进程内唯一
    generator」一节
- i64 主键序列化为 JSON string（`shared/types.rs` 的 serde helper），防 JS 精度截断
- 时间列存 naive `timestamp`，写入用 `infra::clock::now_naive()`（Asia/Shanghai）
- 迁移命名：`<13位时间戳>_<顺序>_<描述>.sql`，见 `migrations/README.md`
- 菜单 / 角色等配置数据走 `seeds/*.sql`，不走 migration

## worktree 服务（2026-09-30 起每分支一套容器）

**要解决的问题**：`src/main.rs` 的 `sqlx::migrate!("./migrations")` 路径相对 `CARGO_MANIFEST_DIR`（= 各 worktree 自己的目录），所以每个分支天然只 apply 自己的 migrations；但所有 worktree 的 `.env` 此前都写死同一个 `DATABASE_URL`（`postgres://hsh:6065161@localhost:5430/hsh`）。两者叠加 → 任一 worktree 跑一次 app 就往**共用**的 `_sqlx_migrations` 写一行，别的 worktree 立刻启动失败：

```
Error: 执行数据库迁移失败
Caused by: migration 20260930000000 was previously applied but is missing in the resolved migrations
```

**机制**：`scripts/wt_services.sh` 给每个 worktree 分配独立的 `postgres-dev` + `redis-dev` 容器、命名卷与端口，并把该 worktree 的 `.env` 三行（`DATABASE_URL` / `REDIS_URL` / `LISTEN_ADDR`）定点改写为容器专属值。

| 项 | 约定 |
|---|---|
| compose project | `hshwt-<slug>`（`docker compose -p hshwt-<slug>`，命名卷随 project 自动隔离） |
| 容器名 | `wt-<slug>-dev` / `wt-<slug>-redis-dev`（`docker-compose.yml` 里 `container_name` 已参数化为 `${CONTAINER_PREFIX:-}dev`，主 checkout 不注入变量 → 行为与改造前逐字一致） |
| 端口保留段 | PG `5431-5499`、Redis `6381-6449`、App HTTP `3001-3099`；起点按 `cksum(slug)` 取模确定性定位，再线性探测首个空闲端口 |
| 状态文件 | `<worktree>/.wt-services`（`.claude/` 已 gitignore），兼作「受管」标记 —— `up` 靠它复用端口，`doctor` 靠它识别受管 worktree |
| 生命周期 | 由 orchestrator skill 自动驱动：`setup-worktree.sh` 复制 `.env` 之后调 `up`，`teardown-worktree.sh` 移除 worktree **之前**调 `down`（`down -v` 连卷一起删，数据一次性） |

**手工调用**：`./scripts/wt_services.sh {up|down|ps|doctor} <slug>`，见「常用命令」。

**两条约定**：

1. `docker compose` 一律从**主 checkout 根**执行、靠 shell 环境变量注入插值（compose 插值优先级 shell env > `.env` 文件）。**切勿手工 export `CONTAINER_PREFIX` / `POSTGRES_PORT` / `REDIS_PORT` 后跑全量 `docker compose up -d`** —— 那会用 worktree 的参数重建主 checkout 的容器。
2. 端口在 worktree 生命周期内稳定（容器 restart 不变），可写死给前端联调；`down && up` 会按状态文件复用原端口，不变。

### ⚠️ 跑 cargo 前先 `set -a; . ./.env; set +a`（2026-10-16 实测登记）

worktree 的 `.env`（`DATABASE_URL` 指向自己那套容器）**不保证被 `sqlx` 宏读到**：
`sqlx` 宏在进程 env 里没有 `DATABASE_URL` 时会向上找 `.env`，实测找到的是**主 checkout 的**
`backend-rust/.env`（`localhost:5430`）而不是 worktree 的那份。症状是：迁移明明已经
手工 apply 到本 worktree 的 5469 库，`query!` 宏仍报 `relation "t_xxx" does not exist`
—— 看起来像「迁移没生效」，真因是**连错了库**。

所以本 worktree 里所有 `cargo check` / `cargo clippy` / `scripts/test_nextest.sh` 之前都要：

```bash
set -a; . ./.env; set +a     # dotenvy 不覆盖已存在的环境变量，故 shell 变量优先
```

（`scripts/sqlx_prepare.sh` 自己已经这么做，故只有手工跑 cargo 的路径会踩。）
⚠️ 这不是 worktree 隔离的缺陷，是 `sqlx` 宏读 env 的既有行为。

**已知限制 / 遗留**：

- **清理测试容器只按容器名 / `com.docker.compose.project` label 过滤（2026-10-03 规约）**。正常路径不需要人工兜底扫描：`scripts/test_nextest.sh` 起的 session 容器是裸 `docker run`（无 compose project label、无命名卷，`/var/lib/postgresql` 挂 `--tmpfs`），脚本 trap 按捕获的 CID 精确 `docker rm -f`（nextest 失败时故意保留并打印 CID 供 debug）。只有上一轮进程被强杀、trap 没跑到时才需人工清理这批遗留 session 容器，**正向白名单**用「空 label」筛：`docker ps --format '{{.ID}}\t{{.Image}}\t{{.Names}}\t{{.Label "com.docker.compose.project"}}' | awk -F'\t' '$4 == ""'`，先核对清单里没有别人的容器，再逐个 `docker rm -f <CID>`。**禁止** `docker ps -q --filter ancestor=postgres:18-alpine` 之类按 image 祖先筛：主 checkout 的 `dev`(5430) 与各 worktree 的 `wt-<slug>-dev` 是同一个 image digest，祖先过滤对它们零区分力；Docker 29.x 下 `--filter "label!=com.docker.compose.project"` / `--filter '!label=…'` 也都是 invalid filter，只有上面白名单这条路线可用。2026-10-03 事故：用祖先过滤清 nextest 孤儿容器时误删了主 checkout `dev`(5430) 与另一 worktree `wt-fix-move-result-shelf-id-dev`(5456)；因未带 `-v` 卷与数据存活，带上 `-v` 就是开发库数据丢失。
- `docker-compose.yml` 写 `redis:7-alpine` 而在跑的 `redis-dev`/`redis-test` 实为 `redis:8-alpine`（仓库里无 `8-alpine` 字样）。因此 `wt_services.sh up` **刻意逐服务 up**、不用全量 `up -d`：全量会把 redis 重建回 7-alpine。这个分歧建议单独对齐一次。


## 环境要点

- 主 checkout 三个容器（docker compose 分服务启动）：`postgres-dev` 在 **5430**（库 `hsh`）、`redis-dev` 在 **6379**；测试库 `postgres-test` 在 **5429**（库 `postgres_rust_test`，⚠️ 当前被孤儿容器占着）、`redis-test` 在 **6380**
- 每个 worktree 另有自己的一套（PG 5431+ / Redis 6381+ / App 3001+），见「worktree 服务」
- 配置全部走 `.env`（`infra/config.rs`）：`DATABASE_URL` 优先，缺省回退 `POSTGRES_*` 拆分变量拼接；测试库 URL 由 `build_test_database_url()` 构建（`DATABASE_TEST_URL` / `POSTGRES_TEST_*`）。`dotenvy::from_filename(".env")` **不覆盖**已存在的环境变量，故临时覆盖用 `DATABASE_URL=... cargo run` 即可，无需改文件
- 优雅退出：`AppState.shutdown`（CancellationToken）同时通知 axum serve 与 `task/auto_complete` 后台循环
