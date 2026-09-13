//! cnc_program 域数据模型占位（2026-09-14 Phase 3）
//!
//! CNC 程序本质是两条 `t_part_file` 配对行（kind='G_CODE' + kind='SETUP_SHEET'），
//! 通过 `paired_file_id` 互相指向。本域不引入新表，复用 part_file 的存储。
//!
//! 本文件目前为占位 — DTO 在 `dto.rs`，service/handler 直接读写 part_file。
//! 保留此文件是为对齐 `mod.rs` 的 `pub mod model;` 与迁移指南约定的六件套结构。