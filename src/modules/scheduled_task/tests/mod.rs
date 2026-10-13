//! scheduled_task 域 lib 单测
//!
//! 覆盖面（**全部不连库**）：
//! - SMTP 预设表（`dto::MailAccountPreset`）：解析 / 取值域 / host/port/tls_mode 映射
//! - 写侧校验矩阵（`service` 的 `validate_*`）：21704 的每条规则
//! - 出参的**密文屏蔽**：`MailAccountOut::from_row` 逐字段挑选这件事本身就是断言对象
//! - 域隔离护栏（`scheduled_task_domain_depends_on_no_other_domain`）
//!
//! ## ⚠️ 遵守「测试数据两条硬约定」第 2 条
//!
//! 本文件**不插入任何带 `now_naive()` 时间戳的行**，因此不存在「查询窗口硬编码
//! 绝对日期」的写法。若将来在本文件加需要落库的用例，窗口必须由 `now_naive()`
//! 推导，或照 `tests/statistics/event_driven.rs` 的 `insert_*(…, at_date)` 把
//! 插入时间戳一起钉死 —— 否则下个月 1 号会再次变红。

use super::dto::{MailAccountOut, MailAccountPreset};
use crate::modules::scheduled_task::model::TMailAccount;

// ============================================================================
// SMTP 预设
// ============================================================================

#[test]
fn preset_parses_all_variants_and_lists_legal_values() {
    assert_eq!(MailAccountPreset::ALL.len(), 2);
    for p in MailAccountPreset::ALL {
        let parsed = MailAccountPreset::parse(p.as_str()).expect("字面量必须可解析");
        assert_eq!(parsed, *p);
    }
    // 未知值 → 21704，且错误信息带合法值清单
    let err = MailAccountPreset::parse("OUTLOOK").unwrap_err();
    assert_eq!(err.code(), 21704);
    let msg = err.to_string();
    assert!(
        msg.contains("NET_EASE_163") && msg.contains("TENCENT_QQ"),
        "{msg}"
    );
}

/// 两家预设的 host / port / tls_mode 是**硬编码的对外契约**（前端下拉里展示会显示
/// host；发信时的 SNI 域名也是它）⇒ 逐字钉死，防「改主机名」被当成无害重构。
#[test]
fn preset_smoke_parameters_are_pinned() {
    let netease = MailAccountPreset::NetEase163;
    assert_eq!(netease.as_str(), "NET_EASE_163");
    assert_eq!(netease.label(), "网易 163");
    assert_eq!(netease.host(), "smtp.163.com");
    assert_eq!(netease.port(), 465);
    assert_eq!(netease.tls_mode().to_string(), "WRAPPED");

    let qq = MailAccountPreset::TencentQQ;
    assert_eq!(qq.as_str(), "TENCENT_QQ");
    assert_eq!(qq.label(), "QQ 邮箱");
    assert_eq!(qq.host(), "smtp.qq.com");
    assert_eq!(qq.port(), 465);
    assert_eq!(qq.tls_mode().to_string(), "WRAPPED");
}

/// ⚠️ **不许出现明文/STARTTLS 预设**：lettre 的 `Tls::Opportunistic` 在服务端
/// 不支持时会**降级明文继续发**（凭据可能裸传），而预设的语义是「选一个能用的
/// 服务商」，不该让管理员去挑协议档位。
#[test]
fn every_preset_is_implicit_tls_on_465() {
    for p in MailAccountPreset::ALL {
        assert_eq!(p.port(), 465, "{} 必须是隐式 TLS 端口", p.as_str());
        assert_eq!(
            p.tls_mode().to_string(),
            "WRAPPED",
            "{} 不得使用 STARTTLS / NONE",
            p.as_str()
        );
    }
}

// ============================================================================
// 写侧校验
// ============================================================================

// ============================================================================
// 写侧校验矩阵（21704 的每条规则）
// ============================================================================

/// 授权码：空 / 纯空白 / 含控制字符都必须被拒（控制字符会进 SMTP `AUTH` 命令行，
/// 是凭据注入面）；含普通空格的**允许**（真实授权码不会有，但误拒一个合法串的
/// 代价是「管理员看着一个正确的码却被拒」，比放行一个空格严重）。
#[test]
fn secret_validation_matrix() {
    for bad in ["", "   ", "abc\r\ndef", "abc\ndef", "\u{0}abc"] {
        let err = super::service::validate_secret(bad).expect_err(&format!("{bad:?} 必须被拒"));
        assert_eq!(err.code(), 21704);
    }
    assert_eq!(
        super::service::validate_secret("  ab12cd  ").unwrap(),
        "ab12cd"
    );
    // 超长（上限 200 字）
    assert!(super::service::validate_secret(&"x".repeat(201)).is_err());
    assert!(super::service::validate_secret(&"x".repeat(200)).is_ok());
}

/// 名称：trim 后非空、≤50 **字**（不是字节 —— 20 个纯中文字只有 60 字节，
/// 按字节判会误伤管理员最常用的中文命名）。
#[test]
fn name_validation_uses_char_count_not_bytes() {
    assert_eq!(
        super::service::validate_name("  汇森告警  ").unwrap(),
        "汇森告警"
    );
    assert_eq!(
        super::service::validate_name(&"名".repeat(50))
            .unwrap()
            .len(),
        150
    ); // 50 字 = 150 字节
    assert!(super::service::validate_name(&"名".repeat(51)).is_err());
    assert!(super::service::validate_name("   ").is_err());
}

