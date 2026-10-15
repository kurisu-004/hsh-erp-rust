//! 交期状态白名单（**跨域共享常量**，2026-10-23 新增）
//!
//! ## 为什么是 `shared` 而不是某一域的常量
//!
//! 「哪些状态算**未交付**」是跨域的既定契约：`dashboard` 的交期面板 / 大屏柱状图、
//! `mail_digest` 的交期汇总邮件、`wecom_daily` 的每日总结，三处**各自写 SQL**
//! （行单位、窗口、投影列都不同），但状态判据是同一份 6 态白名单。白名单留成多份
//! 副本时，「加一个状态」要在若干个文件里各改一次，**漏一处不会编译失败**，只会在
//! 三处数字之间产生互相矛盾的口径（某处把新状态当未交、另两处当已交），而且没有任何
//! 单测会红 —— 集成测试只会看到「逾期数不对」，查不出是哪一份白名单漂了。
//!
//! `shared::` **不属于任何业务域**：各域的 `assert_no_foreign_domain` 护栏判的是
//! 「域根段 + 他域路径」的前缀失配，引 `shared::delivery_status` 不触发任何跨域判定，
//! **护栏强度不降**（与 `shared::analytics` / `shared::customer` 同款取舍）。
//!
//! ## 两份白名单的分工
//!
//! | 常量 | 形态 | 语义 |
//! |---|---|---|
//! | [`DELIVERY_STATUSES`] | 6 态 | 「未交付」全集：`t_part.status` 的非终态部分 |
//! | [`RED_TIER_STATUSES`] | 4 态 | **品检前**：`DELIVERY_STATUSES` 去掉 `INSPECTION` / `READY_TO_SHIP` |
//!
//! 两份放在**同一个文件**里：`RED_TIER_STATUSES` 是 `DELIVERY_STATUSES` 的真子集，
//! 分开放就会出现「加状态只改了其中一份」这种自相矛盾的形态，而这种矛盾在代码上
//! 完全看不出来（两个 `const` 各自都编译得过）。
//!
//! ## ⚠️ 跨语言人工同步点：全仓**只剩 2 处**
//!
//! 后端侧（本文件）是第 1 处；第 2 处是**前端**
//! `views/dashboard/components/UpcomingDeliveryChart.vue` 的 `LAYERS[].statuses`
//! —— 其中 `key: 'top'`（顶层「品检前」）那一层是 4 态，与
//! [`RED_TIER_STATUSES`] **逐字对应**；`middle`（待品检/待送货）是另 2 态；
//! `bottom`（已送货）是 `DELIVERED`，**不在**未交付白名单里。
//!
//! ⚠️ 这条同步**没有编译期保障**（前端是 TS、后端是 Rust），所以：
//! - 后端这侧只保留**一份**副本，改动面收敛到本文件；
//! - 大屏端点 3（抽屉下钻）对 `statuses` **不做白名单校验**（不属于白名单的字面量
//!   一律查 0 行而不是 400），所以前端即使先于后端上线新图层状态也不会报错 —— 反过
//!   来，**后端加了状态而前端没加，那个图层就恒为 0 且不报错**，只能靠人核对。

use std::sync::LazyLock;

/// 「未交付」状态白名单（6 态）：`t_part.status` 的非终态部分。
///
/// 取集合用于各域统计 SQL 的 `status = ANY($n::varchar[])`（6 态，前端柱状图
/// `LAYERS[].statuses` 的 top(4) + middle(2) 两层）。
///
/// ⚠️ 装配件侧（`t_assembly`）复用同一份白名单，天然只命中其中 4 态
/// （`AssemblyStatus` 无 `PROGRAMMING` / `OUTSOURCE`）—— 这是有意的：白名单按 part
/// 状态域取全集，装配件侧对不上的两个状态自然不参与，**不需要第二份常量**。
pub const DELIVERY_STATUSES: [&str; 6] = [
    "PENDING",
    "PROGRAMMING",
    "IN_PROCESS",
    "OUTSOURCE",
    "INSPECTION",
    "READY_TO_SHIP",
];

/// 交期分层的**品检前** 4 态（`DELIVERY_STATUSES` 去掉 `INSPECTION` / `READY_TO_SHIP`）。
///
/// 与前端 `UpcomingDeliveryChart.vue` 的 `LAYERS[].statuses` 里 `key: 'top'` 那一层
/// **逐字对应**（那里注释就写着「顶层（top）4 状态：PENDING / PROGRAMMING /
/// IN_PROCESS / OUTSOURCE（品检前）」）—— 分层展示时「还没进品检环节」与「品检后待
/// 发货」要分开看，两者的催办动作完全不同，合成一个数字就看不出该催哪一批。
pub const RED_TIER_STATUSES: [&str; 4] = ["PENDING", "PROGRAMMING", "IN_PROCESS", "OUTSOURCE"];

/// [`DELIVERY_STATUSES`] 的 `Vec<String>` 形态（`LazyLock` 只算一次）。
///
/// ⚠️ 为什么需要这一层：**走 `sqlx::query_as!` 宏的那些域**（`mail_digest` /
/// `wecom_daily`）里，`$n::varchar[]` 推出的参数类型是 `&[String]`，
/// `&[&str; 6]` **绑不上去**（宏要求参数是具体可绑的 Rust 类型）；而走**运行时**
/// `sqlx::query(..).bind(..)` 的域（`dashboard`）可以直接绑 `&[&str; 6]`。
/// 于是同一份常量有两种绑定形态，各域按自己的宏形态取用。
///
/// 每次调用都新建 `Vec` 会给「每天发一条推送」平白加 6 次分配；`LazyLock` 把它压到
/// 进程内一次。
pub fn delivery_statuses_vec() -> &'static [String] {
    static VEC: LazyLock<Vec<String>> =
        LazyLock::new(|| DELIVERY_STATUSES.iter().map(|s| s.to_string()).collect());
    &VEC
}

/// [`RED_TIER_STATUSES`] 的 `Vec<String>` 形态（理由同 [`delivery_statuses_vec`]）。
pub fn red_tier_statuses_vec() -> &'static [String] {
    static VEC: LazyLock<Vec<String>> =
        LazyLock::new(|| RED_TIER_STATUSES.iter().map(|s| s.to_string()).collect());
    &VEC
}
