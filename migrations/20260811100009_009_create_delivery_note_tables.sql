-- Migration 009: t_delivery_note, t_delivery_note_counter
-- Notes:
--   * t_delivery_note_counter has only `created_at` + `updated_at`; PK is
--     `date_ymd varchar(8)` — no snowflake id, no AuditMixin.
--   * t_delivery_note.delivery_date was added by migration 0010.
--   * Phase E rewrite: aligned 1:1 with /Users/ren/Code/schema_ddl.sql (column order, COMMENT,
--     CREATE SEQUENCE ... OWNED BY, ALTER TABLE ... SET DEFAULT nextval).

------------------------------------------------------------------------
-- t_delivery_note_counter  (business-day sequence)
------------------------------------------------------------------------
CREATE TABLE public.t_delivery_note_counter (
    date_ymd    character varying(8) NOT NULL,
    last_value  integer DEFAULT 0 NOT NULL,
    created_at  timestamp without time zone DEFAULT now() NOT NULL,
    updated_at  timestamp without time zone DEFAULT now() NOT NULL
);

COMMENT ON COLUMN public.t_delivery_note_counter.date_ymd IS '自然日，格式 YYYYMMDD';
COMMENT ON COLUMN public.t_delivery_note_counter.last_value IS '当日已发放的最大序列号（0 起；NNN 段从 1 开始）';

ALTER TABLE ONLY public.t_delivery_note_counter ADD CONSTRAINT t_delivery_note_counter_pkey PRIMARY KEY (date_ymd);

------------------------------------------------------------------------
-- t_delivery_note
------------------------------------------------------------------------
CREATE TABLE public.t_delivery_note (
    id                  bigint NOT NULL,
    delivery_note_no    character varying(16) NOT NULL,
    customer_id         bigint NOT NULL,
    status              character varying(16) DEFAULT 'DRAFT'::character varying NOT NULL,
    submitted_at        timestamp without time zone,
    picked_up_at        timestamp without time zone,
    submitted_by        bigint,
    picked_up_by        bigint,
    driver_worker_id    bigint,
    note                character varying(500),
    version             integer DEFAULT 0 NOT NULL,
    created_at          timestamp without time zone DEFAULT now() NOT NULL,
    created_by          bigint,
    updated_at          timestamp without time zone DEFAULT now() NOT NULL,
    updated_by          bigint,
    deleted_at          timestamp without time zone,
    delivery_date       date,
    CONSTRAINT ck_t_delivery_note_status CHECK (((status)::text = ANY ((ARRAY['DRAFT'::character varying, 'SUBMITTED'::character varying, 'PICKED_UP'::character varying, 'ARCHIVED'::character varying])::text[])))
);

COMMENT ON COLUMN public.t_delivery_note.delivery_note_no IS '单号，格式 DN-YYYYMMDD-NNNN（4 位数）；唯一约束见 uq_t_delivery_note_no_active';
COMMENT ON COLUMN public.t_delivery_note.customer_id IS '逻辑外键 → t_customer.id；送货单的客户（叶子二级）';
COMMENT ON COLUMN public.t_delivery_note.status IS 'DRAFT / SUBMITTED / PICKED_UP / ARCHIVED';
COMMENT ON COLUMN public.t_delivery_note.submitted_by IS '提交人 t_user.id（MANAGER / CLERK）';
COMMENT ON COLUMN public.t_delivery_note.picked_up_by IS '领取时登入账号 t_user.id（一般是 SHELF_ACCOUNT 等扫码台账号）';
COMMENT ON COLUMN public.t_delivery_note.driver_worker_id IS '司机 t_worker.id；必须是 work_type.code=''送货司机'' 的活跃工人';
COMMENT ON COLUMN public.t_delivery_note.note IS '备注';
COMMENT ON COLUMN public.t_delivery_note.version IS '乐观锁版本号；每次 UPDATE 自增；冲突抛 BIZ_VERSION_CONFLICT 409';
COMMENT ON COLUMN public.t_delivery_note.delivery_date IS '送货日期；默认 = 创建当天；DRAFT/SUBMITTED 可改';

CREATE SEQUENCE public.t_delivery_note_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.t_delivery_note_id_seq OWNED BY public.t_delivery_note.id;

ALTER TABLE ONLY public.t_delivery_note ALTER COLUMN id SET DEFAULT nextval('public.t_delivery_note_id_seq'::regclass);
ALTER TABLE ONLY public.t_delivery_note ADD CONSTRAINT t_delivery_note_pkey PRIMARY KEY (id);

CREATE UNIQUE INDEX uq_t_delivery_note_no_active ON public.t_delivery_note USING btree (delivery_note_no)
    WHERE (deleted_at IS NULL);
CREATE INDEX ix_t_delivery_note_customer_id ON public.t_delivery_note USING btree (customer_id);
CREATE INDEX ix_t_delivery_note_deleted_at ON public.t_delivery_note USING btree (deleted_at);
CREATE INDEX ix_t_delivery_note_status ON public.t_delivery_note USING btree (status);
CREATE INDEX ix_t_delivery_note_submitted_at ON public.t_delivery_note USING btree (submitted_at);