/// 登录账号：必须像邮箱、拒空白 / CR / LF、≤120 字。
#[test]
fn account_validation_matrix() {
    assert_eq!(
        super::service::validate_account(" ops@163.com ").unwrap(),
        "ops@163.com"
    );
    for bad in [
        "",
        "  ",
        "ops163.com",
        "a@b.com c@x.com",
        "a@b.com\r\nX-Evil: 1",
    ] {
        assert!(
            super::service::validate_account(bad).is_err(),
            "{bad:?} 必须被拒"
        );
    }
}

// ============================================================================
// 出参：密文不回显
// ============================================================================

fn sample_row() -> TMailAccount {
    let ts = crate::infra::clock::now_naive();
    TMailAccount {
        id: 9_000_000_000_000_000_001,
        version: 3,
        name: "汇森告警邮箱".into(),
        preset: "NET_EASE_163".into(),
        account: "ops@163.com".into(),
        encrypted_secret: "Y2Fubm90LWJlLWV2aWxvZA==".into(),
        is_default: true,
        created_at: ts,
        created_by: Some(1),
        updated_at: ts,
        updated_by: Some(1),
        deleted_at: None,
    }
}

/// ★ 核心回归：**出参里不能出现密文，也不能出现任何掩码 / 指纹**。
///
/// 这条断言的是「`from_row` 是逐字段挑选」这个事实：把 `encrypted_secret` 加进
/// `MailAccountOut` 会让它立刻红。掩码同理 —— 出参里加一个
/// `secret_masked: "abc***"` 也会红（字段名里出现 `secret` 就足以判失败）。
#[test]
fn account_out_never_exposes_secret_material() {
    let out = MailAccountOut::from_row(&sample_row());
    let json = serde_json::to_string(&out).expect("出参必须可序列化");
    assert!(
        !json.contains("Y2Fubm90LWJlLWV2aWxvZA=="),
        "出参泄漏了密文：{json}"
    );
    assert!(
        !json.to_lowercase().contains("secret"),
        "出参出现了与 secret 相关的字段：{json}"
    );
    assert!(
        !json.to_lowercase().contains("mask"),
        "出参出现了掩码字段：{json}"
    );
    // 正向：确实带出了管理员需要的公开信息
    assert!(json.contains("ops@163.com"));
    assert!(json.contains("smtp.163.com"));
    assert!(json.contains("\"port\":465"));
    assert!(json.contains("汇森告警邮箱"));
}

/// `id` 必须序列化成 **JSON string**（雪花 ID 防 JS 精度截断）。
#[test]
fn account_out_serializes_id_as_string() {
    let json = serde_json::to_string(&MailAccountOut::from_row(&sample_row())).unwrap();
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert!(
        v.get("id").and_then(|x| x.as_str()).is_some(),
        "id 必须是 JSON string：{json}"
    );
}

/// ★ **表行模型的 `Debug` 必须屏蔽密文**（与 `MailAccountSecret` / `ResolvedSender` 同款）。
///
/// 这是「不靠约定靠类型」的回归闸：只要有人把 `impl Debug` 删掉改回
/// `#[derive(Debug)]`，本测试立刻红；而那之后任何一处 `{row:?}` 都会把授权码密文写进
/// 日志（密文是 base64，可解码）。
#[test]
fn account_row_debug_masks_ciphertext() {
    let row = sample_row();
    let dbg = format!("{row:?}");
    assert!(
        !dbg.contains(&row.encrypted_secret),
        "表行 Debug 泄漏了密文：{dbg}"
    );
    assert!(
        dbg.contains("encrypted_secret: \"***\""),
        "密文应固定显示为 ***：{dbg}"
    );
    // 排障需要的非敏感字段仍可见（否则这条 Debug 等于没用）
    assert!(dbg.contains("ops@163.com"), "account 应可见：{dbg}");
    assert!(dbg.contains("汇森告警邮箱"), "name 应可见：{dbg}");
}

/// 库里存了未知 preset（手工改库 / 加了预设没同步）时**降级而不丢行**：
/// 列表页整体 500 会让人以为「邮箱全没了」。
#[test]
fn unknown_preset_degrades_instead_of_panicking() {
    let mut row = sample_row();
    row.preset = "OUTLOOK".into();
    let out = MailAccountOut::from_row(&row);
    assert_eq!(out.preset, "OUTLOOK");
    assert_eq!(out.host, "");
    assert_eq!(out.port, 0);
}

// ============================================================================
// 域隔离护栏
// ============================================================================

/// 域隔离护栏：把「scheduled_task 不 import 其它域」从口头约定变成 CI 强制。
///
/// 探测器实现（剥注释、根段 + 域路径前缀匹配、元测试）见
/// [`crate::shared::domain_guard`]，本域只负责传参 + 域专属指引。
///
/// ⚠️ 本域**适用**这条护栏（与 `dashboard` / `statistics` 同款）：跨域只读的
/// `t_mail_digest_config` 引用检查是本域自己写的 SQL，不 import `mail_digest`
/// 的任何符号。见 [`super`] 的模块 doc 末节。
#[test]
fn scheduled_task_domain_depends_on_no_other_domain() {
    use std::path::Path;

    use crate::shared::domain_guard::assert_no_foreign_domain;

    assert_no_foreign_domain(
        "scheduled_task",
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src/modules/scheduled_task"),
        "需要别的域的数据时，正确做法是像 statistics / admin / dashboard 那样在本域 SQL 里\
         只读聚合（本域唯一的跨域表引用是 repo.rs::count_digest_configs_referencing 查 \
         t_mail_digest_config 的一个 COUNT，SQL 真源就在本文件里），而不是 import 别人的\
         service / repo。",
    );
}
