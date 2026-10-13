# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

> 📌 **前端对接**：后端 API 参考见 [`docs/api/`](docs/api/)。**2026-10-16 现状**：`docs/api/` 有 14 份文件，每份是一个域的**整域契约**（端点表 / 逐字段 / 口径表 / 错误码 / 移除记录 / WS 关系 / 表依赖与前端配套 / 已知偏差登记），范本是 [`dashboard.md`](docs/api/dashboard.md) 的八节骨架：
> - [`docs/api/dashboard.md`](docs/api/dashboard.md) —— 大屏聚合域（3 个只读 HTTP 端点 + WS 首帧 / 增量；`DELIVERY_STATUSES` 四处共用、行单位、装配件整套交付不变式、`ts` 格式、前端配套清单）
> - [`docs/api/programming.md`](docs/api/programming.md) —— `prod::programming` 待编程一览（part 状态闸门 + 三规则并集、part 级去重、批次锚点、排序白名单大小写不对称）
> - [`docs/api/inspection.md`](docs/api/inspection.md) —— `prod::inspection` 待品检（队列列表 + 扫码三层树、`l1_customer_name` 与返修侧有意分叉、域隔离漏报盲区）
> - [`docs/api/queue.md`](docs/api/queue.md) —— `prod::queue` 生产队列（9 端点；候选池判据的两列一致 / 货架 JOIN 不一致、`pending_count` 口径、`batch_id` 必须是 JSON 字符串）
> - [`docs/api/scan.md`](docs/api/scan.md) —— `prod::scan` 报工台（**2026-10-10 新建**，5 端点硬切自 `prod::worker` / `part` / `prod::batch`；`ScanListItem` 39 → 17 字段收敛、`ScanWorkerBrief` 12 → 4 字段收敛、旧路径 4×404 + 1×405 的实跑形态、`pickable_shelf_scope` 与 `shelf_scope_for` 的刻意分歧、`listing/` 子模块的域隔离护栏；**2026-10-11 worker-scan 加部分数量**：`quantity` 为 JSON 字符串，缺省 = 整批、`0 < q < batch.quantity` 自动拆批、余量**留在工人手上**（与 pick-up 的「余量留架上」相反），响应 `scan.batch_id` 返回被处理的那一批；**2026-10-15 增至 8 端点**：`GET /locate` 扫码找件诊断 + `POST /take-over` 扫码接管 + `POST /outsource-receive` 外协确认收回（自助恢复三件套）+ `pick-up` PENDING 分支补链首落位；★ **2026-10-16 接管 / 外协收回的目标工序改由「工人的工种映射」推导**：`next_process_id` 变三态（缺省 + 单候选自动 / 缺省 + 多候选必填 `40001` / 显式 + 越链落 `NULL` step），`20702` 不再表示「越链」（`outsource-receive` 自 review 第 1 轮起**完全不产生**该码：stage① 传 `StepResolve::Lenient`），读侧 `locate` 出参新增顶层 `worker_processes` 供前端收窄 picker；⚠️ 闸门候选集与写侧逐字同源、**出参**才是窄集合）
> - [`docs/api/batch.md`](docs/api/batch.md) —— `prod::batch` 批次流转（**剥离中间态**，域内 17 条 + 域外 `/batches/split` 1 条；剥离登记表 / `ROUTES` 权威源 / 状态派生契约 / 外协三端点已迁往 `outsource::queue` / 报工台 2 条已迁往 `prod::scan`）
> - [`docs/api/outsource.md`](docs/api/outsource.md) —— `outsource` 外协域（4 个 router 工厂 19 端点；外协看板 + `move` 三合一写端点、公司 / 报价两域收敛（端点 8+9 → 7+7、`keyword` 拆 `drawing_no` / `name`、报价 `statuses` 多状态筛选接线）、报价与对账、候选侧两处行粒度一致性、移除记录、WS 事件与审计字面量的区分）
> - [`docs/api/delivery_note.md`](docs/api/delivery_note.md) —— `com::delivery_note` 送货单（17 端点；扫码三层树 + DP 批次分配 / 建单判定键单键 / 移除记录 29 条 VO 字段）
> - [`docs/api/shelves.md`](docs/api/shelves.md) —— **iam 域下的货架子模块**（2026-10-10 自独立 shelf 域迁入 iam，URL 硬切 `/api/v2/shelves/*` → `/api/v2/iam/shelves/*`、旧前缀 404；5 端点、`t_shelf.capacity` 负载上限 + `ShelfOut.current_load` 出参、自动选架算法与 `shelf_scope_for` 三分支、picker 两端点移除记录）
> - [`docs/api/iam.md`](docs/api/iam.md) —— `iam` 认证 + 账号 + 货架子模块 + RBAC 角色权限面板域（**30 端点** = session 5 + users 12 + shelves 5 + rbac 8；4 条破坏性变更（`DELETE wx-bind` → `POST .../unbind`、`GET wx-bind` 返 `Option`、bind 删 `corp_id`、4 端点 body 必填 `version`）、`t_wx_identity` 的双向一对一与 TOCTOU 偏差、`uk_t_user_role_user_role_scope` **非 partial**；**2026-10-14 RBAC**：8 条 MANAGER-only 端点（角色 CRUD / grants 整包 / 权限点 / 全量菜单树）、ADMIN 元角色通配且拒写授权、停用即降权 + 变更踢持有者）
> - [`docs/api/wx.md`](docs/api/wx.md) —— `wx` 微信小程序 BFF 域（**2026-10-11 新建**；按小程序页面切子模块：`login/` + `part_list/`；URL 硬切无 alias 5 条（`/wx/iam/wx-login` → `/wx/login/wecom`、`/wx/parts/*` → `/wx/part-list/*`、`/wx/dashboard/home` 删除）、VO 逐字对齐前端卡片模型改 camelCase、`deliveredQty` 真值、尾斜杠 404 实测钉死、★ 2026-10-12 `part_list` 接上日期 + 7 类 tab 双维筛选（`?date=` 打 `system_delivery_date`、`all` 不再是「不过滤」））
> - [`docs/api/statistics.md`](docs/api/statistics.md) —— `statistics` 生产统计域（**2026-10-12 新建**；3 个 MANAGER-only 只读端点、有效工时口径表、链外工序计 0、`LEFT JOIN LATERAL … LIMIT 1` 的去重理由与「数据覆盖受工序链配置率限制」的登记）
> - [`docs/api/mail_digest.md`](docs/api/mail_digest.md) —— `mail_digest` 交期汇总邮件域（**2026-10-13 新建**；6 个 MANAGER-only 端点、行单位工单级 / 刻意 `LIMIT 30` 分叉、频率→**6 段带秒** cron 与 WEEKLY 减 1、217xx 错误码、失败也落流水、**六处**「实现与注释不符」的偏差登记（其中第 5 条已修）；**2026-10-16**：`MAIL_SMTP_*` / `MAIL_FROM_*` / `MAIL_DIGEST_ENABLED` 整轮删除，凭据改由 `scheduled_task` 域的邮箱表提供，配置加 `sender_account_id`、`GET /meta` 删掉 `smtp` 段；★ **同日第二轮**：需求①状态中文 / ②工单下方紧跟全部批次 / ③批次「位置」列（工人姓名·外协厂名）/ ④**本域重写 SQL、不复用 dashboard 常量** / ⑥列对齐与打印；列白名单 **11 → 13**（新增 `batch_no` / `location` + 新迁移回填存量勾选）、**取数恒 ≤ 3 条语句**、**批次子行只填 4 列**、源码级 N+1 护栏）
> - [`docs/api/mail.md`](docs/api/mail.md) —— `scheduled_task` 域「邮件通道」（**2026-10-16 新建**；6 个 MANAGER-only 端点、邮箱 CRUD + SMTP 预设表、**AES-256-GCM 加密入库 + 掩码不回显**、被引用禁删 21708、**21706..21708** 新错误码、密钥轮换未实现的缺口登记）
>
> **其它域的契约在代码注释里**（各域 `mod.rs` / `repo.rs` / `vo` / `dto` 的模块 doc 与逐字段 doc），本仓的目录约定见本文件「`docs/api/` 目录约定」一节。⚠️ **引用不存在的文档路径是禁止的** —— 后端代码变更（新增 / 修改 / 删除端点，或修改 DTO 字段 / 错误码）必须同步更新对应域的 `docs/api/` 文件（若该域有）与代码注释。

## 常用命令

```bash
docker compose up -d postgres-dev    # 开发库（localhost:5430，库 hsh）：cargo run 与 query! 编译期校验依赖
docker compose up -d postgres-test   # 测试库（localhost:5429，库 postgres_rust_test）：集成测试依赖，首次自动建库+迁移
                                    # ⚠️ 2026-09-30 起 5429 被一个已删除 worktree 的孤儿
                                    #    容器占着（见「worktree 服务」），此命令会失败；
                                    #    集成测试请直接走 scripts/test_nextest.sh（自带容器）

cargo check                 # 已有 query! 宏：编译期经 .env 的 DATABASE_URL 连开发库校验；无库时用 SQLX_OFFLINE=true（.sqlx 已提交）
cargo clippy --all-targets
cargo test                  # 需先起 postgres-test；跑单个测试：cargo test <name>
                            # ⚠️ lib 单测与每个 integration test binary **各起各的进程**，
                            #   故雪花取号源也各有一个（进程内唯一，不是全仓唯一）：
                            #   详见「测试取号：进程内唯一 generator」一节。
                            # ⚠️ wx 域有一条 access_token 缓存单测用**真 Redis**
                            # （默认 redis-test:6380，可用 TEST_REDIS_URL 覆盖）；
                            #   无 Redis 时该测试**自动跳过并打印 [跳过] 提示**，
                            #   不会 panic、不影响整体结果——但缓存路径（read/write/
                            #   invalidate）也就**没有被覆盖**，非静默失效。
                            #   要覆盖它：docker compose up -d redis-test
cargo run                   # 需先 cp .env.example .env 并起 postgres-dev

./scripts/sqlx_prepare.sh   # 每次 query! 宏改动后必须重跑，生成 .sqlx/query-*.json 并提交
SQLX_OFFLINE=true cargo build --release   # CI/Docker 用离线元数据构建

# 一次性安装 nextest
cargo install cargo-nextest --locked

# 全量并行测试（推荐；走 test_nextest.sh 起 session 级容器 + per-test database）
scripts/test_nextest.sh

# 单个 binary 调试（runner 自动起容器、用完即删）
cargo test --test <name>   # ⚠️ 该路径下同一 binary 是**多线程同进程**跑的，
                           #    正是「进程内 ID 唯一性」最值得验的路径（历史上两个
                           #    instance=1 的域内单例就是这样撞出 23505 的）

# 快速路：复用 postgres-test 服务（:5429，跳过容器)
# ⚠️ 2026-09-30 起 5429 被孤儿容器占着，此路当前不可用 → 走 scripts/test_nextest.sh
TEST_DATABASE_BASE_URL=postgres://hsh_test:6065161test@localhost:5429 cargo nextest run

# 手工应用 seeds（菜单等配置数据；通常 app 启动钩子自动跑）
./scripts/seed_apply.sh

# 从 db_backup/*.dump 恢复生产数据（见「从备份恢复」一节）
./scripts/restore_from_backup.sh                    # 重建 schema + 灌数据（默认）
DRY_RUN=1 ./scripts/restore_from_backup.sh          # 只打印列漂移决策表，不改库

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

## 从备份恢复（scripts/restore_from_backup.sh，2026-10-01 重写）

**铁律：restore 绝不修改 `public` schema。** dump 是 Python 端老 schema 的快照，与 `migrations/` 有列漂移（027 删了 `t_part` 的 6 个批次依附列 / 028 删了 `t_part_batch` 的 2 列 / 004 改了 `next_process_id` 语义）。2026-09-25 的老脚本靠「先 `ADD COLUMN` 把老列补回来」让 `pg_restore` 不报错 —— 于是那 10 个已废弃的列**永久留在 schema 里**，就是「重建后字段又恢复了」的根因。

现在的流程：

1. **`REBUILD_SCHEMA=1`（默认）** —— `DROP SCHEMA public CASCADE` + `sqlx migrate run`（需 sqlx-cli），目标 schema 严格等于 `migrations/` HEAD，顺带补上从未应用的 004。
2. **漂移自动检测** —— 从 `pg_restore --data-only` 的 `COPY` 表头拿 dump 列清单，与 `information_schema` 比对，输出 `直灌 / stage 投影 / 目标新列置 NULL` 决策表（`DRY_RUN=1` 只打印）。
3. **stage 投影** —— 漂移表先进 `restore_stage`（用 dump 自己的 DDL 原样建表），再 `INSERT INTO public.<t> (交集列) SELECT ... FROM restore_stage.<t>`，逐张对账行数。
4. **schema 快照 diff** —— 灌数据前后各存一份 `information_schema`，diff 非空即失败（永久堵死「列被复活」这类回归）。
5. **`_sqlx_migrations` / `alembic_version` 从 TOC 排除** —— dump 里带的是 Python 端 15 条旧迁移记录，灌进去会让 sqlx 报 `VersionMissing`，之后 `cargo run` 起不来。
6. `t_menu` / `t_role_menu` 默认跳过（菜单目录归 `seeds/menu.sql`，角色→菜单授权基线归 `seeds/rbac.sql`），最后按序幂等补灌。

**改 schema 契约时先改脚本顶部的两张表**（不在其中 → 脚本硬失败，不会静默丢数据）：

| 表 | 格式 | 含义 |
|---|---|---|
| `RENAME_MAP` | `table\|dump_col\|target_col\|出处` | 老列 → 新列的语义延续（如 `t_part_batch.next_process_id` → `current_process_id`，依据 004） |
| `KNOWN_DROP` | `table\|dump_col\|出处` | baseline 已删、允许丢弃的历史列（027/028 留下的） |

| 开关 | 默认 | 说明 |
|---|---|---|
| `REBUILD_SCHEMA` | `1` | `0` = 原地只灌数据（配 `RESET=1`），且会警告目标 schema 落后于 HEAD |
| `DRY_RUN` | `0` | `1` = 只做漂移检测 |
| `INCLUDE_MENU` | `0` | `1` = 同时灌 dump 的 `t_menu`/`t_role_menu`（应急） |
| `ALLOW_UNKNOWN_DROP` | `0` | `1` = 放行未登记历史列（会丢数据） |
| `DUMP_FILE` | 最新 `.dump` | 指定 dump |
| `POSTGRES_CONTAINER` / `RESTORE_DATABASE_URL` | `dev` / 由 `DATABASE_URL` 派生 | 换容器（如验证用 `hsh-restore-test:5433`） |

**已知数据缺口（正常，脚本会打印告警）**：dump 早于工艺链特性，缺 `t_part_process_chain` / `t_process_chain_step` / `t_e2e_seeded` / `t_wx_identity` 四张表的数据 → 恢复后 `t_part.process_chain_id` 全 NULL、无任何工艺链步骤；`t_part_batch.current_process_step_id`、`t_process.color` / `is_cnc`、`t_work_type.max_held_minutes` 同为空。但 `t_part_batch.next_process_id` 已映射进 `current_process_id`（004 的权威列），池内批次可正常定位。

## 领域结构（垂直切片）

`src/modules/<域>/` 标准六件套，与 `backend-python/` 文件一一对应（迁移时逐域对照）：

| 文件 | 职责 | 对应 `backend-python` |
|---|---|---|
| `handler.rs` | axum handler + 路由 | `api/v1/<mod>.py` |
| `service.rs` | 业务逻辑，签名收 `&mut PgConnection` | `service/<mod>_service.py` |
| `repo.rs` | sqlx 查询，签名收 `impl PgExecutor<'_>` | `repository/<mod>_repository.py` |
| `model.rs` / `dto.rs` | 表行模型+域枚举 / 请求响应 DTO | `model/` / `schema/` |
| `statemachine.rs` | 仅 assembly / outsource / process_chain 三域（`delivery_note` 于 2026-10-08 随事件子系统下线移除；`part` 那份于 2026-10-13 整文件上提到 `shared::batch::status_machine`、不再是域内文件） | `statemachines/` |

**part 是跨域枢纽**（`com::delivery_note`、assembly、outsource、part_file、statistics、shelf 均依赖它），实施顺序见 architecture.md 第 7 节。

### `src/modules/prod/*` 子模块清单（prod 域 = 生产调度容器，2026-09-19 聚合）

