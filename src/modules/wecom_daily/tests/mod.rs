//! wecom_daily 域 lib 单测（**不连库**）
//!
//! 覆盖面：
//! - 枚举解析 / 取值域（`dto`）—— `Frequency` / `WorkerMetric` / `TriggerSource` /
//!   `SendStatus`；
//! - 配置校验矩阵（`service` 的 `#[cfg(test)]` 模块内）—— `21804` 的每条规则；
//! - **渲染与分片**（`template` 内部自带 `mod tests`，见该文件）—— 本域最关键的
//!   14 条守卫全在那里；
//! - cron 生成（`schedule` 内部自带 `mod tests`，见该文件）；
//! - 路由表与 `router()` 源码逐条比对（`handler` 内部自带 `mod tests`）。
//!
//! ## ⚠️ 遵守「测试数据两条硬约定」第 2 条
//! 本文件**不插入任何带 `now_naive()` 时间戳的行**，因此不存在「查询窗口硬编码绝对
//! 日期」的写法。连库的用例全在 `tests/wecom_daily/`，窗口由 `now_naive()` 推导。
//!
//! ## 为什么不装 `assert_no_foreign_domain`
//! 见 [`super`] 模块 doc 的「与 `wecom` 的切分线」一节：内容域向通道域取推送通道是
//! **这个切分的必然结果**（与 `mail_digest → scheduled_task` 同款），不是「图省事
//! import 了别人的 service」。本域的统计 SQL 完全写在本域内、**零他域符号引用**。

use super::dto::{Frequency, TriggerSource, WorkerMetric};

/// 四个频率字面量与「哪些频率要哪些配套字段」三组判据。
#[test]
fn frequency_parses_all_four_variants() {
    assert_eq!(Frequency::ALL.len(), 4);
    assert_eq!(Frequency::Daily.as_str(), "DAILY");
    assert_eq!(Frequency::Weekly.as_str(), "WEEKLY");
    assert_eq!(Frequency::Monthly.as_str(), "MONTHLY");
    assert_eq!(Frequency::ManualOnly.as_str(), "MANUAL_ONLY");

    for f in Frequency::ALL {
        let parsed: Frequency = f.as_str().parse().expect("字面量必须可解析");
        assert_eq!(parsed, *f);
        // label 非空（`GET /meta` 直接下发，前端不另存映射）。
        assert!(!f.label().is_empty());
    }
    // 未知值 → 21804，且错误信息带合法值清单。
    let err = "HOURLY".parse::<Frequency>().unwrap_err();
    assert_eq!(err.code(), 21804);
    let msg = err.to_string();
    assert!(msg.contains("MANUAL_ONLY"), "{msg}");
}

/// 频率的三个 `needs_*` 判据（前端表单的显隐与校验全部读它们 ⇒ 后端是唯一真源）。
#[test]
fn frequency_needs_flags_match_product_decisions() {
    assert!(Frequency::Daily.needs_time());
    assert!(!Frequency::Daily.needs_weekday());
    assert!(!Frequency::Daily.needs_day_of_month());

    assert!(Frequency::Weekly.needs_time());
    assert!(Frequency::Weekly.needs_weekday());
    assert!(!Frequency::Weekly.needs_day_of_month());

    assert!(Frequency::Monthly.needs_time());
    assert!(!Frequency::Monthly.needs_weekday());
    assert!(Frequency::Monthly.needs_day_of_month());

    // MANUAL_ONLY 全不要（否则会向用户要一个按定义不需要的字段）。
    assert!(!Frequency::ManualOnly.needs_time());
    assert!(!Frequency::ManualOnly.needs_weekday());
    assert!(!Frequency::ManualOnly.needs_day_of_month());
}

/// 工人榜指标只有两个值，且 label 逐字对应日报里的口径名。
#[test]
fn worker_metric_has_exactly_two_variants() {
    assert_eq!(WorkerMetric::ALL.len(), 2);
    assert_eq!(WorkerMetric::Quantity.as_str(), "QUANTITY");
    assert_eq!(WorkerMetric::Quantity.label(), "送件数");
    assert_eq!(WorkerMetric::Quantity.heading_suffix(), "送件数");
    assert_eq!(WorkerMetric::Batch.as_str(), "BATCH");
    assert_eq!(WorkerMetric::Batch.label(), "报工批次数");
    assert_eq!(WorkerMetric::Batch.heading_suffix(), "报工批次数");

    for m in WorkerMetric::ALL {
        assert_eq!(m.as_str().parse::<WorkerMetric>().unwrap(), *m);
    }
    // ⚠️ 迁移的列注释写的是 `HOURS`，**本域不实现它**（见 `dto.rs` 文件头）。
    assert_eq!(
        "HOURS".parse::<WorkerMetric>().unwrap_err().code(),
        21804,
        "HOURS 不是合法取值"
    );
}

