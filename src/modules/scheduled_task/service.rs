//! scheduled_task 域 service 层：校验 / 加密 / OCC / 默认邮箱的唯一性编排
//!
//! ## 事务边界（CLAUDE.md §1）
//! 常规读写在 handler：`pool.acquire()` → service 借 `&mut *conn` → drop conn。
//! **唯一开显式事务的写路径是 [`set_default_account`]**：它必须「先降旧默认、
//! 再升新默认」两条 UPDATE 严格有序，且第二步失败时第一步要一起回滚（否则库里
//! 会一个默认都没有）。⚠️ 事务**消不掉**「两次并发置默认到不同邮箱」那一支 ——
//! 见 [`set_default_account`] 的 doc 与 `docs/api/mail.md` §4.3。其余写端点各只做
//! 单行 INSERT / UPDATE，语句自身原子，开事务反而会让「唯一索引冲突」变成「整笔
//! 回滚」而丢掉诊断信息（那一支改在 service 层捕 `is_unique_violation` 转成
//! `21707`，见 [`create_account`] / [`update_account`]）。

use sqlx::{Acquire, PgConnection};

use crate::auth::rbac::CurrentUser;
use crate::infra::clock::now_naive;
use crate::infra::crypto;
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::scheduled_task::dto::{
    CreateAccountRequest, DeleteAccountRequest, MailAccountListOut, MailAccountOut,
    MailAccountPreset, MailMetaOut, PresetOption, SECRET_MAX_CHARS, SetDefaultAccountRequest,
    UpdateAccountRequest,
};
use crate::modules::scheduled_task::repo::{self, AccountPatch, NewAccount};
use crate::shared::error::{AppError, code};

// ============================================================================
// 读
// ============================================================================

/// `GET /meta`：SMTP 预设表（**纯函数**，不查库）。
pub fn meta() -> MailMetaOut {
    MailMetaOut {
        presets: MailAccountPreset::ALL
            .iter()
            .map(|p| PresetOption {
                value: p.as_str().to_string(),
                label: p.label().to_string(),
                host: p.host().to_string(),
                port: p.port(),
                tls_mode: p.tls_mode().to_string(),
            })
            .collect(),
    }
}

/// `GET /accounts`：列全部未软删邮箱（默认邮箱排最前）。
pub async fn list_accounts(conn: &mut PgConnection) -> Result<MailAccountListOut, AppError> {
    let rows = repo::list_accounts(conn).await?;
    Ok(MailAccountListOut {
        items: rows.iter().map(MailAccountOut::from_row).collect(),
    })
}

// ============================================================================
// 写
// ============================================================================

/// `POST /accounts`：新建邮箱（授权码加密入库）。
///
/// ⚠️ **不设默认**（见 `repo::insert_account` 的 doc）：建邮箱与「设为默认发信
/// 通道」是两种意图，由两条端点分别承担。
pub async fn create_account(
    conn: &mut PgConnection,
    req: &CreateAccountRequest,
    current: &CurrentUser,
    snowflake: &SnowflakeIdGenerator,
) -> Result<MailAccountOut, AppError> {
    let name = validate_name(&req.name)?;
    let preset = MailAccountPreset::parse(&req.preset)?;
    let account = validate_account(&req.account)?;
    let secret = validate_secret(&req.secret)?;
    let encrypted = crypto::seal(secret.as_str())?;

    let id = snowflake.next_id();
    let now = now_naive();
    match repo::insert_account(
        conn,
        &NewAccount {
            id,
            // ⚠️ clone 一次：`name` 还要留给下面两个 `name_duplicate` 分支的报错文案。
            // 报错用**校验并 trim 后**的这个值（不是 `req.name` 原文），与 `update_account`
            // 同款 —— 两条端点的 `21707` 文案口径必须一致。
            name: name.clone(),
            preset: preset.as_str().to_string(),
            account,
            encrypted_secret: encrypted,
            operator: current.id,
            now,
        },
    )
    .await
    {
        Ok(true) => {}
        Ok(false) => return Err(name_duplicate(&name)),
        Err(e) if is_unique_violation(&e) => return Err(name_duplicate(&name)),
        Err(e) => return Err(e.into()),
    }

    let row = repo::get_account(conn, id)
        .await?
        .ok_or_else(account_not_found)?;
    Ok(MailAccountOut::from_row(&row))
}

