-- ============================================================================
-- 20261014000000_001_rbac —— 角色 / 权限点 / 角色授权（iam::rbac 子模块）
-- ============================================================================
-- 需求：把「角色」与「权限点」从散落在代码里的 varchar 提升为**可管理的数据**：
--   * `t_role`            角色定义（内置 6 个 + 管理员自定义），带 `is_system`
--   * `t_permission`      权限点定义（按钮级操作码，如 `part.drawing.print`）
--   * `t_role_permission` 角色 → 权限点授权
--
-- 归属域：`iam::rbac`（`src/modules/iam/rbac/`），URL 挂 `/api/v2/iam/{roles,permissions,menus}`。
--
-- ## 三条设计取舍
--
-- ### 1）零外键，全部 varchar 关联
-- 本文件**刻意不建任何物理外键**。`t_role_permission.role_code` 与
-- `t_permission.code` 之间、`t_role.code` 与既有 `t_user_role.role` /
-- `t_role_menu.role` 之间，一律是无 FK 的 varchar 关联 —— 与仓库既有的
-- `t_role_menu` / `t_user_role` **完全同款**。
--
-- 理由：关联表的写入侧是「整包替换」（角色面板的 grants 提交），需要级联改名
-- （自定义角色改 code 时三张关联表同事务级联）与软删回收；物理外键会让
-- `ON DELETE` 语义与本仓「只软删、不硬删」的约定打架，且 PG 的 FK 校验会带来
-- 每行的额外开销。存在性由 service 层校验（`POST /roles` 的 code 查重、
-- grants 提交的 permission_code 逐个校验），与本仓其它表一致。
--
-- ⚠️ 代价：删掉一个 permission code 后 `t_role_permission` 可能留下孤儿行。
--   读侧的 `JOIN t_permission ON ... WHERE p.deleted_at IS NULL` 会自然过滤掉，
--   不会把孤儿 code 喂给前端。
--
-- ### 2）雪花 ID 由应用层生成
-- `id` 一律是 App 侧 `SnowflakeIdGenerator` 生成，不在 DB 里取号，也不建
-- `DEFAULT nextval(...)` —— 与 `t_shelf` / `t_menu` / `t_part` 等表一致。
-- （baseline 里那批 `nextval` 是 Python 期遗留，新表不继承。）
--
-- ### 3）软删语义
-- 三张表**都**带 `deleted_at`，读侧一律 `deleted_at IS NULL`。
-- - `t_role.code` / `t_permission.code` 的唯一索引是 **partial**（`WHERE
--   deleted_at IS NULL`，照 `uk_t_menu_code` 的写法）：软删后同一个 code 可以
--   重新启用而不撞唯一约束，同时保留旧行的审计痕迹。
-- - `t_role_permission` 的唯一索引同样是 partial（`role_code, permission_code`
--   WHERE deleted_at IS NULL）；grants 整包替换走「先软删旧行、再插新行」，
--   重复跑不会叠行。
--
-- ## `is_system` 的作用
-- `t_role.is_system = true` 标记**内置角色**（ADMIN / MANAGER / CLERK /
-- INSPECTOR / CNC_PROGRAMMER / SHELF_ACCOUNT，六个，见 `seeds/rbac.sql`）。
-- 内置角色可停用不可删、code 不可改；自定义角色（`is_system = false`）code 可改，
-- 改名时三张关联表同事务级联。
--
-- ## 与 `seeds/menu.sql` 的分工
-- 本文件只建**表**。表里的初始数据（六个内置角色、权限点、授权矩阵）由
-- `seeds/rbac.sql` 声明式种子灌；菜单树仍是 `seeds/menu.sql` 的活，两者关注点
-- 分离（菜单管「看得见的入口」，本表管「能做什么操作」）。
-- ============================================================================

-- ---------------------------------------------------------------------------
-- t_role：角色定义
-- ---------------------------------------------------------------------------
-- `is_system` 区分内置 / 自定义（见文件头说明）；`is_active` 停用位与软删
-- `deleted_at` 分开：停用保留行、只是不能被授予与登录，删除才带审计痕迹。
CREATE TABLE public.t_role (
    id          bigint NOT NULL,
    -- 角色码（大写）。partial unique `uk_t_role_code` 保证未软删行内唯一。
    code        character varying(20) NOT NULL,
    -- 中文展示名
    name        character varying(50) NOT NULL,
    description text,
    -- 内置角色标记（true = 代码级语义，改名改 code 都受限）
    is_system   boolean DEFAULT false NOT NULL,
    -- 停用位；false 时不能被授予新用户（存量授予行不受影响，但登录取不到权限）
    is_active   boolean DEFAULT true NOT NULL,
    version     integer DEFAULT 0 NOT NULL,

    created_at  timestamp without time zone DEFAULT now() NOT NULL,
    created_by  bigint,
    updated_at  timestamp without time zone DEFAULT now() NOT NULL,
    updated_by  bigint,
    deleted_at  timestamp without time zone
);

