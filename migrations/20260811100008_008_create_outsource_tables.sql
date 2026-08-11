-- Migration 008: t_outsource_quote, t_outsource_shipment
-- Notes:
--   * t_outsource_quote.status CHECK keeps the 8-value list from migration 0019.
--   * t_outsource_quote.price CHECK is `>= 0` (relaxed — 0 means "to be negotiated").
--   * uq_t_outsource_quote_approved_part_process enforces one APPROVED+direct=false
--     quote per (part, process); rebuilt by migration 0022.

------------------------------------------------------------------------
-- t_outsource_quote
------------------------------------------------------------------------
CREATE TABLE t_outsource_quote (
    id                    BIGINT          PRIMARY KEY,
    part_id               BIGINT          NOT NULL,
    outsource_company_id  BIGINT          NOT NULL,
    process_id            BIGINT          NOT NULL,
    price                 NUMERIC(12, 2)  NOT NULL,
    note                  VARCHAR(500),
    status                VARCHAR(16)     NOT NULL DEFAULT 'DRAFT',
    submitted_at          timestamp,
    reviewed_at           timestamp,
    review_note           VARCHAR(500),
    sent_at               timestamp,
    received_at           timestamp,
    quantity              INT,
    is_billed             BOOLEAN         NOT NULL DEFAULT FALSE,
    is_direct             BOOLEAN         NOT NULL DEFAULT FALSE,
    version               INT             NOT NULL DEFAULT 0,
    created_at            timestamp       NOT NULL DEFAULT now(),
    created_by            BIGINT,
    updated_at            timestamp       NOT NULL DEFAULT now(),
    updated_by            BIGINT,
    deleted_at            timestamp,
    CONSTRAINT ck_t_outsource_quote_price_positive CHECK (price >= 0),
    CONSTRAINT ck_t_outsource_quote_status CHECK (status IN ('DRAFT', 'SUBMITTED', 'APPROVED', 'REJECTED', 'OUTSOURCING', 'RECEIVED', 'BILLED', 'USED'))
);

CREATE INDEX ix_t_outsource_quote_part_id
    ON t_outsource_quote (part_id);

CREATE INDEX ix_t_outsource_quote_outsource_company_id
    ON t_outsource_quote (outsource_company_id);

CREATE INDEX ix_t_outsource_quote_process_id
    ON t_outsource_quote (process_id);

CREATE INDEX ix_t_outsource_quote_status
    ON t_outsource_quote (status);

CREATE INDEX ix_t_outsource_quote_deleted_at
    ON t_outsource_quote (deleted_at);

CREATE UNIQUE INDEX uq_t_outsource_quote_approved_part_process
    ON t_outsource_quote (part_id, process_id)
    WHERE deleted_at IS NULL
      AND status = 'APPROVED'
      AND is_direct = FALSE;

CREATE INDEX ix_t_outsource_quote_company_sent_at
    ON t_outsource_quote (outsource_company_id, sent_at);

CREATE INDEX ix_t_outsource_quote_company_received_at
    ON t_outsource_quote (outsource_company_id, received_at);

------------------------------------------------------------------------
-- t_outsource_shipment
------------------------------------------------------------------------
CREATE TABLE t_outsource_shipment (
    id                    BIGINT          PRIMARY KEY,
    quote_id              BIGINT          NOT NULL,
    part_id               BIGINT          NOT NULL,
    batch_id              BIGINT,
    outsource_company_id  BIGINT          NOT NULL,
    process_id            BIGINT          NOT NULL,
    quantity              INT             NOT NULL,
    unit_price            NUMERIC(12, 2)  NOT NULL,
    status                VARCHAR(16)     NOT NULL DEFAULT 'OUTSOURCING',
    sent_at               timestamp       NOT NULL,
    received_at           timestamp,
    is_billed             BOOLEAN         NOT NULL DEFAULT FALSE,
    version               INT             NOT NULL DEFAULT 0,
    created_at            timestamp       NOT NULL DEFAULT now(),
    created_by            BIGINT,
    updated_at            timestamp       NOT NULL DEFAULT now(),
    updated_by            BIGINT,
    deleted_at            timestamp,
    CONSTRAINT ck_t_outsource_shipment_status            CHECK (status IN ('OUTSOURCING', 'RECEIVED', 'CANCELLED')),
    CONSTRAINT ck_t_outsource_shipment_quantity_positive CHECK (quantity > 0)
);

CREATE INDEX ix_t_outsource_shipment_quote_id
    ON t_outsource_shipment (quote_id);

CREATE INDEX ix_t_outsource_shipment_part_id
    ON t_outsource_shipment (part_id);

CREATE INDEX ix_t_outsource_shipment_batch_id
    ON t_outsource_shipment (batch_id);

CREATE INDEX ix_t_outsource_shipment_outsource_company_id
    ON t_outsource_shipment (outsource_company_id);

CREATE INDEX ix_t_outsource_shipment_process_id
    ON t_outsource_shipment (process_id);

CREATE INDEX ix_t_outsource_shipment_status
    ON t_outsource_shipment (status);

CREATE UNIQUE INDEX uq_t_outsource_shipment_open_batch
    ON t_outsource_shipment (batch_id)
    WHERE deleted_at IS NULL
      AND status = 'OUTSOURCING';
