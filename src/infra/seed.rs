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
//! ## 应用时机
//!
//! `run_seeds()` 由 `src/main.rs` 在 `sqlx::migrate!()` 之后调用，启动即跑一次。
//! 种子文件本身幂等（多次跑结果一致），可手工 `psql $DATABASE_URL -f seeds/menu.sql`
//! 触发（见 scripts/seed_apply.sh）。
//!
//! ## 编译期 SQL 校验
//!
//! 本模块用 `sqlx::raw_sql`，**不**走 `query!` 编译期校验宏，因此改动 seeds/menu.sql
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

/// 跑全部 seeds。启动时调用，幂等。
///
/// 当前只装载菜单种子（seeds/menu.sql）。后续如需抽离其它配置数据（用户、
/// 工种、工序等），在此处追加 `include_str!` + `sqlx::raw_sql` 调用即可。
pub async fn run_seeds(pool: &PgPool) -> Result<(), sqlx::Error> {
    tracing::info!("应用 seeds/menu.sql ...");
    sqlx::raw_sql(MENU_SEED).execute(pool).await?;
    tracing::info!("✓ seeds/menu.sql 应用完成");
    Ok(())
}