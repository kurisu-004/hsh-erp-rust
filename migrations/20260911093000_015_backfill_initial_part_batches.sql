-- 015: 补建存量无批次 part 的初始 t_part_batch 行
-- 2026-09-11 part/assembly/batch 重构方案 §4.1 (PR-B1)
--
-- 目的：方案实施前已有 part 可能不带任何 t_part_batch 行，新建工单直接 to-inspection
-- 会触发 20109 BIZ_PART_BATCH_NOT_FOUND。本迁移为「无活跃批次」的活跃 part 补建
-- batch_no=1 / quantity=part.quantity / status=part.status / 派生列同步 / version=0
-- 的初始批次，保证车间流转语义统一。
--
-- 设计：
-- * id 生成：雪花 App 侧生成（migration 中不可用），专用 `seq_t_part_batch_backfill`
--   序列兜底（与 `t_part_batch_id_seq` 物理隔离，避免与 App 雪花 id 撞号）。
-- * 去重：`uq_t_part_batch_part_no (part_id, batch_no)` 不允许重复；用
--   `NOT EXISTS` 守卫只补「无活跃批次」的 part，已有 batch 的跳过。
-- * 派生列：status / location / current_holder_id / next_process_id / placed_at /
--   delivery_note_id / has_been_repaired 从 part 复制；
--   version=0 / created_by=NULL / updated_by=NULL（迁移无可追溯操作人）。
-- * 软删：只补 `p.deleted_at IS NULL`（活跃 part）。

CREATE SEQUENCE IF NOT EXISTS public.seq_t_part_batch_backfill
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

INSERT INTO public.t_part_batch (
    id, part_id, batch_no, quantity, status, location,
    current_holder_id, next_process_id, placed_at,
    delivery_note_id, parent_batch_id, has_been_repaired,
    version, created_at, created_by, updated_at, updated_by, deleted_at
)
SELECT
    nextval('public.seq_t_part_batch_backfill'),
    p.id, 1, p.quantity, p.status, p.location,
    p.current_holder_id, p.next_process_id, p.placed_at,
    p.delivery_note_id, NULL, p.has_been_repaired,
    0, now(), NULL, now(), NULL, NULL
FROM public.t_part p
WHERE p.deleted_at IS NULL
  AND NOT EXISTS (
        SELECT 1 FROM public.t_part_batch b
        WHERE b.part_id = p.id AND b.deleted_at IS NULL
  );
