//! 通用工具模块
//!
//! 对应 Python myERP/utils/：
//! - barcode：Code128 条码（PNG/PDF 嵌入）
//! - excel：送货单 xlsx 模板填充
//! - pdf：PDF 拆分/渲染/胸牌
//! - cos_key：CAS key 派生 + 安全文件名（对齐 Python core/file_hash.py）
//!   2026-09-11 新增 cos_key

pub mod barcode;
pub mod cos_key; // 2026-09-11 新增
pub mod excel;
pub mod pdf;
