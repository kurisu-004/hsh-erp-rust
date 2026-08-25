//! 应用错误类型 + 错误码常量段
//! 对应 Python myERP/core/error_code.py + exception*.py
//!
//! ## 错误码分段契约（与 Python 前端保持兼容）
//! - `0`          成功
//! - `4xxxx`      HTTP 语义（40000 BAD_REQUEST、40001 VALIDATION、40100 UNAUTHORIZED、
//!   40101 BIZ_AUTH_INVALID、40102 TOKEN_EXPIRED、40103 REFRESH_INVALID、40104 OLD_PASSWORD_MISMATCH、
//!   40300 FORBIDDEN、40301 SHELF_MISMATCH、40400 NOT_FOUND、40901 VERSION_CONFLICT、41301 REQUEST_TOO_LARGE）
//!   Auth 业务码与通用 UNAUTHORIZED 的区别：业务码携带细分原因。
//! - `5xxxx`      系统错误（50000 INTERNAL、50001 DATABASE）
//! - `2xxxx`      业务域错误：200xx 用户/订单、201xx 零件/客户、202xx 工人、203xx 装配体、
//!   204xx 图纸文件、205xx 货架、206xx 账号、208xx 工序、209xx 工种、210xx 申请人、
//!   211xx 零件文件、212xx 外协公司、213xx 外协报价、214xx 送货单、215xx 外协发货。
//!
//! ### 与 Python 的差异（冲突解决记录）
//! - `20109` 在 Python 中被 `BIZ_PART_BATCH_NOT_FOUND` 与 `BIZ_CUSTOMER_IN_USE` 双重占用。
//!   Rust 中 `20109` 保留给 `BIZ_PART_BATCH_NOT_FOUND`，`BIZ_CUSTOMER_IN_USE` 移到新槽位 `20113`。
//! - `21110 BIZ_DELIVERY_PARTS_MULTIPLE_CUSTOMERS` 与 `21407 BIZ_DELIVERY_NOTE_PARTS_MULTIPLE_CUSTOMERS` 同义。
//!   Rust 以 `21407` 为正典，`21110` 保留为 `#[deprecated]` 别名（编译期 = 21407），便于未迁移的调用点平滑收敛。
//!
//! 业务实现阶段直接 `AppError::biz(code::BIZ_..., "...")`；常量在 `code` 模块内集中维护。

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use thiserror::Error;

/// 错误码常量（沿用 Python 数字契约）
pub mod code {
    pub const SUCCESS: i32 = 0;

    // HTTP 语义
    pub const BAD_REQUEST: i32 = 40000;
    pub const VALIDATION_ERROR: i32 = 40001;
    pub const UNAUTHORIZED: i32 = 40100;

    // Auth 域业务码（401xx，HTTP 语义层里的业务码，区别于通用 40100 UNAUTHORIZED）
    pub const BIZ_AUTH_INVALID: i32 = 40101;        // 登录：用户不存在/已删/已停用/密码错统一
    pub const TOKEN_EXPIRED: i32 = 40102;
    pub const REFRESH_INVALID: i32 = 40103;         // refresh token 失效/版本不匹配/用户停用
    pub const OLD_PASSWORD_MISMATCH: i32 = 40104;   // 修改密码时旧密码错误

    pub const FORBIDDEN: i32 = 40300;

    // 货架越权
    pub const SHELF_MISMATCH: i32 = 40301;

    pub const NOT_FOUND: i32 = 40400;
    pub const VERSION_CONFLICT: i32 = 40901;
    pub const REQUEST_TOO_LARGE: i32 = 41301;

    // ===== 2xxxx 业务域错误（沿用 Python core/error_code.py 数值契约） =====

    // 200xx 用户 / 订单
    pub const BIZ_USER_NOT_FOUND: i32 = 20001;
    pub const BIZ_USER_DUPLICATE: i32 = 20002;
    pub const BIZ_ORDER_NOT_FOUND: i32 = 20003;

    // 201xx 零件 / 客户 / 序列号
    pub const BIZ_PART_NOT_FOUND: i32 = 20101;
    pub const BIZ_CUSTOMER_NOT_FOUND: i32 = 20102;
    pub const BIZ_INVALID_TRANSITION: i32 = 20103;
    pub const BIZ_INVALID_VALUE: i32 = 20104;
    pub const BIZ_PART_SERIAL_EXHAUSTED: i32 = 20105;            // 序列号池耗尽（>5000 活跃/PREFIX）
    pub const BIZ_SERIAL_PREFIX_UNKNOWN: i32 = 20108;            // t_serial_counter 找不到对应 prefix
    pub const BIZ_PART_BATCH_NOT_FOUND: i32 = 20109;             // 批次不存在 / 不属于该工单（Python 中此值曾被双重占用，详见模块 docstring）
    pub const BIZ_PART_PRICE_LOCKED_BY_ASSEMBLY: i32 = 20110;    // 父装配体已设总价，子件不能再单独改价
    pub const BIZ_PART_BATCH_INVALID_QUANTITY: i32 = 20111;      // 拆分/部分流转数量非法（≤0 或超过批次量）
    pub const BIZ_PART_QUANTITY_LOCKED: i32 = 20112;             // 已拆分或已流转的工单禁止改总量
    pub const BIZ_CUSTOMER_IN_USE: i32 = 20113;                  // 客户仍被 part/assembly 引用 → 拒软删（Python 原 20109，新槽位独占）

    // 202xx 工人
    pub const BIZ_WORKER_NOT_FOUND: i32 = 20201;
    pub const BIZ_WORKER_INACTIVE: i32 = 20202;
    pub const BIZ_WORKER_IN_USE: i32 = 20203;                    // 还有 part.current_holder_id 指向 → 拒停用
    pub const BIZ_WORKER_HOLD_LIMIT_EXCEEDED: i32 = 20204;       // 工种 max_held_batches 上限触顶 → 拒领取

    // 203xx 装配体
    pub const BIZ_ASSEMBLY_NOT_FOUND: i32 = 20301;
    pub const BIZ_ASSEMBLY_BAD_CUSTOMER: i32 = 20302;            // 客户节点不允许（一级集团 / 不存在）
    pub const BIZ_ASSEMBLY_TOO_MANY_CHILDREN: i32 = 20303;       // 子件 > 99，序列号 {serial}-{i:02d} 派生失败

    // 204xx 图纸文件（t_drawing_file + COS）
    pub const BIZ_DRAWING_FILE_NOT_FOUND: i32 = 20401;
    pub const BIZ_DRAWING_FILE_BAD_TYPE: i32 = 20402;            // 扩展名不在 COS_ALLOWED_TYPES 白名单
    pub const BIZ_DRAWING_FILE_TOO_LARGE: i32 = 20403;           // 文件大小 ≤0 或 > cos_max_file_size_bytes
    pub const BIZ_DRAWING_UPLOAD_FAILED: i32 = 20404;            // COS SDK 抛错

    // 205xx 货架（t_shelf）
    pub const BIZ_SHELF_NOT_FOUND: i32 = 20501;
    pub const BIZ_SHELF_DUPLICATE_CODE: i32 = 20502;
    pub const BIZ_SHELF_IN_USE: i32 = 20503;                     // 还有 IN_PROCESS/INSPECTION 零件 → 拒软删
    pub const BIZ_SHELF_PROCESS_SHELF_NOT_FOUND: i32 = 20504;    // 货架不存在
    pub const BIZ_SHELF_PROCESS_PROCESS_NOT_FOUND: i32 = 20505;  // 工序不存在
    pub const BIZ_SHELF_NO_MATCH_FOR_PROCESS: i32 = 20506;       // 没有 active 货架映射指定 process
    pub const BIZ_SHELF_PROCESS_NOT_MAPPED: i32 = 20507;         // 货架未映射该工序

    // 206xx 账号（t_user / t_user_role）—— Python 命名 BIZ_* 长名为正典
    pub const BIZ_USER_ACCOUNT_NOT_FOUND: i32 = 20601;
    pub const BIZ_USER_DUPLICATE_USERNAME: i32 = 20602;
    pub const BIZ_USER_INACTIVE: i32 = 20603;
    pub const BIZ_USER_ROLE_DUPLICATE: i32 = 20604;
    pub const BIZ_USER_ROLE_NOT_FOUND: i32 = 20605;
    pub const BIZ_USER_NO_ROLE: i32 = 20606;
    // 既有代码用的短别名（保留兼容，值与上面长名相等）
    pub const USER_NOT_FOUND: i32 = BIZ_USER_ACCOUNT_NOT_FOUND;
    pub const DUPLICATE_USERNAME: i32 = BIZ_USER_DUPLICATE_USERNAME;
    pub const ROLE_DUPLICATE: i32 = BIZ_USER_ROLE_DUPLICATE;
    pub const ROLE_NOT_FOUND: i32 = BIZ_USER_ROLE_NOT_FOUND;
    pub const NO_ROLE: i32 = BIZ_USER_NO_ROLE;

