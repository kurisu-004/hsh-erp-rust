-- 订正 `t_work_type` 两处列注释（2026-10-15）
--
-- ## 为什么需要
--
-- 1. `max_held_minutes` 的 live 注释（baseline migration 写入）仍写着
--    「TIME 模式调用会触发 20703 BIZ_WORK_TYPE_MAX_HELD_MINUTES_NOT_SET」——
--    但 TIME 模式与 20703 错误码已随 2026-10-15 的「自动分配上限」特性释放
--    （`auto-allocate` 删 `mode` / `fill_ratio`，本列无任何 Rust 读侧）。
--    注释与实现不符会让后人误信「这条路径还在」。
--
-- 2. `max_held_batches` 的 live 注释由 `20261014000001_001` 写入，其中
--    「取料按 cap 0 拦死」描述的是 `take_one_from_pool` 里的 `max_batches` CTE
--    （`COALESCE(wt.max_held_batches, 0)`）—— 该 CTE 已于 2026-10-15 随同一
--    特性删除，cap 改由 service 算出后传入（`auto_allocate_limit` 兜底
--    `max_held_batches`），本列不再被取料 SQL 直接读取。这是与 NIT-1 同类的
--    「注释描述已删机制」漂移，故一并由本条覆盖。
--
-- ## append-only
--
-- 按 CLAUDE.md「新 schema 变更走追加新 migration」：**本文件是新增，不修改
-- `migrations/20260925000000_001_baseline.sql`，也不动已应用的
-- `20261014000001_001` / `20261015000001_001`**。`COMMENT ON COLUMN` 作用于
-- 当前库，不追溯 migration 文件文本。
--
-- ## 幂等
--
-- 两条 `COMMENT ON` 重跑结果相同、不依赖任何一次性假设。

COMMENT ON COLUMN public.t_work_type.max_held_minutes IS
  '已弃用（2026-10-15）：TIME 模式与 20703 已移除，本列无任何 Rust 读侧；保留仅为兼容历史数据';

COMMENT ON COLUMN public.t_work_type.max_held_batches IS
  '工种工人最多可同时持有批次数；NULL = 未配置（move / pick-up / refill 返 20904，auto-allocate 逐 worker 记 MAX_BATCHES_NOT_SET 跳过），不是「不限」；取料 cap 自 2026-10-15 起由 service 传入，本列不再被 take_one_from_pool 直接读取';
