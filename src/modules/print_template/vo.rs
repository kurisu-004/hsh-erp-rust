//! `print_template` 域端点响应 VO（**2026-10-16 新建**）
//!
//! ⚠️ **刻意不复用 `files::part_file::vo::PartFileOut`**（那是 files 域的出参，
//! 归它所有）：`PartFileOut` 的 `owner_kind` 字段是**从请求参数回显**的
//! （`service::render_out(&row, owner_kind)` 取形参），而本域的 4 条端点
//! **全部**是按 `(part_id, kind='DELIVERY_TEMPLATE')` 取回或列举的 —— 回显不出
//! `CUSTOMER`。
//! 照抄一份 VO 出来、再让 `owner_kind` 变成「要么对、要么空」的字段，只会把
//! files 域那处已登记的偏差（`files/part_file/mod.rs` 已知偏差第 7 / 9 条）复制过来。
//! 本域的 VO 全部**从行上真实字段渲染**，`kind` 恒为 `DELIVERY_TEMPLATE`。
//!
//! 字段取舍：本域只服务「模板管理页」与「打印对话框」两个消费方，故
//! **不**回显 `object_key`（前端拿不到也不需要 COS 原始 key）、**不**回显
//! `upload_status`（写路径恒落 `READY`）、**不**回显 `paired_file_id`
//! （CNC 配对专用，模板恒 NULL）。

use chrono::NaiveDateTime;
use serde::Serialize;

use crate::modules::files::part_file::model::TPartFile;
use crate::shared::types::serialize_i64;

/// 单份送货单打印模板的出参（端点 1 列表 / 端点 2 resolve 复用同一形状；
/// 端点 3 回裸字节、端点 4 无出参，都不用它）。
#[derive(Debug, Clone, Serialize)]
pub struct PrintTemplateOut {
    /// `t_part_file.id`（雪花 ID，序列化为 **string**，防 JS 精度截断）。
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    /// 绑定的一级客户 `t_customer.id`（⚠️ 存放在 `t_part_file.part_id` 列里）。
    #[serde(serialize_with = "serialize_i64")]
    pub customer_id: i64,
    /// 绑定的一级客户**名称**（2026-10-16 review 第 2 轮 C-1 新增）。
    ///
    /// ⚠️ **前端 `printTemplateSchema` 是 `.strict()` 且必填它** —— 缺这个键
    /// 会让上传（201 也一样）、列表（每行）、自动选用**三条路径全部抛 ZodError**，
    /// 即本域对着真后端 100% 不可用。它由本域**自己的一条 `t_customer` 只读查询**
    /// 填（不是从 `t_part_file` JOIN 出来的：那会让「取文件」的端点顺带把客户主数据
    /// 的读取权限变成文件端点的隐式能力）。
    ///
    /// 查不到（客户已软删 / 被删）时为 `None` ⇒ wire 上是 `null`，**不报错**：
    /// 软删闸门只管 `t_part_file`，模板行本身还在，前端展示模板即可。
    /// ⚠️ **它与 [`PrintTemplateResolveOut::customer_name`] 是两个字段、刻意重复**：
    /// 前端在「模板管理列表」读行内这个、在「打印对话框」读 resolve 顶层那个。
    /// 删掉任一个都会再破一次前端；让前端在两处各判一次 null 也不划算（同一份数据、
    /// 同一种查法）。两条取值逐字相同。
    pub customer_name: Option<String>,
    /// 恒为 `"DELIVERY_TEMPLATE"`（服务端钉死，见 `dto::TEMPLATE_KIND`）。
    pub kind: String,
    /// 恒为 `"XLSX"`（`policy::file_type_for_ext("xlsx")` 的推导结果）。
    pub file_type: String,
    /// 上传时的原始文件名（模板管理页列表列显示它）。
    pub original_filename: String,
    /// 字节数（模板管理页可据此提示「这个模板很小，建议排版检查」）。
    pub file_size: i64,
    /// 恒为 xlsx 的规范 MIME（上传闸门只收精确值，故出参也只有一个可能值）。
    pub content_type: String,
    /// 内容 SHA-256 hex（64 字符小写；`t_part_file.content_sha256`）。
    pub content_sha256: Option<String>,
    /// OCC 锚点：`POST /{id}/delete` 的 body 必须回传它（CLAUDE.md §8）。
    pub version: i32,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

impl PrintTemplateOut {
    /// `TPartFile` 行 → 出参。**只读行上的真实列**，不做任何参数回显。
    ///
    /// ⚠️ `customer_name` **不是行上的列**（`t_part_file` 没有这一列），由调用方
    /// 用本域自己的 `t_customer` 查询填进来 —— 本方法刻意不做 DB 访问，
    /// 因此它对「列表 N 行」是 O(N) 的纯内存映射（见 `service::list_templates`）。
    pub fn from_row(row: &TPartFile, customer_name: Option<String>) -> Self {
        Self {
            id: row.id,
            customer_id: row.part_id,
            customer_name,
            kind: row.kind.clone(),
            file_type: row.file_type.clone(),
            original_filename: row.original_filename.clone(),
            file_size: row.file_size,
            content_type: row.content_type.clone(),
            content_sha256: row.content_sha256.clone(),
            version: row.version,
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }
}

/// `GET /print-templates/resolve` 的出参：模板本体 + 该一级客户的名称。
///
/// ⚠️ `customer_name` 来自**本域自己的一条 `t_customer` 只读查询**，不是从
/// `t_part_file` JOIN 出来的（那会让一个「取文件」的端点顺带把客户主数据的读取
/// 权限变成文件端点的隐式能力）。客户已软删 / 被删时为 `None`，**不**报错 ——
/// 模板行本身还在（软删闸门只管 `t_part_file`），此时前端展示模板即可，不必因
/// 客户改名 / 下架而打不开打印对话框。
/// ⚠️ **与 [`PrintTemplateOut::customer_name`] 刻意重复**（同一条查询、同一种
/// 查法、取值逐字相同）：打印对话框读的是**顶层**这个（前端 schema 已按
/// `{template, customer_name}` 嵌套写死），模板管理列表读的是**行内**那个。
/// 两个都别删。
#[derive(Debug, Clone, Serialize)]
pub struct PrintTemplateResolveOut {
    pub template: PrintTemplateOut,
    /// 一级客户名称；查不到（不存在 / 已软删）时为 `null`。
    pub customer_name: Option<String>,
}

/// `GET /print-templates` 列表出参（纯 list，无信封嵌套层）。
///
/// ⚠️ `total` **不受** `limit` 截断影响（独立 COUNT，与
/// `PartFileService::list_files` 逐字同款），前端据此渲染「共 N 份」。
#[derive(Debug, Clone, Serialize)]
pub struct PrintTemplateListOut {
    pub items: Vec<PrintTemplateOut>,
    pub total: i64,
}
