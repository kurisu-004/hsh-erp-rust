-- Migration 005: t_assembly, t_part
-- Notes:
--   * t_assembly.status and t_part.status are varchar(20) WITHOUT CHECK constraint
--     (state machine lives in src/modules/{assembly,part}/statemachine.rs).
--   * t_part is the largest table — final-state columns include
--     order_no, system_delivery_date, note, delivery_note_id, has_been_repaired
--     merged from later ALTER migrations.

------------------------------------------------------------------------
-- t_assembly
------------------------------------------------------------------------
CREATE TABLE t_assembly (
    id                     BIGINT          PRIMARY KEY,
    drawing_no             VARCHAR(100)    NOT NULL,
    name                   VARCHAR(200)    NOT NULL,
    applicant_name         VARCHAR(50),
    customer_id            BIGINT          NOT NULL,
    request_date           DATE            NOT NULL,
    planned_delivery_date  DATE            NOT NULL,
    actual_delivery_date   DATE,
    is_urgent              BOOLEAN         NOT NULL DEFAULT FALSE,
    status                 VARCHAR(20)     NOT NULL DEFAULT 'PENDING',
    serial_no              VARCHAR(8),
    quantity               INT             NOT NULL DEFAULT 1,
    unit_price             NUMERIC(12, 2)  NOT NULL DEFAULT 0,
    total_price            NUMERIC(14, 2)  NOT NULL DEFAULT 0,
    order_no               VARCHAR(30),
    system_delivery_date   DATE,
    note                   VARCHAR(500),
    version                INT             NOT NULL DEFAULT 0,
    created_at             timestamp       NOT NULL DEFAULT now(),
    created_by             BIGINT,
    updated_at             timestamp       NOT NULL DEFAULT now(),
    updated_by             BIGINT,
    deleted_at             timestamp
);

CREATE INDEX ix_t_assembly_drawing_no
    ON t_assembly (drawing_no);

CREATE INDEX ix_t_assembly_customer_id
    ON t_assembly (customer_id);

CREATE INDEX ix_t_assembly_status
    ON t_assembly (status);

CREATE INDEX ix_t_assembly_planned_delivery
    ON t_assembly (planned_delivery_date);

CREATE INDEX ix_t_assembly_customer_status
    ON t_assembly (customer_id, status);

CREATE INDEX ix_t_assembly_deleted_at
    ON t_assembly (deleted_at);

CREATE INDEX ix_t_assembly_serial_no
    ON t_assembly (serial_no);

CREATE UNIQUE INDEX uk_t_assembly_serial_no
    ON t_assembly (serial_no)
    WHERE deleted_at IS NULL
      AND serial_no IS NOT NULL;

CREATE INDEX ix_t_assembly_order_no
    ON t_assembly (order_no);

------------------------------------------------------------------------
-- t_part
------------------------------------------------------------------------
CREATE TABLE t_part (
    id                     BIGINT          PRIMARY KEY,
    serial_no              VARCHAR(8),
    name                   VARCHAR(200)    NOT NULL,
    drawing_no             VARCHAR(100)    NOT NULL,
    applicant_name         VARCHAR(50)     NOT NULL,
    quantity               INT             NOT NULL DEFAULT 1,
    unit_price             NUMERIC(12, 2)  NOT NULL DEFAULT 0,
    total_price            NUMERIC(14, 2)  NOT NULL DEFAULT 0,
    request_date           DATE            NOT NULL,
    planned_delivery_date  DATE            NOT NULL,
    actual_delivery_date   DATE,
    status                 VARCHAR(20)     NOT NULL DEFAULT 'PENDING',
    location               VARCHAR(20),
    is_urgent              BOOLEAN         NOT NULL DEFAULT FALSE,
    current_holder_id      BIGINT,
    placed_at              timestamp,
    next_process_id        BIGINT,
    customer_id            BIGINT          NOT NULL,
    assembly_id            BIGINT,
    order_no               VARCHAR(30),
    system_delivery_date   DATE,
    note                   VARCHAR(500),
    delivery_note_id       BIGINT,
    has_been_repaired      BOOLEAN         NOT NULL DEFAULT FALSE,
    version                INT             NOT NULL DEFAULT 0,
    created_at             timestamp       NOT NULL DEFAULT now(),
    created_by             BIGINT,
    updated_at             timestamp       NOT NULL DEFAULT now(),
    updated_by             BIGINT,
    deleted_at             timestamp
);

CREATE INDEX ix_t_part_name
    ON t_part (name);

CREATE INDEX ix_t_part_drawing_no
    ON t_part (drawing_no);

CREATE INDEX ix_t_part_customer_id
    ON t_part (customer_id);

CREATE INDEX ix_t_part_status
    ON t_part (status);

CREATE INDEX ix_t_part_is_urgent
    ON t_part (is_urgent);

CREATE INDEX ix_t_part_request_date
    ON t_part (request_date);

CREATE INDEX ix_t_part_planned_delivery_date
    ON t_part (planned_delivery_date);

CREATE INDEX ix_t_part_deleted_at
    ON t_part (deleted_at);

CREATE UNIQUE INDEX uk_t_part_serial_no
    ON t_part (serial_no)
    WHERE serial_no IS NOT NULL;

CREATE INDEX ix_t_part_current_holder_id
    ON t_part (current_holder_id);

CREATE INDEX ix_t_part_placed_at
    ON t_part (placed_at);

CREATE INDEX ix_t_part_location
    ON t_part (location);

CREATE INDEX ix_t_part_assembly_id
    ON t_part (assembly_id);

CREATE INDEX ix_t_part_customer_status_delivery
    ON t_part (customer_id, status, planned_delivery_date);

CREATE INDEX ix_t_part_assembly_id_status
    ON t_part (assembly_id, status);

CREATE INDEX ix_t_part_status_holder
    ON t_part (status, current_holder_id);

CREATE INDEX ix_t_part_location_status_next_process
    ON t_part (location, status, next_process_id);

CREATE INDEX ix_t_part_next_process_id
    ON t_part (next_process_id);

CREATE INDEX ix_t_part_order_no
    ON t_part (order_no);

CREATE INDEX ix_t_part_delivery_note_id
    ON t_part (delivery_note_id);
