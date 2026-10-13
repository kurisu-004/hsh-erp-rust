//! scheduled_task 域 repo 层 —— `t_mail_account` 的 SQL 真源
//!
//! ## 全域**唯一**一条跨域表引用：`count_digest_configs_referencing`
//!
//! 「被引用的邮箱禁止删除」这条硬约束必须查 `t_mail_digest_config.sender_account_id`
//! —— 那是 mail_digest 域的表。本仓对「跨域只读」有既定 pattern（`dashboard` /
//! `statistics` / `admin` 都在本域 SQL 里只读聚合别域表），**不 import 别域的
//! service / repo**。本文件正是那个 pattern 的应用：SQL 写在本域，只查一个标量。
//!
//! ⚠️ 这条查询**刻意不参与域隔离护栏的判定**：护栏扫的是 `crate::modules::<他域>`
//! 的**代码路径**引用，而这里是 SQL 表名，两者不同轴。见 `mod.rs` 的护栏说明。

use chrono::NaiveDateTime;
use sqlx::{PgConnection, Row};

use crate::modules::scheduled_task::model::TMailAccount;

/// 列出全部未软删邮箱：**默认邮箱排最前**，同组内按 `id ASC`。
///
/// 排序理由：默认邮箱是「配好就能用」的那一个，管理员打开列表第一眼要看到它。
/// `id ASC` 只是为了让分页/展示有确定次序（本端点**不**分页，列表规模是管理员手
/// 动维护的个位数）。
pub async fn list_accounts(conn: &mut PgConnection) -> Result<Vec<TMailAccount>, sqlx::Error> {
    sqlx::query_as!(
        TMailAccount,
        r#"
        SELECT id, version, name, preset, account, encrypted_secret, is_default,
               created_at, created_by, updated_at, updated_by, deleted_at
        FROM t_mail_account
        WHERE deleted_at IS NULL
        ORDER BY is_default DESC, id ASC
        "#
    )
    .fetch_all(&mut *conn)
    .await
}

/// 按 id 读单个未软删邮箱（`None` = 不存在**或**已软删）。
///
/// ⚠️ 发信路径（`mail_digest::service::resolve_sender`）也走它，于是「指向已软删
/// 邮箱」会与「指向不存在的 id」得到同一个 `None` ⇒ 同一个 `21701`。这是刻意的：
/// 对调用方而言两者没有区别 —— 邮箱没了就是发不出信。
pub async fn get_account(
    conn: &mut PgConnection,
    id: i64,
) -> Result<Option<TMailAccount>, sqlx::Error> {
    sqlx::query_as!(
        TMailAccount,
        r#"
        SELECT id, version, name, preset, account, encrypted_secret, is_default,
               created_at, created_by, updated_at, updated_by, deleted_at
        FROM t_mail_account
        WHERE id = $1 AND deleted_at IS NULL
        "#,
        id
    )
    .fetch_optional(&mut *conn)
    .await
}

/// 新建邮箱的一行（id / 时间戳由调用方生成后填进来）。
///
/// ⚠️ **为什么用结构体而不是 7 个平铺形参**：本函数的形参有 7 个（加 `conn` 共 8 个），
/// 逐个传下去要靠调用点的位置记忆「哪个是 `operator`、哪个是 `now`」—— 而它们在
/// SQL 的 VALUES 里是**互换不出错**的一对。结构体让这件事在类型上不可互换。
///
/// ⚠️ **刻意不 derive `Debug`，手写一个屏蔽 `encrypted_secret`**（与
/// [`crate::modules::scheduled_task::model::TMailAccount`] 同一手法）：derive 版会让任何
/// 一处 `{new:?}` 把密文原文写进日志，而密文是 base64、可解码。
#[derive(Clone)]
pub struct NewAccount {
    pub id: i64,
    pub name: String,
    /// 预设字面量（`MailAccountPreset::as_str()`，不存 host/port）。
    pub preset: String,
    /// SMTP 登录账号（同时是发件人地址）。
    pub account: String,
    /// 授权码密文（`crypto::seal` 的产物）。
    pub encrypted_secret: String,
    pub operator: i64,
    pub now: NaiveDateTime,
}

