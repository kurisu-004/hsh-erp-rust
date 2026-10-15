//! `print_template` 域 HTTP handler：**4 个端点** + 路由表
//!
//! 端点（挂在 `/api/v2/print-templates`，由 [`router`] 桥接）：
//! - `GET  /`                  —— 列表（`?customer_id=` + `limit` / `offset`）
//! - `GET  /resolve`           —— 打印对话框取该 L1 的模板；未绑定 ⇒ `21109`（HTTP 422）
//! - `GET  /{template_id}/content` —— 后端代理 xlsx 字节（**裸字节**，非 `R<T>`）
//! - `POST /{template_id}/delete`  —— 软删（body **必含** `version`）
//!
//! ## ⚠️ 本域**没有写端点**
//!
//! 模板的写入走 `files` 域的统一收文件链路（`POST /files/objects/upload-intents`
//! → 前端 XHR PUT 直传 COS → `POST /files/objects/confirm`），本文件**不持任何
//! 收字节的路由**。「一个 L1 至多一份活跃模板」的替换语义（软删旧行 + INSERT 在
//! 同一事务里）由 confirm 承接，角色闸门 `CUSTOMER ⇒ 仅 Manager` 在
//! `policy::write_roles_for`。见 `docs/api/print_template.md` §1.0 与 §4.2。
//!
//! ## 角色 / 权限守卫（**下沉在 handler 第一行**，仓库惯例）
//!
//! | 端点 | 守卫 | 为什么 |
//! |---|---|---|
//! | `GET /` · `POST /{id}/delete` | `require_role(Manager)` | 模板是**配置**：改它会影响别人出货单上的抬头 / 地址 / 行数与顺序 |
//! | `GET /resolve` · `GET /{id}/content` | 权限点 `delivery_note.print` | **打印对话框是 CLERK / INSPECTOR 在用**（备货、点检、装车都要打单），绝不能 MANAGER-only。该权限点默认矩阵恰是 MANAGER / CLERK / INSPECTOR（`seeds/rbac.sql`），ADMIN 元角色由 `require_permission` 内部短路放行 |
//!
//! service 内**再复核一次**（双层守卫）。`require_role` / `require_permission`
//! 在 `src/auth/rbac.rs::CurrentUser`。
//!
//! ## 事务边界
//!
//! | 端点 | 事务 | 理由 |
//! |---|---|---|
//! | `GET /` · `GET /resolve` | **不开**，走 `pool.acquire()` | 纯读（同 `mail_digest` / `statistics` 范式） |
//! | `GET /{id}/content` | **不开**，走 `pool.acquire()` | COS `get_object` 是外部 IO，包进 PG 事务会让连接被外部依赖长时间占住（与 `files::part_file::preview` 同款取舍） |
//! | `POST /{id}/delete` | `pool.begin()` → `delete_template` → `tx.commit()` → **commit 之后**才 spawn `cos.delete_object` | `files` 域 2026-09-15 review A2 修的「commit 在前、spawn 在后」纪律：避免 commit 失败却已触发 COS 删除、产生孤儿对象 |
//!
//! ## 错误码
//!
//! | 码 | HTTP | 条件 |
//! |---|---|---|
//! | 40300 | 403 | 角色 / 权限点不满足（守卫在 handler 第一行，先于任何 service 调用） |
//! | 20104 | 400 | `customer_id` 缺参 / 非数字（走 `R<T>` 信封，不是纯文本 400） |
//! | 21109 | 422 | `resolve`：该 L1 未绑定（或已软删） |
//! | 21101 | 404 | `content` / `delete`：`template_id` 不存在、或不是 `DELIVERY_TEMPLATE` 行 |
//! | 40901 | 409 | `delete`：OCC `version` 与库里不符 |
//! | 40001 | 422 | **纯文本**：`delete` body 缺 `version`（axum `Json` 提取器层，**不在信封内**） |
//!
//! ⚠️ **`POST /{template_id}/delete` 的 body 反序列化失败不在信封内**：
//! `DeleteTemplateRequest::version` 无 `#[serde(default)]`（CLAUDE.md OCC 铁律）
//! ⇒ 缺字段时 axum `Json` 提取器返 **HTTP 422 纯文本**，前端只能当「请求体不合法」
//! 处理。同款坑在 `files` 域两条写入口（`file_size` 走 `deserialize_i64`、只收
//! JSON 字符串）上也存在，登记在 `files/part_file/mod.rs` 的「前端配套改动清单」。
//!
//! ## ⚠️ matchit 注册顺序：**无冲突**（实测钉死）
//!
//! | 路由 | 段数 | 说明 |
//! |---|---|---|
//! | `/` | 0（nest 根） | 段数不同 |
//! | `/resolve` | 1（**静态**） | 本 router 内**没有** 1 段的 `Path` catch-all（无 `/{template_id}` 端点）⇒ 静态段不会被吞 |
//! | `/{template_id}/content` | 2 | 第 2 段是静态 `content`，第 1 段是捕获段 |
//! | `/{template_id}/delete` | 2 | 与上一行**位置完全平行**、无静态/捕获之争（matchit 同层静态优先，无歧义） |
//!
//! 与「静态段先于 catch-all」那条历史踩坑（`prod/shelf_process` 的
//! `/by-process/{id}`）的差别：那处是**同段数、同位置**的静态 vs 捕获之争，
//! 本域不存在那种形态。**但别只靠读源码判断** —— 集成测试
//! `tests/print_template/resolve.rs::resolve_and_static_paths_are_not_swallowed`
//! 实测钉死。

