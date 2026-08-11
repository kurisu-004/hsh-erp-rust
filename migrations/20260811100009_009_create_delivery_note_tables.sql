-- Migration 009: t_delivery_note, t_delivery_note_counter
-- Notes:
--   * t_delivery_note_counter has only `created_at` + `updated_at`; PK is
--     `date_ymd varchar(8)` — no snowflake id, no AuditMixin.
--   * t_delivery_note.delivery_date was added by migration 0010.

------------------------------------------------------------------------
-- t_delivery_note_counter  (business-day sequence)
------------------------------------------------------------------------
CREATE TABLE t_delivery_note_counter (
    date_ymd    VARCHAR(8) PRIMARY KEY,
    last_value  INT         NOT NULL DEFAULT 0,
    created_at  timestamp   NOT NULL DEFAULT now(),
    updated_at  timestamp   NOT NULL DEFAULT now()
);

------------------------------------------------------------------------
-- t_delivery_note
------------------------------------------------------------------------
CREATE TABLE t_delivery_note (
    id                  BIGINT       PRIMARY KEY,
    delivery_note_no    VARCHAR(16)  NOT NULL,
    customer_id         BIGINT       NOT NULL,
    status              VARCHAR(16)  NOT NULL DEFAULT 'DRAFT',
    submitted_at        timestamp,
    picked_up_at        timestamp,
    submitted_by        BIGINT,
    picked_up_by        BIGINT,
    driver_worker_id    BIGINT,
    note                VARCHAR(500),
    delivery_date       DATE,
    version             INT          NOT NULL DEFAULT 0,
    created_at          timestamp    NOT NULL DEFAULT now(),
    created_by          BIGINT,
    updated_at          timestamp    NOT NULL DEFAULT now(),
    updated_by          BIGINT,
    deleted_at          timestamp,
    CONSTRAINT ck_t_delivery_note_status CHECK (status IN ('DRAFT', 'SUBMITTED', 'PICKED_UP', 'ARCHIVED'))
);

CREATE UNIQUE INDEX uq_t_delivery_note_no_active
    ON t_delivery_note (delivery_note_no)
    WHERE deleted_at IS NULL;

CREATE INDEX ix_t_delivery_note_status
    ON t_delivery_note (status);

CREATE INDEX ix_t_delivery_note_customer_id
    ON t_delivery_note (customer_id);

CREATE INDEX ix_t_delivery_note_submitted_at
    ON t_delivery_note (submitted_at);

CREATE INDEX ix_t_delivery_note_deleted_at
    ON t_delivery_note (deleted_at);
