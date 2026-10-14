//! wecom 域 repo 层 —— `t_wecom_webhook` 的 SQL 真源
//!
//! ## 全域**唯一**一条跨域表引用：`count_daily_configs_referencing`
//!
//! 「被引用的 webhook 禁止删除」这条硬约束必须查 `t_wecom_daily_config.webhook_id`
//! —— 那是 `wecom_daily` 域的表。本仓对「跨域只读」有既定 pattern（`dashboard` /
//! `statistics` / `admin` / `scheduled_task` 都在本域 SQL 里只读聚合别域表），
//! **不 import 别域的 service / repo**。本文件正是那个 pattern 的应用：SQL 写在本域，
//! 只查一个标量。
//!
//! ⚠️ 这条查询**刻意不参与域隔离护栏的判定**：护栏扫的是 `crate::modules::<他域>` 的
//! **代码路径**引用，而这里是 SQL 表名，两者不同轴。见 `mod.rs` 的护栏说明。
//!
//! ## 与 `wx::wecom_client.rs` 无关
//!
//! 本文件读写的是「定时任务」菜单下的群机器人 webhook 实体，与小程序登录那条链路零交集。

use chrono::NaiveDateTime;
use sqlx::{PgConnection, Row};

use crate::modules::wecom::model::TWeComWebhookRow;

/// 列出全部未软删 webhook：**默认 webhook 排最前**，同组内按 `id ASC`。
///
/// 排序理由：默认 webhook 是「配好就能用」的那一个，管理员打开列表第一眼要看到它。
/// ⚠️ **排序只是展示**：`is_default` **不参与发送** —— 发送只认
/// `t_wecom_daily_config.webhook_id` 指到哪一行（见模块 doc 的口径表）。
/// `id ASC` 只是为了让展示有确定次序（本端点**不**分页，列表规模是管理员手动维护的个位数）。
pub async fn list_webhooks(conn: &mut PgConnection) -> Result<Vec<TWeComWebhookRow>, sqlx::Error> {
    sqlx::query_as!(
        TWeComWebhookRow,
        r#"
        SELECT id, version, name, encrypted_url, is_default,
               created_at, created_by, updated_at, updated_by, deleted_at
        FROM t_wecom_webhook
        WHERE deleted_at IS NULL
        ORDER BY is_default DESC, id ASC
        "#
    )
    .fetch_all(&mut *conn)
    .await
}

/// 按 id 读单个未软删 webhook（`None` = 不存在**或**已软删）。
///
/// ⚠️ 探针端点（`/test`）与内容域的推送路径（`service::resolve_target`）也走它，于是
/// 「指向已软删 webhook」会与「指向不存在的 id」得到同一个 `None`。这是刻意的：探针侧
/// 分流成 `21806`（管理员点了这个 id，但它已经没了），推送侧统一 `21801`（对调用方而言
/// 两者没有区别 —— 通道没了就是发不出去）。
pub async fn get_webhook(
    conn: &mut PgConnection,
    id: i64,
) -> Result<Option<TWeComWebhookRow>, sqlx::Error> {
    sqlx::query_as!(
        TWeComWebhookRow,
        r#"
        SELECT id, version, name, encrypted_url, is_default,
               created_at, created_by, updated_at, updated_by, deleted_at
        FROM t_wecom_webhook
        WHERE id = $1 AND deleted_at IS NULL
        "#,
        id
    )
    .fetch_optional(&mut *conn)
    .await
}

/// 新建 webhook 的一行（id / 时间戳由调用方生成后填进来）。
///
/// ⚠️ **为什么用结构体而不是 6 个平铺形参**：`operator` 与 `now` 在 SQL 的 VALUES 里
/// 是**互换不出错**的一对（都是「元信息」），靠位置记忆迟早会传反。结构体让这件事在
/// 类型上不可互换（照 `scheduled_task::repo::NewAccount` 同款理由）。
///
/// ⚠️ **刻意不 derive `Debug`，手写一个屏蔽 `encrypted_url`**（与
/// [`crate::modules::wecom::model::TWeComWebhookRow`] 同一手法）：derive 版会让任何一处
/// `{new:?}` 把密文原文写进日志，而密文是 base64、可解码。
#[derive(Clone)]
pub struct NewWebhook {
    pub id: i64,
    pub name: String,
    /// 地址密文（`crypto::seal_with_aad` 的产物）。
    pub encrypted_url: String,
    pub operator: i64,
    pub now: NaiveDateTime,
}

/// 手写 `Debug`：屏蔽 `encrypted_url`（2026-10-18）。
impl std::fmt::Debug for NewWebhook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NewWebhook")
            .field("id", &self.id)
            .field("name", &self.name)
            // ⚠️ 永远输出 `***`
            .field("encrypted_url", &"***")
            .field("operator", &self.operator)
            .field("now", &self.now)
            .finish()
    }
}

