-- ============================================================================
--  客户树**任意深度** fixture（2026-10-17 新增，客户树无限层级化 Gate E）
--
--  加载入口：test-support::fixture::load_customer_tree_fixture(pool)
--  加载方式：sqlx::raw_sql(include_str!("../../fixtures/customer_tree.sql")).execute(pool).await
--
--  ## 为什么单独一个文件（不并进 part.sql / customer.sql）
--  既有 fixture 的 id / name 是**大量集成测试的公共基线**（`PartFixture::CUSTOMER_L1_ID`
--  = 10、`CUSTOMER_L2_ID` = 11 被 10+ 个 sub-file 引用）。往既有 SQL 里加节点会
--  ① 让「列表返回 N 条」类断言多出 1 行；② 让「整棵树」类断言的根数变化。本文件
--  用**独立 id 段**（60-69，与 part 10-49 / delivery 50-52 物理不相交）+ 独立
--  加载函数，**对既有 fixture 零影响** —— 只有显式调 `load_customer_tree_fixture`
--  的用例才会看到这些行。
--
--  ## 树形（与 tests/customer_tree_depth.rs 的 helper 版同形状，供「不想在测试里
--   现造树」的用例直接复用）
--
--  ```text
--  FX 深层集团(L1, serial_prefix='Q')            id 60
--  └─ FX 深层事业群(L2)                          id 61   ← 中间层（非叶子）
--     ├─ FX 深层事业部(L3)                       id 62   ← 中间层（非叶子）
--     │  ├─ FX 深层车间(L4)                      id 63   ← 叶子
--     │  └─ FX 深层班组(L4)                      id 64   ← 叶子（63 的兄弟）
--     └─ FX 深层分部(L3)                         id 65   ← 叶子（62 的兄弟）
--  ```
--
--  `customer_path` 口径（第 2 层到叶子，不含根）：
--  - 60 → `FX 深层集团`
--  - 61 → `FX 深层事业群`
--  - 62 → `FX 深层事业群 / FX 深层事业部`
--  - 63 → `FX 深层事业群 / FX 深层事业部 / FX 深层车间`
--
--  ## ID 段分配（60-69；statistics / user_repo 等其它 fixture 段见各自文件头）
--  TREE_L1_ID        60   根（带 serial_prefix='Q'）
--  TREE_L2_ID        61   非根（中间层）
--  TREE_L3_A_ID      62   非根（中间层）
--  TREE_L4_A_ID      63   叶子
--  TREE_L4_B_ID      64   叶子（63 的兄弟）
--  TREE_L3_B_ID      65   叶子（62 的兄弟）
--
--  ## 序列号计数器
--  建单 / 建送货单会派发序列号，根缺 counter 会被 20108 先拒。与 part.sql 同理，
--  这里给 'Q' 播一行（counter=0 ⇒ 首个号 Q1000）。
-- ============================================================================

INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, created_at, updated_at)
VALUES
  (9000000000000000060, 'FX 深层集团',     NULL,                       'Q', 0, now(), now()),
  (9000000000000000061, 'FX 深层事业群',   9000000000000000060,        NULL, 0, now(), now()),
  (9000000000000000062, 'FX 深层事业部',   9000000000000000061,        NULL, 0, now(), now()),
  (9000000000000000063, 'FX 深层车间',     9000000000000000062,        NULL, 0, now(), now()),
  (9000000000000000064, 'FX 深层班组',     9000000000000000062,        NULL, 0, now(), now()),
  (9000000000000000065, 'FX 深层分部',     9000000000000000061,        NULL, 0, now(), now());

INSERT INTO t_serial_counter (prefix, counter, version, created_at, updated_at)
VALUES ('Q', 0, 0, now(), now())
ON CONFLICT (prefix) DO NOTHING;