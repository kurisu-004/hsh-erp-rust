//! wecom_daily 域「`/send` 必须在请求级超时白名单里」的**集成侧**护栏（2026-10-19）
//!
//! ## 这个文件防的是什么
//!
//! `POST /api/v2/cron/wecom-daily/send` 在**同一个请求内**串着做「取数（恒 ≤ 6 条
//! SQL）+ 最多 `max_chunks`（上界 20）次**顺序 HTTP**」。单片超时 5s
//! （`wecom::sender::HTTP_TIMEOUT_MS`）⇒ 最坏 ≈ 100s。而
//! `middleware::timeout_middleware` 是 `tokio::time::timeout` —— **到点把 handler 的
//! future 整个 drop 掉**，于是：
//!
//! ```text
//! 前 k 片已经推到群里 → 第 k+1 片卡住 → 30s 到点 → 整个 future 被 drop
//!      ⇒ 已发出的内容留在群里、已成功的片数没落库、`GET /logs` 里一行都没有
//! ```
//!
//! 这**静默破坏**了 `docs/api/wecom-daily.md` 与验收点 19 的不变量「任一片失败 ⇒
//! 整条 FAILED + `chunk_sent` 记录已成功数」，而且失败发生在 `tokio` 层、**不经过
//! 任何 `AppError`** ⇒ 运维在流水表里查不到痕迹。这就是本域的 `/send` 必须进
//! `is_long_timeout_path` 长档白名单的原因。
//!
//! ## 为什么是「纯函数断言」而不是一个真跑 ≈100s 的集成测试
//!
//! 真跑一次最坏路径要 100s 起步（还要一个真不可达的 webhook + 一个能把响应体撑住的
//! 代理），不可测。而白名单判定本身是**纯函数**（`is_long_timeout_path` /
//! `select_timeout` 都不碰 IO）⇒ 直接喂字面量断言即可。
//! ⚠️ lib 侧已有同款单测（`src/middleware/timeout.rs::tests`）；**两处都要有**：
//! lib 单测守判定函数本身，本文件守「**本域的这条路由名**在白名单里」这条跨域不变量
//! —— 两者的失效模式不同（改坏判定 vs. 改坏路由名），少了任一条都是零测试信号。
//!
//! ## 变异验证
//!
//! 本文件**刻意会红**：把 `is_long_timeout_path` 里 `["cron","wecom-daily","send"]`
//! 那一行删掉（2026-10-19 落地时实测过，见回执），或把路由名从 `send` 改成别的，
//! `wecom_daily_send_path_must_take_the_long_timeout_tier` 即失败。

use hsh_erp_rust::middleware::timeout::{is_long_timeout_path, select_timeout};

/// 本域两条投递相关的路由名（真源是 `src/modules/wecom_daily/handler::ROUTES`）。
const SEND_PATH: &str = "/api/v2/cron/wecom-daily/send";
const PREVIEW_PATH: &str = "/api/v2/cron/wecom-daily/preview";

/// 生产形态（带 `/api/v2` 前缀）必须命中长档。
#[test]
fn wecom_daily_send_path_must_take_the_long_timeout_tier() {
    assert!(
        is_long_timeout_path(SEND_PATH),
        "`{SEND_PATH}` 必须在长档白名单里：拿 30s 通用档时，超时会把已推到群里的分片 \
         变成「零流水」（future 被整个 drop，GET /logs 里查不到任何痕迹）"
    );
}

/// 集成测试形态（`test_app` 直挂 `v2_router`，路径不带 `/api/v2`）同样命中 ——
/// 两种形态必须同时成立，否则生产与测试的分档会不一致。
#[test]
fn wecom_daily_send_path_is_recognized_without_api_v2_prefix() {
    let bare = "/cron/wecom-daily/send";
    assert!(
        is_long_timeout_path(bare),
        "无 `/api/v2` 前缀的形态也必须命中长档：{bare}"
    );
}

/// 分档结果层面再钉一道：长档取到的秒数必须**严格大于**通用档。
///
/// ⚠️ 这一条比「判定为真」多一层：它同时抓住「白名单还在但两个 env 被配成同一个值」
/// 这类退化（`PRINT_REQUEST_TIMEOUT_SECONDS` 被误设成 30 / `REQUEST_TIMEOUT_SECONDS`
/// 被误设成 660 时，判定为真但分档等于没分）。
#[test]
fn long_tier_is_strictly_greater_than_the_default_tier() {
    let long = select_timeout(SEND_PATH, 30, 660);
    let default = select_timeout("/api/v2/health", 30, 660);
    assert_eq!(long, std::time::Duration::from_secs(660));
    assert_eq!(default, std::time::Duration::from_secs(30));
    assert!(
        long > default,
        "长档必须严格大于通用档，否则这条白名单等于没加：long={long:?} default={default:?}"
    );
}

/// ⚠️ `/preview` **必须不命中**长档：它只渲染、不投递（恒 ≤ 6 条 SQL），30s 绰绰有余。
///
/// 把它捞进长档不会出错，但会让一个真卡住的预览迟迟不被兜底挡住 —— 白名单越宽，
/// 「慢请求不被兜底」这个保证越稀释。
#[test]
fn wecom_daily_preview_path_stays_on_the_default_tier() {
    assert!(
        !is_long_timeout_path(PREVIEW_PATH),
        "只渲染不投递的 `{PREVIEW_PATH}` 不该命中长档"
    );
    assert_eq!(
        select_timeout(PREVIEW_PATH, 30, 660),
        std::time::Duration::from_secs(30)
    );
}

/// 反向：家族里**名字相近但不是长档**的路径一律不命中（否则普通端点白拿 660s）。
#[test]
fn wecom_daily_lookalike_paths_are_rejected() {
    for p in [
        "/api/v2/cron/wecom-daily/sendx",
        "/api/v2/cron/wecom/send",
        "/api/v2/cron/wecom-daily/send/extra",
        // 邮件那条**刻意不对称**（单次 SMTP 往返，秒级），见 `is_long_timeout_path` 的 doc
        "/api/v2/cron/mail-digest/send",
    ] {
        assert!(!is_long_timeout_path(p), "不应命中长档：{p}");
    }
}
