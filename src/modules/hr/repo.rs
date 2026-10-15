//! `hr` 域数据访问层（**2026-10-19 新增**）—— 三张表的 **SQL 真源** + 守卫谓词。
//!
//! ## 全部走运行时 `sqlx::query_as` / `sqlx::query`，**不进 `query!` 宏**
//!
//! 与 `serial::repo`（2026-10-19 最新域立的先例）同款取舍：本域新增 SQL
//! **不需要**跑 `./scripts/sqlx_prepare.sh`、**不产生** `.sqlx/` 缓存 diff。
//!
//! ## ⚠️ 为什么 jsonb + 不用 `query!` 宏
//!
//! 两层理由，缺一不可：
//!
//! 1. **零先例组合**：本仓 `.sqlx/` 的 281 份离线元数据里 **0 个 `JSONB` type_info**，
//!    建库至今 42 张表**零 jsonb 列** ⇒ `query!` 宏 + jsonb 在本仓是**零先例**组合。
//!    `cargo sqlx prepare` 能否为 jsonb 生成正确的 type_info **未经验证**；一旦它
//!    生成不出来或生成错，整个离线构建链（CI / Docker 的 `SQLX_OFFLINE=true`）会当场
//!    断掉，而那是本仓发版的主路径。
//! 2. **运行时路线本就更直接**：时段内联 jsonb 之后，读端点必须**逐行**
//!    `serde_json::from_value::<Vec<ShiftSegment>>` 解析（还要在解析失败时降级成
//!    `21903` 而不是 500）—— 用宏拿到的已经是「按列名对齐的强类型」，反倒没有地方
//!    插这层可失败、可降级的解析。
//!
//! `serde_json::Value` 覆盖 JSONB 的仓内先例是 `outsource::board::repo` 的
//! `to_jsonb(array_agg(json_build_object(..)))` 读路径。
//!
//! ## 守卫谓词的唯一真源
//!
//! [`ALIVE`] = `deleted_at IS NULL` 是本域**每一条**读查询都带的谓词；
//! 需要读软删行时用 `AND ($N::bool OR deleted_at IS NULL)` 放开（本域当前**没有**
//! 调用点 —— 全局日历软删行等价于「该日无显式行」、班次软删行等价于「该班次不存在」，
//! 两处都不该把软删行读出来）。
//!
//! ## 写路径的 OCC
//!
//! `UPDATE … WHERE id = $1 AND version = $2 AND deleted_at IS NULL`；
//! `rows_affected == 0` 由 service 回查归因（不存在 → `21901` / 版本陈旧 → `40901`），
//! repo 层**只回传行数**（CLAUDE.md §8：禁止「读-再-比」式的隐式 OCC）。

use chrono::{NaiveDate, NaiveDateTime};
use sqlx::{AssertSqlSafe, PgConnection};

use crate::modules::hr::model::{TCalendarDayRow, TShiftCalendarRow, TShiftRow};
use crate::shared::error::AppError;

/// 未软删守卫谓词（**唯一真源**）。
pub const ALIVE: &str = "deleted_at IS NULL";

/// 班次投影（列表 / 单读共用）。
const SHIFT_PROJECTION: &str = "id, version, code, name, color, segments, is_active, \
                               sort_order, remark, created_at, created_by, updated_at, \
                               updated_by, deleted_at";

// ============================================================================
// t_hr_calendar_day —— 全局日历显式行
// ============================================================================

