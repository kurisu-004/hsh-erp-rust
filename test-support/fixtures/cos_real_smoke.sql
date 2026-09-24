-- ============================================================================
--  cos_real_smoke 域集成测试 fixture (PR13 Phase I, 2026-09-24)
--
--  加载入口：test-support::fixture::load_cos_real_smoke_fixture(pool)
--  加载方式：sqlx::raw_sql(include_str!("../../fixtures/cos_real_smoke.sql")).execute(pool).await
--
--  ## 设计原则
--  - 所有 ID 走常量 9_000_000_000_000_000_220+ 区段（physical 不相交）。
--
--  ## ID 段分配（220+）
--  -- (空 stub)
--
--  ## 不预置任何表
--  cos_real_smoke.rs 是 opt-in 真 COS 烟雾测试（`#[ignore]`），用 OpenDalCos 直连
--  腾讯云 COS 凭据从环境变量读取。本 fixture 是空 stub，仅占位 ID 段 220-229。
-- ============================================================================

-- （本 fixture 为 stub：cos_real_smoke 不依赖任何 DB 数据）
SELECT 1;