/// 触发来源 / 发送状态两个枚举的字面量与解析。
#[test]
fn trigger_source_and_send_status_parse() {
    assert_eq!(TriggerSource::Scheduled.as_str(), "SCHEDULED");
    assert_eq!(TriggerSource::Manual.as_str(), "MANUAL");
    assert_eq!(
        "SCHEDULED".parse::<TriggerSource>().unwrap(),
        TriggerSource::Scheduled
    );
    assert_eq!("AUTO".parse::<TriggerSource>().unwrap_err().code(), 21802);
}

/// 缺省配置：`webhook_id` 恒 `None`（**没有**回落到 `is_default` 的分支）、
/// `enabled` 恒 `false`、四节全开、三个数值取各自缺省。
#[test]
fn default_config_is_the_meta_defaults_segment() {
    let d = super::dto::NormalizedConfig::default();
    assert!(!d.enabled, "少发一条永远好过多发一条");
    assert_eq!(d.webhook_id, None, "默认不替用户选通道");
    assert_eq!(d.frequency, Frequency::Daily);
    assert_eq!((d.send_hour, d.send_minute), (Some(8), Some(0)));
    assert_eq!(d.weekday, None);
    assert_eq!(d.day_of_month, None);
    assert_eq!(d.upcoming_days, 3);
    assert_eq!(d.worker_metric, WorkerMetric::Quantity);
    assert_eq!(d.worker_top_n, 15);
    assert_eq!(d.max_chunks, 3);
    assert!(d.include_overview && d.include_delivery && d.include_orders && d.include_workers);
    assert_eq!(d.subject_template, None);

    // `GET /meta` 的 `defaults` 段就是它的投影。
    let defaults = super::service::config_defaults();
    assert_eq!(defaults.frequency, "DAILY");
    assert_eq!(defaults.worker_metric, "QUANTITY");
    assert_eq!(defaults.webhook_id, None);
    assert_eq!(defaults.max_chunks, 3);
}

/// 四个节 slug 逐字固定（前端拿它们解释「我明明开了为什么没有这一节」）。
#[test]
fn section_slugs_are_stable() {
    assert_eq!(super::dto::SECTION_OVERVIEW, "overview");
    assert_eq!(super::dto::SECTION_DELIVERY, "delivery");
    assert_eq!(super::dto::SECTION_ORDERS, "orders");
    assert_eq!(super::dto::SECTION_WORKERS, "workers");
}

/// 分片常量：`4096` 字节上限、`48` 字节预留、条形图 12 格。
#[test]
fn chunk_constants_match_the_wecom_sender_contract() {
    use super::template::{BAR_CELLS, CHUNK_BYTE_LIMIT, DEFAULT_SUBJECT_TEMPLATE, RESERVE};
    assert_eq!(CHUNK_BYTE_LIMIT, 4096, "企业微信 markdown v1 的官方上限");
    assert_eq!(RESERVE, 48);
    const { assert!(RESERVE > 0, "「（i/N）」后缀必须预留，否则第 1 片会炸") };
    const { assert!(CHUNK_BYTE_LIMIT - RESERVE > 0) };
    assert_eq!(BAR_CELLS, 12);
    assert_eq!(DEFAULT_SUBJECT_TEMPLATE, "# 生产日报 · {date}");
}

/// 取值范围常量与 `GET /meta` 下发的那三段一致（前端输入项的 min/max 由此驱动）。
#[test]
fn range_constants_are_the_three_meta_segments() {
    use super::dto::{
        MAX_CHUNKS_MAX, MAX_CHUNKS_MIN, UPCOMING_DAYS_MAX, UPCOMING_DAYS_MIN, WORKER_TOP_N_MAX,
        WORKER_TOP_N_MIN,
    };
    assert_eq!((UPCOMING_DAYS_MIN, UPCOMING_DAYS_MAX), (1, 60));
    assert_eq!((WORKER_TOP_N_MIN, WORKER_TOP_N_MAX), (0, 200));
    assert_eq!((MAX_CHUNKS_MIN, MAX_CHUNKS_MAX), (1, 20));
    assert_eq!(
        MAX_CHUNKS_MAX, 20,
        "20 是企业微信官方限频（每个消息推送 ≤ 20 条/分钟），不是随手取的数"
    );
}

/// 本域**不** import 邮件域的枚举（域隔离）：两个枚举是**不同的类型**。
///
/// ⚠️ 这条断言**只**比较本域类型的名字，**故意不引用** `mail_digest` 的类型 ——
/// 一条测试里的跨域引用同样会构成「本域 import 了他域符号」的依赖边，与模块 doc
/// 「统计 SQL 零他域符号引用」的承诺冲突。
#[test]
fn frequency_is_a_domain_local_type() {
    let mine: Frequency = "DAILY".parse().unwrap();
    assert_eq!(mine.as_str(), "DAILY");
    assert!(
        std::any::type_name::<Frequency>().contains("wecom_daily"),
        "本域的 Frequency 必须自带域前缀，避免与邮件域那份混淆：{}",
        std::any::type_name::<Frequency>()
    );
}
