# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

> 📌 **前端对接**：后端 API 参考见 [`docs/api/`](docs/api/index.md)（[index.md](docs/api/index.md) 为总入口，含通用约定 + 跨域错误码速查；按模块拆分为 `iam.md` / `applicants.md` / `customers.md` / `shelves.md` / `websocket.md` / `delivery-groups.md` / `cnc-programs.md` / `files.md` / `outsource-companies.md` / `outsource-quotes.md` / `outsource-shipments.md` / `outsource-sendable.md` / `outsource-pool.md` / `_e2e.md` / `dashboard.md`（2026-10-06 新增，HTTP 首取 + WS 大屏），`parts` 因端点 ≥49 已拆为 `docs/api/parts/` 子目录（`index.md` / `crud.md` / `lifecycle.md` / `inspection.md` / `batch.md` / `print.md`），`assemblies` 拆为 `docs/api/assemblies/`，`production` 拆为 `docs/api/production/`（`index.md` + 子页：work-types / processes / work-type-process-mapping / **shelf-process-mapping（2026-10-02 新增，货架↔工序映射自 shelves 域搬入）** / process-chain / worker-pool / workers / batches / pending-programming / **process-design（2026-10-05 新增，制定工序页零件列表）** / **inspection（2026-10-05 新增，扫码查询：装配件→子件→批次三层树）**），`delivery-notes` 拆为 `docs/api/delivery-notes/`（4 子页）；2026-09-19 IAM 域合并：原 `auth.md` + `users.md` → `iam.md`；2026-09-19 prod 容器聚合：原 `workers.md` → `production/workers.md`，工种/工序/工艺链/工人池/工人 5 支撑域聚合为 `src/modules/prod/*`，URL 硬切换 `/api/v2/prod/*`）；2026-09-28/29 wx 域：原 7 个小程序 BFF 聚合端点 + 新增 `POST /api/v2/wx/iam/wx-login`（企业微信小程序登录）→ `docs/api/wx.md`，配套 iam 域 3 个 `/iam/users/{id}/wx-bind` 端点见 `docs/api/iam.md`。**后端代码变更（新增 / 修改 / 删除端点，或修改 DTO 字段 / 错误码）必须立即同步更新对应模块文件**。

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
cargo test --test <name>

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
- `seeds/menu.sql` —— 菜单树声明式种子（t_menu + t_role_menu），幂等可反复跑
- `seeds/README.md` —— seed 编写规范

**新 schema 变更走追加新 migration**（append-only，永不修改已有文件）。
**菜单变更 = 改 `seeds/menu.sql` + 重启 app**（启动钩子自动跑，无需新 migration）。

### 初始管理员账号（2026-09-26 新增）

`seeds/admin.sql` 是**可选**初始管理员 seed（`username=admin / password=changeme / role=MANAGER`），由环境变量 `BOOTSTRAP_ADMIN_ENABLED` 门控（默认 `false`）。在 `src/main.rs` 启动钩子、`src/infra/seed.rs::run_seeds(pool, bootstrap_admin_enabled)` 处执行。开启流程：env=`true` → cargo run / docker compose up → 用 admin/changeme 登录 `/api/v2/iam/login` → 改密 → env=`false` → 重启。生产默认关，避免无意中创建初始账号。明文密码与 `src/modules/iam/service/account.rs::DEFAULT_RESET_PASSWORD` 同源，bcrypt 哈希复用 `test-support/fixtures/iam.sql` 同款字面值。

## 从备份恢复（scripts/restore_from_backup.sh，2026-10-01 重写）

**铁律：restore 绝不修改 `public` schema。** dump 是 Python 端老 schema 的快照，与 `migrations/` 有列漂移（027 删了 `t_part` 的 6 个批次依附列 / 028 删了 `t_part_batch` 的 2 列 / 004 改了 `next_process_id` 语义）。2026-09-25 的老脚本靠「先 `ADD COLUMN` 把老列补回来」让 `pg_restore` 不报错 —— 于是那 10 个已废弃的列**永久留在 schema 里**，就是「重建后字段又恢复了」的根因。

现在的流程：

