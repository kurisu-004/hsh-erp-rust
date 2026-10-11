-- 工种「自动分配上限」`t_work_type.auto_allocate_limit`（2026-10-15）
--
-- ## 为什么需要
--
-- 三条自动补料路径（worker-scan 同事务 refill、`POST /prod/queue/refill`、
-- `POST /prod/queue/auto-allocate`）此前一律把工人补到 `max_held_batches`
-- （工人持有硬上限）。业务上需要「自动路径只补到一个更小的目标」而「手动
-- 单批路径（move / pick-up）仍按 max 放行」—— 例如某种工种手上只该同时开
-- 2 批，但允许临时手动多领。
--
-- ## 语义（在 service 层强制，DB 无 CHECK）
--
-- - `NULL` = **未配置** ⇒ 自动路径兜底 `max_held_batches`（行为与引入本列前
--   逐字一致，存量行零迁移）；
-- - 有值须 `1..=max_held_batches`（交叉校验在 `work_type/service.rs` 的
--   create / update 同一处矩阵，错误码复用 `20104 BIZ_INVALID_VALUE`）；
-- - 自动路径的三条诊断字面量见 `prod::queue::vo::worker::stopped_reason`
--   （新增 `AUTO_LIMIT_REACHED`）。
--
-- ## append-only
--
-- 按 CLAUDE.md「新 schema 变更走追加新 migration」：**本文件是新增，不修改
-- `migrations/20260925000000_001_baseline.sql`**，也不动 `archive/`。
--
-- ## 幂等
--
-- 只 ran 一次（sqlx 迁移记录保证）；SQL 本身对「列已存在」不幂等，
-- 重放请走 sqlx 的版本表，不要手工重复执行。

ALTER TABLE public.t_work_type
    ADD COLUMN auto_allocate_limit integer;

COMMENT ON COLUMN public.t_work_type.auto_allocate_limit IS
  '工种自动补料上限（refill / auto-allocate 的目标）；NULL=未配置⇒自动路径兜底 max_held_batches；有值须 1..=max_held_batches';