/// 区间内的全部**未软删**全局日历显式行（按 `cal_date` 升序）。
///
/// ⚠️ 区间**缺行不等于出错**：常态工作日 / 常态周末本就不落行，service 拿到这一批
/// 显式行后会**逐日补齐**推导值并把那些日标 `derived = true`。
pub async fn list_calendar_days(
    conn: &mut PgConnection,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<Vec<TCalendarDayRow>, AppError> {
    let sql = format!(
        "SELECT id, cal_date, day_type, name, remark, version, created_at, created_by, \
                updated_at, updated_by, deleted_at \
         FROM t_hr_calendar_day WHERE {ALIVE} AND cal_date BETWEEN $1 AND $2 \
         ORDER BY cal_date ASC"
    );
    Ok(sqlx::query_as::<_, TCalendarDayRow>(AssertSqlSafe(sql))
        .bind(from)
        .bind(to)
        .fetch_all(&mut *conn)
        .await?)
}

/// 待 upsert 的全局日历行（**id 由 App 侧生成**后一并交给本层）。
///
/// ⚠️ **刻意用一个结构体而不是 5 个平铺数组形参**：`day_type` / `name` / `remark` 是
/// 一一对应的三列，传 5 个等长数组时，任何一处顺序错位都会变成「A 日期写上了 B 的
/// 日型」这种**静默**错行 —— 结构体让配对关系由类型保证（逐字同
/// `wecom::repo::NewWebhook` 用结构体而非平铺形参的理由）。
#[derive(Debug, Clone)]
pub struct NewCalendarDay {
    pub id: i64,
    pub cal_date: NaiveDate,
    pub day_type: String,
    pub name: Option<String>,
    pub remark: Option<String>,
}

/// 批量 upsert 全局日历显式行（`ON CONFLICT (cal_date) WHERE deleted_at IS NULL`）。
///
/// ## 为什么 `ON CONFLICT` 的推断目标要**带谓词**
///
/// PG 的冲突目标必须与**被推断的那个索引**逐字一致，而 `uk_t_hr_calendar_day_date`
/// 是 partial unique（`WHERE deleted_at IS NULL`）⇒ 目标必须写成
/// `(cal_date) WHERE deleted_at IS NULL`。写成裸 `(cal_date)` 会报
/// `there is no unique or exclusion constraint matching the ON CONFLICT specification`。
///
/// ## 多列数组 `UNNEST` 的等长前提
///
/// PG 10+ 的多参数 `UNNEST` 会把短数组**补 NULL** 到最长数组的长度。补 NULL 正是
/// 本函数要的语义（`name` / `remark` 缺省就是 NULL），但 `cal_date` / `day_type` 被
/// 补 NULL 会**整条 INSERT 失败**（NOT NULL 列）⇒ 五个数组**逐项等长**。这里由
/// `NewCalendarDay` 的逐项映射保证（一个元素的五个字段必然同时产出），不在 SQL 里依赖
/// 补齐。
pub async fn upsert_calendar_days(
    conn: &mut PgConnection,
    rows: &[NewCalendarDay],
    operator: i64,
    now: NaiveDateTime,
) -> Result<i64, AppError> {
    let sql = "\
        INSERT INTO t_hr_calendar_day (
            id, cal_date, day_type, name, remark, version,
            created_at, created_by, updated_at, updated_by
        )
        SELECT u.id, u.cal_date, u.day_type, u.name, u.remark, 0, $6, $7, $6, $7
        FROM UNNEST($1::bigint[], $2::date[], $3::text[], $4::text[], $5::text[])
             AS u(id, cal_date, day_type, name, remark)
        ON CONFLICT (cal_date) WHERE deleted_at IS NULL DO UPDATE SET
            day_type   = EXCLUDED.day_type,
            name       = EXCLUDED.name,
            remark     = EXCLUDED.remark,
            version    = t_hr_calendar_day.version + 1,
            updated_at = EXCLUDED.updated_at,
            updated_by = EXCLUDED.updated_by";
    let res = sqlx::query(sql)
        .bind(rows.iter().map(|r| r.id).collect::<Vec<_>>())
        .bind(rows.iter().map(|r| r.cal_date).collect::<Vec<_>>())
        .bind(rows.iter().map(|r| &r.day_type).collect::<Vec<_>>())
        .bind(rows.iter().map(|r| &r.name).collect::<Vec<_>>())
        .bind(rows.iter().map(|r| &r.remark).collect::<Vec<_>>())
        .bind(now)
        .bind(operator)
        .execute(&mut *conn)
        .await?;
    Ok(res.rows_affected() as i64)
}

