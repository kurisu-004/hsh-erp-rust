-- 清掉「已终态但仍挂在某个位置 / 持有人」上的批次行（2026-10-14）
--
-- ## 为什么需要这个 migration
--
-- `t_part_batch` 的三个终态写点（取消 CANCELLED / 强制完成 COMPLETED / 交付
-- DELIVERED）此前**都不清** `location` + `current_holder_id`。于是被判终态的批次
-- 仍以 `location='WORKER' + current_holder_id=<某工人>` 的形态留在表里：
--
--   - 容量校验（`prod::batch::repo::queries::count_held_by_worker`）把它们**算数**
--     —— 工人手上的「幽灵批次」永久吃掉 `t_work_type.max_held_batches` 的额度，
--     取消得越多工人越被卡死；
--   - 队列板看板（`prod::queue::board::repo::SQL_HELD_BATCHES_BY_WORKERS`）按
--     `status='IN_PROCESS'` 过滤，**看不见**它们 —— 于是「面板显示还有额度、
--     实际一律取件被拒」，运维完全无法解释；
--   - `/held` 报工台列表同样看不见，但它的 `total` COUNT 口径已与看板对齐，
--     两边数字对得上、只有写侧对不上。
--
-- 这类行**永久累积**（没有任何清理路径），所以必须一次性回补。
--
-- 本 migration 只清**数据**、不改判据。判据侧的修复（`count_held_by_worker` 与
-- `queue::repo::sql::take_one_from_pool` 的 `held` CTE 补
-- `status = 'IN_PROCESS'`）在同一次改动里落到代码上，两者是**互补**的两道：
-- 本 migration 治已存在的存量行，判据修复保证同类行再产生时不会被算进容量。
--
-- ## 为什么必须排除 'OFFICE'
--
-- 初始批次的 `location = 'OFFICE'` 是 `part::repo::part_sql` 里
-- `create_initial_batch` 写死的初值，与持有人**无关** —— 那种行的
-- `current_holder_id` 本来就是 NULL（建单期还没有工人）。把 OFFICE 一并清掉会
-- 破坏 OFFICE 列表（`location='OFFICE'` 是它的查询谓词）。
--
-- ## 判据
--
-- - `location IN ('WORKER','PRODUCTION_SHELF','INSPECTION_SHELF','OUTSOURCE_COMPANY')`
--   —— 即「除 OFFICE 之外的全部 5 种位置」。三种货架 / 外协位置上挂着
--   `current_holder_id` 是正常的（货架 id 就存在 `current_holder_id` 列里，见
--   `ix_t_part_batch_holder_location`），终态批次残留其中同样要清；
-- - `current_holder_id IS NOT NULL` —— 无持有人的行本来就是 NULL，不动；
-- - `status IN ('CANCELLED','COMPLETED','DELIVERED')` —— 只碰终态，
--   `IN_PROCESS` / `PENDING` / `INSPECTION` 等一律不碰；
-- - `deleted_at IS NULL` —— 软删行不再关心。
--
-- ⚠️ 这三个终态在批次表上是终态，**不回退**；清掉位置列不改变任何状态派生
-- （`shared::batch::status` 的 rollup 只看 `status`，不看 `location`）。
--
-- ## append-only
--
-- 按 CLAUDE.md 的「Schema 迁移与查询必须沿用」：**新变更走追加新 migration，
-- 永不改已有文件**。本文件是新增，不修改任何历史 migration。
--
-- ⚠️ 幂等：重跑一次命中 0 行（第一次已把 `current_holder_id` 清成 NULL），
-- 不依赖任何一次性假设。

UPDATE t_part_batch
   SET location = NULL, current_holder_id = NULL, updated_at = now()
 WHERE location IN ('WORKER','PRODUCTION_SHELF','INSPECTION_SHELF','OUTSOURCE_COMPANY')
   AND current_holder_id IS NOT NULL
   AND status IN ('CANCELLED','COMPLETED','DELIVERED')
   AND deleted_at IS NULL;