| 子模块 | 端点数 | URL 前缀 | 表 | 说明 |
|---|---:|---|---|---|
| `prod::worker` | 6 | `/api/v2/prod/workers` | `t_worker` | 工人档案主数据（**2026-10-10** 报工台的 `POST /verify-badge` 迁往 `prod::scan`，旧路径 `/prod/workers/verify-badge` **405** —— 落进 `/{id}` 而那条只注册 GET） |
| `prod::work_type` | 7 | `/api/v2/prod/work-types` | `t_work_type` + `t_work_type_process` | 工种 CRUD 5 + 工种↔工序映射 2。**2026-10-15** 新增 `auto_allocate_limit`（自动补料上限：NULL 兜底 max、有值须 1..=max，create / update 合并校验） |
| `prod::process` | 5 | `/api/v2/prod/processes` | `t_process` | 工序主数据（INHOUSE/OUTSOURCE）；**2026-10-02 订正**：文档原写 6，逐 router 复核为 5（`/` 的 GET+POST 算 2 条 route） |
| `prod::process_chain` | 3 | `/api/v2/prod/process-chains` | `t_part_process_chain` + `t_process_chain_step` | 工单工艺链 |
| `prod::shelf_process` | 3 | `/api/v2/prod/shelf-processes` | `t_shelf_process` | **2026-10-02 新增**：货架 ↔ 工序映射，2026-10-02 自当时的 shelf 域搬入（`t_shelf_process` 关联的是 prod 域实体 `t_process`，故归 prod 而非 iam）。旧路径 `GET|POST /api/v2/shelves/{id}/processes` + `GET /api/v2/shelves/processes` 已删除（**无 alias**；⚠️ 2026-10-10 起这三条在 wire 上的形态统一变成 404 —— `/api/v2/shelves` 整段前缀随货架子模块迁入 iam 而下线）。这 3 个端点请求 / 响应契约逐字不变；前端配套改动清单见 `src/modules/prod/shelf_process/mod.rs` 的模块 doc |
| ⚠️ **货架本体不在本表**：货架子模块 `iam::shelf`（5 端点）是 iam 域下的嵌套子模块，见下表 |
| `prod::queue` | 9 | `/api/v2/prod/queue` | `t_part_batch`（候选池视图）+ `t_process` / `t_worker` / `t_work_type` / `t_shelf`（板聚合只读） | **2026-10-08** `prod::worker_pool` 更名（URL `/pool` → `/queue`，**硬切无 alias**）+ 从 `prod::batch` 吸收下发流 3 条（`pending` / `dispatch` / `auto-dispatch`）+ 召回 1 条（`{batch_id}/recall-to-pending` → `POST /queue/recall`，`batch_id` 改入 body）+ 新增队列板聚合 2 条（`GET /snapshot` / `GET /processes/{id}`，消掉 `/state` `/counts` `/{process_id}` 三条旧读）。整域契约见 [`docs/api/queue.md`](docs/api/queue.md)。**2026-10-15**：`refill` / `auto-allocate` / worker-scan 同事务 refill 的 target 改为工种自动上限 `auto_allocate_limit`（兜底 `max_held_batches`），`auto-allocate` 删除 `mode` / `fill_ratio`（入参只剩 `{ process_id, shelf_id }`） |
| `prod::batch` | 17 | `/api/v2/prod/batches` | `t_part_batch` | 批次流转域（**2026-10-08** 下发流 3 条 + 召回 1 条剥离往 `prod::queue` 后剩 23 条；**2026-10-09** 外协三端点合并为 `POST /api/v2/outsource-queue/move`（`outsource` 域）后剩 20 条，同日**拆批 1 条提升为顶层共用端点 `POST /api/v2/batches/split`**（`batch_id` 入 body、三处消费）后剩 19 条；2026-10-07 待品检队列读 `GET /inspection` 迁往 `prod::inspection`；**2026-10-10 报工台 2 条 `POST /worker-scan` + `POST /{batch_id}/pick-up` 迁往 `prod::scan`** 后剩 **17** 条）。⚠️ **本域有 2 处挂载**：本行的域内前缀 + 顶层 `/api/v2/batches`（拆批，经 `prod::split_router()` 转发，登记在 `src/modules/mod.rs::v2_router()`）。**权威路由清单是 `src/modules/prod/batch/handler/mod.rs::ROUTES`**（`mod tests` 断言它与 `router()` 源码逐条一致），域整体处于逐端点剥离的中间态、尚未删除；剥离登记表见 `STRIP_TARGETS` 与 [`docs/api/batch.md`](docs/api/batch.md) |
| `prod::programming` | 1 | `/api/v2/prod/programming` | `t_part` + `t_part_batch` + chain | 待编程一览（2026-10-01 新增） |
| `prod::process_design` | 1 | `/api/v2/prod/process-design` | `t_part` | **2026-10-05 新增**：制定工序页零件列表。软删闸门 + `status = 'PENDING'` 闸门，7 字段最小集，**刻意不加** `AND assembly_id IS NULL` 守卫（part 域 `GET /parts` 带 `part_only: true` 会把装配件子件全部排除）故**含装配件子件**；入参只有 `sort_dir` / `limit` / `offset`。前端「制定工序」页自 part 域 `GET /api/v2/parts?status=PENDING` 切来，part 域旧端点保留兼容、一行未改 |
| `com::delivery_note` | 17 | `/api/v2/com/delivery` | `t_delivery_note`（送货单）+ `t_delivery_group` / `t_delivery_group_member`（送货分组）+ 跨域只读 `t_part` / `t_assembly` / `t_part_batch` / `t_customer` / `t_worker` / `t_work_type` / `t_shelf` / `t_process` / `t_outsource_company` | **2026-10-08 自顶层 `delivery_note` 域平移**（URL `/api/v2/delivery-notes` + `/api/v2/delivery-groups` → `/api/v2/com/delivery/note` + `/api/v2/com/delivery/group`，**硬切无 alias**）+ **事件子系统下线**（`DROP TABLE t_delivery_note_event`）+ **`NoteScope` 范围判定逻辑删除**，建单判定键收敛为 `(customer_id, status='DRAFT')` 单键（`uk_t_delivery_note_l1_open_draft` 部分唯一索引兜底）+ **入单收敛为扫码单一入口** `POST /scan`（删 7 端点；客户端显式提交 `entries[]`，服务端在同一事务内 find-or-create + **DP 批次分配** + 拆批 + 挂单）+ 新增只读扫码三层树 `GET /scan/{serial_no}` + **打印链路全删**（xlsx 改由前端 hucre 本地生成）+ 新增 `POST /{id}/driver` 与 `GET /drivers`（后者刻意不用 `prod::worker` 的 11 字段 `WorkerOut`，只返 3 字段、不带 `id_card_no` / `phone`）+ `/pickup` 入参瘦身（司机从单据读 + **重跑** `validate_driver`）+ VO 字段裁剪 29 条。**权威路由清单是 `handler::{ROUTES, GROUP_ROUTES, DRIVERS_ROUTES}`**（单测逐条比对各自 `xxx_router()` 源码）。整域契约见 [`docs/api/delivery_note.md`](docs/api/delivery_note.md) |
| `prod::scan` | 8 | `/api/v2/prod/scan` | `t_part` + `t_part_batch` + `t_work_type_process` + `t_shelf` + `t_process` + `t_process_chain` / `t_process_chain_step`（报工台）+ `t_worker` | **2026-10-10 新建**（按**前端消费方**立域：5 条端点的唯一消费方是 `views/production/scan/` 三页 + 扫工牌弹窗 + 队列看板一条动作）：`POST /verify-badge`（自 `prod::worker`，任意已登录）+ `GET /pickable` + `GET /held`（自 part 域，两条 URL **硬切**：`{work_type_id}` / `{worker_id}` 由 path 段改 query 参数） + `POST /worker-scan` + `POST /batches/{batch_id}/pick-up`（自 `prod::batch`）。★ **VO 按前端实际消费收敛**：`PartListItem` 40 → `ScanListItem` 17 字段（23 个恒占位字段退场）、`WorkerOut` 11 → `ScanWorkerBrief` 4 字段（`WorkerOut` 本身**不删**，worker 管理端点仍在用）。★ `?shelf_id=` 入参删除（批次 B 已让选架全自动）。★ **2026-10-11 `worker-scan` 支持部分数量**：`WorkerScanRequest.quantity`（**JSON 字符串**，与同域 `PickUpRequest.quantity` 同形），缺省 = 整批、`0 < q < batch.quantity` 自动拆批并让本次流转作用在**拆出来的新批次**上（响应 `scan.batch_id` 返回它、多补发 `PART_BATCH_SPLIT`），**余量继承 `current_holder_id` 留在工人手上**（与 pick-up 的「余量留架上」相反）；`<= 0` / `>` 返 `20111`。★ **2026-10-15 增至 8 端点**：`GET /locate`（扫码找件只读诊断）+ `POST /take-over`（扫码接管）+ `POST /outsource-receive`（外协确认收回）= 自助恢复三件套，另有 `pick-up` 的 PENDING 分支补链首落位；locate 只读诊断的 `action` / `disabled_reason` 与三个写端点（pick-up PENDING / take-over / outsource-receive）的判据**同源**。★ **2026-10-16 接管 / 外协收回的目标工序改由「工人的工种映射」推导**（`next_process_id` 三态：链可推导 ⇒ 用链且**显式值也不覆盖**；链推不出 + 显式 ⇒ 允许**工序不在链内**（`current_process_step_id` 落 `NULL`，不再 `20702`）；链推不出 + 缺省 ⇒ 工种候选恰好 1 条自动推导 / >1 条 `40001` / 0 条 `20104`，候选集**不与工序链求交**）。外协收回的次序刻意不同：显式值覆盖链推导的老语义逐字不变，缺省时**先信链**、推不出才退回工种候选；其 stage① 传 `StepResolve::Lenient`（`/outsource-queue/move` 的回收臂传 `Strict` 不变），故该端点**完全不产生 `20702`**。读侧 `locate` 出参新增**顶层** `worker_processes`（工种可执行工序，picker 选项集；⚠️ **闸门判据用宽集合**（含已软删工序的映射行，与写侧 `list_process_ids` 逐字同源）、**出参用窄集合**，故「闸门放行 + 出参为空」是刻意形态），两个闸门函数各加一条「**非链推导形态下**工种零候选 ⇒ `PROCESS_NOT_IN_WORK_TYPE`」（复用既有字面量，不扩 wire 枚举）。★ **整域不适用域隔离护栏**（转发型域），只有 `listing/` 子模块单独装 `assert_no_foreign_domain`。整域契约见 [`docs/api/scan.md`](docs/api/scan.md) |
| `prod::inspection` | 2 | `/api/v2/prod/inspection` | `t_assembly` + `t_part` + `t_part_batch`（另 `LEFT JOIN` `t_customer` / `t_process` / `t_shelf` / `t_worker` / `t_outsource_company` 五表：仅 `t_customer` 有软删闸门（客户名退化为 `null`），`t_process` / `t_shelf` / `t_worker` / `t_outsource_company` 四张展示用附表刻意不加） | **2026-10-05 新增**：扫码查询（`GET /scan/{serial_no}`），返回「装配件（可空）→ 全部子件 → 全部批次」三层树。命中口径先查 `t_part.serial_no`、未命中回退 `t_assembly.serial_no`，都未命中返 `20101` / HTTP 404，`serial_no` trim 后为空同样按未命中；软删闸门覆盖 part / assembly / batch 三表；★ **读全部批次不按状态过滤**（含终态，状态闸门在前端）；★ `process_name` 走 `current_process_id` 权威列（migration 004），故 `INSPECTION` / `DELIVERED` 批次**恒 `null`**（出池清该列不变式的正确结果）；`is_scanned` 是唯一内存派生字段（`t_part_batch` 无序列号列）；批次层一条 SQL（`part_id = ANY($1)`）取回整棵树、无 N+1；角色 Manager + Inspector。★ 响应规模 = 子件数 × 每件批次数，**无上限、无分页**（本端点不接受任何 query 参数）。前端待品检页扫码路径 ⏳ **建议**自 part 域 `GET /parts/by-serial/{serial_no}`（+ `/part-batches`）切来（**尚未在前端仓合入**），part 域旧端点保留兼容、一行未改；完整契约见 [`docs/api/inspection.md`](docs/api/inspection.md)。**2026-10-07 新增第 2 个端点**：`GET /queue` 待品检队列列表（13 字段窄投影 + 表头 7 列各一个筛选 + 服务端排序 + 分页 `limit∈[1,200]`），自 `prod::batch` 迁入 —— ⚠️ **破坏性路由变更**：旧路径 `GET /api/v2/prod/batches/inspection` 已下线且**无 alias**，请求 / 响应契约逐字不变（前端只需改 api 层一处 URL 常量）；迁后本域**零跨域依赖** |

> 表内 12 个子模块端点求和 = 6 + 7 + 5 + 3 + 3 + 9 + 17 + 1 + 1 + 8 + 17 + 2 = **79**。计数口径：一条 `.route(path, m1)` 记 1 条、`.route(path, get(h).post(h))` 记 2 条；2026-10-09 合并 master 后逐个打开上表 12 行的 `xxx_router()` 源码复核过一遍（`prod::batch` 与 `com::delivery_note` 另有 `ROUTES` / `GROUP_ROUTES` / `DRIVERS_ROUTES` 常量互为佐证）。
>
> 逐行算式（子模块 → 端点数）：
>
> | 子模块 | 端点数 | 复核依据 |
> |---|---:|---|
> | `prod::worker` | 6 | `/` GET+POST 记 2 + `/{id}` / `{id}/update` / `{id}/deactivate` / `{id}/reactivate` 各 1（`verify-badge` 已迁往 `prod::scan`） |
> | `prod::work_type` | 7 | `/` 2 + `/{id}` / `{id}/update` / `{id}/soft-delete` 各 1 + `/{id}/processes` GET+POST 记 2 |
> | `prod::process` | 5 | `/` 2 + `/{id}` / `{id}/update` / `{id}/soft-delete` 各 1 |
> | `prod::process_chain` | 3 | `/by-part/{part_id}` GET+POST 记 2 + `/{chain_id}` 1 |
> | `prod::shelf_process` | 3 | `/` 1 + `/{shelf_id}` GET+POST 记 2 |
> | `prod::queue` | 9 | `pending` / `dispatch` / `auto-dispatch` / `recall` / `snapshot` / `refill` / `move` / `auto-allocate` / `processes/{process_id}` 各 1（全单方法） |
> | `prod::batch` | 17 | 17 条全单方法，与 `handler/mod.rs::ROUTES` 的 17 项逐条一致（有单测断言） |
> | `prod::programming` | 1 | `/pending` |
> | `prod::process_design` | 1 | `/parts` |
> | `prod::scan` | 8 | `/verify-badge` / `/worker-scan` / `/pickable` / `/held` / `/locate`（GET）/ `/batches/{batch_id}/pick-up` / `/take-over`（POST）/ `/outsource-receive`（POST）各 1（全单方法；`/pickable` / `/held` / `/locate` 是 GET，其余 POST） |
> | `prod::inspection` | 2 | `/queue` + `/scan/{serial_no}` |
> | `com::delivery_note` | 17 | `note_router()` 12（`batch-detail` / `scan` / `scan/{serial_no}` / `/` / `{id}` 的 GET+POST 7 条按单方法计）+ `group_router()` 4（`/` GET+POST 记 2 + `{id}/update` + `{id}/soft-delete`）+ `drivers_router()` 1（`/`）= 12 + 4 + 1 |
>
> ⚠️ **两处顶层 nest 不计入本表**：`POST /api/v2/batches/split`（`prod::split_router()`）与 `/api/v2/outsource-queue/*`（`outsource::queue_router()`）—— 二者挂 `/api/v2` 顶层而非本表的 `/api/v2/prod/*`，见「路由声明规约」第 9 条的顶层 nest 登记。
>
> ⚠️ **2026-10-11 新增第 3 处**不计入本表的 nest：`POST /api/v2/prod/assemblies/{assembly_id}/force-complete`（`assembly::force_complete_router()`，1 条）。装配件**域本体**仍是核心实体、不进 prod（其 8 条 CRUD / 状态机端点仍在 `/api/v2/assemblies/*`，`modules::v2_router()` 顶层 nest，一行未动）；只有这条生产链路收口的逃生端点归 `/prod` 前缀，理由见 `src/modules/prod/mod.rs` 的模块 doc 与 `src/modules/assembly/mod.rs::force_complete_router`。**本处是首处挂在 `/api/v2/prod` 下的非 prod 子模块 nest**（另两处挂 `/api/v2` 顶层，见上一段）。因它不属于上表任一 prod 子模块，**不计入求和**。
>
> ⚠️ **2026-10-13 新增第 4 处**同样不计入本表的 nest：`POST /api/v2/prod/parts/{part_id}/force-complete`（`part::force_complete_router()`，1 条）。零件侧与上面装配件侧**逐条同构**（唯一实质写入是批次的强推、`t_part.status` 只是派生缓存），故对称处理；**硬切无 alias**，旧 `POST /api/v2/parts/{part_id}/force-complete` 实测是**干净 404 空 body**（`force-complete` 是第 2 段字面量、够不着 1 段的 `/{part_id}` catch-all；别与同域 1 段静态的 400/405 记混）。⚠️ **`POST /parts/{part_id}/cancel` 判据不同、刻意留在 part 域**（主写入是 `t_part.status`、OCC 锚 `part.version`，级联取消批次只是副作用）—— 判据是「主写入对象 + OCC 锚点」，不是「碰没碰批次」。⇒ **part 域有两处挂载**（`/api/v2/parts/*` 顶层 nest + 本处 `/api/v2/prod/parts/*`）。移除记录见 `src/modules/part/mod.rs` 的模块 doc。
>
> 求和演变：2026-10-08 `prod::worker_pool` 6 条 → `prod::queue` 9 条、`prod::batch` 27 条 → 23 条（两域间净移动 4 条，prod 域求和 62 → 61），同日在 `com` 聚合新增 `com::delivery_note` 子模块 17 条 ⇒ 61 + 17 = 78；2026-10-09 外协三端点合并为 `POST /api/v2/outsource-queue/move`（`outsource` 域）⇒ `prod::batch` 23 → 20（prod 域求和 61 → **58**），同日拆批 1 条提升为顶层共用端点（不计入本表）⇒ `prod::batch` 20 → 19（prod 域求和 **57**）⇒ 全表 **57 + 17 = 74**；2026-10-10 报工台 5 端点收拢进新建的 `prod::scan`：`prod::worker` 7 → 6、`prod::batch` 19 → 17、`part` 域两条 list 端点（不计入本表，part 无独立表）迁入 ⇒ `prod::scan` 5 条，prod 域求和 **57** → **59**（−1 worker、−2 batch、+5 scan，净 +2）⇒ 全表 **59 + 17 = 76**；2026-10-15 报工台扫码找件自助恢复三件套（locate / take-over / outsource-receive）⇒ `prod::scan` 5 → **8**（+3），prod 域求和 **59** → **62** ⇒ 全表 **62 + 17 = 79**；12 行求和 76 → **79**。⚠️ **2026-10-15 订正两处历史算术**：2026-10-10 段的 prod 域求和应为 **59**（旧文误记 55，净变化也非 −3）⇒ 全表 76；本段 prod 域求和应为 **62**。订正后「12 行直加」与「prod 域自己的子模块 + com 域」恒等（均为 **79**）。

### `src/modules/iam/*` 子模块清单（iam 域 = 认证 / 账号 / 角色 / 货架实体）

| 子模块 | 端点数 | URL 前缀 | 表 | 说明 |
|---|---:|---|---|---|
| `iam::session` | 5 | `/api/v2/iam`（根） | `t_user` + `t_user_role` + `t_menu`，另读 Redis session | `login` / `me` / `logout` / `change-password` / `refresh`（双 token 一次性轮转 + reuse detection） |
| `iam::account` | 12 | `/api/v2/iam/users` | `t_user` + `t_user_role` + `t_wx_identity` | 9 个账号 CRUD / 角色 / 改密 + 3 个企业微信身份预绑定 |
| `iam::shelf` | 5 | `/api/v2/iam/shelves` | `t_shelf`（自有）+ `t_part_batch`（负载聚合）+ `t_shelf_process`（选架工序过滤，只读） | **2026-10-10 自独立 shelf 域迁入**的嵌套子模块，URL 硬切自 `/api/v2/shelves/*`、**无 alias**（旧前缀 404），请求 / 响应 / 错误码 / OCC 契约逐字不变。逐字段 / 口径表 / 选架算法 / 前端配套清单见 [`docs/api/shelves.md`](docs/api/shelves.md) |
| `iam::rbac` | 8 | `/api/v2/iam`（根） | `t_role` + `t_permission` + `t_role_permission`（+ `t_role_menu` 授权读写） | **2026-10-14 新增**角色与权限面板：`/iam/roles`（CRUD + grants 整包）/ `/iam/permissions` / `/iam/menus`，8 条全部 `require_role(Manager)` 硬切（面板是权限真源、用权限点守它会自锁）；ADMIN 是代码级通配的元角色（不进授权表、grants 拒写）；停用角色即降权并踢持有者。整域契约见 [`docs/api/iam.md`](docs/api/iam.md) §9 |

