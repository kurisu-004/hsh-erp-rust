//! wecom 域 service 层：校验 / 加密 / OCC / 通道解析 / 引用检查
//!
//! ## 事务边界（CLAUDE.md §1）
//! 常规读写在 handler：`pool.acquire()` → service 借 `&mut *conn` → drop conn。
//! **唯一开显式事务的写路径是 [`set_default_webhook`]**：它必须「先降旧默认、再升新默认」
//! 两条 UPDATE 严格有序，且第二步失败时第一步要一起回滚（否则库里会一个默认都没有）。
//! ⚠️ 事务**消不掉**「两次并发置默认到不同 webhook」那一支 —— 见
//! [`set_default_webhook`] 的 doc 与 `docs/api/wecom.md` §4.3。其余写端点各只做单行
//! INSERT / UPDATE，语句自身原子；开事务反而会让「唯一索引冲突」变成「整笔回滚」而丢掉
//! 诊断信息（那一支在 service 层捕 `is_unique_violation` 转成 `21807`，见
//! [`create_webhook`] / [`update_webhook`]）。
//!
//! ## 给内容域的三个公开符号
//!
//! [`resolve_target`] / [`target_is_available`] / [`send_probe`] 是 `wecom_daily` 域的
//! 唯一入口（方向是**内容域 → 通道域**，反向零依赖，见模块 doc 的护栏说明）。

use sqlx::{Acquire, PgConnection};

use crate::auth::rbac::CurrentUser;
use crate::infra::clock::now_naive;
use crate::infra::crypto;
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::wecom::dto::{
    CreateWebhookRequest, DeleteWebhookRequest, SetDefaultWebhookRequest, UpdateWebhookRequest,
    WEBHOOK_NAME_MAX_CHARS, WEBHOOK_URL_AAD, WEBHOOK_URL_MAX_CHARS, WebhookListOut, WebhookOut,
    mask_webhook_url, webhook_url_regex,
};
use crate::modules::wecom::repo::{self, NewWebhook, WebhookPatch};
use crate::modules::wecom::sender::{self, SendOutcome};
use crate::shared::error::{AppError, code};

// ============================================================================
// 读
// ============================================================================

/// `GET /webhooks`：列全部未软删 webhook（默认 webhook 排最前）。
///
/// ⚠️ **每行都要解密一次**才能算出 `url_masked`（表里没有掩码列，掩码是从明文派生的）。
/// 代价是「管理员手动维护的个位数行 × 一次 AES-GCM 解密」，可忽略。
///
/// ⚠️ **任一行解不开 ⇒ 整条列表返 `21801`（503）**，刻意**不**逐行降级成
/// `MASK_UNPARSEABLE` 那样的占位：解不开最常见的成因是「运维把 `MAIL_ACCOUNT_SECRET`
/// 换了」，那是一个必须当场看见的运维问题，逐行降级会让整个列表页看起来「都配好了、
/// 只是显示不出地址」，把问题一直藏到真发推送那天。登记在 `docs/api/wecom.md` §8.3。
pub async fn list_webhooks(conn: &mut PgConnection) -> Result<WebhookListOut, AppError> {
    let rows = repo::list_webhooks(conn).await?;
    let mut items = Vec::with_capacity(rows.len());
    for row in &rows {
        let plain = open_url(&row.encrypted_url, row.id)?;
        items.push(WebhookOut::from_row(row, &plain));
    }
    Ok(WebhookListOut { items })
}

// ============================================================================
// 写
// ============================================================================

