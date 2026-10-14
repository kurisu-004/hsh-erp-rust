//! wecom 域 DTO：加密 AAD 常量 + 地址校验 + 入参请求体 + 出参 VO
//!
//! ## 为什么没有单独的 vo.rs
//!
//! 出参 VO（[`WebhookOut`] / [`WebhookListOut`]）也放在本文件：本域只有一张表、
//! 两个出参结构，入参四个。拆一个 `vo.rs` 只会让读代码的人多一次「该 import 哪个
//! 路径」的判断，而那正是 CLAUDE.md「`docs/api/` 目录约定」第 2 条要避免的事
//! （照 `scheduled_task::dto` 的同款理由）。
//!
//! ## 明文永不回显：靠「出参是手写挑字段」而不是 `#[serde(skip)]`
//!
//! [`WebhookOut`] **不 derive 任何自动字段映射**，字段逐个从行里挑。于是「不小心把
//! `encrypted_url` 加进出参」不是一个「忘了加 skip」的错误，而是一个必须显式写出来
//! 才能发生的动作。

use serde::{Deserialize, Serialize};

use crate::modules::wecom::model::TWeComWebhookRow;

// ============================================================================
// 常量：加密 AAD / 长度上限 / 地址正则
// ============================================================================

/// AEAD 的附加认证数据（AAD）。**封与解两侧必须是同一个常量**。
///
/// ⚠️ **刻意与 `scheduled_task` 的 `t_mail_account.secret.v1` 不是同一个字面量**：
/// 两者共用同一把主密钥（env `MAIL_ACCOUNT_SECRET`），但 AAD 不同 ⇒ 两类密文**互不
/// 串扰**（AEAD 把 AAD 纳入认证：拿 webhook 的密文去冒充 SMTP 授权码会直接解不开）。
/// 改这一个字面量 = 本表存量密文全部解不开（同换主密钥），故它带 `v1` 后缀。
pub const WEBHOOK_URL_AAD: &str = "t_wecom_webhook.url.v1";

/// `name` 长度上限（`varchar(50)`）。
///
/// ⚠️ **按 `chars().count()` 判，不是字节数** —— 中文名 50 字 = 150 字节，按字节判会把
/// 管理员最常用的中文命名整片误拒。与前端的 `WECOM_WEBHOOK_NAME_MAX` 逐字一致。
pub const WEBHOOK_NAME_MAX_CHARS: usize = 50;

/// `webhook_url` 长度上限（列是 `text`，但无限长等于给 DB 塞垃圾）。
///
/// 与前端的 `WECOM_WEBHOOK_URL_MAX` 逐字一致。
pub const WEBHOOK_URL_MAX_CHARS: usize = 500;

/// 企业微信群机器人 webhook 地址的**唯一合法形态**。
///
/// ```text
/// https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=<uuid>
/// ```
///
/// ⚠️ **必须与前端 `weComWebhookSchema.ts` 的 `WECOM_WEBHOOK_URL_RE` 逐字一致**：
/// 两边各写一份、没有编译期约束，放宽或收紧任何一侧都会让「后端放行 / 前端拒收」
/// 或反过来（表现为管理员看着一个正确的地址被拒，却不知道该改哪）。
/// ⚠️ 将来若要放宽（例如支持新域名或带额外 query 段），**两边必须同一次改动一起改**。
///
/// 为什么卡得这么死（SSRF 面）：本域会拿这个 URL **真发 HTTP 请求**。放开 host 就等于
/// 让一条数据库记录变成任意地址的请求发起器。
pub const WEBHOOK_URL_REGEX: &str =
    r"^https://qyapi\.weixin\.qq\.com/cgi-bin/webhook/send\?key=\S+$";

/// 编译一次的地址正则（`validate_webhook_url` 每次调用都命中它，故用 `LazyLock`
/// 避免每次重建，见 `mail_digest::repo` 的 `LazyLock` 同款取舍）。
pub(crate) fn webhook_url_regex() -> &'static regex::Regex {
    static RE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(WEBHOOK_URL_REGEX).expect("常量正则必合法"));
    &RE
}