> 端点求和 = 5 + 12 + 5 + 8 = **30**，与 [`docs/api/iam.md`](docs/api/iam.md) §1 的三张端点表 + §9.1 的 RBAC 一张恒等。计数口径同 prod 表（一条 `.route(path, m1)` 记 1 条、`.route(path, get(h).post(h))` 记 2 条）。
>
> ⚠️ **货架归 iam 而不是 prod**：账号与货架同属「谁能碰什么」的权限资源 —— `t_user_role` 里 `SHELF_ACCOUNT` 角色的 `scope_id` 指向某个货架，货架实体是这套权限体系的落点；5 条写端点又全部 MANAGER 独占，与账号 / 角色管理同一批授权动作。**工序映射**（`prod::shelf_process`）因关联 prod 域实体 `t_process` 仍归 prod；**选架设施**（`shared::shelf`）是跨域设施层、无域归属，两者都不在 iam。
>
> ⚠️ **`t_shelf` 全仓只有一份行结构**：`iam::shelf::model::TShelf`（14 列含 `capacity`）。iam 账号侧原有的 13 列投影已删除，`IamRepoTrait::get_shelf_by_id` 改为委托 `ShelfRepo::get_by_id`（两条 SQL 谓词逐字相同：都只过滤 `deleted_at`、不过滤 `is_active`）。

### `src/modules/files/*` 子模块清单（files 域 = 文件资源容器域，2026-10-13 新建）

| 子模块 | 端点数 | URL 前缀 | 表 | 说明 |
|---|---:|---|---|---|
| （容器自带） | 1 | `/api/v2/files/sts-tmp-keys` | 无（薄壳鉴权转发到 python） | 2026-09-28 新增，逐字未动 |
| `files::part_file` | 7 | `/api/v2/files/objects` | `t_part_file`（自有） | `POST /objects`（**上传四合一**）/ `GET /objects`（列表）/ `POST /objects/confirm`（STS 直传回执）/ `GET /objects/{id}/url` / `GET /objects/{id}/content` / `POST /objects/{id}/delete` / `GET /objects/{id}/preview`（**2026-10-13 PR2**：COS CI 预览，惰性轮询零后台任务；**2026-10-12** 增可选 `?size=view\|thumb\|full` 投放档位，缺省 view、非法值 40001/422） |
| `files::part_file::cnc` | 1 | `/api/v2/files/cnc` | 复用 `t_part_file`（无 repo 层） | `POST /cnc/pairs`：G_CODE + 工艺卡配对写入 |

> 端点求和 = 1 + 7 + 1 = **9**，与 `src/modules/files/mod.rs` 模块 doc 的端点表恒等。计数口径同 iam 表。
>
> ★ **权威路由清单是三张 `ROUTES` 常量**（2026-10-13 review 第 2 轮 N-3 / N-10 补齐）：`files::handler::ROUTES`（1 条 `sts-tmp-keys`）/ `part_file::handler::ROUTES`（7）/ `part_file::cnc::handler::ROUTES`（1）。三者由**同一个**比对器 `src/modules/files/route_guard.rs::routes_in_router_source` 逐条核对各自的 `router()` 源码（单测 `routes_declared_in_router`）—— 加 / 删端点忘同步 `ROUTES` 会当场红。⚠️ 各 `.nest(...)` 的路由清单归**子模块**自己的 `ROUTES`，不在父级重复计数。
>
> **2026-10-13 PR2 新增 `files::part_file::preview`**（`GET /objects/{id}/preview`，1 条）：COS CI 逐页光栅化预览。三条必须知道的事实：
> ① **惰性轮询、零后台任务、零新增 cron** —— 状态推进完全由前端下一次调用驱动（`PENDING & 到期 → describe`），代价是「没人调就没人推进」；
> ② **内容寻址 + 独立表** —— `t_file_preview` 按 `preview_seed`（内容哈希）去重而非按 `t_part_file` 行去重，因为 CI **按页计费**，同一份 PDF 的 N 份拷贝只该跑 1 次 job。⚠️ **并发不重复提交是「两道闸」不是一道**（2026-10-13 review 第 1 轮 MAJOR-1 订正）：`uk_t_file_preview_seed` 唯一索引只闸**首次登记**；「行已存在 + 无真 job id」的**重试**与**自愈**两条路径靠 `PreviewRepo::claim_submit` 的**条件 UPDATE 认领**（占位 token `__claiming__:<snowflake>` + 30s 租约），只有 `rows == 1` 的调用才允许 POST `/doc_jobs`。⚠️ 「先查再插 / 先查再打」在这两条路径上是 TOCTOU，会重复计费；
> ③ ⚠️ **安全取舍反转**：本端点**放开 `SHELF_ACCOUNT`** 且返回**永久 public-read 直链**，比 `/url` 的 1 小时预签窗口**更宽** —— 这是用户拍板的选项 A，取舍全文 / 风险补偿见 `src/modules/files/part_file/preview/mod.rs` 的「安全取舍反转」一节。**不要**「顺手把 `/preview` 与 `/url` 的角色集合对齐」；
> ④ **2026-10-12 增 `?size=` 投放档位 + 配方版本入 seed**：`?size=view\|thumb\|full`（缺省 `view`，大小写敏感）按档位给 `pages[]` 尾部拼 `CosConfig` 的 `preview_image_view_params` / `preview_image_thumb_params`（EdgeOne 图片即时处理参数，**只允许单轴**；`full` 档恒无参数），非法值 ⇒ 40001 / HTTP 422 且解析先于任何 DB / CI 副作用；`preview_seed` 改为「`content:{sha}` / `key:{sha256(object_key)}` 材料 + `PREVIEW_RECIPE_VERSION`」整体哈希取前 32 位 —— **改渲染配方必须递增该常量**，否则旧 READY 行与 CDN 缓存不换代（已知偏差 #1 因此标为已修）。⚠️ 重复键 `?size=a&size=b` 仍是提取器层纯文本 400（平台级残余，偏差 #11）。
>
> ⚠️ **`POST /objects/{id}/delete` 不回收预览**（2026-10-13 review 第 1 轮 m-7 登记）：该端点只软删 `t_part_file` + 异步删 COS **源对象**，`previews/<seed>/` 下的 JPG 与 `t_file_preview` 行**都不动** ⇒ 图纸「删除」后完整视觉副本仍 public-read 永久可读，且该表只增不减、无 TTL / GC / 级联。
>
> ⚠️ **两套时钟的纪律**（2026-10-13 review 第 2 轮 N-7 登记）：`next_poll_at` **进谓词**（`claim_submit` 的租约判据）⇒ **必须 bind** `infra::clock::now_naive()`，**不得**写 SQL `now()`（DB 会话时区与应用时钟是两个时钟，容器里可能 UTC，差 8h ⇒ 过期认领永远接管不了）；`updated_at` 是**纯审计**列、不进任何谓词 ⇒ 沿用全仓既有的 `now()`，**不要**「顺手统一」。同款纪律见 `CLAUDE.md`「状态派生契约」与 mail_digest 一节的 `CURRENT_DATE` 提醒。
>
> ⚠️ **`preview_seed char(64)` 有空白填充陷阱**：PG 的 `bpchar` 会把 32 位种子右侧填空格到 64，读出来带 32 个尾随空格。`WHERE preview_seed = $1` 不受影响（bpchar 比较忽略尾随空格），但**拼进 URL 就是 404**。故 `PreviewRepo::find_by_seed` 是本表唯一的读出口并在此统一 `trim_end()` —— 新增读路径必须走它，不要自己写 SQL 读该列。
>
> **2026-10-13 两批硬切**（均**无 alias**、旧前缀一律 404）：
> - PR1a：`part_file` / `cnc_program` 两个独立域收编成 `files` 容器域及其嵌套子模块。URL `/api/v2/part-files/*` → `/api/v2/files/objects/*`、`/api/v2/cnc-programs/*` → `/api/v2/files/cnc/*`；同批删除 12 条零调用方端点（part_file 8 = 7 条零件维度嵌套 + 同批删除的 `POST /part-files/` 上传〔PR1b 又复活并入四合一〕；cnc 4 = 3 条 alias + 1 条配对列表）与两张死表（`t_cnc_program` / `t_drawing_file`）。⚠️ 计数别与 `files/part_file/handler.rs` 移除记录表的 **7** 条混：那张表只列嵌套的 7 条，不含已复活的 `POST /part-files/`（review 第 1 轮 NIT-9）。
> - PR1b：part / assembly 侧的文件端点收编 —— **6 条旧端点**（`POST /parts/{id}/upload-drawing`、`POST /parts/{id}/upload-3d-model`、`POST /assemblies/{id}/files`、`POST /part-files/`（PR1a 下线又复活）、`POST /parts/{id}/files/confirm`、`GET /assemblies/{id}/files`）**并成 2 条**（`POST /files/objects` 四合一 + `POST /files/objects/confirm`），其中 `GET /assemblies/{id}/files` 是**直接删除**。⇒ files 域是**全仓唯一**持有 `t_part_file` 写入路径的域。（2026-10-13 review 第 1 轮 NIT-11 订正：原文写「5 条旧端点」，漏数了复活那条；且把「端点」与「路径」混用。）
>   ★ **part 侧的响应形状与状态码也变了**（MINOR-5 登记，此前三处 doc 都查不到）：HTTP **200 → 201**；出参 `R<TPartFile>` → `R<PartFileOut>`；owner 键 `part_id` → `owner_id`（新增 `owner_kind` 回显）；丢 `updated_at` / `updated_by` / `deleted_at`。逐字段对照见 `src/modules/files/part_file/mod.rs` 的「零件侧上传的响应形状与状态码变更」一节。
>
> ⚠️ **COS 对象 key 前缀：代码侧统一 `obj/`，容器部署下仍未统一（2026-10-13）**：`COS_UPLOAD_PREFIX` 此前有两套实际取值（生产 `.env` 的 `drawings/` 与 `upload_file_for_owner` 硬编码空串 ⇒ 桶根），代码侧现已收敛为 `obj/`（`config.rs` 缺省值 / `.env.example` / 代码回退三处同步）。**但 `docker-compose{,-local,-staging}.yml` 的 `${COS_UPLOAD_PREFIX:-drawings/}` 共 6 处仍在根仓**（backend-rust 仓外，本 PR 结构上无法触及）⇒ **容器部署下前缀仍落 `drawings/`**，代码回退值 `obj/` 只在不经 compose 的 `cargo run` 场景生效；根仓那 6 处待单独提交。**不要**在本文件或代码注释里写成「前缀已统一」（2026-10-13 review 第 1 轮 MAJOR-2）。**不需要数据迁移、不要跑 `src/bin/cos_key_migrate.rs`**：`t_part_file.object_key` 存完整 key，读端点 `PartFileService::resolve_effective_key` 先 `head_object(db_object_key)` 优先命中，历史对象原地不动，只有新上传改落配置里的那个前缀。
>
> ⚠️ **files 域的两条写路径别再弄丢**（review 第 1 轮 MAJOR-1 回归）：`POST /files/objects` 与 `POST /files/objects/confirm` 都从配置读 `COS_MAX_FILE_SIZE`（`.env.example` = 100MB），0 字节 → 40001、超限 → 21103；两条路径**同时**校验 `owner_kind` × `kind` 配对（`PART` 只收 DRAWING / 3D_MODEL，`ASSEMBLY` 只收 ASSEMBLY_MASTER，否则 20104）。已知例外：`POST /parts/batch` 的 bindings 分支仍写死 300MB（part 域、本 PR 未触碰，登记在 `files/part_file/mod.rs` 的「已知偏差登记」第 5 条）。
>
> ⚠️ **刻意留在别的域的两处文件写入**：`POST /api/v2/parts/batch` 的 **bindings 分支**（端点本体是「批量建零件」，文件只是可选入参）；`POST /api/v2/assemblies` 的 **multipart PDF**（只喂 `lopdf` 数页数，不入库不上 COS）。⚠️ **2026-10-13**：原先并列的 `POST /api/v2/parts/batch-with-pdfs`（multipart PDF 同款）随该端点零消费方清理**整条删除**。
>
> ⚠️ `files` 域**不在** `docs/api/` 的整域覆盖清单里（该目录只覆盖部分域，判据见本文件「`docs/api/` 目录约定」一节），契约真源是各级模块 doc；「已知偏差登记」节落在 `src/modules/files/part_file/mod.rs` 的模块 doc 末尾。

### `src/modules/dashboard/` 域（只读大屏聚合，2026-10-07 VO 重构后 3 个端点）

| 端点 | 权限 | 说明 |
|---|---|---|
| `GET /api/v2/dashboard/snapshot` | 登录即可 | 大屏首帧：`overdue_count` / `in_inspection_count` / `in_process[]`(7 字段 `WorkerHeldBatch`) / `system_delivery_orders{upcoming,overdue,partial}`（各 `{items[≤30], total}`，**工单级**）；**无 query 参数** |
| `GET /api/v2/dashboard/upcoming-delivery` | 登录即可 | 交期柱状图分桶：`today` + `buckets[]`；`days` 缺省 14 clamp 1..60，`basis` 缺省 **system** |
| `GET /api/v2/dashboard/delivery-orders` | 登录即可 | 柱状图下钻抽屉：`date` + `statuses` 必填（逗号分隔），`basis` 缺省 system，`total` 不受 200 条截断影响 |
| `GET /ws/dashboard` | JWT + session | 握手首帧 snapshot + `WsEvent::DashboardEvent` 增量 + text 心跳 |

- **域外依赖 = 0**（只读 5 表 `t_part` / `t_part_batch` / `t_assembly` / `t_customer` / `t_worker`，只读跨域聚合是本仓既定 pattern，同 `statistics`；⚠️ 原举例的 `admin` 域已于 2026-10-13 整域删除）。
  - **护栏设施**：`src/shared/domain_guard.rs`（`#[cfg(test)]` 项、不进 lib 产物，故**只有 `src/` 内单元测试能用**，`tests/` 集成测试拿不到、要靠它得先改调用方式）的 `assert_no_foreign_domain(本域路径, 本域源码目录, 域专属指引)` —— 在剥掉注释的代码区里找「域根段 + 他域路径」前缀失配；不连网不连库，单测可在无 DB 环境跑。已登记的漏报盲区与精度边界见该文件顶部 doc。
  - **已接入共 7 处 = 整域 5 + 非整域切片 3**（2026-10-11 review 第 1 轮由 6 = 4 + 2 订正：wx BFF 重构新增 `wx::production` 整域接入 + `wx::part_list` 切片接入。与 `src/shared/domain_guard.rs` 顶部 doc 同一套口径）：整域接入的 5 处是 `modules::dashboard::tests::dashboard_domain_depends_on_no_other_domain`（扫 `src/modules/dashboard/**/*.rs`）、`modules::iam::tests::iam_domain_depends_on_no_other_domain`（扫 `src/modules/iam/**/*.rs`，含嵌套子模块 `iam::shelf`；⚠️ iam 是「**纯自有表域**」—— `t_user` / `t_user_role` / `t_menu` / `t_wx_identity` / `t_shelf` 全部归它，不需要 dashboard / statistics 那种「在本域 SQL 里跨域只读聚合」的取舍，故这条护栏只有「不许 import 别人的 service / repo」一重含义）、`modules::prod::programming::tests::programming_domain_depends_on_no_other_domain`、`modules::prod::inspection::tests::inspection_domain_depends_on_no_other_domain`（后两者分别扫 `src/modules/prod/programming/**/*.rs` 与 `src/modules/prod/inspection/**/*.rs`，本域路径为嵌套域 `prod::xxx`，**同父兄弟域 `prod::batch` 同样算跨域**）、`modules::wx::production::tests::production_domain_depends_on_no_other_domain`（扫 `src/modules/wx/production/**/*.rs`；本域读 `t_user` / `t_worker` / `t_work_type` 但按既定 pattern 在本域 SQL 里只读聚合，不 import `iam::service::account`）；**非整域切片接入**的 3 处是 `modules::prod::queue::board::tests::board_aggregation_depends_on_no_other_domain`（只扫 `src/modules/prod/queue/board/**/*.rs` —— queue 域整体**不适用**该护栏，它的写端点按既定 pattern 经本域 trait 转发其它域单表查询）、`modules::prod::scan::listing::tests::listing_aggregation_depends_on_no_other_domain`（只扫 `src/modules/prod/scan/listing/**/*.rs` —— `prod::scan` 是**转发型**域，`worker_scan` 必然 import `part` / `assembly` / `prod::queue` / `prod::worker` 四处域，但它的 3 条只读聚合端点（pickable / held / locate）完全可以零跨域；圈出可守的部分比整域不守要强）与 `modules::wx::part_list::tests::part_list_domain_depends_on_no_other_domain`（只扫 `src/modules/wx/part_list/**/*.rs` —— `wx` 域整体**不适用**该护栏，其 `login` 子模块必然 import `iam::service::account`，但 `part_list` 这块只读聚合完全可以零跨域）。新增只读跨域聚合域时照此加一行调用即可。
  - **元测试**：`shared::domain_guard::tests::*`（6 条，含「本域标识符非法必须 panic」与 raw string 字符串状态两组）。任一写法漏报、误报或探测器瞎了都会红。