/// 按给定日期软删全局日历显式行（删完该日回退到按星期推导）。
pub async fn soft_delete_calendar_days_by_dates(
    conn: &mut PgConnection,
    dates: &[NaiveDate],
    operator: i64,
    now: NaiveDateTime,
) -> Result<i64, AppError> {
    let sql = format!(
        "UPDATE t_hr_calendar_day SET deleted_at = $2, updated_at = $2, updated_by = $3, \
                version = version + 1 \
         WHERE {ALIVE} AND cal_date = ANY($1::date[])"
    );
    let res = sqlx::query(AssertSqlSafe(sql))
        .bind(dates)
        .bind(now)
        .bind(operator)
        .execute(&mut *conn)
        .await?;
    Ok(res.rows_affected() as i64)
}

/// 按区间软删全局日历显式行（`from <= cal_date <= to`，两端闭）。
pub async fn soft_delete_calendar_days_by_range(
    conn: &mut PgConnection,
    from: NaiveDate,
    to: NaiveDate,
    operator: i64,
    now: NaiveDateTime,
) -> Result<i64, AppError> {
    let sql = format!(
        "UPDATE t_hr_calendar_day SET deleted_at = $3, updated_at = $3, updated_by = $4, \
                version = version + 1 \
         WHERE {ALIVE} AND cal_date BETWEEN $1 AND $2"
    );
    let res = sqlx::query(AssertSqlSafe(sql))
        .bind(from)
        .bind(to)
        .bind(now)
        .bind(operator)
        .execute(&mut *conn)
        .await?;
    Ok(res.rows_affected() as i64)
}

// ============================================================================
// t_hr_shift —— 班次
// ============================================================================

/// 班次列表。
///
/// - `include_inactive = false`：只列启用中的（默认，见
///   [`crate::modules::hr::dto::ShiftListQuery`]）。
/// - 排序：`is_active ASC`（停用排最后）、`sort_order ASC`、`id ASC` 兜底。
pub async fn list_shifts(
    conn: &mut PgConnection,
    include_inactive: bool,
) -> Result<Vec<TShiftRow>, AppError> {
    let sql = format!(
        "SELECT {SHIFT_PROJECTION} FROM t_hr_shift \
         WHERE {ALIVE} AND ($1::bool OR is_active) \
         ORDER BY is_active ASC, sort_order ASC, id ASC"
    );
    Ok(sqlx::query_as::<_, TShiftRow>(AssertSqlSafe(sql))
        .bind(include_inactive)
        .fetch_all(&mut *conn)
        .await?)
}

/// 按 id 读单个未软删班次（`None` = 不存在**或**已软删）。
pub async fn get_shift(conn: &mut PgConnection, id: i64) -> Result<Option<TShiftRow>, AppError> {
    let sql = format!("SELECT {SHIFT_PROJECTION} FROM t_hr_shift WHERE id = $1 AND {ALIVE}");
    Ok(sqlx::query_as::<_, TShiftRow>(AssertSqlSafe(sql))
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?)
}

/// `code` 是否已被某个未软删班次占用（建班次前的应用层预检）。
///
/// ⚠️ **预检不是防线**，`uk_t_hr_shift_code` 才是；并发建同名班次仍会撞 `23505`，
/// service 把那个也映射成 `21902`（逐字同码，见 `service::create_shift`）。
pub async fn code_exists(
    conn: &mut PgConnection,
    code: &str,
    exclude_id: Option<i64>,
) -> Result<bool, AppError> {
    let sql = format!(
        "SELECT EXISTS (SELECT 1 FROM t_hr_shift WHERE code = $1 AND {ALIVE} \
                        AND ($2::bigint IS NULL OR id <> $2))"
    );
    let found: bool = sqlx::query_scalar(AssertSqlSafe(sql))
        .bind(code)
        .bind(exclude_id)
        .fetch_one(&mut *conn)
        .await?;
    Ok(found)
}

