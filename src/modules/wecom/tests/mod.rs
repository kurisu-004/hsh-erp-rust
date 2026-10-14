//! wecom 域 lib 单测
//!
//! 覆盖面（**全部不连库、不发网络请求**）：
//! - 写侧校验矩阵（`service` 的 `validate_*`）：21803 / 21804 的每条规则
//! - 掩码算法（前 4 + `***` + 后 4；短 key 降级不 panic）
//! - 出参的**密文 / 明文都不回显**：`WebhookOut::from_row` 逐字段挑选本身就是断言对象
//! - 凭据类型的 `Debug` 纪律：表行手写屏蔽版、`ResolvedTarget` 手写屏蔽版，
//!   两个入参 DTO 一律**没有** `Debug`
//! - `sender::classify` 的四条分支（★ **不发真实网络请求**，`/test` 的真发路径靠
//!   `tests/wecom/probe.rs` 的集成用例覆盖）
//! - 入参的 serde 语义：`webhook_url` 省略 ⇒ `None`；`version` 缺失 ⇒ 反序列化失败
//! - 域隔离护栏（`wecom_domain_depends_on_no_other_domain`）

use crate::modules::wecom::dto::{WebhookOut, mask_webhook_url};
use crate::modules::wecom::model::TWeComWebhookRow;
use crate::modules::wecom::repo::{NewWebhook, WebhookPatch};
use crate::modules::wecom::sender::{SendOutcome, classify};
use crate::modules::wecom::service::{ResolvedTarget, validate_name, validate_webhook_url};

/// 一条测试用的合法地址（key 是 36 字符 uuid 形态，与真实 key 同长度）。
const GOOD_URL: &str =
    "https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=693a91f6-7xxx-4bc4-97a0-0ec2sifa5aaa";

// ============================================================================
//  写侧校验矩阵（21803 / 21804）
// ============================================================================

/// ★ 地址校验矩阵：**非法一律 `21803`**，形态与前端的 `WECOM_WEBHOOK_URL_RE` 逐字对齐。
///
/// ⚠️ **SSRF 面**：本域会拿这个地址真发 HTTP 请求（`/test` 探针 + 内容域推送），放开
/// host 就等于让一条数据库记录变成任意地址的请求发起器。故「非 qyapi 域」「http 而非
/// https」「地址里带空格」三类必须一律拒。
#[test]
fn url_validation_matrix() {
    // 正向：带首尾空白也接受（trim 后判）
    assert_eq!(
        validate_webhook_url(&format!("  {GOOD_URL}\n")).unwrap(),
        GOOD_URL
    );

    let bad: &[(&str, &str)] = &[
        ("缺 key", "https://qyapi.weixin.qq.com/cgi-bin/webhook/send"),
        (
            "缺 key 参数名",
            "https://qyapi.weixin.qq.com/cgi-bin/webhook/send?693a91f6-7xxx",
        ),
        (
            "非 qyapi 域（SSRF 面）",
            "https://evil.example.com/cgi-bin/webhook/send?key=693a91f6-7xxx-4bc4",
        ),
        (
            "http 而非 https",
            "http://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=693a91f6-7xxx-4bc4",
        ),
        (
            "key 为空",
            "https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=",
        ),
        (
            "key 里含空格（\\S+ 不允许）",
            "https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=693a 91f6",
        ),
        (
            "含 CR-LF（头注入面）",
            "https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=693a\r\nX-Evil: 1",
        ),
        (
            "路径多了一段",
            "https://qyapi.weixin.qq.com/cgi-bin/webhook/send/v2?key=693a91f6-7xxx-4bc4",
        ),
        ("空串", ""),
        ("纯空白", "   "),
    ];
    for (why, raw) in bad {
        let err = validate_webhook_url(raw).unwrap_err(); // 该组全部必须被拒
        assert_eq!(
            err.code(),
            21803,
            "{why}：{raw:?} 必须是 21803，实际 {}",
            err.code()
        );
    }

    // ⚠️ **刻意放行的一条**：key 里含 `&`（`?key=A&x=1`）。正则的 `\S+` 不含任何空白
    // 约束，`&` 落在里面 ⇒ **前端 `WECOM_WEBHOOK_URL_RE` 同样接受**（契约要求逐字一致，
    // 后端不得比前端严）。后果是企微会把 key 解析成 `A`、忽略 `x=1` ⇒ 推送失败（21802），
    // 不会静默错发。真实 key 是 uuid，不含这类字符。
    assert!(
        validate_webhook_url("https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=A&x=1").is_ok()
    );

    // 超长（上限 500 **字**，按 char 计）：前缀 + 501 - 前缀长度 个 `k`
    const PREFIX: &str = "https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=";
    let long = format!("{PREFIX}{}", "k".repeat(501 - PREFIX.chars().count()));
    let err = validate_webhook_url(&long).unwrap_err();
    assert_eq!(err.code(), 21803, "超长必须是 21803");
    // 恰好 500 字合法
    let exact = format!("{PREFIX}{}", "k".repeat(500 - PREFIX.chars().count()));
    assert_eq!(exact.chars().count(), 500);
    assert!(validate_webhook_url(&exact).is_ok());
}

