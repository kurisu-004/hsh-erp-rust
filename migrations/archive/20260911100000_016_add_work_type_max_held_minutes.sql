-- 016: 给 t_work_type 增加 max_held_minutes 列（auto-allocate TIME 模式阈值依据）
-- 2026-09-11 part-worker-pool-federated-rocket 方案
--
-- 目的：worker-pool auto-allocate 端点（POST /api/v2/admin/worker-pool/auto-allocate）
-- 在 TIME 模式下，按 `work_type.max_held_minutes × fill_ratio` 累计预估工时分配候选批次；
-- NULL 表示该工种未设置工时阈值，TIME 模式调用会触发
-- 20703 BIZ_WORK_TYPE_MAX_HELD_MINUTES_NOT_SET。
--
-- 与 `max_held_batches` 的关系：两个阈值独立，service 层根据 mode 字段二选一使用。
-- COUNT 模式用 max_held_batches，TIME 模式用 max_held_minutes。

ALTER TABLE public.t_work_type
    ADD COLUMN max_held_minutes INTEGER;

COMMENT ON COLUMN public.t_work_type.max_held_minutes IS
    '工种按预估工时计算的最大持有分钟数；NULL=未设置，auto-allocate TIME 模式调用会触发 20703 BIZ_WORK_TYPE_MAX_HELD_MINUTES_NOT_SET';