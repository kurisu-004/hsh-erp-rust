-- ============================================================================
-- Migration 002: t_process.is_cnc 列 + 部分索引 + CNC 工序 backfill
-- ============================================================================
--
-- 2026-09-29 新增：给 t_process 加 is_cnc 列（默认值 FALSE），用于：
--   1. 待编程一览（GET /api/v2/parts/pending-programming）的链上 CNC step 过滤
--   2. 待编程一览的 Tab 切换（has_cnc_program: bool?）
--   3. worker_pool 候选池的 has_cnc_program! 派生 + 自动分配优先级
--
-- 部分索引（仅 is_cnc=TRUE 行）保持索引体积小，避免每行非 CNC 工序入索引。
--
-- backfill：把现有 code='CNC' 的工序置 is_cnc=TRUE（按业务唯一键定位）。
--
-- 实施细节：
--   - 不写 NOT NULL CHECK 列约束，保持与既有 t_process 风格一致
--     （sort_order / requires_approval 也是 BOOLEAN NOT NULL DEFAULT）
--   - 部分索引谓词与 t_process.deleted_at IS NULL 对齐，确保活跃 CNC 行可见
-- ============================================================================

ALTER TABLE public.t_process
    ADD COLUMN is_cnc BOOLEAN NOT NULL DEFAULT FALSE;

COMMENT ON COLUMN public.t_process.is_cnc IS
    '是否 CNC 工序（用于待编程列表过滤与自动分配优先级）；2026-09-29 新增';

CREATE INDEX ix_t_process_is_cnc
    ON public.t_process(is_cnc)
    WHERE deleted_at IS NULL AND is_cnc = TRUE;

-- backfill：现有 CNC code 行置 is_cnc=TRUE（code='CNC' 是 t_process 业务唯一键）
UPDATE public.t_process
SET is_cnc = TRUE
WHERE code = 'CNC' AND deleted_at IS NULL;