/// `POST /webhooks`：新建 webhook（地址加密入库）。
///
/// ⚠️ **不设默认**（见 `repo::insert_webhook` 的 doc）。
pub async fn create_webhook(
    conn: &mut PgConnection,
    req: &CreateWebhookRequest,
    current: &CurrentUser,
    snowflake: &SnowflakeIdGenerator,
) -> Result<WebhookOut, AppError> {
    let name = validate_name(&req.name)?;
    let url = validate_webhook_url(&req.webhook_url)?;
    let encrypted = crypto::seal_with_aad(&url, WEBHOOK_URL_AAD.as_bytes())?;

    let id = snowflake.next_id();
    match repo::insert_webhook(
        conn,
        &NewWebhook {
            id,
            // ⚠️ clone 一次：`name` 还要留给下面 `name_duplicate` 分支的报错文案。
            // 报错用**校验并 trim 后**的这个值（不是 `req.name` 原文），与
            // `update_webhook` 同款 —— 两条端点的 `21807` 文案口径必须一致。
            name: name.clone(),
            encrypted_url: encrypted,
            operator: current.id,
            now: now_naive(),
        },
    )
    .await
    {
        Ok(true) => {}
        // ⚠️ INSERT 影响 0 行只在「有 BEFORE 触发器把行挡掉」时发生（当前 schema 没有），
        // 但它与「同名未软删行已存在」在现象上同源，统一按 `21807` 报，不放行成 500。
        Ok(false) => return Err(name_duplicate(&name)),
        Err(e) if is_unique_violation(&e) => return Err(name_duplicate(&name)),
        Err(e) => return Err(e.into()),
    }

    let row = repo::get_webhook(conn, id)
        .await?
        .ok_or_else(webhook_not_found)?;
    // ⚠️ 出参的 `url_masked` 用**刚校验过的那份明文**算，不必再解一次密。
    Ok(WebhookOut::from_row(&row, &url))
}

/// `POST /webhooks/{id}`：全量覆写（OCC）。
///
/// **地址的「沿用」语义**：body 的 `webhook_url` 缺省**或** trim 后为空 ⇒ 取回旧行原样
/// 写回。这条规则在**本层**翻译，repo 层因此完全不感知「空 = 沿用」（见
/// `repo::update_webhook` 的 doc）。
///
/// ⚠️ **非空地址走与新建同款的 `validate_webhook_url`**（长度 / 控制字符 / 正则），
/// **不是**直接封：改档是绕过「建档时那道闸」的旁路，只在 create 校验等于留了个后门
/// （一个不合规的地址会在这里进库，然后在 `/test` 或真推送时以 `21802` 失败）。
///
/// ⚠️ **改名撞同名未软删行 ⇒ `21807`**（与 create 同款）：本端点是**全量覆写**，`name`
/// 每次都参与写入 ⇒ 撞 `uk_t_wecom_webhook_name_alive` 与新建撞车是同一件事，
/// 若放行成 `AppError::Database` 就是「一句数据库错误、不带失败原因」的 500。
pub async fn update_webhook(
    conn: &mut PgConnection,
    id: i64,
    req: &UpdateWebhookRequest,
    current: &CurrentUser,
) -> Result<WebhookOut, AppError> {
    let row = repo::get_webhook(conn, id)
        .await?
        .ok_or_else(webhook_not_found)?;

    let name = validate_name(&req.name)?;

    let (encrypted, plain) = match req.webhook_url.as_deref().map(str::trim) {
        // ⚠️ 非空 ⇒ **全量过 `validate_webhook_url`**（与 create 同款）。
        Some(s) if !s.is_empty() => {
            let v = validate_webhook_url(s)?;
            (crypto::seal_with_aad(&v, WEBHOOK_URL_AAD.as_bytes())?, v)
        }
        // 空 ⇒ 沿用旧密文；掩码仍需一份明文，所以这里解一次。
        _ => {
            let plain = open_url(&row.encrypted_url, row.id)?;
            (row.encrypted_url.clone(), plain)
        }
    };

    let ok = match repo::update_webhook(
        conn,
        id,
        req.version,
        &WebhookPatch {
            // ⚠️ 这里 clone 一次：`name` 还要留给下面 `is_unique_violation` 分支的报错文案。
            name: name.clone(),
            encrypted_url: encrypted,
            operator: current.id,
            now: now_naive(),
        },
    )
    .await
    {
        Ok(ok) => ok,
        Err(e) if is_unique_violation(&e) => return Err(name_duplicate(&name)),
        Err(e) => return Err(e.into()),
    };
    if !ok {
        return Err(version_conflict());
    }

    let fresh = repo::get_webhook(conn, id)
        .await?
        .ok_or_else(webhook_not_found)?;
    Ok(WebhookOut::from_row(&fresh, &plain))
}

