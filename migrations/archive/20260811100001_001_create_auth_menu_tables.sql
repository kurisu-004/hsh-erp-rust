-- Migration 001: t_user, t_user_role, t_menu, t_role_menu
-- Notes:
--   * t_user_role unique constraint is a NON-partial UNIQUE (user_id, role, scope_type, scope_id)
--     to match the production schema. After soft-delete of a SHELF_ACCOUNT row, the same
--     (user_id, role, scope_type, scope_id) tuple cannot be re-inserted until the soft-deleted
--     row is physically deleted. This is the same behavior as the Python production deployment.
--   * Phase E rewrite: aligned 1:1 with /Users/ren/Code/schema_ddl.sql (column order, COMMENT,
--     CREATE SEQUENCE ... OWNED BY, ALTER TABLE ... SET DEFAULT nextval).

------------------------------------------------------------------------
-- t_user
------------------------------------------------------------------------
CREATE TABLE public.t_user (
    id                      bigint NOT NULL,
    username                character varying(50) NOT NULL,
    password_hash           character varying(255) NOT NULL,
    full_name               character varying(50) NOT NULL,
    phone                   character varying(20),
    is_active               boolean DEFAULT true NOT NULL,
    last_login_at           timestamp without time zone,
    version                 integer DEFAULT 0 NOT NULL,
    created_at              timestamp without time zone DEFAULT now() NOT NULL,
    created_by              bigint,
    updated_at              timestamp without time zone DEFAULT now() NOT NULL,
    updated_by              bigint,
    deleted_at              timestamp without time zone,
    refresh_token_version   integer DEFAULT 0 NOT NULL
);

COMMENT ON COLUMN public.t_user.version IS '乐观锁版本号；每次 UPDATE 自增；冲突抛 BIZ_VERSION_CONFLICT 409';
COMMENT ON COLUMN public.t_user.refresh_token_version IS 'refresh token 轮转计数器；每次成功 refresh 后 +1';

CREATE SEQUENCE public.t_user_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.t_user_id_seq OWNED BY public.t_user.id;

ALTER TABLE ONLY public.t_user ALTER COLUMN id SET DEFAULT nextval('public.t_user_id_seq'::regclass);
ALTER TABLE ONLY public.t_user ADD CONSTRAINT t_user_pkey PRIMARY KEY (id);

CREATE UNIQUE INDEX uk_t_user_username ON public.t_user USING btree (username) WHERE (deleted_at IS NULL);
CREATE INDEX ix_t_user_deleted_at ON public.t_user USING btree (deleted_at);

------------------------------------------------------------------------
-- t_user_role
------------------------------------------------------------------------
CREATE TABLE public.t_user_role (
    id          bigint NOT NULL,
    user_id     bigint NOT NULL,
    role        character varying(20) NOT NULL,
    scope_type  character varying(20),
    scope_id    bigint,
    version     integer DEFAULT 0 NOT NULL,
    created_at  timestamp without time zone DEFAULT now() NOT NULL,
    created_by  bigint,
    updated_at  timestamp without time zone DEFAULT now() NOT NULL,
    updated_by  bigint,
    deleted_at  timestamp without time zone
);

COMMENT ON COLUMN public.t_user_role.version IS '乐观锁版本号；每次 UPDATE 自增；冲突抛 BIZ_VERSION_CONFLICT 409';

CREATE SEQUENCE public.t_user_role_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.t_user_role_id_seq OWNED BY public.t_user_role.id;

ALTER TABLE ONLY public.t_user_role ALTER COLUMN id SET DEFAULT nextval('public.t_user_role_id_seq'::regclass);
ALTER TABLE ONLY public.t_user_role ADD CONSTRAINT t_user_role_pkey PRIMARY KEY (id);

