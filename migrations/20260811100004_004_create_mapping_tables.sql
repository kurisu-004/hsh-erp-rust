-- Migration 004: t_work_type_process, t_shelf_process,
--                t_outsource_company, t_outsource_company_process
-- Notes:
--   * All four mapping tables carry a CHECK to forbid self-loop rows
--     (the pair (A, A) is rejected even though the unique index would also block it).

------------------------------------------------------------------------
-- t_work_type_process
------------------------------------------------------------------------
CREATE TABLE t_work_type_process (
    id           BIGINT       PRIMARY KEY,
    work_type_id BIGINT       NOT NULL,
    process_id   BIGINT       NOT NULL,
    sort_order   INT          NOT NULL DEFAULT 0,
    version      INT          NOT NULL DEFAULT 0,
    created_at   timestamp    NOT NULL DEFAULT now(),
    created_by   BIGINT,
    updated_at   timestamp    NOT NULL DEFAULT now(),
    updated_by   BIGINT,
    deleted_at   timestamp,
    CONSTRAINT ck_t_work_type_process_no_self_loop CHECK (work_type_id <> process_id)
);

CREATE UNIQUE INDEX uk_t_work_type_process
    ON t_work_type_process (work_type_id, process_id)
    WHERE deleted_at IS NULL;

CREATE INDEX ix_t_work_type_process_work_type
    ON t_work_type_process (work_type_id);

CREATE INDEX ix_t_work_type_process_process
    ON t_work_type_process (process_id);

CREATE INDEX ix_t_work_type_process_deleted_at
    ON t_work_type_process (deleted_at);

------------------------------------------------------------------------
-- t_shelf_process
------------------------------------------------------------------------
CREATE TABLE t_shelf_process (
    id          BIGINT       PRIMARY KEY,
    shelf_id    BIGINT       NOT NULL,
    process_id  BIGINT       NOT NULL,
    sort_order  INT          NOT NULL DEFAULT 0,
    version     INT          NOT NULL DEFAULT 0,
    created_at  timestamp    NOT NULL DEFAULT now(),
    created_by  BIGINT,
    updated_at  timestamp    NOT NULL DEFAULT now(),
    updated_by  BIGINT,
    deleted_at  timestamp,
    CONSTRAINT ck_t_shelf_process_no_self_loop CHECK (shelf_id <> process_id)
);

CREATE UNIQUE INDEX uk_t_shelf_process
    ON t_shelf_process (shelf_id, process_id)
    WHERE deleted_at IS NULL;

CREATE INDEX ix_t_shelf_process_shelf
    ON t_shelf_process (shelf_id);

CREATE INDEX ix_t_shelf_process_process
    ON t_shelf_process (process_id);

CREATE INDEX ix_t_shelf_process_deleted_at
    ON t_shelf_process (deleted_at);

------------------------------------------------------------------------
-- t_outsource_company
------------------------------------------------------------------------
CREATE TABLE t_outsource_company (
    id            BIGINT       PRIMARY KEY,
    name          VARCHAR(100) NOT NULL,
    contact_name  VARCHAR(50),
    contact_phone VARCHAR(50),
    address       VARCHAR(200),
    is_active     BOOLEAN      NOT NULL DEFAULT TRUE,
    version       INT          NOT NULL DEFAULT 0,
    created_at    timestamp    NOT NULL DEFAULT now(),
    created_by    BIGINT,
    updated_at    timestamp    NOT NULL DEFAULT now(),
    updated_by    BIGINT,
    deleted_at    timestamp
);

CREATE INDEX ix_t_outsource_company_name
    ON t_outsource_company (name);

CREATE INDEX ix_t_outsource_company_deleted_at
    ON t_outsource_company (deleted_at);

CREATE UNIQUE INDEX uk_t_outsource_company_name
    ON t_outsource_company (name)
    WHERE deleted_at IS NULL;

------------------------------------------------------------------------
-- t_outsource_company_process
------------------------------------------------------------------------
CREATE TABLE t_outsource_company_process (
    id                    BIGINT       PRIMARY KEY,
    outsource_company_id  BIGINT       NOT NULL,
    process_id            BIGINT       NOT NULL,
    sort_order            INT          NOT NULL DEFAULT 0,
    version               INT          NOT NULL DEFAULT 0,
    created_at            timestamp    NOT NULL DEFAULT now(),
    created_by            BIGINT,
    updated_at            timestamp    NOT NULL DEFAULT now(),
    updated_by            BIGINT,
    deleted_at            timestamp,
    CONSTRAINT ck_t_outsource_company_process_no_self_loop CHECK (outsource_company_id <> process_id)
);

CREATE UNIQUE INDEX uk_t_outsource_company_process
    ON t_outsource_company_process (outsource_company_id, process_id)
    WHERE deleted_at IS NULL;

CREATE INDEX ix_t_outsource_company_process_company
    ON t_outsource_company_process (outsource_company_id);

CREATE INDEX ix_t_outsource_company_process_process
    ON t_outsource_company_process (process_id);

CREATE INDEX ix_t_outsource_company_process_deleted_at
    ON t_outsource_company_process (deleted_at);
