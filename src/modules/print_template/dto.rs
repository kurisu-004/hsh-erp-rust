//! `print_template` 域 DTO：入参请求体 + multipart 的 `data` 字段
//!
//! 出参 VO 另放 [`super::vo`]（与 `files::part_file` / `mail_digest` 两域的取舍不同：
//! 本域入参只有 3 个小结构、出参有 3 个，分开放可读性更好）。
//!
//! ## i64 一律是 JSON string
//!
//! 本仓无 DB ENUM、无物理外键，雪花主键出参按 `shared::types::serialize_i64`
//! 序列化成 **string**（防 JS 精度截断，见 CLAUDE.md「DB 约定」最后几条）。
//! 入参侧同理：**雪花 id 必须以字符串形式提交**（`customer_id`），
//! 非数字字符串走 `20104 BIZ_INVALID_VALUE`（不是 422 —— 与 `files` 域
//! `POST /objects` 的 `owner_id` 处理逐字一致）。

use serde::{Deserialize, Serialize};

use crate::modules::files::part_file::policy;
use crate::shared::error::{AppError, code};

/// `GET /` 列表：缺省单页条数。
pub const LIST_LIMIT_DEFAULT: i64 = 50;
/// `GET /` 列表：单页条数上限（与 `PartFileService::list_files` 的 clamp 同档）。
pub const LIST_LIMIT_MAX: i64 = 200;

// ============================================================================
//  multipart `data` 字段
// ============================================================================

/// `POST /` 的 multipart 文本字段 `data` 的 JSON 体。
///
/// ⚠️ **只带 `customer_id` 一个键** —— `owner_kind` / `kind` 刻意**不**由客户端给：
/// 本域只有「一级客户的送货单打印模板」这一种东西，两者由服务端钉死
/// （`"CUSTOMER"` / `"DELIVERY_TEMPLATE"`，见 [`super::service`]）。
/// 与 `POST /files/objects` 的 `UploadFileData` 恰好相反：那边是个通用上传原语，
/// 这边是单一用途端点，把两个字段暴露给 client 只会多出「传错组合」的失败形态。
///
/// `customer_id` 必须是**一级客户**的 id（JSON string 形态的雪花 id）。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UploadTemplateData {
    /// 一级客户 `t_customer.id`，JSON **string** 形态。
    pub customer_id: String,
}

impl UploadTemplateData {
    /// 解析 `customer_id` → `i64`（非数字 ⇒ 20104）。
    ///
    /// 刻意**不**用 `deserialize_i64` 那类自动转换：那类 helper 收数字 / 字符串两
    /// 种形态，而本仓的约定是雪花 id 出入参统一 string（`shared::types`），
    /// 这里与 `files::part_file::handler::upload_part_file` 的 `owner_id` 逐字同款。
    pub fn parse_customer_id(&self) -> Result<i64, AppError> {
        self.customer_id
            .parse::<i64>()
            .map_err(|_| {
                AppError::biz(
                    code::BIZ_INVALID_VALUE,
                    format!("customer_id 非法: {:?}", self.customer_id),
                )
            })
            .and_then(|id| {
                if id <= 0 {
                    return Err(AppError::biz(
                        code::BIZ_INVALID_VALUE,
                        format!("customer_id 非法（须为正雪花 id）: {id}"),
                    ));
                }
                Ok(id)
            })
    }
}

// ============================================================================
//  查询 / 路径 / 请求体
// ============================================================================

/// `GET /print-templates` 列表的 query 参数（三项**全部可选**）。
///
/// ⚠️ `customer_id` 是 `Option<String>` 而**不是** `Option<i64>`：非法值走
/// `20104` 业务信封（而不是 axum `Query` 提取器层的**纯文本 400**），
/// 与 `dashboard` 域那条「非法 query 也要走 `R<T>` 信封」的纪律一致。
#[derive(Debug, Clone, Deserialize)]
pub struct ListQuery {
    /// 按一级客户 id 过滤（JSON string 形态的雪花 id）。
    pub customer_id: Option<String>,
    /// 单页条数，缺省 50，clamp `[1, 200]`。
    pub limit: Option<i64>,
    /// 偏移，缺省 0，`max(0)`。
    pub offset: Option<i64>,
}

impl ListQuery {
    pub fn customer_id(&self) -> Result<Option<i64>, AppError> {
        self.customer_id
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(|s| {
                s.parse::<i64>().map_err(|_| {
                    AppError::biz(code::BIZ_INVALID_VALUE, format!("customer_id 非法: {s:?}"))
                })
            })
            .transpose()
    }

    pub fn limit(&self) -> i64 {
        self.limit
            .unwrap_or(LIST_LIMIT_DEFAULT)
            .clamp(1, LIST_LIMIT_MAX)
    }

    pub fn offset(&self) -> i64 {
        self.offset.unwrap_or(0).max(0)
    }
}

/// `GET /print-templates/resolve` 的 query 参数（`customer_id` **必填**）。
///
/// 缺参走 20104（**不是** 422 纯文本）：本端点的调用方是前端打印对话框，
/// 它需要拿到 `{code, message}` 才能弹「该客户还没配模板，去绑定或手动上传」的提示。
#[derive(Debug, Clone, Deserialize)]
pub struct ResolveQuery {
    /// 一级客户 `t_customer.id`，JSON **string** 形态的雪花 id。
    pub customer_id: Option<String>,
}

