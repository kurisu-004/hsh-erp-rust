-- 2026-10-07 新增：dashboard 域逾期未交查询打的是 system_delivery_date，
-- 而 t_assembly 既有索引只有 planned 交期（ix_t_assembly_planned_delivery）。
-- 部分索引 + deleted_at IS NULL 与本域既有 6 条风格一致
-- （ix_t_part_batch_holder_location / ix_t_part_batch_status_holder 等）。
CREATE INDEX ix_t_assembly_system_delivery_date
  ON public.t_assembly (system_delivery_date)
  WHERE deleted_at IS NULL;