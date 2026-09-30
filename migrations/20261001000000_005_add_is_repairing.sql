-- ============================================================================
-- Migration 005: t_part_batch.is_repairing —— REPAIRING 降级为标记（flag）
-- ============================================================================
-- 2026-10-01 新增
--
-- 背景：
--   2026-09-30 用户拍板「REPAIRING 不再作为状态机状态，仅作标记（flag）」，
--   本迁移是那次决策的 schema 落地。配套的 Rust 侧改造见
--   `src/modules/part/repo/status_gate.rs`（单一写入口 status_gate）：
--   批次进入返修时 `status` 保持 `IN_PROCESS`（返修仍在生产中，
--   `part_status_progress('IN_PROCESS') = 2` 与原 `REPAIRING` 同档），
--   返修事实改由本列承载。
--
-- 为什么不复用 `t_part_event` 的 REPAIR_STARTED 事件做查询依据：
--   事件表是 append-only 审计流水，「当前是否处于返修中」需要的是**状态**，
--   每次查询都去扫事件流既慢又易与「已完成的返修」混淆。本列是当前态，
--   事件表保留历史，两者互补。
--
-- 与既有 `has_been_repaired` 的区别（2026-09-16 PR-2 已删该列）：
--   `has_been_repaired` 表达的是「**曾经**返修过」（历史事实，拆批后无法确定是
--   哪个批次返修，列语义失真而废弃）；本列表达的是「**当前**处于返修中」
--   （当前态，批次粒度，不受拆批影响）。
--
-- 写入不变式（src 侧全部经 status_gate，调用方无「要不要顺手写一下」的选择权）：
--   开始返修（start_repair / scan-inspect FAIL）→ is_repairing = true
--   返修完成 / 送检 / 其它任何流转        → is_repairing = false
--   初始批次 / 拆批继承                   → 见 `_split_batch_inner`（与源行同 SELECT 列表）
--
-- 索引选择说明：
--   复合索引 (part_id) WHERE is_repairing AND deleted_at IS NULL —— 现有
--   「返修中批次」类查询（`GET /parts/repairing-batches` 等）判据都是
--   `part_id + is_repairing + deleted_at IS NULL`，本索引精确覆盖；
--   `is_repairing` 单列选择性过低（false 占绝大多数），故只作复合索引首列
--   之外的**部分索引谓词**，不单独建单列索引。

ALTER TABLE public.t_part_batch
    ADD COLUMN is_repairing boolean DEFAULT false NOT NULL;

COMMENT ON COLUMN public.t_part_batch.is_repairing IS
    '本批次是否处于返修中（status 合并 REPAIRING→IN_PROCESS 后的返修语义承载列）';

CREATE INDEX ix_t_part_batch_is_repairing
    ON public.t_part_batch (part_id)
    WHERE is_repairing AND deleted_at IS NULL;
