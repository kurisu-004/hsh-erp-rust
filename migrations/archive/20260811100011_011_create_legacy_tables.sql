-- Migration 011: legacy tables t_drawing_file + t_cnc_program
-- Notes:
--   * Both tables are legacy — superseded by t_part_file (kind=DRAWING / kind=G_CODE).
--     They have no business writes today, but the schema must remain in production to
--     preserve historical data.
--   * Phase E rewrite: aligned 1:1 with /Users/ren/Code/schema_ddl.sql:447-507
--     (column order, COMMENT, CREATE SEQUENCE ... OWNED BY, ALTER TABLE ... SET DEFAULT nextval).
--   * No CHECK constraints, no partial unique INDEX, no other indexes — kept verbatim.

------------------------------------------------------------------------
-- t_drawing_file
------------------------------------------------------------------------
CREATE TABLE public.t_drawing_file (
    id                    bigint NOT NULL,
    part_id               bigint NOT NULL,
    object_key            character varying(500) NOT NULL,
    original_filename     character varying(255) NOT NULL,
    file_size             bigint NOT NULL,
    content_type          character varying(100) NOT NULL,
    upload_status         character varying(20) DEFAULT 'READY'::character varying NOT NULL,
    version               integer DEFAULT 0 NOT NULL,
    created_at            timestamp without time zone DEFAULT now() NOT NULL,
    created_by            bigint,
    updated_at            timestamp without time zone DEFAULT now() NOT NULL,
    updated_by            bigint,
    deleted_at            timestamp without time zone
);

COMMENT ON COLUMN public.t_drawing_file.version IS '乐观锁版本号；每次 UPDATE 自增；冲突抛 BIZ_VERSION_CONFLICT 409';

CREATE SEQUENCE public.t_drawing_file_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.t_drawing_file_id_seq OWNED BY public.t_drawing_file.id;

ALTER TABLE ONLY public.t_drawing_file ALTER COLUMN id SET DEFAULT nextval('public.t_drawing_file_id_seq'::regclass);
ALTER TABLE ONLY public.t_drawing_file ADD CONSTRAINT t_drawing_file_pkey PRIMARY KEY (id);

------------------------------------------------------------------------
-- t_cnc_program
------------------------------------------------------------------------
CREATE TABLE public.t_cnc_program (
    id                    bigint NOT NULL,
    part_id               bigint NOT NULL,
    object_key            character varying(500) NOT NULL,
    original_filename     character varying(255) NOT NULL,
    file_size             bigint NOT NULL,
    content_type          character varying(100) NOT NULL,
    upload_status         character varying(20) DEFAULT 'READY'::character varying NOT NULL,
    version               integer DEFAULT 0 NOT NULL,
    created_at            timestamp without time zone DEFAULT now() NOT NULL,
    created_by            bigint,
    updated_at            timestamp without time zone DEFAULT now() NOT NULL,
    updated_by            bigint,
    deleted_at            timestamp without time zone
);

COMMENT ON COLUMN public.t_cnc_program.version IS '乐观锁版本号；每次 UPDATE 自增；冲突抛 BIZ_VERSION_CONFLICT 409';

CREATE SEQUENCE public.t_cnc_program_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.t_cnc_program_id_seq OWNED BY public.t_cnc_program.id;

ALTER TABLE ONLY public.t_cnc_program ALTER COLUMN id SET DEFAULT nextval('public.t_cnc_program_id_seq'::regclass);
ALTER TABLE ONLY public.t_cnc_program ADD CONSTRAINT t_cnc_program_pkey PRIMARY KEY (id);