/// 插入 webhook（`version = 0`）。
///
/// ⚠️ **不设默认**：新建永远从 `is_default = false` 起，置默认是
/// `POST /webhooks/{id}/set-default` 那条独立端点的事。理由与 mail 域同款：建通道与
/// 「让它成为默认推送通道」是两种意图，混在一个请求里会出现「新建第三个 webhook 顺手
/// 把默认抢走」这种完全没被要求的副作用。
pub async fn insert_webhook(
    conn: &mut PgConnection,
    new: &NewWebhook,
) -> Result<bool, sqlx::Error> {
    let res = sqlx::query(
        r#"
        INSERT INTO t_wecom_webhook (
            id, version, name, encrypted_url, is_default,
            created_at, created_by, updated_at, updated_by
        ) VALUES ($1, 0, $2, $3, false, $4, $5, $4, $5)
        "#,
    )
    .bind(new.id)
    .bind(&new.name)
    .bind(&new.encrypted_url)
    .bind(new.now)
    .bind(new.operator)
    .execute(&mut *conn)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// 全量覆写的一行（见 [`insert_webhook`] 的形参打包理由）。
///
/// ⚠️ **同样手写 `Debug` 屏蔽 `encrypted_url`**（本域的凭据一律走手写 `Debug`）。
#[derive(Clone)]
pub struct WebhookPatch {
    pub name: String,
    /// 新密文（service 层已把「空 URL ⇒ 沿用旧密文」翻译成本字段的旧值）。
    pub encrypted_url: String,
    pub operator: i64,
    pub now: NaiveDateTime,
}

/// 手写 `Debug`：屏蔽 `encrypted_url`（2026-10-18）。
impl std::fmt::Debug for WebhookPatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebhookPatch")
            .field("name", &self.name)
            // ⚠️ 永远输出 `***`
            .field("encrypted_url", &"***")
            .field("operator", &self.operator)
            .field("now", &self.now)
            .finish()
    }
}

/// 全量覆写（OCC）。`rows_affected == 0` ⇒ 版本对不上 ⇒ 调用方返 `40901`。
///
/// ⚠️ **不碰 `is_default`**：改默认是独立端点 + 独立唯一索引的操作，混进普通覆写会让
/// 「先降后升」的事务顺序无法保证。
pub async fn update_webhook(
    conn: &mut PgConnection,
    id: i64,
    expected_version: i32,
    patch: &WebhookPatch,
) -> Result<bool, sqlx::Error> {
    let res = sqlx::query(
        r#"
        UPDATE t_wecom_webhook
        SET name           = $3,
            encrypted_url  = $4,
            version        = version + 1,
            updated_at     = $5,
            updated_by     = $6
        WHERE id = $1 AND version = $2 AND deleted_at IS NULL
        "#,
    )
    .bind(id)
    .bind(expected_version)
    .bind(&patch.name)
    .bind(&patch.encrypted_url)
    .bind(patch.now)
    .bind(patch.operator)
    .execute(&mut *conn)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// 软删（`deleted_at` + `version + 1`），带 OCC。
pub async fn soft_delete_webhook(
    conn: &mut PgConnection,
    id: i64,
    expected_version: i32,
    operator: i64,
    now: NaiveDateTime,
) -> Result<bool, sqlx::Error> {
    let res = sqlx::query(
        r#"
        UPDATE t_wecom_webhook
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
/// `uk_t_wecom_webhook_one_default ON t_wecom_webhook ((true)) WHERE deleted_at IS NULL AND is_default`
/// 保证「至多一个未软删行为 true」，于是「先升新的再降旧的」会在第二个 UPDATE 上撞
/// 23505。调用方（`service::set_default_webhook`）在**同一事务内**按「先降后升」顺序
/// 调用本函数与 [`promote_to_default`]。
///
/// 返回影响行数（0 或 1）。**不判 `deleted_at`**：已软删行的 `is_default` 无意义，
/// 改它不违反任何约束，但没必要多一次写。
pub async fn clear_default(executor: &mut PgConnection) -> Result<u64, sqlx::Error> {
    let res = sqlx::query(
        r#"
        UPDATE t_wecom_webhook
        SET is_default = false
        WHERE is_default = true
        "#,
    )
    .execute(&mut *executor)
    .await?;
    Ok(res.rows_affected())
}

/// 把指定 webhook 升为默认（`set-default` 的第二步；带 OCC，故 0 行仍可能是
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
        UPDATE t_wecom_webhook
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

/// 该 webhook 是否被「每日总结」配置引用（`t_wecom_daily_config.webhook_id`）。
///
/// ⚠️ **只数未软删的配置行**：`t_wecom_daily_config` 的存活条件是 `deleted_at IS NULL`
/// （见 `uk_t_wecom_daily_config_alive`），已软删的配置行既不会推送也不会被配置页读到，
/// 拿它当「占用」会让管理员删不掉一个实际上谁也没在用的 webhook。
///
/// 这是本域**唯一**一条触及他域表的 SQL，理由见文件头。
pub async fn count_daily_configs_referencing(
    conn: &mut PgConnection,
    webhook_id: i64,
) -> Result<i64, sqlx::Error> {
    let row = sqlx::query(
        r#"
        SELECT COUNT(*) AS n
        FROM t_wecom_daily_config
        WHERE webhook_id = $1 AND deleted_at IS NULL
        "#,
    )
    .bind(webhook_id)
    .fetch_one(&mut *conn)
    .await?;
    row.try_get::<i64, _>(0)
}
