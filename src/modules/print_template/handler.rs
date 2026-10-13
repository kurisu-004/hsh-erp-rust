//! `print_template` 域 HTTP handler：5 个端点 + 路由表（**2026-10-16 新建**）
//!
//! 端点（挂在 `/api/v2/print-templates`，由 [`router`] 桥接）：
//! - `POST /`                  —— multipart 上传并**替换**该 L1 的模板（**201 Created**）
//! - `GET  /`                  —— 列表（`?customer_id=` + `limit` / `offset`）
//! - `GET  /resolve`           —— 打印对话框取该 L1 的模板；未绑定 ⇒ `21109`（HTTP 422）
//! - `GET  /{template_id}/content` —— 后端代理 xlsx 字节（**裸字节**，非 `R<T>`）
//! - `POST /{template_id}/delete`  —— 软删（body **必含** `version`）
//!
//! ## 角色 / 权限守卫（**下沉在 handler 第一行**，仓库惯例）
//!
//! | 端点 | 守卫 | 为什么 |
//! |---|---|---|
//! | `POST /` · `GET /` · `POST /{id}/delete` | `require_role(Manager)` | 模板是**配置**：改它会影响别人出货单上的抬头 / 地址 / 行数与顺序 |
//! | `GET /resolve` · `GET /{id}/content` | 权限点 `delivery_note.print` | **打印对话框是 CLERK / INSPECTOR 在用**（备货、点检、装车都要打单），绝不能 MANAGER-only。该权限点默认矩阵恰是 MANAGER / CLERK / INSPECTOR（`seeds/rbac.sql`），ADMIN 元角色由 `require_permission` 内部短路放行 |
//!
//! service 内**再复核一次**（双层守卫）。`require_role` / `require_permission`
//! 在 `src/auth/rbac.rs::CurrentUser`。
//!
//! ## 事务边界
//!
//! | 端点 | 事务 | 理由 |
//! |---|---|---|
//! | `POST /` | `pool.begin()` → `upload_template` → `tx.commit()` | **必须**有：「先软删旧行 + 再写新行」共用一个 tx，否则第二步失败会留下空洞（见 `service.rs` 模块 doc） |
//! | `GET /` · `GET /resolve` | **不开**，走 `pool.acquire()` | 纯读（同 `mail_digest` / `statistics` 范式） |
//! | `GET /{id}/content` | **不开**，走 `pool.acquire()` | COS `get_object` 是外部 IO，包进 PG 事务会让连接被外部依赖长时间占住（与 `files::part_file::preview` 同款取舍） |
//! | `POST /{id}/delete` | `pool.begin()` → `delete_template` → `tx.commit()` → **commit 之后**才 spawn `cos.delete_object` | `files` 域 2026-09-15 review A2 修的「commit 在前、spawn 在后」纪律：避免 commit 失败却已触发 COS 删除、产生孤儿对象 |
//!
//! ## multipart 契约（端点 1）
//!
//! body（`multipart/form-data`）：
//! - `data`：**必填文本字段**，`UploadTemplateData` 的 JSON —— `{"customer_id":"<雪花 id>"}`
//! - `file`：**必填二进制字段**（xlsx）
//!
//! ⚠️ multipart 循环的写法逐字照抄 `files::part_file::handler::upload_part_file`
//! （`field.name()` 分发 + `field.text()` / `field.bytes()` + 其余字段 drain 掉），
//! 但**只有两个字段**：本域前端只走这一次性 multipart，**不扩展 STS 两段式**
//! （没有 `POST /confirm` 对应端点，见 `docs/api/print_template.md` §8.3 第 3 条）。
//!
//! ⚠️ `data` 里**只有 `customer_id` 一个键**：`owner_kind` / `kind` 由服务端钉死
//! （`dto::OWNER_KIND` / `dto::TEMPLATE_KIND`），不接受客户端指定 —— 与
//! `files` 域通用上传原语的 `UploadFileData` 恰好相反，理由见 `dto.rs`。
//!
//! ## 错误码（端点 1，按校验顺序，先命中先返回）
//!
//! | 码 | HTTP | 条件 |
//! |---|---|---|
//! | 40300 | 403 | 角色不在 M / C / I 内（**守卫先于 multipart 解析**） |
//! | 20104 | 400 | 缺 `data` / 缺 `file` / 缺文件名 / `customer_id` 非数字 / **客户不存在 · 已软删 · 不是一级客户** |
//! | 40001 | 422 | **0 字节**（空文件是「无效输入」，不是「太大」，同 `files` 域取舍） |
//! | 21102 | 400 | 扩展名非 xlsx / **前 4 字节不是 `PK\x03\x04`** |
//! | 21103 | 400 | 超过 `config.cos.max_file_size`（`COS_MAX_FILE_SIZE`） |
//! | 21105 | 404 | owner 不存在（`upload_file_for_owner` 内的**兜底**；正常流程走不到） |
//! | 21108 | 400 | 撞 `uk_t_part_file_single`（并发双传同一 L1 的极端形态） |
//!
//! ## ⚠️ matchit 注册顺序：**无冲突**（逐条核对过）
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
    extract::{Multipart, Path, Query, State},
    http::{StatusCode, header},
    response::Response,
    routing::{get, post},
};

