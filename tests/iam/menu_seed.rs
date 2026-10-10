//! `seeds/menu.sql` 授权矩阵回归测试（2026-10-05 新增）
//!
//! 钉住 commit `420652b`（「2026-10-05 收紧生产管理菜单可见性」）的授权口径，
//! 防止将来有人把 code 悄悄加回 `seeds/menu.sql` 第 4 节白名单、或删掉第 4.7 段
//! 回收区段。**一行 Rust 产品代码都不改**——纯 seed 可见性变更，回归护栏只落测试。
//!
//! ## 用户需求原文（2026-10-05）
//!
//! 「收紧权限，生产管理下的工序工种、制定工序菜单需要 MANAGER，生产队列菜单需要
//! CLERK，品检看不到这三个菜单才对」
//!
//! | menuCode          | 菜单   | MANAGER | CLERK | INSPECTOR |
//! |-------------------|--------|:-------:|:-----:|:---------:|
//! | `process_work_type` | 工序工种 | 保留 | 收回 | 收回 |
//! | `part_process_chain`| 制定工序 | 保留 | 收回 | 收回 |
//! | `worker_queue`     | 生产队列 | 保留 | 保留 | 收回 |
//!
//! `production_group`（生产管理分组）在 MANAGER / CLERK / INSPECTOR 三方都必须保留：
//! INSPECTOR 靠它挂 `inspection_pending`（待品检），CLERK 靠它挂 `worker_queue`。
//!
//! ## 5 个用例各钉什么
//!
//! 1. **`menu_seed_role_matrix`** —— 钉住 `t_role_menu` 授权口径的全量快照
//!    （每角色 code 集合**完全相等**，不是「包含」这种弱断言）。
//! 2. **`menu_seed_revokes_legacy_grants`** —— 钉住「回收作用于存量行」。这是最关键的
//!    一个用例：`seeds/menu.sql` 第 4 节的授权写入是**纯增量**
//!    （`ON CONFLICT (role, menu_id) WHERE deleted_at IS NULL DO NOTHING`），
//!    **从白名单里删 code 并不会回收生产库已存在的授权行**——所以才有第 4.7 段显式
//!    软删回收。若这条用例缺席，将来有人删掉 4.7 段、把 code 加回白名单，测试全绿
//!    而线上菜单没收紧。
//! 3. **`menu_seed_idempotent`** —— 钉住幂等：重跑不产生重复 live 行、可见矩阵逐次
//!    不变、且已被回收的软删行 `version` **不再被 bump**（4.7 段的 `AND rm.deleted_at
//!    IS NULL` 守卫；缺守卫会导致每次启动都 +1）。
//! 4. **`menu_seed_rendered_tree_per_role`** —— 端到端：登录后 `GET /iam/me` 返回的
//!    `data.menus` 树（前端真正消费的数据：`src/layouts/MainLayout.vue:109` 渲染
//!    `auth.menus`、`src/router/index.ts:486` 用 `meta.menuCode` 在这棵树里做成员判断）。
//! 5. **`menu_seed_live_matrix_matches_whitelist`**（2026-10-05 review 第 1 轮 MINOR-1
//!    新增）—— 把「库里的 live 矩阵 == seed 文本里的 4.1-4.5 白名单」这条不变量**从 seed
//!    文本现场解析**出来断言，并断言「4.7 回收元组 ∩ 白名单 == ∅」。关的是前 4 个用例的
//!    盲区：**只从白名单删 code、忘了写进 4.7 段**，在「测试库继承 template 的 live 行」
//!    的前提下不改变任何 live 矩阵 → 前 4 个用例全绿而线上菜单没收紧。
//! 6. **`menu_seed_revives_soft_deleted_grants`**（2026-10-13 新增）—— 钉住 4.0 复活段：
//!    白名单内已被软删的授权行，重跑 seed 后必须**原地复活**（同一个 `id`、`version + 1`），
//!    且**总行数不变**。缺 4.0 段时会插出第二行 —— `uk_t_role_menu_role_menu` 是 partial
//!    （`WHERE deleted_at IS NULL`），软删行不在索引里，`ON CONFLICT` 根本命中不到它。
//!    同一条用例顺带钉住 4.1-4.5 段的**派生 id**：跑完之后所有 live 授权行的 `id` 必须
//!    落在 `9100000000000000..9104999999999999` 派生段内，且两两不等 —— 这正是
//!    2026-10-13 那次 `t_role_menu_pkey` 冲突的回归护栏（见下节）。
//!
//! ## 2026-10-13 事故：`duplicate key value violates unique constraint "t_role_menu_pkey"`
//!
//! 4.1-4.5 段原来用 `900000000100{n}001 + row_number() OVER ()` 发号，那是**按行位置**
//! 而不是按授权身份发号。4.1 的 MANAGER 白名单新增 `mail_group` / `mail_digest` 两个 code
//! 后，两行在 SELECT 结果里换了位置、拿到 `9000000001000030/031`，而 030 已被
//! `MANAGER/workers_list` 占用 → PK 冲突。
//!
//! 关键点：**为什么 `ON CONFLICT` 没兜住** —— `ON CONFLICT (role, menu_id) WHERE
//! deleted_at IS NULL` 只声明了 `uk_t_role_menu_role_menu` 这一个 arbiter；
//! `t_role_menu_pkey` 是另一个，撞它不会被 `DO NOTHING` 吞掉，整条 INSERT 直接失败。
//! 又因为 seed 是 `BEGIN … COMMIT` 单事务，**第 1/2 节的 `t_menu` 插入一并回滚** ——
//! 表现为「migration 成功、菜单却没有」的半吊状态。
//!
//! 修法：id 改为按 `(role, code)` 唯一决定（`9100000000000000 + 角色序号 × 1e12 +
//! md5(role,code) 低 48bit`），与白名单顺序、增删、其他角色完全无关。用例 ⑥ 钉住它。
//!
//! ## 维护约定（改 `seeds/menu.sql` 前必读，2026-10-05 新增）
//!
//! 本文件是 seed 的**回归护栏**，多处断言是**精确集合相等**，不是「包含」。改动 seed 时
//! 漏改对应常量，测试会以「快照不匹配」的红脸失败 —— **这是设计意图，不是误报**，但
//! 失败信息只说「不符」不说「你该改哪个文件」，所以维护约定写在这里：
//!
//! 1. 改 **4.1-4.5 段白名单**（增删任一 role 的任一 code）或增删 `production_group`
//!    下的子菜单时，**必须同步**改这 3 处（缺任一处都会红）：
//!    - `EXPECTED_SNAPSHOT`（= `MANAGER_EXPECTED` / `CLERK_EXPECTED` /
//!      `INSPECTOR_EXPECTED` / `CNC_PROGRAMMER_EXPECTED` / `SHELF_ACCOUNT_EXPECTED`）；
//!    - `HEADLINE`（仅当动的是 3 个目标 code 之一的可见性）；
//!    - `menu_seed_rendered_tree_per_role` 里 3 处 `children_of(...)` 的**精确子节点
//!      集合**断言（MANAGER 6 个 / CLERK 1 个 / INSPECTOR 1 个）—— 给
//!      `production_group` 加一个子菜单就会在这里红。
//! 2. 改 **4.7 段回收元组**时，**必须同步** `REVOKED_PAIRS`（用例 ⑤ 的断言 C 会直接
//!    比对两处来源）。用例会伪造这些 `(role, code)` 的 legacy 存量行来验回收生效，
//!    两处不一致 = 要么伪造的行没被回收、要么期望的期望值错了。
//!    - 同一批改动里，若还动到了 **4.1-4.5 段白名单**，**必须同步 4.0 段的复活元组
//!      列表**（用例 ⑤ 的断言 D 会比对「4.0 名单 == 4.1-4.5 白名单」）。4.0 的 61 条
//!      元组是白名单的手工复制：少了 → 该 code 被软删后再加回白名单时会插出第二行；
//!      多了 → 4.7 回收名单里的 code 会被误复活（当前 4.7 在 4.0 之后跑、同一事务，
//!      净结果仍是软删，但那段顺序契约一改就静默失效）。
//! 3. 白名单与 4.7 回收段**必须成对变更**：第 4 节是 **add-only** 语义，只删白名单
//!    **不会**回收存量授权行，生产库菜单不会消失。用例 ⑤ 的断言 A 就是为了钉住这条
//!    契约（`live == 白名单 − 已软删菜单`）；断言 A 的独立价值在**存量库 / 老 template
//!    / 生产升级**场景（库里的 live 行不是当轮 seed 建的），标准 CI 路径下 template
//!    由当轮 seed 重建，此时抓手是上面 1 里的硬编码快照。
//! 4. **解析器防呆不变量**（2026-10-05 review 第 2 轮 NIT-2 补）——前 3 条说的是
//!    「该同步哪个常量」，这条说的是「**会先撞上哪个 panic**」。改 seed 骨架前先读：
//!    - 4.1-4.5 段角色授权必须**恒为 5 段**（`WHITELIST_SECTIONS`）：加第 6 个角色段
//!      或删掉某段会先 panic「应恰有 5 段…请先修解析器（**不是** seed 写错了）」，
//!      改法是先把 `EXPECTED_SNAPSHOT` 补上，再改 `WHITELIST_SECTIONS`；
//!    - **`AND (rm.role, m.code) IN (` 现在有两处**（4.0 复活段 + 4.7 回收段）。
//!      `revoked_re` / `revived_re` 因此**额外锚定** `AND rm.deleted_at IS NULL` /
//!      `IS NOT NULL` 来区分两段。只靠元组列表形状会把 4.0 的 61 条白名单误读成回收
//!      名单。多写/少写任一段都会 panic「应恰有 1 处…请先修解析器」；
//!    - code 字面量必须匹配 `^[a-z_]+$`：引号写错位置 / 混入非小写字符会 panic。
//!    - 附：code 列表**中间**的 `--` 注释行由 `strip_sql_line_comments` 剥除后才交给
//!      `quoted_re`，所以注释里可自由出现单引号；但注释里若出现 ASCII `)`，
//!      `whitelist_re` 的 `[^)]*` 会提前截断（表现为「缺失若干 code」的 fail-loud）。
//!    - 4.0/4.7 段元组列表里的 `-- 角色（对应 4.x）` 分组注释行会被
//!      `strip_sql_line_comments` 剥除后解析；**注释里不能出现 `('ROLE', 'code')` 形状**，
//!      否则会被当成真元组计进断言 D。
//! 5. **为什么没有「生产升级演练」用例**（2026-10-05 review 第 2 轮 MINOR-1，决定不
//!    做，故记在这里备查）：reviewer 建议在 `test-support/fixtures/` 放一份上一版
//!    `menu.sql` 快照 + 加第 6 个用例，理由是「快照与 seed 一起改 → 白名单删了 + 快照
//!    也改了 + 4.7 漏写」三重手误时无人抓。不做的理由：那份快照是 300 行 SQL 的
//!    **冻结副本**，会随 `seeds/menu.sql` 演化无声变陈旧 —— 维护者改了 seed 却忘了改
//!    fixture，测试就拿一份错误的「上一版」当基线，制造出**比它要防的更隐蔽**的假
//!    失败/假绿；而它要防的是三重手误同时发生，①（硬编码快照）已覆盖最常见的「白名单
//!    删了但没改快照」，⑤ 覆盖存量库场景下的同一不变量。真正的升级演练在 seed 变更时
//!    手工用 psql 跑一次即可（本次变更已跑过 3 次连跑 + 存量→新 seed 升级模拟，见
//!    commit `420652b` 与 review 记录），固化成 fixture 的边际价值低于其陈旧成本。
//!
//! ## 期望值的来源（防 tautology）
//!
//! 下面 5 份 `*_EXPECTED` 快照是**逐条读 `seeds/menu.sql` 第 4.1-4.5 段白名单推导**的，
//! 不是「跑一遍 DB 打印出来」的：
//! - 白名单里的 `settings_root`（第 3.1 节）与 `delivery_dispatch`（第 3.5 节，
//!   2026-10-08 随 `GET /pickup-pending` + `POST /{id}/pickup-scan` 端点下线）被
//!   soft-delete，而 4.x 段的 INSERT 带 `AND m.deleted_at IS NULL`，故二者都不入库；
//! - `delivery_dispatch` 同时已从 4.1 / 4.3 白名单移除，两处都要改 —— 只改一处会让
//!   本用例的断言 A（live == 白名单 − 软删）报「多出 / 缺失」而红，这是有意的 fail-loud；
//! - `floor_group` 只被第 3.2 段置 `is_active = false`（不软删），仍进授权表；
//! - 4.6 段 `DELETE` 会硬删 `settings_root` 等已软删菜单的 role_menu 行。
//!
//! 两类期望值并存、各司其职，**都不许删也不许放宽**：
//! - 硬编码快照（上面 5 份 + `HEADLINE` + `REVOKED_PAIRS`）= 「人读得懂的期望值」，
//!   角色/代码改名或增删时红给改的人看；
//! - 用例 ⑤ 从 `MENU_SEED_SQL` **现场解析**出的白名单 = 「与 DB 对账的权威」，专治
//!   「seed 改了但快照忘了改」与「只删白名单不写 4.7」两类错误。
//!
//! ## 测试策略
//!
//! 与 `bootstrap_admin_seed.rs` 同形：不依赖 `src/main.rs` 启动钩子（避免并行测试间
//! 状态泄露），**测试内直接 `sqlx::raw_sql(include_str!(...))` 触发 seed**——与生产
//! 路径等价（同一份 SQL 文件 + 同样 raw_sql 执行），但时序可控。
//!
//! 注意 fresh database 是从 `hsh_erp_template` 克隆的，而 template 已 apply 过
//! `seeds/menu.sql`（见 `scripts/test_nextest.sh`），故**测试库开箱即处于收紧后的
//! 状态**：①③ 里的 seed 调用此时是 no-op（顺带验幂等），② 必须自己伪造 legacy 存量行。

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use axum::http::StatusCode;
use chrono::NaiveDateTime;
use regex::Regex;
use serde_json::Value;
use sqlx::{PgPool, Row};

