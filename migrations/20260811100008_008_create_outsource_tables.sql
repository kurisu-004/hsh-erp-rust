-- Migration 008: t_outsource_quote, t_outsource_shipment
-- Notes:
--   * t_outsource_quote.status CHECK keeps the 8-value list from migration 0019.
--   * t_outsource_quote.price CHECK is `>= 0` (relaxed — 0 means "to be negotiated").
--   * uq_t_outsource_quote_approved_part_process enforces one APPROVED+direct=false
--     quote per (part, process); rebuilt by migration 0022.
--   * Phase E rewrite: aligned 1:1 with /Users/ren/Code/schema_ddl.sql (column order, COMMENT,
--     CREATE SEQUENCE ... OWNED BY, ALTER TABLE ... SET DEFAULT nextval).
--   * NOTE: t_outsource_quote_event is intentionally left to migration 010 (per-file
--     assignment in the Phase E plan), even though the table logically belongs to this domain.

------------------------------------------------------------------------
-- t_outsource_quote
------------------------------------------------------------------------
CREATE TABLE public.t_outsource_quote (
    id                    bigint NOT NULL,
    part_id               bigint NOT NULL,
    outsource_company_id  bigint NOT NULL,
    process_id            bigint NOT NULL,
    price                 numeric(12,2) NOT NULL,
    note                  character varying(500),
    status                character varying(16) DEFAULT 'DRAFT'::character varying NOT NULL,
    submitted_at          timestamp without time zone,
    reviewed_at           timestamp without time zone,
    review_note           character varying(500),
    version               integer DEFAULT 0 NOT NULL,
    created_at            timestamp without time zone DEFAULT now() NOT NULL,
    created_by            bigint,
    updated_at            timestamp without time zone DEFAULT now() NOT NULL,
    updated_by            bigint,
    deleted_at            timestamp without time zone,
    sent_at               timestamp without time zone,
    received_at           timestamp without time zone,
    quantity              integer,
    is_billed             boolean DEFAULT false NOT NULL,
    is_direct             boolean DEFAULT false NOT NULL,
    CONSTRAINT ck_t_outsource_quote_price_positive CHECK ((price >= (0)::numeric)),
    CONSTRAINT ck_t_outsource_quote_status CHECK (((status)::text = ANY ((ARRAY['DRAFT'::character varying, 'SUBMITTED'::character varying, 'APPROVED'::character varying, 'REJECTED'::character varying, 'OUTSOURCING'::character varying, 'RECEIVED'::character varying, 'BILLED'::character varying, 'USED'::character varying])::text[])))
);

COMMENT ON COLUMN public.t_outsource_quote.part_id IS '逻辑外键 → t_part.id';
COMMENT ON COLUMN public.t_outsource_quote.outsource_company_id IS '逻辑外键 → t_outsource_company.id';
COMMENT ON COLUMN public.t_outsource_quote.process_id IS '逻辑外键 → t_process.id（必须 OUTSOURCE 类别）';
COMMENT ON COLUMN public.t_outsource_quote.price IS '单件单价（CNY）';
COMMENT ON COLUMN public.t_outsource_quote.note IS '备注';
COMMENT ON COLUMN public.t_outsource_quote.status IS 'DRAFT / SUBMITTED / APPROVED / REJECTED / USED';
COMMENT ON COLUMN public.t_outsource_quote.review_note IS '审批意见（reject 必填）';
COMMENT ON COLUMN public.t_outsource_quote.version IS '乐观锁版本号；每次 UPDATE 自增；冲突抛 BIZ_VERSION_CONFLICT 409';
COMMENT ON COLUMN public.t_outsource_quote.sent_at IS '发送时间（send_to_outsource 触发时写入）';
COMMENT ON COLUMN public.t_outsource_quote.received_at IS '接收时间（receive_from_outsource 触发时写入）';
COMMENT ON COLUMN public.t_outsource_quote.quantity IS '本次发送数量 snapshot；可能与 t_part.quantity 不同';
COMMENT ON COLUMN public.t_outsource_quote.is_billed IS '对账标记（与状态 RECEIVED/BILLED 配套）';

CREATE SEQUENCE public.t_outsource_quote_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.t_outsource_quote_id_seq OWNED BY public.t_outsource_quote.id;

