-- Migration 006: t_part_batch
-- Notes:
--   * Unique constraint is a plain UNIQUE(part_id, batch_no) — batch numbers are
--     allocated by the service per part, no need to make it partial.
--   * status varchar(20) WITHOUT CHECK (state machine lives in service).
--   * Phase E rewrite: aligned 1:1 with /Users/ren/Code/schema_ddl.sql (column order, COMMENT,
--     CREATE SEQUENCE ... OWNED BY, ALTER TABLE ... SET DEFAULT nextval).

CREATE TABLE public.t_part_batch (
    id                  bigint NOT NULL,
    part_id             bigint NOT NULL,
    batch_no            integer NOT NULL,
    quantity            integer NOT NULL,
    status              character varying(20) DEFAULT 'PENDING'::character varying NOT NULL,
    location            character varying(20),
    current_holder_id   bigint,
    next_process_id     bigint,
    placed_at           timestamp without time zone,
    delivery_note_id    bigint,
    parent_batch_id     bigint,
    version             integer DEFAULT 0 NOT NULL,
    created_at          timestamp without time zone DEFAULT now() NOT NULL,
    created_by          bigint,
    updated_at          timestamp without time zone DEFAULT now() NOT NULL,
    updated_by          bigint,
    deleted_at          timestamp without time zone,
    has_been_repaired   boolean DEFAULT false NOT NULL,
    CONSTRAINT uq_t_part_batch_part_no UNIQUE (part_id, batch_no)
);

COMMENT ON COLUMN public.t_part_batch.part_id IS '逻辑外键 → t_part.id';
COMMENT ON COLUMN public.t_part_batch.batch_no IS '工单内批次序号（1 起）';
COMMENT ON COLUMN public.t_part_batch.quantity IS '本批次数量';
COMMENT ON COLUMN public.t_part_batch.location IS 'OFFICE / PRODUCTION_SHELF / WORKER / INSPECTION_SHELF / OUTSOURCE_COMPANY';
COMMENT ON COLUMN public.t_part_batch.current_holder_id IS '多态 holder：shelf/worker/outsource_company';
COMMENT ON COLUMN public.t_part_batch.next_process_id IS '逻辑外键 → t_process.id';
COMMENT ON COLUMN public.t_part_batch.placed_at IS '批次首次进入 ON_SHELF 的时间';
COMMENT ON COLUMN public.t_part_batch.delivery_note_id IS '逻辑外键 → t_delivery_note.id';
COMMENT ON COLUMN public.t_part_batch.parent_batch_id IS '拆分谱系：源批次 id；根批次 NULL';
COMMENT ON COLUMN public.t_part_batch.version IS '乐观锁版本号；每次 UPDATE 自增；冲突抛 BIZ_VERSION_CONFLICT 409';
COMMENT ON COLUMN public.t_part_batch.has_been_repaired IS '本批次是否经历过返修（与 t_part.has_been_repaired 同步写入）';

CREATE SEQUENCE public.t_part_batch_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.t_part_batch_id_seq OWNED BY public.t_part_batch.id;

ALTER TABLE ONLY public.t_part_batch ALTER COLUMN id SET DEFAULT nextval('public.t_part_batch_id_seq'::regclass);
ALTER TABLE ONLY public.t_part_batch ADD CONSTRAINT t_part_batch_pkey PRIMARY KEY (id);

CREATE INDEX ix_t_part_batch_current_holder_id ON public.t_part_batch USING btree (current_holder_id);
CREATE INDEX ix_t_part_batch_deleted_at ON public.t_part_batch USING btree (deleted_at);
CREATE INDEX ix_t_part_batch_delivery_note_id ON public.t_part_batch USING btree (delivery_note_id);
CREATE INDEX ix_t_part_batch_location ON public.t_part_batch USING btree (location);
CREATE INDEX ix_t_part_batch_location_status_next_process ON public.t_part_batch USING btree (location, status, next_process_id);
CREATE INDEX ix_t_part_batch_next_process_id ON public.t_part_batch USING btree (next_process_id);
CREATE INDEX ix_t_part_batch_part_id ON public.t_part_batch USING btree (part_id);
CREATE INDEX ix_t_part_batch_placed_at ON public.t_part_batch USING btree (placed_at);
CREATE INDEX ix_t_part_batch_status ON public.t_part_batch USING btree (status);
CREATE INDEX ix_t_part_batch_status_holder ON public.t_part_batch USING btree (status, current_holder_id);