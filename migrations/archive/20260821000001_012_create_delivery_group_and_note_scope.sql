-- Migration 012: t_delivery_group + t_delivery_group_member + delivery_note scope columns
-- Notes:
--   * 新实体（送货分组）：按 L1 客户配置的具名分组，成员为该 L1 的直接子客户（L2）。
--   * t_delivery_note 增加 delivery_group_id / leaf_customer_id 两列；
--     CHECK 约束两列不同时非空（D1：每单范围唯一）。
--   * 唯一约束均为 partial unique（WHERE deleted_at IS NULL），软删后允许复用。
--   * DRAFT 草稿 find-or-create 并发兜底：
--       - 分组单 / 单厂单各加 partial unique（status='DRAFT' AND deleted_at IS NULL）
--       - L1 全域草稿**不加**唯一索引（D4 + D5：保持多草稿兼容，靠
--         `ORDER BY id ASC LIMIT 1` 自然收敛）。
--   * t_delivery_group_member 单 L2 全局活跃唯一
--     （uq_t_delivery_group_member_customer_active），一个 L2 最多属于一个活跃分组。
--   * 初始数据（法拉电子「二五六厂」={二厂，五厂，六厂}）不入迁移文件，上线后
--     文员通过 UI 录入（与 Python 一致）。
--   * Phase P1（送货分组）实施；DDL 与 docs/delivery-note-redesign.md §4 对齐。

------------------------------------------------------------------------
-- t_delivery_group
------------------------------------------------------------------------
CREATE TABLE public.t_delivery_group (
    id          bigint NOT NULL,
    customer_id bigint NOT NULL,               -- L1 root，逻辑外键
    name        character varying(100) NOT NULL,
    version     integer DEFAULT 0 NOT NULL,
    created_at  timestamp without time zone DEFAULT now() NOT NULL,
    created_by  bigint,
    updated_at  timestamp without time zone DEFAULT now() NOT NULL,
    updated_by  bigint,
    deleted_at  timestamp without time zone
);

COMMENT ON COLUMN public.t_delivery_group.customer_id IS '逻辑外键 → t_customer.id（一级客户 / parent_id IS NULL）';
COMMENT ON COLUMN public.t_delivery_group.name IS '分组名（同 L1 下活跃内唯一）';
COMMENT ON COLUMN public.t_delivery_group.version IS '乐观锁版本号；每次 UPDATE 自增；冲突抛 BIZ_VERSION_CONFLICT 409';

ALTER TABLE ONLY public.t_delivery_group ADD CONSTRAINT t_delivery_group_pkey PRIMARY KEY (id);
CREATE INDEX ix_t_delivery_group_customer_id ON public.t_delivery_group USING btree (customer_id);
CREATE INDEX ix_t_delivery_group_deleted_at  ON public.t_delivery_group USING btree (deleted_at);
CREATE UNIQUE INDEX uq_t_delivery_group_name_active
    ON public.t_delivery_group USING btree (customer_id, name) WHERE (deleted_at IS NULL);

------------------------------------------------------------------------
-- t_delivery_group_member
------------------------------------------------------------------------
CREATE TABLE public.t_delivery_group_member (
    id          bigint NOT NULL,
    group_id    bigint NOT NULL,               -- 逻辑外键 → t_delivery_group.id
    customer_id bigint NOT NULL,               -- L2 叶子，逻辑外键 → t_customer.id
    created_at  timestamp without time zone DEFAULT now() NOT NULL,
    created_by  bigint,
    deleted_at  timestamp without time zone
);

COMMENT ON COLUMN public.t_delivery_group_member.group_id IS '逻辑外键 → t_delivery_group.id';
COMMENT ON COLUMN public.t_delivery_group_member.customer_id IS '逻辑外键 → t_customer.id（必须是 group.customer_id 的直接 L2 子节点）';

ALTER TABLE ONLY public.t_delivery_group_member ADD CONSTRAINT t_delivery_group_member_pkey PRIMARY KEY (id);
CREATE INDEX ix_t_delivery_group_member_group_id    ON public.t_delivery_group_member USING btree (group_id);
CREATE INDEX ix_t_delivery_group_member_customer_id ON public.t_delivery_group_member USING btree (customer_id);
-- 一个 L2 最多属于一个活跃分组
CREATE UNIQUE INDEX uq_t_delivery_group_member_customer_active
    ON public.t_delivery_group_member USING btree (customer_id) WHERE (deleted_at IS NULL);

------------------------------------------------------------------------
-- t_delivery_note 范围列（D1：每单一个范围；D4：L1 全域单 = 两列都 NULL）
------------------------------------------------------------------------
ALTER TABLE public.t_delivery_note
    ADD COLUMN delivery_group_id bigint,
    ADD COLUMN leaf_customer_id  bigint,
    ADD CONSTRAINT ck_t_delivery_note_scope_exclusive
        CHECK (NOT (delivery_group_id IS NOT NULL AND leaf_customer_id IS NOT NULL));

COMMENT ON COLUMN public.t_delivery_note.delivery_group_id IS '逻辑外键 → t_delivery_group.id；非空 = 分组单（D1）';
COMMENT ON COLUMN public.t_delivery_note.leaf_customer_id  IS '逻辑外键 → t_customer.id（叶子 L2）；非空 = 单厂单（D1）';

CREATE INDEX ix_t_delivery_note_delivery_group_id ON public.t_delivery_note USING btree (delivery_group_id)
    WHERE (delivery_group_id IS NOT NULL);
CREATE INDEX ix_t_delivery_note_leaf_customer_id ON public.t_delivery_note USING btree (leaf_customer_id)
    WHERE (leaf_customer_id IS NOT NULL);
-- 同范围活跃 DRAFT 唯一（find-or-create 并发兜底）
CREATE UNIQUE INDEX uq_t_delivery_note_draft_group ON public.t_delivery_note
    USING btree (customer_id, delivery_group_id)
    WHERE (deleted_at IS NULL AND status = 'DRAFT' AND delivery_group_id IS NOT NULL);
CREATE UNIQUE INDEX uq_t_delivery_note_draft_leaf ON public.t_delivery_note
    USING btree (customer_id, leaf_customer_id)
    WHERE (deleted_at IS NULL AND status = 'DRAFT' AND leaf_customer_id IS NOT NULL);