/// 插入一个新班次（`version = 0`，`is_active = true`）。
///
/// ⚠️ **刻意不写 `is_active` 入参**：新建永远从启用态起（「建了就是能用的」）；
/// 下架是独立的 `POST /shifts/{id}/deactivate`，混进建请求会让「建第三个班次顺手把
/// 前两个停了」这种完全没被要求的副作用出现（同 `wecom::repo::insert_webhook` 不设默认）。
///
/// ⚠️ 返回 `Err(sqlx::Error)` 而非 `AppError`：唯一索引冲突（`23505`）要由 service
/// 映射成 `21902`，repo 层不猜业务码。
pub async fn insert_shift(conn: &mut PgConnection, new: &NewShift) -> Result<(), sqlx::Error> {
    sqlx::query(
        "\
        INSERT INTO t_hr_shift (
            id, version, code, name, color, segments, is_active, sort_order, remark,
            created_at, created_by, updated_at, updated_by
        ) VALUES ($1, 0, $2, $3, $4, $5, true, $6, $7, $8, $9, $8, $9)",
    )
    .bind(new.id)
    .bind(&new.code)
    .bind(&new.name)
    .bind(&new.color)
    // ⚠️ **必须包成 `Value::Array`**：直接 bind 一个 `Vec<serde_json::Value>` 会被
    // sqlx 编成 **jsonb[]**（数组类型），PG 报
    // `column "segments" is of type jsonb but expression is of type jsonb[]`。
    .bind(serde_json::Value::Array(new.segments.clone()))
    .bind(new.sort_order)
    .bind(&new.remark)
    .bind(new.now)
    .bind(new.operator)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// 建班次的入参（结构体而非平铺形参：`operator` 与 `now` 在 VALUES 里位置可互换，
/// 靠记忆迟早传反 —— 逐字同 `wecom::repo::NewWebhook` 的理由）。
#[derive(Debug, Clone)]
pub struct NewShift {
    pub id: i64,
    pub code: String,
    pub name: String,
    pub color: Option<String>,
    /// 规范化后的时段数组（已按 `start` 升序重排 + 重编 `seq`）。
    pub segments: Vec<serde_json::Value>,
    pub sort_order: i32,
    pub remark: Option<String>,
    pub operator: i64,
    pub now: NaiveDateTime,
}

/// 班次更新参数（**部分覆盖**：字段为 `None` 表示「这次不改」）。
///
/// ⚠️ `segments` / `color` / `remark` 三个列**一起写**：即使这次请求没带它们，
/// 也用「当前行里读出来的旧值」原样写回。这避免了 `SET segments = COALESCE(..)`
/// 那种「NULL 到底是「不改」还是「清空」的二义写法 —— 语义直接编码在
/// `Option<>` 上：请求里没带就传回旧值，带了就传新值。
#[derive(Debug, Clone)]
pub struct ShiftPatch {
    pub code: String,
    pub name: String,
    pub color: Option<String>,
    pub segments: Vec<serde_json::Value>,
    pub sort_order: i32,
    pub remark: Option<String>,
    pub operator: i64,
    pub now: NaiveDateTime,
}

/// 全列覆写一个班次（OCC：`WHERE id=$1 AND version=$2 AND deleted_at IS NULL`）。
///
/// 返回 `rows_affected`：1 = 成功；0 = **归因由 service 决定**（行不存在 → `21901`；
/// 存在但 `version` 陈旧 → `40901`）。repo 层刻意不替 service 做那个回查。
///
/// ⚠️ 返回 `sqlx::Error` 而非 `AppError`：改成已占用的 `code` 会撞
/// `uk_t_hr_shift_code`（`23505`），那个要由 service 映射成 `21902` 而不是 `50001`
/// —— 与 [`insert_shift`] 同一处理（同 `wecom::repo::update_webhook`）。
pub async fn update_shift(
    conn: &mut PgConnection,
    id: i64,
    version: i32,
    patch: &ShiftPatch,
) -> Result<i64, sqlx::Error> {
    let sql = format!(
        "UPDATE t_hr_shift SET code = $3, name = $4, color = $5, segments = $6, \
                sort_order = $7, remark = $8, version = version + 1, \
                updated_at = $9, updated_by = $10 \
         WHERE id = $1 AND version = $2 AND {ALIVE}"
    );
    let res = sqlx::query(AssertSqlSafe(sql))
        .bind(id)
        .bind(version)
        .bind(&patch.code)
        .bind(&patch.name)
        .bind(&patch.color)
        .bind(serde_json::Value::Array(patch.segments.clone()))
        .bind(patch.sort_order)
        .bind(&patch.remark)
        .bind(patch.now)
        .bind(patch.operator)
        .execute(&mut *conn)
        .await?;
    Ok(res.rows_affected() as i64)
}

