-- Migration 027: t_part / t_part_batch / t_assembly 瘦身 —— 删除批次依附列
-- 2026-09-16 PR-2 part-slim-down
--
-- 背景（2026-09-16 用户决策）：
--   t_part 只表示工单，actual_delivery_date / location / current_holder_id /
--   placed_at / delivery_note_id 均依附于具体批次，不应物化在工单行上；
--   t_assembly.actual_delivery_date 同理。t_part 与 t_part_batch 的
--   has_been_repaired 一并移除：拆批后无法确定是哪一个批次返修，列语义已失真。
--
-- 数据影响说明：
--   - t_part.placed_at / actual_delivery_date 的历史信息在 t_part_event 中有
--     等价时间戳（上架 / DELIVERED 事件），统计口径改为事件派生
--     （statistics 域已同步改写为 MAX(t_part_event.created_at::date)
--     WHERE event_type='DELIVERED'）。
--   - location / current_holder_id / delivery_note_id 的真相源是
--     t_part_batch 同名列（本迁移不动）；引用方（cancel / soft-delete /
--     worker/shelf 停用守卫 / to_inspection 组合校验）已改为查批次。
--   - t_part.next_process_id 保留作 rollup 读缓存（不在本迁移范围）。
--
-- 变更：
--   1. t_part DROP 6 列：actual_delivery_date / location / current_holder_id /
--      placed_at / delivery_note_id / has_been_repaired
--      PG 自动带出涉及这些列的索引：
--        ix_t_part_current_holder_id / ix_t_part_delivery_note_id /
--        ix_t_part_location / ix_t_part_location_status_next_process /
--        ix_t_part_placed_at / ix_t_part_status_holder
--      （下方仍显式 DROP INDEX IF EXISTS 兜底，保证无残留）
--   2. t_part_batch DROP has_been_repaired
--   3. t_assembly DROP actual_delivery_date
--
-- 幂等：DROP COLUMN / DROP INDEX 均带 IF EXISTS，可重复执行。

-- 1. t_part 批次依附列删除（显式先落索引，再落列；两者幂等）
DROP INDEX IF EXISTS public.ix_t_part_current_holder_id;
DROP INDEX IF EXISTS public.ix_t_part_delivery_note_id;
DROP INDEX IF EXISTS public.ix_t_part_location;
DROP INDEX IF EXISTS public.ix_t_part_location_status_next_process;
DROP INDEX IF EXISTS public.ix_t_part_placed_at;
DROP INDEX IF EXISTS public.ix_t_part_status_holder;

ALTER TABLE public.t_part
    DROP COLUMN IF EXISTS actual_delivery_date,
    DROP COLUMN IF EXISTS location,
    DROP COLUMN IF EXISTS current_holder_id,
    DROP COLUMN IF EXISTS placed_at,
    DROP COLUMN IF EXISTS delivery_note_id,
    DROP COLUMN IF EXISTS has_been_repaired;

-- 2. t_part_batch 返修标删除
ALTER TABLE public.t_part_batch
    DROP COLUMN IF EXISTS has_been_repaired;

-- 3. t_assembly 实际交付日删除
ALTER TABLE public.t_assembly
    DROP COLUMN IF EXISTS actual_delivery_date;