use crate::auth::rbac::{CurrentUser, Role};
use crate::modules::print_template::dto::{
    DeleteTemplateRequest, ListQuery, ResolveQuery, UploadTemplateData, XLSX_MIME,
    assert_xlsx_filename, has_zip_magic,
};
use crate::modules::print_template::service;
use crate::modules::print_template::vo::{PrintTemplateListOut, PrintTemplateOut};
use crate::shared::error::{AppError, code};
use crate::shared::response::R;
use crate::state::AppState;

/// 打印模板端点用的权限点（`seeds/rbac.sql` 里已登记，默认矩阵 M / C / I）。
///
/// ⚠️ 该权限点 2026-10-16 前的描述是「**前端-only**，后端无打印端点，仅登记」，
/// 本轮起它**真的**守后端两条端点了（`/resolve` + `/{id}/content`）——
/// 授权矩阵**逐字不变**（仍照前端 `deliveryNotePermissions.ts::canPrint` 的
/// MANAGER / CLERK / INSPECTOR），只改描述文字。
const PRINT_PERMISSION: &str = "delivery_note.print";

// ============================================================================
//  端点 1：POST /
// ============================================================================

/// `POST /api/v2/print-templates` → **201 Created**
///
/// multipart：`data`（JSON 文本字段 `{"customer_id":"<雪花 id>"}`）+ `file`（xlsx 字节）。
/// 语义是**替换**：该 L1 先有的模板被软删，新模板生效（`uk_t_part_file_single`
/// 的硬要求，顺序理由见 `service.rs` 模块 doc）。
///
/// 权限：Manager 独占（守卫在**读 multipart 之前**，避免未授权请求触发整包内存分配；
/// service 内再复核一次）。⚠️ 不是权限点：模板是配置，写它要有管理面。
pub async fn upload_template(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<R<PrintTemplateOut>>), AppError> {
    current.require_role(Role::Manager)?;

    let mut data_json: Option<String> = None;
    let mut file_bytes: Option<Vec<u8>> = None;
    let mut file_name: Option<String> = None;
    let mut file_content_type: Option<String> = None;

    // ⚠️ 循环写法逐字照抄 `files::part_file::handler::upload_part_file`：
    //    按 `field.name()` 分发，未识别的字段 drain 掉忽略（不报错 ——
    //    multipart 里多塞一个字段不该让整个请求失败）。
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::validation(format!("multipart 解析失败: {e}")))?
    {
        match field.name().unwrap_or("") {
            "data" => {
                data_json = Some(
                    field
                        .text()
                        .await
                        .map_err(|e| AppError::validation(format!("data 字段读取失败: {e}")))?,
                );
            }
            "file" => {
                file_name = field.file_name().map(|s| s.to_string());
                file_content_type = field.content_type().map(|s| s.to_string());
                let bytes = field
                    .bytes()
                    .await
                    .map_err(|e| AppError::validation(format!("file 字段读取失败: {e}")))?;
                file_bytes = Some(bytes.to_vec());
            }
            _ => {
                let _ = field.bytes().await;
            }
        }
    }

    let data_json =
        data_json.ok_or_else(|| AppError::biz(code::BIZ_INVALID_VALUE, "缺少 data 字段"))?;
    let data: UploadTemplateData = serde_json::from_str(&data_json)
        .map_err(|e| AppError::biz(code::BIZ_INVALID_VALUE, format!("JSON 解析失败: {e}")))?;
    let customer_id = data.parse_customer_id()?;
    let bytes =
        file_bytes.ok_or_else(|| AppError::biz(code::BIZ_INVALID_VALUE, "缺少 file 字段"))?;
    let filename = file_name.ok_or_else(|| AppError::biz(code::BIZ_INVALID_VALUE, "缺少文件名"))?;
    let content_type = file_content_type
        .unwrap_or_else(|| crate::modules::print_template::dto::XLSX_MIME.to_string());

    // ---- ⚠️ 本域自己的四道轻校验（**在调 service 之前**、零事务零 COS 零 DB）----
    //
    // 顺序即错误码优先级：0 字节 → 扩展名 → 大小 → ZIP magic。
    // ⚠️ **第 4 道只挡「改名成 .xlsx 的非 zip」**：xlsx 结构合法性由前端已有的
    // `assertTemplateMatches` 把关，后端**刻意不引入 zip crate**（理由与代价见
    // `dto.rs` 末尾）。魔法字节就是 zip 的前 4 字节 `PK\x03\x04`。
    if bytes.is_empty() {
        return Err(AppError::validation("模板字节为空"));
    }
    assert_xlsx_filename(&filename)?;
    if bytes.len() > state.config.cos.max_file_size {
        return Err(AppError::biz(
            code::BIZ_PART_FILE_TOO_LARGE,
            format!(
                "模板 {} bytes 超过上限 {}（{}MB）",
                bytes.len(),
                state.config.cos.max_file_size,
                state.config.cos.max_file_size / 1024 / 1024
            ),
        ));
    }
    if !has_zip_magic(&bytes) {
        return Err(AppError::biz(
            code::BIZ_PART_FILE_BAD_TYPE,
            format!(
                "送货单打印模板必须是 xlsx（zip 容器），但 {} 的前 4 字节不是 PK\\x03\\x04 —— 可能只是把别的文件改名成了 .xlsx",
                filename
            ),
        ));
    }

    let mut tx = state.pool.begin().await?;
    let out = service::upload_template(
        &state.part_file_service,
        &mut tx,
        &state.config.cos.upload_prefix,
        state.config.cos.max_file_size,
        customer_id,
        &filename,
        &content_type,
        bytes,
        &current,
    )
    .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(R::ok(out))))
}