/// `POST /accounts/{id}`：全量覆写（OCC）。
///
/// **授权码的「沿用」语义**：body 的 `secret` 缺省**或** trim 后为空 ⇒
/// 取回旧行原样写回（`None`）。这条规则在**本层**翻译，repo 层因此完全不感知
/// 「空 = 沿用」（见 `repo::update_account` 的 doc）。
///
/// ⚠️ **非空 `secret` 走与新建同款的 `validate_secret`**（控制字符 / ≤200 字），**不是**
/// 直接封：改档是绕过「建档时那道闸」的旁路，只在 create 校验等于留了个后门（控制字符会
/// 进 SMTP `AUTH` 命令行）。
///
/// ⚠️ **改名撞同名未软删行 ⇒ `21707`**（与 create 同款，见本函数内 `is_unique_violation`
/// 分支）：本端点是**全量覆写**，`name` 每次都参与写入 ⇒ 撞 `uk_t_mail_account_name_alive`
/// 与新建撞车是同一件事，若放行成 `AppError::Database` 就是「一句数据库错误、不带失败原因」
/// 的 500（`docs/api/mail.md` §2.4 承诺的是 21707）。
pub async fn update_account(
    conn: &mut PgConnection,
    id: i64,
    req: &UpdateAccountRequest,
    current: &CurrentUser,
) -> Result<MailAccountOut, AppError> {
    let row = repo::get_account(conn, id)
        .await?
        .ok_or_else(account_not_found)?;

    let name = validate_name(&req.name)?;
    let preset = MailAccountPreset::parse(&req.preset)?;
    let account = validate_account(&req.account)?;

    let encrypted = match req.secret.as_deref().map(str::trim) {
        // ⚠️ 非空 ⇒ **全量过 `validate_secret`**（控制字符 / ≤200 字），与 create 同款。
        Some(s) if !s.is_empty() => crypto::seal(&validate_secret(s)?)?,
        _ => row.encrypted_secret.clone(),
    };

    let now = now_naive();
    let ok = match repo::update_account(
        conn,
        id,
        req.version,
        &AccountPatch {
            // ⚠️ 这里 clone 一次：`name` 还要留给下面 `is_unique_violation` 分支的报错文案
            // （用**校验并 trim 后**的那个值，而不是 `req.name` 原文）。
            name: name.clone(),
            preset: preset.as_str().to_string(),
            account,
            encrypted_secret: encrypted,
            operator: current.id,
            now,
        },
    )
    .await
    {
        Ok(ok) => ok,
        // 改名撞 `uk_t_mail_account_name_alive` ⇒ 业务唯一键冲突，**不是** OCC。
        Err(e) if is_unique_violation(&e) => return Err(name_duplicate(&name)),
        Err(e) => return Err(e.into()),
    };
    if !ok {
        return Err(version_conflict());
    }

    let fresh = repo::get_account(conn, id)
        .await?
        .ok_or_else(account_not_found)?;
    Ok(MailAccountOut::from_row(&fresh))
}

/// `POST /accounts/{id}/delete`：软删。
///
/// **被 mail_digest 配置引用 ⇒ `21708` 拒删**（先查引用、再删；两者在同一事务外
/// 的两条语句上，故「查完到删之间」存在极窄的插入窗口 —— 那个窗口里配置可能刚好
/// 被改成指向本邮箱，产出的是一个指向已软删行的配置，而发信时它会以 `21701`
/// 暴露出来。这是有意的取舍：为一个配置型实体引入跨表约束/分布式锁不划算，
/// 而故障面是「发不出信 + 一条明确错误」，不是数据损坏。
pub async fn delete_account(
    conn: &mut PgConnection,
    id: i64,
    req: &DeleteAccountRequest,
    current: &CurrentUser,
) -> Result<(), AppError> {
    let row = repo::get_account(conn, id)
        .await?
        .ok_or_else(account_not_found)?;

    let refs = repo::count_digest_configs_referencing(conn, id).await?;
    if refs > 0 {
        return Err(AppError::biz(
            code::BIZ_MAIL_ACCOUNT_IN_USE,
            format!(
                "邮箱「{}」仍被 {refs} 份「交期汇总」配置选为发信邮箱，无法删除；\
                 请先把那些配置的「发信邮箱」改选或清空",
                row.name
            ),
        ));
    }

    let ok = repo::soft_delete_account(conn, id, req.version, current.id, now_naive()).await?;
    if !ok {
        return Err(version_conflict());
    }
    Ok(())
}