/// ⚠️ **报错文案绝不能回显收到的地址** —— 它可能是一个真的地址（管理员从别处粘来的），
/// 拼进 message 就等于把凭证写进日志与 HTTP 响应体。
#[test]
fn url_validation_error_never_echoes_the_address() {
    let err =
        validate_webhook_url("https://evil.example.com/hook?key=693a-secret-value").unwrap_err();
    let msg = err.to_string();
    assert!(
        !msg.contains("693a-secret-value"),
        "报错文案回显了地址：{msg}"
    );
    assert!(
        !msg.contains("evil.example.com"),
        "报错文案回显了 host：{msg}"
    );
}

/// 名称：trim 后非空、≤50 **字**（不是字节 —— 50 个纯中文字有 150 字节）、
/// 拒控制字符。失败 ⇒ `21804`。
#[test]
fn name_validation_uses_char_count_not_bytes() {
    assert_eq!(
        validate_name("  管理层群机器人  ").unwrap(),
        "管理层群机器人"
    );
    assert_eq!(validate_name(&"名".repeat(50)).unwrap().len(), 150); // 50 字 = 150 字节
    let err = validate_name(&"名".repeat(51)).unwrap_err();
    assert_eq!(err.code(), 21804);
    for bad in ["", "   ", "甲\r\n乙", "甲\u{0}乙"] {
        assert_eq!(
            validate_name(bad).unwrap_err().code(),
            21804,
            "{bad:?} 必须被拒为 21804"
        );
    }
}

// ============================================================================
//  掩码
// ============================================================================

/// ★ 掩码格式：地址保留，`key` **前 4 个字符** + `***` + **后 4 个字符**。
#[test]
fn mask_keeps_first_and_last_four_of_key() {
    assert_eq!(
        mask_webhook_url(GOOD_URL),
        "https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=693a***5aaa"
    );
    // 中间 28 个字符一个字都不许漏
    let masked = mask_webhook_url(GOOD_URL);
    assert!(!masked.contains("91f6"), "{masked}");
    assert!(!masked.contains("0ec2"), "{masked}");
    assert!(!masked.contains("sifa"), "{masked}");
}

/// 短 key 必须**降级**而不是「前 4 + 后 4」（那会重叠 ⇒ 等于把整个 key 打出来），
/// 更不能 panic（按字节切多字节字符会 panic）。
#[test]
fn mask_does_not_panic_on_short_key() {
    for key in ["", "a", "abcd", "abcdefgh", "12345678"] {
        let url = format!("https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key={key}");
        let masked = mask_webhook_url(&url);
        // 降级成固定 8 个星号，且**不泄露真实长度**
        assert_eq!(
            masked, "https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=********",
            "key={key:?} 必须降级"
        );
    }
    // 多字节 key：按 char 切，不 panic、不切出半个字符（9 字 ⇒ 前 4 + 后 4 + `***`）
    let cn =
        mask_webhook_url("https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=一二三四五六七八九");
    assert_eq!(
        cn,
        "https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=一二三四***六七八九"
    );
}

