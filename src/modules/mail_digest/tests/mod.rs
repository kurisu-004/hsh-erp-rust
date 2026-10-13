//! mail_digest 域 lib 单测
//!
//! 覆盖面（**只测不连库的纯逻辑**；不连 DB 的部分全在这里，连库的部分由
//! `query_as!` 宏的编译期校验 + B2 任务的集成测试负责）：
//! - 枚举解析 / 取值域（`dto`）
//! - 配置校验（`service::validate_*`）—— 21703 / 21704 的每条规则
//! - HTML 转义与正文渲染（`template` 内部自带 `mod tests`，见该文件）
//! - cron 生成（`schedule` 内部自带 `mod tests`，见该文件）
//!
//! ## ⚠️ 遵守「测试数据两条硬约定」第 2 条
//! 本文件**不插入任何带 `now_naive()` 时间戳的行**，因此不存在「查询窗口硬编码
//! 绝对日期」的写法。若将来在本文件加需要落库的用例，窗口必须由 `now_naive()`
//! 推导，或照 `tests/statistics/event_driven.rs` 的 `insert_*(…, at_date)` 把
//! 插入时间戳一起钉死 —— 否则下个月 1 号会再次变红。
//!
//! ## 为什么不装 `assert_no_foreign_domain`
//! 见 [`super`] 的模块 doc 末节：本域**共用** dashboard 的 `DELIVERY_STATUSES`
//! 常量是既定契约（CLAUDE.md 明确要求四处共用），不是「跨域依赖违规」。

use super::dto::{
    Frequency, SaveConfigRequest, TriggerSource, VisibleColumn, parse_visible_columns,
};
use super::service;
use super::template::html_escape;

// ============================================================================
// 枚举
// ============================================================================

#[test]
fn frequency_parses_all_four_variants() {
    assert_eq!(Frequency::Daily.as_str(), "DAILY");
    assert_eq!(Frequency::Weekly.as_str(), "WEEKLY");
    assert_eq!(Frequency::Monthly.as_str(), "MONTHLY");
    assert_eq!(Frequency::ManualOnly.as_str(), "MANUAL_ONLY");
    assert_eq!(Frequency::ALL.len(), 4);

    for f in Frequency::ALL {
        let parsed: Frequency = f.as_str().parse().expect("字面量必须可解析");
        assert_eq!(parsed, *f);
    }
    // 未知值 → 21704，且错误信息带合法值清单
    let err = "HOURLY".parse::<Frequency>().unwrap_err();
    assert_eq!(err.code(), 21704);
    assert!(err.to_string().contains("MANUAL_ONLY"));
}

#[test]
fn frequency_meta_flags_match_the_product_decisions() {
    // 仅 DAILY/WEEKLY/MONTHLY 要时刻；仅 WEEKLY 要 weekday；仅 MONTHLY 要 day_of_month
    assert!(Frequency::Daily.needs_time());
    assert!(!Frequency::Daily.needs_weekday());
    assert!(!Frequency::Daily.needs_day_of_month());

    assert!(Frequency::Weekly.needs_time());
    assert!(Frequency::Weekly.needs_weekday());
    assert!(!Frequency::Weekly.needs_day_of_month());

    assert!(Frequency::Monthly.needs_time());
    assert!(!Frequency::Monthly.needs_weekday());
    assert!(Frequency::Monthly.needs_day_of_month());

    assert!(!Frequency::ManualOnly.needs_time());
    assert!(!Frequency::ManualOnly.needs_weekday());
    assert!(!Frequency::ManualOnly.needs_day_of_month());
}

#[test]
fn visible_column_whitelist_is_closed_and_labels_unique() {
    // 白名单 11 项，标签互不重复（否则邮件表头会有两列同名，读信人分不清）
    let mut labels: Vec<&str> = VisibleColumn::ALL.iter().map(|c| c.label()).collect();
    labels.sort_unstable();
    let before = labels.len();
    labels.dedup();
    assert_eq!(labels.len(), before, "列标签有重复");
    for c in VisibleColumn::ALL {
        assert_eq!(VisibleColumn::parse(c.as_str()), Some(*c), "列名不可逆解析");
    }
    assert_eq!(VisibleColumn::parse("customer_name_xxx"), None);

    // 缺省 8 列必须是全量白名单的子集
    for c in VisibleColumn::DEFAULT_SET {
        assert!(
            VisibleColumn::ALL.contains(c),
            "缺省列 {c:?} 不在全量白名单里"
        );
    }
}

#[test]
fn parse_visible_columns_rejects_unknown_key_with_21704() {
    let ok = parse_visible_columns(&["serial_no".to_string(), "name".to_string()]).unwrap();
    assert_eq!(ok, vec![VisibleColumn::SerialNo, VisibleColumn::Name]);

    let err = parse_visible_columns(&["serial_no".into(), "图号".into()]).unwrap_err();
    assert_eq!(err.code(), 21704);
    // 错误信息列出合法列名，省得前端对着猜
    assert!(err.to_string().contains("drawing_no"));
}

