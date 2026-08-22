-- Migration 004: t_work_type_process, t_shelf_process,
--                t_outsource_company, t_outsource_company_process
-- Notes:
--   * All four mapping tables carry a CHECK to forbid self-loop rows
--     (the pair (A, A) is rejected even though the unique index would also block it).
--   * Phase E rewrite: aligned 1:1 with /Users/ren/Code/schema_ddl.sql (column order, COMMENT,
--     CREATE SEQUENCE ... OWNED BY, ALTER TABLE ... SET DEFAULT nextval).

------------------------------------------------------------------------
-- t_work_type_process
------------------------------------------------------------------------
CREATE TABLE public.t_work_type_process (
    id           bigint NOT NULL,
    work_type_id bigint NOT NULL,
    process_id   bigint NOT NULL,
    sort_order   integer DEFAULT 0 NOT NULL,
    version      integer DEFAULT 0 NOT NULL,
    created_at   timestamp without time zone DEFAULT now() NOT NULL,
    created_by   bigint,
    updated_at   timestamp without time zone DEFAULT now() NOT NULL,
    updated_by   bigint,
    deleted_at   timestamp without time zone,
    CONSTRAINT ck_t_work_type_process_no_self_loop CHECK ((work_type_id <> process_id))
);

COMMENT ON COLUMN public.t_work_type_process.work_type_id IS '逻辑外键 → t_work_type.id';
COMMENT ON COLUMN public.t_work_type_process.process_id IS '逻辑外键 → t_process.id';
COMMENT ON COLUMN public.t_work_type_process.sort_order IS '工序在工种映射内的显示顺序';
COMMENT ON COLUMN public.t_work_type_process.version IS '乐观锁版本号；每次 UPDATE 自增；冲突抛 BIZ_VERSION_CONFLICT 409';

CREATE SEQUENCE public.t_work_type_process_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.t_work_type_process_id_seq OWNED BY public.t_work_type_process.id;

ALTER TABLE ONLY public.t_work_type_process ALTER COLUMN id SET DEFAULT nextval('public.t_work_type_process_id_seq'::regclass);
ALTER TABLE ONLY public.t_work_type_process ADD CONSTRAINT t_work_type_process_pkey PRIMARY KEY (id);

CREATE UNIQUE INDEX uk_t_work_type_process ON public.t_work_type_process USING btree (work_type_id, process_id)
    WHERE (deleted_at IS NULL);
CREATE INDEX ix_t_work_type_process_work_type ON public.t_work_type_process USING btree (work_type_id);
CREATE INDEX ix_t_work_type_process_process ON public.t_work_type_process USING btree (process_id);
CREATE INDEX ix_t_work_type_process_deleted_at ON public.t_work_type_process USING btree (deleted_at);

------------------------------------------------------------------------
-- t_shelf_process
------------------------------------------------------------------------
CREATE TABLE public.t_shelf_process (
    id          bigint NOT NULL,
    shelf_id    bigint NOT NULL,
    process_id  bigint NOT NULL,
    sort_order  integer DEFAULT 0 NOT NULL,
    version     integer DEFAULT 0 NOT NULL,
    created_at  timestamp without time zone DEFAULT now() NOT NULL,
    created_by  bigint,
    updated_at  timestamp without time zone DEFAULT now() NOT NULL,
    updated_by  bigint,
    deleted_at  timestamp without time zone,
    CONSTRAINT ck_t_shelf_process_no_self_loop CHECK ((shelf_id <> process_id))
);

COMMENT ON COLUMN public.t_shelf_process.shelf_id IS '逻辑外键 → t_shelf.id';
COMMENT ON COLUMN public.t_shelf_process.process_id IS '逻辑外键 → t_process.id';
COMMENT ON COLUMN public.t_shelf_process.sort_order IS '工序在货架映射内的显示顺序';
COMMENT ON COLUMN public.t_shelf_process.version IS '乐观锁版本号；每次 UPDATE 自增；冲突抛 BIZ_VERSION_CONFLICT 409';

CREATE SEQUENCE public.t_shelf_process_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.t_shelf_process_id_seq OWNED BY public.t_shelf_process.id;

ALTER TABLE ONLY public.t_shelf_process ALTER COLUMN id SET DEFAULT nextval('public.t_shelf_process_id_seq'::regclass);
ALTER TABLE ONLY public.t_shelf_process ADD CONSTRAINT t_shelf_process_pkey PRIMARY KEY (id);

CREATE UNIQUE INDEX uk_t_shelf_process ON public.t_shelf_process USING btree (shelf_id, process_id)
    WHERE (deleted_at IS NULL);
