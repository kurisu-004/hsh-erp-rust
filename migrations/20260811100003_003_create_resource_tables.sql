-- Migration 003: t_work_type, t_process, t_shelf, t_worker
-- Notes:
--   * t_process.category uses CHECK (INHOUSE/OUTSOURCE) — soft enum.
--   * t_worker.id_card_no has a partial unique (only when NOT NULL).

------------------------------------------------------------------------
-- t_work_type
------------------------------------------------------------------------
CREATE TABLE t_work_type (
    id                 BIGINT       PRIMARY KEY,
    code               VARCHAR(32)  NOT NULL,
    name               VARCHAR(50)  NOT NULL,
    description        VARCHAR(200),
    sort_order         INT          NOT NULL DEFAULT 0,
    max_held_batches   INT,
    version            INT          NOT NULL DEFAULT 0,
    created_at         timestamp    NOT NULL DEFAULT now(),
    created_by         BIGINT,
    updated_at         timestamp    NOT NULL DEFAULT now(),
    updated_by         BIGINT,
    deleted_at         timestamp
);

CREATE UNIQUE INDEX uk_t_work_type_code
    ON t_work_type (code)
    WHERE deleted_at IS NULL;

CREATE INDEX ix_t_work_type_code
    ON t_work_type (code);

CREATE INDEX ix_t_work_type_deleted_at
    ON t_work_type (deleted_at);

------------------------------------------------------------------------
-- t_process
------------------------------------------------------------------------
CREATE TABLE t_process (
    id                   BIGINT       PRIMARY KEY,
    code                 VARCHAR(32)  NOT NULL,
    name                 VARCHAR(50)  NOT NULL,
    category             VARCHAR(16)  NOT NULL,
    sort_order           INT          NOT NULL DEFAULT 0,
    description          VARCHAR(200),
    requires_approval    BOOLEAN      NOT NULL DEFAULT TRUE,
    version              INT          NOT NULL DEFAULT 0,
    created_at           timestamp    NOT NULL DEFAULT now(),
    created_by           BIGINT,
    updated_at           timestamp    NOT NULL DEFAULT now(),
    updated_by           BIGINT,
    deleted_at           timestamp,
    CONSTRAINT ck_t_process_category CHECK (category IN ('INHOUSE', 'OUTSOURCE'))
);

CREATE UNIQUE INDEX uk_t_process_code
    ON t_process (code)
    WHERE deleted_at IS NULL;

CREATE INDEX ix_t_process_code
    ON t_process (code);

CREATE INDEX ix_t_process_category
    ON t_process (category);

CREATE INDEX ix_t_process_deleted_at
    ON t_process (deleted_at);

------------------------------------------------------------------------
-- t_shelf
------------------------------------------------------------------------
CREATE TABLE t_shelf (
    id             BIGINT       PRIMARY KEY,
    code           VARCHAR(32)  NOT NULL,
    name           VARCHAR(100) NOT NULL,
    zone           VARCHAR(16)  NOT NULL,
    location       VARCHAR(200),
    is_active      BOOLEAN      NOT NULL DEFAULT TRUE,
    display_order  INT          NOT NULL DEFAULT 0,
    version        INT          NOT NULL DEFAULT 0,
    created_at     timestamp    NOT NULL DEFAULT now(),
    created_by     BIGINT,
    updated_at     timestamp    NOT NULL DEFAULT now(),
    updated_by     BIGINT,
    deleted_at     timestamp
);

CREATE UNIQUE INDEX uk_t_shelf_code
    ON t_shelf (code)
    WHERE deleted_at IS NULL;

CREATE INDEX ix_t_shelf_zone
    ON t_shelf (zone);

CREATE INDEX ix_t_shelf_deleted_at
    ON t_shelf (deleted_at);

CREATE INDEX ix_t_shelf_display_order
    ON t_shelf (display_order, code)
    WHERE deleted_at IS NULL;

------------------------------------------------------------------------
-- t_worker
------------------------------------------------------------------------
CREATE TABLE t_worker (
    id            BIGINT       PRIMARY KEY,
    badge_code    VARCHAR(50)  NOT NULL,
    name          VARCHAR(50)  NOT NULL,
    id_card_no    VARCHAR(18),
    phone         VARCHAR(20),
    is_active     BOOLEAN      NOT NULL DEFAULT TRUE,
    work_type_id  BIGINT,
    version       INT          NOT NULL DEFAULT 0,
    created_at    timestamp    NOT NULL DEFAULT now(),
    created_by    BIGINT,
    updated_at    timestamp    NOT NULL DEFAULT now(),
    updated_by    BIGINT,
    deleted_at    timestamp
);

CREATE UNIQUE INDEX uk_t_worker_badge_code
    ON t_worker (badge_code)
    WHERE deleted_at IS NULL;

CREATE UNIQUE INDEX uk_t_worker_id_card_no
    ON t_worker (id_card_no)
    WHERE id_card_no IS NOT NULL;

CREATE INDEX ix_t_worker_name
    ON t_worker (name);

CREATE INDEX ix_t_worker_deleted_at
    ON t_worker (deleted_at);

CREATE INDEX ix_t_worker_work_type_id
    ON t_worker (work_type_id);