use hsh_erp_test_support::{
    IamFixture, json_request, load_iam_fixture, login_token, send, test_app, test_pool, test_state,
};

/// 编译期嵌入 `seeds/menu.sql`（与生产启动钩子 `src/infra/seed.rs` 同一份文件）。
const MENU_SEED_SQL: &str = include_str!("../../seeds/menu.sql");

// ===========================================================================
// 常量：期望值（期望来自「读 seeds/menu.sql 第 4.1-4.5 段」，非跑库生成）
// ===========================================================================

/// 2026-10-05 收紧的 3 个目标 menuCode。
const TARGET_CODES: [&str; 3] = ["process_work_type", "part_process_chain", "worker_queue"];

/// `seeds/menu.sql` 第 4.1 段 MANAGER 白名单去掉两条已 soft-delete 的菜单
/// （第 3.1 节的 `settings_root`、第 3.5 节的 `delivery_dispatch`）后的全量 live 授权
/// code 快照，**30 项**（2026-10-13 新增 `mail_group` + `mail_digest`：交期汇总邮件
/// 的顶级分组与子菜单，域 `mail_digest` 的 6 端点全部 MANAGER 独占，只归管理员）。
const MANAGER_EXPECTED: &[&str] = &[
    "applicants_list",
    "assemblies_list",
    "auth_group",
    "customer_management",
    "customers_list",
    "delivery_notes_manage",
    "floor_group",
    "home",
    "inspection_pending",
    "mail_digest",
    "mail_group",
    "order_group",
    "outsource_companies_list",
    "outsource_list",
    "outsource_quotes_list",
    "outsource_send_receive_list",
    "part_process_chain",
    "parts_list",
    "parts_new",
    "pending_programming",
    "print_templates_designer",
    "process_work_type",
    "production_group",
    "production_stats",
    "repair_receive",
    "shelves_list",
    "template_management",
    "users_list",
    "worker_queue",
    "workers_list",
];

/// 第 4.2 段 CLERK 白名单，**16 项**（2026-10-05 已剔除 process_work_type /
/// part_process_chain；`worker_queue` 与 `production_group` 保留）。
const CLERK_EXPECTED: &[&str] = &[
    "applicants_list",
    "assemblies_list",
    "customer_management",
    "customers_list",
    "delivery_notes_manage",
    "home",
    "order_group",
    "outsource_companies_list",
    "outsource_list",
    "outsource_quotes_list",
    "outsource_send_receive_list",
    "parts_list",
    "parts_new",
    "production_group",
    "repair_receive",
    "worker_queue",
];

/// 第 4.3 段 INSPECTOR 白名单，**8 项**（2026-10-05 已剔除 3 个生产管理子菜单；
/// 2026-10-08 `delivery_dispatch` 整条下线后也不再出现；`production_group` /
/// `inspection_pending` 保留）。
const INSPECTOR_EXPECTED: &[&str] = &[
    "assemblies_list",
    "delivery_notes_manage",
    "home",
    "inspection_pending",
    "outsource_send_receive_list",
    "parts_list",
    "production_group",
    "repair_receive",
];

/// 第 4.4 段 CNC_PROGRAMMER 白名单，**3 项**。
const CNC_PROGRAMMER_EXPECTED: &[&str] = &["home", "parts_list", "pending_programming"];

/// 第 4.5 段 SHELF_ACCOUNT 白名单，**3 项**。
const SHELF_ACCOUNT_EXPECTED: &[&str] = &["floor_group", "home", "scan_badge"];

/// 5 份快照 = 30 + 16 + 8 + 3 + 3 = **60 条 live 授权**（30 = 28 + 2026-10-13 新增
/// 的 `mail_group` / `mail_digest`）。
const EXPECTED_SNAPSHOT: [(&str, &[&str]); 5] = [
    ("MANAGER", MANAGER_EXPECTED),
    ("CLERK", CLERK_EXPECTED),
    ("INSPECTOR", INSPECTOR_EXPECTED),
    ("CNC_PROGRAMMER", CNC_PROGRAMMER_EXPECTED),
    ("SHELF_ACCOUNT", SHELF_ACCOUNT_EXPECTED),
];

/// headline 口径：3 个目标 code 的逐角色可见集（用户需求原文的直接翻译）。
const HEADLINE: [(&str, &[&str]); 5] = [
    (
        "MANAGER",
        &["part_process_chain", "process_work_type", "worker_queue"],
    ),
    ("CLERK", &["worker_queue"]),
    ("INSPECTOR", &[]),
    ("CNC_PROGRAMMER", &[]),
    ("SHELF_ACCOUNT", &[]),
];

/// `seeds/menu.sql` 第 4.7 段回收的 5 对 `(role, menu_code)` —— 与 SQL 里的元组列表
/// 逐字对应。将来有人改 4.7 段，本常量会与之对不上（见
/// `menu_seed_revokes_legacy_grants`）。
const REVOKED_PAIRS: [(&str, &str); 5] = [
    ("CLERK", "process_work_type"),
    ("CLERK", "part_process_chain"),
    ("INSPECTOR", "process_work_type"),
    ("INSPECTOR", "part_process_chain"),
    ("INSPECTOR", "worker_queue"),
];

/// 伪造 legacy 授权行用的自定义 id 段（2026-10-05）。物理上与三方 ID 段都不相交：
/// - seed 派生段 `9100000000000xxx`（4.1-4.5 段，`9100000000000000 + 角色序号×1e12 + md5`）
/// - 旧 seed 静态段 `900000000100{0..4}xxx`（2026-10-13 之前的 `row_number()` 方案；
///   存量行仍用这个段，但 seed 已不再发这个段的新号）
/// - `t_menu` 静态段 `9000000000xxx`
/// - fixture 段 `9_000_000_000_000_000_1xx`（1e18 量级，雪花 ID 空间）
const LEGACY_GRANT_ID_BASE: i64 = 9_000_000_009_000_001;