- `DELIVERY_STATUSES`（6 态，`repo/delivery.rs`）是「未交付」的**唯一**判据，逾期计数 / 面板 / 抽屉 / 柱状图 top+middle 四处共用；柱状图 bottom 层额外含 `DELIVERED`。**它与前端 `LAYERS[].statuses` 是人工同步关系，无编译期保障**，改一侧必须改另一侧。
- ⚠️ **行单位**（2026-10-10）：逾期 = **工单级**；交期面板三桶 = **工单级**（装配件行**替换**其子件行，行内新增 `row_type` 标 PART / ASSEMBLY，装配件的 `quantity` 与 `delivered_quantity` 单位是**套**）；柱状图 / 抽屉 = **件级**（子件各算 1）。
- ⚠️ **装配件整套交付不变式**（2026-10-10）：逾期 KPI 与面板 `upcoming` / `overdue` 两桶用**同一个** `NOT EXISTS(已交批次)` 谓词（逐字相同 ⇒ `overdue.total == overdue_count` 严格对数）；装配件的 `delivered_sets` 只是 `delivered_quantity` 这个**展示值**的算法、**不参与桶归属**，但在该不变式下与 `NOT EXISTS` 同解（推导见 `docs/api/dashboard.md` §4.4）——不要把其中任何一条当成漏判「修」掉。无装配件套数校验的写路径共 **4 条**（`POST /prod/batches/{batch_id}/deliver`、`POST /prod/batches/scan/deliver`、`POST /prod/parts/{part_id}/force-complete`、`POST /prod/assemblies/{assembly_id}/force-complete`；后两条强推的是 `COMPLETED` 而守卫把 `COMPLETED` 也算已交），排查 SQL 见 §4.4。
- ⚠️ 与 `statistics::repo::sql::count_overdue_undelivered` **有意分叉**（planned 口径 + DELIVERED 事件兜底，服务生产统计页）——不要动它。⚠️ 那是**另一份 SQL**；dashboard 自己这份是 system 口径 + 已交量守卫，两者分叉点只在「交期列 + 事件兜底」两处。
- 完整契约（含每字段 SQL 来源、移除记录、WS 事件集、前端配套改动清单、已知偏差登记）见 [`docs/api/dashboard.md`](docs/api/dashboard.md)。

### `src/modules/statistics/` 域（生产统计，只读报表，MANAGER-only，3 个端点）

| 端点 | 权限 | 说明 |
|---|---|---|
| `GET /api/v2/statistics/overview` | **Manager 独占** | 生产概览：`created_count` / `completed_count` / `in_process_count` / 交付三分类 / 每日新建与完成（零填充）/ 状态分布；入参 `date_from` + `date_to`（**必填**） |
| `GET /api/v2/statistics/workers` | **Manager 独占** | 工人一览：一行一工人，含贡献度百分比 + **有效工时 3 字段**；区间内无事件的工人也列出（零填充） |
| `GET /api/v2/statistics/workers/{worker_id}` | **Manager 独占** | 单工人详情：领取 / 放回 / 参与工单 + **有效工时总额与按工序拆分** |

- **端点 3 条**（跳序取件两条已随 `t_pickup_skip_event` 删除下线，**无 alias**，旧路径 404）。完整契约见 [`docs/api/statistics.md`](docs/api/statistics.md)。
- **纯只读域**：3 个 handler 全走 `pool.acquire()`，不开事务、**不发也不消费 WS 事件**（没有失效通道，新鲜度靠前端自身的 query 策略）。
- ⚠️ **角色守卫下沉在 handler 第一行**（三个 handler 逐字 `require_role(Role::Manager)`）。本域错误码只有两个：`20104`（`date_from > date_to`，或 path `{worker_id}` 非数字）与 `20201`（工人不存在**或已软删**）。
- ⚠️ **本域零跨域模块依赖**（读别域的表是既定 pattern）：唯一例外是 `worker_stats` / `worker_detail` 经 `repo.conn_mut()` 借位调 `prod::worker::repo::WorkerRepo` 与 `prod::work_type::repo::WorkTypeRepo`，与 `DeliveryNoteRepoTrait::conn_mut` 同形。
- ⚠️ **`count_overdue_undelivered` 与 dashboard 的逾期口径有意分叉**（本域 planned 口径 + DELIVERED 事件兜底），不要动它，见上一节末条。

#### 有效工时维度

> 定义：区间内该工人**完成了一道工序**的次数 × 该工序在工单锚链上的
> `t_process_chain_step.estimated_minutes`，求和。**单位是分钟整数**，不是小时、
> 不是浮点。

- **计入**只有两种事件：`RETURNED_TO_SHELF`（放回顺链推进）与 `SENT_TO_INSPECTION`
  （工人主动送检）。**不计入**：`PICKED_UP` / `TAKEN_FROM_POOL` / `MOVED`（那是
  **开始**，一进一出都计则同一道工序被计两次）、`INSPECTED`（**品检员**动作，写点在
  `prod::batch` 的批次扫码与状态流转两处）、`DELIVERED`（无工序语义）。
  判据是「一进一出只认『出』的那一次」，不是「凡有工人的事件都算」。
- ⚠️ **链外工序计 0 分钟，不是丢弃该行**：`e.process_id IS NULL` /
  `p.process_chain_id IS NULL` / 锚链内找不到该 `process_id` 的 step，三种情况一律
  工时贡献 0，但该行照常进 `worked_process_count` / `worked_batch_count` 的统计集合。
- ⚠️ **取工时必须用 `LEFT JOIN LATERAL (…) ON TRUE` + `ORDER BY sort_order ASC,
  id ASC LIMIT 1`，严禁直接 `JOIN t_process_chain_step`**：`t_process_chain_step`
  **没有** `(chain_id, process_id)` 唯一约束（唯一索引只有 `uq_chain_step_chain_order
  (chain_id, sort_order) WHERE deleted_at IS NULL`），写侧 `upsert_chain` 也**只校验
  `sort_order` 不重复、不校验 `process_id` 不重复** ⇒ 同名工序在一条链里出现多次是
  后端照收的合法形态。直接 JOIN 会让工时**按命中条数翻倍且不报错**。这与
  `shared::batch::chain` 把 `hit_count > 1` 判成 `chain_state = 'NONE'` 是同一事实的
  两种处置：那边「下一道是谁」有歧义必须拒答，这边「这一步的预计工时」取链上第一次
  出现的那一步即可。
- ⚠️ **锚链取 `p.process_chain_id` 单来源，不写 `COALESCE(p.process_chain_id,
  step.chain_id)`**：那套 COALESCE 是 `shared::batch::chain` 的**运行时链定位**纪律
  （兼容批次 step 指针指向旧链），统计侧不需要兼容老数据，且 LATERAL 子查询的
  `chain_id` 本就取自 `p.process_chain_id`、无从回退。
- ⚠️ **数据覆盖受工序链配置率限制**：`effective_minutes` 只在**已配工序链**的工单上
  有值。开发库绝大多数工单无 `process_chain_id` ⇒ 该指标恒为 0，**是口径的必然结果
  不是回归**。要它有意义，前提是工单普遍配了链**且**链上配了 `estimated_minutes`。
  不要为了让数字好看而给无链工单兜默认工时。
- **三条 SQL 共用一份骨架常量**（按工人聚合 / 单工人总额 / 按工序拆分）——
  `repo/sql.rs` 的 `effective_minutes` 模块 + `build()`。**改口径必须改那一处**：
  三条各抄一遍必然漂移，而漂移的表现是数字静默错（不报错）。拼装走 `format!` +
  `AssertSqlSafe`，注入面为 0（填进去的全是模块私有常量或字面量，用户输入一律
  `bind`）。
- ⚠️ **`JOIN t_worker w` 是 INNER**（与既有 `worker_pickup_rows` 同款）：只统计真有
  工人身份的作业事件。**不要**改成 LEFT。
- `WorkerProcessBreakdown.process_id` **可空**（写侧只在 `prod::scan` 的 6 个构造点落
  `process_id`（2026-10-15 起：worker_scan 3 + pickup 2 + `TRANSFERRED` 1），其余构造
  点恒 `NULL`；`TRANSFERRED` 不计入有效工时，只补写 `process_id`），这类事件汇成一个
  「无工序」组、对工时总额贡献恒为 0。⚠️ `process_id: null` 与 `process_name: "(已删除)"`
  是**两种不同的「无名」**。

#### `t_part_event` 的两个新列（statistics 侧的消费方）

`worker_id` / `process_id` 两列此前**从未被 Rust 写侧写过**（老 INSERT 的列清单里根本
没有它们），补写后 statistics 域才第一次有「按工人 + 按工序」可聚合的数据：

| 列 | 语义 | 统计侧怎么用 |
|---|---|---|
| `worker_id` | `t_worker.id`（**不是** `t_user.id`） | 全部按工人聚合的 GROUP BY 键。⚠️ 两者共用雪花 ID 空间但**无映射表**，混填 `created_by` 会让统计到从未做过活的账号上 |
| `process_id` | 事件时刻的**工序快照** | 有效工时的链内查键 + breakdown 的分组键 |

⚠️ **`worker_id` 的写入点只有「真有工人语境」的那几个**：其余 part 级 / 批次级状态
流转（`DELIVERED` / `INSPECTED` / `STATUS_CHANGED` / `REPAIR_*` / `BATCH_CANCELLED` /
`RECALLED` / `SERIAL_RELEASED` / `FORCE_COMPLETED` / `CANCELLED` / `SOFT_DELETED` /
`PLACED_ON_SHELF` / `CNC_RELEASED` / `SPLIT` / `MOVED`）恒为 `NULL`，因为发起者是
管理员账号或系统调度而非工人。⇒ **工时类指标天然只覆盖工人主动作业**，不是全量。

#### 为什么 `NewPartEvent` 上提 `shared::part_event`

`t_part_event` 是工单级事件流水，**7 个模块都在写，且没有一个是 owner**（`part` /
`prod::batch` / `prod::scan` / `prod::queue` / `outsource` / `assembly` /
`shared::batch`，各自的写入点见 `shared/part_event/mod.rs` 的模块 doc 表）。留在
`part` 域的后果就是其余 6 个写方全部**反向依赖** `modules::part`，与逐域剥离方向
相反；留在任何单一「用例域」（比如 `prod::batch`）后果相同。故与 `shared::batch`
（批次公共语义）同批上提到 `shared`，依赖方向变成 **域 → shared**。

#### 为什么本表**没有** `T*` 投影

仓库惯例：`T*` = 某域 **owned** 的表的全列 `#[derive(sqlx::FromRow)]` 投影（全仓
`pub struct T*` 全部 derive FromRow、零例外）。`t_part_event` 不满足「owned」（7 个
写方、没有 owner），因而也不该有 `T*` 投影 —— 本表读侧一律走**端点私有 row struct +
VO**：零件时间线用 `part::service::phase1::events` 的 `EventListRow`，statistics 域用
自己的 row struct（`sql.rs` 底部 4 个）。给一张读侧人人自定义列集的表造一个 15 列的
公共投影，只会让「哪一列是端点要的」这件事被投影结构掩盖。

⚠️ **`statistics` 域的 3 条有效工时 repo SQL 目前零集成测试覆盖**（该域三个 service
方法的 happy-path 用例早已删除，见上文「覆盖空洞登记」一节）。补测试时遵守「插入行用
`now_naive()` 时查询窗口不得硬编码绝对日期」的约定。

**2026-10-02 shelf 域拆分**（依据「货架自身包括了账号的部分和工序映射相关的部分，
应拆分到 iam 域和 prod 域」）：
- **账号部分 = 消除**：`ShelfOut.account_count` + `ShelfRepo::count_accounts_by_shelf`
  删除。货架域对账号的唯一耦合就是这一个字段，而绑定真源本来就在 iam 域
  `t_user_role`（`scope_type='shelf'`）→ **零 iam 模块改动**
- **工序映射 = 搬进 prod**：依赖方向由 shelf → prod **翻转为** prod → shelf
  （新 `ShelfProcessService` 只读 `ShelfRepo::get_by_id` 校验货架存在 / scope）；
  shelf 侧 2 个反向 helper（`proc_check_process_exists` /
  `proc_list_existing_process_ids`）删除，`ShelfRepoTrait` 17 → **10** 方法
  （全部 `t_shelf`）
- 端点数：shelf **10 → 7**；prod 32 → **35**（+3）。⚠️ 两条基线均系旧文档计数
  残留，本次逐 router 复核后顺带订正：
  - shelf 侧：文档原写「11」，实为 **10**（`/` 的 GET+POST 算 2 条 route，master 上
    `handler.rs` 自己的 doc 写「写 5」却只列了 4 个 write handler，两处都对不上）；
  - prod 侧：文档原写「31」，系候选池子模块（2026-10-08 起名 `prod::queue`）计数
    残留（标题写 5、router 实为 6 route），实为 **32**；
  - 故 prod 的净变化是 32 → 35（+3），而非按旧账记的 31 → 34。
  - ⚠️ 上文「prod 32 → 35（+3）」是 2026-10-02 shelf 拆分当时的记账（当时 batch 记
    3 端点、shelf_process +3），与本节上表已不同源；端点求和真值见上表下方那行。
- `t_shelf_process` 的 SQL 真源**只在** `prod::shelf_process::repo::ShelfProcessRepo`
  （6 个静态方法 = 平移 4 + 从 `prod::batch` / `prod::queue` 各收 1 处）。
  故意保留 inline 的 3 处见 `src/modules/prod/shelf_process/repo.rs` 的「本仓内保留 inline 的 `t_shelf_process` SQL」一节
- `MockShelfRepoTrait` 全仓零引用，trait 收缩不影响任何单测

**2026-10-10 货架本体迁入 iam 域**（2026-10-02 那轮拆分只处理了账号耦合与工序映射，
货架实体本身当时仍留在独立的 shelf 域，本轮把它整体搬进 iam）：

- 源码 `src/modules/shelf/` → `src/modules/iam/shelf/`（iam 域下的嵌套子模块）；
  URL `/api/v2/shelves/*` → `/api/v2/iam/shelves/*`，**硬切无 alias、旧路径 404**；
  挂载点从 `modules::v2_router` 的 `/shelves` nest 改到 `iam::handler::router()` 的
  `.nest("/shelves", …)`。请求 / 响应 / 错误码 / OCC 契约**逐字未变**。
- **`t_shelf` 行结构收口为一份**：iam 账号侧原有的 `iam::repo::model::Shelf`（13 列、
  不含 `capacity`）删除，`IamRepoTrait::get_shelf_by_id` 改为委托
  `iam::shelf::repo::ShelfRepo::get_by_id`（两条 SQL 谓词逐字相同，都只过滤
  `deleted_at`、不过滤 `is_active`，差别只在列集）。`t_shelf` 全仓只有一份行结构与
  一份 SQL 真源。
- **端点数**：货架子模块自身仍是 5 条（请求 / 响应契约逐字未变），但挂载后 iam 域合计 17 → **22**（+5；**2026-10-14 RBAC 面板再 +8 ⇒ 合计 30**）；`/api/v2` 顶层少一个 `/shelves` nest。
- 测试 binary `shelf` **取消**：`tests/shelf/{main,api,deactivate}.rs` 并入
  `tests/iam/`，用例改名 `shelf.rs` / `shelf_deactivate.rs`（与「一域一份测试 binary」
  的归属一致）。
- ⚠️ **两种「下线」的响应形态不同**：带 `/iam` 前缀的 picker 旧路径
  `/api/v2/iam/shelves/for-return` 仍落进 `/iam/shelves/{id}` 的 `Path<i64>` 提取器 ⇒
  **400 纯文本**；不带 `/iam` 前缀的整段旧路径 ⇒ **干净 404**（连 catch-all 都没了）。
  两条用例分别钉死，别把 400 当成「旧路由还在」。

### `src/modules/mail_digest/` 域（交期到期汇总邮件，MANAGER-only，6 个端点，**2026-10-13 新建**）

| 端点 | 权限 | 说明 |
|---|---|---|
| `GET /api/v2/mail-digest/config` | **Manager 独占** | 读配置**单例**；表空时返**全部默认值 + `version=0` + `id="0"`**（**不是 404**，配置页要能在「还没配过」时打开） |
| `POST /api/v2/mail-digest/config` | **Manager 独占** | **全量覆写**；body **必含 `version`**（缺失 → axum `Json` 提取器返 **HTTP 422 纯文本**，不是业务信封）；成功后调 `task::mail_digest::reschedule` 热重载 cron（**失败不回滚配置**，只 warn） |
| `GET /api/v2/mail-digest/meta` | **Manager 独占** | **内容与调度**侧的表单选项源：频率枚举（含三个 `needs_*`）/ 列白名单 / `upcoming_days_range` / 缺省值（⚠️ **2026-10-16 删掉 `smtp` 段**：发信通道改由 `/api/v2/mail/meta` + `/api/v2/mail/accounts` 提供） |
| `POST /api/v2/mail-digest/preview` | **Manager 独占** | 按「当前配置 + 临时覆盖」渲染 HTML；**不发送、不落流水**；空体 `{}` = 完全用当前配置 |
| `POST /api/v2/mail-digest/send` | **Manager 独占** | 手动触发：渲染 + 真发 + 落流水（**成败都落**） |
| `GET /api/v2/mail-digest/logs` | **Manager 独占** | 发送历史分页（`sent_at DESC, id DESC`；`limit` clamp `[1,200]`、缺省 20；`offset` clamp `>=0`；`total` 不受 `limit` 截断） |

- **只用 GET / POST**（全仓零 PUT/PATCH/DELETE）；挂 `/api/v2/mail-digest`，登记在 `src/modules/mod.rs::v2_router()` 的 `.nest("/mail-digest", …)`（顶层 nest、kebab-case 单段）。**权威路由清单是 `handler::ROUTES`**（lib 单测逐条比对 `router()` 源码）。⚠️ 6 条路由**全是静态段**（无 `Path` 参数）⇒ 不受「静态段先于 catch-all」的 matchit 约束。
- **角色守卫下沉在 handler 第一行**（6 个 handler 逐字 `require_role(Role::Manager)`；菜单亦仅授予 `MANAGER`，`seeds/menu.sql` 的 `scheduled_task` → `mail`，路径 `/mail`）。**错误码开 `217xx` 段**：`21701` 发信通道不可用（**503**，与 `40109` 同款）/ `21702` 发信失败（**502**，SMTP 是外部依赖）/ `21703` **收件人**邮箱非法（含 CR/LF 注入）/ `21704` 配置组合非法 / `21705` 配置不存在（**保留位，当前无生产方**）/ `21706..21708` **scheduled_task 域**的邮箱三条（见下节）。
- ⚠️ **2026-10-16：菜单结构改版** —— 一级「定时任务」/ 二级「邮件」，code 由
  `mail_group` → `mail_digest` 改成 `scheduled_task` → `mail`，路径 `/mail/digest` → `/mail`
  （前端同批改）。旧 code 的**菜单行软删、授权残留由 `rbac.sql` §5 清理**；新行**必须换新 id**
  （`ON CONFLICT (code) WHERE deleted_at IS NULL` 的冲突目标是 code，改 code 后旧行不参与判定 ⇒
  复用旧 id 会撞 `t_menu_pkey`）。