COMMENT ON TABLE  public.t_role IS '角色定义（iam::rbac 域）；is_system=true 为内置角色';
COMMENT ON COLUMN public.t_role.id IS '雪花主键，App 侧生成';
COMMENT ON COLUMN public.t_role.code IS '角色码（大写 varchar）；与 t_user_role.role / t_role_menu.role 同源；Rust 侧 auth::rbac::Role 枚举只覆盖内置六项';
COMMENT ON COLUMN public.t_role.name IS '中文展示名';
COMMENT ON COLUMN public.t_role.is_system IS '内置角色标记；true 时 code 不可改、不可删除（只可停用）';
COMMENT ON COLUMN public.t_role.is_active IS '停用位；false 时不可被新授予，存量用户下次登录起取不到该角色的授权';
COMMENT ON COLUMN public.t_role.version IS '乐观锁版本号；POST /roles/{id}/update 与 grants 整包替换共用此锚点，冲突抛 BIZ_VERSION_CONFLICT 409';

CREATE UNIQUE INDEX uk_t_role_code ON public.t_role USING btree (code) WHERE (deleted_at IS NULL);

-- ---------------------------------------------------------------------------
-- t_permission：权限点定义
-- ---------------------------------------------------------------------------
-- `code` 用 `<域>.<操作>` 的点分形态（如 `part.drawing.print` / `queue.dispatch`），
-- `domain` 是它的**首段**，供 `GET /iam/permissions` 按域分组展示用（读侧可从
-- code 前缀推导，但显式存一列让分组查询不必做字符串切割）。
--
-- `is_active = false` = 该权限点已下线：不出现在面板的可选清单里，读侧授权
-- 联查时也会过滤掉（存量 `t_role_permission` 行不受影响，等同收回）。
CREATE TABLE public.t_permission (
    id          bigint NOT NULL,
    -- 权限码（点分，如 `part.drawing.print`），partial unique
    code        character varying(64) NOT NULL,
    name        character varying(50) NOT NULL,
    -- 业务域（`code` 的首段）：part / queue / delivery_note …
    domain      character varying(50) NOT NULL,
    description text,
    is_active   boolean DEFAULT true NOT NULL,
    version     integer DEFAULT 0 NOT NULL,

    created_at  timestamp without time zone DEFAULT now() NOT NULL,
    created_by  bigint,
    updated_at  timestamp without time zone DEFAULT now() NOT NULL,
    updated_by  bigint,
    deleted_at  timestamp without time zone
);

COMMENT ON TABLE  public.t_permission IS '权限点定义（iam::rbac 域）；code 为 <域>.<操作> 点分形态';
COMMENT ON COLUMN public.t_permission.id IS '雪花主键，App 侧生成';
COMMENT ON COLUMN public.t_permission.code IS '权限码；前端角色面板按此串逐项勾选';
COMMENT ON COLUMN public.t_permission.domain IS '业务域（code 首段）；GET /iam/permissions 按此分组';
COMMENT ON COLUMN public.t_permission.is_active IS '下线位；false 时不进面板可选清单，存量授权行读侧联查时一并过滤';

CREATE UNIQUE INDEX uk_t_permission_code ON public.t_permission USING btree (code) WHERE (deleted_at IS NULL);
CREATE INDEX ix_t_permission_domain ON public.t_permission USING btree (domain) WHERE (deleted_at IS NULL);

-- ---------------------------------------------------------------------------
-- t_role_permission：角色 → 权限点授权
-- ---------------------------------------------------------------------------
-- ⚠️ 刻意**无 FK、无 `role_id` 外键列**：关联键是 `role_code` varchar，
--   与 `t_role_menu.role`（同样是 varchar）完全同款。改角色 code 时由 service 在
--   同一事务里 UPDATE 本表 + `t_user_role` + `t_role_menu` 三张表级联。
--
-- ⚠️ ADMIN 元角色**不写行**：它的通配在代码里（`CurrentUser::require_permission`
--   对 `Role::Admin` 直接短路），不靠数据。写进去反而会让人以为「元角色也走
--   数据授权」，将来漏改一行就把通配语义悄悄降级成逐项勾选。
CREATE TABLE public.t_role_permission (
    id              bigint NOT NULL,
    role_code       character varying(20) NOT NULL,
    permission_code character varying(64) NOT NULL,
    version         integer DEFAULT 0 NOT NULL,

    created_at      timestamp without time zone DEFAULT now() NOT NULL,
    created_by      bigint,
    updated_at      timestamp without time zone DEFAULT now() NOT NULL,
    updated_by      bigint,
    deleted_at      timestamp without time zone
);

COMMENT ON TABLE  public.t_role_permission IS '角色→权限点授权（iam::rbac 域）；ADMIN 元角色不写行，通配在代码里';
COMMENT ON COLUMN public.t_role_permission.role_code IS '角色码 varchar（无 FK），与 t_role.code / t_user_role.role 同源';
COMMENT ON COLUMN public.t_role_permission.permission_code IS '权限码 varchar（无 FK），与 t_permission.code 同源';

CREATE UNIQUE INDEX uk_t_role_permission_role_perm
    ON public.t_role_permission USING btree (role_code, permission_code)
    WHERE (deleted_at IS NULL);
CREATE INDEX ix_t_role_permission_role ON public.t_role_permission USING btree (role_code) WHERE (deleted_at IS NULL);