/// ④ 用例给 `fx_iam_target` 插 INSPECTOR 角色的 `t_user_role.id`（同样避开
/// fixture 段 `9_000_000_000_000_000_115/116`）。
const INSPECTOR_USER_ROLE_ID: i64 = 9_000_000_009_000_101;

// ===========================================================================
// seed 文本解析（2026-10-05 review 第 1 轮 MINOR-1 新增）
// ===========================================================================
// 目的：把「库里的 live 授权矩阵 == seeds/menu.sql 第 4.1-4.5 段白名单」这条不变量
// **从 seed 文本现场解析**出来断言，而不是靠硬编码快照间接覆盖。理由：硬编码快照钉住
// 的是「今天是什么样」，而「只从白名单删掉 code、忘了写进 4.7 段回收」这种错在
// fresh DB（从已 seed 的 template 克隆）里**不改变任何 live 矩阵** → 快照全绿。
//
// 5 段 role_menu INSERT 的骨架是固定的（见 seeds/menu.sql 第 4.1-4.5 段）：
//   INSERT INTO t_role_menu (id, role, ...)
//   SELECT  9100000000000000 + {角色序号} * 1000000000000 + md5(role,code) 低 48bit,
//           '<ROLE>',  m.id,  ...
//   FROM t_menu m
//   WHERE m.code IN ( 'a', 'b', ... )  AND m.deleted_at IS NULL
//   ON CONFLICT (role, menu_id) WHERE deleted_at IS NULL DO NOTHING;
// 4.0 / 4.6 / 4.7 段分别是 UPDATE / DELETE / UPDATE，均不含 `INSERT INTO t_role_menu`，
// 故不会误匹配。
//
// 2026-10-13 更新：4.1-4.5 段的 id 表达式已从 `900000000100{n}001 + row_number() OVER ()`
// 改成「按 (role, code) 派生」。`whitelist_re` 只锚定 `'<ROLE>', m.id,` 这一段，对 id
// 表达式不敏感，故解析结果不变。

/// 角色 → 白名单 code 集合（4.1-4.5 段解析结果）。
type Whitelist = BTreeMap<String, BTreeSet<String>>;

/// `('CLERK', 'process_work_type')` 形式的二元组（4.0 复活名单 / 4.7 回收名单共用）。
type RevokedPairs = BTreeSet<(String, String)>;

/// 段数 = 5（MANAGER / CLERK / INSPECTOR / CNC_PROGRAMMER / SHELF_ACCOUNT）。
const WHITELIST_SECTIONS: usize = 5;

/// 4.1-4.5 段的 INSERT 骨架（DOTALL + 非贪婪）。
fn whitelist_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?s)INSERT INTO t_role_menu \(id, role,.*?'(?P<role>[A-Z_]+)',\s*m\.id,.*?WHERE m\.code IN \((?P<codes>[^)]*)\)",
        )
        .expect("4.1-4.5 段 INSERT 骨架正则编译失败")
    })
}

/// 4.0 复活段 / 4.7 回收段的 `AND (rm.role, m.code) IN ( ... )` 元组列表。
///
/// ⚠️ 2026-10-13：4.0 新增了同一形状的元组列表（`AND rm.deleted_at IS NOT NULL` + 同一个
/// `IN (...)`），所以只锚 `AND (rm.role, m.code) IN (` 会**同时命中两段**、把 4.0 的 61 条
/// 白名单误当成回收名单。必须再锚定 `AND rm.deleted_at IS NULL` —— 4.0 用的是
/// `IS NOT NULL`（复活 live 之外的那些），4.7 用的是 `IS NULL`（只回收 live 行），
/// 两者互斥且不会互相误匹配。这一锚点同时顺带钉住了 4.7 的幂等守卫。
///
/// 终止锚点是语句末的 `\n\s*);`（不是 `)`）—— 元组本身带括号，用 `[^)]*` 会在第一个
/// `('CLERK',` 处就截断。锚点失配时 `captures_iter` 命中 0 处 → `parse_revoked_pairs`
/// 直接 panic（防呆要求），不会静默变成「4.7 段为空」。
fn revoked_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?s)AND rm\.deleted_at IS NULL\s*\n\s*AND \(rm\.role, m\.code\) IN \((?P<tuples>.*?)\n\s*\);",
        )
        .expect("4.7 段元组列表正则编译失败")
    })
}

/// 4.0 复活段的元组列表（`AND rm.deleted_at IS NOT NULL` + 同一个 `IN (...)`）。
///
/// 与 `revoked_re` 的区别只在 `IS NOT NULL` / `IS NULL`，从而把 4.0 和 4.7 分开命中。
fn revived_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?s)AND rm\.deleted_at IS NOT NULL\s*\n\s*AND \(rm\.role, m\.code\) IN \((?P<tuples>.*?)\n\s*\);",
        )
        .expect("4.0 段元组列表正则编译失败")
    })
}

/// 4.1-4.5 段的**派生 id 表达式**（2026-10-13 新增；`t_role_menu_pkey` 事故的回归护栏）。
///
/// 必须逐段匹配：
/// ```text
/// 9100000000000000 + <ord> * 1000000000000
///     + (('x' || substr(md5('t_role_menu:<ROLE>:' || m.code), 1, 15))::bit(60)::bigint % 1000000000000)
/// ```
/// 只锚定到 `md5('t_role_menu:<ROLE>:'` 为止 —— 尾部是纯表达式、无需逐字钉，锚到这
/// 里已足够把「按 (role, code) 唯一决定」和「按行位置发号（`row_number() OVER ()`）」、
/// 「角色序号写错」三种形态区分开。
///
/// 匹配数必须恒为 5（`WHITELIST_SECTIONS`），与 `whitelist_re` 互为交叉校验：前者钉 id
/// 表达式形态，后者钉角色与白名单。
fn grant_id_expr_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"9100000000000000 \+ (?P<ord>[0-9]) \* 1000000000000\s*\+\s*\(\('x' \|\| substr\(md5\('t_role_menu:(?P<role>[A-Z_]+):'",
        )
        .expect("派生 id 表达式正则编译失败")
    })
}

/// 单个 `('ROLE', 'code')` 元组（4.7 段里是逗号对齐排版，故 `\s*` 放宽空白）。
fn revoked_pair_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\(\s*'(?P<role>[A-Z_]+)'\s*,\s*'(?P<code>[a-z_]+)'\s*\)")
            .expect("4.7 段单个元组正则编译失败")
    })
}

/// 单引号字符串片段（从 `WHERE m.code IN (...)` 里取 code 原文）。
fn quoted_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"'[^']*'").expect("单引号片段正则编译失败"))
}

/// menu code 的合法字面值（`t_menu.code` 全是小写下划线；用于解析器防呆）。
fn code_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[a-z_]+$").expect("code 正则编译失败"))
}

/// 从 4.1-4.5 段解析出「角色 → code 集合」白名单。
///
/// **防呆是硬要求**（2026-10-05）：解析失败必须 panic，绝不允许「解析不到 → 集合为空
/// → 断言恰好通过」这种静默假绿。故下列任一情况都直接 panic：
/// - 解析出的段数 ≠ 5（seed 骨架变了 / 某段被删）；
/// - 某个角色的 `code IN (...)` 解析出 0 个 code；
/// - 任一 code 不匹配 `^[a-z_]+$`（说明引号/截断位置不对，或 code 写法变了）；
/// - 同一角色出现两段 INSERT（正则跨段误匹配）。
fn parse_whitelist(sql: &str) -> Whitelist {
    let caps: Vec<_> = whitelist_re().captures_iter(sql).collect();
    assert_eq!(
        caps.len(),
        WHITELIST_SECTIONS,
        "seeds/menu.sql 第 4.1-4.5 段应恰有 {WHITELIST_SECTIONS} 段角色授权 INSERT，\
         实际解析出 {} 段 —— 解析器已与 seed 骨架脱节，用例 ⑤ 的「live == 白名单」\
         不变量随之失去意义，请先修解析器（**不是** seed 写错了）",
        caps.len()
    );

    let mut out: Whitelist = BTreeMap::new();
    for cap in caps {
        let role = cap
            .name("role")
            .expect("匹配必带 role 捕获组")
            .as_str()
            .to_string();
        let codes_body = cap.name("codes").expect("匹配必带 codes 捕获组").as_str();
        let codes = parse_code_list(codes_body, &role);
        assert!(
            !codes.is_empty(),
            "角色 {role} 的 `WHERE m.code IN (...)` 解析出 0 个 code —— 正则多半没吃到 \
             正确的代码列表（片段：{codes_body:?}）"
        );
        if let Some(prev) = out.insert(role.clone(), codes) {
            panic!(
                "角色 {role} 在 seed 里出现两段授权 INSERT（正则跨段误匹配）：{prev:?}；\
                 请修解析器"
            );
        }
    }
    out
}