/// 形态无法识别（**连 `?key=` 都没有**，库里被手工改坏）⇒ 整段打码，**不回显任何原文**。
#[test]
fn mask_of_unparsable_shape_is_fully_masked() {
    for raw in [
        "not-a-url",
        "",
        "https://qyapi.weixin.qq.com/cgi-bin/webhook/send",
    ] {
        assert_eq!(mask_webhook_url(raw), "***", "{raw:?} 必须整段打码");
    }
    // ⚠️ 反过来：**有** `?key=` 的畸形地址仍走「前 4 + *** + 后 4」——凭据在 query 里，
    // 逐段掩码比整段打码更有排查价值（能看出「填的是哪个群的 key」），而 host 不是凭据。
    let evil = mask_webhook_url("https://evil.example.com/hook?key=693a-secret-value");
    assert!(!evil.contains("secret"), "{evil}");
}

/// 库里被手工改成「key 后面还挂了一段 query」时，**尾部那一段不得进掩码**。
#[test]
fn mask_drops_extra_query_segments() {
    assert_eq!(
        mask_webhook_url(
            "https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=693a91f6-7xxx-4bc4-97a0-0ec2sifa5aaa&secret=zzz"
        ),
        "https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=693a***5aaa"
    );
}

// ============================================================================
//  出参：密文 / 明文都不回显
// ============================================================================

fn sample_row() -> TWeComWebhookRow {
    let ts = crate::infra::clock::now_naive();
    TWeComWebhookRow {
        id: 9_000_000_000_000_000_001,
        version: 3,
        name: "管理层群机器人".into(),
        // 一段 base64 密文（真实形态：`base64(nonce‖ct‖tag)`，可解码 ⇒ 等同明文的危险度）
        encrypted_url: "Y2Fubm90LWJlLWV2aWxvZA==".into(),
        is_default: true,
        created_at: ts,
        created_by: Some(1),
        updated_at: ts,
        updated_by: Some(1),
        deleted_at: None,
    }
}

/// ★ 核心回归：**出参里不能出现密文，也不能出现地址明文或 key 的中段**。
///
/// 这条断言的是「`from_row` 是逐字段挑选」这个事实：把 `encrypted_url` 加进
/// `WebhookOut` 会让它立刻红。
#[test]
fn webhook_out_never_exposes_ciphertext_or_plaintext_url() {
    let out = WebhookOut::from_row(&sample_row(), GOOD_URL);
    let json = serde_json::to_string(&out).expect("出参必须可序列化");
    assert!(
        !json.contains("Y2Fubm90LWJlLWV2aWxvZA=="),
        "出参泄漏了密文：{json}"
    );
    assert!(
        !json.contains("693a91f6-7xxx-4bc4-97a0-0ec2sifa5aaa"),
        "出参泄漏了地址明文：{json}"
    );
    assert!(!json.contains("693a91f6"), "url_masked 未被掩码：{json}");
    // 字段名层面：不得出现 `url` 这个裸字段（只允许 `url_masked`）
    assert!(!json.contains("\"url\":"), "出参出现了裸 url 字段：{json}");
    assert!(
        !json.contains("url_configured"),
        "出参出现了 url_configured 死字段：{json}"
    );
    // 正向：管理员需要的公开信息齐
    assert!(json.contains("管理层群机器人"), "{json}");
    assert!(
        json.contains("key=693a***5aaa"),
        "应给出可辨识的掩码：{json}"
    );
}

/// `id` 必须序列化成 **JSON string**（雪花 ID 防 JS 精度截断）。
#[test]
fn webhook_out_serializes_id_as_string() {
    let json = serde_json::to_string(&WebhookOut::from_row(&sample_row(), GOOD_URL)).unwrap();
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert!(
        v.get("id").and_then(|x| x.as_str()).is_some(),
        "id 必须是 JSON string：{json}"
    );
    assert_eq!(v["id"], "9000000000000000001");
}

// ============================================================================
//  凭据类型的 Debug 纪律
// ============================================================================

