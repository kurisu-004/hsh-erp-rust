//! customer 域 service 子模块聚合
//!
//! 拆分依据（com/customer/applicant plan）：把单文件 `service.rs` 拆为
//! - `crud` —— list/get/create/update/soft_delete（含 `CustomerService` struct）
//!
//! 让每个文件都落在 < 800 行（conventions.md §2）。customer 域当前仅 CRUD，
//! 无 picker / mapping 等其他端点，故暂只拆 `crud`。
//!
//! ## 调用方契约
//! `handler.rs` 仅引 `crate::modules::com::customer::service::CustomerService::*`，
//! 不直接访问 `crud`。本模块用 `pub use crud::*` 把 `CustomerService` 类型
//! 重新汇出到 `service` 命名空间。

pub mod crud;

pub use crud::CustomerService;