/// 剥掉 `--` 行注释（2026-10-05 review 第 2 轮 MINOR-2）。
///
/// **为什么必须剥**：`seeds/menu.sql` 是注释极密的文件，4.2 / 4.3 段的 code 列表
/// **中间就夹着整段中文注释**。若某条注释里出现单引号（例如
/// `-- 参照 'scan_badge' 的 HMI 口径`），`quoted_re`（`'[^']*'`）会把它当成一个
/// code 抓走 → 断言 A 报「缺失 scan_badge」的**假红**。fail-loud 不是假绿，但会给
/// 维护者一个与真实缺陷无关的红脸，而踩中它的概率随注释密度上升。
///
/// **为什么不做成「让 `whitelist_re` 的 `codes` 组跳过注释行」**：`codes` 是捕获组，
/// 必然是 `IN (` 与 `)` 之间一段**连续**文本，中间的注释行无论用哪种跳写法（先行
/// 断言 / 重复跳行）都仍落在组内 —— 除非把整个括号体抓成一个 `codes_body` 再二次
/// 加工，那还不如直接在 `parse_code_list` 里剥。故选「先剥后解析」，改动面最小；
/// 且**剥在 `parse_code_list` 内部**（而不是调用点前），未来新增调用点不会漏掉这步。
///
/// 只认「`trim_start()` 后以 `--` 开头」的行：SQL 里 `--` 一律是行注释，
/// `seeds/menu.sql` 也没有含 `--` 的字符串字面量，code 列表内更无行尾注释。
/// 残留局限（fail-loud，非静默）：注释行里若出现 ASCII `)`，会让 `whitelist_re` 的
/// `[^)]*` codes 组提前截断，表现为「缺失若干 code」而不是误报多出 code。
fn strip_sql_line_comments(body: &str) -> String {
    body.lines()
        .filter(|line| !line.trim_start().starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 从 `WHERE m.code IN (...)` 的括号内容里取出 code 集合，逐个按 `^[a-z_]+$` 校验。
///
/// 括号体先经 `strip_sql_line_comments` 剥掉 `--` 行注释再交给 `quoted_re`（理由见
/// 该函数注释）；报错信息里的「片段」是**剥完注释后**正则真正看到的那段文本。
fn parse_code_list(body: &str, role: &str) -> BTreeSet<String> {
    let stripped = strip_sql_line_comments(body);
    let raws: Vec<&str> = quoted_re()
        .find_iter(&stripped)
        .map(|m| m.as_str())
        .collect();
    assert!(
        !raws.is_empty(),
        "角色 {role} 的 code 列表里一个单引号片段都没有（剥注释后的片段：{stripped:?}）"
    );
    let mut out = BTreeSet::new();
    for raw in raws {
        let code = raw.trim_matches('\'');
        assert!(
            code_re().is_match(code),
            "角色 {role} 的 code {code:?} 不匹配 `^[a-z_]+$` —— 引号位置/字面值与解析器 \
             假设不符，请修解析器或核对 seed（剥注释后的片段：{stripped:?}）"
        );
        out.insert(code.to_string());
    }
    out
}

/// 把一段 `('ROLE', 'code')` 元组列表文本解析成二元组集合。
fn parse_pair_tuples(tuples: &str, what: &str) -> RevokedPairs {
    let stripped = strip_sql_line_comments(tuples);
    let mut out: RevokedPairs = BTreeSet::new();
    for cap in revoked_pair_re().captures_iter(&stripped) {
        out.insert((
            cap.name("role").expect("必带 role 组").as_str().to_string(),
            cap.name("code").expect("必带 code 组").as_str().to_string(),
        ));
    }
    assert!(
        !out.is_empty(),
        "{what} 元组列表解析出 0 个 ('ROLE', 'code') —— 片段：{stripped:?}"
    );
    out
}

/// 从 4.7 段解析出 `('ROLE', 'code')` 回收二元组集合。
fn parse_revoked_pairs(sql: &str) -> RevokedPairs {
    let caps: Vec<_> = revoked_re().captures_iter(sql).collect();
    assert_eq!(
        caps.len(),
        1,
        "seeds/menu.sql 应恰有 1 处「`AND rm.deleted_at IS NULL` + `AND (rm.role, m.code) \
         IN (...)`」（4.7 段回收清单），实际解析出 {} 处 —— 解析器已与 seed 骨架脱节\
         （**不是**「4.7 段可以为空」），请先修解析器",
        caps.len()
    );
    parse_pair_tuples(
        caps[0]
            .name("tuples")
            .expect("匹配必带 tuples 捕获组")
            .as_str(),
        "4.7 段",
    )
}

/// 从 4.0 复活段解析出 `('ROLE', 'code')` 复活二元组集合。
///
/// 4.0 段的元组列表必须与 4.1-4.5 白名单**逐条镜像**（见 `assert_revive_list_mirrors_
/// whitelist`），所以这里只负责解析，"镜像" 的判定留给那条断言。
fn parse_revived_pairs(sql: &str) -> RevokedPairs {
    let caps: Vec<_> = revived_re().captures_iter(sql).collect();
    assert_eq!(
        caps.len(),
        1,
        "seeds/menu.sql 应恰有 1 处「`AND rm.deleted_at IS NOT NULL` + `AND (rm.role, \
         m.code) IN (...)`」（4.0 段复活清单），实际解析出 {} 处 —— 解析器已与 seed 骨架\
         脱节（**不是**「4.0 段可以为空」），请先修解析器",
        caps.len()
    );
    parse_pair_tuples(
        caps[0]
            .name("tuples")
            .expect("匹配必带 tuples 捕获组")
            .as_str(),
        "4.0 段",
    )
}

// ===========================================================================
// Helpers
// ===========================================================================

/// 跑一次 `seeds/menu.sql`（与生产启动钩子 `src/infra/seed.rs` 同一份 SQL）。
async fn apply_menu_seed(pool: &PgPool) {
    sqlx::raw_sql(MENU_SEED_SQL)
        .execute(pool)
        .await
        .expect("apply menu seed");
}

/// 每角色 live 授权的 code 集合（`rm.deleted_at IS NULL`，即前端可见性的授权侧输入）。
async fn live_codes_by_role(pool: &PgPool) -> BTreeMap<String, BTreeSet<String>> {
    let rows = sqlx::query(
        "SELECT rm.role, array_agg(m.code ORDER BY m.code) AS codes \
         FROM t_role_menu rm JOIN t_menu m ON m.id = rm.menu_id \
         WHERE rm.deleted_at IS NULL \
         GROUP BY rm.role ORDER BY rm.role",
    )
    .fetch_all(pool)
    .await
    .expect("查询 t_role_menu live 授权矩阵");

    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for row in rows {
        let role: String = row.get("role");
        let codes: Vec<String> = row.get("codes");
        out.insert(role, codes.into_iter().collect());
    }
    out
}

/// `t_menu` 里**已软删**（`deleted_at IS NOT NULL`）的 code 集合。
///
/// 4.1-4.5 段的 INSERT 都带 `AND m.deleted_at IS NULL`，故白名单里的软删 code
/// （当前只有第 3.1 段的 `settings_root`；fresh 测试库里 `work_types_list` 等 3 个旧
/// 菜单根本不存在）不会进授权表。用例 ⑤ 的「live == 白名单 − 软删菜单」要用它做差集。
async fn soft_deleted_menu_codes(pool: &PgPool) -> BTreeSet<String> {
    sqlx::query_scalar::<_, String>("SELECT code FROM t_menu WHERE deleted_at IS NOT NULL")
        .fetch_all(pool)
        .await
        .expect("查询 t_menu 已软删 code")
        .into_iter()
        .collect()
}

/// 取某角色 code 集合；角色整体不在结果里（授权被清空）时返回空集 —— 由调用方的
/// 快照断言负责报出「整个角色都没了」。
fn codes_of(matrix: &BTreeMap<String, BTreeSet<String>>, role: &str) -> BTreeSet<String> {
    matrix.get(role).cloned().unwrap_or_default()
}

/// 该角色对 3 个目标 code 的可见集（与 `TARGET_CODES` 求交集）。
fn headline_of(matrix: &BTreeMap<String, BTreeSet<String>>, role: &str) -> BTreeSet<String> {
    codes_of(matrix, role)
        .into_iter()
        .filter(|c| TARGET_CODES.contains(&c.as_str()))
        .collect()
}

/// 断言 5 份全量快照（集合**完全相等**，不是「包含」）。
fn assert_full_snapshot(matrix: &BTreeMap<String, BTreeSet<String>>) {
    for (role, expected) in EXPECTED_SNAPSHOT {
        assert_code_set(role, &codes_of(matrix, role), expected);
    }
    // 兜底：不应冒出 4.1-4.5 段之外的角色（防止有人往 seed 里加第 6 个角色授权）
    let declared: BTreeSet<&str> = EXPECTED_SNAPSHOT.iter().map(|(r, _)| *r).collect();
    let actual: BTreeSet<&str> = matrix.keys().map(String::as_str).collect();
    assert_eq!(
        actual, declared,
        "t_role_menu 里出现了 seeds/menu.sql 第 4 节未声明的角色"
    );
}

/// 断言 3 个目标 code 的逐角色可见性（headline 口径）。
fn assert_headline(matrix: &BTreeMap<String, BTreeSet<String>>) {
    for (role, expected) in HEADLINE {
        let actual = headline_of(matrix, role);
        let expected_set: BTreeSet<String> = expected.iter().map(|s| s.to_string()).collect();
        assert_eq!(
            actual, expected_set,
            "角色 {role} 对 3 个目标菜单（工序工种 / 制定工序 / 生产队列）的可见集不符 \
             （2026-10-05 收紧口径：工序工种+制定工序=仅 MANAGER，生产队列=MANAGER+CLERK，品检都看不到）"
        );
    }
}

/// 集合相等断言；失败信息直接列出「多了什么 / 少了什么」，避免整块 dump 让人自己找差异。
fn assert_code_set(role: &str, actual: &BTreeSet<String>, expected: &[&str]) {
    let expected: BTreeSet<String> = expected.iter().map(|s| s.to_string()).collect();
    if actual == &expected {
        return;
    }
    let extra: Vec<&str> = actual.difference(&expected).map(String::as_str).collect();
    let missing: Vec<&str> = expected.difference(actual).map(String::as_str).collect();
    panic!(
        "角色 {role} 的 live 授权 code 快照与 seeds/menu.sql 第 4 节白名单不符：\n\
         \x20 实际 {} 项 / 期望 {} 项\n\
         \x20 多出（白名单里没有，却仍在库里 = 该收回没收回）: {extra:?}\n\
         \x20 缺失（白名单里有，却不在库里）: {missing:?}\n\
         \x20 实际全集（排序）: {actual:?}",
        actual.len(),
        expected.len()
    );
}

/// 断言 4.0 复活名单与 4.1-4.5 白名单**逐条镜像**（2026-10-13）。
///
/// 4.0 段的 61 条元组是 4.1-4.5 五段白名单的手工复制。两份必须完全一致：
/// - 少了 → 白名单里的授权被软删后再加回白名单时不会被复活，会插出**第二行**
///   （`uk_t_role_menu_role_menu` 是 partial，软删行不在索引里，ON CONFLICT 命中不到）；
/// - 多了 → 4.7 回收名单里的 code 会被 4.0 复活。4.7 在 4.0 之后跑、同一事务，
///   当前配置下净结果仍是软删，但两段的先后契约一改就静默失效。
///
/// 判定放在这里而不是让 4.0 段直接查库，是为了把「声明式文本自身一致」这条不变量
/// 与 DB 状态解耦：即便 fresh 库恰好没触发问题，文本漂移也会立刻炸。
fn assert_revive_list_mirrors_whitelist(whitelist: &Whitelist) {
    let revived = parse_revived_pairs(MENU_SEED_SQL);

    let from_whitelist: RevokedPairs = whitelist
        .iter()
        .flat_map(|(role, codes)| codes.iter().map(move |c| (role.clone(), c.clone())))
        .collect();

    if revived == from_whitelist {
        return;
    }
    let only_in_revive: Vec<&(String, String)> = revived.difference(&from_whitelist).collect();
    let only_in_whitelist: Vec<&(String, String)> = from_whitelist.difference(&revived).collect();
    panic!(
        "seeds/menu.sql 4.0 复活名单与 4.1-4.5 白名单不镜像：\n\
         \x20 4.0 段 {} 条 / 4.1-4.5 白名单 {} 条\n\
         \x20 只在 4.0 复活名单里（会被误复活）: {only_in_revive:?}\n\
         \x20 只在 4.1-4.5 白名单里（不会被复活 = 加回白名单时会插出第二行）: {only_in_whitelist:?}\n\
         \x20 改白名单时务必同步 4.0 段的元组列表",
        revived.len(),
        from_whitelist.len()
    );
}

/// 伪造一条 **live** `t_role_menu` 行：`menu_id` 按 code 反查（不写死 id，与 seed
/// 同一约定，避免环境间菜单 id 漂移）。模拟「今天的生产库：老 seed 留下的 live 授权行」。
async fn insert_live_grant(pool: &PgPool, id: i64, role: &str, code: &str, version: i32) {
    let affected = sqlx::query(
        "INSERT INTO t_role_menu \
           (id, role, menu_id, version, created_at, created_by, updated_at, updated_by, deleted_at) \
         SELECT $1, $2, m.id, $3, now(), 0, now(), 0, NULL \
         FROM t_menu m WHERE m.code = $4 AND m.deleted_at IS NULL",
    )
    .bind(id)
    .bind(role)
    .bind(version)
    .bind(code)
    .execute(pool)
    .await
    .expect("插入伪造 live t_role_menu 行");
    assert_eq!(
        affected.rows_affected(),
        1,
        "伪造 t_role_menu 行失败：code={code} 在 t_menu 里找不到 live 行（种子未 apply？）"
    );
}

/// 伪造一条**已软删**的 `t_role_menu` 行（模拟「上一版 seed 已回收过」），用来验 4.7 段
/// 的 `AND rm.deleted_at IS NULL` 守卫：缺守卫会导致每次重跑都再 bump 一次 version。
async fn insert_revoked_grant(pool: &PgPool, id: i64, role: &str, code: &str, version: i32) {
    let affected = sqlx::query(
        "INSERT INTO t_role_menu \
           (id, role, menu_id, version, created_at, created_by, updated_at, updated_by, deleted_at) \
         SELECT $1, $2, m.id, $3, now(), 0, now(), 0, now() \
         FROM t_menu m WHERE m.code = $4 AND m.deleted_at IS NULL",
    )
    .bind(id)
    .bind(role)
    .bind(version)
    .bind(code)
    .execute(pool)
    .await
    .expect("插入伪造已软删 t_role_menu 行");
    assert_eq!(
        affected.rows_affected(),
        1,
        "伪造 t_role_menu 行失败：code={code} 在 t_menu 里找不到 live 行（种子未 apply？）"
    );
}

/// 读回某条 `t_role_menu` 行的 `(deleted_at, version)`；行不存在直接 panic。
async fn read_grant(pool: &PgPool, id: i64) -> (Option<NaiveDateTime>, i32) {
    sqlx::query("SELECT deleted_at, version FROM t_role_menu WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .map(|row| {
            (
                row.get::<Option<NaiveDateTime>, _>("deleted_at"),
                row.get::<i32, _>("version"),
            )
        })
        .unwrap_or_else(|e| panic!("读 t_role_menu id={id} 失败：{e}"))
}

/// `t_role_menu` 总行数（live + 软删；回收只改 `deleted_at`、不增删行）。
async fn total_grant_rows(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM t_role_menu")
        .fetch_one(pool)
        .await
        .expect("统计 t_role_menu 总行数")
}

/// 递归 DFS 收集渲染树里的全部 code。
fn collect_codes(nodes: &[Value], out: &mut BTreeSet<String>) {
    for node in nodes {
        if let Some(code) = node.get("code").and_then(Value::as_str) {
            out.insert(code.to_string());
        }
        if let Some(kids) = node.get("children").and_then(Value::as_array) {
            collect_codes(kids, out);
        }
    }
}

/// 在渲染树里按 code 深度优先找节点。
fn find_node<'a>(nodes: &'a [Value], code: &str) -> Option<&'a Value> {
    for node in nodes {
        if node.get("code").and_then(Value::as_str) == Some(code) {
            return Some(node);
        }
        if let Some(kids) = node.get("children").and_then(Value::as_array)
            && let Some(found) = find_node(kids, code)
        {
            return Some(found);
        }
    }
    None
}

