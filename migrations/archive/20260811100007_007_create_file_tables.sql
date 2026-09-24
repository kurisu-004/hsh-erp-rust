-- Migration 007: t_part_file
-- Notes:
--   * part_id is polymorphic — points to either t_part or t_assembly (no FK).
--   * uk_t_part_file_single covers DRAWING / 3D_MODEL / ASSEMBLY_MASTER / CAD_2D
--     (SETUP_SHEET is excluded — multiple setup sheets per part are allowed).
--   * uk_t_part_file_part_kind_sha catches exact-content duplicates per (part, kind).
--   * paired_file_id column links a G_CODE file with its SETUP_SHEET (bidirectional).
--   * Phase E rewrite: aligned 1:1 with /Users/ren/Code/schema_ddl.sql (column order, COMMENT,
--     CREATE SEQUENCE ... OWNED BY, ALTER TABLE ... SET DEFAULT nextval).
--   * Legacy tables t_drawing_file and t_cnc_program are now added in migration 011.

CREATE TABLE public.t_part_file (
    id                   bigint NOT NULL,
    part_id              bigint NOT NULL,
    kind                 character varying(20) NOT NULL,
    file_type            character varying(20) NOT NULL,
    object_key           character varying(500) NOT NULL,
    original_filename    character varying(255) NOT NULL,
    file_size            bigint NOT NULL,
    content_type         character varying(100) NOT NULL,
    upload_status        character varying(20) DEFAULT 'READY'::character varying NOT NULL,
    content_sha256       character(64),
    version              integer DEFAULT 0 NOT NULL,
    created_at           timestamp without time zone DEFAULT now() NOT NULL,
    created_by           bigint,
    updated_at           timestamp without time zone DEFAULT now() NOT NULL,
    updated_by           bigint,
    deleted_at           timestamp without time zone,
    paired_file_id       bigint,
    CONSTRAINT ck_t_part_file_kind CHECK (((kind)::text = ANY ((ARRAY['DRAWING'::character varying, '3D_MODEL'::character varying, 'G_CODE'::character varying, 'SETUP_SHEET'::character varying, 'ASSEMBLY_MASTER'::character varying, 'CAD_2D'::character varying])::text[])))
);

COMMENT ON COLUMN public.t_part_file.part_id IS 'polymorphic: t_part.id 或 t_assembly.id (kind=ASSEMBLY_MASTER)';
COMMENT ON COLUMN public.t_part_file.kind IS 'DRAWING / 3D_MODEL / G_CODE / SETUP_SHEET / ASSEMBLY_MASTER / CAD_2D';
COMMENT ON COLUMN public.t_part_file.file_type IS '扩展名大写（PDF / STEP / NC / ...），与 kind 配套';
COMMENT ON COLUMN public.t_part_file.object_key IS 'COS 对象 key';
COMMENT ON COLUMN public.t_part_file.upload_status IS 'PENDING / READY / FAILED';
COMMENT ON COLUMN public.t_part_file.content_sha256 IS 'SHA-256 hex of file bytes（去重用）；NULL = 未计算 / 历史记录';
COMMENT ON COLUMN public.t_part_file.version IS '乐观锁版本号；每次 UPDATE 自增；冲突抛 BIZ_VERSION_CONFLICT 409';
COMMENT ON COLUMN public.t_part_file.paired_file_id IS '关联的配对文件ID（G_CODE <-> SETUP_SHEET 双向关联）';

CREATE SEQUENCE public.t_part_file_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.t_part_file_id_seq OWNED BY public.t_part_file.id;

ALTER TABLE ONLY public.t_part_file ALTER COLUMN id SET DEFAULT nextval('public.t_part_file_id_seq'::regclass);
ALTER TABLE ONLY public.t_part_file ADD CONSTRAINT t_part_file_pkey PRIMARY KEY (id);

CREATE INDEX ix_t_part_file_created_at ON public.t_part_file USING btree (created_at);
CREATE INDEX ix_t_part_file_part_id ON public.t_part_file USING btree (part_id);
CREATE INDEX ix_t_part_file_part_kind ON public.t_part_file USING btree (part_id, kind);
CREATE UNIQUE INDEX uk_t_part_file_part_kind_sha ON public.t_part_file USING btree (part_id, kind, content_sha256)
    WHERE ((deleted_at IS NULL) AND (content_sha256 IS NOT NULL));
CREATE UNIQUE INDEX uk_t_part_file_single ON public.t_part_file USING btree (part_id, kind)
    WHERE ((deleted_at IS NULL) AND ((kind)::text = ANY ((ARRAY['DRAWING'::character varying, '3D_MODEL'::character varying, 'ASSEMBLY_MASTER'::character varying, 'CAD_2D'::character varying])::text[])));