/// `POST /webhooks/{id}/delete`：软删。
///
/// **被每日总结配置引用 ⇒ `21808` 拒删**（先查引用、再删；两者不在同一事务 ⇒ 存在极窄
/// 的插入窗口，那个窗口里配置可能刚好被改成指向本 webhook，产出的是一个指向已软删行的
/// 配置，而推送时它会以 `21801` 暴露出来）。**有意不修** —— 为一个配置型实体引入跨表
/// 约束 / 分布式锁不划算，而故障面是「发不出推送 + 一条明确错误」，不是数据损坏。
pub async fn delete_webhook(
    conn: &mut PgConnection,
    id: i64,
    req: &DeleteWebhookRequest,
    current: &CurrentUser,
) -> Result<(), AppError> {
    let row = repo::get_webhook(conn, id)
        .await?
        .ok_or_else(webhook_not_found)?;

    let refs = repo::count_daily_configs_referencing(conn, id).await?;
    if refs > 0 {
        return Err(AppError::biz(
            code::BIZ_WECOM_WEBHOOK_IN_USE,
            format!(
                "群机器人「{}」仍被 {refs} 份「每日总结」配置选为推送通道，无法删除；\
                 请先把那些配置的「推送通道」改选或清空",
                row.name
            ),
        ));
    }

    let ok = repo::soft_delete_webhook(conn, id, req.version, current.id, now_naive()).await?;
    if !ok {
        return Err(version_conflict());
    }
    Ok(())
}

/// `POST /webhooks/{id}/set-default`：置默认（**本域唯一开事务的写路径**）。
///
/// ## 事务解决的是「失败不留半截状态」
///
/// `uk_t_wecom_webhook_one_default` 保证「至多一个未软删行为 `is_default`」，于是「先升
/// 新的、再降旧的」必然在第二条 UPDATE 上撞 23505 ⇒ 必须**先降后升**。而两条语句若各自
/// 自动提交，则「降已生效 + 升没生效」就是库的终态 —— 中途失败最常见的一支是升格那步
/// 自己失败（body 的 `version` 对不上 ⇒ 40901），后果是**一个默认 webhook 都没有**。
/// 同一事务里任何一步失败都整笔回滚 ⇒ 要么原默认还在、要么新默认已升上。
///
/// ## 事务**解决不了**的是「两个管理员同时对不同 webhook 点置默认」
///
/// 各自的事务都看不见对方未提交的结果 ⇒ 后到者的 [`repo::promote_to_default`] 撞 23505。
/// 这一支在 service 层捕下来转 `40901`（**不是** 500）：本次提交基于一份已过期的
/// 「谁是默认」快照，处置是刷新后重试（与 mail 域 `set-default` 同款处理）。
///
/// ⚠️ **不要**为了让这个窗口消失而去动那个 partial 唯一索引：它是「至多一个默认」的
/// **唯一**保障，去掉它会让「库里两个默认」变成可能，而那个状态没有任何代码能收敛回来。
///
/// ## ⚠️ `is_default` **不参与发送**
///
/// 发送只认 `t_wecom_daily_config.webhook_id` 指到哪一行；本列只用于**列表排序**与
/// 唯一索引。改它不会改变任何消息的落点（登记在 `docs/api/wecom.md` §4.3）。
pub async fn set_default_webhook(
    conn: &mut PgConnection,
    id: i64,
    req: &SetDefaultWebhookRequest,
    current: &CurrentUser,
) -> Result<WebhookOut, AppError> {
    // 先确认存在并区分「不存在」（404）于「版本对不上」（409）。
    let row = repo::get_webhook(conn, id)
        .await?
        .ok_or_else(webhook_not_found)?;
    let plain = open_url(&row.encrypted_url, row.id)?;

    let mut tx = conn.begin().await?;
    repo::clear_default(&mut tx).await?;
    let now = now_naive();
    let ok = match repo::promote_to_default(&mut tx, id, req.version, current.id, now).await {
        Ok(ok) => ok,
        // 并发的另一次 set-default 已升了别的 webhook ⇒ 本次快照过期。
        Err(e) if is_unique_violation(&e) => return Err(default_conflict()),
        Err(e) => return Err(e.into()),
    };
    if !ok {
        return Err(version_conflict());
    }
    tx.commit().await?;

    let fresh = repo::get_webhook(conn, id)
        .await?
        .ok_or_else(webhook_not_found)?;
    Ok(WebhookOut::from_row(&fresh, &plain))
}

// ============================================================================
// 供内容域（wecom_daily）调用的三个符号
// ============================================================================