use std::sync::Arc;

use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::Response,
    routing::{get, post},
};

use crate::auth::rbac::{CurrentUser, Role};
use crate::modules::print_template::dto::{
    DeleteTemplateRequest, ListQuery, ResolveQuery, XLSX_MIME,
};
use crate::modules::print_template::service;
use crate::modules::print_template::vo::PrintTemplateListOut;
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

/// 打印模板端点用的权限点（`seeds/rbac.sql` 里已登记，默认矩阵 M / C / I）。
///
/// ⚠️ 该权限点的描述从「**前端-only**，后端无打印端点，仅登记」改成「打印对话框取
/// 模板 + 取 xlsx 字节的守卫点」，**授权矩阵逐字不变**（仍照前端
/// `deliveryNotePermissions.ts::canPrint` 的 MANAGER / CLERK / INSPECTOR）。
const PRINT_PERMISSION: &str = "delivery_note.print";

// ============================================================================
//  端点 1：GET /
// ============================================================================

/// `GET /api/v2/print-templates` → 200 OK
///
/// query：`customer_id?`（JSON string 形态雪花 id）、`limit?`（缺省 50，clamp
/// `[1,200]`）、`offset?`（缺省 0，`max(0)`）。`total` 不受 `limit` 截断影响。
///
/// 权限：Manager 独占 —— 列表是模板管理页的数据源，含全部客户的抬头信息。
pub async fn list_templates(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(q): Query<ListQuery>,
) -> Result<Json<R<PrintTemplateListOut>>, AppError> {
    current.require_role(Role::Manager)?;
    let customer_id = q.customer_id()?;
    let mut conn = state.pool.acquire().await?;
    let out = service::list_templates(&mut conn, customer_id, q.limit(), q.offset()).await?;
    Ok(Json(R::ok(out)))
}

// ============================================================================
//  端点 2：GET /resolve
// ============================================================================

/// `GET /api/v2/print-templates/resolve?customer_id=<L1>` → 200 OK
///
/// 打印对话框的取模板入口。该客户**未绑定（含已软删）⇒ `21109` / HTTP 422**。
///
/// 权限：权限点 `delivery_note.print`（M / C / I）—— ⚠️ **绝不能 MANAGER-only**：
/// 打送货单是 CLERK / INSPECTOR 的日常动作，管理面与作业面不是同一批人。
pub async fn resolve_template(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(q): Query<ResolveQuery>,
) -> Result<Json<R<crate::modules::print_template::vo::PrintTemplateResolveOut>>, AppError> {
    current.require_permission(PRINT_PERMISSION)?;
    let customer_id = q.require_customer_id()?;
    let mut conn = state.pool.acquire().await?;
    let out = service::resolve_template(&mut conn, customer_id).await?;
    Ok(Json(R::ok(out)))
}