// ============================================================================
//  端点 2：GET /
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
//  端点 3：GET /resolve
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
//  端点 4：GET /{template_id}/content
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
        &current,
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
//  端点 5：POST /{template_id}/delete
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
/// 本域端点数（5）此前只散在 `src/modules/print_template/mod.rs` 的端点表、
/// `docs/api/print_template.md` §1、`CLAUDE.md` 三处**手工维护**；本常量是那
/// 三处的**唯一真源**，`mod tests::routes_declared_in_router` 逐条比对 `router()`
/// 源码 —— 加 / 删端点忘同步会当场红（范式同 `files::part_file::handler::ROUTES`，
/// 解析器也复用同一个 `files::route_guard::routes_in_router_source`）。
///
/// 计数口径与全仓一致：一条 `.route(path, m1)` 记 1 条；一条
/// `.route(path, get(h).post(h))` 记 2 条。
pub const ROUTES: &[&str] = &[
    // ⚠️ 顺序按 `routes_in_router_source` 的解析结果：`.route("/", get(h).post(h))`
    //    记「GET 先、POST 后」。mod.rs 的端点表按**语义**排（上传在前），
    //    两处顺序不同是刻意的 —— 这里必须跟解析器走。
    "GET /",
    "POST /",
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
        .route("/", get(list_templates).post(upload_template))
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
    /// 那份是 files 域三个 router 共用的比对器，`#[cfg(test)] pub(crate)`
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

    /// 5 条路由无重复；`/` 的 GET / POST 各占一条（读列表 vs 上传替换是
    /// 两个语义完全不同的动作，重复登记会掩盖其中一条被删掉）。
    #[test]
    fn routes_have_no_duplicates() {
        let mut seen = BTreeSet::new();
        for r in ROUTES.iter() {
            assert!(seen.insert(*r), "ROUTES 里 `{r}` 重复了");
        }
        assert!(ROUTES.contains(&"GET /"));
        assert!(ROUTES.contains(&"POST /"));
        assert_eq!(
            ROUTES.len(),
            5,
            "本域端点数是 5；改端点必须同步改这里与 mod.rs 端点表"
        );
    }

    /// `router()` 本身能构造出来（挡掉「写错了 axum API 签名」这类编译期之外的坑）。
    #[test]
    fn router_builds() {
        let r: Router<Arc<AppState>> = router();
        drop(r);
    }

    /// ZIP magic 判据本身：`PK\x03\x04` 放行、其余拒、短于 4 字节拒。
    #[test]
    fn zip_magic_accepts_only_pk_header() {
        use crate::modules::print_template::dto::has_zip_magic;
        assert!(has_zip_magic(b"PK\x03\x04rest-of-zip"));
        assert!(!has_zip_magic(b"%PDF-1.5\n%%EOF\n"));
        assert!(!has_zip_magic(b"PK\x03"));
        assert!(!has_zip_magic(b""));
    }

    /// 扩展名守卫：只收 `xlsx`（大小写不敏感），其余一律 21102。
    #[test]
    fn filename_guard_only_accepts_xlsx() {
        use crate::modules::print_template::dto::assert_xlsx_filename;
        assert!(assert_xlsx_filename("模板.xlsx").is_ok());
        assert!(assert_xlsx_filename("TEMPLATE.XLSX").is_ok());
        for bad in ["模板.xls", "模板.pdf", "模板", "模板.xlsx.exe"] {
            let err = assert_xlsx_filename(bad).expect_err(bad);
            assert_eq!(err.code(), 21102, "{bad} 应得 21102");
        }
    }
}
