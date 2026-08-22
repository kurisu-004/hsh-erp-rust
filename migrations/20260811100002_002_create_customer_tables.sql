-- Migration 002: t_customer, t_applicant, t_serial_counter
-- Notes:
--   * t_customer has a 1-letter upper-case `serial_prefix` (CHECK ~ '^[A-Z]$').
--   * t_serial_counter uses `prefix varchar(1)` as the business PK (snowflake id NOT used here).
--   * Phase E rewrite: aligned 1:1 with /Users/ren/Code/schema_ddl.sql (column order, COMMENT,
--     CREATE SEQUENCE ... OWNED BY, ALTER TABLE ... SET DEFAULT nextval).

------------------------------------------------------------------------
-- t_customer
------------------------------------------------------------------------
CREATE TABLE public.t_customer (
    id            bigint NOT NULL,
    name          character varying(100) NOT NULL,
    parent_id     bigint,
    version       integer DEFAULT 0 NOT NULL,
    created_at    timestamp without time zone DEFAULT now() NOT NULL,
    created_by    bigint,
    updated_at    timestamp without time zone DEFAULT now() NOT NULL,
    updated_by    bigint,
    deleted_at    timestamp without time zone,
    serial_prefix character varying(1),
    CONSTRAINT ck_t_customer_no_self_parent CHECK (((parent_id IS NULL) OR (parent_id <> id))),
    CONSTRAINT ck_t_customer_serial_prefix_uppercase CHECK (((serial_prefix IS NULL) OR ((serial_prefix)::text ~ '^[A-Z]$'::text)))
);

COMMENT ON COLUMN public.t_customer.version IS '乐观锁版本号；每次 UPDATE 自增；冲突抛 BIZ_VERSION_CONFLICT 409';
COMMENT ON COLUMN public.t_customer.serial_prefix IS '一级客户序列号前缀（A-Z）；叶子客户 NULL';

ALTER TABLE ONLY public.t_customer ADD CONSTRAINT t_customer_pkey PRIMARY KEY (id);

CREATE INDEX ix_t_customer_name ON public.t_customer USING btree (name);
CREATE INDEX ix_t_customer_parent_id ON public.t_customer USING btree (parent_id);
CREATE INDEX ix_t_customer_deleted_at ON public.t_customer USING btree (deleted_at);
CREATE UNIQUE INDEX uq_t_customer_root_prefix ON public.t_customer USING btree (serial_prefix)
    WHERE ((deleted_at IS NULL) AND (parent_id IS NULL) AND (serial_prefix IS NOT NULL));

------------------------------------------------------------------------
-- t_applicant
------------------------------------------------------------------------
CREATE TABLE public.t_applicant (
    id          bigint NOT NULL,
    name        character varying(50) NOT NULL,
    customer_id bigint NOT NULL,
    version     integer DEFAULT 0 NOT NULL,
    created_at  timestamp without time zone DEFAULT now() NOT NULL,
    created_by  bigint,
    updated_at  timestamp without time zone DEFAULT now() NOT NULL,
    updated_by  bigint,
    deleted_at  timestamp without time zone
);

COMMENT ON COLUMN public.t_applicant.name IS '申请人姓名';
COMMENT ON COLUMN public.t_applicant.customer_id IS '逻辑外键 → t_customer.id（一级客户）';
COMMENT ON COLUMN public.t_applicant.version IS '乐观锁版本号；每次 UPDATE 自增；冲突抛 BIZ_VERSION_CONFLICT 409';

CREATE SEQUENCE public.t_applicant_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.t_applicant_id_seq OWNED BY public.t_applicant.id;

ALTER TABLE ONLY public.t_applicant ALTER COLUMN id SET DEFAULT nextval('public.t_applicant_id_seq'::regclass);
ALTER TABLE ONLY public.t_applicant ADD CONSTRAINT t_applicant_pkey PRIMARY KEY (id);

CREATE INDEX ix_t_applicant_customer_id ON public.t_applicant USING btree (customer_id);
CREATE INDEX ix_t_applicant_name ON public.t_applicant USING btree (name);
CREATE INDEX ix_t_applicant_deleted_at ON public.t_applicant USING btree (deleted_at);
CREATE UNIQUE INDEX uq_t_applicant_name_customer_active ON public.t_applicant USING btree (name, customer_id)
    WHERE (deleted_at IS NULL);

------------------------------------------------------------------------
-- t_serial_counter  (PK is `prefix varchar(1)` — business key, NOT snowflake)
------------------------------------------------------------------------
CREATE TABLE public.t_serial_counter (
    prefix      character varying(1) NOT NULL,
    counter     bigint DEFAULT 0 NOT NULL,
    version     integer DEFAULT 0 NOT NULL,
    created_at  timestamp without time zone DEFAULT now() NOT NULL,
    created_by  bigint,
    updated_at  timestamp without time zone DEFAULT now() NOT NULL,
    updated_by  bigint,
    deleted_at  timestamp without time zone
);

COMMENT ON COLUMN public.t_serial_counter.version IS '乐观锁版本号；每次 UPDATE 自增；冲突抛 BIZ_VERSION_CONFLICT 409';

ALTER TABLE ONLY public.t_serial_counter ADD CONSTRAINT t_serial_counter_pkey PRIMARY KEY (prefix);