/// 解析出来的推送目标：**明文 URL 只在调用栈里活着**。
///
/// ⚠️ **刻意不 derive `Debug`**，`url` 恒打成 `"***"` —— 明文里就是企业微信的 bearer
/// 凭证，本仓对含凭据的类型一律走手写 `Debug`（`mail_digest::mailer::ResolvedSender`
/// 同款手法）。
#[derive(Clone)]
pub struct ResolvedTarget {
    pub webhook_id: i64,
    /// webhook 名称（进 `t_wecom_daily_log.webhook_name` 快照）。
    pub name: String,
    /// 地址掩码（进 `t_wecom_daily_log.webhook_masked` 快照，审计时肉眼确认「这两次发的是
    /// 不是同一个群」）。
    pub masked: String,
    /// **明文地址**，只在调用栈里活着。
    pub url: String,
}

/// 手写 `Debug`：屏蔽 `url`（2026-10-18）。
impl std::fmt::Debug for ResolvedTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedTarget")
            .field("webhook_id", &self.webhook_id)
            .field("name", &self.name)
            .field("masked", &self.masked)
            // ⚠️ 永远输出 `***`
            .field("url", &"***")
            .finish()
    }
}

/// 把 `t_wecom_daily_config.webhook_id` 解析成一条可推送的 [`ResolvedTarget`]。
///
/// ## 四类失败**统一**映射到 `21801 BIZ_WECOM_NOT_CONFIGURED`（HTTP 503）
///
/// | 失败 | 判据 |
/// |---|---|
/// | 未选通道 | `webhook_id IS NULL` |
/// | 指向不存在 / 已软删的 webhook | `get_webhook` 返回 `None` |
/// | 库里有其它未知行状态 | `get_webhook` 抛错（透传为 500） |
/// | 地址解不开（base64 坏 / tag 失配 / **主密钥被换过**） | `crypto::open_with_aad` 失败 |
///
/// ⚠️ 四者**共用一个码**是刻意的（与 `mail_digest::service::resolve_sender` 同款）：
/// 调用方（HTTP handler / cron job）对这四类的可行动动作**完全相同** —— 都是「去把推送
/// 通道配好」。分码只会让前端多出四个分支去渲染同一句提示。真正的差异只写进 `message`
/// 文本与本函数的一行 `tracing::error!`。
///
/// ⚠️ **密文解不开在本域被转成 `21801` 而不是让 `crypto` 的 `21701` 直接冒出来**：
/// `21701` 的字面义是「邮箱发信通道不可用」，让推送域吐一个邮件域的码会让前端的错误文案
/// 串台（前端 CLAUDE.md 登记过「`21701` 与 `21801` 是两个域各自的通道码」）。
pub async fn resolve_target(
    conn: &mut PgConnection,
    webhook_id: Option<i64>,
) -> Result<ResolvedTarget, AppError> {
    let Some(id) = webhook_id else {
        return Err(no_target_err(
            "未选择推送通道：请在「定时任务 → 企业微信 → webhook 配置」里添加通道，并在「每日总结」配置中选定一个",
        ));
    };
    let row = repo::get_webhook(conn, id).await?.ok_or_else(|| {
        no_target_err(&format!(
            "推送通道（id={id}）不存在或已被删除：请在「每日总结」配置中重新选择一个"
        ))
    })?;
    let url = open_url(&row.encrypted_url, row.id)?;
    Ok(ResolvedTarget {
        webhook_id: row.id,
        name: row.name.clone(),
        masked: mask_webhook_url(&url),
        url,
    })
}

/// 「通道是否可用」：判据同 [`resolve_target`]，但**不返回明文**。
///
/// ## ⚠️ 本函数**会尝试解密**（与 `mail_digest::service::sender_is_available` 相反，
/// 那一个只做 DB 存在性检查）。选它的理由与代价，逐条写在这里：
///
/// - **选它的理由**：cron 注册闸问的是「到点**能不能真的发出去**」。密文解不开时注册
///   那个 job 必然每天产出一条 `FAILED` 流水并占掉官方的 20 条/分钟限频配额，而摘掉
///   job 是零成本的。
/// - **代价**：主密钥被换过这一类**运维问题**在配置页上会显示成
///   `webhook_configured = false`（=「通道不可用」），而管理员据此会去检查「是不是没配
///   通道」而不是去查 env。⚠️ 若内容域把这个布尔也用在配置出参上，**必须**在页面上把
///   「通道不可用」与「通道未选」分开写文案（已登记在 `docs/api/wecom.md` §8.3）。
/// - 反过来若选 mail 那套「只查存在性」，上面那条运维问题就完全不可见 —— 只在真推送
///   那一刻以 `21801` 暴露，而那时它已经连续失败了很多天。
pub async fn target_is_available(conn: &mut PgConnection, webhook_id: Option<i64>) -> bool {
    let Some(id) = webhook_id else {
        return false;
    };
    match repo::get_webhook(conn, id).await {
        Ok(Some(row)) => open_url(&row.encrypted_url, row.id).is_ok(),
        Ok(None) => false,
        Err(e) => {
            tracing::error!(error = %e, webhook_id = id, "wecom: 读推送通道行失败");
            false
        }
    }
}

