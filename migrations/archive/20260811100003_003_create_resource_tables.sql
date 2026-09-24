-- Migration 003: t_work_type, t_process, t_shelf, t_worker
-- Notes:
--   * t_process.category uses CHECK (INHOUSE/OUTSOURCE) — soft enum.
--   * t_worker.id_card_no has a partial unique (only when NOT NULL).
--   * Phase E rewrite: aligned 1:1 with /Users/ren/Code/schema_ddl.sql (column order, COMMENT,
--     CREATE SEQUENCE ... OWNED BY, ALTER TABLE ... SET DEFAULT nextval).

------------------------------------------------------------------------
-- t_work_type
------------------------------------------------------------------------
CREATE TABLE public.t_work_type (
    id                 bigint NOT NULL,
    code               character varying(32) NOT NULL,
    name               character varying(50) NOT NULL,
    description        character varying(200),
    sort_order         integer DEFAULT 0 NOT NULL,
    version            integer DEFAULT 0 NOT NULL,
    created_at         timestamp without time zone DEFAULT now() NOT NULL,
    created_by         bigint,
    updated_at         timestamp without time zone DEFAULT now() NOT NULL,
    updated_by         bigint,
    deleted_at         timestamp without time zone,
    max_held_batches   integer
);

COMMENT ON COLUMN public.t_work_type.code IS '工种代码（业务唯一键，不可变）';
COMMENT ON COLUMN public.t_work_type.name IS '工种名称（前端显示）';
COMMENT ON COLUMN public.t_work_type.sort_order IS '显示顺序';
COMMENT ON COLUMN public.t_work_type.version IS '乐观锁版本号；每次 UPDATE 自增；冲突抛 BIZ_VERSION_CONFLICT 409';
COMMENT ON COLUMN public.t_work_type.max_held_batches IS '工种工人最多可同时持有批次数；NULL=不限';

CREATE SEQUENCE public.t_work_type_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.t_work_type_id_seq OWNED BY public.t_work_type.id;

ALTER TABLE ONLY public.t_work_type ALTER COLUMN id SET DEFAULT nextval('public.t_work_type_id_seq'::regclass);
ALTER TABLE ONLY public.t_work_type ADD CONSTRAINT t_work_type_pkey PRIMARY KEY (id);

CREATE UNIQUE INDEX uk_t_work_type_code ON public.t_work_type USING btree (code) WHERE (deleted_at IS NULL);
CREATE INDEX ix_t_work_type_code ON public.t_work_type USING btree (code);
CREATE INDEX ix_t_work_type_deleted_at ON public.t_work_type USING btree (deleted_at);

------------------------------------------------------------------------
-- t_process
------------------------------------------------------------------------
CREATE TABLE public.t_process (
    id                   bigint NOT NULL,
    code                 character varying(32) NOT NULL,
    name                 character varying(50) NOT NULL,
    category             character varying(16) NOT NULL,
    sort_order           integer DEFAULT 0 NOT NULL,
    description          character varying(200),
    version              integer DEFAULT 0 NOT NULL,
    created_at           timestamp without time zone DEFAULT now() NOT NULL,
    created_by           bigint,
    updated_at           timestamp without time zone DEFAULT now() NOT NULL,
    updated_by           bigint,
    deleted_at           timestamp without time zone,
    requires_approval    boolean DEFAULT true NOT NULL,
    CONSTRAINT ck_t_process_category CHECK (((category)::text = ANY ((ARRAY['INHOUSE'::character varying, 'OUTSOURCE'::character varying])::text[])))
);

COMMENT ON COLUMN public.t_process.code IS '工序代码（业务唯一键，不可变）';
COMMENT ON COLUMN public.t_process.name IS '工序名称（前端显示）';
COMMENT ON COLUMN public.t_process.category IS 'INHOUSE 自产 / OUTSOURCE 外协';
COMMENT ON COLUMN public.t_process.sort_order IS '显示顺序';
COMMENT ON COLUMN public.t_process.version IS '乐观锁版本号；每次 UPDATE 自增；冲突抛 BIZ_VERSION_CONFLICT 409';
COMMENT ON COLUMN public.t_process.requires_approval IS '外协工序是否需要报价审批';

CREATE SEQUENCE public.t_process_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.t_process_id_seq OWNED BY public.t_process.id;

ALTER TABLE ONLY public.t_process ALTER COLUMN id SET DEFAULT nextval('public.t_process_id_seq'::regclass);
ALTER TABLE ONLY public.t_process ADD CONSTRAINT t_process_pkey PRIMARY KEY (id);

