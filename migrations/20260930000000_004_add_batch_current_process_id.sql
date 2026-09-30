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
--   `current_process_step_id` 降级为**可选的进度指针**：仅当工单有工序链时
--   才写，允许 NULL。目标：让**没有工序链的工单，其批次也能正常入池**。
--
-- 写入不变式（src 侧实现见各调用点注释）：
--   进池（status='IN_PROCESS' + location='PRODUCTION_SHELF'）→ 写目标 process_id
--   出池（转 PENDING / INSPECTION / INSPECTION_SHELF）       → 置 NULL
--   池内移动（worker→货架归还、move 端点）                   → 不动（工序不变）
--   非生产流（初始批次、子批次）                             → NULL
--   拆分批次 `_split_batch_inner`                           → 从源行同 SELECT 列表继承
--
-- 数据回填顺序（同一事务内完成）：
--   1. 加新列 current_process_id bigint（可空、无默认值）
--   2. 回填：从已有的 current_process_step_id 反查 process_id
--      （s.id = pb.current_process_step_id AND s.deleted_at IS NULL）
--   3. 建部分索引 ix_t_part_batch_current_process_id（WHERE 列非空）
--
-- 已知局限（本次不处理，2026-09-30 记录）：
--   回填**覆盖不到历史死数据** —— 已下发但 `current_process_step_id IS NULL`
--   的批次无法反推 process_id：`t_part_event` 的 `PLACED_ON_SHELF` 事件里没有
--   target_process_id 字段可回捞。这类批次仍是「status=IN_PROCESS +
--   location=PRODUCTION_SHELF 但池归属为空」的死状态，只能由运营手工 recall
--   （recall-to-pending 会把列置 NULL）后重新下发才能恢复。生产库脏数据量与
--   是否需要一次性修复脚本，待后续按实际数据量另行决策。
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
