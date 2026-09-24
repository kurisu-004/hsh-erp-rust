-- Migration 005: t_assembly, t_part
-- Notes:
--   * t_assembly.status and t_part.status are varchar(20) WITHOUT CHECK constraint
--     (state machine lives in src/modules/{assembly,part}/statemachine.rs).
--   * t_part is the largest table — final-state columns include
--     order_no, system_delivery_date, note, delivery_note_id, has_been_repaired
--     merged from later ALTER migrations.
--   * Phase E rewrite: aligned 1:1 with /Users/ren/Code/schema_ddl.sql (column order, COMMENT,
--     CREATE SEQUENCE ... OWNED BY, ALTER TABLE ... SET DEFAULT nextval).

------------------------------------------------------------------------
-- t_assembly
------------------------------------------------------------------------
CREATE TABLE public.t_assembly (
    id                     bigint NOT NULL,
    drawing_no             character varying(100) NOT NULL,
    name                   character varying(200) NOT NULL,
    applicant_name         character varying(50),
    customer_id            bigint NOT NULL,
    request_date           date NOT NULL,
    planned_delivery_date  date NOT NULL,
    actual_delivery_date   date,
    is_urgent              boolean DEFAULT false NOT NULL,
    status                 character varying(20) DEFAULT 'PENDING'::character varying NOT NULL,
    version                integer DEFAULT 0 NOT NULL,
    created_at             timestamp without time zone DEFAULT now() NOT NULL,
    created_by             bigint,
    updated_at             timestamp without time zone DEFAULT now() NOT NULL,
    updated_by             bigint,
    deleted_at             timestamp without time zone,
    serial_no              character varying(8),
    quantity               integer DEFAULT 1 NOT NULL,
    unit_price             numeric(12,2) DEFAULT 0 NOT NULL,
    total_price            numeric(14,2) DEFAULT 0 NOT NULL,
    order_no               character varying(30),
    system_delivery_date   date,
    note                   character varying(500)
);

COMMENT ON COLUMN public.t_assembly.drawing_no IS '总图图号（如 E42FX1020107101）';
COMMENT ON COLUMN public.t_assembly.name IS '装配体名称（如 精研挡料座）';
COMMENT ON COLUMN public.t_assembly.customer_id IS '逻辑外键 → t_customer.id 叶子节点';
COMMENT ON COLUMN public.t_assembly.status IS 'PENDING（默认）/ IN_PROCESS / COMPLETED / CANCELLED';
COMMENT ON COLUMN public.t_assembly.version IS '乐观锁版本号；每次 UPDATE 自增；冲突抛 BIZ_VERSION_CONFLICT 409';
COMMENT ON COLUMN public.t_assembly.serial_no IS '装配体序列号；子件派生为 ''{serial_no}-{i:02d}''';

-- t_assembly.id has NO snowflake SEQUENCE in production — id is supplied by application code
-- (the previous Rust migration also omitted DEFAULT). Keeping verbatim with schema_ddl.sql.

ALTER TABLE ONLY public.t_assembly ADD CONSTRAINT t_assembly_pkey PRIMARY KEY (id);

CREATE INDEX ix_t_assembly_customer_id ON public.t_assembly USING btree (customer_id);
CREATE INDEX ix_t_assembly_customer_status ON public.t_assembly USING btree (customer_id, status);
CREATE INDEX ix_t_assembly_deleted_at ON public.t_assembly USING btree (deleted_at);
CREATE INDEX ix_t_assembly_drawing_no ON public.t_assembly USING btree (drawing_no);
CREATE INDEX ix_t_assembly_order_no ON public.t_assembly USING btree (order_no);
CREATE INDEX ix_t_assembly_planned_delivery ON public.t_assembly USING btree (planned_delivery_date);
CREATE INDEX ix_t_assembly_serial_no ON public.t_assembly USING btree (serial_no);
CREATE INDEX ix_t_assembly_status ON public.t_assembly USING btree (status);
CREATE UNIQUE INDEX uk_t_assembly_serial_no ON public.t_assembly USING btree (serial_no)
    WHERE ((deleted_at IS NULL) AND (serial_no IS NOT NULL));

