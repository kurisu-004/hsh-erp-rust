//! worker 域 DTO 占位
//!
// 对应 Python myERP/schema/worker.py。命名约定：
// - `CreateXxxRequest` / `UpdateXxxRequest`：写操作入参
// - `XxxOut`：单条详情出参（id 字段用 #[serde(serialize_with = shared::types::serialize_i64)]）
// - `XxxListItem` / `XxxListOut`：列表分页
// - `XxxListQuery`：列表查询参数（继承/字段对应 PageQuery）
