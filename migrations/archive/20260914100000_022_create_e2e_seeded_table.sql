-- Migration 022: t_e2e_seeded 元数据表
--   2026-09-14 新增
--
-- 用途：记录哪些行是由 /api/v2/_e2e/* seed 端点灌入的（reset 仅删除本表标记的行，
--       不动 alembic prod_data 灌入的种子用户/货架等）。
--
-- 设计取舍：
--   * 不引入 created_by = 固定 e2e 系统用户 ID 的方案，因为各业务表 created_by 列存在性不一致
--     （部分表无该列 / 部分列非空约束），且多 worktree 共享同一 dev DB 时 e2e 系统用户会被冲突。
--   * 改用独立元数据表 + 「entity / entity_id / seeded_at」三列，与业务表解耦。
--   * reset = 按 entity + entity_id 反查对应业务表软删（deleted_at = now()）；业务表本身
--     无硬删权限的，不强软删、仅清元数据表（不让 reset 改业务表数据完整性）。
--     —— 后续如果 reset 范围要扩大成「真删 + 重 seed alembic」，再加开关与 cascade 逻辑。
--
-- 不要混入业务查询 —— 本表仅 _e2e 模块读写。

CREATE TABLE public.t_e2e_seeded (
    entity       varchar(32)  NOT NULL,                          -- 'customer' / 'applicant' / 'worker' / ...
    entity_id    bigint       NOT NULL,
    seeded_at    timestamp    DEFAULT now() NOT NULL,
    CONSTRAINT t_e2e_seeded_pkey PRIMARY KEY (entity, entity_id)  -- 同 entity+id 只记一次（idempotent）
);

COMMENT ON TABLE  public.t_e2e_seeded IS '_e2e 模块 seed/reset 元数据：哪些行是 _e2e 灌入的';
COMMENT ON COLUMN public.t_e2e_seeded.entity    IS '业务实体类型，与路由段同名（customer/applicant/...）';
COMMENT ON COLUMN public.t_e2e_seeded.entity_id IS '业务表雪花主键；reset 时按 (entity, entity_id) 反查';
COMMENT ON COLUMN public.t_e2e_seeded.seeded_at IS 'seed 时间，仅供调试观察';

CREATE INDEX ix_t_e2e_seeded_seeded_at ON public.t_e2e_seeded USING btree (seeded_at);