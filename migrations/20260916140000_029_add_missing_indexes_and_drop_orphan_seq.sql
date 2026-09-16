-- Migration 029: 补齐缺失索引 + 清理孤儿序列（2026-09-17 PR-4 卫生项）
--
-- 背景（docs/audit-5-tables-2026-09-16.md §3.3 / §3.5）：
--   - t_part_batch.parent_batch_id 缺索引（拆分谱系查询当前无 hot path，
--     但 history 查询会变慢；B1 补上）
--   - t_part.system_delivery_date 缺索引（worker_pool/repo.rs:115 hot 排序
--     字段；B1 补上）
--   - t_assembly.name 缺索引（assembly 列表筛名；B1 补上）
--   - t_assembly_id_seq 是孤儿序列（migrations/005:47 注释：「id is supplied
--     by application code」，但 ATTACHED 序列仍在；B4 清理）
--
-- 幂等：CREATE INDEX IF NOT EXISTS / DROP SEQUENCE IF EXISTS 均幂等。

------------------------------------------------------------------------
-- B1: 补齐 3 个索引
------------------------------------------------------------------------

-- t_part_batch.parent_batch_id（拆分谱系；按值常驻使用；不为 NULL 部分索引）
CREATE INDEX IF NOT EXISTS ix_t_part_batch_parent_batch_id
    ON public.t_part_batch(parent_batch_id)
    WHERE parent_batch_id IS NOT NULL;

-- t_part.system_delivery_date（worker_pool/repo.rs:115 hot sort）
CREATE INDEX IF NOT EXISTS ix_t_part_system_delivery_date
    ON public.t_part(system_delivery_date);

-- t_assembly.name（assembly 列表筛名）
CREATE INDEX IF NOT EXISTS ix_t_assembly_name
    ON public.t_assembly(name);

------------------------------------------------------------------------
-- B4: 清理孤儿 t_assembly_id_seq
--
-- 依据 migrations/005:47 注释「t_assembly.id has NO snowflake SEQUENCE in
-- production — id is supplied by application code」，id 列无 DEFAULT nextval
-- 调用，序列 ATTACHED 但无引用。DROP SEQUENCE 前 IF EXISTS 已兜底。
------------------------------------------------------------------------

DROP SEQUENCE IF EXISTS public.t_assembly_id_seq;