/// 某节点的**直接**子节点 code 集合（结构断言用：专门防「父分组丢了导致子节点被
/// `build_menu_tree` 提升为顶级」的孤儿回归，见 `src/modules/iam/service/menu.rs:44-52`）。
fn children_of(tree: &[Value], code: &str) -> BTreeSet<String> {
    let node = find_node(tree, code).unwrap_or_else(|| panic!("渲染树里找不到节点 {code}"));
    node["children"]
        .as_array()
        .unwrap_or_else(|| panic!("节点 {code} 缺 children 数组"))
        .iter()
        .map(|c| {
            c.get("code")
                .and_then(Value::as_str)
                .unwrap_or("<无 code>")
                .to_string()
        })
        .collect()
}

/// 登录 + `GET /iam/me` → 返回 `data.menus` 原始树（前端直接消费的结构）。
async fn rendered_menus(app: &axum::Router, username: &str) -> Vec<Value> {
    let token = login_token(app, username, IamFixture::PASSWORD).await;
    let (status, env) = send(
        app.clone(),
        json_request("GET", "/iam/me", None, Some(&token)),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "登录后 GET /iam/me 应 200，user={username} env={env}"
    );
    env["data"]["menus"]
        .as_array()
        .unwrap_or_else(|| panic!("/iam/me 响应缺 data.menus 数组，user={username} env={env}"))
        .clone()
}

// ===========================================================================
// 用例 ①：t_role_menu 授权口径的全量快照
// ===========================================================================
#[tokio::test]
async fn menu_seed_role_matrix() {
    let pool = test_pool().await;
    // fresh DB 继承自已 seed 的 template；这里仍显式跑一次走真实路径
    //（此时是 no-op，顺带把幂等也过了一遍）。
    apply_menu_seed(&pool).await;

    let matrix = live_codes_by_role(&pool).await;

    // ①-1 每角色 code 集合与「读 seeds/menu.sql 4.1-4.5 白名单」推导出的快照**完全相等**
    assert_full_snapshot(&matrix);

    // ①-2 headline：3 个目标 code 的逐角色可见性（失败信息能一眼看出哪个角色多了/少了哪个）
    assert_headline(&matrix);

    // ①-3 三方都要保留 production_group（CLERK 靠它挂 worker_queue、
    //     INSPECTOR 靠它挂 inspection_pending；丢了子节点会被提升为顶级）
    for role in ["MANAGER", "CLERK", "INSPECTOR"] {
        assert!(
            codes_of(&matrix, role).contains("production_group"),
            "角色 {role} 必须保留 production_group（生产管理分组）授权"
        );
    }
}