- 🔒 **SMTP 凭据走 `scheduled_task` 域的加密邮箱表**（2026-10-16 整段替换掉原先的「只走 env」）：授权码 AES-256-GCM 加密进 `t_mail_account.encrypted_secret`（主密钥 env `MAIL_ACCOUNT_SECRET`，`src/infra/crypto.rs`），本域只存「选哪个邮箱」（`t_mail_digest_config.sender_account_id`）。**不是散列**：SMTP 发信必须取回明文，bcrypt 取不回。`MAIL_SMTP_*` / `MAIL_FROM_*` / `MAIL_DIGEST_ENABLED` **已全部删除、运行时不读**，无 env 兜底、无引导导入。**注册 cron 的判据**从「env 总闸 + DB 闸」双闸改成单条 DB 闸 `t_mail_digest_config.enabled` **且** `sender_account_id` 指向一个未软删邮箱；**不满足时后端照常启动**（定时任务打一行 info 后不注册 job，手动触发返 `21701`）。
- ⚠️ **域外依赖 = 1 个模块**：`scheduled_task`（内容域 → 通道域；反向不存在）。故本域**不适用** `assert_no_foreign_domain`（已在 `docs/api/mail_digest.md` §8.1 与该域 `mod.rs` 的模块 doc 复核登记），**反向**的护栏装在 `scheduled_task` 域。
- ⚠️ **落流水在发送之后、且不分成败**：只记成功会让「定时任务每天都在失败」完全不可观测。`t_mail_digest_log` 存**收件人快照**（逗号分隔，不 join 配置表）与 `config_version` 快照 —— 配置改了之后仍要看「当时发给谁、用的哪版配置」。该表**刻意无 `deleted_at` / `version`**（只增的审计流水）。
- ⚠️ **`SCHEDULED` 的准备阶段失败也落流水**（2026-10-13）：定时路径下「读配置解析失败 / 收件人为空」发生在 `deliver` **之前**，原先直接 `return` ⇒ 流水表里查不到，「每天都在失败」只剩一行会被轮转掉的 `tracing::warn!`。现在补一行 `status='FAILED'`（`subject` 空串、`html_bytes=0`、两个 count 为 0）。唯一落不了的是 `pool.acquire()` 本身。`MANUAL` 路径不变（失败有 HTTP 错误码给调用方看，21701/21703/21704 仍不落流水）。
- ⚠️ **节开关真的作用于正文**（2026-10-13）：`include_overdue=false` 时正文**完全不出现**该节（不是「出现标题 + 0 条」）；空态由渲染着的那一节自己的「本节无符合条件的工单」承担，不再叠一句全局空态。
- ⚠️ **配置单例靠表达式唯一索引** `uk_t_mail_digest_config_alive ON t_mail_digest_config ((true)) WHERE deleted_at IS NULL` 兜底并发首次初始化（两个请求都看到 `version=0`）⇒ 后到者撞 23505 → service **转成 OCC 冲突**（符合「40901 只表达 OCC」）。
- **域隔离护栏：不适用**（与 `dashboard` / `statistics` 不同）：本域读**若干张别域表**（只读聚合是既定 pattern，统计 SQL 完全在本域内写全、**不 import** 他域 service / repo）：`t_part` / `t_assembly` / `t_part_batch` / `t_customer` + **2026-10-16 批次与位置列新增** `t_shelf` / `t_worker` / `t_outsource_company`（后三张只在 `repo::list_batches` 一条 SQL 里以 `LEFT JOIN` 出现，软删闸门放 `ON` 里 ⇒ 软删的持有人不出现在邮件、批次行也不丢）+ 通道域那一行 `t_mail_account`。⚠️ **别域表的张数刻意不写死**（加表就过期），完整清单（含自有表）以 [`docs/api/mail_digest.md`](docs/api/mail_digest.md) §8.1 的表格为准。⚠️ **2026-10-16 起这里零他域符号引用**：原先唯一的引用是共用常量 `dashboard::repo::DELIVERY_STATUSES`，按用户要求「在邮件通知的域重写 SQL，不要复用其它域的」改成本域自有常量（字面量逐字相同，见下条）。
- ⚠️ **与 dashboard 的口径分叉登记**（逐条理由见 [`docs/api/mail_digest.md`](docs/api/mail_digest.md) §4）：**共用** `DELIVERY_STATUSES` 六态的**取值**（但**常量是本域自己那份** `mail_digest::repo::DELIVERY_STATUSES`，2026-10-16 由 `use dashboard::repo::…` 改来 ⇒ 变成三处人工同步：本域常量 / dashboard 常量 / 前端 `UpcomingDeliveryChart.vue` 的 `LAYERS[].statuses`，改一处必须同步另两处） / 行单位工单级 / 已交量守卫 `NOT EXISTS … status IN ('DELIVERED','COMPLETED')`（含 `COMPLETED` 是因为 force-complete 端点强推它）；**刻意不同** = dashboard 每桶 `LIMIT 30` 而本域**无 LIMIT**（邮件必须给全量）/ 本域多取 `drawing_no` / `order_no` 两列 / 本域未来节有 `[today, today+N)` 上界（`N` clamp 1..60）。**窗口下界取形参 `today`，SQL 里绝不写 `CURRENT_DATE`**（DB 会话时区与 `now_naive()` 是两个时钟，Shanghai 00:00–08:00 会整体偏移一天）。
- ⚠️ **cron 是 6 段带秒**（tokio-cron-scheduler 0.15 内部 `croner` 固定 `Seconds::Required`），且 **`WEEKLY` 必减 1**（本域 `weekday` 用 ISO `1`=周一，cron 第 6 段 `0`=周日；漏减不报错，只会把邮件安静挪到隔一天）。时区**显式传 `Asia/Shanghai`**：`Job::new_async()` 的默认值是 **UTC**，不是本地时区。`JobScheduler::new()` 在 0.15 是 **`async fn`**。
- ⚠️ **2026-10-16 第二轮：工单行下面挂批次子行 + 位置列 + 状态中文**（需求原文① / ② / ③ / ④ / ⑥；⑤ COS 预览链接本轮不做）。**正文里的行有两种**：`DigestRow`（工单级）与**紧跟其下**的 `DigestBatchRow`（批次级，逐行展示**全部**批次、**不过滤状态**）。⚠️ 批次不过滤状态与工单行的已交量守卫**方向相反**：守卫是 `NOT EXISTS（任一批次 `DELIVERED` / `COMPLETED`）` ⇒ **一件交过，整个工单行就不出现** ⇒ 工单行一旦出现，它名下**不可能**有这两态批次（「已交的那几批跟着列出来」不可达），不过滤实际只对 `CANCELLED` 这类**非交割终态**批次有意义。子行**只填 4 列**（批次 / 数量 / 状态 / 位置），其余列留空；「批次」与「位置」两列**工单行留空**。⚠️ **`t_part` 上没有 `is_repairing` 列** ⇒ 工单行只能查 `status` 表，批次子行 `is_repairing = true` 才显示「返修中」（同一件东西两行可能一个「生产中」一个「返修中」，是表结构事实）。状态列输出**中文**（订单管理语义，逐字对齐前端 `src/types/parts.ts::ORDER_STATUS_LABEL`；**不是**统计页那套 `src/constants/partStatus.ts::STATUS_LABEL`），未知值**原样回显**不静默变空串。
  ⚠️ **列白名单 11 → 13 项**（新增 `batch_no` / `location`，都进 `DEFAULT_SET`）⇒ **必须新开迁移**改列 DEFAULT **并回填存量行**（`migrations/20261016000001_001_mail_digest_default_columns.sql`；迁移 append-only、不得改旧文件）。⚠️ **回填会静默改掉管理员已存的勾选**（刻意决定，理由与后果见 docs §8.3）。⚠️ **缺省集是四份副本**（2026-10-16 review 终审更正，原先写「三份」且第三份指错了对象）：Rust `DEFAULT_SET` / 迁移里的列 DEFAULT 字面量 / `tests/mail_digest/main.rs::DEFAULT_COLUMNS`（fixture）/ `tests/mail_digest/config.rs` 的缺省断言。**前 3 份**由 `default_column_set_stays_in_sync_across_its_three_copies` 逐字对拍（② 从迁移文件解析出来当场比），**第 4 份**不参与那次对拍、只由自己那条 `assert_eq!` 守「缺省响应 == `DEFAULT_SET`」⇒ 四条链都锚在 `DEFAULT_SET`。缺省集**按名**描述为「`ALL` 中除 `row_type` / `is_urgent` / `order_no` 外的全部」，**不**说「前 N 个」。
  ⚠️ **`/meta` 的 `columns[]` 多下发两个 key，前端零改动**：`visible_columns` 的 schema 是 `z.array(z.string())`（不是枚举），`MailContentSection` 逐 `meta.columns[].key` 渲勾选框 ⇒ 新 key 自动出现。**前端不要**补一份列名常量表。
- ⚠️ **统计取数恒 ≤ 3 条语句，与工单数 / 批次数无关**（2026-10-16，需求④「禁止出现 N+1 问题」）：上界 = 两条主查询（逾期 / 未来）各 1 条 + `repo::list_batches` 1 条（两侧 id 都为空时不发）⇒ **两节都开（缺省）且查到工单时恰好 3 条**，关掉一节 2 条。⚠️ 节开关拦的是**「不能同时关」**（`21704`）**、不是「只能开一个」**，两节都开是缺省形态 —— 「恒定 2 条」是错的说法（本域**每一处**契约载体都已统一：`CLAUDE.md` / `docs/api/mail_digest.md` §4.7 / `repo.rs` 模块 doc 硬约束 4 / `service::fetch_rows` doc / `tests/mail_digest/n_plus_one_guard.rs` 文件头。⚠️ **刻意不写总数**：这类计数必然过期，错的那个数比没有更坏）。批次查询是**一条语句两个 `UNION ALL` 分支**（PART 支 `t_part_batch ⋈ t_part`、ASSEMBLY 支经子件两跳），**holder 三向解析在同一句里完成**（三张 holder 表 `LEFT JOIN` + `location` 分桶谓词放 `ON` 里，`CASE b.location` 取对应列）。⚠️ `service::fetch_rows` 的**循环体内只许做内存操作**，绝不调 `repo::` —— 由 `tests/mail_digest/n_plus_one_guard.rs` 的**源码级**护栏钉住（扫 `for`/`while`/`loop` 体命中 `repo::`/`sqlx::query` 即红 + 顶层恰好 3 个调用点）。**逐工单循环查批次是功能完全正确、集成测试照样全绿的写法**，只有语句数会线性涨，所以只能靠源码护栏守（局限已在该文件头与 docs §8.3 登记）。
- ⚠️ **列对齐与打印**（2026-10-16，需求⑥）：每张子表各带一份 `<colgroup>`（列宽按**权重归一化后取整**、**最后一列吸收余数** ⇒ 合计恒 100%）+ `table-layout:fixed`；⚠️ **表头每组一张、且与该组 `<colgroup>` 同表**（`<colgroup>` 只对本表列宽生效，表头留在外层表则那行 `<th>` 由另一套布局算法定宽 ⇒ 横向扫行在表头行对不上；**重复是故意的，别「优化」回节级一行**，回归 `template.rs::tests::header_row_shares_table_with_colgroup`）；`colspan` 取**真实列数**（13 列全开即 `colspan="13"`，**不是** `"*"`）。⚠️ **绝不写 `<style>` 块 / `@media print`**（邮件客户端整块剥掉 ⇒ 等于什么都没写），全部内联；有底色的单元格补 `print-color-adjust:exact;-webkit-print-color-adjust:exact`。**批次子行的区分不只靠颜色**：淡底之外还有「第 N 批」文案 + 首格全角缩进 —— 打印丢色时仍分得开。
- ⚠️ **多副本部署不防重**（沿用 `task::auto_complete` 同一现状，N 个副本可能同一时刻发 N 封信）；**`/send` 无频控**（唯一护栏是 MANAGER 角色 + 两道 enabled 闸）。两者均登记为后续独立重构。⚠️ **与「进程内互斥」是两层不同的问题（2026-10-13 review 第 2 轮 M1）**：`task::mail_digest::swap_job` 用一把 `tokio::sync::Mutex` 把「摘旧 → 读库 → 注册新」整段串行，保证**单进程内**调度器里恒 ≤ 1 个 job（原先只有「取 job id」受保护，两个并发 `POST /config` 会各注册一个、先注册那个**永远摘不掉**）；多副本防重**仍需**分布式锁 / 领导者选举，两者不要互相替代。
- **覆盖空洞**：`/send` 的**成功路径**（`status='SUCCESS'` 流水行 / 真实投递）**无集成测试** —— 测试环境无可达 SMTP 且刻意不引入 lettre mock transport。替代覆盖：`/preview` 覆盖渲染 + 统计 SQL、「必失败」路径覆盖落流水、`mailer.rs` lib 单测覆盖 TLS 映射表与收件人解析。⚠️ **2026-10-16「SMTP 层失败」的集成测试造法换过一次**：host/port 由编译期预设表固定后，「指向 `127.0.0.1:1`」不可复现，改用「自定义名含 `<`/`>` ⇒ `From` 头 `Mailbox` 解析失败」（零网络 IO）。详见 [`docs/api/mail_digest.md`](docs/api/mail_digest.md) §8.3「覆盖空洞」第 3 条与 [`docs/api/mail.md`](docs/api/mail.md) §8.3 第 8 条。

### `src/modules/scheduled_task/` 域（邮件通道：邮箱实体 + SMTP 预设，MANAGER-only，6 个端点，**2026-10-16 新建**）

| 端点 | 权限 | 说明 |
|---|---|---|
| `GET /api/v2/mail/meta` | **Manager 独占** | SMTP 预设表（**纯函数、不查库**）：网易 163 / QQ，均 `465` + `WRAPPED` |
| `GET /api/v2/mail/accounts` | **Manager 独占** | 邮箱列表（默认排最前）；**绝不返回**密文 |
| `POST /api/v2/mail/accounts` | **Manager 独占** | 新建（`name`/`preset`/`account`/`secret`）；**不设默认** |
| `POST /api/v2/mail/accounts/{id}` | **Manager 独占** | 全量覆写；body **必含 `version`**；`secret` 空串/缺省 ⇒ **沿用旧密文** |
| `POST /api/v2/mail/accounts/{id}/delete` | **Manager 独占** | 软删（body 也必含 `version`）；被 mail_digest 引用 ⇒ `21708` |
| `POST /api/v2/mail/accounts/{id}/set-default` | **Manager 独占** | 置默认（同事务**先降后升**；body 必含 `version`） |

- **域范围 = 通道**（邮箱 + 预设 + 授权码加密），**内容与调度仍归 `mail_digest`**（`/mail-digest/*` 前缀不动）。切分线：「有哪些发信通道」与「用通道发什么」是两个独立演进的职责，加第二种邮件类型时前者不动。
- 🔒 **凭据加密入库（AES-256-GCM）+ 掩码不回显**：`encrypted_secret` 存 `base64(nonce12‖ct‖tag16)`、AAD = `t_mail_account.secret.v1`，主密钥 env **`MAIL_ACCOUNT_SECRET`**（`src/infra/crypto.rs` + `config.rs::MailAccountSecret`，**缺失/长度不对 ⇒ 启动 bail**）。⚠️ **不是散列**：SMTP 发信必须取回明文，bcrypt 取不回。**刻意不设** `secret_configured` 出参 / `secret_fingerprint` 列 / 掩码字段 —— 建邮箱时授权码必填 ⇒ 该布尔恒 true，是会误导出「以后再补授权码」流程的**死字段**。
- ⚠️ **`ring 0.17` 是「把传递依赖提为直接依赖」，零新 crate**（早于本轮就在 `Cargo.lock` 里，rustls 的传递依赖，版本不变）。**刻意不用** `aes-gcm`（不在 lock 里）与 bcrypt（同上）。
- ⚠️ **`MAIL_ACCOUNT_SECRET` 一换，`t_mail_account` 里所有授权码永久解不开**（GCM 无轮换机制）。「逐行解密→换密钥重新加密」的迁移流程**未实现**，已登记在 [`docs/api/mail.md`](docs/api/mail.md) §8.3 第 1 条。
- **只用 GET / POST**（全仓零 PUT/PATCH/DELETE）；挂 `/api/v2/mail`，登记在 `src/modules/mod.rs::v2_router()` 的 `.nest("/mail", …)`（顶层 nest、**单段、刻意不是 kebab-case**，与菜单 seed 的 `path='/mail'` / 前端路由同一个词）。**权威路由清单是 `handler::ROUTES`**（lib 单测逐条比对 `router()` 源码）。⚠️ 与同族的 `/mail-digest` 无冲突：matchit 按段数 + 静态段优先匹配。
- **角色守卫下沉在 handler 第一行**（6 个 handler 逐字 `require_role(Role::Manager)`；菜单亦仅授予 `MANAGER`）。**新错误码**：`21706` 邮箱不存在/已软删（**404**）/ `21707` 邮箱重名（**409**，⚠️ **不是** 40901 —— 那只表达 OCC）/ `21708` 邮箱被引用禁删（**409**）。
- ⚠️ **`set-default` 是本域唯一开事务的写路径**（「先降旧、再升新」两条 UPDATE 必须原子，否则两个并发请求会让库里**一个默认都没有**）；删 / 改 / 置默认**三条写端点都吃 OCC**（表带 `version` 列 ⇒ 按 CLAUDE.md §8 body 必填）。
- ⚠️ **「被引用禁删」有 TOCTOU 窗口**（查引用与软删不在同一事务）⇒ 产出「配置指向已软删行」，发信时以 `21701` 暴露。**有意不修**：为一个配置型实体引入跨表约束不划算，故障面是「发不出信 + 一条明确错误」。
- **域隔离护栏：适用**（`assert_no_foreign_domain` 扫 `src/modules/scheduled_task/**`，`scheduled_task_domain_depends_on_no_other_domain` 在 CI 里钉住）。⚠️ 本域**唯一**的跨域表引用是「查 `t_mail_digest_config` 有几份配置指向本邮箱」那一条 COUNT，**SQL 写在本域**（既定 pattern，同 `dashboard`/`statistics`/`admin`），**不 import** 他域任何符号。**反向**（mail_digest import 本域）见上一节。
- 完整契约（逐字段 / 预设枚举 / 加密口径 / 前端配套 / **已知偏差 11 条（§8.3 第 1–11 条）**）见 [`docs/api/mail.md`](docs/api/mail.md)。

### 状态派生契约（2026-10-01）

三层状态**单向**派生（子件 = `t_part` 中 `assembly_id` 非空的行）：

```
t_part_batch.status            ← 唯一真源
   │  shared::batch::status::rollup_part_derived（min-progress）
   ▼
t_part.status / next_process_id   ← 派生缓存
   │  assembly::compute_assembly_target
   ▼
t_assembly.status               ← 派生缓存
```