------------------------------------------------------------------------
-- t_part
------------------------------------------------------------------------
CREATE TABLE public.t_part (
    id                     bigint NOT NULL,
    serial_no              character varying(8),
    name                   character varying(200) NOT NULL,
    drawing_no             character varying(100) NOT NULL,
    applicant_name         character varying(50) NOT NULL,
    quantity               integer DEFAULT 1 NOT NULL,
    unit_price             numeric(12,2) DEFAULT 0 NOT NULL,
    total_price            numeric(14,2) DEFAULT 0 NOT NULL,
    request_date           date NOT NULL,
    planned_delivery_date  date NOT NULL,
    actual_delivery_date   date,
    status                 character varying(20) DEFAULT 'PENDING'::character varying NOT NULL,
    location               character varying(20),
    is_urgent              boolean DEFAULT false NOT NULL,
    current_holder_id      bigint,
    placed_at              timestamp without time zone,
    next_process_id        bigint,
    customer_id            bigint NOT NULL,
    assembly_id            bigint,
    version                integer DEFAULT 0 NOT NULL,
    created_at             timestamp without time zone DEFAULT now() NOT NULL,
    created_by             bigint,
    updated_at             timestamp without time zone DEFAULT now() NOT NULL,
    updated_by             bigint,
    deleted_at             timestamp without time zone,
    order_no               character varying(30),
    system_delivery_date   date,
    note                   character varying(500),
    delivery_note_id       bigint,
    has_been_repaired      boolean DEFAULT false NOT NULL
);

COMMENT ON COLUMN public.t_part.location IS '零件物理位置: OFFICE / PRODUCTION_SHELF / WORKER / INSPECTION_SHELF';
COMMENT ON COLUMN public.t_part.is_urgent IS '是否加急';
COMMENT ON COLUMN public.t_part.current_holder_id IS '多态 holder → t_worker.id 或 t_shelf.id';
COMMENT ON COLUMN public.t_part.placed_at IS 'PENDING→IN_PROCESS 时置位';
COMMENT ON COLUMN public.t_part.next_process_id IS '逻辑外键 → t_process.id；place_on_shelf / RETURNED 时更新';
COMMENT ON COLUMN public.t_part.assembly_id IS '逻辑外键 → t_assembly.id；NULL = 非装配件子件';
COMMENT ON COLUMN public.t_part.version IS '乐观锁版本号；每次 UPDATE 自增；冲突抛 BIZ_VERSION_CONFLICT 409';
COMMENT ON COLUMN public.t_part.delivery_note_id IS '逻辑外键 → t_delivery_note.id（active 状态下最多 1 张）';
COMMENT ON COLUMN public.t_part.has_been_repaired IS '该工单是否经历过返修（返修件标识，贯穿到 COMPLETED/CANCELLED）';

CREATE SEQUENCE public.t_part_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.t_part_id_seq OWNED BY public.t_part.id;

ALTER TABLE ONLY public.t_part ALTER COLUMN id SET DEFAULT nextval('public.t_part_id_seq'::regclass);
ALTER TABLE ONLY public.t_part ADD CONSTRAINT t_part_pkey PRIMARY KEY (id);

CREATE INDEX ix_t_part_assembly_id ON public.t_part USING btree (assembly_id);
CREATE INDEX ix_t_part_assembly_id_status ON public.t_part USING btree (assembly_id, status);
CREATE INDEX ix_t_part_current_holder_id ON public.t_part USING btree (current_holder_id);
CREATE INDEX ix_t_part_customer_id ON public.t_part USING btree (customer_id);
CREATE INDEX ix_t_part_customer_status_delivery ON public.t_part USING btree (customer_id, status, planned_delivery_date);
CREATE INDEX ix_t_part_deleted_at ON public.t_part USING btree (deleted_at);
CREATE INDEX ix_t_part_delivery_note_id ON public.t_part USING btree (delivery_note_id);
CREATE INDEX ix_t_part_drawing_no ON public.t_part USING btree (drawing_no);
CREATE INDEX ix_t_part_is_urgent ON public.t_part USING btree (is_urgent);
CREATE INDEX ix_t_part_location ON public.t_part USING btree (location);
CREATE INDEX ix_t_part_location_status_next_process ON public.t_part USING btree (location, status, next_process_id);
CREATE INDEX ix_t_part_name ON public.t_part USING btree (name);
CREATE INDEX ix_t_part_next_process_id ON public.t_part USING btree (next_process_id);
CREATE INDEX ix_t_part_order_no ON public.t_part USING btree (order_no);
CREATE INDEX ix_t_part_placed_at ON public.t_part USING btree (placed_at);
CREATE INDEX ix_t_part_planned_delivery_date ON public.t_part USING btree (planned_delivery_date);
CREATE INDEX ix_t_part_request_date ON public.t_part USING btree (request_date);
CREATE INDEX ix_t_part_status ON public.t_part USING btree (status);
CREATE INDEX ix_t_part_status_holder ON public.t_part USING btree (status, current_holder_id);
CREATE UNIQUE INDEX uk_t_part_serial_no ON public.t_part USING btree (serial_no) WHERE (serial_no IS NOT NULL);