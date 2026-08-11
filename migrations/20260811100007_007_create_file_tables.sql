-- Migration 007: t_part_file
-- Notes:
--   * part_id is polymorphic — points to either t_part or t_assembly (no FK).
--   * Legacy tables t_drawing_file and t_cnc_program are intentionally SKIPPED;
--     drawing + G-code lives in this table (kind = DRAWING / G_CODE).
--   * uk_t_part_file_single covers DRAWING / 3D_MODEL / ASSEMBLY_MASTER / CAD_2D
--     (SETUP_SHEET is excluded — multiple setup sheets per part are allowed).
--   * uk_t_part_file_part_kind_sha catches exact-content duplicates per (part, kind).

CREATE TABLE t_part_file (
    id                   BIGINT        PRIMARY KEY,
    part_id              BIGINT        NOT NULL,
    kind                 VARCHAR(20)   NOT NULL,
    file_type            VARCHAR(20)   NOT NULL,
    object_key           VARCHAR(500)  NOT NULL,
    original_filename    VARCHAR(255)  NOT NULL,
    file_size            BIGINT        NOT NULL,
    content_type         VARCHAR(100)  NOT NULL,
    upload_status        VARCHAR(20)   NOT NULL DEFAULT 'READY',
    content_sha256       CHAR(64),
    paired_file_id       BIGINT,
    version              INT           NOT NULL DEFAULT 0,
    created_at           timestamp     NOT NULL DEFAULT now(),
    created_by           BIGINT,
    updated_at           timestamp     NOT NULL DEFAULT now(),
    updated_by           BIGINT,
    deleted_at           timestamp,
    CONSTRAINT ck_t_part_file_kind CHECK (kind IN ('DRAWING', '3D_MODEL', 'G_CODE', 'SETUP_SHEET', 'ASSEMBLY_MASTER', 'CAD_2D'))
);

CREATE INDEX ix_t_part_file_part_id
    ON t_part_file (part_id);

CREATE INDEX ix_t_part_file_part_kind
    ON t_part_file (part_id, kind);

CREATE INDEX ix_t_part_file_created_at
    ON t_part_file (created_at);

CREATE UNIQUE INDEX uk_t_part_file_single
    ON t_part_file (part_id, kind)
    WHERE deleted_at IS NULL
      AND kind IN ('DRAWING', '3D_MODEL', 'ASSEMBLY_MASTER', 'CAD_2D');

CREATE UNIQUE INDEX uk_t_part_file_part_kind_sha
    ON t_part_file (part_id, kind, content_sha256)
    WHERE deleted_at IS NULL
      AND content_sha256 IS NOT NULL;