// ============================================================================
// 掩码
// ============================================================================

/// 地址形态无法识别（库里被手工改过 / 将来加了别的 query 段）时的**整段打码**占位。
///
/// ⚠️ 刻意**不是**「把原文打出来」也不是「按固定前缀拼一个像样的地址」—— 前者在库里
/// 被改坏时会把不可信内容送进前端，后者会让人以为地址是对的。
pub const MASK_UNPARSEABLE: &str = "***";

/// 把 webhook URL 掩成 `…?key=<前 4>***<后 4>`。
///
/// ```text
/// 输入 https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=693a91f6-7xxx-4bc4-97a0-0ec2sifa5aaa
/// 输出 https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=693a***faaa
/// ```
///
/// ## 为什么掩码长度固定取 4+4
///
/// 管理员在页面上需要确认的是「我填的是不是这一个群」，而真实 key 都是 uuid（36 字符）
/// ⇒ 前后各 4 位足以分辨，且**永远还原不出可用凭证**（少 28 个字符）。
///
/// ## ⚠️ 短 key 必须降级，**不许 panic 也不许原样回显**
///
/// key 短于 9 个字符时「前 4 + 后 4」会重叠 ⇒ 等于把整个 key 打出来。因此那一条统一
/// 降级成固定 8 个星号（**不按真实长度**输出，避免顺手泄露长度信息）。
/// 真实的 `t_wecom_webhook.encrypted_url` 解出来永远过得了上面那道正则（≥ 1 个非空白
/// 字符），但库里可能被手工改坏，所以这条降级路径**必须有**而不是「理论上走不到」。
///
/// ## 按 char 切而不是按字节切
///
/// 同 `validate_name` 的理由：`.len()` 与 `[..4]` 的字节切片在多字节字符上会 panic 或切出
/// 半个字符。这里先收成 `Vec<char>` 再切，任何输入都不会 panic。
pub fn mask_webhook_url(url: &str) -> String {
    const QUERY_PREFIX: &str = "?key=";
    let Some(at) = url.find(QUERY_PREFIX) else {
        return MASK_UNPARSEABLE.to_string();
    };
    let prefix = &url[..at + QUERY_PREFIX.len()];
    let key = &url[at + QUERY_PREFIX.len()..];
    // ⚠️ 尾部若还有别的 query 段（当前正则不允许，但库里可能是手工改过的形态）一并丢掉，
    // 否则那一段会原样进 `t_wecom_daily_log.webhook_masked`。
    let key = key.split('&').next().unwrap_or(key);

    let chars: Vec<char> = key.chars().collect();
    if chars.len() < 9 {
        return format!("{prefix}********");
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{prefix}{head}***{tail}")
}

// ============================================================================
// 入参请求体
// ============================================================================

/// `POST /api/v2/cron/wecom/webhooks` 请求体。
///
/// ⚠️ **无 `version`**：这是新建，没有「别人先改过」这回事（CLAUDE.md §8 的 OCC
/// 只适用于已存在的行）。
///
/// ⚠️ **刻意不 derive `Debug`**：`webhook_url` 是**明文** bearer 凭证（比 repo 两个结构体
/// 手上的密文更危险 —— 密文还要 base64 解一道），本仓对含凭据的类型一律走「不实现
/// `Debug`」或「手写屏蔽版」，不靠「调用方记得别打印」这条约定。
/// 守卫：`tests::write_params_must_not_derive_debug`。
#[derive(Clone, Deserialize)]
pub struct CreateWebhookRequest {
    /// 自定义名称（管理端展示用）。≤50 **字**。
    pub name: String,
    /// 群机器人 webhook 地址。**必填非空**；解密要用它，不存在「先建档、以后再补地址」。
    pub webhook_url: String,
}

/// `POST /api/v2/cron/wecom/webhooks/{id}` 请求体：**全量覆写**。
///
/// ⚠️ `version` **必填、无 `#[serde(default)]`**（CLAUDE.md §8 硬约定）：缺失时 axum
/// `Json` 提取器直接返 **HTTP 422 纯文本**，不是业务信封。
///
/// ⚠️ **同样刻意不 derive `Debug`**（`webhook_url` 是明文凭证，与
/// [`CreateWebhookRequest`] 同一理由）。
#[derive(Clone, Deserialize)]
pub struct UpdateWebhookRequest {
    /// 乐观锁锚点。0 行命中 → `40901 VERSION_CONFLICT`。
    pub version: i32,
    pub name: String,
    /// webhook 地址。**缺省或 trim 后为空 ⇒ 沿用旧密文**（不覆写、不报错）。
    ///
    /// 之所以是 `Option<String>` 而不是裸 `String`：管理员改个名称时前端拿不到地址明文
    /// （服务端从不回显），强制必填等于逼前端每次把「不知道的东西」原样传回来。
    /// ⚠️ 用裸 `String` 的话**省略键会 422**，而「省略 ⇒ 沿用」正是前端编辑弹窗的
    /// 既定交互。
    #[serde(default)]
    pub webhook_url: Option<String>,
}

/// `POST /api/v2/cron/wecom/webhooks/{id}/delete` 请求体。
///
/// ⚠️ **只有 `version`**：本表带 `version` 列，故按 CLAUDE.md §8「所有对带 `version`
/// 列的表的写端点，body 必须显式收客户端 `version` 且必填」，软删也不例外（这正是
/// 「读-再-比式隐式 OCC」被明令禁止的场景：两人同时打开同一个编辑页，一人改了名、
/// 一人删了行，后者会静默成功）。
#[derive(Debug, Clone, Deserialize)]
pub struct DeleteWebhookRequest {
    pub version: i32,
}

/// `POST /api/v2/cron/wecom/webhooks/{id}/set-default` 请求体。
///
/// 与 [`DeleteWebhookRequest`] 同款：目标行的 `version` 必填。
#[derive(Debug, Clone, Deserialize)]
pub struct SetDefaultWebhookRequest {
    pub version: i32,
}

// ============================================================================
// 出参
// ============================================================================

/// `GET /api/v2/cron/wecom/webhooks` 的一个群机器人 webhook。
///
/// ⚠️ **没有 `encrypted_url`、没有 `url_configured`**：
/// 建 webhook 时地址**必填** ⇒ 「是否配了地址」恒为 true，是个死字段；而死字段会让人
/// 误以为「地址可以是空的」，进而写出「先建档、地址以后再补」的流程。
/// 密文不回显，只回显 [`WebhookOut::url_masked`]。
#[derive(Debug, Clone, Serialize)]
pub struct WebhookOut {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub id: i64,
    pub version: i32,
    /// 自定义名称（管理端展示用）。
    pub name: String,
    /// 地址掩码（`…?key=<前4>***<后4>`）。**明文 URL 一个字都不回显**。
    pub url_masked: String,
    pub is_default: bool,
    /// naive timestamp（Asia/Shanghai），格式 `YYYY-MM-DD HH:MM:SS`。
    pub created_at: String,
    pub updated_at: String,
}

impl WebhookOut {
    /// 行 + 明文地址 → 出参。**逐字段挑**，是「密文不回显」的执行点（见文件头 doc）。
    ///
    /// ⚠️ `url_plain` 由调用方**当场解密**后传入，掩码在本函数里算 —— 出参结构因此
    /// 永远拿不到密文本体的支配权（加字段时它也不会顺手把 `encrypted_url` 带出去）。
    pub fn from_row(row: &TWeComWebhookRow, url_plain: &str) -> Self {
        Self {
            id: row.id,
            version: row.version,
            name: row.name.clone(),
            url_masked: mask_webhook_url(url_plain),
            is_default: row.is_default,
            created_at: format_ts(row.created_at),
            updated_at: format_ts(row.updated_at),
        }
    }
}

/// `GET /api/v2/cron/wecom/webhooks` 响应体。
#[derive(Debug, Clone, Serialize)]
pub struct WebhookListOut {
    pub items: Vec<WebhookOut>,
}

fn format_ts(ts: chrono::NaiveDateTime) -> String {
    ts.format("%Y-%m-%d %H:%M:%S").to_string()
}