/// `POST /webhooks/{id}/test`：解密地址 + 发一条 text 探针。
///
/// ⚠️ **不写任何日志表**（`t_wecom_daily_log` 一行都不动）：这是「管理员点一下看通不
/// 通的诊断动作」，不是一次推送。把它记进流水会让流水里混进与业务统计无关的噪音，而
/// 「今天到底推了几次」是内容域报表的口径。
pub async fn send_probe(conn: &mut PgConnection, id: i64) -> Result<(), AppError> {
    let row = repo::get_webhook(conn, id)
        .await?
        .ok_or_else(webhook_not_found)?;
    let url = open_url(&row.encrypted_url, row.id)?;
    match sender::send_text(&url, PROBE_TEXT).await {
        SendOutcome::Ok => Ok(()),
        // ⚠️ `Failed` 的文案由 sender 保证**不含 URL 与 key**（见该文件的安全纪律段）。
        SendOutcome::Failed(reason) => {
            tracing::warn!(
                error = %reason,
                webhook_id = id,
                "wecom: 测试连通性失败"
            );
            Err(AppError::biz(
                code::BIZ_WECOM_SEND_FAILED,
                format!("群机器人「{}」测试失败：{reason}", row.name),
            ))
        }
    }
}

/// 探针文案（固定一句，**不含任何业务数据**）。
const PROBE_TEXT: &str = "【汇森 ERP】webhook 通道测试消息：本条由「定时任务 → 企业微信」的「测试连通」按钮发出，可忽略。";

// ============================================================================
// 校验（写侧 21803 / 21804 / 21807）
// ============================================================================

/// `name` 校验：trim 后非空、≤50 **字**（不是字节）、拒控制字符。
///
/// ⚠️ `.chars().count()` 而不是 `.len()`：后者是字节数，一个 17 字的纯中文名会被算成
/// 51 字节、被误判为超长 —— 而中文命名恰恰是管理员最常用的（照 `scheduled_task` 的
/// `name_validation_uses_char_count_not_bytes`）。
///
/// 失败 ⇒ `21804`（配置组合非法）。⚠️ **重名不是 `21804` 而是 `21807`** —— 语义分层照
/// mail（`21704` 预设非法 / `21707` 重名）。
pub(crate) fn validate_name(raw: &str) -> Result<String, AppError> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(config_err("群机器人名称不能为空"));
    }
    if name.chars().any(char::is_control) {
        return Err(config_err("群机器人名称含控制字符（CR/LF 等，已拒绝）"));
    }
    let n = name.chars().count();
    if n > WEBHOOK_NAME_MAX_CHARS {
        return Err(config_err(&format!(
            "群机器人名称超长：{n} 字（上限 {WEBHOOK_NAME_MAX_CHARS}）"
        )));
    }
    Ok(name.to_string())
}

/// `webhook_url` 校验：trim、1..=500 字、拒控制字符、必须匹配官方地址形态。
///
/// ⚠️ **为什么卡得这么死（SSRF 面）**：本域会拿这个地址**真发 HTTP 请求**（探针与内容域
/// 推送都走）。放开 host 就等于让一条数据库记录变成任意地址的请求发起器；只放行
/// `https://qyapi.weixin.qq.com/…` 这一个官方端点，那才是「群机器人 webhook」的定义。
///
/// ⚠️ 正则与前端 `weComWebhookSchema.ts::WECOM_WEBHOOK_URL_RE` **逐字同一份**
/// （`dto::WEBHOOK_URL_REGEX` 就是它的转写）；放宽/收紧任何一侧都必须同一次改动一起改。
///
/// 失败一律 ⇒ `21803`（HTTP 400），四类原因（空 / 超长 / 控制字符 / 形态不符）**共用**
/// 一个码：它们对管理员是同一句「这个地址不对」。
pub(crate) fn validate_webhook_url(raw: &str) -> Result<String, AppError> {
    let url = raw.trim();
    if url.is_empty() {
        return Err(url_err("webhook 地址不能为空"));
    }
    if url.chars().any(char::is_control) {
        return Err(url_err("webhook 地址含控制字符（CR/LF 等，已拒绝）"));
    }
    let n = url.chars().count();
    if n > WEBHOOK_URL_MAX_CHARS {
        return Err(url_err(&format!(
            "webhook 地址超长：{n} 字（上限 {WEBHOOK_URL_MAX_CHARS}）"
        )));
    }
    if !webhook_url_regex().is_match(url) {
        // ⚠️ **不回显收到的地址**：它可能是一个真的地址（管理员从别处粘来的），把它拼进
        // 错误文案就等于把凭证写进日志与 HTTP 响应体。
        return Err(url_err(
            "webhook 地址形态非法：必须是 https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=…",
        ));
    }
    Ok(url.to_string())
}

