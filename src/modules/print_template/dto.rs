//! `print_template` 域 DTO：入参请求体（query / body）
//!
//! 出参 VO 另放 [`super::vo`]（与 `files::part_file` / `mail_digest` 两域的取舍不同：
//! 本域入参只有 3 个小结构、出参有 3 个，分开放可读性更好）。
//!
//! ⚠️ 2026-10-15：上传端点下线，本域随之不再有「multipart `data` 字段」与
//! **ZIP magic / xlsx 扩展名**这两道后端校验 —— 后端 confirm 拿不到字节，
//! 「这份 xlsx 是不是真 xlsx」改由前端 `assertTemplateMatches` 承担。
//! 完整取舍见本文件末尾。
//!
//! ## i64 一律是 JSON string
//!
//! 本仓无 DB ENUM、无物理外键，雪花主键出参按 `shared::types::serialize_i64`
//! 序列化成 **string**（防 JS 精度截断，见 CLAUDE.md「DB 约定」最后几条）。
//! 入参侧同理：**雪花 id 必须以字符串形式提交**（`customer_id`），
//! 非数字字符串走 `20104 BIZ_INVALID_VALUE`（不是 422 —— 与 `files` 域
//! `POST /objects` 的 `owner_id` 处理逐字一致）。

use serde::Deserialize;

use crate::shared::error::{AppError, code};

/// `GET /` 列表：缺省单页条数。
pub const LIST_LIMIT_DEFAULT: i64 = 50;
/// `GET /` 列表：单页条数上限（与 `PartFileService::list_files` 的 clamp 同档）。
pub const LIST_LIMIT_MAX: i64 = 200;

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

// ============================================================================
//  取舍说明：xlsx 内容校验为什么不在后端（2026-10-15）
// ============================================================================
//
// **后端不做任何 xlsx 内容校验** —— 连「前 4 字节是不是 `PK\x03\x04`」那道
// 也没有了。
//
// 原因：统一上传链路后，字节由前端直传 COS、后端 confirm 只拿到 `head`
// （size / etag），**拿不到内容**。ZIP magic 判据与「是不是真 xlsx」这类问题在
// 后端**物理上无法回答**（除非把整份字节再拉回来，那正是本次要消除的代理上传）。
//
// 职责移交前端：前端在选文件时用 hucre 逐格解析（`assertTemplateMatches`），
// 解析不过就不发起 upload-intents ⇒ 后端不会收到「不是 xlsx 的 .xlsx」。
//
// ⚠️ **这是有意弱化，代价要说清**：绕过前端直接打
// `POST /files/objects/upload-intents` + `confirm` 的话，一份非 xlsx 的
// `.xlsx` 能存进库，报错点后移到前端渲染时。登记在
// `docs/api/print_template.md`。⚠️ 不要在后人「补全 xlsx 校验」时又在**另一个**
// 时机加回来 —— 那会造出第二个失败面；若将来要后端校验结构，必须与前端
// `assertTemplateMatches` 对齐判据清单后再定。