// ===========================================================================
// 用例 ②：回收（seeds/menu.sql 4.7 段）真的作用于存量行
// ===========================================================================
// 这是最关键的用例：4 节的授权写入是纯增量（`ON CONFLICT ... DO NOTHING`），
// **从白名单里删 code 不会回收生产库已存在的授权行**。若本用例缺席，将来有人删掉
// 4.7 段 / 把 code 加回白名单，用例 ①③④ 仍会全绿而线上菜单没收紧。
#[tokio::test]
async fn menu_seed_revokes_legacy_grants() {
    let pool = test_pool().await;

    // 1. 先跑一次 seed，把 fresh DB 置于收紧后状态
    apply_menu_seed(&pool).await;

    // 2. 手工伪造 legacy 存量：5 条 **live**（deleted_at 留空）授权行，模拟「今天的
    //    生产库 —— 老版本 seed 留下的、尚未被 4.7 段回收的授权行」。
    //    （测试库是从已 seed 的 template 克隆的，这 5 对在库里本来一行都没有，
    //    所以必须自己造，否则回收无从发生、断言就成了空转。）
    for (idx, (role, code)) in REVOKED_PAIRS.iter().enumerate() {
        let id = LEGACY_GRANT_ID_BASE + idx as i64;
        insert_live_grant(&pool, id, role, code, 0).await;

        // 前提校验：这 5 行确实必须是 live 的
        let (deleted_at, _) = read_grant(&pool, id).await;
        assert_eq!(
            deleted_at, None,
            "伪造的 legacy 授权行 id={id} ({role}/{code}) 必须是 live（deleted_at IS NULL），\
             否则本用例的前提不成立"
        );
    }

    // 3. 记下回收前的总行数（live + 软删）
    let rows_before = total_grant_rows(&pool).await;

    // 4. 再跑一次 seed —— 4.7 段应当把这 5 行软删
    apply_menu_seed(&pool).await;

    // 5. 断言这 5 行变成 `deleted_at IS NOT NULL`（**软删**、不是物理消失，留审计）
    for (idx, (role, code)) in REVOKED_PAIRS.iter().enumerate() {
        let id = LEGACY_GRANT_ID_BASE + idx as i64;
        let (deleted_at, version) = read_grant(&pool, id).await;
        assert!(
            deleted_at.is_some(),
            "4.7 段必须把存量授权行 {role}/{code}（id={id}）**软删**（deleted_at IS NOT NULL）；\
             实际仍为 live —— 说明回收段没生效（或白名单被加回了这个 code）"
        );
        assert_eq!(
            version, 1,
            "回收应恰好 bump 一次 version（0 → 1），id={id} ({role}/{code})"
        );
    }

    // 6. 断言 4.7 段只改 `deleted_at`、**不增删行**
    let rows_after = total_grant_rows(&pool).await;
    assert_eq!(
        rows_after, rows_before,
        "回收只允许改 deleted_at，不允许增删 t_role_menu 行（回收前 {rows_before} / 回收后 {rows_after}）"
    );

    // 7. 断言「可见矩阵」回到收紧后状态（即用例 ① 的 headline 成立）
    let matrix = live_codes_by_role(&pool).await;
    assert_headline(&matrix);
    assert_full_snapshot(&matrix);
}

// ===========================================================================
// 用例 ③：幂等（重跑 3 次）
// ===========================================================================
#[tokio::test]
async fn menu_seed_idempotent() {
    let pool = test_pool().await;

    // ── 第 1 次 ──
    apply_menu_seed(&pool).await;
    let snap1 = live_codes_by_role(&pool).await;
    assert_full_snapshot(&snap1);

    // 伪造「上一版 seed 已回收」的 5 条软删行（version=0），用来验 4.7 段的
    // `AND rm.deleted_at IS NULL` 守卫：**缺守卫会导致每次重跑都 +1 version**。
    for (idx, (role, code)) in REVOKED_PAIRS.iter().enumerate() {
        insert_revoked_grant(&pool, LEGACY_GRANT_ID_BASE + idx as i64, role, code, 0).await;
    }
    let before: Vec<(Option<NaiveDateTime>, i32)> = read_revoked_rows(&pool).await;
    assert!(
        before
            .iter()
            .all(|(deleted_at, version)| deleted_at.is_some() && *version == 0),
        "伪造的 5 条已回收行应处于 (soft_deleted, version=0) 状态，实际 {before:?}"
    );

    // ── 第 2 次 ──
    apply_menu_seed(&pool).await;
    let snap2 = live_codes_by_role(&pool).await;
    let mid: Vec<(Option<NaiveDateTime>, i32)> = read_revoked_rows(&pool).await;

    // ── 第 3 次 ──
    apply_menu_seed(&pool).await;
    let snap3 = live_codes_by_role(&pool).await;
    let after: Vec<(Option<NaiveDateTime>, i32)> = read_revoked_rows(&pool).await;

    // ③-1 无重复 live 行（`uk_t_role_menu_role_menu` 是 partial unique index，
    //     正常 insert 会被 DB 挡；这里防的是「有人改成 upsert 覆盖」这类回归）
    let dupes: Vec<(String, i64, i64)> = sqlx::query(
        "SELECT rm.role, rm.menu_id, count(*) AS n \
         FROM t_role_menu rm WHERE rm.deleted_at IS NULL \
         GROUP BY rm.role, rm.menu_id HAVING count(*) > 1 ORDER BY rm.role, rm.menu_id",
    )
    .fetch_all(&pool)
    .await
    .expect("查重复 live 授权行")
    .into_iter()
    .map(|r| (r.get("role"), r.get("menu_id"), r.get("n")))
    .collect();
    assert!(
        dupes.is_empty(),
        "t_role_menu 不应存在重复 live 行（role, menu_id），实际 {dupes:?}"
    );

    // ③-2 每角色 live code 集合在 3 次之间**逐次完全相同**
    assert_eq!(
        snap2, snap1,
        "第 2 次跑 seed 改变了可见授权矩阵（幂等被破坏）"
    );
    assert_eq!(
        snap3, snap2,
        "第 3 次跑 seed 改变了可见授权矩阵（幂等被破坏）"
    );
    assert_full_snapshot(&snap3);

    // ③-3 那 5 条已回收的软删行，第 2/3 次跑之后 `(deleted_at, version)` 都没再变
    //     （4.7 段的 `AND rm.deleted_at IS NULL` 守卫生效）
    assert_eq!(
        mid, before,
        "第 2 次跑 seed 改动了已回收的软删行（4.7 段缺 `AND rm.deleted_at IS NULL` 守卫？）"
    );
    assert_eq!(
        after, mid,
        "第 3 次跑 seed 改动了已回收的软删行（4.7 段缺 `AND rm.deleted_at IS NULL` 守卫？）"
    );
}

/// 读回 `REVOKED_PAIRS` 对应 5 条伪造行的 `(deleted_at, version)`（按 id 序）。
async fn read_revoked_rows(pool: &PgPool) -> Vec<(Option<NaiveDateTime>, i32)> {
    let mut out = Vec::with_capacity(REVOKED_PAIRS.len());
    for idx in 0..REVOKED_PAIRS.len() {
        out.push(read_grant(pool, LEGACY_GRANT_ID_BASE + idx as i64).await);
    }
    out
}

/// 库里的 live 授权行 id（按 id 升序）。
async fn live_grant_ids(pool: &PgPool) -> Vec<i64> {
    sqlx::query_scalar("SELECT id FROM t_role_menu WHERE deleted_at IS NULL ORDER BY id")
        .fetch_all(pool)
        .await
        .expect("查 live 授权行 id")
}