ALTER TABLE ONLY public.t_outsource_quote ALTER COLUMN id SET DEFAULT nextval('public.t_outsource_quote_id_seq'::regclass);
ALTER TABLE ONLY public.t_outsource_quote ADD CONSTRAINT t_outsource_quote_pkey PRIMARY KEY (id);

CREATE INDEX ix_t_outsource_quote_company_received_at ON public.t_outsource_quote USING btree (outsource_company_id, received_at);
CREATE INDEX ix_t_outsource_quote_company_sent_at ON public.t_outsource_quote USING btree (outsource_company_id, sent_at);
CREATE INDEX ix_t_outsource_quote_deleted_at ON public.t_outsource_quote USING btree (deleted_at);
CREATE INDEX ix_t_outsource_quote_outsource_company_id ON public.t_outsource_quote USING btree (outsource_company_id);
CREATE INDEX ix_t_outsource_quote_part_id ON public.t_outsource_quote USING btree (part_id);
CREATE INDEX ix_t_outsource_quote_process_id ON public.t_outsource_quote USING btree (process_id);
CREATE INDEX ix_t_outsource_quote_status ON public.t_outsource_quote USING btree (status);
CREATE UNIQUE INDEX uq_t_outsource_quote_approved_part_process ON public.t_outsource_quote USING btree (part_id, process_id)
    WHERE ((deleted_at IS NULL) AND ((status)::text = 'APPROVED'::text) AND (is_direct = false));

------------------------------------------------------------------------
-- t_outsource_shipment
------------------------------------------------------------------------
CREATE TABLE public.t_outsource_shipment (
    id                    bigint NOT NULL,
    quote_id              bigint NOT NULL,
    part_id               bigint NOT NULL,
    batch_id              bigint,
    outsource_company_id  bigint NOT NULL,
    process_id            bigint NOT NULL,
    quantity              integer NOT NULL,
    unit_price            numeric(12,2) NOT NULL,
    status                character varying(16) DEFAULT 'OUTSOURCING'::character varying NOT NULL,
    sent_at               timestamp without time zone NOT NULL,
    received_at           timestamp without time zone,
    is_billed             boolean DEFAULT false NOT NULL,
    version               integer DEFAULT 0 NOT NULL,
    created_at            timestamp without time zone DEFAULT now() NOT NULL,
    created_by            bigint,
    updated_at            timestamp without time zone DEFAULT now() NOT NULL,
    updated_by            bigint,
    deleted_at            timestamp without time zone,
    CONSTRAINT ck_t_outsource_shipment_quantity_positive CHECK ((quantity > 0)),
    CONSTRAINT ck_t_outsource_shipment_status CHECK (((status)::text = ANY ((ARRAY['OUTSOURCING'::character varying, 'RECEIVED'::character varying, 'CANCELLED'::character varying])::text[])))
);

CREATE SEQUENCE public.t_outsource_shipment_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.t_outsource_shipment_id_seq OWNED BY public.t_outsource_shipment.id;

ALTER TABLE ONLY public.t_outsource_shipment ALTER COLUMN id SET DEFAULT nextval('public.t_outsource_shipment_id_seq'::regclass);
ALTER TABLE ONLY public.t_outsource_shipment ADD CONSTRAINT t_outsource_shipment_pkey PRIMARY KEY (id);

CREATE INDEX ix_t_outsource_shipment_batch_id ON public.t_outsource_shipment USING btree (batch_id);
CREATE INDEX ix_t_outsource_shipment_outsource_company_id ON public.t_outsource_shipment USING btree (outsource_company_id);
CREATE INDEX ix_t_outsource_shipment_part_id ON public.t_outsource_shipment USING btree (part_id);
CREATE INDEX ix_t_outsource_shipment_process_id ON public.t_outsource_shipment USING btree (process_id);
CREATE INDEX ix_t_outsource_shipment_quote_id ON public.t_outsource_shipment USING btree (quote_id);
CREATE INDEX ix_t_outsource_shipment_status ON public.t_outsource_shipment USING btree (status);
CREATE UNIQUE INDEX uq_t_outsource_shipment_open_batch ON public.t_outsource_shipment USING btree (batch_id)
    WHERE ((deleted_at IS NULL) AND ((status)::text = 'OUTSOURCING'::text));