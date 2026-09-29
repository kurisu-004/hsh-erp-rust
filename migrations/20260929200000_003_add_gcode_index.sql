-- 003: t_part_file 部分索引（G_CODE 编程文件）— 加速 worker_pool EXISTS 子查询
-- 2026-09-29 新增
--
-- 背景：
--   src/modules/prod/worker_pool/repo/sql.rs 三处 EXISTS 子查询用
--   `WHERE part_id=$1 AND kind='G_CODE' AND deleted_at IS NULL` 判断"是否已上传 G_CODE"。
--   现存 ix_t_part_file_part_id 是 (part_id, deleted_at)，但不带 kind 过滤，无法直接用上。
--   worker_pool 候选池视图随 part 数与上传 G_CODE 数平方级扩大，缺索引会做全表扫描。
--
-- 索引设计：
--   partial index on (part_id) WHERE kind = 'G_CODE' AND deleted_at IS NULL
--   体量：仅 ~kind='G_CODE' 的行进入索引（典型 CNC 工件程序 1~5 个/零件，
--         远小于 part_file 总行数）。partial predicate 让索引只覆盖热点过滤条件，
--         体积最小且能直接走 index-only EXISTS。
--
-- 与已有索引关系：
--   ix_t_part_file_part_id (part_id, deleted_at) 保留作为通用查询；
--   本 partial index 仅加速 G_CODE 路径，不替代前者。
--
-- 加索引选项 IF NOT EXISTS 保证幂等；不影响在线 DDL（PG ≥11 CREATE INDEX IF NOT EXISTS 仅 S/R lock）。
CREATE INDEX IF NOT EXISTS ix_t_part_file_kind_gcode
    ON public.t_part_file (part_id)
    WHERE kind = 'G_CODE' AND deleted_at IS NULL;

COMMENT ON INDEX public.ix_t_part_file_kind_gcode
    IS 'worker_pool 已编程 batch 标注 EXISTS 子查询用（part_id+kind=G_CODE+deleted_at IS NULL）；2026-09-29 新增';