// ===========================================================================
// 用例 ⑥：4.0 复活段 + 4.1-4.5 段的派生 id（2026-10-13 新增）
// ===========================================================================
// 这条用例是 `t_role_menu_pkey` 事故的回归护栏，同时覆盖修复的**两半**：
//
// ① **派生 id**（治「撞 PK」本体）——4.1-4.5 段改用 `9100000000000000 + 角色序号×1e12 +
//    md5(role,code) 低 48bit` 发号。断言 fresh 库里每一条 live 授权行的 id 都落在这个
//    派生段内（`INSERT 0 2` 时新插入的 mail_* 两行正是这条的产物），且两两不等。
//    只要有人把 id 表达式改回任何「按行位置发号」的写法（如 `row_number() OVER ()`），
//    这里就会红。
//
// ② **复活段**（治「插出第二行」）——`uk_t_role_menu_role_menu` 是 **partial** 唯一索引
//    （`WHERE deleted_at IS NULL`），一条被软删的授权行**不在**索引里 ⇒ 4.1-4.5 段的
//    `ON CONFLICT (role, menu_id) WHERE deleted_at IS NULL` 命中不到它 ⇒ 缺 4.0 段时
//    重跑 seed 会**再插一行**，同一 (role, menu_id) 出现两条（一条 live 一条软删）。
//    用例把一条白名单内的授权软删掉，重跑，断言它被**原地复活**（同一个 `id`、
//    `version + 1`）且总行数不变。
//
// 选 (CLERK, parts_list) 作为软删对象的原因：它在 4.2 白名单里、不在 4.7 回收名单里，
// 且不是 `production_group` 的子菜单（不给 `children_of` 断言添变量）。
#[tokio::test]
async fn menu_seed_revives_soft_deleted_grants() {
    let pool = test_pool().await;

    // ── 1. 先跑一次 seed，把 fresh DB 置于「本轮 seed 建出来的状态」──
    apply_menu_seed(&pool).await;

    // ── 2. 前提：目标授权行存在且是 live 的 ──
    let target_id: i64 = sqlx::query_scalar(
        "SELECT rm.id FROM t_role_menu rm JOIN t_menu m ON m.id = rm.menu_id \
         WHERE rm.role = 'CLERK' AND m.code = 'parts_list' AND rm.deleted_at IS NULL",
    )
    .fetch_optional(&pool)
    .await
    .expect("查 CLERK/parts_list 授权行")
    .expect("CLERK/parts_list 必须有一条 live 授权（否则本用例前提不成立）");

    let rows_before = total_grant_rows(&pool).await;

    // ── 3. 人为把它软删（模拟「被回收过」或「管理员手工收回」）──
    sqlx::query("UPDATE t_role_menu SET deleted_at = now(), version = version + 1 WHERE id = $1")
        .bind(target_id)
        .execute(&pool)
        .await
        .expect("软删 CLERK/parts_list 授权行");
    let (deleted_at, version_before) = read_grant(&pool, target_id).await;
    assert!(
        deleted_at.is_some(),
        "前提校验：id={target_id} (CLERK/parts_list) 应已被软删"
    );

    // ── 4. 重跑 seed —— 4.0 段应当原地复活它 ──
    apply_menu_seed(&pool).await;

    // ── 5. 断言复活：live、`version + 1`、**id 不变** ──
    let (deleted_at, version_after) = read_grant(&pool, target_id).await;
    assert!(
        deleted_at.is_none(),
        "4.0 复活段必须把白名单内已软删的授权行复活（deleted_at IS NULL）；\
         id={target_id} (CLERK/parts_list) 仍为软删 —— 说明复活段没生效，\
         或该 code 没被列进 4.0 的元组列表（断言 D 会同时报出镜像漂移）"
    );
    assert_eq!(
        version_after,
        version_before + 1,
        "复活应恰好 bump 一次 version（{version_before} → {}），id={target_id} \
         (CLERK/parts_list)",
        version_before + 1
    );

    // ── 6. 断言没有插出第二行（缺 4.0 段时这一步就会红）──
    let rows_after = total_grant_rows(&pool).await;
    assert_eq!(
        rows_after, rows_before,
        "复活只允许改 deleted_at，不允许增删 t_role_menu 行（复活前 {rows_before} / \
         复活后 {rows_after}）—— 多出一行说明 4.0 复活段缺失，4.1-4.5 的 ON CONFLICT \
         命中不到软删行、插了重复授权"
    );
    let dupes: Vec<(String, String, i64)> = sqlx::query(
        "SELECT rm.role, m.code, count(*) AS n FROM t_role_menu rm \
         JOIN t_menu m ON m.id = rm.menu_id \
         GROUP BY rm.role, m.code HAVING count(*) > 1 ORDER BY 1, 2",
    )
    .fetch_all(&pool)
    .await
    .expect("查重复 (role, code) 授权行")
    .into_iter()
    .map(|r| (r.get("role"), r.get("code"), r.get("n")))
    .collect();
    assert!(
        dupes.is_empty(),
        "t_role_menu 不应存在同一 (role, menu_id) 的多行（live + 软删合计），实际 {dupes:?}"
    );

    // ── 7. 断言派生 id（治「撞 PK」本体）──
    //    7a. seed 文本层面：5 段 id 表达式必须逐段符合派生式，且**不含任何按行位置发号的
    //        写法**。这是 2026-10-13 事故的直接回归护栏 —— `row_number() OVER ()` 一旦被
    //        改回来，正则立刻失配。
    //    7b. 库层面：派生段内的行，其 id 必须**逐条等于**按 (role, code) 独立算出的期望值
    //        （不依赖 seed 文本，直接在 DB 侧重算），且全库 live 行 id 两两不等。
    let exprs: Vec<(&str, char)> = grant_id_expr_re()
        .captures_iter(MENU_SEED_SQL)
        .map(|c| {
            (
                c.name("role").expect("必带 role 组").as_str(),
                c.name("ord")
                    .expect("必带 ord 组")
                    .as_str()
                    .chars()
                    .next()
                    .expect("ord 非空"),
            )
        })
        .collect();
    assert_eq!(
        exprs.len(),
        WHITELIST_SECTIONS,
        "seeds/menu.sql 应恰有 5 段符合派生式的 t_role_menu.id 表达式，实际解析出 {} 段 —— \
         4.1-4.5 段的 id 必须写成 \
         `9100000000000000 + <角色序号> * 1000000000000 + md5('t_role_menu:<ROLE>:' || m.code) \
         低 48bit`，**绝不能**用 `基址 + row_number() OVER ()`（按行位置发号，白名单增删 \
         code 会让整段位移、撞 t_role_menu_pkey —— 2026-10-13 事故）",
        exprs.len()
    );
    let expected_ord: BTreeMap<&str, char> = [
        ("MANAGER", '0'),
        ("CLERK", '1'),
        ("INSPECTOR", '2'),
        ("CNC_PROGRAMMER", '3'),
        ("SHELF_ACCOUNT", '4'),
    ]
    .into_iter()
    .collect();
    for (role, ord) in &exprs {
        let want = expected_ord
            .get(role)
            .unwrap_or_else(|| panic!("4.1-4.5 段出现了未知角色 {role} 的 id 表达式"));
        assert_eq!(
            ord, want,
            "角色 {role} 的 id 表达式用了角色序号 {ord}，应为 {want} \
             （序号错位会让两个角色的派生 id 段交叠）"
        );
    }

    let derived_rows: Vec<(String, String, i64)> = sqlx::query(
        "SELECT rm.role, m.code, rm.id FROM t_role_menu rm JOIN t_menu m ON m.id = rm.menu_id \
         WHERE rm.deleted_at IS NULL \
           AND rm.id BETWEEN 9100000000000000 AND 9104999999999999",
    )
    .fetch_all(&pool)
    .await
    .expect("查派生段内的 live 授权行")
    .into_iter()
    .map(|r| (r.get("role"), r.get("code"), r.get("id")))
    .collect();
    assert!(
        !derived_rows.is_empty(),
        "派生 id 段（9100000000000000..9104999999999999）里一条 live 授权行都没有 —— \
         4.1-4.5 段是不是把 id 表达式改回去了？"
    );

    // 逐条独立重算期望 id（SQL 里直接算 md5，不复用 seed 文本）
    let mismatched: Vec<(String, String, i64)> = sqlx::query(
        "SELECT rm.role, m.code, rm.id FROM t_role_menu rm JOIN t_menu m ON m.id = rm.menu_id \
         WHERE rm.deleted_at IS NULL \
           AND rm.id BETWEEN 9100000000000000 AND 9104999999999999 \
           AND rm.id <> 9100000000000000 \
                 + (CASE rm.role WHEN 'MANAGER' THEN 0 WHEN 'CLERK' THEN 1 \
                                 WHEN 'INSPECTOR' THEN 2 WHEN 'CNC_PROGRAMMER' THEN 3 \
                                 WHEN 'SHELF_ACCOUNT' THEN 4 END) * 1000000000000 \
                 + (('x' || substr(md5('t_role_menu:' || rm.role || ':' || m.code), 1, 15)) \
                    ::bit(60)::bigint % 1000000000000)",
    )
    .fetch_all(&pool)
    .await
    .expect("重算派生 id")
    .into_iter()
    .map(|r| (r.get("role"), r.get("code"), r.get("id")))
    .collect();
    assert!(
        mismatched.is_empty(),
        "派生段内的授权行 id 与「按 (role, code) 独立重算」的期望值不符：{mismatched:?} —— \
         发号必须只由 (role, code) 决定"
    );

    let mut sorted_ids = live_grant_ids(&pool).await;
    let total = sorted_ids.len();
    sorted_ids.sort_unstable();
    sorted_ids.dedup();
    assert_eq!(
        sorted_ids.len(),
        total,
        "live 授权行里出现重复 id —— 发号必须由 (role, code) 唯一决定，不能按行位置发号 \
         （2026-10-13 t_role_menu_pkey 事故）"
    );

    // ── 8. 再跑一次：复活是一次性动作，已 live 的行不得被反复 bump version ──
    let (still_live, v_after_rerun) = read_grant(&pool, target_id).await;
    apply_menu_seed(&pool).await;
    let (_, v_final) = read_grant(&pool, target_id).await;
    assert!(still_live.is_none(), "sanity：id={target_id} 应保持 live");
    assert_eq!(
        v_final, v_after_rerun,
        "4.0 复活段缺 `AND rm.deleted_at IS NOT NULL` 幂等守卫：已 live 的行每次重跑 \
         都会被 bump version（{v_after_rerun} → {v_final}）"
    );
}

