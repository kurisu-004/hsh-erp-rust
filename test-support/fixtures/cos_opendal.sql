-- ============================================================================
--  cos_opendal 域集成测试 fixture (PR13 Phase I, 2026-09-24)
--
--  加载入口：test-support::fixture::load_cos_opendal_fixture(pool)
--  加载方式：sqlx::raw_sql(include_str!("../../fixtures/cos_opendal.sql")).execute(pool).await
--
--  ## 设计原则
--  - 所有 ID 走常量 9_000_000_000_000_000_210+ 区段（physical 不相交）。
--
--  ## ID 段分配（210+）
--  -- (空 stub)
--
--  ## 不预置任何表
--  cos_opendal_api.rs 是 COS 客户端单测 + AppConfig 解析路径测试，不依赖 PG / Redis
--  （用 NoopOpenDal Memory backend + 临时 RSA PEM 文件）。本 fixture 是空 stub，
--  仅占位 ID 段 210-219。
-- ============================================================================

-- （本 fixture 为 stub：cos_opendal_api 不依赖任何 DB 数据）
SELECT 1;