/// 停用一个班次（OCC 同上；额外要求 `is_active = true`）。
///
/// ⚠️ **不软删、不级联软删 `t_hr_shift_calendar`**：停用是「不再用这个班次排新的班」，
/// 历史考勤还要复现当年那天的口径。`AND is_active` 让「重复停用」落到 0 行，
/// service 归因成 `21906`（幂等拒绝，与 `BIZ_PART_ALREADY_CANCELLED` 同款语义）。
pub async fn deactivate_shift(
    conn: &mut PgConnection,
    id: i64,
    version: i32,
    operator: i64,
    now: NaiveDateTime,
) -> Result<i64, AppError> {
    let sql = format!(
        "UPDATE t_hr_shift SET is_active = false, version = version + 1, \
                updated_at = $3, updated_by = $4 \
         WHERE id = $1 AND version = $2 AND {ALIVE} AND is_active"
    );
    let res = sqlx::query(AssertSqlSafe(sql))
        .bind(id)
        .bind(version)
        .bind(now)
        .bind(operator)
        .execute(&mut *conn)
        .await?;
    Ok(res.rows_affected() as i64)
}

// ============================================================================
// t_hr_shift_calendar —— 班次单日覆盖行
// ============================================================================

/// 某班次在区间内的全部**未软删**覆盖行（按 `cal_date` 升序）。
pub async fn list_shift_days(
    conn: &mut PgConnection,
    shift_id: i64,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<Vec<TShiftCalendarRow>, AppError> {
    let sql = format!(
        "SELECT id, version, shift_id, cal_date, day_type, remark, created_at, created_by, \
                updated_at, updated_by, deleted_at \
         FROM t_hr_shift_calendar WHERE {ALIVE} AND shift_id = $1 \
                AND cal_date BETWEEN $2 AND $3 \
         ORDER BY cal_date ASC"
    );
    Ok(sqlx::query_as::<_, TShiftCalendarRow>(AssertSqlSafe(sql))
        .bind(shift_id)
        .bind(from)
        .bind(to)
        .fetch_all(&mut *conn)
        .await?)
}

/// 待 upsert 的班次覆盖行（**id 由 App 侧生成**；理由同 [`NewCalendarDay`]）。
#[derive(Debug, Clone)]
pub struct NewShiftDay {
    pub id: i64,
    pub cal_date: NaiveDate,
    pub day_type: String,
    pub remark: Option<String>,
}

/// 批量 upsert 班次覆盖行（`ON CONFLICT (shift_id, cal_date) WHERE deleted_at IS NULL`）。
///
/// 冲突目标带谓词的理由同 [`upsert_calendar_days`]（那个索引是 partial unique）。
///
/// ## ⚠️ `is_active` 判据**下沉进 SQL**（2026-10-23）
///
/// `FROM UNNEST(…) AS u(…), t_hr_shift s WHERE s.id = $N AND s.is_active AND s.deleted_at
/// IS NULL`：班次不可写时整个 SELECT 命中 0 行 ⇒ 0 行插入，service 回查归因
/// （不存在 → `21901` / 已停用 → `21906`）。
///
/// 判据必须落在**写语句自己的谓词**里，只在 service 侧 `get_shift` + `ensure_writable`
/// 是读-再-写：并发的 `POST /shifts/{id}/deactivate` 落在读与写之间时，覆盖行会写进一个
/// 已经停用的班次，而 `21906` 被整个绕过。
pub async fn upsert_shift_days(
    conn: &mut PgConnection,
    shift_id: i64,
    rows: &[NewShiftDay],
    operator: i64,
    now: NaiveDateTime,
) -> Result<i64, AppError> {
    let sql = "\
        INSERT INTO t_hr_shift_calendar (
            id, version, shift_id, cal_date, day_type, remark,
            created_at, created_by, updated_at, updated_by
        )
        SELECT u.id, 0, $5, u.cal_date, u.day_type, u.remark, $6, $7, $6, $7
        FROM UNNEST($1::bigint[], $2::date[], $3::text[], $4::text[])
             AS u(id, cal_date, day_type, remark),
             t_hr_shift s
        WHERE s.id = $5 AND s.is_active AND s.deleted_at IS NULL
        ON CONFLICT (shift_id, cal_date) WHERE deleted_at IS NULL DO UPDATE SET
            day_type   = EXCLUDED.day_type,
            remark     = EXCLUDED.remark,
            version    = t_hr_shift_calendar.version + 1,
            updated_at = EXCLUDED.updated_at,
            updated_by = EXCLUDED.updated_by";
    let res = sqlx::query(sql)
        .bind(rows.iter().map(|r| r.id).collect::<Vec<_>>())
        .bind(rows.iter().map(|r| r.cal_date).collect::<Vec<_>>())
        .bind(rows.iter().map(|r| &r.day_type).collect::<Vec<_>>())
        .bind(rows.iter().map(|r| &r.remark).collect::<Vec<_>>())
        .bind(shift_id)
        .bind(now)
        .bind(operator)
        .execute(&mut *conn)
        .await?;
    Ok(res.rows_affected() as i64)
}