/// ★ **表行模型的 `Debug` 必须屏蔽密文**。
///
/// 这是「不靠约定靠类型」的回归闸：只要有人把 `impl Debug` 删掉改回
/// `#[derive(Debug)]`，本测试立刻红；而那之后任何一处 `{row:?}` 都会把地址密文写进
/// 日志（密文是 base64，可解码 ⇒ 泄漏后果约等于明文）。
#[test]
fn webhook_row_debug_masks_ciphertext() {
    let row = sample_row();
    let dbg = format!("{row:?}");
    assert!(
        !dbg.contains(&row.encrypted_url),
        "表行 Debug 泄漏了密文：{dbg}"
    );
    assert!(
        dbg.contains("encrypted_url: \"***\""),
        "密文应固定显示为 ***：{dbg}"
    );
    // 排障需要的非敏感字段仍可见（否则这条 Debug 等于没用）
    assert!(dbg.contains("管理层群机器人"), "name 应可见：{dbg}");
}

/// ★ **两个写侧的凭据载体 `Debug` 必须屏蔽密文**（`NewWebhook` / `WebhookPatch`）。
///
/// 那一条钉的是「读出来的那份」；这一批手上的是**写进去**的那一份。
#[test]
fn write_params_debug_masks_ciphertext() {
    let ts = crate::infra::clock::now_naive();
    let sealed = "Y2Fubm90LWJlLWV2aWxvZA==".to_string();

    let new = NewWebhook {
        id: 9_000_000_000_000_000_002,
        name: "车间群机器人".into(),
        encrypted_url: sealed.clone(),
        operator: 1,
        now: ts,
    };
    let dbg = format!("{new:?}");
    assert!(!dbg.contains(&sealed), "NewWebhook Debug 泄漏了密文：{dbg}");
    assert!(
        dbg.contains("encrypted_url: \"***\""),
        "密文应固定显示为 ***：{dbg}"
    );
    assert!(dbg.contains("车间群机器人"), "name 应可见：{dbg}");

    let patch = WebhookPatch {
        name: "财务群机器人".into(),
        encrypted_url: sealed.clone(),
        operator: 2,
        now: ts,
    };
    let dbg = format!("{patch:?}");
    assert!(
        !dbg.contains(&sealed),
        "WebhookPatch Debug 泄漏了密文：{dbg}"
    );
    assert!(dbg.contains("财务群机器人"), "name 应可见：{dbg}");
}

/// ★ **`ResolvedTarget` 的 `Debug` 必须屏蔽明文 `url`**（它上面拿的就是裸地址）。
#[test]
fn resolved_target_debug_masks_plaintext_url() {
    let t = ResolvedTarget {
        webhook_id: 9_000_000_000_000_000_003,
        name: "管理层群机器人".into(),
        masked: "https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=693a***5aaa".into(),
        url: GOOD_URL.to_string(),
    };
    let dbg = format!("{t:?}");
    assert!(!dbg.contains("693a91f6"), "Debug 泄漏了明文 url：{dbg}");
    assert!(dbg.contains("url: \"***\""), "url 应固定显示为 ***：{dbg}");
    // 非敏感字段仍可见
    assert!(dbg.contains("管理层群机器人"), "{dbg}");
    assert!(dbg.contains("693a***5aaa"), "掩码应可见：{dbg}");
}