1. **`REBUILD_SCHEMA=1`（默认）** —— `DROP SCHEMA public CASCADE` + `sqlx migrate run`（需 sqlx-cli），目标 schema 严格等于 `migrations/` HEAD，顺带补上从未应用的 004。
2. **漂移自动检测** —— 从 `pg_restore --data-only` 的 `COPY` 表头拿 dump 列清单，与 `information_schema` 比对，输出 `直灌 / stage 投影 / 目标新列置 NULL` 决策表（`DRY_RUN=1` 只打印）。
3. **stage 投影** —— 漂移表先进 `restore_stage`（用 dump 自己的 DDL 原样建表），再 `INSERT INTO public.<t> (交集列) SELECT ... FROM restore_stage.<t>`，逐张对账行数。
4. **schema 快照 diff** —— 灌数据前后各存一份 `information_schema`，diff 非空即失败（永久堵死「列被复活」这类回归）。
5. **`_sqlx_migrations` / `alembic_version` 从 TOC 排除** —— dump 里带的是 Python 端 15 条旧迁移记录，灌进去会让 sqlx 报 `VersionMissing`，之后 `cargo run` 起不来。
6. `t_menu` / `t_role_menu` 默认跳过（`seeds/menu.sql` 是权威源），最后幂等补灌。

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
| `statemachine.rs` | 仅 part / assembly / delivery_note / outsource / process_chain 五域 | `statemachines/` |

**part 是跨域枢纽**（delivery_note、assembly、outsource、part_file、statistics、shelf 均依赖它），实施顺序见 architecture.md 第 7 节。

### `src/modules/prod/*` 子模块清单（prod 域 = 生产调度容器，2026-09-19 聚合）

| 子模块 | 端点数 | URL 前缀 | 表 | 说明 |
|---|---:|---|---|---|
| `prod::worker` | 7 | `/api/v2/prod/workers` | `t_worker` | 工人档案主数据 |
| `prod::work_type` | 7 | `/api/v2/prod/work-types` | `t_work_type` + `t_work_type_process` | 工种 CRUD 5 + 工种↔工序映射 2 |
| `prod::process` | 5 | `/api/v2/prod/processes` | `t_process` | 工序主数据（INHOUSE/OUTSOURCE）；**2026-10-02 订正**：文档原写 6，逐 router 复核为 5（`/` 的 GET+POST 算 2 条 route） |
| `prod::process_chain` | 3 | `/api/v2/prod/process-chains` | `t_part_process_chain` + `t_process_chain_step` | 工单工艺链 |
| `prod::shelf_process` | 3 | `/api/v2/prod/shelf-processes` | `t_shelf_process` | **2026-10-02 新增**：货架 ↔ 工序映射，自 `src/modules/shelf/process_mapping/` 搬入（`t_shelf_process` 关联的是 prod 域实体 `t_process`）。旧路径 `GET|POST /api/v2/shelves/{id}/processes` + `GET /api/v2/shelves/processes` 已删除（**无 alias**），这 3 个端点请求 / 响应契约逐字不变；但同 commit 删的 `account_count` 出参会打爆前端 Zod 必填字段，**前端配套改动清单见 `docs/api/production/shelf-process-mapping.md#前端配套改动清单`** |
| `prod::worker_pool` | 6 | `/api/v2/prod/pool` | `t_part_batch`（候选池视图） | 工人候选池（2026-09-30 `/worker-pool` + `/admin/worker-pool` 双 nest 合并为 `/pool`；**2026-10-02 订正**：文档原写 5，实际 6 条 route） |
| `prod::batch` | 28 | `/api/v2/prod/batches` | `t_part_batch` | 批次域全集（2026-09-29 建 3 条下发端点；**2026-10-02** 硬切进 25 条批次路由，操作对象是批次、无 alias，路由表见 `src/modules/prod/batch/mod.rs` 模块 doc） |
| `prod::programming` | 1 | `/api/v2/prod/programming` | `t_part` + `t_part_batch` + chain | 待编程一览（2026-10-01 新增） |
| `prod::process_design` | 1 | `/api/v2/prod/process-design` | `t_part` | **2026-10-05 新增**：制定工序页零件列表。软删闸门 + `status = 'PENDING'` 闸门，7 字段最小集，**刻意不加** `AND assembly_id IS NULL` 守卫（part 域 `GET /parts` 带 `part_only: true` 会把装配件子件全部排除）故**含装配件子件**；入参只有 `sort_dir` / `limit` / `offset`。前端「制定工序」页自 part 域 `GET /api/v2/parts?status=PENDING` 切来，part 域旧端点保留兼容、一行未改 |
| `prod::inspection` | 1 | `/api/v2/prod/inspection` | `t_assembly` + `t_part` + `t_part_batch`（另 `LEFT JOIN` `t_customer` / `t_process` / `t_shelf` / `t_worker` / `t_outsource_company` 五表：仅 `t_customer` 有软删闸门（客户名退化为 `null`），`t_process` / `t_shelf` / `t_worker` / `t_outsource_company` 四张展示用附表刻意不加） | **2026-10-05 新增**：扫码查询（`GET /scan/{serial_no}`），返回「装配件（可空）→ 全部子件 → 全部批次」三层树。命中口径先查 `t_part.serial_no`、未命中回退 `t_assembly.serial_no`，都未命中返 `20101` / HTTP 404，`serial_no` trim 后为空同样按未命中；软删闸门覆盖 part / assembly / batch 三表；★ **读全部批次不按状态过滤**（含终态，状态闸门在前端）；★ `process_name` 走 `current_process_id` 权威列（migration 004），故 `INSPECTION` / `DELIVERED` 批次**恒 `null`**（出池清该列不变式的正确结果）；`is_scanned` 是唯一内存派生字段（`t_part_batch` 无序列号列）；批次层一条 SQL（`part_id = ANY($1)`）取回整棵树、无 N+1；角色 Manager + Inspector。★ 响应规模 = 子件数 × 每件批次数，**无上限、无分页**（本端点不接受任何 query 参数）。前端待品检页扫码路径 ⏳ **建议**自 part 域 `GET /parts/by-serial/{serial_no}`（+ `/part-batches`）切来（**尚未在前端仓合入**），part 域旧端点保留兼容、一行未改；完整契约见 `docs/api/production/inspection.md` |