impl ResolveQuery {
    pub fn require_customer_id(&self) -> Result<i64, AppError> {
        let raw = self.customer_id.as_deref().unwrap_or("").trim();
        if raw.is_empty() {
            return Err(AppError::biz(
                code::BIZ_INVALID_VALUE,
                "缺少 customer_id（须为一级客户的雪花 id，JSON string 形态）",
            ));
        }
        raw.parse::<i64>().map_err(|_| {
            AppError::biz(
                code::BIZ_INVALID_VALUE,
                format!("customer_id 非法: {raw:?}"),
            )
        })
    }
}

/// `POST /print-templates/{template_id}/delete` 的请求体（OCC）。
///
/// ⚠️ **`version` 无 `#[serde(default)]`** —— CLAUDE.md OCC 铁律：所有对带 `version`
/// 列的表的写端点，body 必须显式收客户端 `version` 且必填。缺失时由 axum `Json`
/// 提取器返 **HTTP 422 纯文本**（**不是** `R<T>` 信封），前端只能把它当
/// 「请求体不合法」处理。
#[derive(Debug, Clone, Deserialize)]
pub struct DeleteTemplateRequest {
    /// OCC 锚点（`t_part_file.version`）。与库里不符 ⇒ `40901 VERSION_CONFLICT`。
    pub version: i32,
}

// ============================================================================
//  常量：kind / owner_kind / MIME
// ============================================================================

/// 本域写侧的 `owner_kind`（**服务端钉死**，不接受客户端指定）。
pub const OWNER_KIND: &str = "CUSTOMER";

/// 本域写侧的 `kind`（**服务端钉死**）。
///
/// ⚠️ 与 `files::part_file::policy` 里那几张表的字面量同源，改一处必须四处同步
/// （migration `ck_t_part_file_kind` / `uk_t_part_file_single` 白名单 /
/// `policy::allowed_exts` / 本常量）。真正的单一真相源是 DB 的 CHECK 约束。
pub const TEMPLATE_KIND: &str = "DELIVERY_TEMPLATE";

/// xlsx 的规范 MIME（`GET /{id}/content` 的 `Content-Type` 响应头用）。
///
/// 与 `policy::expected_content_types_for_ext("xlsx")` 的返回值逐字一致 ——
/// 上传闸门**只收**这个精确值，所以回包也只能是它。
pub const XLSX_MIME: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";

/// ZIP 容器的前 4 字节（xlsx 就是个 zip）—— `PK\x03\x04`。
///
/// ⚠️ **只挡「改名成 `.xlsx` 的非 zip」**，不解析 zip 结构（后端刻意**不引入
/// zip crate**，见本文件末尾的取舍说明）。
pub const ZIP_MAGIC: [u8; 4] = [0x50, 0x4B, 0x03, 0x04];

/// 判字节是否以 ZIP magic 开头。
///
/// 只看**前 4 字节**，且要求字节长度 ≥ 4（0 字节由上一道 40001 挡下，这里也
/// 独立成立，不依赖调用顺序）。
pub fn has_zip_magic(bytes: &[u8]) -> bool {
    bytes.len() >= ZIP_MAGIC.len() && bytes[..ZIP_MAGIC.len()] == ZIP_MAGIC
}

/// 校验扩展名必须是 `xlsx`（否则 21102 `BIZ_PART_FILE_BAD_TYPE`）。
///
/// 复用 `policy::ext_of`（`Path::extension`，大小写不敏感、自动取最后一段）。
/// ⚠️ 这是**本域的第一道格式守卫**：`policy::allowed_exts(TEMPLATE_KIND)` 也会
/// 拒非 xlsx（那是在 `upload_file_for_owner` 内部、更晚一步），本函数让拒绝
/// 发生在**调 service 之前** —— 少一次事务往返、少一次 COS 交互。
pub fn assert_xlsx_filename(filename: &str) -> Result<(), AppError> {
    match policy::ext_of(filename) {
        Some(ext) if ext == "xlsx" => Ok(()),
        Some(ext) => Err(AppError::biz(
            code::BIZ_PART_FILE_BAD_TYPE,
            format!("送货单打印模板只支持 .xlsx，收到扩展名 {ext:?}（filename={filename:?}）"),
        )),
        None => Err(AppError::biz(
            code::BIZ_PART_FILE_BAD_TYPE,
            format!("送货单打印模板缺少扩展名（filename={filename:?}）"),
        )),
    }
}

// ============================================================================
//  取舍说明（为什么不引入 zip crate）
// ============================================================================
//
// **后端刻意不做 xlsx 内部结构校验**（不引入 `zip` / `calamine` 之类 crate）。
//
// 判据是**分工**：xlsx 内部结构（有没有 `xl/workbook.xml`、命名空间对不对、
// 模板区域（`print_area` / `print_titles`）是否被 hucre 认得）**只有渲染方知道
// 答案**，而渲染方是前端的 `assertTemplateMatches`（已在用）。后端能廉价提供的
// 只有「这至少是个 zip 容器」这一层 —— 那正是 `has_zip_magic` 做的。
//
// 代价与收益的平衡：
// - 收益：零新依赖、零 CVE 面、零「后端解压一个用户上传的 zip」的 zip-bomb 面。
// - 代价：一个把 `.xlsx` 内部结构改坏的模板能**存进库**（报错点后移到前端渲染时）。
//
// ⚠️ **不要**在后人「补全 xlsx 校验」时顺手把校验加到上传端点上、又在不同时机
// 放宽 —— 那是两个失败面。若将来真要在后端加结构校验，应当**先**与前端
// `assertTemplateMatches` 对齐判据清单，登记在 `docs/api/print_template.md` §8.3。