/// `POST /accounts/{id}/set-default`：置默认（**本域唯一开事务的写路径**）。
///
/// ## 事务解决的是「失败不留半截状态」
///
/// `uk_t_mail_account_one_default` 保证「至多一个未软删行为 `is_default`」，于是「先升
/// 新的、再降旧的」必然在第二条 UPDATE 上撞 23505 ⇒ 必须**先降后升**。而两条语句若各自
/// 自动提交，则**「降已生效 + 升没生效」就是库的终态** —— 中途失败最常见的一支是升格那
/// 步自己失败（body 的 `version` 对不上 ⇒ 40901），后果是**一个默认邮箱都没有**。
/// 同一事务里任何一步失败都整笔回滚 ⇒ 要么原默认还在、要么新默认已升上。
///
/// ## 事务**解决不了**的是「两个管理员同时对不同邮箱点置默认」
///
/// 各自的事务都看不见对方未提交的结果：两边都「先降（降的不是对方要升的那行）、
/// 再升」，于是后到者的 [`repo::promote_to_default`] 撞 23505。这一支在 service 层
/// 捕下来转 `40901`（**不是** 500）：本次提交基于一份已过期的「谁是默认」快照，
/// 处置是刷新后重试 —— 与 CLAUDE.md 对 `mail_digest` 配置单例并发首次初始化的
/// 同款处理（23505 → OCC 冲突）。
///
/// ⚠️ **不要**为了让这个窗口消失而去动那个 partial 唯一索引：它是「至多一个默认」的
/// **唯一**保障，去掉它会让「库里两个默认」变成可能，而那个状态没有任何代码能收敛回来。
///
/// ## 为什么先降的是「全部」而不是「当前那一个」
///
/// [`repo::clear_default`] 的谓词是 `WHERE is_default = true`，不是
/// `WHERE id = <上一个默认>`。后者要求「先读出上一个默认是谁」—— 那次读取与本次
/// 写入之间同样有窗口，而前者不需要读。副作用：如果库里本来就没有默认邮箱
/// （正常状态，见 `repo::insert_account` 的 doc），第一条 UPDATE 影响 0 行，
/// 仍然正确。
pub async fn set_default_account(
    conn: &mut PgConnection,
    id: i64,
    req: &SetDefaultAccountRequest,
    current: &CurrentUser,
) -> Result<MailAccountOut, AppError> {
    // 先确认存在并区分「不存在」（404）于「版本对不上」（409）。
    repo::get_account(conn, id)
        .await?
        .ok_or_else(account_not_found)?;

    let mut tx = conn.begin().await?;
    repo::clear_default(&mut tx).await?;
    let now = now_naive();
    let ok = match repo::promote_to_default(&mut tx, id, req.version, current.id, now).await {
        Ok(ok) => ok,
        // 并发的另一次 set-default 已升了别的邮箱 ⇒ 本次快照过期。
        Err(e) if is_unique_violation(&e) => return Err(default_conflict()),
        Err(e) => return Err(e.into()),
    };
    if !ok {
        return Err(version_conflict());
    }
    tx.commit().await?;

    let fresh = repo::get_account(conn, id)
        .await?
        .ok_or_else(account_not_found)?;
    Ok(MailAccountOut::from_row(&fresh))
}

// ============================================================================
// 校验（写侧 21704 / 21706 / 21707）
// ============================================================================

/// `name` 校验：trim 后非空、≤50 **字**（不是字节）。
///
/// ⚠️ `.chars().count()` 而不是 `.len()`：后者是字节数，一个 20 字的纯中文名
/// 会被算成 60 字节、被误判为超长。
pub(crate) fn validate_name(raw: &str) -> Result<String, AppError> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(config_err("邮箱名称不能为空"));
    }
    let n = name.chars().count();
    if n > 50 {
        return Err(config_err(&format!("邮箱名称超长：{n} 字（上限 50）")));
    }
    Ok(name.to_string())
}

