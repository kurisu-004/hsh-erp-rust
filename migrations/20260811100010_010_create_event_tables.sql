-- Migration 010: t_part_event, t_outsource_quote_event,
--                 t_delivery_note_event, t_pickup_skip_event
-- Notes:
--   * Event tables carry ONLY `created_at` (no version / updated_at / deleted_at).
--   * t_delivery_note_event no longer has drawing_code / badge_code /
--     scanned_count / expected_count — those columns were dropped by migration 0013.
--   * t_part_event gained outsource_company_id (0018), batch_id and quantity (0020).

------------------------------------------------------------------------
-- t_part_event
------------------------------------------------------------------------
CREATE TABLE t_part_event (
    id                     BIGINT       PRIMARY KEY,
    part_id                BIGINT       NOT NULL,
    worker_id              BIGINT,
    event_type             VARCHAR(30)  NOT NULL,
    from_status            VARCHAR(20),
    to_status              VARCHAR(20),
    drawing_code           VARCHAR(100),
    badge_code             VARCHAR(50),
    note                   VARCHAR(500),
    created_by             BIGINT,
    outsource_company_id   BIGINT,
    batch_id               BIGINT,
    quantity               INT,
    created_at             timestamp    NOT NULL DEFAULT now()
);

CREATE INDEX ix_part_event_part_id
    ON t_part_event (part_id);

CREATE INDEX ix_part_event_created_at
    ON t_part_event (created_at);

CREATE INDEX ix_part_event_event_type
    ON t_part_event (event_type);

CREATE INDEX ix_part_event_worker_id
    ON t_part_event (worker_id);

CREATE INDEX ix_t_part_event_outsource_company_id
    ON t_part_event (outsource_company_id)
    WHERE outsource_company_id IS NOT NULL;

CREATE INDEX ix_t_part_event_batch_id
    ON t_part_event (batch_id)
    WHERE batch_id IS NOT NULL;

------------------------------------------------------------------------
-- t_outsource_quote_event
------------------------------------------------------------------------
CREATE TABLE t_outsource_quote_event (
    id           BIGINT       PRIMARY KEY,
    quote_id     BIGINT       NOT NULL,
    event_type   VARCHAR(32)  NOT NULL,
    from_status  VARCHAR(16),
    to_status    VARCHAR(16),
    note         VARCHAR(500),
    created_by   BIGINT,
    created_at   timestamp    NOT NULL DEFAULT now()
);

CREATE INDEX ix_t_outsource_quote_event_quote_id
    ON t_outsource_quote_event (quote_id);

CREATE INDEX ix_t_outsource_quote_event_created_at
    ON t_outsource_quote_event (created_at);

------------------------------------------------------------------------
-- t_delivery_note_event
------------------------------------------------------------------------
CREATE TABLE t_delivery_note_event (
    id                 BIGINT       PRIMARY KEY,
    delivery_note_id   BIGINT       NOT NULL,
    event_type         VARCHAR(32)  NOT NULL,
    from_status        VARCHAR(16),
    to_status          VARCHAR(16),
    note               VARCHAR(500),
    created_by         BIGINT,
    created_at         timestamp    NOT NULL DEFAULT now()
);

CREATE INDEX ix_t_delivery_note_event_note_id
    ON t_delivery_note_event (delivery_note_id);

CREATE INDEX ix_t_delivery_note_event_note_created
    ON t_delivery_note_event (delivery_note_id, created_at);

CREATE INDEX ix_t_delivery_note_event_created_at
    ON t_delivery_note_event (created_at);

------------------------------------------------------------------------
-- t_pickup_skip_event
------------------------------------------------------------------------
CREATE TABLE t_pickup_skip_event (
    id                              BIGINT        PRIMARY KEY,
    worker_id                       BIGINT        NOT NULL,
    part_id                         BIGINT        NOT NULL,
    batch_id                        BIGINT        NOT NULL,
    batch_no                        INT           NOT NULL,
    part_serial_no                  VARCHAR(100),
    shelf_id                        BIGINT        NOT NULL,
    work_type_id                    BIGINT,
    quantity                        INT           NOT NULL,
    part_planned_delivery_date      DATE,
    skipped_earliest_date           DATE,
    created_at                      timestamp     NOT NULL DEFAULT now()
);

CREATE INDEX ix_t_pickup_skip_event_worker_id
    ON t_pickup_skip_event (worker_id);

CREATE INDEX ix_t_pickup_skip_event_created_at
    ON t_pickup_skip_event (created_at);
