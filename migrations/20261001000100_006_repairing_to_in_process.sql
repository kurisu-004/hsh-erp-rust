-- ============================================================================
-- Migration 006: REPAIRING 数据迁移 → is_repairing = true + status = IN_PROCESS
-- ============================================================================
-- 2026-10-01 新增
--
-- 背景：
--   Migration 005 把 `REPAIRING` 从状态机降级为标记列 `is_repairing`，但存量行
--   的 `t_part_batch.status` / `t_part.status` 里还留着 'REPAIRING' 字符串。
--   Rust 侧 `PartStatus` 枚举已删除 REPAIRING 变体（`from_str` 仍保留
--   `"REPAIRING" => Some(IN_PROCESS)` 的过渡兼容分支，容忍在途 / 历史数据，
--   见 `src/modules/part/statemachine.rs`），本迁移负责把存量行**一次性洗白**，
--   使 DB 层与 Rust 枚举严格一致。
--
-- 为什么两条 UPDATE 都要跑（不能只洗批次）：
--   `t_part.status` 是 batch → part rollup 的**派生缓存**，本迁移不回跑 rollup
--   （那要连开事务逐 part 算 `compute_part_target`），故直接就地改写。
--   改写结果与 rollup 派生值等价：REPAIRING 的 progress 原本就与 IN_PROCESS
--   同档（均为 2，见 `part_status_progress`），所以 part 侧不存在
--   「改完与 rollup 结果不一致」的情况。
--
-- 只处理未软删行（`deleted_at IS NULL`）：软删行不参与任何状态判定
-- （全仓所有 t_part / t_part_batch 查询都带该谓词），留着无副作用。
-- 唯一的例外是唯一索引 `uk_t_part_serial_no`（当时谓词只有
-- `serial_no IS NOT NULL`，软删行也占坑）—— 该索引的谓词修复与
-- 存量序列号释放见 Migration 007。
--
-- 幂等性：两条 UPDATE 都带 `status = 'REPAIRING'` 谓词，重复执行 0 行，安全。

UPDATE public.t_part_batch
   SET is_repairing = true,
       status       = 'IN_PROCESS',
       updated_at   = now()
 WHERE status = 'REPAIRING'
   AND deleted_at IS NULL;

UPDATE public.t_part
   SET status     = 'IN_PROCESS',
       updated_at = now()
 WHERE status = 'REPAIRING'
   AND deleted_at IS NULL;