// ============================================================================
//  端点 3：GET /{template_id}/content
// ============================================================================

/// `GET /api/v2/print-templates/{template_id}/content` —— 后端代理 xlsx 字节。
///
/// ⚠️ **返回裸字节**、**不是** `R<T>` 信封（与 `files` 域的
/// `GET /objects/{id}/content` 一致：字节流没法套 JSON 信封）。
///
/// 响应头：
/// - `Content-Type: application/vnd.openxmlformats-officedocument.spreadsheetml.sheet`
///   —— **恒定值**，不用 `t_part_file.content_type` 回显。上传闸门
///   `policy::expected_content_types_for_ext("xlsx")` 只收这一个精确值，
///   所以库里那一列本就只有这一个可能值；写死是为了让「回显型」偏差（files 域
///   已知偏差第 7 / 9 条那一族）不蔓延到本域。
/// - `Content-Length`：字节数。
/// - `Cache-Control: private, max-age=300` —— ⚠️ **比 files 域 `/content` 的
///   1200s 短**：图纸是长期不变的资产，而**模板会被重绑**（`POST /` 就是替换语义）。
///   5 分钟足够吃掉浏览器重复请求，又不会让「换了模板之后老模板还从 HTTP 缓存里
///   冒出来」跨过一个工作时段。
///
/// 权限：权限点 `delivery_note.print`（M / C / I）。⚠️ 刻意与 files 域
/// `GET /objects/{id}/content` 的 5 角色集合**不同** —— 那条放 `SHELF_ACCOUNT`
/// 是为了报工台工控机预览图纸，且它 2026-10-16 起加了 kind 白名单拒掉
/// `DELIVERY_TEMPLATE`；本端点是**打印**语义，工控机账号没有打印送货单的权限。
pub async fn get_template_content(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(template_id): Path<i64>,
) -> Result<Response, AppError> {
    current.require_permission(PRINT_PERMISSION)?;
    let mut conn = state.pool.acquire().await?;
    let out = service::get_template_content(
        &state.part_file_service,
        &mut conn,
        state.cos.clone(),
        &state.config.cos.upload_prefix,
        template_id,
    )
    .await?;
    drop(conn);
    let resp = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, XLSX_MIME)
        .header(header::CONTENT_LENGTH, out.bytes.len())
        .header(header::CACHE_CONTROL, "private, max-age=300")
        .body(Body::from(out.bytes))
        .map_err(|e| AppError::internal(format!("response build: {e}")))?;
    Ok(resp)
}

// ============================================================================
//  端点 4：POST /{template_id}/delete
// ============================================================================

/// `POST /api/v2/print-templates/{template_id}/delete` —— 软删（OCC）。
///
/// 入参（JSON）：`{"version": i32}`。⚠️ `version` 在 `DeleteTemplateRequest`
/// 上**无 `#[serde(default)]`**（CLAUDE.md OCC 铁律）⇒ 缺失时 axum `Json`
/// 提取器返 **HTTP 422 纯文本**，不是 `R<T>` 信封。
///
/// 权限：Manager 独占。删除后 `GET /resolve` 对该客户返 `21109`。
///
/// ⚠️ **commit 之后**才 spawn `cos.delete_object`（files 域 A2 纪律）。
pub async fn delete_template(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(template_id): Path<i64>,
    Json(req): Json<DeleteTemplateRequest>,
) -> Result<Json<R<()>>, AppError> {
    current.require_role(Role::Manager)?;
    let mut tx = state.pool.begin().await?;
    let object_key = service::delete_template(
        &state.part_file_service,
        &mut tx,
        template_id,
        req.version,
        &current,
    )
    .await?;
    tx.commit().await?;
    let cos = state.cos.clone();
    tokio::spawn(async move {
        if let Err(e) = cos.delete_object(&object_key).await {
            tracing::warn!(
                key = %object_key,
                error = %e,
                "print_template COS 异步清理失败（已软删，不影响 API 返回）"
            );
        }
    });
    Ok(Json(R::ok_empty()))
}