CREATE UNIQUE INDEX uk_t_process_code ON public.t_process USING btree (code) WHERE (deleted_at IS NULL);
CREATE INDEX ix_t_process_code ON public.t_process USING btree (code);
CREATE INDEX ix_t_process_category ON public.t_process USING btree (category);
CREATE INDEX ix_t_process_deleted_at ON public.t_process USING btree (deleted_at);

------------------------------------------------------------------------
-- t_shelf
------------------------------------------------------------------------
CREATE TABLE public.t_shelf (
    id             bigint NOT NULL,
    code           character varying(32) NOT NULL,
    name           character varying(100) NOT NULL,
    zone           character varying(16) NOT NULL,
    location       character varying(200),
    is_active      boolean DEFAULT true NOT NULL,
    version        integer DEFAULT 0 NOT NULL,
    created_at     timestamp without time zone DEFAULT now() NOT NULL,
    created_by     bigint,
    updated_at     timestamp without time zone DEFAULT now() NOT NULL,
    updated_by     bigint,
    deleted_at     timestamp without time zone,
    display_order  integer DEFAULT 0 NOT NULL
);

COMMENT ON COLUMN public.t_shelf.version IS '乐观锁版本号；每次 UPDATE 自增；冲突抛 BIZ_VERSION_CONFLICT 409';
COMMENT ON COLUMN public.t_shelf.display_order IS '物理顺序（0=未设置；manager 在 ShelfList 后台手填）';

CREATE SEQUENCE public.t_shelf_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.t_shelf_id_seq OWNED BY public.t_shelf.id;

ALTER TABLE ONLY public.t_shelf ALTER COLUMN id SET DEFAULT nextval('public.t_shelf_id_seq'::regclass);
ALTER TABLE ONLY public.t_shelf ADD CONSTRAINT t_shelf_pkey PRIMARY KEY (id);

CREATE UNIQUE INDEX uk_t_shelf_code ON public.t_shelf USING btree (code) WHERE (deleted_at IS NULL);
CREATE INDEX ix_t_shelf_zone ON public.t_shelf USING btree (zone);
CREATE INDEX ix_t_shelf_deleted_at ON public.t_shelf USING btree (deleted_at);
CREATE INDEX ix_t_shelf_display_order ON public.t_shelf USING btree (display_order, code) WHERE (deleted_at IS NULL);

------------------------------------------------------------------------
-- t_worker
------------------------------------------------------------------------
CREATE TABLE public.t_worker (
    id            bigint NOT NULL,
    badge_code    character varying(50) NOT NULL,
    name          character varying(50) NOT NULL,
    id_card_no    character varying(18),
    phone         character varying(20),
    is_active     boolean DEFAULT true NOT NULL,
    work_type_id  bigint,
    version       integer DEFAULT 0 NOT NULL,
    created_at    timestamp without time zone DEFAULT now() NOT NULL,
    created_by    bigint,
    updated_at    timestamp without time zone DEFAULT now() NOT NULL,
    updated_by    bigint,
    deleted_at    timestamp without time zone
);

COMMENT ON COLUMN public.t_worker.id_card_no IS '身份证号；与 id 组成联合唯一索引';
COMMENT ON COLUMN public.t_worker.phone IS '手机号';
COMMENT ON COLUMN public.t_worker.is_active IS '是否在职';
COMMENT ON COLUMN public.t_worker.work_type_id IS '逻辑外键 → t_work_type.id；NULL = 未分配工种';
COMMENT ON COLUMN public.t_worker.version IS '乐观锁版本号；每次 UPDATE 自增；冲突抛 BIZ_VERSION_CONFLICT 409';

CREATE SEQUENCE public.t_worker_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.t_worker_id_seq OWNED BY public.t_worker.id;

ALTER TABLE ONLY public.t_worker ALTER COLUMN id SET DEFAULT nextval('public.t_worker_id_seq'::regclass);
ALTER TABLE ONLY public.t_worker ADD CONSTRAINT t_worker_pkey PRIMARY KEY (id);

CREATE UNIQUE INDEX uk_t_worker_badge_code ON public.t_worker USING btree (badge_code) WHERE (deleted_at IS NULL);
CREATE UNIQUE INDEX uk_t_worker_id_card_no ON public.t_worker USING btree (id_card_no) WHERE (id_card_no IS NOT NULL);
CREATE INDEX ix_t_worker_name ON public.t_worker USING btree (name);
CREATE INDEX ix_t_worker_deleted_at ON public.t_worker USING btree (deleted_at);
CREATE INDEX ix_t_worker_work_type_id ON public.t_worker USING btree (work_type_id);