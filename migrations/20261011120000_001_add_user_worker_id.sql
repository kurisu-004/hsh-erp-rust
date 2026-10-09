-- ============================================================================
-- 2026-10-11：t_user.worker_id —— 系统账号「正式绑定」到某个工人（t_worker.id）
-- ============================================================================
-- 背景（wx BFF 按小程序页面切模块重构第 1 步）：
--   小程序生产页要显示「当前登录工人 + 当月工作量」。既有端点
--   `GET /api/v2/wx/worker/stats?period=YYYY-MM` 拿 `CurrentUser.id`
--   （= `t_user.id`）直接去查 `t_part_event.worker_id`，而后者的真实语义是
--   **`t_worker.id`**。两者之间**没有任何映射表**，只是「碰巧共用同一个雪花
--   ID 空间」—— 实测 5750 条 `t_part_event` 的 13 个 worker_id 全部命中
--   `t_worker`、零命中 `t_user`，故该端点对任何真实用户恒返 `batch_count = 0`。
--
-- 为什么新增列而不是在查询里现推：
--   `t_user.username = t_worker.badge_code` 这种推断在真实数据上已被证伪
--   （如 `13350114794` 的工人工牌是 `13359114794`，号段笔误；另有工人在
--   `t_worker` 里查无此人）。绑定关系是**业务事实**，必须由人确认后落库，
--   不能由查询期猜测。故新增本列作为正式绑定。
--
-- 本仓硬约束（见 migrations/README.md）：
--   - **无物理外键**：跨表引用 = 普通 `bigint` 列 + 索引，存在性 / 级联由
--     service 层校验。故本列**不**加 `REFERENCES t_worker(id)`。
--   - 软删 `deleted_at`：软删账号的绑定关系与账号同生共死，本列无独立的
--     软删语义。
--
-- 可空：绝大多数系统账号（admin / 系统管理员 / hmi-* 等非工人账号）没有
-- 对应工人，NULL 表示「未绑定」，调用方须把 NULL 与「绑定但当月零工作量」
-- 区分开。
--
-- 回填：见 `scripts/sql/20261011_backfill_t_user_worker_id.sql`
-- （**一次性数据回填脚本，不在本 migration 内**，需人工确认后手工执行）。

ALTER TABLE public.t_user
    ADD COLUMN worker_id bigint;

COMMENT ON COLUMN public.t_user.worker_id IS
  '该系统账号绑定的工人（t_worker.id）；NULL = 非工人账号 / 尚未绑定。'
  '无物理外键（仓库铁律），指向的工人是否存在由 service 层校验。'
  '2026-10-11 随 wx BFF 重构新增：此前 t_user 与 t_worker 之间无映射，'
  'GET /api/v2/wx/worker/stats 把 t_user.id 当 t_worker.id 用，恒返 0。';

-- 部分索引：只索引「已绑定」的活跃账号。账号总量小、绑定比例更低
-- （多数是 admin / hmi-* 这类非工人账号），谓词过滤能显著压住索引体积。
-- 与既有 partial 索引风格一致（如 ix_t_part_event_batch_id / ix_t_user_username）。
CREATE INDEX ix_t_user_worker_id
    ON public.t_user USING btree (worker_id)
    WHERE (worker_id IS NOT NULL AND deleted_at IS NULL);