- **写 `t_part_batch.status` 只能走 `src/shared/batch/status.rs`**
  （`apply_batch_status_change` / `apply_bulk_batch_status_change_for_part`）。
  它一函数内完成「写批次（OCC + SQL 层源状态白名单）→ 回填 part 派生列 →
  级联 assembly → 终态序列号归档 / 释放」，所以 **caller 没有「要不要顺手调
  sync」这个选项**（13 个历史 `mark_batch_*` 写点已改写为其上的薄包装）。
  ⚠️ 手工补调 `PartService::sync_from_batch_change` 是**反模式**：第二次派生必为
  `NoChange`，会把响应的 `synced_assembly_id` 吞成 `null`、连带 WS 的
  `ASSEMBLY_UPDATED` 永不发。
  ⚠️ **例外（2026-10-11 新增）**：装配件级逃生写点
  `force_complete_all_batches_for_assembly` **只写不派生** —— 批量 UPDATE 命中
  0 行时派生循环一次都不跑，而该端点的语义是「整体判为已交」，终态改由
  `AssemblyRepo::force_complete_children` / `force_complete_status` 两条显式写
  承担。职责切分与返回值用法限制见该函数 doc，不要按上面「写必派生」的口径去
  「修」它。
- **CI 强制**：`cargo test --lib` 的
  `shared::batch::status::write_guard_tests::no_outside_file_writes_batch_status`
  扫全 `src/**/*.rs`，除 `src/shared/batch/status.rs` 外任何文件写
  `UPDATE t_part_batch SET status …` 即失败（注释 / `#[cfg(test)]` 块 / 只改其它列
  的 UPDATE 不在判定范围）。写入口再搬家时该测试的允许路径常量要同步改。
  同模块另有 `shared::batch::status::bind_guard_tests::bind_placeholders_are_contiguous`
  钉住单行 UPDATE 的占位符 ↔ bind 个数。改动 `mark_batch_*` 后请顺手跑一次。
- **CI 强制（本仓级，2026-10-09 新增）**：`cargo test --lib` 的两道
  `shared::snowflake_guard::tests::*` 把「测试期进程内只从唯一源取号」钉成事实 ——
  守卫对象是 `src/shared/snowflake_guard.rs`，与上面的 `write_guard_tests` 同级：
  - `no_local_generator_construction_outside_whitelist` —— 扫 `src/` + `tests/` +
    `test-support/src`，白名单 4 个文件（`SANCTIONED`）+ 每个白名单文件的命中处数上限
    4 条（`SANCTIONED_HIT_CAPS`，实测值；含两个「唯一源」文件各 1 条，防白名单文件内部
    再开一个 generator）。判定只认**代码字符**，注释 / 字符串 / raw string 里贴的旧写法
    样例天然不误报；
  - `no_lib_unit_test_pulls_in_a_second_generator` —— 扫 `src/`，只放行 test-support 的
    `::test_pool` / `::test_redis_url` 两个**不碰 generator** 的入口。防的是 lib 单测
    进程里出现第二个 generator（那不是一处 `::new`，上一条抓不到，成因见「测试取号」
    一节的 dev-dependency 环）。
  ⚠️ **已知漏报盲区登记在 `src/shared/snowflake_guard.rs` 的模块 doc 顶部**（「未登记的
  漏报形态一律视为护栏缺陷」），CLAUDE.md 只放指针、不复制内容。护栏搬家或改判定逻辑时
  请连带复核那份 doc。
- **派生层不得否决主操作**：派生写不抛错，`sync_assembly_status` 的 OCC 冲突降级为
  「跳过 + `tracing::warn!`」。
- **派生层不得覆盖主操作**（2026-10-01 review 第 1 轮 B1 补齐，两条都要守）：
  - `PartRepo::update_part_rollup` 的 WHERE 带
    `AND status NOT IN ('COMPLETED','CANCELLED')`。`POST /parts/{id}/cancel` 先把
    part 打成 CANCELLED（主操作），随后的批次级联若按 min-progress 算出 COMPLETED
    （「已完成批次 + 其余被批量取消」），**不许**写回去；
  - bulk 入口传 `PartDerivation::KeepPartTerminalAsIs`：显式「跳过 part 写、继续派生
    父装配件」，否则跳过 part 写的同时父件也会与子件长期不一致。
  - 回归测试：`tests/part/lifecycle.rs::cancel_part_is_not_overwritten_by_rollup_completed`。
  - 已知代价：**已终态**的 part 不再被 rollup 改写（那需要一次人工决策）——
    ⚠️ 2026-10-13 起**没有**任何自动对账端点能改它了（原 `admin/recompute-rollup`
    的 `parts_skipped_terminal` 显式上报能力随域一起消失）。此类行只能靠
    force-complete / cancel 或业务流修。
- **`StatusChange` 的三态约定**：`Option` 的 `None` 一律是「保持原值」，「清 NULL」
  由同名 `clear_location` / `clear_holder_id` / `clear_process_id` /
  `clear_process_step_id` 显式表达。出池（召回 / 送检 / 外协收回 / 返修出池）
  必须同时清 `location` + `current_holder_id` + `current_process_id` +
  `current_process_step_id`，否则 UI 会显示「待投产的工单还压在生产架上」。
- **终态批次一律不在任何位置、也不在任何人手上**（**2026-10-14 新增不变式**）：
  `CANCELLED` / `COMPLETED` / `DELIVERED` 三个终态状态下共 6 个写点（逐点清单见
  [`docs/api/batch.md`](docs/api/batch.md) §6.1 与 `PartBatchRepo::update` 的函数
  doc）都必须把 `location` + `current_holder_id` **一并清 NULL**（`clear_location` /
  `clear_holder_id`，或 bulk 入口与装配件级强推的 UPDATE 常量赋值）。此前这 6 个
  写点都只改 `status`，留下
  `location='WORKER' + current_holder_id=<某工人>` 的「幽灵行」—— 永久吃掉工人
  `max_held_batches` 的额度且没有任何清理路径。存量由 append-only migration
  `20261013000002_001_clear_terminal_batch_location` 回补（只清数据、不改判据），
  两者互补、缺一不可。⚠️ **`mark_batch_status_only` 只对 `CANCELLED` 特判**、绝不
  无条件清 —— 它还服务 `READY_TO_SHIP` / `IN_PROCESS` 两个**非终态**目标，那两处的
  `location` 是有业务含义的落点。完整读侧影响清单（**含 4 处不过滤 `status` 的读侧**，
  且清单不穷尽）见 [`docs/api/batch.md`](docs/api/batch.md) §6.1。
- ⚠️ **`t_work_type.max_held_batches` 的 `NULL` = 「未配置」，不是「不限」**
  （2026-10-14 订正列注释，migration `20261014000001_001_fix_max_held_batches_column_comment`）：
  `move` / `refill` / 报工台 `pick-up` 返 `20904`，`auto-allocate` 逐 worker 记
  `MAX_BATCHES_NOT_SET` 跳过。「未配 max ⇒ 不可取件」由各路径的 service 入口把关
  （**2026-10-15** 起 `take_one_from_pool` 不再自己查 max，cap 由 service 算出传入）。
  完整四路径对照见 [`docs/api/queue.md`](docs/api/queue.md) §4.7。
- ⚠️ **`t_work_type.auto_allocate_limit`（自动分配上限，2026-10-15 新增）**：
  三条自动补料路径（`POST /prod/queue/refill`、`POST /prod/queue/auto-allocate`、
  报工台 worker-scan 的同事务 refill）的 target =
  `auto_allocate_limit.unwrap_or(max_held_batches)`；`NULL` = 未配置 ⇒ 兜底 max；
  有值须 `1..=max_held_batches`（create / update 同一处合并校验矩阵，错误码
  `20104`）；**`move` / `pick-up` 手动单批路径只按 `max_held_batches` 判、不受自动
  上限约束**（「自动补料只补到 auto、人工可临时手动领到 max」是刻意分工）；自动路径
  触顶的诊断字面量是 `AUTO_LIMIT_REACHED`（`stopped_reason` / `skipped_reason`；
  ⚠️ `WORKER_POOL_EMPTY` 的 WS 广播条件不变、仍只认 `POOL_EMPTY`）。同一轮删除
  auto-allocate 的 `mode` / `fill_ratio`（TIME 模式下线，`max_held_minutes` 列保留
  但不被 Rust 引用；`20703` / `20704` 错误码释放）。完整口径见
  [`docs/api/queue.md`](docs/api/queue.md) §3.2 / §4.8 与
  [`docs/api/scan.md`](docs/api/scan.md) §2.5 / §4.2。
- **终态序列号归档事件的 id 必须是真实雪花**（`StatusChange::event_id`，由 caller 从
  `SnowflakeIdGenerator` 透传）：`GET /parts/{id}/events` 是 `ORDER BY id DESC`，
  拿 `part_id` 之类的建单期 id 顶替会把归档事件排到时间线最底部，且二次进终态时
  pkey 冲突 → 事务 500。
- ⚠️ **兜底对账端点已于 2026-10-13 下线**（admin 域整域删除，见「派生层对账逃生口
  的移除记录」一节）。**算法本体一行未动**：派生仍由 `rollup_part_derived` /
  `sync_assembly_status` 在业务流里跑，新增派生算法时**不要**在任何别处重写一遍。
  现在唯一的调用方是业务写入口（`apply_batch_status_change` /
  `apply_bulk_batch_status_change_for_part`），没有第二个「手动重算」的口子。
- 词汇：batch/part 8 态 + `is_repairing` 标记（`REPAIRING` 已于 2026-10-01 降级为
  boolean 列，DB 不再产生该 status）；assembly 7 态（无 `OUTSOURCE`，子件
  `OUTSOURCE` ⇒ 父 `IN_PROCESS`）。完整状态派生契约见本文件上方「状态派生契约（2026-10-01）」一节与 `src/shared/batch/status.rs`。

### 派生层对账逃生口的移除记录（2026-10-13：admin 域整域删除）

**硬切下线、**无 alias**：旧 `POST /api/v2/admin/recompute-rollup`（Manager）
整段 `/api/v2/admin/*` 前缀随域删除 ⇒ 旧路径是**干净 404**（整段 nest 都没了，
不是落进某个 `Path<i64>` catch-all 的 400 纯文本 —— 后者才对应「同段数里
静态段被 catch-all 吞掉」的形态，见「路由声明规约」的 matchit 约束）。

随端点一起消失的是三项**曾经是这个端点独有**的能力，登记在此以免后人重建时
重复踩同一个坑：

1. **分表游标续扫**：全量对账靠 `part_after_id` / `assembly_after_id` **两个独立
   游标**推进并逐轮回传 `next_part_after_id` / `next_assembly_after_id`。教训：
   `t_part` 与 `t_assembly` 的 id 出自**同一个雪花流**、按时间序交错，共用一个游标
   会**永久跳过** `(assembly_max, part_max]` 那段装配件、却仍报
   `truncated=false`（「已覆盖全表」是假的）。
2. **终态跳过显式计数**：终态守卫拦下的行库里一个字节都没改，若报告不把它单独
   计数（`parts_skipped_terminal` + `skipped_terminal[{id,current,derived}]`），
   就与「数据已一致」在报告里**同形** —— 兜底工具给出假干净报告是最坏的失败模式。
   ⚠️ 该诊断能力**在算法层仍然存在**（`RollupOutcome::terminal_skip` 上抛），
   消失的只是「把它呈报给人」的那一层；新增派生算法时**必须**继续填它。
3. **`limit` 上限**：防止单次请求把整表锁在一个长事务里。

⚠️ **WS 事件 `ROLLUP_RECOMPUTED` 自此无人发送**：它只在真有变化时、且在全部块
commit 之后发一次汇总。前端若订阅了它，会成**死订阅**（永不触发、也不报错）。
按用户指示本轮**不处理 WS 事件**，只在此点出。

### ⚠️ 但派生算法一行未动（不要连算法一起删）

保留：`shared::batch::status` 的 `rollup_part_derived` / `sync_assembly_status` /
`compute_part_target` / `compute_assembly_target`；`seeds/admin.sql`
（那是 ADMIN **账号 / 元角色**的初始种子，与 admin **域**毫无关系，不许碰）；
`t_part` / `t_assembly` 上的派生缓存列与全部业务写入口。

**回归网**（admin 域删除**之前**先补上的）：
- `src/shared/batch/status.rs` 的 `#[cfg(test)] mod sync_assembly_status_tests` ——
  7 条 mock 单测，守父装配件聚合的终态短路 / OCC 冲突降级 /
  `ALL_CANCELLED → CANCELLED`（零 DB）；
- `tests/part/rollup_derived.rs` —— 8 条直调 `rollup_part_derived` 的集成测试
  （**不走 HTTP**），守漂移修正 + 幂等 + 序列号归档释放 + 级联父装配件
  + 终态守卫上抛 `TerminalSkip`。

## 必须遵守的架构约定

1. **事务边界在 handler（2026-09-21 重构 + 2026-09-22 删 `PgIamRepo` 转发壳后 iam 与其余 20 个 handler 文件一致）**：handler 显式 `state.pool.begin()` / `tx.commit()`，错误路径 tx drop 隐式回滚。service 不知事务——所有跨 repo 操作经 `repo: R`（by-value；`IamRepo` / 域内对应 trait 已直接 `impl for &mut PgConnection`，handler/service 借 `&mut *tx` / `&mut *conn` 即可）参数传入。
   - 例外清单（仍走 handler 边界）：
     - `_e2e` 直调方（测试 fixture 自管 tx）
     - 既有 `tests/iam/{api.rs,middleware.rs}`（2026-09-23 PR13 拆分；原 `tests/iam_api.rs` / `tests/auth_middleware.rs` 已合到 `tests/iam/`）等 HTTP 契约测试（不改测试代码）
     - 读端点（`me` / `list_users` / `get_user` 等）`pool.acquire()` 不开事务，service 借 `&mut PgConnection` 跑查询，连接用完即 drop。
     - `POST /api/v2/parts/batch-update-order-info`（2026-10-06）：该端点契约是
       **逐行部分成功**（HTTP 200 + 信封，`failed[]` 逐条列出行级错误）。PG 事务内
       一条语句报错后整笔进入 aborted 状态 ⇒ 其余行连带失败、末尾 `commit()` 也报错，
       「部分成功」被整体降级成 500。**不要**改回 `begin()+commit()`，也**不要**上
       savepoint（savepoint 下末尾 `commit()` 一旦失败会回滚已成功的行却仍返回
       `updated_count = N`，变成静默数据丢失）。各行写的是互不相干的行、单条 UPDATE
       自身原子，autocommit 才是这个契约要的语义。
2. **统一响应信封**：handler 返回 `Result<Json<R<T>>, AppError>`。`R { code: 0, message: "ok", data }`；错误由 `AppError::into_response()` 装入同一信封。不做 middleware 后置包装。
3. **错误码分段契约**（`src/shared/error.rs::code`，与 Python 前端对齐）：0 成功、4xxxx HTTP 语义、5xxxx 系统、2xxxx 业务域（每域一个段，如 201xx 零件/客户、214xx 送货单，新增域错误码先入对应段）。
5. **状态机不写 DB**：`statemachine.rs` 只做内存 enum + `can_transition_to` 迁移表；事件日志由 service 在事务内统一插入。
6. **WS 广播在 commit 之后**（对齐 Python 延迟广播模式），用 `state.ws_hub.broadcast(...)`。
7. **路由挂载**：业务 REST 统一 `/api/v2`（与 Python `/api/v1` 并行），WS 在 `/ws/dashboard`。`/api/mcp` 不在本仓库。
8. **OCC（乐观锁）**
   - **所有对带 `version` 列的表的写端点，body 必须显式收客户端 `version` 且必填**（无 `#[serde(default)]`）。缺失 → HTTP **422 纯文本**（axum `Json` 提取器），**不是**业务信封。
   - **禁止「读-再-比」式的隐式 OCC**。service 内 `get_batch_by_id` 拿到的 `version` 只能用来做**兜底对账**（0 行时区分归因），不能**替代**客户端传值。理由：看板数据是 30s 缓存的快照，隐式 OCC 下「用户看到 5 件 → 实际移动 3 件」（期间他人改动）会**静默成功**。
   - **`40901 VERSION_CONFLICT` 只表达 OCC**，不得用于业务唯一键冲突（如 worker 的 `badge_code` create 撞车 —— 已知违规，见 `prod/worker/service.rs` 的 `create_worker`）。业务唯一键冲突须有专用码。
   - **豁免清单（刻意无锁，改动时勿「补齐」）**：
     - 逃生口：`prod/parts/{id}/force-complete`（dto 注释明写「依赖 SQL 行锁串行化」）、`assemblies/{id}/cancel`（service 注释明写「设计上无 OCC，单向状态翻转」）
     - 幂等：`iam/users/{id}/reset-password`（⚠️ 另一条 `admin/recompute-rollup`
       已于 2026-10-13 随 admin 域删除，见本文件「派生层对账逃生口的移除记录」）
     - junction 整组替换：`work-types/{id}/processes`、`shelf-processes/{shelf_id}` —— 锚在父表 `version`，不是映射行
     - 服务端驱动：`queue/refill`、`queue/auto-allocate` —— `SKIP LOCKED` 是正解，客户端对「取哪几批」没有意见
     - BFF 转发 / `_e2e`
   - `shared::batch::status::StatusChange.expected_version: None` 是逃生通道，只有豁免项可用。
   - **一条端点的目标域按「前端消费方」判定**（哪个页面的哪个按钮在调它），不按后端逻辑相似度 —— 后端按表/状态机聚在一起，前端按页面聚在一起，切分线不同。
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

   - **仓内多数域用转发式**（`outsource` / `files` / `assembly` / `statistics` / `iam`），少数用 `mod.rs` 内联（`part` / `com` / `prod::queue`；⚠️ 2026-10-13：`admin` 域整域删除）。转发式是多数形态，新域与重构域一律按转发式。（2026-10-10 订正：原清单里的 `shelf` 已不是一级域 —— 货架子模块于同日迁入 iam，仍是转发式。**2026-10-13 订正**：`part_file` / `cnc_program` 也已不是一级域 —— 两者于同日收编成 `files` 容器域及其嵌套子模块，URL 硬切到 `/files/objects/*` 与 `/files/cnc/*`，`files` 本身是转发式；⚠️ **同批再订正 `delivery_note`** —— 它自 **2026-10-08** 起是 `com::delivery_note`（原清单里写它时它还是一级域），容器 `com` 走 `mod.rs` 内联（`com/mod.rs::router()` 直接 `.nest(…)`），故已从转发式清单移到内联侧。）
   - **一个前缀一个 router 工厂**。多个独立顶层前缀时用多个 `xxx_router()`（`outsource` 有 4 个），而不是一个工厂返回合并 router —— 合并后 matchit 的注册顺序约束会跨前缀纠缠，排查困难。
   - **`handler.rs` > 600 行或工厂数 > 4 时拆 `handler/<子域>.rs`**，`handler.rs` 退化为聚合器。`prod/queue/handler/`、`prod/batch/handler/` 是范本。
   - **`mod.rs` 的模块 doc 逐条列出端点表与硬切记录**；域改名/URL 硬切必须写「旧路径 404，无 alias」。
   - **matchit 注册顺序是硬约束**：同段数时静态段必须先于 `Path<i64>` catch-all，否则静态路径被吞成 **400**（不是 404）。历史踩坑：`outsource` 旧 `pool_router` 的 `/counts`、`quote_router` 的 `/quotable-parts`，`prod/shelf_process` 的 `/by-process/{id}`。段数不同则天然无冲突（`/move` 1 段 vs `/processes/{id}` 2 段）。
   - **顶层 nest 集中登记在 `src/modules/mod.rs::v2_router()`**，一个前缀一行 + 注释说明归属域与硬切日期。跨域共用的端点也在这里登记，并在**拥有它的域**的 `mod.rs` doc 里注明「本域有 N 处挂载」。
     - **2026-10-13 新增**：`.nest("/mail-digest", mail_digest::router())`（`mail_digest` 域，6 端点，**顶层 nest、kebab-case 单段**，与 `/part-files` / `/cnc-programs` / `/outsource-queue` 同款；无硬切记录，本域新立）。本域 6 条路由**全是静态段**（无 `Path` 参数）⇒ 不受上面那条 matchit 排序约束。
     - **2026-10-16 新增**：`.nest("/mail", scheduled_task::router())`（`scheduled_task` 域，6 端点，**顶层 nest、单段、刻意不是 kebab-case** —— 菜单结构改版时与前端路由、菜单 seed 的 `path='/mail'` 对齐成同一个词）。⚠️ 与 `/mail-digest` **无冲突**：matchit 按段数 + 静态段优先匹配，`/mail/meta`（2 段）不会落到 `/mail-digest`（1 段静态段）上。本域最长段数 3、`Path<i64>` 只在第 2 段、**无 2 段静态路径** ⇒ 也不受上面那条排序约束。