/// 手写 `Debug`：屏蔽 `encrypted_secret`（2026-10-16）。
///
/// ⚠️ 字段顺序与声明顺序一致，便于和 derive 版对照阅读。
impl std::fmt::Debug for NewAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NewAccount")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("preset", &self.preset)
            .field("account", &self.account)
            // ⚠️ 永远输出 `***`
            .field("encrypted_secret", &"***")
            .field("operator", &self.operator)
            .field("now", &self.now)
            .finish()
    }
}

/// 插入邮箱（`version = 0`）。
///
/// ⚠️ **不设默认**：新建永远从 `is_default = false` 起，置默认是
/// `POST /accounts/{id}/set-default` 那条独立端点的事。理由：建邮箱与「让它成为
/// 默认发信通道」是两种意图，混在一个请求里会出现「新建第三个邮箱顺手把默认
/// 抢走」这种完全没被要求的副作用。代价是新装的库第一个邮箱不是默认 —— 用
/// set-default 补一次即可，前端在「还没有默认邮箱」时会显示提示。
///
/// 返回 `Ok(false)` 表示撞了 `uk_t_mail_account_name_alive`（同名的未软删行已存在）
/// ⇒ 调用方转 `21707 BIZ_MAIL_ACCOUNT_NAME_DUPLICATE`（**不是** 40901：
/// CLAUDE.md 明令 40901 只表达 OCC，业务唯一键冲突须有专用码）。
pub async fn insert_account(
    conn: &mut PgConnection,
    new: &NewAccount,
) -> Result<bool, sqlx::Error> {
    let res = sqlx::query(
        r#"
        INSERT INTO t_mail_account (
            id, version, name, preset, account, encrypted_secret, is_default,
            created_at, created_by, updated_at, updated_by
        ) VALUES ($1, 0, $2, $3, $4, $5, false, $6, $7, $6, $7)
        "#,
    )
    .bind(new.id)
    .bind(&new.name)
    .bind(&new.preset)
    .bind(&new.account)
    .bind(&new.encrypted_secret)
    .bind(new.now)
    .bind(new.operator)
    .execute(&mut *conn)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// 全量覆写（OCC）。`rows_affected == 0` ⇒ 版本对不上 ⇒ 调用方返 `40901`。
///
/// ⚠️ **不覆写 `encrypted_secret`**：本函数刻意**没有**这个参数 —— 「空 secret
/// ⇒ 沿用旧密文」这条规则在 service 层就完成了（它要把 `None` 翻译成「把旧列
/// 原样写回去」）。放在 SQL 层就要么加一个 `COALESCE($n, encrypted_secret)`，
/// 要么加一个 bool 开关切两套语句 —— 两种都把「沿用」这件事藏进 SQL，而它
/// 恰恰是最需要被看见的一条业务规则。
///
/// ⚠️ **不碰 `is_default`**：改默认是独立端点 + 独立唯一索引的操作，混进普通
/// 覆写会让「先降后升」的事务顺序无法保证。
pub async fn update_account(
    conn: &mut PgConnection,
    id: i64,
    expected_version: i32,
    patch: &AccountPatch,
) -> Result<bool, sqlx::Error> {
    let res = sqlx::query(
        r#"
        UPDATE t_mail_account
        SET name             = $3,
            preset           = $4,
            account          = $5,
            encrypted_secret = $6,
            version          = version + 1,
            updated_at       = $7,
            updated_by       = $8
        WHERE id = $1 AND version = $2 AND deleted_at IS NULL
        "#,
    )
    .bind(id)
    .bind(expected_version)
    .bind(&patch.name)
    .bind(&patch.preset)
    .bind(&patch.account)
    .bind(&patch.encrypted_secret)
    .bind(patch.now)
    .bind(patch.operator)
    .execute(&mut *conn)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// 全量覆写的目标值（见 [`update_account`] 的「为什么不覆写 `encrypted_secret` 之外
/// 的两列」与 [`NewAccount`] 的形参打包理由）。
///
/// ⚠️ **同样手写 `Debug` 屏蔽 `encrypted_secret`**（与 [`NewAccount`] 同款理由：本域的
/// 凭据一律走手写 `Debug`，不靠「调用方记得别打印」这条约定）。
#[derive(Clone)]
pub struct AccountPatch {
    pub name: String,
    pub preset: String,
    pub account: String,
    /// 新密文（service 层已把「空 secret ⇒ 沿用旧密文」翻译成本字段的旧值）。
    pub encrypted_secret: String,
    pub operator: i64,
    pub now: NaiveDateTime,
}

/// 手写 `Debug`：屏蔽 `encrypted_secret`（2026-10-16）。
impl std::fmt::Debug for AccountPatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountPatch")
            .field("name", &self.name)
            .field("preset", &self.preset)
            .field("account", &self.account)
            // ⚠️ 永远输出 `***`
            .field("encrypted_secret", &"***")
            .field("operator", &self.operator)
            .field("now", &self.now)
            .finish()
    }
}

