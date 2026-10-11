//! seeds/ 目录声明式种子加载
//!
//! 2026-09-25 sqlx 接管后新增：菜单等配置数据从 migration 抽离到 seeds/，由启动钩子
//! 自动应用。
//!
//! ## 设计动机
//!
//! 修改菜单曾经是"改 migration 文件 + 算 SHA384 + 同步 _sqlx_migrations.checksum
//! + 部署"四步曲，每次都要担心硬编码 ID 撞号、ON CONFLICT 守卫、optimistic
//!   lock +1。重构后改为"改 seeds/menu.sql + 重启 app"两步，零摩擦。
//!
//! seeds/menu.sql 是声明式的：
//!   * INSERT ... ON CONFLICT (code) WHERE deleted_at IS NULL DO UPDATE
//!   * t_menu.id 用静态 ID（9000000000xxx 段），便于识别"seed 灌的"
//   * 已存在的雪花 ID 行 ON CONFLICT 后保留原 ID，只更新其他字段
//!   * 不自动删"未在文件声明的菜单"——菜单下线必须显式列在 soft-delete 区段
//!
//! ## 初始管理员 seed（2026-09-26 新增）
//!
//! seeds/admin.sql 是**可选**的初始管理员账号（username=admin / password=changeme /
//! role=ADMIN），由环境变量 `BOOTSTRAP_ADMIN_ENABLED` 门控（默认 false）。首次启用后
//! **必须**立刻登录并改密，再设回 `BOOTSTRAP_ADMIN_ENABLED=false` 重启（明文密码
//! 与 `src/modules/iam/service/account/mod.rs::DEFAULT_RESET_PASSWORD` 同源）。
//!
//! ## RBAC 基线 seed（2026-10-14 新增）
//!
//! seeds/rbac.sql 装载 RBAC 基线数据：6 个内置角色（`t_role`），BE-8 起追加权限点
//! 与系统角色授权矩阵（`t_permission` / `t_role_permission`），2026-10-15 起并入
//! 角色→菜单**首次基线**（`t_role_menu`）。**无条件加载**，与
//! `BOOTSTRAP_ADMIN_ENABLED` 解耦：角色 / 权限点 / 授权基线是面板与业务端点的
//! 判定基线，初始管理员账号只是可选的开户动作。
//!
//! 与 menu.sql 的关注点分离：menu.sql 管「菜单目录」（t_menu 菜单树，部署配置、
//! 与前端路由耦合），rbac.sql 管「角色 / 权限点 / 授权基线」（角色定义 + 权限点 +
//! 角色→权限点基线 + 角色→菜单首次基线）。两份都幂等，改动只需
//! 重启（`sqlx::raw_sql`，不走 `query!`，无需 `sqlx prepare`）。
//!
//! ## 应用时机
//!
//! `run_seeds()` 由 `src/main.rs` 在 `sqlx::migrate!()` 之后调用，启动即跑一次。
//! 种子文件本身幂等（多次跑结果一致），可手工跑 scripts/seed_apply.sh 触发
//! （默认按序应用 menu → rbac，与启动钩子一致）。
//!
//! ## 编译期 SQL 校验
//!
//! 本模块用 `sqlx::raw_sql`，**不**走 `query!` 编译期校验宏，因此改动 seeds/*.sql
//! 不需要重跑 `./scripts/sqlx_prepare.sh`。这是有意为之——seeds 是声明式数据，
//! 不是参数化查询。
//!
//! ## 调用方
//!
//! - `src/main.rs`：启动时自动跑（默认开启）
//! - `scripts/seed_apply.sh`：手工触发入口

use sqlx::PgPool;

/// 菜单种子 SQL（编译期嵌入，避免运行时 IO 路径问题）。
const MENU_SEED: &str = include_str!("../../seeds/menu.sql");

/// RBAC 基线种子 SQL（编译期嵌入）：内置角色 + 权限点 + 授权矩阵。
///
/// 无条件加载，与 `BOOTSTRAP_ADMIN_ENABLED` 解耦（见模块 doc）。
const RBAC_SEED: &str = include_str!("../../seeds/rbac.sql");

/// 初始管理员账号种子 SQL（编译期嵌入）。
///
/// 仅当 `BOOTSTRAP_ADMIN_ENABLED=true` 时执行。详见模块顶部 doc + `seeds/README.md`。
const ADMIN_SEED: &str = include_str!("../../seeds/admin.sql");

/// 跑全部 seeds。启动时调用，幂等。
///
/// 装载顺序：
/// - `seeds/menu.sql` —— 菜单树（无条件加载）
/// - `seeds/rbac.sql` —— RBAC 基线：内置角色 + 权限点 + 授权矩阵（无条件加载，
///   与 admin seed 解耦 —— 关掉初始管理员开关后 `t_role` 仍有 6 行）
/// - `seeds/admin.sql` —— 初始管理员账号（仅当 `bootstrap_admin_enabled=true`）
///
/// 后续如需抽离其它配置数据（用户、工种、工序等），在此处追加 `include_str!` +
/// `sqlx::raw_sql` 调用即可。
pub async fn run_seeds(pool: &PgPool, bootstrap_admin_enabled: bool) -> Result<(), sqlx::Error> {
    tracing::info!("应用 seeds/menu.sql ...");
    sqlx::raw_sql(MENU_SEED).execute(pool).await?;
    tracing::info!("✓ seeds/menu.sql 应用完成");

    run_rbac_seed(pool).await?;

    if bootstrap_admin_enabled {
        tracing::info!("BOOTSTRAP_ADMIN_ENABLED=true，应用 seeds/admin.sql（初始管理员账号）");
        sqlx::raw_sql(ADMIN_SEED).execute(pool).await?;
        tracing::info!("✓ seeds/admin.sql 应用完成；务必登录 admin/changeme 后改密并关闭 env");
        Ok(())
    } else {
        tracing::info!("BOOTSTRAP_ADMIN_ENABLED=false，跳过 seeds/admin.sql");
        Ok(())
    }
}

/// 应用 RBAC 基线 seed（菜单之后、admin 之前）。
///
/// 单独成函数便于 `tests/iam/bootstrap_admin_seed.rs` 直接验「关掉 admin 门控
/// 时 t_role 仍有 6 行」这条解耦契约。
pub async fn run_rbac_seed(pool: &PgPool) -> Result<(), sqlx::Error> {
    tracing::info!("应用 seeds/rbac.sql ...");
    sqlx::raw_sql(RBAC_SEED).execute(pool).await?;
    tracing::info!("✓ seeds/rbac.sql 应用完成");
    Ok(())
}