/// `account` 校验：非空、≤120 字、拒空白 / CR / LF、必须含 `@`。
///
/// ⚠️ **刻意不做 RFC 5322 全量校验**（理由照 `mail_digest::service::validate_email`）：
/// 企业邮箱的真实形态千奇百怪，全量校验的误拒率高到运维宁可关掉功能。这里只挡
/// 「明显不是地址」与「有注入风险」两类。
///
/// ⚠️ **错误码是 `21704` 而不是 `21703`**：`21703` 在本仓的语义是「**收件人**邮箱
/// 非法」，而这里是**发信登录账号**。两者都用会让前端的错误文案串台。
pub(crate) fn validate_account(raw: &str) -> Result<String, AppError> {
    let acc = raw.trim();
    if acc.is_empty() {
        return Err(config_err("SMTP 登录账号不能为空"));
    }
    if acc.chars().any(|c| c.is_whitespace()) {
        return Err(config_err("SMTP 登录账号含空白字符"));
    }
    if acc.contains('\r') || acc.contains('\n') {
        return Err(config_err("SMTP 登录账号含 CR/LF"));
    }
    if !acc.contains('@') {
        return Err(config_err(&format!(
            "SMTP 登录账号应为一个完整邮箱地址（163 / QQ 都是），收到 `{acc}`"
        )));
    }
    let n = acc.chars().count();
    if n > 120 {
        return Err(config_err(&format!(
            "SMTP 登录账号超长：{n} 字（上限 120）"
        )));
    }
    Ok(acc.to_string())
}

/// 授权码校验：trim 后非空、≤200 字、拒控制字符。
///
/// ⚠️ **必填**：不存在「先建邮箱、以后再补授权码」。授权码是发信的必要输入，
/// 空串进了库只会让 `/send` 在几周后以 `21701` 失败，而那时没人想得起来是
/// 建邮箱时留空的。
///
/// ⚠️ **拒 CR/LF / 控制字符**：授权码会进 SMTP 的 `AUTH` 命令行。这些字节在
/// 协议里是行终止符，带进去属于凭据注入面。真实授权码是纯字母数字。
pub(crate) fn validate_secret(raw: &str) -> Result<String, AppError> {
    let s = raw.trim();
    if s.is_empty() {
        return Err(config_err("授权码不能为空"));
    }
    if s.chars().any(|c| c.is_control()) {
        return Err(config_err("授权码含控制字符（CR/LF 等，已拒绝）"));
    }
    let n = s.chars().count();
    if n > SECRET_MAX_CHARS {
        return Err(config_err(&format!(
            "授权码超长：{n} 字（上限 {SECRET_MAX_CHARS}）"
        )));
    }
    Ok(s.to_string())
}

// ============================================================================
// 错误构造
// ============================================================================

fn config_err(msg: &str) -> AppError {
    AppError::biz(code::BIZ_MAIL_CONFIG_INVALID, msg.to_string())
}

fn account_not_found() -> AppError {
    AppError::biz(
        code::BIZ_MAIL_ACCOUNT_NOT_FOUND,
        "邮箱不存在（或已被删除）：请刷新邮箱列表后重试",
    )
}

fn name_duplicate(name: &str) -> AppError {
    AppError::biz(
        code::BIZ_MAIL_ACCOUNT_NAME_DUPLICATE,
        format!("邮箱名称「{name}」已存在（未删除的邮箱之间不允许重名）"),
    )
}

/// OCC 冲突文案与 mail_digest 域同款（同一个 `40901`，同一件事）。
fn version_conflict() -> AppError {
    AppError::biz(
        code::VERSION_CONFLICT,
        "邮箱已被其他人修改（或删除），请刷新后重试",
    )
}

/// `set-default` 与**并发的另一次 set-default** 撞「至多一个默认」时的文案。
///
/// ⚠️ **用 `40901` 而不是复用 `version_conflict()` 的文案、也不是新开业务码**：本次提交
/// 是基于一份已过期的「谁是默认」快照（判据照 CLAUDE.md 对 `mail_digest` 配置单例并发
/// 首次初始化的同款处理：23505 → OCC 冲突）。复用 `version_conflict()` 会把「别人改了
/// **这个**邮箱」与「别人改了**全局的默认**」混成一句话，管理员照提示去刷新单个邮箱的
/// 编辑框会看不出任何变化。
fn default_conflict() -> AppError {
    AppError::biz(
        code::VERSION_CONFLICT,
        "另一个管理员刚刚改动了默认邮箱，请刷新列表后重试",
    )
}

/// PG 唯一约束冲突（SQLSTATE 23505）—— **委托全仓单一真源**。
///
/// 2026-10-17：本函数原先是逐字私有副本（与 `mail_digest::service` 同一份判据两份
/// 代码），随 `com::applicant` 引入第三份而一并上提到 `shared::error`。
/// ⚠️ **本域的错误码映射逐字不动**：`21707`（重名）/ `40901`（默认邮箱 OCC）仍在本文件里。
fn is_unique_violation(e: &sqlx::Error) -> bool {
    crate::shared::error::is_unique_violation(e)
}
