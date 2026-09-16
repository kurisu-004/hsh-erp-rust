-- Migration 028: 批次 step 化 —— t_part_batch.next_process_id → current_process_step_id
-- 2026-09-16 PR-3 batch-step-ify
--
-- 背景（2026-09-16 用户决策）：
--   t_part_batch 中的 next_process_id 改为 current_process_step_id 指向所属
--   part 绑定的 process_chain 中的步骤（t_process_chain_step.id）；placed_at
--   移除（不再统计生产时间）。
--
-- 数据回填顺序：
--   1. 加新列 current_process_step_id bigint（先加 → 回填 → 才能 DROP 旧列）
--   2. 数据回填：
--      对 part 有链且 active batch 仍保留 next_process_id 非空 → 按
--        batch.next_process_id = t_process_chain_step.process_id
--        AND step.chain_id = part.process_chain_id
--        AND step.deleted_at IS NULL
--        AND batch.deleted_at IS NULL
--        AND p.deleted_at IS NULL
--      匹配 chain step，回填 current_process_step_id。
--      无链 part 的 batch → NULL。
--   3. 删旧列 next_process_id、placed_at。
--   4. 普通部分索引 ix_t_part_batch_current_step_id（按 current_process_step_id
--      加速 worker-pool JOIN + step 解析）。
--
-- 幂等：ADD COLUMN / DROP COLUMN / CREATE INDEX 均带 IF [NOT] EXISTS；
-- UPDATE 用 LEFT JOIN，未匹配的 batch 行 current_process_step_id 保持 NULL。

-- 1. 加新列
ALTER TABLE public.t_part_batch
    ADD COLUMN IF NOT EXISTS current_process_step_id bigint;

COMMENT ON COLUMN public.t_part_batch.current_process_step_id IS
    '逻辑 FK → t_process_chain_step.id；batch 当前所处的工艺链步骤。NULL 表示批次尚未进入生产流（PENDING/PROGRAMMING）或所属 part 无工艺链或 step 已软删。';

-- 2. 数据回填（同一事务内完成）：按 batch.next_process_id = step.process_id
--    AND step.chain_id = part.process_chain_id 匹配。
UPDATE public.t_part_batch pb
SET current_process_step_id = s.id
FROM public.t_part p,
     public.t_process_chain_step s
WHERE pb.part_id = p.id
  AND p.process_chain_id IS NOT NULL
  AND p.deleted_at IS NULL
  AND pb.deleted_at IS NULL
  AND pb.next_process_id IS NOT NULL
  AND s.deleted_at IS NULL
  AND s.chain_id = p.process_chain_id
  AND s.process_id = pb.next_process_id;

-- 3. 删旧列（连带 PG 自动带出的索引若有，DDL 顺序：DROP COLUMN → DROP INDEX，
--    此表无 next_process_id / placed_at 上的索引，跳过显式 DROP INDEX）。
ALTER TABLE public.t_part_batch
    DROP COLUMN IF EXISTS next_process_id;

ALTER TABLE public.t_part_batch
    DROP COLUMN IF EXISTS placed_at;

-- 4. 普通部分索引（按 current_process_step_id 加速 worker-pool JOIN + step 解析）
CREATE INDEX IF NOT EXISTS ix_t_part_batch_current_step_id
    ON public.t_part_batch(current_process_step_id)
    WHERE current_process_step_id IS NOT NULL;