> 表内 10 个子模块端点求和 = 7 + 7 + 5 + 3 + 3 + 6 + 28 + 1 + 1 + 1 = **62**（2026-10-05 增 `prod::process_design` 与 `prod::inspection` 各 1 端点）。

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
  - prod 侧：文档原写「31」，系 `prod::worker_pool` 计数残留（标题写 5、router
    实为 6 route），实为 **32**；
  - 故 prod 的净变化是 32 → 35（+3），而非按旧账记的 31 → 34。
  - ⚠️ 上文「prod 32 → 35（+3）」是 2026-10-02 shelf 拆分当时的记账（当时 batch 记
    3 端点、shelf_process +3），与本节上表已不同源；端点求和真值见上表下方那行。
- `t_shelf_process` 的 SQL 真源**只在** `prod::shelf_process::repo::ShelfProcessRepo`
  （6 个静态方法 = 平移 4 + 从 `prod::batch` / `prod::worker_pool` 各收 1 处）。
  故意保留 inline 的 3 处见 `docs/api/production/shelf-process-mapping.md#维护约定`
- `MockShelfRepoTrait` 全仓零引用，trait 收缩不影响任何单测
- 文档：`docs/api/shelves.md` / `docs/api/production/shelf-process-mapping.md` /
  `docs/api/production/index.md` / `docs/api/index.md` 已同步

### 状态派生契约（2026-10-01）

三层状态**单向**派生（子件 = `t_part` 中 `assembly_id` 非空的行）：

```
t_part_batch.status            ← 唯一真源
   │  status_gate::rollup_part_derived（min-progress）
   ▼
t_part.status / next_process_id   ← 派生缓存
   │  assembly::compute_assembly_target
   ▼
t_assembly.status               ← 派生缓存
```

- **写 `t_part_batch.status` 只能走 `src/modules/prod/batch/status_gate.rs`**
  （`apply_batch_status_change` / `apply_bulk_batch_status_change_for_part`）。
  它一函数内完成「写批次（OCC + SQL 层源状态白名单）→ 回填 part 派生列 →
  级联 assembly → 终态序列号归档 / 释放」，所以 **caller 没有「要不要顺手调
  sync」这个选项**（13 个历史 `mark_batch_*` 写点已改写为其上的薄包装）。
  ⚠️ 手工补调 `PartService::sync_from_batch_change` 是**反模式**：第二次派生必为
  `NoChange`，会把响应的 `synced_assembly_id` 吞成 `null`、连带 WS 的
  `ASSEMBLY_UPDATED` 永不发。
- **CI 强制**：`cargo test --lib` 的
  `prod::batch::status_gate::write_guard_tests::no_outside_file_writes_batch_status`
  扫全 `src/**/*.rs`，除 `status_gate.rs` 外任何文件写
  `UPDATE t_part_batch SET status …` 即失败（注释 / `#[cfg(test)]` 块 / 只改其它列
  的 UPDATE 不在判定范围）。改动 `mark_batch_*` 后请顺手跑一次。
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
  - 已知代价：**已终态**的 part 不再被 rollup / admin 对账改写（那需要一次人工决策）。
