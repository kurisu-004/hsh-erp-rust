-- Migration 002: t_customer, t_applicant, t_serial_counter
-- Notes:
--   * t_customer has a 1-letter upper-case `serial_prefix` (CHECK ~ '^[A-Z]$').
--   * t_serial_counter uses `prefix varchar(1)` as the business PK (snowflake id NOT used here).

------------------------------------------------------------------------
-- t_customer
------------------------------------------------------------------------
CREATE TABLE t_customer (
    id             BIGINT       PRIMARY KEY,
    name           VARCHAR(100) NOT NULL,
    parent_id      BIGINT,
    serial_prefix  VARCHAR(1),
    version        INT          NOT NULL DEFAULT 0,
    created_at     timestamp    NOT NULL DEFAULT now(),
    created_by     BIGINT,
    updated_at     timestamp    NOT NULL DEFAULT now(),
    updated_by     BIGINT,
    deleted_at     timestamp,
    CONSTRAINT ck_t_customer_no_self_parent    CHECK (parent_id IS NULL OR parent_id <> id),
    CONSTRAINT ck_t_customer_serial_prefix_uppercase CHECK (serial_prefix IS NULL OR serial_prefix ~ '^[A-Z]$')
);

CREATE INDEX ix_t_customer_name
    ON t_customer (name);

CREATE INDEX ix_t_customer_parent_id
    ON t_customer (parent_id);

CREATE INDEX ix_t_customer_deleted_at
    ON t_customer (deleted_at);

CREATE UNIQUE INDEX uq_t_customer_root_prefix
    ON t_customer (serial_prefix)
    WHERE deleted_at IS NULL
      AND parent_id IS NULL
      AND serial_prefix IS NOT NULL;

------------------------------------------------------------------------
-- t_applicant
------------------------------------------------------------------------
CREATE TABLE t_applicant (
    id          BIGINT       PRIMARY KEY,
    name        VARCHAR(50)  NOT NULL,
    customer_id BIGINT       NOT NULL,
    version     INT          NOT NULL DEFAULT 0,
    created_at  timestamp    NOT NULL DEFAULT now(),
    created_by  BIGINT,
    updated_at  timestamp    NOT NULL DEFAULT now(),
    updated_by  BIGINT,
    deleted_at  timestamp
);

CREATE INDEX ix_t_applicant_customer_id
    ON t_applicant (customer_id);

CREATE INDEX ix_t_applicant_name
    ON t_applicant (name);

CREATE INDEX ix_t_applicant_deleted_at
    ON t_applicant (deleted_at);

CREATE UNIQUE INDEX uq_t_applicant_name_customer_active
    ON t_applicant (name, customer_id)
    WHERE deleted_at IS NULL;

------------------------------------------------------------------------
-- t_serial_counter  (PK is `prefix varchar(1)` — business key, NOT snowflake)
------------------------------------------------------------------------
CREATE TABLE t_serial_counter (
    prefix      VARCHAR(1) PRIMARY KEY,
    counter     BIGINT      NOT NULL DEFAULT 0,
    version     INT         NOT NULL DEFAULT 0,
    created_at  timestamp   NOT NULL DEFAULT now(),
    created_by  BIGINT,
    updated_at  timestamp   NOT NULL DEFAULT now(),
    updated_by  BIGINT,
    deleted_at  timestamp
);