// ============================================================================
//  路由表
// ============================================================================

/// 本域 router 注册的全部路由（`METHOD /path`，相对 `/api/v2/print-templates`）。
///
/// 本域端点数（**4**：写入不在本域，见模块 doc）此前只散在
/// `src/modules/print_template/mod.rs` 的端点表、`docs/api/print_template.md` §1、
/// `CLAUDE.md` 三处**手工维护**；本常量是那三处的**唯一真源**，
/// `mod tests::routes_declared_in_router` 逐条比对 `router()` 源码 —— 加 / 删端点
/// 忘同步会当场红（范式同 `files::part_file::handler::ROUTES`，解析器也复用
/// 同一个 `files::route_guard::routes_in_router_source`）。
///
/// 计数口径与全仓一致：一条 `.route(path, m1)` 记 1 条；一条
/// `.route(path, get(h).post(h))` 记 2 条。
pub const ROUTES: &[&str] = &[
    "GET /",
    "GET /resolve",
    "GET /{template_id}/content",
    "POST /{template_id}/delete",
];

/// `print_template` 域 axum 子路由（挂 `/api/v2/print-templates`）。
///
/// ⚠️ `/resolve` **刻意写在前面**（虽然本 router 内无 1 段 catch-all、顺序无关）——
/// 与 CLAUDE.md「路由声明规约」第 9 条的 matchit 硬约束保持同一书写习惯：
/// 静态段先于捕获段。将来若有人加 `GET /{template_id}`，这里不用改也不会炸。
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/", get(list_templates))
        .route("/resolve", get(resolve_template))
        .route("/{template_id}/content", get(get_template_content))
        .route("/{template_id}/delete", post(delete_template))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::Arc;

    use axum::Router;

    use super::{ROUTES, router};
    use crate::modules::files::route_guard::routes_in_router_source;
    use crate::state::AppState;

    /// `ROUTES` 必须与 `router()` 源码**逐条**一致（不多、不少、同序）。
    ///
    /// ⚠️ 复用 `files::route_guard::routes_in_router_source`（**不复制一份**）：
    /// 那份是 files 域两个 router + 本域共用的比对器，`#[cfg(test)] pub(crate)`
    /// 在 crate 内跨模块可达。抄一份会让「解析规则」变成两份可漂移的散文。
    #[test]
    fn routes_declared_in_router() {
        let actual = routes_in_router_source(include_str!("handler.rs"), "router");
        assert_eq!(
            actual.len(),
            ROUTES.len(),
            "`router()` 注册了 {} 条路由，ROUTES 只列了 {} 条（新增端点忘了登记？）\n\
             router(): {:?}\nROUTES: {:?}",
            actual.len(),
            ROUTES.len(),
            actual,
            ROUTES,
        );
        for (got, want) in actual.iter().zip(ROUTES.iter()) {
            assert_eq!(got, want, "`router()` 与 ROUTES 的条目不一致");
        }
    }

    /// 4 条路由无重复（`ROUTES` 里出现重复条目 ⇒ 加端点时抄错行，且会让下面那
    /// 条计数断言失去意义）。
    #[test]
    fn routes_have_no_duplicates() {
        let mut seen = BTreeSet::new();
        for r in ROUTES.iter() {
            assert!(seen.insert(*r), "ROUTES 里 `{r}` 重复了");
        }
        assert!(ROUTES.contains(&"GET /"));
        assert_eq!(
            ROUTES.len(),
            4,
            "本域端点数是 4（写入走 files 域两条写入口，本域不持上传端点）；改端点必须同步改这里与 mod.rs 端点表"
        );
    }

    /// `router()` 本身能构造出来（挡掉「写错了 axum API 签名」这类编译期之外的坑）。
    #[test]
    fn router_builds() {
        let r: Router<Arc<AppState>> = router();
        drop(r);
    }
}