    // 208xx 工序（t_process）
    pub const BIZ_PROCESS_NOT_FOUND: i32 = 20801;
    pub const BIZ_PROCESS_DUPLICATE_CODE: i32 = 20802;
    pub const BIZ_PROCESS_IN_USE: i32 = 20803;                   // 仍有 part.next_process_id 或 mapping 引用时拒软删

    // 209xx 工种（t_work_type）
    pub const BIZ_WORK_TYPE_NOT_FOUND: i32 = 20901;
    pub const BIZ_WORK_TYPE_DUPLICATE_CODE: i32 = 20902;
    pub const BIZ_WORK_TYPE_IN_USE: i32 = 20903;                 // 仍有 worker.work_type_id 或 mapping 引用时拒软删

    // 210xx 申请人（t_applicant）
    pub const BIZ_APPLICANT_NOT_FOUND: i32 = 21001;
    pub const BIZ_APPLICANT_DUPLICATE_NAME: i32 = 21002;         // 同一一级客户下重名（DB partial unique 兜底）
    pub const BIZ_APPLICANT_BAD_CUSTOMER: i32 = 21003;           // customer 不存在或不是一级
    pub const BIZ_APPLICANT_IN_USE: i32 = 21004;                 // 被 part.applicant_name 引用 → 拒软删

    // 211xx 零件文件（t_part_file，统一 5 类；含送货模板相关码）
    pub const BIZ_PART_FILE_NOT_FOUND: i32 = 21101;
    pub const BIZ_PART_FILE_BAD_TYPE: i32 = 21102;               // 扩展名与 kind 不匹配
    pub const BIZ_PART_FILE_TOO_LARGE: i32 = 21103;              // 文件大小 ≤0 或 > cos_max_file_size_bytes
    pub const BIZ_PART_FILE_UPLOAD_FAILED: i32 = 21104;          // COS SDK 抛错
    pub const BIZ_PART_FILE_OWNER_NOT_FOUND: i32 = 21105;        // polymorphic owner (part/assembly) 不存在
    pub const BIZ_PART_FILE_DUPLICATE: i32 = 21108;              // 同 part+kind+content_sha256 撞唯一索引（并发兜底）
    pub const BIZ_DELIVERY_TEMPLATE_NOT_CONFIGURED: i32 = 21109; // root prefix 未配置 DELIVERY_NOTE_TEMPLATE_BY_PREFIX
    pub const BIZ_DELIVERY_PART_STATUS_INVALID: i32 = 21111;     // 所选零件状态非 READY_TO_SHIP
    pub const BIZ_DELIVERY_TEMPLATE_TOO_MANY_PARTS: i32 = 21112; // 所选零件超过模板容量（法 14 / 路 25）
    pub const BIZ_DELIVERY_PRINT_BAD_ORDER: i32 = 21113;         // custom_order 含非法 batch id 或漏行（422）
    // 21110 → 21407 的 deprecated 别名：旧调用点收敛后移除（详见模块 docstring）
    #[deprecated(
        since = "0.1.0",
        note = "改用 BIZ_DELIVERY_NOTE_PARTS_MULTIPLE_CUSTOMERS (21407)；21110 是旧别名，已统一到 21407"
    )]
    pub const BIZ_DELIVERY_PARTS_MULTIPLE_CUSTOMERS: i32 =
        BIZ_DELIVERY_NOTE_PARTS_MULTIPLE_CUSTOMERS;

    // 212xx 外协公司（t_outsource_company + t_outsource_company_process）
    pub const BIZ_OUTSOURCE_COMPANY_NOT_FOUND: i32 = 21201;
    pub const BIZ_OUTSOURCE_COMPANY_DUPLICATE: i32 = 21202;      // uk_t_outsource_company_name 部分唯一兜底
    pub const BIZ_OUTSOURCE_COMPANY_BAD_PROCESS: i32 = 21203;    // 工序不存在 / 不是 OUTSOURCE/INHOUSE 类别
    pub const BIZ_OUTSOURCE_PROCESS_NOT_MAPPED: i32 = 21204;     // 公司未映射该 OUTSOURCE 工序
    pub const BIZ_OUTSOURCE_COMPANY_IN_USE: i32 = 21205;         // 被 part OUTSOURCE 引用 / 仍映射工序
    pub const BIZ_PART_NOT_OUTSOURCEABLE: i32 = 21206;           // 当前状态不允许发送外协（兜底，正常流不该撞）
    pub const BIZ_OUTSOURCE_DIRECT_REQUIRES_C2_SHELF: i32 = 21207; // 直接发送外协要求零件位于绑定了外协工序的货架
    pub const BIZ_OUTSOURCE_NO_SHELF: i32 = 21208;               // 系统无任何绑定了外协工序的货架

    // 213xx 外协报价（t_outsource_quote）
    pub const BIZ_OUTSOURCE_QUOTE_NOT_FOUND: i32 = 21301;
    pub const BIZ_OUTSOURCE_QUOTE_INVALID_TRANSITION: i32 = 21302;  // 当前状态不允许此操作
    pub const BIZ_OUTSOURCE_QUOTE_DUPLICATE: i32 = 21303;           // 同 (part,company,process) 已存在活跃报价
    pub const BIZ_OUTSOURCE_QUOTE_NOT_APPROVED: i32 = 21307;        // 找不到该 tuple 的 APPROVED 报价

    // 214xx 送货单（t_delivery_note）
    pub const BIZ_DELIVERY_NOTE_NOT_FOUND: i32 = 21401;             // 找不到指定的送货单
    pub const BIZ_DELIVERY_NOTE_INVALID_TRANSITION: i32 = 21402;    // 当前状态不允许此操作
    pub const BIZ_DELIVERY_NOTE_NOT_DRAFT: i32 = 21403;             // 非 DRAFT 状态不能 soft_delete
    pub const BIZ_DELIVERY_NOTE_NOT_SUBMITTED: i32 = 21404;         // 非 SUBMITTED 状态不能 recall / pickup
    pub const BIZ_DELIVERY_NOTE_PART_NOT_READY: i32 = 21405;        // 零件状态非 READY_TO_SHIP
    pub const BIZ_DELIVERY_NOTE_PART_ALREADY_ASSIGNED: i32 = 21406; // 零件已在另一张送货单上（提升为 409：状态冲突）
    pub const BIZ_DELIVERY_NOTE_PARTS_MULTIPLE_CUSTOMERS: i32 = 21407; // 同一单内混客户（与老 21110 同义）
    pub const BIZ_DELIVERY_NOTE_SCAN_MISMATCH: i32 = 21408;         // 扫码的 serial_no 不在本单范围内
    pub const BIZ_DELIVERY_NOTE_DRIVER_INVALID: i32 = 21409;        // 司机非送货司机 / 不活跃
    pub const BIZ_DELIVERY_NOTE_SCAN_INCOMPLETE: i32 = 21410;       // pickup 时还没扫齐
    pub const BIZ_DELIVERY_NOTE_INVALID_VALUE: i32 = 21411;         // 空单 / 等其他非法入参
    pub const BIZ_DELIVERY_NOTE_PARTS_LOCKED: i32 = 21412;          // SUBMITTED/PICKED_UP 后禁止 add_parts / remove_parts
    pub const BIZ_DELIVERY_GROUP_NOT_FOUND: i32 = 21413;            // 找不到指定的送货分组 / 已软删
    pub const BIZ_DELIVERY_GROUP_DUPLICATE_NAME: i32 = 21414;       // 同 L1 下分组重名
    pub const BIZ_DELIVERY_GROUP_MEMBER_CONFLICT: i32 = 21415;      // L2 已属于其他活跃分组
    pub const BIZ_DELIVERY_NOTE_SCOPE_MISMATCH: i32 = 21416;        // 零件分类与送货单范围不符（add_parts 校验）
    pub const BIZ_DELIVERY_SCAN_UNKNOWN_CODE: i32 = 21417;          // 扫码的 serial_no 无法识别
    pub const BIZ_DELIVERY_ASSEMBLY_PARTS_NOT_READY: i32 = 21418;   // 装配件整套拒绝：含不可入单子件（message 附明细）
    pub const BIZ_DELIVERY_NOTE_DRAFT_SCOPE_CONFLICT: i32 = 21419;  // recall 时同范围已存在 DRAFT

    // 215xx 外协发货（t_outsource_shipment）
    pub const BIZ_OUTSOURCE_SHIPMENT_NOT_FOUND: i32 = 21501;

    // 系统错误
    pub const INTERNAL: i32 = 50000;
    pub const DATABASE: i32 = 50001;
}