// ============================================================================
// 错误构造与共用小工具
// ============================================================================

/// 解密一条 `encrypted_url`，失败时补一行点名「主密钥可能被换过」的 error 日志。
///
/// ⚠️ 这行 `tracing::error!` 是运维定位的**唯一线索**：解密失败的成因有四类
/// （base64 坏 / tag 失配 / AAD 不匹配 / 主密钥被换），而其中「主密钥被换」最常见也最难
/// 从业务现象反推出来（照 `mail_digest::service::resolve_sender` 同款做法）。
///
/// ⚠️ 返回的错误码由调用点决定（列表 / 覆写 / 解析通道都转成 `21801`，`/test` 转 `21802`
/// —— 见各调用点），本函数只保证**日志**与「不泄漏地址」两件事。
fn open_url(sealed: &str, webhook_id: i64) -> Result<String, AppError> {
    crypto::open_with_aad(sealed, WEBHOOK_URL_AAD.as_bytes()).map_err(|e| {
        tracing::error!(
            error = %e,
            webhook_id,
            "wecom: webhook 地址解密失败 —— 疑似 env MAIL_ACCOUNT_SECRET 已被更换（换密钥会让全部存量地址永久解不开）"
        );
        e
    })
}

fn config_err(msg: &str) -> AppError {
    AppError::biz(code::BIZ_WECOM_CONFIG_INVALID, msg.to_string())
}

fn url_err(msg: &str) -> AppError {
    AppError::biz(code::BIZ_WECOM_WEBHOOK_URL_INVALID, msg.to_string())
}

fn webhook_not_found() -> AppError {
    AppError::biz(
        code::BIZ_WECOM_WEBHOOK_NOT_FOUND,
        "群机器人不存在（或已被删除）：请刷新 webhook 列表后重试",
    )
}

fn name_duplicate(name: &str) -> AppError {
    AppError::biz(
        code::BIZ_WECOM_WEBHOOK_NAME_DUPLICATE,
        format!("群机器人名称「{name}」已存在（未删除的 webhook 之间不允许重名）"),
    )
}

/// OCC 冲突文案与 mail 域同款（同一个 `40901`，同一件事）。
fn version_conflict() -> AppError {
    AppError::biz(
        code::VERSION_CONFLICT,
        "群机器人已被其他人修改（或删除），请刷新后重试",
    )
}

/// `set-default` 与**并发的另一次 set-default** 撞「至多一个默认」时的文案。
///
/// ⚠️ **用 `40901` 而不是复用 `version_conflict()` 的文案、也不是新开业务码**：本次提交
/// 是基于一份已过期的「谁是默认」快照。复用 `version_conflict()` 会把「别人改了**这个**
/// webhook」与「别人改了**全局的默认**」混成一句话，管理员照提示去刷新单个编辑框会看不出
/// 任何变化。
fn default_conflict() -> AppError {
    AppError::biz(
        code::VERSION_CONFLICT,
        "另一个管理员刚刚改动了默认群机器人，请刷新列表后重试",
    )
}

fn no_target_err(msg: &str) -> AppError {
    AppError::biz(code::BIZ_WECOM_NOT_CONFIGURED, msg.to_string())
}

/// PG 唯一约束冲突（SQLSTATE 23505）。
fn is_unique_violation(e: &sqlx::Error) -> bool {
    matches!(
        e,
        sqlx::Error::Database(db) if db.code().as_deref() == Some("23505")
    )
}
