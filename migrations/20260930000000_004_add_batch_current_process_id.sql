-- ============================================================================
-- Migration 004: t_part_batch.current_process_id —— 工序池归属的权威依据
-- ============================================================================
-- 2026-09-30 新增
--
-- 背景（2026-09-30 用户报告的 bug）：
--   「在生产队列菜单中拖动批次下发到特定工序后，批次下发后对应的工序池中却没有
--   显示当前工序池中的批次，点击 tab 进入页面时也没有看到向后端发起请求拉取该
--   工序池中的所有批次。」
--   定位结论：前端失效与重拉都正常（`GET /prod/pool/{process_id}` 返回 200），
--   **响应里本来就没有该批次**。根因链：
--     1. `BatchRepo::update_batch_dispatched` 硬编码写
--        `current_process_step_id = NULL`（dispatch 路径不解析 step）；
--     2. 三条候选池 SQL（`list_candidates_by_process_all_shelves` /
--        `group_count_by_process_all_shelves` / `take_one_from_pool`）与
--        `count_pool_by_shelf_and_process` 全部 **INNER JOIN
--        t_process_chain_step ON s.id = pb.current_process_step_id`；
--     3. `s.id = NULL` 匹配不到任何行 → 批次对所有池查询隐身。
--   更糟的是这是个**死状态**：唯一会推进 step 的路径是 worker-scan RETURNED
--   （`part/service/worker_scan.rs`），但它要求批次先进得了池 —— 鸡生蛋死锁。
--
-- 决策（2026-09-30 用户拍板）：
--   新增 `current_process_id bigint`（逻辑 FK → `t_process.id`），并把它确立为
--   **判断批次是否属于某工序池的唯一权威依据**。原
--   `current_process_step_id` 降级为**可选的显示用定位信息**：仅当工单有工序链
--   时才写，允许 NULL；且**只在首次定位工序时写、之后不再推进**（不是「当前走到
--   第几步」的进度指针，见下方已知局限 (3)）。目标：让**没有工序链的工单，其批次也能正常入池**。
--
-- 写入不变式（src 侧实现见各调用点注释）：
--   进池（status='IN_PROCESS' + location='PRODUCTION_SHELF'）→ 写目标 process_id
--   出池（转 PENDING / INSPECTION / INSPECTION_SHELF）       → 置 NULL
--   工序推进（worker-scan RETURNED：工人在 P1 完工、传 next_process_id=P2）→ 写 P2
--   池内移动（move 端点 POOL↔WORKER / WORKER↔WORKER、
--             pick_up 的 IN_PROCESS+PRODUCTION_SHELF 分支、
--             take_one/take_specific 派发）                  → 不动（工序不变）
--   非生产流（初始批次、子批次）                             → NULL
--   拆分批次 `_split_batch_inner`                           → 从源行同 SELECT 列表继承
--
-- ⚠️ 2026-09-30 review 第 3 轮 M1 订正：第 3 行原为「池内移动（**worker→货架
--    归还**、move 端点）→ 不动」。worker-scan RETURNED **也是**「worker→货架归还」，
--    但它的语义是**推进工序**（写 next_process_id），不是池内移动 —— 原措辞与本
--    文件下方「已知局限 (3)」自相矛盾。现已拆成「工序推进」与「池内移动」两行。
--    **判据表以本块为准**，后续做写点穷举时 RETURNED 归「工序推进」行。
--
-- ⚠️ 读取方分工（2026-09-30 review 第 3 轮 M3 确立，勿越界）：
--    `current_process_id` 的读取方严格限定为 **5 条工序池 SQL**
--    （take_one_from_pool / take_specific_from_pool /
--      list_candidates_by_process_all_shelves / group_count_by_process_all_shelves /
--      count_pool_by_shelf_and_process）+ `list_pickable_by_work_type`
--    + rollup 派生 `t_part.next_process_id`。池查询全部硬限定
--    `status='IN_PROCESS' AND location='PRODUCTION_SHELF'`。
--    **展示类列表**（inspection-batches / repair-batches / part 批次明细 /
--    dashboard）继续从 `current_process_step_id` → step JOIN 派生工序名 ——
--    INSPECTION 批次按出池不变式本列恒 NULL，直读会让那些端点的 `next_process_id`
--    恒 null。
--
-- 例外（2026-09-30 review 第 1 轮 M1 记录，勿按表机械核对后误判为 bug）：
--   `send_to_outsource`（status='OUTSOURCE' + location='OUTSOURCE_COMPANY'）写
--   `Some(req.process_id)` 而非 NULL —— 外协加工的就是这道工序，rollup 派生
--   `t_part.next_process_id` 需要它；且与本迁移**前**的行为一致（旧代码写
--   `Some(step_id)`，rollup 再翻成 process_id），不写才是行为变更。它不可能被
--   任何工序池查询命中（池 SQL 硬限定 IN_PROCESS + PRODUCTION_SHELF）。
--
-- 数据回填顺序（同一事务内完成）：
--   1. 加新列 current_process_id bigint（可空、无默认值）
--   2. 回填：从已有的 current_process_step_id 反查 process_id
--      （s.id = pb.current_process_step_id AND s.deleted_at IS NULL）
--   3. 建部分索引 ix_t_part_batch_current_process_id（WHERE 列非空）
--
-- 执行成本（2026-09-30 review 第 1 轮 L5 补充）：
--   ADD COLUMN **不带 DEFAULT** → PG 11+ 只改系统目录、不重写表、不取
--   ACCESS EXCLUSIVE 长锁，代价可忽略。真正的成本在第 2 步的 UPDATE：它全表
--   扫 t_part_batch，并对命中的行加行锁（命中行数 = 「有 step_id 且 step 未软删」
--   的批次，通常远小于全表）。生产库若 t_part_batch 达百万级，建议低峰期执行。
--   仓内无分批迁移先例（archive/ 的 29 个迁移都是单文件全量），故本次同样单事务。
--
-- 已知局限（本次不处理，2026-09-30 记录）：
--   (1) 回填**覆盖不到历史死数据** —— 已下发但 `current_process_step_id IS NULL`
--       的批次无法反推 process_id：`t_part_event` 的 `PLACED_ON_SHELF` 事件里没有
--       target_process_id 字段可回捞。这类批次仍是「status=IN_PROCESS +
--       location=PRODUCTION_SHELF 但池归属为空」的死状态，只能由运营手工 recall
--       （recall-to-pending 会把列置 NULL）后重新下发才能恢复。生产库脏数据量与
--       是否需要一次性修复脚本，待后续按实际数据量另行决策。
--
--   (2) 存在一条**持续生产新死数据**的路径（review L3 提出，2026-09-30 review
--       第 3 轮 M2 收窄）：
--         work_type.rs::pick_up 的 PENDING 分支 → prod/worker_pool move_batch
--         的 WORKER→POOL 分支
--       `pick_up` 的 PENDING 分支把批次从 PENDING 直送工人（status=IN_PROCESS +
--       location='WORKER'），此时 PENDING 批次的 `current_process_id` 恒为 NULL
--       （recall-to-pending 已按出池不变式置 NULL），该分支只换 holder、不写工序。
--       之后若走 **`prod/pool move` 的 WORKER→POOL 归还**（`advance_to_process_id`
--       恒传 `None`，属「池内移动 → 不动」不变式），批次就落回
--       IN_PROCESS + PRODUCTION_SHELF + 池归属为空的形态 —— 即本迁移要消灭的
--       同一形态，且是**活水不是存量**。
--
--       ⚠️ **收窄说明（M2）**：本条原先写「worker-scan RETURNED / prod/pool move
--       归还货架时也不会补写」。前半句**已不成立** —— H1 修复（review 第 1 轮）让
--       `mark_batch_returned` 在 RETURNED 时写 `COALESCE($5, current_process_id)`，
--       worker_scan 恒传 `Some(next_pid)`，故 RETURNED 路径**已自愈**：
--       同一批 pick_up 出来的批次只要走 worker-scan RETURNED 归还，就会被正确
--       写入目标工序。残留的活水路径**只剩 `prod/pool move` 一条**。
--
--       修法需产品决策：给 `move_batch` 的 WORKER→POOL 分支加「池归属为空的批次
--       守卫 / 告警」，或要求 `pick_up` 的 PENDING 分支由调用方传
--       `next_process_id` 补归属。
--
--       **非本迁移引入**（旧设计下 `current_process_step_id` 同样为 NULL，行为一致），
--       但它是**活水不是存量**。
--
--   (3) `current_process_step_id`（可选的**显示用**定位信息）在 worker-scan
--       RETURNED 时**不推进**（review H1 附带决策，本轮刻意不扩 scope）：RETURNED
--       已解析出目标 step_id，但 `mark_batch_returned` 的 step SET 子句在
--       2026-09-30 prod/pool move 重构中被移除，RETURNED 复用该函数后该显示用
--       列在 RETURNED 时也不再更新。
--       ⚠️ **措辞订正（2026-09-30 review 第 3 轮附带发现）**：本列**不是会随流转
--       推进的「进度指针」**，它只在**首次定位**工序时被写入（dispatch 路径刻意写
--       NULL；其余由 place_on_shelf / release_from_programming / send_to_outsource /
--       receive_from_outsource / complete_repair / to_process 写），**之后一律不再
--       推进**。对多工序链工单它永远停在首次定位的那一步，故**不可**当「当前走到
--       第几步」用；它的实际用途是「INSPECTION 期间显示批次首次定位在哪一步」。
--
--   (4) **已接受债务**（2026-09-30 review 第 3 轮 M4 记录，本次不修）：以下 4 个
--       出池写点**既不写也不清** `current_process_id`，会让批次带着上一道工序的 id
--       停在非 IN_PROCESS 状态，与本列 COMMENT「NULL 表示批次不在生产工序池中」
--       矛盾。review 已确认**功能上无影响**（5 条池 SQL 全部 `status='IN_PROCESS'
--       AND location='PRODUCTION_SHELF'` 双重限定；rollup 过滤 CANCELLED），唯一
--       可观察后果是 force-complete / 全部取消后 `t_part.next_process_id` 残留 →
--       保守地挡住删工序（20803）。
--         (4a) `phase1/batch_ops.rs::cancel_batch`
--              （经 `mark_batch_status_only`，起点可含 IN_PROCESS+PRODUCTION_SHELF）
--         (4b) `part/repo/sql/batch_sql.rs::cancel_all_active_batches_for_part`
--              （**无 status 白名单**，会命中在池批次）
--         (4c) `part/repo/sql/batch_sql.rs::force_complete_all_batches_for_part`
--              （仅排除 CANCELLED，会命中在池批次）
--         (4d) `part/repo/sql/batch_sql.rs::mark_batch_repairing`
--              （IN_PROCESS → REPAIRING，location 仍 PRODUCTION_SHELF）
--       修 (4a)~(4c) 只需在对应 SQL 加 `current_process_id = NULL`。
--       **(4d) 需产品拍板**：REPAIRING 是否仍算「在 P 加工中」？若算，则本列
--       保留非 NULL 是正确的，应把 REPAIRING 显式列为不变式的例外写进本文件
--       头部；若无归属则同 (4a)~(4c) 处理。此项**未定**。
--
-- 幂等：ADD COLUMN / CREATE INDEX 均带 IF NOT EXISTS；UPDATE 为普通回填
-- （重复执行结果幂等：已回填行再次 UPDATE 得到同值）。列刻意保持**可空、无
-- 默认值** —— 有 2 个写入点确实没有工序（且约 25 个集成测试文件 INSERT 时不
-- 带该列），加 NOT NULL / DEFAULT 会让既有 INSERT 语句全部失效。
-- ============================================================================

ALTER TABLE public.t_part_batch
    ADD COLUMN IF NOT EXISTS current_process_id bigint;

COMMENT ON COLUMN public.t_part_batch.current_process_id IS
    '逻辑 FK → t_process.id；batch 当前归属的工序，是工序候选池归属的权威依据（GET /prod/pool/{process_id}、/prod/pool/counts、take_one_from_pool 均按本列过滤）。NULL 表示批次不在生产工序池中（PENDING / INSPECTION / OFFICE 等）。2026-09-30 新增。';

-- 回填：从已有的 current_process_step_id 反查 process_id（同一事务内完成）。
-- JOIN 方向与 archive/20260916130000_028_batch_step_ify.sql 相反：
-- 028 是「batch.next_process_id = step.process_id」正向匹配，本次是
-- 「step.id = batch.current_process_step_id」反查。
UPDATE public.t_part_batch pb
SET current_process_id = s.process_id
FROM public.t_process_chain_step s
WHERE s.id = pb.current_process_step_id
  AND s.deleted_at IS NULL
  AND pb.current_process_step_id IS NOT NULL
  AND pb.deleted_at IS NULL;

CREATE INDEX IF NOT EXISTS ix_t_part_batch_current_process_id
    ON public.t_part_batch(current_process_id)
    WHERE current_process_id IS NOT NULL;

COMMENT ON INDEX public.ix_t_part_batch_current_process_id IS
    '工序池候选查询（GET /prod/pool/{process_id}、/prod/pool/counts、take_one_from_pool、count_pool_by_shelf_and_process）按 current_process_id 过滤，本索引使其免于全表扫描。2026-09-30 新增。';