#[derive(Debug, Error)]
pub enum AppError {
    /// 业务错误：自定义错误码 + 自定义 HTTP 状态码
    #[error("[{code}] {message}")]
    Biz {
        code: i32,
        message: String,
        http: StatusCode,
    },

    /// 业务错误（带失败明细列表，用于装配件整套拒绝 21418 等）。
    ///
    /// `IntoResponse` 会把 `failures` 序列化进 `R::err.data` 字段，结构：
    /// ```jsonc
    /// { "failures": [ {"serial_no": "...", "name": "...", "reason": "..."}, ... ] }
    /// ```
    /// `message` 仍按既有约定继续携带人读文案（设计 §5：「message 含全部失败子件」）。
    ///
    /// `failures` 用 `serde_json::Value` 而非具体 DTO 是为了**避免** `shared::error`
    /// 反向依赖具体业务域的 DTO 类型。
    #[error("[{code}] {message}")]
    BizWithFailures {
        code: i32,
        message: String,
        http: StatusCode,
        failures: Vec<serde_json::Value>,
    },

    /// 校验失败（40001，HTTP 422）
    #[error("校验失败: {0}")]
    Validation(String),

    /// 未授权（40100，HTTP 401）
    #[error("未授权: {0}")]
    Unauthorized(String),

    /// 禁止访问（40300，HTTP 403）
    #[error("禁止访问")]
    Forbidden,

    /// JWT 错误（40100，HTTP 401）
    #[error("JWT: {0}")]
    Jwt(String),

    /// 数据库错误（50001，HTTP 500）
    #[error("数据库错误")]
    Database(#[from] sqlx::Error),

    /// 内部错误（50000，HTTP 500）
    #[error("内部错误: {0}")]
    Internal(String),
}

impl AppError {
    /// 构造业务错误（HTTP 状态码由 code 自动推导）
    pub fn biz(code: i32, message: impl Into<String>) -> Self {
        Self::Biz {
            code,
            message: message.into(),
            http: status_from_code(code),
        }
    }

    /// 构造业务错误并显式指定 HTTP 状态码（绕过 `status_from_code` 自动推导）
    ///
    /// 何时使用：
    /// - 错误码不在 `status_from_code` 表里，需要指定非常规 HTTP 状态
    /// - 想覆盖表里的默认值（如 `BIZ_DELIVERY_PRINT_BAD_ORDER` 强制 422）
    ///
    /// 表驱动场景优先用 [`AppError::biz`]。
    pub fn biz_with_status(
        code: i32,
        message: impl Into<String>,
        http_status: StatusCode,
    ) -> Self {
        Self::Biz {
            code,
            message: message.into(),
            http: http_status,
        }
    }

    pub fn validation(msg: impl Into<String>) -> Self {
        Self::Validation(msg.into())
    }

    pub fn unauthorized(msg: impl Into<String>) -> Self {
        Self::Unauthorized(msg.into())
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        Self::Internal(msg.into())
    }

    pub fn code(&self) -> i32 {
        match self {
            Self::Biz { code, .. } | Self::BizWithFailures { code, .. } => *code,
            Self::Validation(_) => code::VALIDATION_ERROR,
            Self::Unauthorized(_) => code::UNAUTHORIZED,
            Self::Forbidden => code::FORBIDDEN,
            Self::Jwt(_) => code::UNAUTHORIZED,
            Self::Database(_) => code::DATABASE,
            Self::Internal(_) => code::INTERNAL,
        }
    }

