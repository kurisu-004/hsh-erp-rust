-- Migration 010: t_part_event, t_outsource_quote_event,
--                 t_delivery_note_event, t_pickup_skip_event
-- Notes:
--   * Event tables carry ONLY `created_at` (no version / updated_at / deleted_at).
--   * t_delivery_note_event no longer has drawing_code / badge_code /
--     scanned_count / expected_count — those columns were dropped by migration 0013.
--   * t_part_event gained outsource_company_id (0018), batch_id and quantity (0020).
--   * Phase E rewrite: aligned 1:1 with /Users/ren/Code/schema_ddl.sql (column order, COMMENT,
--     CREATE SEQUENCE ... OWNED BY, ALTER TABLE ... SET DEFAULT nextval).
--   * NOTE: t_outsource_quote_event logically belongs to the outsource domain (008),
--     but is intentionally kept here per the existing per-file migration assignment.

------------------------------------------------------------------------
-- t_part_event
------------------------------------------------------------------------
CREATE TABLE public.t_part_event (
    id                     bigint NOT NULL,
    part_id                bigint NOT NULL,
    worker_id              bigint,
    event_type             character varying(30) NOT NULL,
    from_status            character varying(20),
    to_status              character varying(20),
    drawing_code           character varying(100),
    badge_code             character varying(50),
    note                   character varying(500),
    created_at             timestamp without time zone DEFAULT now() NOT NULL,
    created_by             bigint,
    outsource_company_id   bigint,
    batch_id               bigint,
    quantity               integer
);

COMMENT ON COLUMN public.t_part_event.created_by IS '操作者 t_user.id（NULL = 系统调度/历史数据）';
COMMENT ON COLUMN public.t_part_event.outsource_company_id IS 'SENT_TO_OUTSOURCE / RECEIVED_FROM_OUTSOURCE 时填入；外协对账按此列聚合';
COMMENT ON COLUMN public.t_part_event.batch_id IS '逻辑外键 → t_part_batch.id；NULL = 工单级事件';
COMMENT ON COLUMN public.t_part_event.quantity IS '本次事件涉及的数量；NULL = 历史数据 / 不适用';

CREATE SEQUENCE public.t_part_event_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.t_part_event_id_seq OWNED BY public.t_part_event.id;

ALTER TABLE ONLY public.t_part_event ALTER COLUMN id SET DEFAULT nextval('public.t_part_event_id_seq'::regclass);
ALTER TABLE ONLY public.t_part_event ADD CONSTRAINT t_part_event_pkey PRIMARY KEY (id);

CREATE INDEX ix_part_event_created_at ON public.t_part_event USING btree (created_at);
CREATE INDEX ix_part_event_event_type ON public.t_part_event USING btree (event_type);
CREATE INDEX ix_part_event_part_id ON public.t_part_event USING btree (part_id);
CREATE INDEX ix_part_event_worker_id ON public.t_part_event USING btree (worker_id);
CREATE INDEX ix_t_part_event_outsource_company_id ON public.t_part_event USING btree (outsource_company_id)
    WHERE (outsource_company_id IS NOT NULL);
CREATE INDEX ix_t_part_event_batch_id ON public.t_part_event USING btree (batch_id)
    WHERE (batch_id IS NOT NULL);

------------------------------------------------------------------------
-- t_outsource_quote_event
------------------------------------------------------------------------
CREATE TABLE public.t_outsource_quote_event (
    id           bigint NOT NULL,
    quote_id     bigint NOT NULL,
    event_type   character varying(32) NOT NULL,
    from_status  character varying(16),
    to_status    character varying(16),
    note         character varying(500),
    created_by   bigint,
    created_at   timestamp without time zone DEFAULT now() NOT NULL
);

COMMENT ON COLUMN public.t_outsource_quote_event.quote_id IS '逻辑外键 → t_outsource_quote.id';
COMMENT ON COLUMN public.t_outsource_quote_event.event_type IS 'CREATED / EDITED / SUBMITTED / APPROVED / REJECTED / USED';
COMMENT ON COLUMN public.t_outsource_quote_event.from_status IS '状态机前态';
COMMENT ON COLUMN public.t_outsource_quote_event.to_status IS '状态机后态';
COMMENT ON COLUMN public.t_outsource_quote_event.note IS '事件备注';
COMMENT ON COLUMN public.t_outsource_quote_event.created_by IS '操作人 user id';

CREATE SEQUENCE public.t_outsource_quote_event_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.t_outsource_quote_event_id_seq OWNED BY public.t_outsource_quote_event.id;

ALTER TABLE ONLY public.t_outsource_quote_event ALTER COLUMN id SET DEFAULT nextval('public.t_outsource_quote_event_id_seq'::regclass);
ALTER TABLE ONLY public.t_outsource_quote_event ADD CONSTRAINT t_outsource_quote_event_pkey PRIMARY KEY (id);