#[test]
fn trigger_source_round_trips() {
    assert_eq!(TriggerSource::Scheduled.as_str(), "SCHEDULED");
    assert_eq!(TriggerSource::Manual.as_str(), "MANUAL");
    assert_eq!(
        "SCHEDULED".parse::<TriggerSource>().unwrap(),
        TriggerSource::Scheduled
    );
    assert!("AUTO".parse::<TriggerSource>().is_err());
}

// ============================================================================
// 配置校验（21703 / 21704）
// ============================================================================

/// 构造一份合法的 `POST /config` 请求体（各用例只改自己关心的字段）。
fn req() -> SaveConfigRequest {
    SaveConfigRequest {
        version: 0,
        enabled: true,
        frequency: "DAILY".into(),
        send_hour: Some(8),
        send_minute: Some(30),
        weekday: None,
        day_of_month: None,
        recipients: vec!["ops@example.com".into()],
        cc_recipients: vec![],
        include_overdue: true,
        include_upcoming: true,
        upcoming_days: 14,
        visible_columns: vec![
            "serial_no".into(),
            "drawing_no".into(),
            "name".into(),
            "quantity".into(),
            "customer_name".into(),
            "status".into(),
            "system_delivery_date".into(),
            "remaining_days".into(),
        ],
        subject_template: None,
        // 2026-10-16：发信邮箱。`None` = 未选邮箱（合法，见
        // `service::validate_sender_account`），所以域内这批**纯校验**用例可以
        // 照常跑 —— 存在性校验是独立的一条 async 规则（要查库）。
        sender_account_id: None,
    }
}

#[test]
fn valid_daily_request_passes_validation() {
    let n = service::validate_save_request(&req()).unwrap();
    assert_eq!(n.frequency, Frequency::Daily);
    assert_eq!(n.upcoming_days, 14);
    assert_eq!(n.visible_columns.len(), 8);
    assert!(n.enabled);
}

#[test]
fn weekly_requires_weekday_and_clears_day_of_month() {
    let mut r = req();
    r.frequency = "WEEKLY".into();
    r.weekday = None;
    r.day_of_month = Some(5);
    let err = service::validate_save_request(&r).unwrap_err();
    assert_eq!(err.code(), 21704, "WEEKLY 缺 weekday 必须报错");

    r.weekday = Some(1);
    let n = service::validate_save_request(&r).unwrap();
    assert_eq!(n.weekday, Some(1));
    assert_eq!(n.day_of_month, None, "WEEKLY 之下 day_of_month 必须被清掉");
}

#[test]
fn monthly_requires_day_of_month_in_range_1_to_28() {
    let mut r = req();
    r.frequency = "MONTHLY".into();
    r.day_of_month = None;
    assert_eq!(
        service::validate_save_request(&r).unwrap_err().code(),
        21704
    );

    // 上界 28 是刻意的（避开 2 月末 / 30-31 日短月）
    for bad in [Some(0), Some(29), Some(31), Some(-1)] {
        r.day_of_month = bad;
        let e = service::validate_save_request(&r).unwrap_err();
        assert_eq!(e.code(), 21704, "day_of_month={bad:?} 应被拒");
    }
    r.day_of_month = Some(28);
    assert!(service::validate_save_request(&r).is_ok());
}

#[test]
fn daily_clears_weekday_and_day_of_month() {
    let mut r = req();
    r.weekday = Some(3);
    r.day_of_month = Some(9);
    let n = service::validate_save_request(&r).unwrap();
    assert_eq!(
        n.weekday, None,
        "DAILY 之下 weekday 必须清 NULL（不留脏值）"
    );
    assert_eq!(n.day_of_month, None, "DAILY 之下 day_of_month 必须清 NULL");
}

#[test]
fn manual_only_does_not_require_time_but_keeps_recipients_required() {
    let mut r = req();
    r.frequency = "MANUAL_ONLY".into();
    r.send_hour = None;
    r.send_minute = None;
    let n = service::validate_save_request(&r).unwrap();
    assert!(n.recipients.iter().any(|x| !x.is_empty()));

    // 即便 MANUAL_ONLY 也必须至少一个收件人 —— 否则手动发送无从下手。
    // ⚠️ 注意**校验层**不拦空收件人：它只逐个校验格式。空数组由下游
    // `service::deliver` 的「收件人为空」分支拦（21704）。本用例钉死这条分工。
    r.recipients = vec![];
    let n = service::validate_save_request(&r).unwrap();
    assert!(n.recipients.is_empty(), "校验层不判空（判空在 deliver）");
    assert!(service::validate_recipients(&[]).unwrap().is_empty());
}

#[test]
fn hour_and_minute_bounds_are_enforced() {
    for bad in [Some(-1), Some(24), Some(99)] {
        let mut r = req();
        r.send_hour = bad;
        assert_eq!(
            service::validate_save_request(&r).unwrap_err().code(),
            21704,
            "send_hour={bad:?} 应被拒"
        );
    }
    for bad in [Some(-1), Some(60), Some(99)] {
        let mut r = req();
        r.send_minute = bad;
        assert_eq!(
            service::validate_save_request(&r).unwrap_err().code(),
            21704,
            "send_minute={bad:?} 应被拒"
        );
    }
    // 缺时刻 → 21704（三个非 MANUAL_ONLY 频率）
    for f in ["DAILY", "WEEKLY", "MONTHLY"] {
        let mut r = req();
        r.frequency = f.into();
        r.weekday = Some(1);
        r.day_of_month = Some(1);
        r.send_hour = None;
        assert_eq!(
            service::validate_save_request(&r).unwrap_err().code(),
            21704,
            "频率 {f} 缺 send_hour 应被拒"
        );
    }
}

