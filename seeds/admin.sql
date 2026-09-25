-- ============================================================================
-- seeds/admin.sql —— 可选初始管理员账号种子（由 BOOTSTRAP_ADMIN_ENABLED 门控，2026-09-26 新增）
-- ============================================================================
--
-- 应用方式：
--   1. App 启动钩子自动跑（见 src/infra/seed.rs）—— 由环境变量 BOOTSTRAP_ADMIN_ENABLED 门控，默认关闭
--   2. 手工：BOOTSTRAP_ADMIN_ENABLED=true 后 psql $DATABASE_URL -v ON_ERROR_STOP=1 -f seeds/admin.sql
--
-- 编写约定：
--   * 初始管理员账号固定 username=admin / password=changeme / role=MANAGER（无 scope）
--   * id 用静态常量 900000000000000001 / 900000000000000002（与 production 雪花 ID 段物理不相交）
--   * bcrypt 哈希字面值复用 test-support/fixtures/iam.sql（同 cost=12、同明文）
--   * ON CONFLICT 幂等：t_user ON CONFLICT (username) WHERE deleted_at IS NULL DO NOTHING；
--     t_user_role ON CONFLICT DO NOTHING（由 PG 推断唯一约束）
--
-- 安全语义（必须遵守）：
--   * BOOTSTRAP_ADMIN_ENABLED 默认 false；生产环境绝不允许开启
--   * 首次启用后必须立刻：登录 → 修改密码 → 设回 BOOTSTRAP_ADMIN_ENABLED=false → 重启
--   * 明文密码 `changeme` 与 src/modules/iam/service/account.rs::DEFAULT_RESET_PASSWORD 同源
--
-- 域内校验：
--   * t_user.username 唯一（uk_t_user_username WHERE deleted_at IS NULL）
--   * t_user_role (user_id, role, scope_type, scope_id) 唯一
-- ============================================================================

-- ---- 初始管理员用户（密码明文 "changeme"，bcrypt cost=12）----
INSERT INTO t_user (
    id, username, password_hash, full_name, is_active,
    refresh_token_version, version, created_at, created_by,
    updated_at, updated_by
) VALUES (
    900000000000000001,
    'admin',
    '$2b$12$KjlHPD7WcAbXC1v6RZxXKOfFHGJJDGS4bxfKGPRnLBmWmqFMvS/4W',
    'Initial Administrator',
    true,
    0, 0, now(), NULL,
    now(), NULL
)
ON CONFLICT (username) WHERE deleted_at IS NULL DO NOTHING;

-- ---- 初始管理员角色（MANAGER，无 scope）----
INSERT INTO t_user_role (
    id, user_id, role, scope_type, scope_id,
    version, created_at, created_by,
    updated_at, updated_by
) VALUES (
    900000000000000002,
    900000000000000001,
    'MANAGER',
    NULL, NULL,
    0, now(), NULL,
    now(), NULL
)
ON CONFLICT DO NOTHING;