CREATE INDEX ix_t_outsource_quote_event_created_at ON public.t_outsource_quote_event USING btree (created_at);
CREATE INDEX ix_t_outsource_quote_event_quote_id ON public.t_outsource_quote_event USING btree (quote_id);

------------------------------------------------------------------------
-- t_delivery_note_event
------------------------------------------------------------------------
CREATE TABLE public.t_delivery_note_event (
    id                 bigint NOT NULL,
    delivery_note_id   bigint NOT NULL,
    event_type         character varying(32) NOT NULL,
    from_status        character varying(16),
    to_status          character varying(16),
    note               character varying(500),
    created_by         bigint,
    created_at         timestamp without time zone DEFAULT now() NOT NULL
);

COMMENT ON COLUMN public.t_delivery_note_event.delivery_note_id IS '逻辑外键 → t_delivery_note.id';
COMMENT ON COLUMN public.t_delivery_note_event.event_type IS 'CREATED / EDITED / ITEM_ADDED / ITEM_REMOVED / SUBMITTED / RECALLED / PICKUP_SCANNED / PICKED_UP / ARCHIVED';
COMMENT ON COLUMN public.t_delivery_note_event.note IS '事件备注 / 扩展元数据';
COMMENT ON COLUMN public.t_delivery_note_event.created_by IS '操作用户 t_user.id';

CREATE SEQUENCE public.t_delivery_note_event_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.t_delivery_note_event_id_seq OWNED BY public.t_delivery_note_event.id;

ALTER TABLE ONLY public.t_delivery_note_event ALTER COLUMN id SET DEFAULT nextval('public.t_delivery_note_event_id_seq'::regclass);
ALTER TABLE ONLY public.t_delivery_note_event ADD CONSTRAINT t_delivery_note_event_pkey PRIMARY KEY (id);

CREATE INDEX ix_t_delivery_note_event_created_at ON public.t_delivery_note_event USING btree (created_at);
CREATE INDEX ix_t_delivery_note_event_note_created ON public.t_delivery_note_event USING btree (delivery_note_id, created_at);
CREATE INDEX ix_t_delivery_note_event_note_id ON public.t_delivery_note_event USING btree (delivery_note_id);

------------------------------------------------------------------------
-- t_pickup_skip_event
------------------------------------------------------------------------
CREATE TABLE public.t_pickup_skip_event (
    id                              bigint NOT NULL,
    worker_id                       bigint NOT NULL,
    part_id                         bigint NOT NULL,
    batch_id                        bigint NOT NULL,
    batch_no                        integer NOT NULL,
    part_serial_no                  character varying(100),
    shelf_id                        bigint NOT NULL,
    work_type_id                    bigint,
    quantity                        integer NOT NULL,
    part_planned_delivery_date      date,
    skipped_earliest_date           date,
    created_at                      timestamp without time zone DEFAULT now() NOT NULL
);

COMMENT ON COLUMN public.t_pickup_skip_event.worker_id IS '逻辑外键 → t_worker.id；触发跳序的工人';
COMMENT ON COLUMN public.t_pickup_skip_event.part_id IS '逻辑外键 → t_part.id；本次实际领取的工单';
COMMENT ON COLUMN public.t_pickup_skip_event.batch_id IS '逻辑外键 → t_part_batch.id；记录实际领取批次（拆分后为新批次）';
COMMENT ON COLUMN public.t_pickup_skip_event.batch_no IS '快照：领取批次号';
COMMENT ON COLUMN public.t_pickup_skip_event.part_serial_no IS '快照：工单流水号（流水号会被释放复用，必须快照）';
COMMENT ON COLUMN public.t_pickup_skip_event.shelf_id IS '取件货架 t_shelf.id';
COMMENT ON COLUMN public.t_pickup_skip_event.work_type_id IS '工人当时工种 t_work_type.id 快照';
COMMENT ON COLUMN public.t_pickup_skip_event.quantity IS '本次领取数量';
COMMENT ON COLUMN public.t_pickup_skip_event.part_planned_delivery_date IS '所取件计划交期；NULL 表示无交期';
COMMENT ON COLUMN public.t_pickup_skip_event.skipped_earliest_date IS '被跳过的候选件中最早交期；NULL 表示无可比候选';

CREATE SEQUENCE public.t_pickup_skip_event_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.t_pickup_skip_event_id_seq OWNED BY public.t_pickup_skip_event.id;

ALTER TABLE ONLY public.t_pickup_skip_event ALTER COLUMN id SET DEFAULT nextval('public.t_pickup_skip_event_id_seq'::regclass);
ALTER TABLE ONLY public.t_pickup_skip_event ADD CONSTRAINT t_pickup_skip_event_pkey PRIMARY KEY (id);

CREATE INDEX ix_t_pickup_skip_event_created_at ON public.t_pickup_skip_event USING btree (created_at);
CREATE INDEX ix_t_pickup_skip_event_worker_id ON public.t_pickup_skip_event USING btree (worker_id);