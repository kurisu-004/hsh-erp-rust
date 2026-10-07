//! 送货单域「跨域只读」的 SQL 真源
//!
//! 本文件只放**本域专有、别的域没有对应 repo 方法**的跨域查询。规则：
//! - 能用他域既有 repo 方法的，一律走他域 repo（`WorkerRepo::get_by_id` /
//!   `CustomerRepo::get_by_id` / `PartRepo::get_by_serial` …），不重复造 SQL；
//! - 只有「本域独有的语义投影」才写在这里。
//!
//! 2026-10-08 新增 `DeliveryDriverRepo::list_drivers`：司机候选下拉。它**不复用**
//! `prod::worker::vo::WorkerOut` —— 后者带 `id_card_no` / `phone` / `version` /
//! `created_at` 等 11 个字段，对「选一个送货司机」这个用途全是多余载荷（且
//! `id_card_no` / `phone` 属于个人信息，不该因为一个下拉框就发到前端）。

use crate::modules::prod::worker::model::TWorker;

/// 送货单域的跨域只读 ZST（无状态，方法全是 `static`）。
pub struct DeliveryDriverRepo;

impl DeliveryDriverRepo {
    /// `GET /api/v2/com/delivery/drivers` 的候选送货司机一览。
    ///
    /// 判据（4 条，缺一不可）：
    /// - `wt.code = '送货司机'`（工种字面量与 `validate_driver` 的
    ///   `WORK_TYPE_DRIVER_CODE` 同源，见 `service/lifecycle.rs`）；
    /// - `w.is_active`（在职）；
    /// - `w.deleted_at IS NULL` 与 `wt.deleted_at IS NULL`（软删闸门，工种软删后
    ///   不该再出现在候选里）。
    ///
    /// 排序 `w.name, w.id`：同名时按 id 定序，保证同一数据集的分页/快照稳定。
    ///
    /// ⚠️ 与 `GET /api/v2/prod/workers` 的 MANAGER-only 不同，本端点放行
    /// Manager / Clerk / Inspector —— 但**只返司机**这一个人群，不泄露整张工人表。
    pub async fn list_drivers(conn: &mut sqlx::PgConnection) -> Result<Vec<TWorker>, sqlx::Error> {
        sqlx::query_as!(
            TWorker,
            r#"
            SELECT w.id, w.badge_code, w.name, w.id_card_no, w.phone, w.is_active,
                   w.version, w.created_at, w.created_by, w.updated_at, w.updated_by,
                   w.deleted_at, w.work_type_id
            FROM t_worker w
            JOIN t_work_type wt ON wt.id = w.work_type_id
            WHERE wt.code = '送货司机'
              AND w.is_active
              AND w.deleted_at IS NULL
              AND wt.deleted_at IS NULL
            ORDER BY w.name, w.id
            "#,
        )
        .fetch_all(&mut *conn)
        .await
    }
}