CREATE INDEX ix_t_shelf_process_shelf ON public.t_shelf_process USING btree (shelf_id);
CREATE INDEX ix_t_shelf_process_process ON public.t_shelf_process USING btree (process_id);
CREATE INDEX ix_t_shelf_process_deleted_at ON public.t_shelf_process USING btree (deleted_at);

------------------------------------------------------------------------
-- t_outsource_company
------------------------------------------------------------------------
CREATE TABLE public.t_outsource_company (
    id            bigint NOT NULL,
    name          character varying(100) NOT NULL,
    contact_name  character varying(50),
    contact_phone character varying(50),
    address       character varying(200),
    is_active     boolean DEFAULT true NOT NULL,
    version       integer DEFAULT 0 NOT NULL,
    created_at    timestamp without time zone DEFAULT now() NOT NULL,
    created_by    bigint,
    updated_at    timestamp without time zone DEFAULT now() NOT NULL,
    updated_by    bigint,
    deleted_at    timestamp without time zone
);

COMMENT ON COLUMN public.t_outsource_company.name IS '外协公司名';
COMMENT ON COLUMN public.t_outsource_company.contact_name IS '联系人';
COMMENT ON COLUMN public.t_outsource_company.contact_phone IS '联系电话';
COMMENT ON COLUMN public.t_outsource_company.address IS '地址';
COMMENT ON COLUMN public.t_outsource_company.is_active IS '是否启用（停用后下拉不再展示）';
COMMENT ON COLUMN public.t_outsource_company.version IS '乐观锁版本号；每次 UPDATE 自增；冲突抛 BIZ_VERSION_CONFLICT 409';

CREATE SEQUENCE public.t_outsource_company_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.t_outsource_company_id_seq OWNED BY public.t_outsource_company.id;

ALTER TABLE ONLY public.t_outsource_company ALTER COLUMN id SET DEFAULT nextval('public.t_outsource_company_id_seq'::regclass);
ALTER TABLE ONLY public.t_outsource_company ADD CONSTRAINT t_outsource_company_pkey PRIMARY KEY (id);

CREATE INDEX ix_t_outsource_company_name ON public.t_outsource_company USING btree (name);
CREATE INDEX ix_t_outsource_company_deleted_at ON public.t_outsource_company USING btree (deleted_at);
CREATE UNIQUE INDEX uk_t_outsource_company_name ON public.t_outsource_company USING btree (name) WHERE (deleted_at IS NULL);

------------------------------------------------------------------------
-- t_outsource_company_process
------------------------------------------------------------------------
CREATE TABLE public.t_outsource_company_process (
    id                    bigint NOT NULL,
    outsource_company_id  bigint NOT NULL,
    process_id            bigint NOT NULL,
    sort_order            integer DEFAULT 0 NOT NULL,
    version               integer DEFAULT 0 NOT NULL,
    created_at            timestamp without time zone DEFAULT now() NOT NULL,
    created_by            bigint,
    updated_at            timestamp without time zone DEFAULT now() NOT NULL,
    updated_by            bigint,
    deleted_at            timestamp without time zone,
    CONSTRAINT ck_t_outsource_company_process_no_self_loop CHECK ((outsource_company_id <> process_id))
);

COMMENT ON COLUMN public.t_outsource_company_process.outsource_company_id IS '逻辑外键 → t_outsource_company.id';
COMMENT ON COLUMN public.t_outsource_company_process.process_id IS '逻辑外键 → t_process.id（通常 category=OUTSOURCE）';
COMMENT ON COLUMN public.t_outsource_company_process.sort_order IS '工序在该公司能力清单内的显示顺序';
COMMENT ON COLUMN public.t_outsource_company_process.version IS '乐观锁版本号；每次 UPDATE 自增；冲突抛 BIZ_VERSION_CONFLICT 409';

CREATE SEQUENCE public.t_outsource_company_process_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.t_outsource_company_process_id_seq OWNED BY public.t_outsource_company_process.id;

ALTER TABLE ONLY public.t_outsource_company_process ALTER COLUMN id SET DEFAULT nextval('public.t_outsource_company_process_id_seq'::regclass);
ALTER TABLE ONLY public.t_outsource_company_process ADD CONSTRAINT t_outsource_company_process_pkey PRIMARY KEY (id);

CREATE UNIQUE INDEX uk_t_outsource_company_process ON public.t_outsource_company_process USING btree (outsource_company_id, process_id)
    WHERE (deleted_at IS NULL);
CREATE INDEX ix_t_outsource_company_process_company ON public.t_outsource_company_process USING btree (outsource_company_id);
CREATE INDEX ix_t_outsource_company_process_process ON public.t_outsource_company_process USING btree (process_id);
CREATE INDEX ix_t_outsource_company_process_deleted_at ON public.t_outsource_company_process USING btree (deleted_at);