// ===========================================================================
// 用例 ④：端到端 —— 登录后 `/iam/me` 返回的菜单树
// ===========================================================================
// 这是前端真正消费的数据（`frontend/src/layouts/MainLayout.vue:109` 渲染
// `auth.menus`、`frontend/src/router/index.ts:486` 用 `meta.menuCode` 在这棵树里做成员判断）。
#[tokio::test]
async fn menu_seed_rendered_tree_per_role() {
    let pool = test_pool().await;
    let _fx = load_iam_fixture(&pool).await;
    // fixture 里没有 INSPECTOR 用户；给无角色的 `fx_iam_target` 插一条全局 INSPECTOR
    // 角色（scope_type / scope_id 留空 = 非货架账号）。
    add_inspector_role(&pool).await;

    apply_menu_seed(&pool).await;
    let app = test_app(test_state(pool.clone()).await);

    let mgr_tree = rendered_menus(&app, IamFixture::MANAGER_USERNAME).await;
    let clerk_tree = rendered_menus(&app, IamFixture::CLERK_USERNAME).await;
    let insp_tree = rendered_menus(&app, IamFixture::TARGET_USERNAME).await;

    let mut mgr = BTreeSet::new();
    collect_codes(&mgr_tree, &mut mgr);
    let mut clerk = BTreeSet::new();
    collect_codes(&clerk_tree, &mut clerk);
    let mut insp = BTreeSet::new();
    collect_codes(&insp_tree, &mut insp);

    // ── sanity：三方都必须能看到 home（证明树非空、DFS 收集方法本身有效）──
    for (role, codes) in [("MANAGER", &mgr), ("CLERK", &clerk), ("INSPECTOR", &insp)] {
        assert!(
            codes.contains("home"),
            "角色 {role} 的渲染树里应能看到 home（sanity：证明树非空 + DFS 有效）"
        );
    }

    // ── MANAGER：三个目标 code 全在 ──
    for code in TARGET_CODES {
        assert!(
            mgr.contains(code),
            "MANAGER 渲染树里应包含 {code}（工序工种 / 制定工序 / 生产队列 都需要 MANAGER）"
        );
    }
    assert!(
        mgr.contains("production_group"),
        "MANAGER 渲染树里应包含 production_group"
    );
    assert_eq!(
        children_of(&mgr_tree, "production_group"),
        BTreeSet::from([
            "inspection_pending".to_string(),
            "part_process_chain".to_string(),
            "pending_programming".to_string(),
            "process_work_type".to_string(),
            "worker_queue".to_string(),
            "workers_list".to_string(),
        ]),
        "MANAGER 的 production_group 子节点集合不符（6 个：工序工种/制定工序/待编程/生产队列/工人一览/待品检）"
    );

    // ── CLERK：只有 worker_queue 在，另两个收回了 ──
    assert!(
        clerk.contains("worker_queue"),
        "CLERK 渲染树里应包含 worker_queue（生产队列需要 CLERK）"
    );
    for code in ["process_work_type", "part_process_chain"] {
        assert!(
            !clerk.contains(code),
            "CLERK 渲染树里不应包含 {code}（2026-10-05 收回，改 MANAGER 专有）"
        );
    }
    // 结构断言：worker_queue 必须**挂在** production_group 下，而不是被提升为顶级
    assert_eq!(
        children_of(&clerk_tree, "production_group"),
        BTreeSet::from(["worker_queue".to_string()]),
        "CLERK 的 production_group 下应只剩 worker_queue 一个子节点"
    );

    // ── INSPECTOR：三个目标 code 都不在，但 production_group / inspection_pending 都在 ──
    for code in TARGET_CODES {
        assert!(
            !insp.contains(code),
            "INSPECTOR 渲染树里不应包含 {code}（品检看不到生产管理的这 3 个菜单）"
        );
    }
    assert!(
        insp.contains("production_group"),
        "INSPECTOR 渲染树里应包含 production_group（其 inspection_pending 子菜单要挂上去）"
    );
    assert!(
        insp.contains("inspection_pending"),
        "INSPECTOR 渲染树里应包含 inspection_pending（待品检）"
    );
    // 结构断言：inspection_pending 必须挂在 production_group 下 —— 这条专门防「父分组
    // 丢了导致子节点被 build_menu_tree 提升为顶级」的孤儿回归
    //（见 `src/modules/iam/service/menu.rs:44-52` 的孤儿兜底分支）。
    assert_eq!(
        children_of(&insp_tree, "production_group"),
        BTreeSet::from(["inspection_pending".to_string()]),
        "INSPECTOR 的 production_group 下应只剩 inspection_pending 一个子节点 \
         （若为空说明 inspection_pending 被提升成了顶级 → 父分组授权丢了）"
    );

    // ── 渲染口径 ≠ 授权口径 ──
    // `assemblies_list` 在三方的 t_role_menu 里都是 live 授权，但 `t_menu.is_active=false`
    // （seed 3.4 段），所以渲染树里三方都**不该**出现它
    //（过滤见 `src/modules/iam/repo/sql/menu.rs:8-31` 的 `m.is_active = TRUE`）。
    let matrix = live_codes_by_role(&pool).await;
    for role in ["MANAGER", "CLERK", "INSPECTOR"] {
        assert!(
            codes_of(&matrix, role).contains("assemblies_list"),
            "前提校验失败：{role} 的 t_role_menu 里本应有 assemblies_list 的 live 授权"
        );
    }
    for (role, codes) in [("MANAGER", &mgr), ("CLERK", &clerk), ("INSPECTOR", &insp)] {
        assert!(
            !codes.contains("assemblies_list"),
            "角色 {role} 的渲染树里不应出现 assemblies_list（t_menu.is_active=false，被渲染层过滤）"
        );
    }
}

/// 给无角色的 `fx_iam_target` 插一条全局 INSPECTOR 角色。
async fn add_inspector_role(pool: &PgPool) {
    let affected = sqlx::query(
        "INSERT INTO t_user_role \
           (id, user_id, role, scope_type, scope_id, version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, 'INSPECTOR', NULL, NULL, 0, now(), 0, now(), 0)",
    )
    .bind(INSPECTOR_USER_ROLE_ID)
    .bind(IamFixture::TARGET_USER_ID)
    .execute(pool)
    .await
    .expect("给 fx_iam_target 插 INSPECTOR 角色");
    assert_eq!(
        affected.rows_affected(),
        1,
        "给 fx_iam_target 插 INSPECTOR 角色应命中 1 行"
    );
}

// ===========================================================================
// 用例 ⑤：live 授权矩阵 == seed 白名单（4.1-4.5）且与 4.7 回收段不相交
// ===========================================================================
// 2026-10-05 review 第 1 轮 MINOR-1 新增。关的是前 4 个用例的盲区：
// **「只从白名单删 code、忘了写进 4.7 段」在 fresh DB 里不改变任何 live 矩阵**
// （第 4 节 add-only：白名单删 code 不会软删存量授权行；测试库又继承 template 的
// live 行）→ 硬编码快照全绿，而生产库菜单并没有收紧。
// 本用例把不变量从「seed 文本」现场解析出来，与 DB 对账，三条断言：
//   A：每角色 `live == 白名单 − t_menu 已软删的 code`（抓「白名单删了但没进 4.7」）
//   B：`4.7 元组 ∩ 白名单 == ∅`（抓「同一个 code 既被授权又被回收」的自相矛盾配置）
//   C：`4.7 元组 == REVOKED_PAIRS`（两处独立来源必须一致，防只改一处）
#[tokio::test]
async fn menu_seed_live_matrix_matches_whitelist() {
    let pool = test_pool().await;
    apply_menu_seed(&pool).await;

    // ── 从 seed 文本解析期望值（解析器自带防呆：段数 / 空集合 / code 字面值不合规
    //    一律 panic，见 parse_whitelist / parse_revoked_pairs 的注释）──
    let whitelist = parse_whitelist(MENU_SEED_SQL);
    let revoked = parse_revoked_pairs(MENU_SEED_SQL);

    // 前提校验：解析出的角色集合必须与本文件的 5 份快照完全一致（防解析器漏掉某段）
    let declared: BTreeSet<String> = EXPECTED_SNAPSHOT
        .iter()
        .map(|(r, _)| r.to_string())
        .collect();
    let parsed_roles: BTreeSet<String> = whitelist.keys().cloned().collect();
    assert_eq!(
        parsed_roles, declared,
        "seeds/menu.sql 解析出的角色集合与本文件的 EXPECTED_SNAPSHOT 角色集合不符：\
         若 seed 真的增删了角色授权段，请同步改 EXPECTED_SNAPSHOT；若没改，多半是解析器失配"
    );

    let live = live_codes_by_role(&pool).await;
    let soft_deleted = soft_deleted_menu_codes(&pool).await;

    // ── 断言 A：每角色 live 集合 == 白名单 − 已软删菜单 ──
    for (role, wl) in &whitelist {
        let expected: BTreeSet<String> = wl.difference(&soft_deleted).cloned().collect();
        let actual = codes_of(&live, role);
        if actual == expected {
            continue;
        }
        let extra: Vec<&str> = actual.difference(&expected).map(String::as_str).collect();
        let missing: Vec<&str> = expected.difference(&actual).map(String::as_str).collect();
        panic!(
            "角色 {role} 的 live 授权矩阵与 seeds/menu.sql 4.1-4.5 白名单对不上：\n\
             \x20 实际 {} 项 / 白名单（扣掉已软删菜单）{} 项\n\
             \x20 多出（白名单里没有，库里却仍是 live）: {extra:?}\n\
            \x20   ↑ 若这些 code 是「从白名单里删掉」的，说明**漏写了 4.7 段回收**：\
            \x20     第 4 节是 add-only，白名单删 code 不会软删存量 t_role_menu 行，\
            \x20     生产库菜单不会消失。断言 A 的独立价值在**存量库 / 老 template /\
            \x20     生产升级**场景（库里的 live 行不是当轮 seed 建的）；标准 CI 路径下\
            \x20     hsh_erp_template 由当轮 seed 重建，抓手是上面那几份硬编码快照\n\
             \x20 缺失（白名单里有，库里却没有）: {missing:?}\n\
             \x20   ↑ 通常是 4.1-4.5 段的 INSERT 没跑成功，或该菜单已被软删\n\
             \x20 实际全集（排序）: {actual:?}\n\
             \x20 期望全集（排序）: {expected:?}",
            actual.len(),
            expected.len()
        );
    }
    // 兜底：库里不应出现 seed 第 4 节之外的角色授权段（防止有人加了第 6 段而解析器
    // 其实吃到了、白名单集合里却没体现角色名）
    let live_roles: BTreeSet<String> = live.keys().cloned().collect();
    assert_eq!(
        live_roles, declared,
        "t_role_menu 里出现了 seeds/menu.sql 第 4 节未声明的角色"
    );

    // ── 断言 B：4.7 回收元组与白名单**不相交**（逐 role 判 (role, code) 二元组）──
    // 同一个 (role, code) 既在白名单里被 INSERT、又被 4.7 段软删 = 自相矛盾配置：
    // seed 每跑一次就「授权 + 回收」来回抖，`version` 无限 bump，菜单随机闪没。
    for (role, code) in &revoked {
        let granted = whitelist
            .get(role)
            .is_some_and(|codes| codes.contains(code));
        assert!(
            !granted,
            "自相矛盾配置：({role}, {code}) 同时出现在 seeds/menu.sql 的 4.1-4.5 白名单 \
             与 4.7 回收清单里 —— 要么从白名单删掉、要么从 4.7 删掉，不能两处都有"
        );
    }

    // ── 断言 C：4.7 段解析出的元组 == REVOKED_PAIRS 常量（两处独立来源必须一致）──
    let expected_revoked: RevokedPairs = REVOKED_PAIRS
        .iter()
        .map(|(r, c)| (r.to_string(), c.to_string()))
        .collect();
    assert_eq!(
        revoked, expected_revoked,
        "seeds/menu.sql 4.7 段的回收元组与本文件 REVOKED_PAIRS 常量不一致（两处独立来源 \
         必须同步改）：4.7 段新增/删除了回收项时，务必同步 REVOKED_PAIRS"
    );

    // ── 断言 D：4.0 复活名单 == 4.1-4.5 白名单（逐条镜像，见 seeds/menu.sql 4.0 段头）──
    assert_revive_list_mirrors_whitelist(&whitelist);
}
