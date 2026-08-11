-- Migration 006: t_part_batch
-- Notes:
--   * Unique constraint is a plain UNIQUE(part_id, batch_no) — batch numbers are
--     allocated by the service per part, no need to make it partial.
--   * status varchar(20) WITHOUT CHECK (state machine lives in service).

CREATE TABLE t_part_batch (
    id                  BIGINT       PRIMARY KEY,
    part_id             BIGINT       NOT NULL,
    batch_no            INT          NOT NULL,
    quantity            INT          NOT NULL,
    status              VARCHAR(20)  NOT NULL DEFAULT 'PENDING',
    location            VARCHAR(20),
    current_holder_id   BIGINT,
    next_process_id     BIGINT,
    placed_at           timestamp,
    delivery_note_id    BIGINT,
    parent_batch_id     BIGINT,
    has_been_repaired   BOOLEAN      NOT NULL DEFAULT FALSE,
    version             INT          NOT NULL DEFAULT 0,
    created_at          timestamp    NOT NULL DEFAULT now(),
    created_by          BIGINT,
    updated_at          timestamp    NOT NULL DEFAULT now(),
    updated_by          BIGINT,
    deleted_at          timestamp,
    CONSTRAINT uq_t_part_batch_part_no UNIQUE (part_id, batch_no)
);

CREATE INDEX ix_t_part_batch_part_id
    ON t_part_batch (part_id);

CREATE INDEX ix_t_part_batch_status
    ON t_part_batch (status);

CREATE INDEX ix_t_part_batch_location
    ON t_part_batch (location);

CREATE INDEX ix_t_part_batch_current_holder_id
    ON t_part_batch (current_holder_id);

CREATE INDEX ix_t_part_batch_next_process_id
    ON t_part_batch (next_process_id);

CREATE INDEX ix_t_part_batch_placed_at
    ON t_part_batch (placed_at);

CREATE INDEX ix_t_part_batch_delivery_note_id
    ON t_part_batch (delivery_note_id);

CREATE INDEX ix_t_part_batch_deleted_at
    ON t_part_batch (deleted_at);

CREATE INDEX ix_t_part_batch_status_holder
    ON t_part_batch (status, current_holder_id);

CREATE INDEX ix_t_part_batch_location_status_next_process
    ON t_part_batch (location, status, next_process_id);
