-- 2026-10-10 新增：t_shelf.capacity —— 货架负载上限（件数）
--
-- 选架算法（shared::shelf::select::pick_least_loaded）按
-- current_load / capacity 升序挑目标货架；capacity 为 NULL 或 <= 0 视为
-- 「不限」，排序时恒排最后（见该函数 doc）。
--
-- current_load 不是存储列，是读时从 t_part_batch 聚合出来的
-- SUM(quantity)（件数口径，见 shared::shelf::load.rs）。

ALTER TABLE t_shelf ADD COLUMN capacity integer;

COMMENT ON COLUMN t_shelf.capacity IS '负载上限（件数）。NULL 或 <= 0 = 不限。选架按 current_load / capacity 升序，超载不拒。';