## `docs/api/` 目录约定（2026-10-07 确立）

**现状**：`docs/api/` 有 **14 份**文件（与本文件顶部「📌 前端对接」一节同口径；2026-10-11 review 第 1 轮订正过「9 份 / 11 份」两处自相矛盾与漏登 `wx.md`，2026-10-12 新建 `statistics.md` 后为 12，2026-10-13 新建 `mail_digest.md` 后为 13，**2026-10-16 新建 `mail.md` 后为 14**），每份 = **一个域的整域契约**：

| 文件 | 覆盖域 |
|---|---|
| [`docs/api/dashboard.md`](docs/api/dashboard.md) | `dashboard`（只读聚合域，3 个 HTTP 端点 + WS 首帧 / 增量） |
| [`docs/api/programming.md`](docs/api/programming.md) | `prod::programming`（待编程一览） |
| [`docs/api/inspection.md`](docs/api/inspection.md) | `prod::inspection`（待品检队列 + 扫码树） |
| [`docs/api/queue.md`](docs/api/queue.md) | `prod::queue`（工序候选池 + 工人持有 + 下发 / 召回 / 移动 + 队列板聚合） |
| [`docs/api/batch.md`](docs/api/batch.md) | `prod::batch`（批次流转，剥离中间态） |
| [`docs/api/scan.md`](docs/api/scan.md) | `prod::scan`（报工台：扫工牌 + 扫码找件诊断 / 接管 / 外协收回 + 取件 / 放回列表 + 放回送检 + 手动领取） |
| [`docs/api/outsource.md`](docs/api/outsource.md) | `outsource`（外协公司 / 报价 / 发货 + 外协看板与三合一写端点；公司 7 条 / 报价 7 条 / 发货 2 条 / 看板 3 条） |
| [`docs/api/delivery_note.md`](docs/api/delivery_note.md) | `com::delivery_note`（送货单 + 送货分组 + 司机候选，2026-10-08 平移 com + 入单收敛为扫码单一入口） |
| [`docs/api/shelves.md`](docs/api/shelves.md) | `iam::shelf`（**iam 域下的嵌套子模块**：货架 CRUD + 负载与自动选架，5 端点；2026-10-10 自独立 shelf 域迁入 iam，URL 硬切 `/api/v2/iam/shelves/*`） |
| [`docs/api/iam.md`](docs/api/iam.md) | `iam`（认证 + 账号 + 企业微信绑定 + 货架子模块 + **RBAC 角色权限面板**，**30 端点**；session 5 + users 12 + shelves 5 + rbac 8，4 条破坏性变更与 OCC 锚点；ADMIN 元角色通配 / 停用即降权） |
| [`docs/api/wx.md`](docs/api/wx.md) | `wx` 微信小程序 BFF（**2026-10-11 新建**；**按小程序页面切子模块**：`login/` + `part_list/` + `production/`；5 端点；URL 硬切无 alias 5 条、VO camelCase 逐字对齐前端卡片模型、`deliveredQty` 真值、尾斜杠 404 实测钉死、`counts` / `list` 口径对齐、★ 2026-10-12 `part_list` 日期 + 7 类 tab 双维筛选） |
| [`docs/api/statistics.md`](docs/api/statistics.md) | `statistics` 生产统计（**2026-10-12 新建**；3 个 MANAGER-only 只读端点、有效工时口径表与数据覆盖登记、跳序取件域清空的移除记录） |
| [`docs/api/mail_digest.md`](docs/api/mail_digest.md) | `mail_digest` 交期汇总邮件（**2026-10-13 新建**；6 个 MANAGER-only 端点 + 定时任务；6 段带秒 cron 与 WEEKLY 减 1；217xx 错误码；失败也落流水；与 dashboard 的口径分叉与**六处**「实现与注释不符」偏差登记（第 5 条已修）；**2026-10-16** env 凭据整轮删除、配置加 `sender_account_id`、`GET /meta` 删 `smtp` 段；★ **同日第二轮**：批次子行 + 位置列 + 状态中文 + 本域自有 SQL、列白名单 11 → 13、定长取数（恒 ≤ 3 条语句）、列对齐与打印） |
| [`docs/api/mail.md`](docs/api/mail.md) | `scheduled_task` 邮件通道（**2026-10-16 新建**；6 个 MANAGER-only 端点；AES-256-GCM 加密入库 + 掩码不回显；SMTP 预设两家均 465/WRAPPED；被引用禁删；21706..21708；密钥轮换未实现的缺口） |

**约定**：

1. **一域一份 md，文件名取域路径的最后一段**（`prod::programming` → `programming.md`，嵌套域不按父目录分目录）。新增文档照 `dashboard.md` 的**八节骨架**：`1 端点表` / `2 <主结构> 逐字段`（含子结构）/ `3 <第二块能力>` / `4 口径表`（行单位差异、与其它域的有意分叉）/ `5 状态域约定（无编译期保障）` / `6 移除记录` / `7 与 WS 的关系` / `8 表依赖与前端配套`（读的 N 张表 / 前端配套改动清单 / 已知偏差登记）。域小到撑不起八节时可合并，但**「移除记录」与「已知偏差登记」两节不得省** —— 前者防后人重建已删的东西，后者是已知的数字/口径不一致及产品决议（处理还是不处理）的唯一登记处。
2. **注释与文档分工**：注释写**局部**契约（某个字段的取值口径、某段 SQL 的取舍、某个常量的改一同步二义务），`docs/api/` 写**整域**契约（端点全貌、跨端对数前的口径差异、错误码、WS 关系、前端配套）。判据是「读的人会不会需要跨文件拼起来才能不误解」—— 会，就进 `docs/api/`。**注释就是本仓的契约载体**：`docs/api/` 里某条信息只有一两句话、且只有本域维护者会看时，就地写进注释，不要为它单开一份文档。
3. ⚠️ **引用不存在的文档路径是禁止的**。写注释 / 文档时指向代码就指代码（`src/modules/.../repo.rs` 的模块 doc、某个 `resolve_*` / 常量 / 单测名），指向文档就确认那个文件真存在。历史教训：2026-10-07 清理前，26 处引用分布在 23 个文件里，指向 23 个**从不存在的** `docs/api/*.md`，后人无法判断该文件是漏提交还是该信息本就只在注释里。提交前跑一次 `rg -oIN 'docs/api/[A-Za-z0-9_./-]+' | sort -u` 逐个核实目标存在。
4. **文档里不写行号**。指向文件 / 模块 doc 标题 / 函数名即可 —— 行号必然随编辑过期。
5. 后端代码变更（新增 / 修改 / 删除端点，或修改 DTO 字段 / 错误码）必须**同步**该域的 `docs/api/` 文件（若该域有）与代码注释。

## 集成测试目录结构（2026-09-23 PR13 重构后）

integration test binary 已重组为 **22 binary**（12 多文件 domain 子目录化 + 10 single-file 保留；**2026-10-10** 原 `shelf` binary 并入 `iam` binary；**2026-10-13** 原 `cnc_program_api` 与 `files_sts_tmp_keys` 两个单文件 binary 并成新 `files` 目录域 binary，同日新增 `mail_digest`；**2026-10-16** 新增 `scheduled_task`）—— 数字以 `cargo metadata --no-deps` 列出的 test target 为准逐个复核过（**2026-10-13** 复核、**2026-10-16** 增量）：

| 新结构 | 拆前 binary 数 | 拆后 binary 名（nextest filter） |
|---|---:|---|
| `tests/com/{main,union_list,delivery_note_in_use}.rs` | 5 | `com`（送货单域 2026-10-08 平移进 com 容器时目录随之更名 `delivery` → `com`；`group` / `attach_batches` / `scan` 三个 sub-file 已随端点收敛删除）|
| `tests/part/{main,create_serial_price,batch,crud,inspection_batches,lifecycle,list_enrichment,purchase_order_import,repair,rollup_derived,serial,to_inspection,to_process,to_ship}.rs` | 12 | `part`（sub-file 穷举，**14 个文件**，2026-10-13 逐 `ls` 核对 + 本次 `rollup_recompute.rs` 换成 `rollup_derived.rs` 后**仍是 14**；`main.rs` 为 binary 入口。★ `purchase_order_import.rs` 2026-10-06 新增：采购订单 Excel 导入两端点 —— `match-by-excel-items` 分档匹配 + `batch-update-order-info` 三态回填 / skip。★ `rollup_derived.rs` **2026-10-13 新增**（随 admin 域删除而来）：直调 `shared::batch::status::rollup_part_derived` 的 8 条**非 HTTP** 集成测试，守派生修正 + 幂等 + 序列号归档释放 + 级联父装配件 + 终态守卫上抛 `TerminalSkip`；它**取代**（而非补充）原 `rollup_recompute.rs`（8 条 HTTP 用例，走已下线的 `POST /api/v2/admin/recompute-rollup`）。⚠️ **两个 sub-file 已迁出、枚举里不再有**：`file.rs` → `tests/files/part_file.rs`（**2026-10-13**，随 part_file 域收编进 files 域，与 `tests/part/main.rs` 的拆分映射同批记）；`pickable_by_work_type.rs` → `tests/production/scan_listing.rs`（**2026-10-10**，报工台两条 list 端点迁往 `prod::scan` 时带走））|
| `tests/assembly/{main,api,lifecycle,by_part,children,create_serial_price,force_complete,status_sync}.rs` | 3 | `assembly`（**2026-10-11** 新增 `force_complete.rs`：装配件级强制完成端点 `POST /prod/assemblies/{id}/force-complete`，含「子件非取消批次为 0 条」的边角回归；⚠️ **2026-10-13 PR1b 两个 sub-file 已不在枚举里**：`files.rs` → **更名 `lifecycle.rs`**（其中 2 条文件上传用例迁往 `tests/files/upload.rs`，剩下 4 条生命周期用例留在本 binary —— 是更名不是搬家）；`files_list.rs`（D-09 `GET /assemblies/{id}/files` 的 4 条用例）→ **整文件删除**，端点已下线）|
| `tests/iam/{main,api,middleware,bootstrap_admin_seed,menu_seed,wx_bind,shelf,shelf_deactivate}.rs` | 2 | `iam`（**2026-10-09**：原 `redis-flush` 串行组已整体删除 —— Redis 隔离改由 key 前缀承担、触发该组的 `clean_redis`（FLUSHDB）已零调用方随函数删除、两个 binary 的并行度已恢复；删除理由留档在 `.config/nextest.toml` 的注释里。**2026-10-10**：`tests/shelf/` 整体并入本 binary，用例改名 `shelf.rs`（← `api.rs`，`api.rs` 同名冲突）/ `shelf_deactivate.rs`（← `deactivate.rs`），与货架子模块迁入 iam 域同批）|
| `tests/statistics/{main,api,event_driven}.rs` | 2 | `statistics` |
| `tests/production/{main,work_type,process,process_chain,worker,queue,queue_auto_allocate,queue_dispatch,queue_board,pending_programming,shelf_process,pickup,process_design,inspection,scan_badge,scan_listing,scan_worker_scan}.rs` | 6 | `production`（按 `src/modules/prod/*` 对齐；`shelf_process.rs` 2026-10-02 自当时的 `tests/shelf/api.rs` 迁入；**`process_design.rs` 2026-10-05 新增**，★ 核心回归是「装配件子件可见」；**`inspection.rs` 2026-10-05 新增** 13 场景，★ 核心回归是「扫子件 → 返回整棵装配件树」；**2026-10-08** `worker_pool.rs` → `queue.rs`、`worker_pool_auto_allocate.rs` → `queue_auto_allocate.rs`、`batch.rs` → `queue_dispatch.rs`，并新增 `queue_board.rs`（队列板聚合 9 场景）；**2026-10-10** 新增报工台三 sub-file：`scan_badge.rs`（`verify-badge`，★ 核心回归是出参恰好 4 个 key）/ `scan_listing.rs`（取件 / 持有两页列表）/ `scan_worker_scan.rs`（放回顺链推进 + 触发 refill 联动））|
| `tests/mail_digest/{main,config,preview,send,logs,batches,n_plus_one_guard}.rs` | — | `mail_digest`（**2026-10-13 新建**，与新域同批；**6 个 sub-file**：按端点切分的 4 个（`config.rs` 读/写配置与校验矩阵、`preview.rs` 渲染与统计口径、`send.rs` 三条失败路径 + 「失败也落流水」、`logs.rs` 分页与快照语义）+ **2026-10-16 第二轮新增 2 个**（`batches.rs` 工单行下的批次子行 + 位置四档 + 空 id 数组；`n_plus_one_guard.rs` **源码级**护栏：`fetch_rows` 循环体内禁 `repo::` / 顶层钉 3 个调用点）。★ 核心回归是「装配件只出一行」「`COMPLETED` 也算已交」「CR/LF 收件人被拒」「XSS 转义」「批次紧跟工单行」「holder 软删退化成 `—`」；⚠️ 本 binary 走 HTTP 登录，**需要 Redis**（同全仓其它 HTTP 契约测试））|
| `tests/scheduled_task/{main,accounts,delete}.rs` | — | `scheduled_task`（**2026-10-16 新建**，与新域同批；2 个 sub-file 按端点切分：`accounts.rs` 预设表 / 列表 / CRUD / 默认唯一性 / 路由纪律，`delete.rs` 软删 + 被引用禁删 + 删后同名可再建。★ 核心回归是「授权码加密入库 + 出参一个字都不回显」「`secret` 缺省沿用旧密文」「PUT/PATCH/DELETE 不被路由」；⚠️ 本 binary 需要 Redis，且**必须在进程里自设测试主密钥** —— `crypto::seal/open` 每次重读 env 而集成测试不走 `AppConfig::from_env`，见 `tests/scheduled_task/main.rs::ensure_mail_secret_env`）|
| `tests/outsource/{main,company,quote,quotable,send_receive,shipment,pool}.rs` | 3 | `outsource` |
| `tests/user_repo/{main,basic,role,password}.rs` | 1 → 3 sub-file | `user_repo` |
| `tests/files/{main,part_file,upload,cnc,sts}.rs` | 2 → 1 binary | `files`（**2026-10-13 新建**：由 2 个原单文件 binary `tests/cnc_program_api.rs`（→ `cnc.rs`）/ `tests/files_sts_tmp_keys.rs`（→ `sts.rs`）合并而来；⚠️ 另有 2 个 sub-file 自别的 binary 迁入 —— `tests/part/file.rs` → `part_file.rs`（它从来不是独立 binary，在 `part` 里只是 sub-file）与 `tests/assembly/files.rs` 的 2 条文件上传用例 → `upload.rs`（**2026-10-13 PR1b**：端点 `POST /assemblies/{id}/files` 下线后改测 `PartFileService::upload_file_for_owner(owner_kind=ASSEMBLY)`），两者均不计入本列的 binary 数；★ 核心回归是 `part_file.rs` 的硬切 404 守卫 —— PR1a 3 条（`old_part_files_prefix_returns_404` 等，含旧前缀整段 / 数字段 / 旧 CNC 前缀）+ **PR1b 4 条**（`old_part_upload_drawing_returns_404` / `old_part_upload_3d_model_returns_404` / `old_part_files_confirm_returns_404` / `old_assembly_files_returns_404`），共 7 条）|
| `tests/wx/{main,part_list,production}.rs` | 1 → 1 binary | `wx`（**2026-10-13 补记**：2026-10-11「BFF 按页面切子模块」时建的 binary，此前本表漏登记；本次一并补上并按 `cargo metadata --no-deps` 复核计数）|
| 单文件保留：_e2e_api / applicant_api / auto_complete_api / cos_opendal_api / cos_real_smoke / customer_api / dashboard_ws_api / idempotency_api / print_forward / wecom_login | 10 | （各自原 binary 名；⚠️ 原清单里的 `cnc_program_api` / `files_sts_tmp_keys` 已于 2026-10-13 并入上表 `files` binary）|
| **合计** | — | **22 binary**（12 子目录 + 10 单文件；逐目录 `ls` + `cargo metadata --no-deps` 实测，2026-10-13 复核、2026-10-16 增量；⚠️ `tests/com/delivery_note/` 只有 `mod.rs` 没有 `main.rs`，按本仓「cargo 不识别 `tests/<dir>/mod.rs`」的约定不构成 binary，不计入子目录数）|

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

