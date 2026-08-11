-- Migration 001: t_user, t_user_role, t_menu, t_role_menu
-- Notes:
--   * t_user_role unique constraint is a PARTIAL unique index (WHERE deleted_at IS NULL),
--     deliberately diverging from Python's plain UniqueConstraint to allow
--     SHELF_ACCOUNT role to be re-added after soft deletion.

------------------------------------------------------------------------
-- t_user
------------------------------------------------------------------------
CREATE TABLE t_user (
    id                      BIGINT       PRIMARY KEY,
    username                VARCHAR(50)  NOT NULL,
    password_hash           VARCHAR(255) NOT NULL,
    full_name               VARCHAR(50)  NOT NULL,
    phone                   VARCHAR(20),
    is_active               BOOLEAN      NOT NULL DEFAULT TRUE,
    last_login_at           timestamp,
    refresh_token_version   INT          NOT NULL DEFAULT 0,
    version                 INT          NOT NULL DEFAULT 0,
    created_at              timestamp    NOT NULL DEFAULT now(),
    created_by              BIGINT,
    updated_at              timestamp    NOT NULL DEFAULT now(),
    updated_by              BIGINT,
    deleted_at              timestamp
);

CREATE UNIQUE INDEX uk_t_user_username
    ON t_user (username)
    WHERE deleted_at IS NULL;

CREATE INDEX ix_t_user_deleted_at
    ON t_user (deleted_at);

------------------------------------------------------------------------
-- t_user_role
------------------------------------------------------------------------
CREATE TABLE t_user_role (
    id          BIGINT       PRIMARY KEY,
    user_id     BIGINT       NOT NULL,
    role        VARCHAR(20)  NOT NULL,
    scope_type  VARCHAR(20),
    scope_id    BIGINT,
    version     INT          NOT NULL DEFAULT 0,
    created_at  timestamp    NOT NULL DEFAULT now(),
    created_by  BIGINT,
    updated_at  timestamp    NOT NULL DEFAULT now(),
    updated_by  BIGINT,
    deleted_at  timestamp
);

-- partial unique (diverges from Python's plain UniqueConstraint)
CREATE UNIQUE INDEX uk_t_user_role_scope
    ON t_user_role (user_id, role, scope_type, scope_id)
    WHERE deleted_at IS NULL;

CREATE INDEX ix_t_user_role_user_id
    ON t_user_role (user_id);

CREATE INDEX ix_t_user_role_scope
    ON t_user_role (scope_type, scope_id);

CREATE INDEX ix_t_user_role_deleted_at
    ON t_user_role (deleted_at);

------------------------------------------------------------------------
-- t_menu
------------------------------------------------------------------------
CREATE TABLE t_menu (
    id          BIGINT       PRIMARY KEY,
    parent_id   BIGINT,
    code        VARCHAR(64)  NOT NULL,
    title       VARCHAR(50)  NOT NULL,
    path        VARCHAR(200),
    icon        VARCHAR(50),
    sort_order  INT          NOT NULL DEFAULT 0,
    is_active   BOOLEAN      NOT NULL DEFAULT TRUE,
    version     INT          NOT NULL DEFAULT 0,
    created_at  timestamp    NOT NULL DEFAULT now(),
    created_by  BIGINT,
    updated_at  timestamp    NOT NULL DEFAULT now(),
    updated_by  BIGINT,
    deleted_at  timestamp,
    CONSTRAINT ck_t_menu_no_self_loop CHECK (parent_id IS NULL OR parent_id <> id)
);

CREATE UNIQUE INDEX uk_t_menu_code
    ON t_menu (code)
    WHERE deleted_at IS NULL;

CREATE INDEX ix_t_menu_parent_id
    ON t_menu (parent_id);

CREATE INDEX ix_t_menu_deleted_at
    ON t_menu (deleted_at);

------------------------------------------------------------------------
-- t_role_menu
------------------------------------------------------------------------
CREATE TABLE t_role_menu (
    id         BIGINT       PRIMARY KEY,
    role       VARCHAR(20)  NOT NULL,
    menu_id    BIGINT       NOT NULL,
    version    INT          NOT NULL DEFAULT 0,
    created_at timestamp    NOT NULL DEFAULT now(),
    created_by BIGINT,
    updated_at timestamp    NOT NULL DEFAULT now(),
    updated_by BIGINT,
    deleted_at timestamp
);

CREATE UNIQUE INDEX uk_t_role_menu_role_menu
    ON t_role_menu (role, menu_id)
    WHERE deleted_at IS NULL;

CREATE INDEX ix_t_role_menu_role
    ON t_role_menu (role);

CREATE INDEX ix_t_role_menu_menu_id
    ON t_role_menu (menu_id);

CREATE INDEX ix_t_role_menu_deleted_at
    ON t_role_menu (deleted_at);