-- NON-partial UNIQUE (matches production schema_ddl.sql:2469).
-- Note: diverges from pre-Phase-E SQLx migration which used a partial unique INDEX.
ALTER TABLE ONLY public.t_user_role
    ADD CONSTRAINT uk_t_user_role_user_role_scope UNIQUE (user_id, role, scope_type, scope_id);

CREATE INDEX ix_t_user_role_user_id ON public.t_user_role USING btree (user_id);
CREATE INDEX ix_t_user_role_scope ON public.t_user_role USING btree (scope_type, scope_id);
CREATE INDEX ix_t_user_role_deleted_at ON public.t_user_role USING btree (deleted_at);

------------------------------------------------------------------------
-- t_menu
------------------------------------------------------------------------
CREATE TABLE public.t_menu (
    id          bigint NOT NULL,
    parent_id   bigint,
    code        character varying(64) NOT NULL,
    title       character varying(50) NOT NULL,
    path        character varying(200),
    icon        character varying(50),
    sort_order  integer DEFAULT 0 NOT NULL,
    is_active   boolean DEFAULT true NOT NULL,
    version     integer DEFAULT 0 NOT NULL,
    created_at  timestamp without time zone DEFAULT now() NOT NULL,
    created_by  bigint,
    updated_at  timestamp without time zone DEFAULT now() NOT NULL,
    updated_by  bigint,
    deleted_at  timestamp without time zone,
    CONSTRAINT ck_t_menu_no_self_loop CHECK (((parent_id IS NULL) OR (parent_id <> id)))
);

COMMENT ON COLUMN public.t_menu.version IS '乐观锁版本号；每次 UPDATE 自增；冲突抛 BIZ_VERSION_CONFLICT 409';

CREATE SEQUENCE public.t_menu_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.t_menu_id_seq OWNED BY public.t_menu.id;

ALTER TABLE ONLY public.t_menu ALTER COLUMN id SET DEFAULT nextval('public.t_menu_id_seq'::regclass);
ALTER TABLE ONLY public.t_menu ADD CONSTRAINT t_menu_pkey PRIMARY KEY (id);

CREATE UNIQUE INDEX uk_t_menu_code ON public.t_menu USING btree (code) WHERE (deleted_at IS NULL);
CREATE INDEX ix_t_menu_parent_id ON public.t_menu USING btree (parent_id);
CREATE INDEX ix_t_menu_deleted_at ON public.t_menu USING btree (deleted_at);

------------------------------------------------------------------------
-- t_role_menu
------------------------------------------------------------------------
CREATE TABLE public.t_role_menu (
    id         bigint NOT NULL,
    role       character varying(20) NOT NULL,
    menu_id    bigint NOT NULL,
    version    integer DEFAULT 0 NOT NULL,
    created_at timestamp without time zone DEFAULT now() NOT NULL,
    created_by bigint,
    updated_at timestamp without time zone DEFAULT now() NOT NULL,
    updated_by bigint,
    deleted_at timestamp without time zone
);

COMMENT ON COLUMN public.t_role_menu.version IS '乐观锁版本号；每次 UPDATE 自增；冲突抛 BIZ_VERSION_CONFLICT 409';

CREATE SEQUENCE public.t_role_menu_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.t_role_menu_id_seq OWNED BY public.t_role_menu.id;

ALTER TABLE ONLY public.t_role_menu ALTER COLUMN id SET DEFAULT nextval('public.t_role_menu_id_seq'::regclass);
ALTER TABLE ONLY public.t_role_menu ADD CONSTRAINT t_role_menu_pkey PRIMARY KEY (id);

CREATE UNIQUE INDEX uk_t_role_menu_role_menu ON public.t_role_menu USING btree (role, menu_id) WHERE (deleted_at IS NULL);
CREATE INDEX ix_t_role_menu_role ON public.t_role_menu USING btree (role);
CREATE INDEX ix_t_role_menu_menu_id ON public.t_role_menu USING btree (menu_id);
CREATE INDEX ix_t_role_menu_deleted_at ON public.t_role_menu USING btree (deleted_at);