/// ★ **两个入参 DTO 不得有 `Debug`**（`webhook_url` 是**明文** bearer 凭证）。
///
/// ⚠️ 这两个类型**刻意一个 `Debug` 都不实现**（不是手写屏蔽版 —— 明文没有可保留的
/// 「非敏感部分」可展示），故这里守的不是「屏蔽得对不对」而是「有没有」。
///
/// ⚠️ **为什么钉在源码属性上而不是编译期探针**：stable 上没有「断言某类型未实现某
/// trait」的手段（特化未稳定），而 `` `compile_fail` doctest `` 这条路走不通 ——
/// nextest **不跑** doctest，闸门形同虚设。故直接读 `dto.rs` 断言 derive 列表里没有
/// `Debug`。末尾那组**反向自检**是这道闸的保险：若将来文件结构变了导致上面的匹配恒真，
/// 「`DeleteWebhookRequest` 等无凭据 DTO 仍应 derive `Debug`」会先红。
#[test]
fn write_params_must_not_derive_debug() {
    let src = include_str!("../dto.rs");

    for ty in ["CreateWebhookRequest", "UpdateWebhookRequest"] {
        let derive = derive_line_above(src, ty);
        assert!(
            !derive.contains("Debug"),
            "{ty} 不得有 Debug（`webhook_url` 是明文 bearer 凭证）：{derive}"
        );
    }
    for ty in ["DeleteWebhookRequest", "SetDefaultWebhookRequest"] {
        let derive = derive_line_above(src, ty);
        assert!(
            derive.contains("Debug"),
            "{ty} 不含凭据、应保留 Debug（这条是上面扫描的反向自检）：{derive}"
        );
    }
}

/// 取 `pub struct <ty> {` 紧上方那条 `#[derive(...)]`（跳过它自己的 doc 注释）。
fn derive_line_above(src: &str, ty: &str) -> String {
    let lines: Vec<&str> = src.lines().collect();
    let idx = lines
        .iter()
        .position(|l| l.contains(&format!("pub struct {ty} {{")))
        .unwrap_or_else(|| panic!("dto.rs 里找不到 {ty}"));
    lines[..idx]
        .iter()
        .rev()
        .find(|l| l.trim_start().starts_with("#[derive"))
        .unwrap_or_else(|| panic!("{ty} 上方没有 derive 行"))
        .trim()
        .to_string()
}

// ============================================================================
//  入参的 serde 语义
// ============================================================================

