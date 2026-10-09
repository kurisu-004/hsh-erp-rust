-- ============================================================================
-- 2026-10-12：t_part_event.process_id —— 事件发生时批次所处的工序快照
--              （生产统计重构第 1 批，第 1 条）
-- ============================================================================
-- 语义：**事件发生时，该批次所处的工序**，逻辑指向 `t_process.id`。
--       与 `t_part_batch.current_process_id`（批次**当前**所在工序，会随流转推进）
--       是两回事：本列是**写事件那一刻**的快照，读历史行时不会被后续流转改写。
--
-- 为什么需要它（事件流与批次当前位置解不开的必然结果）：
--   `RETURNED_TO_SHELF` 事件写在 `mark_batch_returned` **之后**，而该函数同一
--   事务内已把 `t_part_batch.current_process_id` 推进到**下一道工序**（放回货架后
--   批次重新入池）。等统计侧再回查批次当前工序时，读到的已经是「下一道」，
--   永远拿不到「这位工人刚做完哪道工序」——事件行上没有工序快照就没有第二个
--   数据源。有效工时按工序归集也依赖同一列。
--
-- 本仓硬约束（见 migrations/README.md）：
--   - **无物理外键**：跨表引用 = 普通 `bigint` 列 + 索引，存在性由 service 层
--     校验。故本列**不**加 `REFERENCES t_process(id)`。
--   - 无 DB ENUM；软删 / 级联语义一律在 service 层。
--
-- 可空：本列 2026-10-12 只加列、**不在本 migration 回填**，历史事件的
-- `process_id` 恒 NULL（写入侧补列随本轮重构的后续任务落地）。NULL 读作
-- 「该事件未记录工序」，调用方须与「记录了工序 0」区分开。
--
-- 同批的 `ix_part_event_worker_type_created` 索引是既有聚合查询的支撑：
-- statistics 域「工人取件 / 放回」按 `(worker_id, event_type, created_at)`
-- 过滤，该三元组上无索引时每次聚合都是全表扫（`t_part_event` 是增长最快的表）。
-- 部分谓词 `worker_id IS NOT NULL` 把「无工人的系统调度 / 历史事件」排除在索引外。
-- ============================================================================

ALTER TABLE public.t_part_event
    ADD COLUMN process_id bigint;

COMMENT ON COLUMN public.t_part_event.process_id IS
  '事件发生时该批次所处的工序（t_process.id）；NULL = 未记录（历史事件 / 无批次上下文）。'
  '与 t_part_batch.current_process_id（批次当前工序，会随流转推进）语义不同：'
  '本列是写事件那一刻的快照。无物理外键（仓库铁律），指向的工序是否存在由 service 层校验。'
  '2026-10-12 随生产统计重构新增：RETURNED_TO_SHELF 事件写在 mark_batch_returned 之后，'
  '此时批次 current_process_id 已推进到下一道工序，不快照就永远拿不到「工人刚做完哪道工序」。';

-- worker_id 语义依据：本列真实语义是 **t_worker.id**（不是 t_user.id）。
--   两者无映射表，只是共用同一雪花 ID 空间；实测 5750 条 t_part_event 的 13 个
--   worker_id **全部命中 t_worker、零命中 t_user**
--   （依据见 20261011120000_001_add_user_worker_id.sql 文件头 7-9 行）。
--   统计局标据此建索引；索引前缀列的语义写在这里，避免后人按 t_user.id 去用。
-- 部分索引：三元组聚合（工人 × 事件类型 × 时间窗）是本域最高频的访问形态，
--   worker_id IS NOT NULL 把无工人的系统调度 / 历史事件挡在索引外。
CREATE INDEX ix_part_event_worker_type_created
    ON public.t_part_event USING btree (worker_id, event_type, created_at)
    WHERE (worker_id IS NOT NULL);

-- 按工序归集（有效工时聚合 / 工序维度统计）走本索引；process_id 为 NULL 的
-- 历史事件不参与。谓词与列本身的可空性同义，保留是为了与本批另一条 partial 索引
-- 保持同一形态（并让查询侧 EXPLAIN 能用上 partial index）。
CREATE INDEX ix_part_event_process
    ON public.t_part_event USING btree (process_id)
    WHERE (process_id IS NOT NULL);