/// 软删（`deleted_at` + `version + 1`），带 OCC。
pub async fn soft_delete_account(
    conn: &mut PgConnection,
    id: i64,
    expected_version: i32,
    operator: i64,
    now: NaiveDateTime,
) -> Result<bool, sqlx::Error> {
    let res = sqlx::query(
        r#"
        UPDATE t_mail_account
        SET deleted_at = now(),
            version    = version + 1,
            updated_at = $3,
            updated_by = $4
        WHERE id = $1 AND version = $2 AND deleted_at IS NULL
        "#,
    )
    .bind(id)
    .bind(expected_version)
    .bind(now)
    .bind(operator)
    .execute(&mut *conn)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// 清掉全部「默认」标记（`set-default` 的第一步）。
///
/// ⚠️ **必须先于** [`promote_to_default`] 调用：唯一索引
/// `uk_t_mail_account_one_default ON t_mail_account ((true)) WHERE deleted_at IS NULL AND is_default`
/// 保证「至多一个未软删行为 true」，于是「先升新的再降旧的」会在第二个 UPDATE 上
/// 撞 23505。调用方（`service::set_default_account`）在**同一事务内**按
/// 「先降后升」顺序调用本函数与 [`promote_to_default`]。
///
/// 返回影响行数（0 或 1：至多一个默认行存在）。**不判 `deleted_at`**：
/// 已软删行的 `is_default` 无意义，改它不违反任何约束，但没必要多一次写。
pub async fn clear_default(executor: &mut PgConnection) -> Result<u64, sqlx::Error> {
    let res = sqlx::query(
        r#"
        UPDATE t_mail_account
        SET is_default = false
        WHERE is_default = true
        "#,
    )
    .execute(&mut *executor)
    .await?;
    Ok(res.rows_affected())
}

/// 把指定邮箱升为默认（`set-default` 的第二步；带 OCC，故 0 行仍可能是
/// 「版本对不上」而不是「行不存在」—— 调用方已先查过行，故按 OCC 处理）。
pub async fn promote_to_default(
    conn: &mut PgConnection,
    id: i64,
    expected_version: i32,
    operator: i64,
    now: NaiveDateTime,
) -> Result<bool, sqlx::Error> {
    let res = sqlx::query(
        r#"
        UPDATE t_mail_account
        SET is_default = true,
            version    = version + 1,
            updated_at = $3,
            updated_by = $4
        WHERE id = $1 AND version = $2 AND deleted_at IS NULL
        "#,
    )
    .bind(id)
    .bind(expected_version)
    .bind(now)
    .bind(operator)
    .execute(&mut *conn)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// 该邮箱是否被 mail_digest 配置引用（`t_mail_digest_config.sender_account_id`）。
///
/// ⚠️ **只数未软删的配置行**：`t_mail_digest_config` 的存活条件是
/// `deleted_at IS NULL`（见 `uk_t_mail_digest_config_alive`），已软删的配置行
/// 既不会发信也不会被配置页读到，拿它当「占用」会让管理员删不掉一个实际上谁也没
/// 在用的邮箱。
///
/// 这是本域**唯一**一条触及他域表的 SQL，理由见模块 doc。
pub async fn count_digest_configs_referencing(
    conn: &mut PgConnection,
    account_id: i64,
) -> Result<i64, sqlx::Error> {
    let row = sqlx::query(
        r#"
        SELECT COUNT(*) AS n
        FROM t_mail_digest_config
        WHERE sender_account_id = $1 AND deleted_at IS NULL
        "#,
    )
    .bind(account_id)
    .fetch_one(&mut *conn)
    .await?;
    row.try_get::<i64, _>(0)
}
