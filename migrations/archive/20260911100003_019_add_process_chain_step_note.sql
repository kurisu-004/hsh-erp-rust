-- Migration 019: t_process_chain_step 加 note 备注列
--
-- 业务背景（2026-09-12 需求）：车间调度员希望在每一步工艺上加备注，记录该步的特殊要求
-- （如"必须干燥 24h 后才能上 CNC"、"换工件后需要重新对刀"等）。备注对生产排产无副作用，
-- 仅作人工参考；UI 上每张 ChainStepCard 多一个 el-input（textarea）。
--
-- 与 header 的 `t_part_process_chain.note` 同义但更细粒度 —— header 是整链备注，step 是
-- 单步备注。

ALTER TABLE t_process_chain_step ADD COLUMN note TEXT;

COMMENT ON COLUMN t_process_chain_step.note IS
    '单步备注；车间操作员参考（如"必须干燥 24h 后才能上 CNC"）。NULL = 无备注。';