#[test]
fn upcoming_days_are_clamped_to_1_to_60() {
    for (raw, want) in [
        (-5, 1),
        (0, 1),
        (1, 1),
        (14, 14),
        (60, 60),
        (61, 60),
        (9999, 60),
    ] {
        let mut r = req();
        r.upcoming_days = raw;
        let n = service::validate_save_request(&r).unwrap();
        assert_eq!(
            n.upcoming_days, want,
            "upcoming_days={raw} 应 clamp 到 {want}"
        );
    }
}

#[test]
fn both_sections_off_is_rejected() {
    let mut r = req();
    r.include_overdue = false;
    r.include_upcoming = false;
    let e = service::validate_save_request(&r).unwrap_err();
    assert_eq!(e.code(), 21704);
}

#[test]
fn recipients_whitespace_is_trimmed_and_blank_entries_dropped() {
    let out = service::validate_recipients(&["  a@x.com ".into(), "   ".into(), "b@x.com".into()])
        .unwrap();
    assert_eq!(out, vec!["a@x.com".to_string(), "b@x.com".to_string()]);
}

// ============================================================================
// 邮箱校验（21703）—— 含 SMTP header 注入面
// ============================================================================

#[test]
fn valid_emails_pass() {
    for a in [
        "a@b.com",
        "first.last@sub.domain.cn",
        "user+tag@example.co.uk",
        "user@10.0.0.1",
    ] {
        assert!(service::validate_email(a).is_ok(), "{a} 应合法");
    }
}

#[test]
fn crlf_in_address_is_rejected_as_header_injection() {
    // ⚠️ 这是安全断言：CR/LF 能让攻击者借本系统给任意第三方发信（Bcc 注入）。
    for bad in [
        "a@b.com\r\nBcc: victim@x.com",
        "a@b.com\nBcc: victim@x.com",
        "a@b.com\r",
    ] {
        let e = service::validate_email(bad).unwrap_err();
        assert_eq!(e.code(), 21703, "{bad:?} 必须被拒为 21703");
        assert!(
            e.to_string().contains("CR/LF"),
            "错误信息应点明是 CR/LF 注入面：{e}"
        );
    }
}

#[test]
fn malformed_emails_are_rejected() {
    for bad in [
        "no-at-sign",
        "@nolocal.com",
        "a@",
        "a@nodomain",
        "a@.leading.com",
        "a@trailing.",
        "a@double..dot.com",
        "a b@c.com",
        "a@b.com,c@d.com",
        "a<b>@c.com",
        "a@b.com;Cc:x@y.com",
    ] {
        let e = service::validate_email(bad).unwrap_err();
        assert_eq!(e.code(), 21703, "{bad:?} 应被拒");
    }
}

// ============================================================================
// 主题模板
// ============================================================================

#[test]
fn subject_template_rejects_crlf_and_overlong() {
    assert_eq!(service::validate_subject_template(None).unwrap(), None);
    assert_eq!(
        service::validate_subject_template(Some("   ")).unwrap(),
        None,
        "空白模板按「未设置」处理"
    );
    // CR/LF 注入
    let e = service::validate_subject_template(Some("标题\r\nBcc: x@y.com")).unwrap_err();
    assert_eq!(e.code(), 21704);
    // 超长按**字符**算（不是字节）：80 个中文 = 240 字节但 80 字，应通过
    let cn80 = "标".repeat(80);
    assert!(
        service::validate_subject_template(Some(&cn80))
            .unwrap()
            .is_some()
    );
    let cn201 = "标".repeat(201);
    let e = service::validate_subject_template(Some(&cn201)).unwrap_err();
    assert_eq!(e.code(), 21704);
}

// ============================================================================
// 杂项
// ============================================================================

#[test]
fn recipient_masking_keeps_domain_but_hides_local_part() {
    assert_eq!(
        service::mask_recipients(&["zhangsan@example.com".to_string()]),
        "z***@example.com"
    );
    assert_eq!(
        service::mask_recipients(&["a@x.com".into(), "b@y.com".into()]),
        "a***@x.com,b***@y.com"
    );
    // 非邮箱形状（脏数据）不泄露任何字符
    assert_eq!(service::mask_recipients(&["garbage".into()]), "***");
    assert_eq!(service::mask_recipients(&[]), "");
}

/// `html_escape` 在本文件被重导出一次，让「转义是本域的横切关注点」这件事
/// 在测试索引里可见（真正的逐字符断言在 `template` 的 `mod tests` 里）。
#[test]
fn html_escape_is_exported_and_escapes_ampersand_first() {
    assert_eq!(html_escape("a&b"), "a&amp;b");
}