- **`StatusChange` 的三态约定**：`Option` 的 `None` 一律是「保持原值」，「清 NULL」
  由同名 `clear_location` / `clear_holder_id` / `clear_process_id` /
  `clear_process_step_id` 显式表达。出池（召回 / 送检 / 外协收回 / 返修出池）
  必须同时清 `location` + `current_holder_id` + `current_process_id` +
  `current_process_step_id`，否则 UI 会显示「待投产的工单还压在生产架上」。
- **终态序列号归档事件的 id 必须是真实雪花**（`StatusChange::event_id`，由 caller 从
  `SnowflakeIdGenerator` 透传）：`GET /parts/{id}/events` 是 `ORDER BY id DESC`，
  拿 `part_id` 之类的建单期 id 顶替会把归档事件排到时间线最底部，且二次进终态时
  pkey 冲突 → 事务 500。
- **兜底对账**：`POST /api/v2/admin/recompute-rollup`（Manager）复用上述 rollup
  函数重跑并回报 before→after，**幂等**；新增派生算法时**不要**在对账端点重写一遍。
  全量对账靠**分表游标**续扫（2026-10-01 review 第 2 轮 MAJOR-2：`t_part` 与
  `t_assembly` 的 id 来自同一个雪花流、按时间序交错，共用一个游标会永久跳过
  `(assembly_max, part_max]` 那段装配件）——把响应里非 null 的
  `next_part_after_id` / `next_assembly_after_id` 回传为请求的 `part_after_id` /
  `assembly_after_id`，直到 `truncated=false`。
  ⚠️ 报告里 `parts_skipped_terminal > 0` 表示「该 part 已终态、派生被守卫跳过」，
  **不是**「数据已一致」；这类行只能靠 force-complete / cancel 或业务流修
  （见 `docs/api/admin.md`）。
- 词汇：batch/part 8 态 + `is_repairing` 标记（`REPAIRING` 已于 2026-10-01 降级为
  boolean 列，DB 不再产生该 status）；assembly 7 态（无 `OUTSOURCE`，子件
  `OUTSOURCE` ⇒ 父 `IN_PROCESS`）。详见 [`docs/api/parts/index.md#状态派生契约2026-10-01`](docs/api/parts/index.md)。

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

## 集成测试目录结构（2026-09-23 PR13 重构后）

51 个 integration test binary 已重组为 **20 binary**（9 多文件 domain 子目录化 + 11 single-file 保留 + 1 域内拆 3）：

| 新结构 | 拆前 binary 数 | 拆后 binary 名（nextest filter） |
|---|---:|---|
| `tests/delivery/{main,group,attach_batches,scan,note}.rs` | 5 | `delivery` |
| `tests/part/{main,create_serial_price,batch,crud,file,inspection_batches,lifecycle,list_enrichment,pickable_by_work_type,purchase_order_import,repair,rollup_recompute,serial,to_inspection,to_process,to_ship}.rs` | 12 | `part`（sub-file 穷举，`main.rs` 为 binary 入口。★ `purchase_order_import.rs` 2026-10-06 新增：采购订单 Excel 导入两端点 —— `match-by-excel-items` 分档匹配 + `batch-update-order-info` 三态回填 / skip）|
| `tests/assembly/{main,api,files,status_sync}.rs` | 3 | `assembly` |
| `tests/iam/{main,api,middleware}.rs` | 2 | `iam`（redis-flush group）|
| `tests/shelf/{main,api,deactivate}.rs` | 2 | `shelf` |
| `tests/statistics/{main,api,event_driven}.rs` | 2 | `statistics` |
| `tests/production/{main,work_type,process,process_chain,worker,worker_pool,worker_pool_auto_allocate,batch,pending_programming,shelf_process,pickup,process_design,inspection}.rs` | 6 | `production`（按 `src/modules/prod/*` 对齐；`shelf_process.rs` 2026-10-02 自 `tests/shelf/api.rs` 迁入；**`process_design.rs` 2026-10-05 新增**，★ 核心回归是「装配件子件可见」；**`inspection.rs` 2026-10-05 新增** 13 场景，★ 核心回归是「扫子件 → 返回整棵装配件树」）|
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