⚠️ **新 fixture 的第一坑是取号**：每个测试文件都自己写一个 `SnowflakeIdGenerator::new(...)` 是 2026-10-09 之前本仓积累 200 处本地 generator 的根因（两个 fresh generator 各自从 `seq=0` 起步 ⇒ 同毫秒发出逐字节相同的 id ⇒ `23505`）。**不要**在 fixture 里现建 generator，走「测试取号：进程内唯一 generator」一节登记的两个入口之一。

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

### 范本文件

`tests/production/process_chain.rs` 是首个按 fixture 范本改写的 integration test binary，10 个场景的字面请求 / 断言**逐字保留**，仅替换本地 helper 为 test-support 引入 + 抽出 `bootstrap_as_manager` / `bootstrap_as_clerk` 两个样板函数。新 binary 改造时可参照此模式。

### 后续 27+ 文件改造顺序（按 cargo test nextest filter）

| 优先级 | 域 | 备注 |
|---|---|---|
| Phase F 已完成 | process_chain | 范本 |
| Phase G | part（11 sub-file）/ delivery（5 sub-file） | 涉及 process_chain / queue 引用最多 |
| Phase H | production 其余（work_type / process / worker / queue / queue_auto_allocate）/ assembly / shelf / statistics / outsource | queue fixture 复用度高 |
| Phase I | iam / user_repo / applicant / customer / dashboard_ws / _e2e / cnc_program / auto_complete / guard_dn_in_use / idempotency / cos_opendal / cos_real_smoke | 单 binary 不拆 |

### 测试取号：进程内唯一 generator（2026-10-09 已完成 + 新规约）

⚠️ 本节原名「待办登记：测试内联造 snowflake 生成器应收敛到 `pool_snowflake()`」，内容已随本轮改造作废并重写为**已完成**。若在别处（代码注释 / 测试文件）看到指向旧节名的引用，按本节为准。

**根因（一句话）**：位布局 `ts << 22 | instance << 12 | seq` 里 `last_ms` / `sequence` 是 **generator 对象私有**字段，而 `new()` 一律从 `last_ms=0, sequence=0` 起步 ⇒ 碰撞判据是「**两个不同 helper 各调一次**」（各自 fresh、都取 seq 0、都写同一张表、背靠背毫秒大概率同 ts），**不是**「同一个 helper 调两次」。所以「进程内唯一」的正确保证点是**对象共享**，不是 instance 编号（instance 只有 10 bit = 1024 槽，本就该留给**跨进程**区分）。

**实测口径（护栏探测器实测，非 `rg` 粗算；真实代码行，注释与字符串字面量不计）**：

| 文件 | 处数 | 身份 |
|---|---:|---|
| `test-support/src/pool.rs` | 1 | 集成测试 binary 的**唯一** ID 源（`TEST_SNOWFLAKE_GEN` 的 `get_or_init`） |
| `src/shared/test_snowflake.rs` | 1 | lib 单测 binary 的**唯一** ID 源（`SHARED_TEST_SNOWFLAKE` 的 `get_or_init`） |
| `src/main.rs` | 1 | 生产 instance 来源 |
| `src/infra/snowflake.rs` | 8 | **被测对象自身**的位布局 / sequence 回绕 / epoch / `MAX_INSTANCE` panic 边界单测 |
| **合计** | **11** | 4 个文件 |

`tests/**` 侧取号调用点：`pool_snowflake()`（兼容薄壳）**129** 处、`shared_test_snowflake()` **238** 处。

**新规约：进程内只从唯一源取号**（任何一类的测试代码都不得再 `SnowflakeIdGenerator::new` 另起一条 id 流）：

| 代码位置 | 入口 |
|---|---|
| `tests/**`（集成测试 binary） | `hsh_erp_test_support::shared_test_snowflake()`；存量 `pool_snowflake().lock().unwrap().next_id()` 是它的**兼容薄壳**（同一把锁 + 内层多包一层 `Arc`，靠 `Deref` 照旧可用），129 处调用点一行未改 |
| `src/**` 的 `#[cfg(test)]`（lib 单测 binary） | `crate::shared::test_snowflake::shared_test_snowflake()` |

**为什么是两个进程域各一个源（不是各仓共用一个）**：`test-support` 是 `[dev-dependencies]` 且 path-depends 回主 crate，构成 **dev-dependency 环**，编译 lib 单测目标时同一个二进制里链进**两份** `hsh_erp_rust`，两个 `SnowflakeIdGenerator` 是**两个不同的类型**，把 test-support 那个传给收本 crate 类型的形参即 `E0308`。两类 binary 又是**不同进程**（`cargo test` 里 lib 单测与每个 integration test binary 各起各的进程），跨进程撞号只可能通过 Redis 显形，而 Redis 已由 `RedisConfig::key_prefix` 按进程隔离，故「进程内各有一个唯一源」已经足够。
⚠️ **两个替代方案已被评估并否决，不要重走**：① 把共享 generator 移出 `#[cfg(test)]` 进正常 lib —— `test-support` 链的是**另一份** `hsh_erp_rust`，消不掉第二份；② 让 `test-support` 走 feature 而非 dev-dependency —— 环仍在（它 path-depends 主 crate 这条依赖关系不变）。
**根治方向（登记为独立重构）**：把 `test-support` 合回主 crate，让「两个 crate 实例」这个前提消失。

**已迁走的写法（历史陈述，不是本仓现状）**：
- 刻意填不同 instance 的「权宜之计」曾见于 `tests/part/lifecycle.rs`（`11`/`12`/`13`）、`tests/part/crud.rs`（`99`）、`tests/part/purchase_order_import.rs`（`777`）—— **现已全部迁走**。「换 instance 有效」这个知识本身仍成立（instance 不同 ⇒ 位段不同 ⇒ 必然不撞），但它不再是本仓任何一处的做法，且 instance 填字面量仍有 1/1024 概率与本进程派生值相等 ⇒ 不是可依赖的规约。
- 历史真实事故形态是两个**同为 instance=1** 的独立 generator（`tests/part/crud.rs` 与 `tests/part/rollup_recompute.rs` 各有一个域内单例，同属 `part` 一个 binary，`cargo test --test part` 下同进程多线程 ⇒ 完整复现）。

**旧登记表里「待复核」项已复核**：当时担心「有断言依赖 id 单调递增 / 落在某绝对区段」—— 逐个打开 34 个迁移文件复核，**零 id 算术、零 id 比较**：全仓唯一的 id 算术在 `tests/part/inspection_batches.rs` 的兜底键用例里，且它是**写死的字面量基座** `id_base` 加 0/1/2，与 generator 行为无关。迁移前后断言逐字未改。

### 待办登记：lib 单测基线红 `infra::cos_opendal::tests::memory_backend_copy_object`（2026-10-09）

**当前唯一已知基线红 = 这一条**（2026-10-10 实跑 `./scripts/test_nextest.sh` 复核：1357 run / 1356 passed / 1 failed / 10 skipped），原因与业务代码无关：opendal 的 Memory backend 已支持 copy（当年 spike 结论「Memory backend 不支持 copy（`Unsupported (permanent) at copy`）」已过期），而该单测断言的是「copy 必须返回 Unsupported 类错误」，于是 `result.is_err()` 落空。

**这条红不是任何一次改动引入的**（在 `master` 上同样红），修它要重写该测试的前提：Memory backend 上 copy 现在是 Ok，得改成断言「copy 成功且 `stat` 出来的 size / 内容与源一致」，或者把该断言降级为只在真 COS（S3 backend）下跑。

**登记原因**：review 阶段容易被误判成本轮 diff 带进来的回归。**勿挂在本轮 PR 上** —— 与 `prod::batch` 拆批 quantity 那次修复（`git diff master..HEAD` 只碰 5 个文件、`src/infra/` 与 `Cargo.lock` 均未触碰）无关，单独一个 commit 处理。

⚠️ **配套坑：`cargo test --lib` 的红绿不可直接判读——lib 单测的可靠闸门是 `scripts/test_nextest.sh --lib`**（2026-10-09 登记）：`.cargo/config.toml` 的 runner（`scripts/test_runner.sh`）对 `hsh_erp_rust-*`（lib/bin 单测 binary）走第 4 个转义口直接 `exec`，不起 PG 容器、不注入 `TEST_DATABASE_BASE_URL`，故 `prod/queue/service/dispatch.rs`(18) + `service/queue.rs`(7) + `shared/batch/guards.rs`(4) 共 **29** 条调 `test_pool()` 的 `#[tokio::test]` 在 `cargo test --lib` 下会以「`TEST_DATABASE_BASE_URL` 未设置」panic 额外报错（它们在 `test_nextest.sh --lib` 下是绿的），把这批 panic 当成本轮 diff 的回归就会误判。

口径提示：`cargo test --lib` 的失败总数是 **30**，比上面那 29 条多出的 1 条是上文已单列的 `infra::cos_opendal::tests::memory_backend_copy_object` 基线红（与 `TEST_DATABASE_BASE_URL` 无关）。实测 `cargo test --lib` = 386 passed / 30 failed。

### 待办登记：`cargo fmt --check` 基线红 3 个 `tests/part/*` 文件（2026-10-16）

**当前 `cargo fmt --check` 的红只有这 3 个文件**：`tests/part/create_serial_price.rs` / `tests/part/lifecycle.rs` / `tests/part/rollup_derived.rs`。**不是任何一次改动引入的**（`master` 上同样红；本分支 `git diff master..HEAD -- tests/part/` 为空已核实）。

**登记原因**：review 阶段容易被误判成本轮 diff 带进来的回归，**也容易被"顺手 `cargo fmt` 修掉"** —— 而后者会让一个与本轮无关的格式化改动混进 review diff（`cargo fmt` 是 workspace 级的，不带 `--` 指定文件就会连这三个文件一起改）。⚠️ 要单独开一个 commit 处理它。

⚠️ 配套：`cargo clippy --all-targets` 另有 1 条基线红（`src/modules/files/part_file/preview/service.rs` 的 `too many arguments`），与上面三条同属「与业务无关、别挂在本轮 PR 上」。

### 已清掉的存量基线红：6 条（2026-10-10）

reviewer 在 iam 域重构期间实跑复现出的另外 6 条基线红（与 iam 无关），根因已逐条核实并处置完毕。两条根因各自都**不是** fixture 污染，也都被收进了「测试数据两条硬约定」一节的机制约束：

| 3 条 `user_repo role::list_active_for_roles_*` | 根因：共享 template DB 已灌 `seeds/menu.sql` + `seeds/rbac.sql`，真实角色带着共享 seed 授予的那批菜单；测试用 `MANAGER` / `CLERK` 反查并断言**精确条数**，把 seed 菜单数了进去。处置：改用合成角色 `SYNTHETIC_MENU_ROLE`（断言逐字未削弱），另加 `menu_seed_never_grants_synthetic_menu_roles` 自锁「两份 seed 文本不得出现该串」。 |
| 3 条 `statistics api::{overview,workers_stats,worker_detail}_happy_path` | 根因：**时间炸弹** —— 插入行用 `now_naive()`，查询窗口却硬编码 `2026-09-01..2026-09-30`，`2026-09` 写、`2026-09` 跑是绿的，`2026-10-01` 起窗口里查不到行。处置：**删除**（用户决定接受该覆盖清零，不修复）。 |

### 覆盖空洞登记：`StatisticsService` 的三个方法无 happy-path 测试（2026-10-10）

⚠️ 写 statistics 相关代码前先知道这件事，否则会误以为该层已被集成测试守住：

- `StatisticsService::overview` / `worker_stats` / `worker_detail` **无 happy-path 集成测试**（原三条硬编码日期窗口的用例已按上表删除）。`tests/statistics/api.rs` 现存 **2** 条里，走到这三个方法的只有 `overview` 的**参数校验**用例（`date_from > date_to` → `BIZ 20104`）；另一条 `count_in_process_at_date_to_boundary` 是 repo 层 `count_in_process_at` 的直调覆盖，**不经 service**，故不计入这三个方法。**有效工时维度的 3 条 repo SQL 同样无集成测试覆盖。**
- `shared::analytics` 的两个 helper（`daily_buckets` / `worker_contribution`）仍**各保有 2 条纯函数单测**。
- repo 层 SQL 未被上述删除波及 —— 已知的覆盖点至少有：`tests/statistics/event_driven.rs` 的 3 条（自己钉死时间戳、一直绿）+ 上面那条 `count_in_process_at_date_to_boundary`（钉 `date_to` 边界口径）。**有效工时维度的 3 条**（按工人聚合 / 单工人总额 / 按工序拆分）是零覆盖。这只是**当前已知的**覆盖点、不该当穷举读：判断某个 repo 方法有没有覆盖，要打开 `tests/statistics/` 两个 sub-file 逐个核。
- **待补：`worker_effective_minutes` 集成测试**（`statistics::repo::sql::worker_effective_minutes`）。它是三条有效工时聚合里最容易被静默改坏的那条（总额与 `worker_process_breakdown` 各行之和必须相等，而两者**分处两条 SQL**，共用常量只保证了片段同源、不保证结果自洽）。补测试时：① 造一组「链外工序计 0」与「链内命中 step」混合的事件，断言总额等于 breakdown 各行之和；② 遵守「测试数据两条硬约定」第 2 条，**不得**硬编码绝对日期窗口。
- ⚠️ 这 3 条聚合**没有编译期列名校验**：走运行时 `sqlx::query()` + `format!` 拼装、不进 `query!` 宏，`.sqlx/` 里没有它们的条目（代价登记在 `docs/api/statistics.md` §4.10 与 `sql.rs` 的 `effective_minutes` 模块 doc）。补上面那条集成测试是唯一能兜住「列名写错 / `GROUP BY` 漏列」的自动化手段。
- 补 happy-path 时遵守「测试数据两条硬约定」第 2 条：**不得**硬编码绝对日期窗口。

## DB 约定（迁移与查询必须沿用）

- 无物理外键（`bigint` + 索引，存在性由 service 校验）、无 DB ENUM（`varchar` + Rust enum 校验）
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

- **存量 worktree 未迁移**（2026-09-30 决策）。`fix-batch-current-process-id` 等仍指向共享的 5430/6379/3000。它们跑 app 仍会污染主 checkout 的 `_sqlx_migrations`，`wt_services.sh doctor` 的第 3 节只作信息项列出、不报错。
- **孤儿 compose project**：`worker-pool-counts-endpoint` 的 worktree 目录已删，但它的 `postgres-test` 容器仍活着并占着 **5429** → `docker compose up -d postgres-test` 与测试「快速路」当前不可用。清理：`docker compose -p worker-pool-counts-endpoint down -v`。
- **清理测试容器只按容器名 / `com.docker.compose.project` label 过滤（2026-10-03 规约）**。正常路径不需要人工兜底扫描：`scripts/test_nextest.sh` 起的 session 容器是裸 `docker run`（无 compose project label、无命名卷，`/var/lib/postgresql` 挂 `--tmpfs`），脚本 trap 按捕获的 CID 精确 `docker rm -f`（nextest 失败时故意保留并打印 CID 供 debug）。只有上一轮进程被强杀、trap 没跑到时才需人工清理这批遗留 session 容器，**正向白名单**用「空 label」筛：`docker ps --format '{{.ID}}\t{{.Image}}\t{{.Names}}\t{{.Label "com.docker.compose.project"}}' | awk -F'\t' '$4 == ""'`，先核对清单里没有别人的容器，再逐个 `docker rm -f <CID>`。**禁止** `docker ps -q --filter ancestor=postgres:18-alpine` 之类按 image 祖先筛：主 checkout 的 `dev`(5430) 与各 worktree 的 `wt-<slug>-dev` 是同一个 image digest，祖先过滤对它们零区分力；Docker 29.x 下 `--filter "label!=com.docker.compose.project"` / `--filter '!label=…'` 也都是 invalid filter，只有上面白名单这条路线可用。2026-10-03 事故：用祖先过滤清 nextest 孤儿容器时误删了主 checkout `dev`(5430) 与另一 worktree `wt-fix-move-result-shelf-id-dev`(5456)；因未带 `-v` 卷与数据存活，带上 `-v` 就是开发库数据丢失。
- `docker-compose.yml` 写 `redis:7-alpine` 而在跑的 `redis-dev`/`redis-test` 实为 `redis:8-alpine`（仓库里无 `8-alpine` 字样）。因此 `wt_services.sh up` **刻意逐服务 up**、不用全量 `up -d`：全量会把 redis 重建回 7-alpine。这个分歧建议单独对齐一次。
- `target` **不再软链到主 checkout**（2026-10-02 改）。原先所有 worktree 的 `cargo` 产物落在同一个 `target/`，而测试二进制名**不含路径区分**（如 `part-de11122605098f0b`）：并发 agent 重新构建会把它的二进制写进同一目录，于是 `cargo nextest` 可能跑到**别人源码**编译出的产物，报出一批与本 diff 无关的假失败（实测见过 172 个 `50001 数据库错误`）。这是正确性问题不是性能问题，故没采用「约定 export `CARGO_TARGET_DIR`」——约定已经漏过一次。`setup-worktree.sh` 的 `OWN_BUILD_DIRS` 现在让每个 worktree 自建真实 `target/`，代价是每 worktree 占 ~6.5GB，**用完必须走 `teardown-worktree.sh`**（它会先删 `target/`，否则 `git worktree remove` 会因「含未跟踪文件」拒绝）。副作用：两个 worktree 现在**可以真正并发** `cargo build` / `cargo test`（不再有共享 target 锁）。`LISTEN_ADDR` 隔离的价值仍只是避开残留进程 / 前后脚启动的撞端口，不为并发跑两个后端服务。

## 环境要点

- 主 checkout 三个容器（docker compose 分服务启动）：`postgres-dev` 在 **5430**（库 `hsh`）、`redis-dev` 在 **6379**；测试库 `postgres-test` 在 **5429**（库 `postgres_rust_test`，⚠️ 当前被孤儿容器占着）、`redis-test` 在 **6380**
- 每个 worktree 另有自己的一套（PG 5431+ / Redis 6381+ / App 3001+），见「worktree 服务」
- 配置全部走 `.env`（`infra/config.rs`）：`DATABASE_URL` 优先，缺省回退 `POSTGRES_*` 拆分变量拼接；测试库 URL 由 `build_test_database_url()` 构建（`DATABASE_TEST_URL` / `POSTGRES_TEST_*`）。`dotenvy::from_filename(".env")` **不覆盖**已存在的环境变量，故临时覆盖用 `DATABASE_URL=... cargo run` 即可，无需改文件
- 优雅退出：`AppState.shutdown`（CancellationToken）同时通知 axum serve 与 `task/auto_complete` 后台循环