    pub fn http_status(&self) -> StatusCode {
        match self {
            Self::Biz { http, .. } | Self::BizWithFailures { http, .. } => *http,
            Self::Validation(_) => StatusCode::UNPROCESSABLE_ENTITY,
            Self::Unauthorized(_) | Self::Jwt(_) => StatusCode::UNAUTHORIZED,
            Self::Forbidden => StatusCode::FORBIDDEN,
            Self::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

/// 由错误码推导 HTTP 状态码（与 Python 错误码分段一致）
fn status_from_code(c: i32) -> StatusCode {
    match c {
        c if c == code::BAD_REQUEST => StatusCode::BAD_REQUEST,
        c if c == code::VALIDATION_ERROR => StatusCode::UNPROCESSABLE_ENTITY,
        c if c == code::UNAUTHORIZED => StatusCode::UNAUTHORIZED,
        c if c == code::TOKEN_EXPIRED => StatusCode::UNAUTHORIZED,
        c if c == code::BIZ_AUTH_INVALID
            || c == code::REFRESH_INVALID
            || c == code::OLD_PASSWORD_MISMATCH => StatusCode::UNAUTHORIZED,
        c if c == code::SHELF_MISMATCH => StatusCode::FORBIDDEN,
        c if c == code::USER_NOT_FOUND || c == code::ROLE_NOT_FOUND => StatusCode::NOT_FOUND,
        c if c == code::DUPLICATE_USERNAME || c == code::ROLE_DUPLICATE => StatusCode::CONFLICT,
        c if c == code::NO_ROLE => StatusCode::FORBIDDEN,
        c if c == code::FORBIDDEN => StatusCode::FORBIDDEN,
        c if c == code::NOT_FOUND => StatusCode::NOT_FOUND,
        c if c == code::VERSION_CONFLICT => StatusCode::CONFLICT,
        c if c == code::REQUEST_TOO_LARGE => StatusCode::PAYLOAD_TOO_LARGE,
        c if c == code::INTERNAL || c == code::DATABASE => StatusCode::INTERNAL_SERVER_ERROR,

        // ---- 2xxxx 业务码：404 (资源缺失) ----
        c if c == code::BIZ_USER_NOT_FOUND
            || c == code::BIZ_ORDER_NOT_FOUND
            || c == code::BIZ_PART_NOT_FOUND
            || c == code::BIZ_CUSTOMER_NOT_FOUND
            || c == code::BIZ_PART_BATCH_NOT_FOUND
            || c == code::BIZ_WORKER_NOT_FOUND
            || c == code::BIZ_ASSEMBLY_NOT_FOUND
            || c == code::BIZ_DRAWING_FILE_NOT_FOUND
            || c == code::BIZ_SHELF_NOT_FOUND
            || c == code::BIZ_SHELF_PROCESS_SHELF_NOT_FOUND
            || c == code::BIZ_SHELF_PROCESS_PROCESS_NOT_FOUND
            || c == code::BIZ_PROCESS_NOT_FOUND
            || c == code::BIZ_WORK_TYPE_NOT_FOUND
            || c == code::BIZ_APPLICANT_NOT_FOUND
            || c == code::BIZ_PART_FILE_NOT_FOUND
            || c == code::BIZ_PART_FILE_OWNER_NOT_FOUND
            || c == code::BIZ_OUTSOURCE_COMPANY_NOT_FOUND
            || c == code::BIZ_OUTSOURCE_QUOTE_NOT_FOUND
            || c == code::BIZ_DELIVERY_NOTE_NOT_FOUND
            || c == code::BIZ_DELIVERY_GROUP_NOT_FOUND
            || c == code::BIZ_DELIVERY_SCAN_UNKNOWN_CODE
            || c == code::BIZ_OUTSOURCE_SHIPMENT_NOT_FOUND => StatusCode::NOT_FOUND,

        // ---- 2xxxx 业务码：409 (状态冲突 / 重复 / 占用 / 锁) ----
        c if c == code::BIZ_USER_DUPLICATE
            || c == code::BIZ_USER_DUPLICATE_USERNAME
            || c == code::BIZ_USER_ROLE_DUPLICATE
            || c == code::BIZ_PART_PRICE_LOCKED_BY_ASSEMBLY
            || c == code::BIZ_PART_QUANTITY_LOCKED
            || c == code::BIZ_CUSTOMER_IN_USE
            || c == code::BIZ_WORKER_IN_USE
            || c == code::BIZ_WORKER_HOLD_LIMIT_EXCEEDED
            || c == code::BIZ_SHELF_DUPLICATE_CODE
            || c == code::BIZ_SHELF_IN_USE
            || c == code::BIZ_PROCESS_DUPLICATE_CODE
            || c == code::BIZ_PROCESS_IN_USE
            || c == code::BIZ_WORK_TYPE_DUPLICATE_CODE
            || c == code::BIZ_WORK_TYPE_IN_USE
            || c == code::BIZ_APPLICANT_DUPLICATE_NAME
            || c == code::BIZ_APPLICANT_IN_USE
            || c == code::BIZ_PART_FILE_DUPLICATE
            || c == code::BIZ_OUTSOURCE_COMPANY_DUPLICATE
            || c == code::BIZ_OUTSOURCE_COMPANY_IN_USE
            || c == code::BIZ_OUTSOURCE_QUOTE_DUPLICATE
            || c == code::BIZ_DELIVERY_NOTE_PART_ALREADY_ASSIGNED
            || c == code::BIZ_DELIVERY_NOTE_PARTS_LOCKED
            || c == code::BIZ_DELIVERY_GROUP_DUPLICATE_NAME
            || c == code::BIZ_DELIVERY_GROUP_MEMBER_CONFLICT
            || c == code::BIZ_DELIVERY_NOTE_DRAFT_SCOPE_CONFLICT => StatusCode::CONFLICT,

        // ---- 2xxxx 业务码：422 (校验类，Python 显式声明 21113 → 422) ----
        c if c == code::BIZ_DELIVERY_PRINT_BAD_ORDER => StatusCode::UNPROCESSABLE_ENTITY,

        // ---- 2xxxx 兜底：Python BizError 默认 400 ----
        c if c == code::BIZ_DELIVERY_NOTE_SCOPE_MISMATCH
            || c == code::BIZ_DELIVERY_ASSEMBLY_PARTS_NOT_READY => StatusCode::BAD_REQUEST,

        // ---- 2xxxx 兜底：Python BizError 默认 400 ----
        c if (20000..30000).contains(&c) => StatusCode::BAD_REQUEST,

        c if (40000..50000).contains(&c) => StatusCode::BAD_REQUEST,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = self.http_status();
        // 不向客户端泄露 sqlx 内部信息
        let message = match &self {
            AppError::Database(e) => {
                tracing::error!(error = %e, "数据库错误");
                "数据库错误".to_string()
            }
            other => other.to_string(),
        };
        // 直接拼装 JSON body（不走泛型 `R<T>`，因为 BizWithFailures 需要序列化
        // 非 `()` 的 `data`；这里手动构造，与 `R<T>` 序列化后的字段约定一致）。
        let body: serde_json::Value = match &self {
            AppError::BizWithFailures {
                code, failures, ..
            } => serde_json::json!({
                "code": code,
                "message": &message,
                "data": serde_json::json!({ "failures": failures }),
            }),
            other => serde_json::json!({
                "code": other.code(),
                "message": &message,
                "data": serde_json::Value::Null,
            }),
        };
        (status, Json(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    /// 所有"正典"常量（用于唯一性 + HTTP 表覆盖）。
    /// 故意排除：
    /// - `BIZ_DELIVERY_PARTS_MULTIPLE_CUSTOMERS` (21110)：deprecated 别名，与 21407 值相等
    /// - `USER_NOT_FOUND` / `DUPLICATE_USERNAME` / `ROLE_DUPLICATE` / `ROLE_NOT_FOUND` / `NO_ROLE`：
    ///   206xx 段的短别名，与各自 `BIZ_*` 长名值相等
    const NON_DEPRECATED_CODES: &[(i32, &str)] = &[
        // 0
        (code::SUCCESS, "SUCCESS"),
        // 4xxxx
        (code::BAD_REQUEST, "BAD_REQUEST"),
        (code::VALIDATION_ERROR, "VALIDATION_ERROR"),
        (code::UNAUTHORIZED, "UNAUTHORIZED"),
        (code::BIZ_AUTH_INVALID, "BIZ_AUTH_INVALID"),
        (code::TOKEN_EXPIRED, "TOKEN_EXPIRED"),
        (code::REFRESH_INVALID, "REFRESH_INVALID"),
        (code::OLD_PASSWORD_MISMATCH, "OLD_PASSWORD_MISMATCH"),
        (code::FORBIDDEN, "FORBIDDEN"),
        (code::SHELF_MISMATCH, "SHELF_MISMATCH"),
        (code::NOT_FOUND, "NOT_FOUND"),
        (code::VERSION_CONFLICT, "VERSION_CONFLICT"),
        (code::REQUEST_TOO_LARGE, "REQUEST_TOO_LARGE"),
        // 5xxxx
        (code::INTERNAL, "INTERNAL"),
        (code::DATABASE, "DATABASE"),
        // 206xx 长名（短别名见 constants_match_python_values）
        (code::BIZ_USER_ACCOUNT_NOT_FOUND, "BIZ_USER_ACCOUNT_NOT_FOUND"),
        (code::BIZ_USER_DUPLICATE_USERNAME, "BIZ_USER_DUPLICATE_USERNAME"),
        (code::BIZ_USER_INACTIVE, "BIZ_USER_INACTIVE"),
        (code::BIZ_USER_ROLE_DUPLICATE, "BIZ_USER_ROLE_DUPLICATE"),
        (code::BIZ_USER_ROLE_NOT_FOUND, "BIZ_USER_ROLE_NOT_FOUND"),
        (code::BIZ_USER_NO_ROLE, "BIZ_USER_NO_ROLE"),
        // 200xx
        (code::BIZ_USER_NOT_FOUND, "BIZ_USER_NOT_FOUND"),
        (code::BIZ_USER_DUPLICATE, "BIZ_USER_DUPLICATE"),
        (code::BIZ_ORDER_NOT_FOUND, "BIZ_ORDER_NOT_FOUND"),
        // 201xx
        (code::BIZ_PART_NOT_FOUND, "BIZ_PART_NOT_FOUND"),
        (code::BIZ_CUSTOMER_NOT_FOUND, "BIZ_CUSTOMER_NOT_FOUND"),
        (code::BIZ_INVALID_TRANSITION, "BIZ_INVALID_TRANSITION"),
        (code::BIZ_INVALID_VALUE, "BIZ_INVALID_VALUE"),
        (code::BIZ_PART_SERIAL_EXHAUSTED, "BIZ_PART_SERIAL_EXHAUSTED"),
        (code::BIZ_SERIAL_PREFIX_UNKNOWN, "BIZ_SERIAL_PREFIX_UNKNOWN"),
        (code::BIZ_PART_BATCH_NOT_FOUND, "BIZ_PART_BATCH_NOT_FOUND"),
        (code::BIZ_PART_PRICE_LOCKED_BY_ASSEMBLY, "BIZ_PART_PRICE_LOCKED_BY_ASSEMBLY"),
        (code::BIZ_PART_BATCH_INVALID_QUANTITY, "BIZ_PART_BATCH_INVALID_QUANTITY"),
        (code::BIZ_PART_QUANTITY_LOCKED, "BIZ_PART_QUANTITY_LOCKED"),
        (code::BIZ_CUSTOMER_IN_USE, "BIZ_CUSTOMER_IN_USE"),
        // 202xx
        (code::BIZ_WORKER_NOT_FOUND, "BIZ_WORKER_NOT_FOUND"),
        (code::BIZ_WORKER_INACTIVE, "BIZ_WORKER_INACTIVE"),
        (code::BIZ_WORKER_IN_USE, "BIZ_WORKER_IN_USE"),
        (code::BIZ_WORKER_HOLD_LIMIT_EXCEEDED, "BIZ_WORKER_HOLD_LIMIT_EXCEEDED"),
        // 203xx
        (code::BIZ_ASSEMBLY_NOT_FOUND, "BIZ_ASSEMBLY_NOT_FOUND"),
        (code::BIZ_ASSEMBLY_BAD_CUSTOMER, "BIZ_ASSEMBLY_BAD_CUSTOMER"),
        (code::BIZ_ASSEMBLY_TOO_MANY_CHILDREN, "BIZ_ASSEMBLY_TOO_MANY_CHILDREN"),
        // 204xx
        (code::BIZ_DRAWING_FILE_NOT_FOUND, "BIZ_DRAWING_FILE_NOT_FOUND"),
        (code::BIZ_DRAWING_FILE_BAD_TYPE, "BIZ_DRAWING_FILE_BAD_TYPE"),
        (code::BIZ_DRAWING_FILE_TOO_LARGE, "BIZ_DRAWING_FILE_TOO_LARGE"),
        (code::BIZ_DRAWING_UPLOAD_FAILED, "BIZ_DRAWING_UPLOAD_FAILED"),
        // 205xx
        (code::BIZ_SHELF_NOT_FOUND, "BIZ_SHELF_NOT_FOUND"),
        (code::BIZ_SHELF_DUPLICATE_CODE, "BIZ_SHELF_DUPLICATE_CODE"),
        (code::BIZ_SHELF_IN_USE, "BIZ_SHELF_IN_USE"),
        (code::BIZ_SHELF_PROCESS_SHELF_NOT_FOUND, "BIZ_SHELF_PROCESS_SHELF_NOT_FOUND"),
        (code::BIZ_SHELF_PROCESS_PROCESS_NOT_FOUND, "BIZ_SHELF_PROCESS_PROCESS_NOT_FOUND"),
        (code::BIZ_SHELF_NO_MATCH_FOR_PROCESS, "BIZ_SHELF_NO_MATCH_FOR_PROCESS"),
        (code::BIZ_SHELF_PROCESS_NOT_MAPPED, "BIZ_SHELF_PROCESS_NOT_MAPPED"),
        // 208xx
        (code::BIZ_PROCESS_NOT_FOUND, "BIZ_PROCESS_NOT_FOUND"),
        (code::BIZ_PROCESS_DUPLICATE_CODE, "BIZ_PROCESS_DUPLICATE_CODE"),
        (code::BIZ_PROCESS_IN_USE, "BIZ_PROCESS_IN_USE"),
        // 209xx
        (code::BIZ_WORK_TYPE_NOT_FOUND, "BIZ_WORK_TYPE_NOT_FOUND"),
        (code::BIZ_WORK_TYPE_DUPLICATE_CODE, "BIZ_WORK_TYPE_DUPLICATE_CODE"),
        (code::BIZ_WORK_TYPE_IN_USE, "BIZ_WORK_TYPE_IN_USE"),
        // 210xx
        (code::BIZ_APPLICANT_NOT_FOUND, "BIZ_APPLICANT_NOT_FOUND"),
        (code::BIZ_APPLICANT_DUPLICATE_NAME, "BIZ_APPLICANT_DUPLICATE_NAME"),
        (code::BIZ_APPLICANT_BAD_CUSTOMER, "BIZ_APPLICANT_BAD_CUSTOMER"),
        (code::BIZ_APPLICANT_IN_USE, "BIZ_APPLICANT_IN_USE"),
        // 211xx（21110 deprecated 别名除外）
        (code::BIZ_PART_FILE_NOT_FOUND, "BIZ_PART_FILE_NOT_FOUND"),
        (code::BIZ_PART_FILE_BAD_TYPE, "BIZ_PART_FILE_BAD_TYPE"),
        (code::BIZ_PART_FILE_TOO_LARGE, "BIZ_PART_FILE_TOO_LARGE"),
        (code::BIZ_PART_FILE_UPLOAD_FAILED, "BIZ_PART_FILE_UPLOAD_FAILED"),
        (code::BIZ_PART_FILE_OWNER_NOT_FOUND, "BIZ_PART_FILE_OWNER_NOT_FOUND"),
        (code::BIZ_PART_FILE_DUPLICATE, "BIZ_PART_FILE_DUPLICATE"),
        (code::BIZ_DELIVERY_TEMPLATE_NOT_CONFIGURED, "BIZ_DELIVERY_TEMPLATE_NOT_CONFIGURED"),
        (code::BIZ_DELIVERY_PART_STATUS_INVALID, "BIZ_DELIVERY_PART_STATUS_INVALID"),
        (code::BIZ_DELIVERY_TEMPLATE_TOO_MANY_PARTS, "BIZ_DELIVERY_TEMPLATE_TOO_MANY_PARTS"),
        (code::BIZ_DELIVERY_PRINT_BAD_ORDER, "BIZ_DELIVERY_PRINT_BAD_ORDER"),
        // 212xx
        (code::BIZ_OUTSOURCE_COMPANY_NOT_FOUND, "BIZ_OUTSOURCE_COMPANY_NOT_FOUND"),
        (code::BIZ_OUTSOURCE_COMPANY_DUPLICATE, "BIZ_OUTSOURCE_COMPANY_DUPLICATE"),
        (code::BIZ_OUTSOURCE_COMPANY_BAD_PROCESS, "BIZ_OUTSOURCE_COMPANY_BAD_PROCESS"),
        (code::BIZ_OUTSOURCE_PROCESS_NOT_MAPPED, "BIZ_OUTSOURCE_PROCESS_NOT_MAPPED"),
        (code::BIZ_OUTSOURCE_COMPANY_IN_USE, "BIZ_OUTSOURCE_COMPANY_IN_USE"),
        (code::BIZ_PART_NOT_OUTSOURCEABLE, "BIZ_PART_NOT_OUTSOURCEABLE"),
        (code::BIZ_OUTSOURCE_DIRECT_REQUIRES_C2_SHELF, "BIZ_OUTSOURCE_DIRECT_REQUIRES_C2_SHELF"),
        (code::BIZ_OUTSOURCE_NO_SHELF, "BIZ_OUTSOURCE_NO_SHELF"),
        // 213xx
        (code::BIZ_OUTSOURCE_QUOTE_NOT_FOUND, "BIZ_OUTSOURCE_QUOTE_NOT_FOUND"),
        (code::BIZ_OUTSOURCE_QUOTE_INVALID_TRANSITION, "BIZ_OUTSOURCE_QUOTE_INVALID_TRANSITION"),
        (code::BIZ_OUTSOURCE_QUOTE_DUPLICATE, "BIZ_OUTSOURCE_QUOTE_DUPLICATE"),
        (code::BIZ_OUTSOURCE_QUOTE_NOT_APPROVED, "BIZ_OUTSOURCE_QUOTE_NOT_APPROVED"),
        // 214xx
        (code::BIZ_DELIVERY_NOTE_NOT_FOUND, "BIZ_DELIVERY_NOTE_NOT_FOUND"),
        (code::BIZ_DELIVERY_NOTE_INVALID_TRANSITION, "BIZ_DELIVERY_NOTE_INVALID_TRANSITION"),
        (code::BIZ_DELIVERY_NOTE_NOT_DRAFT, "BIZ_DELIVERY_NOTE_NOT_DRAFT"),
        (code::BIZ_DELIVERY_NOTE_NOT_SUBMITTED, "BIZ_DELIVERY_NOTE_NOT_SUBMITTED"),
        (code::BIZ_DELIVERY_NOTE_PART_NOT_READY, "BIZ_DELIVERY_NOTE_PART_NOT_READY"),
        (code::BIZ_DELIVERY_NOTE_PART_ALREADY_ASSIGNED, "BIZ_DELIVERY_NOTE_PART_ALREADY_ASSIGNED"),
        (code::BIZ_DELIVERY_NOTE_PARTS_MULTIPLE_CUSTOMERS, "BIZ_DELIVERY_NOTE_PARTS_MULTIPLE_CUSTOMERS"),
        (code::BIZ_DELIVERY_NOTE_SCAN_MISMATCH, "BIZ_DELIVERY_NOTE_SCAN_MISMATCH"),
        (code::BIZ_DELIVERY_NOTE_DRIVER_INVALID, "BIZ_DELIVERY_NOTE_DRIVER_INVALID"),
        (code::BIZ_DELIVERY_NOTE_SCAN_INCOMPLETE, "BIZ_DELIVERY_NOTE_SCAN_INCOMPLETE"),
        (code::BIZ_DELIVERY_NOTE_INVALID_VALUE, "BIZ_DELIVERY_NOTE_INVALID_VALUE"),
        (code::BIZ_DELIVERY_NOTE_PARTS_LOCKED, "BIZ_DELIVERY_NOTE_PARTS_LOCKED"),
        (code::BIZ_DELIVERY_GROUP_NOT_FOUND, "BIZ_DELIVERY_GROUP_NOT_FOUND"),
        (code::BIZ_DELIVERY_GROUP_DUPLICATE_NAME, "BIZ_DELIVERY_GROUP_DUPLICATE_NAME"),
        (code::BIZ_DELIVERY_GROUP_MEMBER_CONFLICT, "BIZ_DELIVERY_GROUP_MEMBER_CONFLICT"),
        (code::BIZ_DELIVERY_NOTE_SCOPE_MISMATCH, "BIZ_DELIVERY_NOTE_SCOPE_MISMATCH"),
        (code::BIZ_DELIVERY_SCAN_UNKNOWN_CODE, "BIZ_DELIVERY_SCAN_UNKNOWN_CODE"),
        (code::BIZ_DELIVERY_ASSEMBLY_PARTS_NOT_READY, "BIZ_DELIVERY_ASSEMBLY_PARTS_NOT_READY"),
        (code::BIZ_DELIVERY_NOTE_DRAFT_SCOPE_CONFLICT, "BIZ_DELIVERY_NOTE_DRAFT_SCOPE_CONFLICT"),
        // 215xx
        (code::BIZ_OUTSOURCE_SHIPMENT_NOT_FOUND, "BIZ_OUTSOURCE_SHIPMENT_NOT_FOUND"),
    ];

    #[test]
    fn constants_have_unique_values() {
        let mut seen = std::collections::HashMap::new();
        for &(c, name) in NON_DEPRECATED_CODES {
            if let Some(prev) = seen.insert(c, name) {
                panic!("duplicate value {c}: {prev} and {name}");
            }
        }
    }

    #[test]
    #[allow(deprecated)]
    fn constants_match_python_values() {
        // 0 / 4xxxx / 5xxxx
        assert_eq!(code::SUCCESS, 0);
        assert_eq!(code::BAD_REQUEST, 40000);
        assert_eq!(code::VALIDATION_ERROR, 40001);
        assert_eq!(code::UNAUTHORIZED, 40100);
        assert_eq!(code::BIZ_AUTH_INVALID, 40101);
        assert_eq!(code::TOKEN_EXPIRED, 40102);
        assert_eq!(code::REFRESH_INVALID, 40103);
        assert_eq!(code::OLD_PASSWORD_MISMATCH, 40104);
        assert_eq!(code::FORBIDDEN, 40300);
        assert_eq!(code::SHELF_MISMATCH, 40301);
        assert_eq!(code::NOT_FOUND, 40400);
        assert_eq!(code::VERSION_CONFLICT, 40901);
        assert_eq!(code::REQUEST_TOO_LARGE, 41301);
        assert_eq!(code::INTERNAL, 50000);
        assert_eq!(code::DATABASE, 50001);

        // 200xx
        assert_eq!(code::BIZ_USER_NOT_FOUND, 20001);
        assert_eq!(code::BIZ_USER_DUPLICATE, 20002);
        assert_eq!(code::BIZ_ORDER_NOT_FOUND, 20003);

        // 201xx
        assert_eq!(code::BIZ_PART_NOT_FOUND, 20101);
        assert_eq!(code::BIZ_CUSTOMER_NOT_FOUND, 20102);
        assert_eq!(code::BIZ_INVALID_TRANSITION, 20103);
        assert_eq!(code::BIZ_INVALID_VALUE, 20104);
        assert_eq!(code::BIZ_PART_SERIAL_EXHAUSTED, 20105);
        assert_eq!(code::BIZ_SERIAL_PREFIX_UNKNOWN, 20108);
        assert_eq!(code::BIZ_PART_BATCH_NOT_FOUND, 20109);
        assert_eq!(code::BIZ_PART_PRICE_LOCKED_BY_ASSEMBLY, 20110);
        assert_eq!(code::BIZ_PART_BATCH_INVALID_QUANTITY, 20111);
        assert_eq!(code::BIZ_PART_QUANTITY_LOCKED, 20112);
        assert_eq!(code::BIZ_CUSTOMER_IN_USE, 20113);

        // 202xx
        assert_eq!(code::BIZ_WORKER_NOT_FOUND, 20201);
        assert_eq!(code::BIZ_WORKER_INACTIVE, 20202);
        assert_eq!(code::BIZ_WORKER_IN_USE, 20203);
        assert_eq!(code::BIZ_WORKER_HOLD_LIMIT_EXCEEDED, 20204);

        // 203xx
        assert_eq!(code::BIZ_ASSEMBLY_NOT_FOUND, 20301);
        assert_eq!(code::BIZ_ASSEMBLY_BAD_CUSTOMER, 20302);
        assert_eq!(code::BIZ_ASSEMBLY_TOO_MANY_CHILDREN, 20303);

        // 204xx
        assert_eq!(code::BIZ_DRAWING_FILE_NOT_FOUND, 20401);
        assert_eq!(code::BIZ_DRAWING_FILE_BAD_TYPE, 20402);
        assert_eq!(code::BIZ_DRAWING_FILE_TOO_LARGE, 20403);
        assert_eq!(code::BIZ_DRAWING_UPLOAD_FAILED, 20404);

        // 205xx
        assert_eq!(code::BIZ_SHELF_NOT_FOUND, 20501);
        assert_eq!(code::BIZ_SHELF_DUPLICATE_CODE, 20502);
        assert_eq!(code::BIZ_SHELF_IN_USE, 20503);
        assert_eq!(code::BIZ_SHELF_PROCESS_SHELF_NOT_FOUND, 20504);
        assert_eq!(code::BIZ_SHELF_PROCESS_PROCESS_NOT_FOUND, 20505);
        assert_eq!(code::BIZ_SHELF_NO_MATCH_FOR_PROCESS, 20506);
        assert_eq!(code::BIZ_SHELF_PROCESS_NOT_MAPPED, 20507);

        // 206xx + 短别名
        assert_eq!(code::BIZ_USER_ACCOUNT_NOT_FOUND, 20601);
        assert_eq!(code::BIZ_USER_DUPLICATE_USERNAME, 20602);
        assert_eq!(code::BIZ_USER_INACTIVE, 20603);
        assert_eq!(code::BIZ_USER_ROLE_DUPLICATE, 20604);
        assert_eq!(code::BIZ_USER_ROLE_NOT_FOUND, 20605);
        assert_eq!(code::BIZ_USER_NO_ROLE, 20606);
        assert_eq!(code::USER_NOT_FOUND, code::BIZ_USER_ACCOUNT_NOT_FOUND);
        assert_eq!(code::DUPLICATE_USERNAME, code::BIZ_USER_DUPLICATE_USERNAME);
        assert_eq!(code::ROLE_DUPLICATE, code::BIZ_USER_ROLE_DUPLICATE);
        assert_eq!(code::ROLE_NOT_FOUND, code::BIZ_USER_ROLE_NOT_FOUND);
        assert_eq!(code::NO_ROLE, code::BIZ_USER_NO_ROLE);

        // 208xx
        assert_eq!(code::BIZ_PROCESS_NOT_FOUND, 20801);
        assert_eq!(code::BIZ_PROCESS_DUPLICATE_CODE, 20802);
        assert_eq!(code::BIZ_PROCESS_IN_USE, 20803);

        // 209xx
        assert_eq!(code::BIZ_WORK_TYPE_NOT_FOUND, 20901);
        assert_eq!(code::BIZ_WORK_TYPE_DUPLICATE_CODE, 20902);
        assert_eq!(code::BIZ_WORK_TYPE_IN_USE, 20903);

        // 210xx
        assert_eq!(code::BIZ_APPLICANT_NOT_FOUND, 21001);
        assert_eq!(code::BIZ_APPLICANT_DUPLICATE_NAME, 21002);
        assert_eq!(code::BIZ_APPLICANT_BAD_CUSTOMER, 21003);
        assert_eq!(code::BIZ_APPLICANT_IN_USE, 21004);

        // 211xx + 21110 deprecated 别名指向 21407
        assert_eq!(code::BIZ_PART_FILE_NOT_FOUND, 21101);
        assert_eq!(code::BIZ_PART_FILE_BAD_TYPE, 21102);
        assert_eq!(code::BIZ_PART_FILE_TOO_LARGE, 21103);
        assert_eq!(code::BIZ_PART_FILE_UPLOAD_FAILED, 21104);
        assert_eq!(code::BIZ_PART_FILE_OWNER_NOT_FOUND, 21105);
        assert_eq!(code::BIZ_PART_FILE_DUPLICATE, 21108);
        assert_eq!(code::BIZ_DELIVERY_TEMPLATE_NOT_CONFIGURED, 21109);
        assert_eq!(code::BIZ_DELIVERY_PART_STATUS_INVALID, 21111);
        assert_eq!(code::BIZ_DELIVERY_TEMPLATE_TOO_MANY_PARTS, 21112);
        assert_eq!(code::BIZ_DELIVERY_PRINT_BAD_ORDER, 21113);
        assert_eq!(code::BIZ_DELIVERY_PARTS_MULTIPLE_CUSTOMERS, 21407);

        // 212xx
        assert_eq!(code::BIZ_OUTSOURCE_COMPANY_NOT_FOUND, 21201);
        assert_eq!(code::BIZ_OUTSOURCE_COMPANY_DUPLICATE, 21202);
        assert_eq!(code::BIZ_OUTSOURCE_COMPANY_BAD_PROCESS, 21203);
        assert_eq!(code::BIZ_OUTSOURCE_PROCESS_NOT_MAPPED, 21204);
        assert_eq!(code::BIZ_OUTSOURCE_COMPANY_IN_USE, 21205);
        assert_eq!(code::BIZ_PART_NOT_OUTSOURCEABLE, 21206);
        assert_eq!(code::BIZ_OUTSOURCE_DIRECT_REQUIRES_C2_SHELF, 21207);
        assert_eq!(code::BIZ_OUTSOURCE_NO_SHELF, 21208);

        // 213xx
        assert_eq!(code::BIZ_OUTSOURCE_QUOTE_NOT_FOUND, 21301);
        assert_eq!(code::BIZ_OUTSOURCE_QUOTE_INVALID_TRANSITION, 21302);
        assert_eq!(code::BIZ_OUTSOURCE_QUOTE_DUPLICATE, 21303);
        assert_eq!(code::BIZ_OUTSOURCE_QUOTE_NOT_APPROVED, 21307);

        // 214xx
        assert_eq!(code::BIZ_DELIVERY_NOTE_NOT_FOUND, 21401);
        assert_eq!(code::BIZ_DELIVERY_NOTE_INVALID_TRANSITION, 21402);
        assert_eq!(code::BIZ_DELIVERY_NOTE_NOT_DRAFT, 21403);
        assert_eq!(code::BIZ_DELIVERY_NOTE_NOT_SUBMITTED, 21404);
        assert_eq!(code::BIZ_DELIVERY_NOTE_PART_NOT_READY, 21405);
        assert_eq!(code::BIZ_DELIVERY_NOTE_PART_ALREADY_ASSIGNED, 21406);
        assert_eq!(code::BIZ_DELIVERY_NOTE_PARTS_MULTIPLE_CUSTOMERS, 21407);
        assert_eq!(code::BIZ_DELIVERY_NOTE_SCAN_MISMATCH, 21408);
        assert_eq!(code::BIZ_DELIVERY_NOTE_DRIVER_INVALID, 21409);
        assert_eq!(code::BIZ_DELIVERY_NOTE_SCAN_INCOMPLETE, 21410);
        assert_eq!(code::BIZ_DELIVERY_NOTE_INVALID_VALUE, 21411);
        assert_eq!(code::BIZ_DELIVERY_NOTE_PARTS_LOCKED, 21412);
        assert_eq!(code::BIZ_DELIVERY_GROUP_NOT_FOUND, 21413);
        assert_eq!(code::BIZ_DELIVERY_GROUP_DUPLICATE_NAME, 21414);
        assert_eq!(code::BIZ_DELIVERY_GROUP_MEMBER_CONFLICT, 21415);
        assert_eq!(code::BIZ_DELIVERY_NOTE_SCOPE_MISMATCH, 21416);
        assert_eq!(code::BIZ_DELIVERY_SCAN_UNKNOWN_CODE, 21417);
        assert_eq!(code::BIZ_DELIVERY_ASSEMBLY_PARTS_NOT_READY, 21418);
        assert_eq!(code::BIZ_DELIVERY_NOTE_DRAFT_SCOPE_CONFLICT, 21419);

        // 215xx
        assert_eq!(code::BIZ_OUTSOURCE_SHIPMENT_NOT_FOUND, 21501);
    }

    /// 数据驱动的 HTTP 表覆盖测试：每个 (code, expected_http, name) 一行。
    /// 覆盖所有显式映射到 404/409/422 的新码，以及 2xxxx 默认 400 兜底。
    const HTTP_TABLE_CASES: &[(i32, StatusCode, &str)] = &[
        // 4xxxx（既有）
        (code::BAD_REQUEST, StatusCode::BAD_REQUEST, "BAD_REQUEST"),
        (code::VALIDATION_ERROR, StatusCode::UNPROCESSABLE_ENTITY, "VALIDATION_ERROR"),
        (code::UNAUTHORIZED, StatusCode::UNAUTHORIZED, "UNAUTHORIZED"),
        (code::BIZ_AUTH_INVALID, StatusCode::UNAUTHORIZED, "BIZ_AUTH_INVALID"),
        (code::TOKEN_EXPIRED, StatusCode::UNAUTHORIZED, "TOKEN_EXPIRED"),
        (code::REFRESH_INVALID, StatusCode::UNAUTHORIZED, "REFRESH_INVALID"),
        (code::OLD_PASSWORD_MISMATCH, StatusCode::UNAUTHORIZED, "OLD_PASSWORD_MISMATCH"),
        (code::FORBIDDEN, StatusCode::FORBIDDEN, "FORBIDDEN"),
        (code::SHELF_MISMATCH, StatusCode::FORBIDDEN, "SHELF_MISMATCH"),
        (code::NO_ROLE, StatusCode::FORBIDDEN, "NO_ROLE"),
        (code::USER_NOT_FOUND, StatusCode::NOT_FOUND, "USER_NOT_FOUND"),
        (code::ROLE_NOT_FOUND, StatusCode::NOT_FOUND, "ROLE_NOT_FOUND"),
        (code::DUPLICATE_USERNAME, StatusCode::CONFLICT, "DUPLICATE_USERNAME"),
        (code::ROLE_DUPLICATE, StatusCode::CONFLICT, "ROLE_DUPLICATE"),
        (code::NOT_FOUND, StatusCode::NOT_FOUND, "NOT_FOUND"),
        (code::VERSION_CONFLICT, StatusCode::CONFLICT, "VERSION_CONFLICT"),
        (code::REQUEST_TOO_LARGE, StatusCode::PAYLOAD_TOO_LARGE, "REQUEST_TOO_LARGE"),
        (code::INTERNAL, StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL"),
        (code::DATABASE, StatusCode::INTERNAL_SERVER_ERROR, "DATABASE"),
        // 2xxxx 显式 404
        (code::BIZ_USER_NOT_FOUND, StatusCode::NOT_FOUND, "BIZ_USER_NOT_FOUND"),
        (code::BIZ_ORDER_NOT_FOUND, StatusCode::NOT_FOUND, "BIZ_ORDER_NOT_FOUND"),
        (code::BIZ_PART_NOT_FOUND, StatusCode::NOT_FOUND, "BIZ_PART_NOT_FOUND"),
        (code::BIZ_CUSTOMER_NOT_FOUND, StatusCode::NOT_FOUND, "BIZ_CUSTOMER_NOT_FOUND"),
        (code::BIZ_PART_BATCH_NOT_FOUND, StatusCode::NOT_FOUND, "BIZ_PART_BATCH_NOT_FOUND"),
        (code::BIZ_WORKER_NOT_FOUND, StatusCode::NOT_FOUND, "BIZ_WORKER_NOT_FOUND"),
        (code::BIZ_ASSEMBLY_NOT_FOUND, StatusCode::NOT_FOUND, "BIZ_ASSEMBLY_NOT_FOUND"),
        (code::BIZ_DRAWING_FILE_NOT_FOUND, StatusCode::NOT_FOUND, "BIZ_DRAWING_FILE_NOT_FOUND"),
        (code::BIZ_SHELF_NOT_FOUND, StatusCode::NOT_FOUND, "BIZ_SHELF_NOT_FOUND"),
        (code::BIZ_PROCESS_NOT_FOUND, StatusCode::NOT_FOUND, "BIZ_PROCESS_NOT_FOUND"),
        (code::BIZ_WORK_TYPE_NOT_FOUND, StatusCode::NOT_FOUND, "BIZ_WORK_TYPE_NOT_FOUND"),
        (code::BIZ_APPLICANT_NOT_FOUND, StatusCode::NOT_FOUND, "BIZ_APPLICANT_NOT_FOUND"),
        (code::BIZ_PART_FILE_NOT_FOUND, StatusCode::NOT_FOUND, "BIZ_PART_FILE_NOT_FOUND"),
        (code::BIZ_OUTSOURCE_COMPANY_NOT_FOUND, StatusCode::NOT_FOUND, "BIZ_OUTSOURCE_COMPANY_NOT_FOUND"),
        (code::BIZ_OUTSOURCE_QUOTE_NOT_FOUND, StatusCode::NOT_FOUND, "BIZ_OUTSOURCE_QUOTE_NOT_FOUND"),
        (code::BIZ_DELIVERY_NOTE_NOT_FOUND, StatusCode::NOT_FOUND, "BIZ_DELIVERY_NOTE_NOT_FOUND"),
        (code::BIZ_DELIVERY_GROUP_NOT_FOUND, StatusCode::NOT_FOUND, "BIZ_DELIVERY_GROUP_NOT_FOUND"),
        (code::BIZ_DELIVERY_SCAN_UNKNOWN_CODE, StatusCode::NOT_FOUND, "BIZ_DELIVERY_SCAN_UNKNOWN_CODE"),
        (code::BIZ_OUTSOURCE_SHIPMENT_NOT_FOUND, StatusCode::NOT_FOUND, "BIZ_OUTSOURCE_SHIPMENT_NOT_FOUND"),
        // 2xxxx 显式 409
        (code::BIZ_USER_DUPLICATE, StatusCode::CONFLICT, "BIZ_USER_DUPLICATE"),
        (code::BIZ_USER_DUPLICATE_USERNAME, StatusCode::CONFLICT, "BIZ_USER_DUPLICATE_USERNAME"),
        (code::BIZ_USER_ROLE_DUPLICATE, StatusCode::CONFLICT, "BIZ_USER_ROLE_DUPLICATE"),
        (code::BIZ_PART_PRICE_LOCKED_BY_ASSEMBLY, StatusCode::CONFLICT, "BIZ_PART_PRICE_LOCKED_BY_ASSEMBLY"),
        (code::BIZ_PART_QUANTITY_LOCKED, StatusCode::CONFLICT, "BIZ_PART_QUANTITY_LOCKED"),
        (code::BIZ_CUSTOMER_IN_USE, StatusCode::CONFLICT, "BIZ_CUSTOMER_IN_USE"),
        (code::BIZ_WORKER_IN_USE, StatusCode::CONFLICT, "BIZ_WORKER_IN_USE"),
        (code::BIZ_WORKER_HOLD_LIMIT_EXCEEDED, StatusCode::CONFLICT, "BIZ_WORKER_HOLD_LIMIT_EXCEEDED"),
        (code::BIZ_SHELF_DUPLICATE_CODE, StatusCode::CONFLICT, "BIZ_SHELF_DUPLICATE_CODE"),
        (code::BIZ_SHELF_IN_USE, StatusCode::CONFLICT, "BIZ_SHELF_IN_USE"),
        (code::BIZ_PROCESS_DUPLICATE_CODE, StatusCode::CONFLICT, "BIZ_PROCESS_DUPLICATE_CODE"),
        (code::BIZ_PROCESS_IN_USE, StatusCode::CONFLICT, "BIZ_PROCESS_IN_USE"),
        (code::BIZ_WORK_TYPE_DUPLICATE_CODE, StatusCode::CONFLICT, "BIZ_WORK_TYPE_DUPLICATE_CODE"),
        (code::BIZ_WORK_TYPE_IN_USE, StatusCode::CONFLICT, "BIZ_WORK_TYPE_IN_USE"),
        (code::BIZ_APPLICANT_DUPLICATE_NAME, StatusCode::CONFLICT, "BIZ_APPLICANT_DUPLICATE_NAME"),
        (code::BIZ_APPLICANT_IN_USE, StatusCode::CONFLICT, "BIZ_APPLICANT_IN_USE"),
        (code::BIZ_PART_FILE_DUPLICATE, StatusCode::CONFLICT, "BIZ_PART_FILE_DUPLICATE"),
        (code::BIZ_OUTSOURCE_COMPANY_DUPLICATE, StatusCode::CONFLICT, "BIZ_OUTSOURCE_COMPANY_DUPLICATE"),
        (code::BIZ_OUTSOURCE_COMPANY_IN_USE, StatusCode::CONFLICT, "BIZ_OUTSOURCE_COMPANY_IN_USE"),
        (code::BIZ_OUTSOURCE_QUOTE_DUPLICATE, StatusCode::CONFLICT, "BIZ_OUTSOURCE_QUOTE_DUPLICATE"),
        (code::BIZ_DELIVERY_NOTE_PART_ALREADY_ASSIGNED, StatusCode::CONFLICT, "BIZ_DELIVERY_NOTE_PART_ALREADY_ASSIGNED"),
        (code::BIZ_DELIVERY_NOTE_PARTS_LOCKED, StatusCode::CONFLICT, "BIZ_DELIVERY_NOTE_PARTS_LOCKED"),
        (code::BIZ_DELIVERY_GROUP_DUPLICATE_NAME, StatusCode::CONFLICT, "BIZ_DELIVERY_GROUP_DUPLICATE_NAME"),
        (code::BIZ_DELIVERY_GROUP_MEMBER_CONFLICT, StatusCode::CONFLICT, "BIZ_DELIVERY_GROUP_MEMBER_CONFLICT"),
        (code::BIZ_DELIVERY_NOTE_DRAFT_SCOPE_CONFLICT, StatusCode::CONFLICT, "BIZ_DELIVERY_NOTE_DRAFT_SCOPE_CONFLICT"),
        // 2xxxx 显式 422
        (code::BIZ_DELIVERY_PRINT_BAD_ORDER, StatusCode::UNPROCESSABLE_ENTITY, "BIZ_DELIVERY_PRINT_BAD_ORDER"),
        // 2xxxx 默认 400 兜底
        (code::BIZ_INVALID_TRANSITION, StatusCode::BAD_REQUEST, "BIZ_INVALID_TRANSITION"),
        (code::BIZ_INVALID_VALUE, StatusCode::BAD_REQUEST, "BIZ_INVALID_VALUE"),
        (code::BIZ_DELIVERY_TEMPLATE_NOT_CONFIGURED, StatusCode::BAD_REQUEST, "BIZ_DELIVERY_TEMPLATE_NOT_CONFIGURED"),
        (code::BIZ_DELIVERY_NOTE_SCOPE_MISMATCH, StatusCode::BAD_REQUEST, "BIZ_DELIVERY_NOTE_SCOPE_MISMATCH"),
        (code::BIZ_DELIVERY_ASSEMBLY_PARTS_NOT_READY, StatusCode::BAD_REQUEST, "BIZ_DELIVERY_ASSEMBLY_PARTS_NOT_READY"),
    ];

    #[test]
    fn status_from_code_is_correct_for_every_known_code() {
        for &(c, expected, name) in HTTP_TABLE_CASES {
            let err = AppError::biz(c, "x");
            let actual = err.http_status();
            assert_eq!(
                actual, expected,
                "status_from_code({c} / {name}): got {actual:?}, want {expected:?}"
            );
            assert_eq!(err.code(), c, "{name} code() 必须是 {c}");
        }
    }

    #[test]
    fn status_from_code_fallback_for_out_of_range() {
        // 任何不在表里的码 → 500（catch-all 不变）
        assert_eq!(
            AppError::biz(99_999, "x").http_status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        // 任何 2xxxx 但不在显式映射里 → 400（Python BizError 默认）
        assert_eq!(
            AppError::biz(20_999, "x").http_status(),
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    #[allow(deprecated)]
    fn deprecated_alias_resolves_to_canonical_code() {
        // 21110 → 21407（编译期）
        assert_eq!(
            code::BIZ_DELIVERY_PARTS_MULTIPLE_CUSTOMERS,
            code::BIZ_DELIVERY_NOTE_PARTS_MULTIPLE_CUSTOMERS
        );
        assert_eq!(code::BIZ_DELIVERY_PARTS_MULTIPLE_CUSTOMERS, 21407);
    }

    #[test]
    fn biz_with_status_overrides_table() {
        // 40100 表里 → 401；显式覆盖为 418
        let err = AppError::biz_with_status(code::UNAUTHORIZED, "teapot", StatusCode::IM_A_TEAPOT);
        assert_eq!(err.code(), code::UNAUTHORIZED);
        assert_eq!(err.http_status(), StatusCode::IM_A_TEAPOT);
        assert_eq!(err.to_string(), "[40100] teapot");
    }

    #[test]
    fn biz_with_status_preserves_code_in_envelope() {
        // 20606 NO_ROLE 表里 → 403；biz_with_status 也走 403（一致）
        let err = AppError::biz_with_status(code::NO_ROLE, "no role", StatusCode::FORBIDDEN);
        assert_eq!(err.http_status(), StatusCode::FORBIDDEN);
        // 与表驱动形式结果一致
        let err2 = AppError::biz(code::NO_ROLE, "no role");
        assert_eq!(err.http_status(), err2.http_status());
    }

    #[test]
    fn biz_with_status_renders_in_response() {
        let err = AppError::biz_with_status(20109, "missing batch", StatusCode::NOT_FOUND);
        let s = err.to_string();
        assert!(s.contains("20109"), "Display 必须包含 code: {s}");
        assert!(s.contains("missing batch"), "Display 必须包含 message: {s}");
    }

    #[test]
    fn biz_with_failures_carries_failures_into_envelope() {
        let err = AppError::BizWithFailures {
            code: code::BIZ_DELIVERY_ASSEMBLY_PARTS_NOT_READY,
            message: "整套拒绝".to_string(),
            http: StatusCode::BAD_REQUEST,
            failures: vec![
                serde_json::json!({
                    "part_id": "1001",
                    "batch_id": "2001",
                    "drawing_no": "DWG-001",
                    "status": "IN_PROCESS",
                    "serial_no": "B01",
                    "name": "fala-A",
                    "reason": "status=IN_PROCESS",
                }),
                serde_json::json!({
                    "part_id": "1002",
                    "batch_id": "2002",
                    "drawing_no": "DWG-002",
                    "status": "INSPECTION",
                    "serial_no": "B02",
                    "name": "fala-B",
                    "reason": "on note DN-20260821-0001",
                }),
            ],
        };
        assert_eq!(err.code(), code::BIZ_DELIVERY_ASSEMBLY_PARTS_NOT_READY);
        assert_eq!(err.http_status(), StatusCode::BAD_REQUEST);
        let s = err.to_string();
        assert!(s.contains("21418"));
        assert!(s.contains("整套拒绝"));
        if let AppError::BizWithFailures { failures, .. } = &err {
            assert_eq!(failures.len(), 2);
            assert_eq!(failures[0]["part_id"], "1001");
            assert_eq!(failures[0]["batch_id"], "2001");
            assert_eq!(failures[0]["drawing_no"], "DWG-001");
            assert_eq!(failures[0]["status"], "IN_PROCESS");
            assert_eq!(failures[1]["part_id"], "1002");
        } else {
            panic!("expected BizWithFailures variant");
        }
    }
}