-- ============================================================================
--  企业微信登录域集成测试 fixture（2026-09-29 新增）
--
--  加载入口：test-support::fixture::load_wecom_fixture(pool)
--  加载方式：sqlx::raw_sql(include_str!("../../fixtures/wecom.sql")).execute(pool).await
--
--  ## 前置
--  必须先加载 `load_iam_fixture(&pool)`（本 fixture 只建 t_wx_identity 映射行，
--  其 user_id 全部指向 iam fixture 的 5 个常量用户）。
--
--  ## ID 段分配（120+）
--  MANAGER_BIND_ID  120  fx_wx_manager  → fx_iam_manager   (110, MANAGER)
--  CLERK_BIND_ID    121  fx_wx_clerk    → fx_iam_clerk     (111, CLERK)
--  LONELY_BIND_ID   122  fx_wx_lonely   → fx_iam_lonely    (112, 无角色 → 20606)
--  INACTIVE_BIND_ID 123  fx_wx_inactive → fx_iam_inactive  (114, 停用 → 40101)
--  TARGET_BIND_ID   124  fx_wx_target   → fx_iam_target    (113, 用于重复绑 / 解绑后重绑)
--
--  ## corp_id 约定
--  全部用 FIXTURE_CORP_ID（'ww-fixture-corp'）。测试构造 state 时用
--  `test_state_with_wecom(pool, mock, WecomFixture::CORP_ID)` 注入同名 corpid，
--  wx-login 才会通过 corpid 比对（否则返 40107）。
--  「corpid 不符」场景用固定串 'ww-other-corp' 喂给 mock。
--
--  ## wx_user_id 一律小写
--  绑定端点会把 userid 转小写存储，fixture 字面值保持小写以便 wx-login 命中。
-- ============================================================================

-- ---- 4 条绑定（覆盖 200 / 20606 / 40101 三类登录结果）+ 1 条管理端点专用 ----
INSERT INTO t_wx_identity (id, corp_id, wx_user_id, user_id, version, created_at, updated_at) VALUES
  (9000000000000000120, 'ww-fixture-corp', 'fx_wx_manager',  9000000000000000110, 0, now(), now()),
  (9000000000000000121, 'ww-fixture-corp', 'fx_wx_clerk',    9000000000000000111, 0, now(), now()),
  (9000000000000000122, 'ww-fixture-corp', 'fx_wx_lonely',   9000000000000000112, 0, now(), now()),
  (9000000000000000123, 'ww-fixture-corp', 'fx_wx_inactive', 9000000000000000114, 0, now(), now()),
  (9000000000000000124, 'ww-fixture-corp', 'fx_wx_target',   9000000000000000113, 0, now(), now());