/// `webhook_url` **省略 ⇒ `None`**（沿用旧密文的前端交互）。用裸 `String` 的话省略键会
/// 422 —— 那会让「只改名称」这个最常见的操作失败。
#[test]
fn update_request_treats_missing_url_as_none() {
    let v: serde_json::Value = serde_json::from_str(r#"{"version":3,"name":"新名字"}"#).unwrap();
    let req: crate::modules::wecom::dto::UpdateWebhookRequest = serde_json::from_value(v).unwrap();
    assert_eq!(req.version, 3);
    assert_eq!(req.name, "新名字");
    assert!(
        req.webhook_url.is_none(),
        "省略 webhook_url 必须是 None（= 沿用旧密文），实测 {:?}",
        req.webhook_url
    );

    // 显式 null 同理
    let v: serde_json::Value =
        serde_json::from_str(r#"{"version":3,"name":"新名字","webhook_url":null}"#).unwrap();
    let req: crate::modules::wecom::dto::UpdateWebhookRequest = serde_json::from_value(v).unwrap();
    assert!(req.webhook_url.is_none());
}

/// `version` **缺失 ⇒ 反序列化失败** ⇒ handler 层返回 **HTTP 422 纯文本**（不是业务信封）。
#[test]
fn write_requests_reject_missing_version() {
    for ty_check in [
        serde_json::from_value::<crate::modules::wecom::dto::UpdateWebhookRequest>(
            serde_json::json!({"name": "新名字"}),
        )
        .is_err(),
        serde_json::from_value::<crate::modules::wecom::dto::DeleteWebhookRequest>(
            serde_json::json!({}),
        )
        .is_err(),
        serde_json::from_value::<crate::modules::wecom::dto::SetDefaultWebhookRequest>(
            serde_json::json!({}),
        )
        .is_err(),
    ] {
        assert!(ty_check, "缺 version 必须反序列化失败（否则 422 契约失效）");
    }
}

// ============================================================================
//  sender::classify（★ 不发网络请求）
// ============================================================================

/// `errcode == 0` ⇒ `Ok`（**唯一**的成功判据 —— 绝不看 HTTP 状态）。
#[test]
fn classify_ok_on_zero_errcode() {
    assert_eq!(
        classify(200, r#"{"errcode":0,"errmsg":"ok"}"#),
        SendOutcome::Ok
    );
}

/// `errcode != 0` + HTTP 200 ⇒ `Failed`。这是企业微信的**常态失败形态**，
/// 不解析它就是静默成功（见 `sender.rs` 文件头）。
#[test]
fn classify_failed_on_nonzero_errcode() {
    let got = classify(200, r#"{"errcode":93000,"errmsg":"invalid webhook url"}"#);
    assert!(matches!(got, SendOutcome::Failed(_)), "{got:?}");
    let msg = format!("{got:?}");
    assert!(msg.contains("93000"), "应带 errcode：{msg}");
    assert!(msg.contains("invalid webhook url"), "应带 errmsg：{msg}");
}

/// 响应体不是合法 JSON / 缺 `errcode` ⇒ `Failed`，且**不记原文**。
#[test]
fn classify_failed_on_unparsable_body() {
    for body in [
        "",
        "<html>502</html>",
        "not json",
        r#"{"errmsg":"ok"}"#,
        "[1,2,3]",
    ] {
        let got = classify(200, body);
        match got {
            SendOutcome::Failed(msg) => {
                assert!(msg.contains("无法解析"), "应报「无法解析」：{msg}");
                assert!(
                    !msg.contains(body) || body.is_empty(),
                    "失败文案不得回显响应体原文：{msg}"
                );
            }
            SendOutcome::Ok => panic!("响应体 {body:?} 不得判成 Ok"),
        }
    }
}

/// 非 2xx ⇒ `Failed`，**只带 status code**，body 一个字都不进文案。
#[test]
fn classify_failed_on_http_error_status() {
    let got = classify(502, "<html>bad gateway 693a91f6-secret</html>");
    match got {
        SendOutcome::Failed(msg) => {
            assert!(msg.contains("502"), "应带 status code：{msg}");
            assert!(!msg.contains("693a91f6"), "失败文案不得回显响应体：{msg}");
        }
        SendOutcome::Ok => panic!("HTTP 502 不得判成 Ok"),
    }
}

/// ★ `Failed` 的文案里**绝不含 webhook URL 或 key** —— 它会原样进
/// `t_wecom_daily_log.error_message` 与 HTTP 响应体。
#[test]
fn classify_failed_text_never_contains_url_or_key() {
    let bodies = [
        (200, r#"{"errcode":93000,"errmsg":"invalid webhook url"}"#),
        (200, r#"{"errcode":40001,"errmsg":"bad key"}"#),
        (200, "not-json"),
        (500, "boom"),
        (404, r#"{"errcode":93000,"errmsg":"nope"}"#),
    ];
    for (status, body) in bodies {
        let got = classify(status, body);
        if let SendOutcome::Failed(msg) = got {
            assert!(!msg.contains("qyapi.weixin.qq.com"), "{msg}");
            assert!(!msg.contains("693a"), "{msg}");
            assert!(!msg.contains("key="), "{msg}");
        }
    }
}

// ============================================================================
//  域隔离护栏
// ============================================================================

/// 域隔离护栏：把「wecom 不 import 其它域」从口头约定变成 CI 强制。
///
/// 探测器实现（剥注释、根段 + 域路径前缀匹配、元测试）见
/// [`crate::shared::domain_guard`]，本域只负责传参 + 域专属指引。
///
/// ⚠️ 本域**适用**这条护栏（与 `scheduled_task` / `dashboard` / `statistics` 同款）：
/// 跨域只读的 `t_wecom_daily_config` 引用检查是本域自己写的 SQL，不 import
/// `wecom_daily` 的任何符号。见 [`super`] 的模块 doc 末节。
#[test]
fn wecom_domain_depends_on_no_other_domain() {
    use std::path::Path;

    use crate::shared::domain_guard::assert_no_foreign_domain;

    assert_no_foreign_domain(
        "wecom",
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src/modules/wecom"),
        "需要别的域的数据时，正确做法是像 statistics / admin / dashboard / scheduled_task 那样\
         在本域 SQL 里只读聚合（本域唯一的跨域表引用是 repo.rs::count_daily_configs_referencing \
         查 t_wecom_daily_config 的一个 COUNT，SQL 真源就在本文件里），而不是 import 别人的\
         service / repo。",
    );
}