/// 按给定日期软删某班次的覆盖行（清完该日回退到全局日历）。
///
/// ⚠️ 班次侧的 `is_active` 守卫同 [`upsert_shift_days`]（经 `FROM t_hr_shift s` 关联），
/// 0 行时由 service 回查归因。
pub async fn soft_delete_shift_days_by_dates(
    conn: &mut PgConnection,
    shift_id: i64,
    dates: &[NaiveDate],
    operator: i64,
    now: NaiveDateTime,
) -> Result<i64, AppError> {
    let sql = format!(
        "UPDATE t_hr_shift_calendar c SET deleted_at = $3, updated_at = $3, updated_by = $4, \
                version = c.version + 1 \
         FROM t_hr_shift s \
         WHERE s.id = $1 AND s.id = c.shift_id AND s.is_active AND s.deleted_at IS NULL \
                AND c.{ALIVE} AND c.cal_date = ANY($2::date[])"
    );
    let res = sqlx::query(AssertSqlSafe(sql))
        .bind(shift_id)
        .bind(dates)
        .bind(now)
        .bind(operator)
        .execute(&mut *conn)
        .await?;
    Ok(res.rows_affected() as i64)
}

/// 按区间软删某班次的覆盖行（两端闭）。
///
/// ⚠️ 班次侧的 `is_active` 守卫同 [`upsert_shift_days`]。
pub async fn soft_delete_shift_days_by_range(
    conn: &mut PgConnection,
    shift_id: i64,
    from: NaiveDate,
    to: NaiveDate,
    operator: i64,
    now: NaiveDateTime,
) -> Result<i64, AppError> {
    let sql = format!(
        "UPDATE t_hr_shift_calendar c SET deleted_at = $4, updated_at = $4, updated_by = $5, \
                version = c.version + 1 \
         FROM t_hr_shift s \
         WHERE s.id = $1 AND s.id = c.shift_id AND s.is_active AND s.deleted_at IS NULL \
                AND c.{ALIVE} AND c.cal_date BETWEEN $2 AND $3"
    );
    let res = sqlx::query(AssertSqlSafe(sql))
        .bind(shift_id)
        .bind(from)
        .bind(to)
        .bind(now)
        .bind(operator)
        .execute(&mut *conn)
        .await?;
    Ok(res.rows_affected() as i64)
}
