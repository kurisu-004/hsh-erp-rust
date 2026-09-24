//! assembly 域生命周期 service（2026-09-22 Group D-3 拆自原 service.rs）
//!
//! 承载两类非 CRUD 业务：
//! 1. **状态机推进** —— `start_assembly`（PENDING → IN_PROCESS 守卫）。
//! 2. **文件上传** —— `upload_assembly_files`（multipart PDF → COS + t_part_file INSERT）。
//!
//! 状态机翻转（cancel / soft_delete）已归到 `crud.rs`（属于 CRUD 端点）。
//!
//! ## 事务边界（2026-09-22 重构对齐 iam 范本）
//! 事务移交 handler：service 仅业务逻辑，所有跨 repo 操作经 `repo: R`
//! （by-value；`R: AssemblyRepoTrait`）参数传入——handler/service 借 `&mut *tx` /
//! `&mut *conn` 喂给 trait（trait 已直接 `impl for &mut PgConnection`）。
//!
//! ## 跨域调用（2026-09-22 D-3 决策）
//! 所有跨域 SQL 通过 `AssemblyRepoTrait` 跨域 helper 收口（CAS 去重 + INSERT）。
//!
//! COS client 通过方法参数注入（handler 持有 `state.cos`），service 不持
//! `Arc<dyn CosClient>` 字段。

use std::sync::Arc;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::cos::CosClient;
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::assembly::model::TAssembly;
use crate::modules::assembly::repo::AssemblyRepoTrait;
use crate::modules::assembly::statemachine::AssemblyStatus;
use crate::modules::assembly::vo::AssemblyOut;
use crate::modules::part_file::repo::{NewPartFile, hash_bytes};
use crate::shared::error::{AppError, code};

use super::AssemblyService;

// =============================================================================
// 兼容旧测试的 ZST 静态 wrapper 实现（2026-09-22 D-3 决策）
// =============================================================================
//
// 既存集成测试以 `AssemblyService::start_assembly(&mut tx, ...)` 形式直调 service。
// trait 注入式新签名要求 caller 写 `AssemblyService.xxx(&mut *tx, ...)`，与旧测试
// 不兼容。本任务"不修改测试代码"，故以下 `_impl` 函数保留旧签名，内部一行委托。

pub(crate) async fn start_assembly_dispatch(
    conn: &mut sqlx::PgConnection,
    assembly_id: i64,
    current: &CurrentUser,
) -> Result<AssemblyOut, AppError> {
    AssemblyService
        .start_assembly_inner(conn, assembly_id, current)
        .await
}

pub(crate) async fn upload_assembly_files_dispatch(
    conn: &mut sqlx::PgConnection,
    snowflake: &SnowflakeIdGenerator,
    cos: Arc<dyn CosClient>,
    assembly_id: i64,
    files: Vec<(Vec<u8>, String, String)>,
    current: &CurrentUser,
) -> Result<Vec<crate::modules::assembly::vo::AssemblyFileRef>, AppError> {
    AssemblyService
        .upload_assembly_files_inner(conn, snowflake, cos, assembly_id, files, current)
        .await
}

pub(crate) async fn list_assembly_files_dispatch(
    conn: &mut sqlx::PgConnection,
    assembly_id: i64,
    current: &CurrentUser,
) -> Result<crate::modules::part_file::vo::PartFileListOut, AppError> {
    AssemblyService
        .list_assembly_files_inner(conn, assembly_id, current)
        .await
}

impl AssemblyService {
    // =======================================================================
    // 状态机推进：start_assembly
    // =======================================================================

    /// `POST /assemblies/{id}/start`：PENDING → IN_PROCESS（状态机守卫）。
    ///
    /// 2026-09-14 Phase 3（deferred #4）：独立端点暴露装配体进入加工态。
    /// 权限：Manager / Clerk；状态机 `PENDING → IN_PROCESS` 校验在 service 层。
    pub async fn start_assembly_inner<R: AssemblyRepoTrait>(
        &self,
        mut repo: R,
        assembly_id: i64,
        current: &CurrentUser,
    ) -> Result<AssemblyOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        let asm = repo
            .get_by_id(assembly_id, false)
            .await
            .map_err(AppError::from)?
            .ok_or_else(|| AppError::biz(code::BIZ_ASSEMBLY_NOT_FOUND, "assembly 不存在"))?;
        let from = AssemblyStatus::from_str(&asm.status).ok_or_else(|| {
            AppError::biz(
                code::BIZ_INVALID_VALUE,
                format!("未知 assembly status: {}", asm.status),
            )
        })?;
        if !from.can_transition_to(AssemblyStatus::IN_PROCESS) {
            return Err(AppError::biz(
                code::BIZ_INVALID_TRANSITION,
                format!("start 状态机禁止: {} → IN_PROCESS", from.as_str()),
            ));
        }
        let affected = repo
            .update_status_if_not_terminal(
                assembly_id,
                asm.version,
                AssemblyStatus::IN_PROCESS.as_str(),
                current.id,
            )
            .await
            .map_err(AppError::from)?;
        if affected == 0 {
            return Err(AppError::biz(
                code::VERSION_CONFLICT,
                "version 不匹配或已终态",
            ));
        }
        let fresh = repo
            .get_by_id(assembly_id, false)
            .await
            .map_err(AppError::from)?
            .ok_or_else(|| AppError::biz(code::BIZ_ASSEMBLY_NOT_FOUND, "start 后查不到"))?;
        Ok(render_assembly_out(fresh))
    }

    // =======================================================================
    // 文件上传：upload_assembly_files
    // =======================================================================

    /// `POST /assemblies/{id}/files`：multipart PDF 上传（deferred #1）。
    ///
    /// 复用 part_file 域上传逻辑（SHA-256 CAS + COS PUT + t_part_file INSERT）。
    /// owner_kind='ASSEMBLY'，kind='ASSEMBLY_MASTER'；多 PDF 用多 part。
    ///
    /// 返回 AssemblyFileRef 列表（不含下载 URL，前端用 `GET /part-files/{id}/url`）。
    pub async fn upload_assembly_files_inner<R: AssemblyRepoTrait>(
        &self,
        mut repo: R,
        snowflake: &SnowflakeIdGenerator,
        cos: Arc<dyn CosClient>,
        assembly_id: i64,
        files: Vec<(Vec<u8>, String, String)>, // (bytes, filename, content_type)
        current: &CurrentUser,
    ) -> Result<Vec<crate::modules::assembly::vo::AssemblyFileRef>, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        // 校验 assembly 存在
        let asm = repo
            .get_by_id(assembly_id, false)
            .await
            .map_err(AppError::from)?
            .ok_or_else(|| AppError::biz(code::BIZ_ASSEMBLY_NOT_FOUND, "assembly 不存在"))?;

        let mut out = Vec::with_capacity(files.len());
        for (bytes, filename, content_type) in files {
            // 扩展名校验（仅允许 PDF）
            let ext = crate::modules::part_file::policy::ext_of(&filename)
                .ok_or_else(|| AppError::biz(code::BIZ_PART_FILE_BAD_TYPE, "文件缺少扩展名"))?;
            if ext != "pdf" {
                return Err(AppError::biz(
                    code::BIZ_PART_FILE_BAD_TYPE,
                    format!("ASSEMBLY_MASTER 仅接受 PDF，扩展名 {ext:?} 不允许"),
                ));
            }
            // SHA-256 → CAS
            let sha = hash_bytes(&bytes);
            if let Some(existing) = repo
                .part_file_get_by_owner_kind_sha(asm.id, "ASSEMBLY_MASTER", &sha)
                .await?
            {
                out.push(crate::modules::assembly::vo::AssemblyFileRef {
                    id: existing.id,
                    original_filename: existing.original_filename,
                    page_count: None, // PDF 页数由 GET 时 lopdf 重算；这里省略
                });
                continue;
            }
            // 上传 COS
            let safe_filename = sanitize_cos_filename(&filename);
            let object_key = format!(
                "assembly/{}/ASSEMBLY_MASTER/{}_{}",
                asm.id,
                &sha[..16],
                safe_filename,
            );
            cos.put_object(&object_key, bytes.clone(), &content_type)
                .await?;
            // INSERT
            let file_id = snowflake.next_id();
            let nf = NewPartFile {
                id: file_id,
                part_id: asm.id,
                owner_kind: "ASSEMBLY",
                kind: "ASSEMBLY_MASTER",
                file_type: "PDF",
                object_key: &object_key,
                original_filename: &filename,
                file_size: bytes.len() as i64,
                content_type: &content_type,
                upload_status: "READY",
                content_sha256: Some(&sha),
                created_by: current.id,
            };
            repo.part_file_create(nf).await?;
            out.push(crate::modules::assembly::vo::AssemblyFileRef {
                id: file_id,
                original_filename: filename,
                page_count: None,
            });
        }
        Ok(out)
    }

    // =======================================================================
    // 文件列出：list_assembly_files（2026-09-25 新增 D-09 端点）
    // =======================================================================

    /// `GET /assemblies/{id}/files`：列出装配体已上传 PDF（kind=ASSEMBLY_MASTER）。
    ///
    /// 复用 part_file 域 `list_files`（走 `part_file_repo::list_with_filters`
    /// `owner_kind='ASSEMBLY'`）；与 `PartFileService::list_files` 完全对齐。
    /// 权限：4 角色全开放（与 `list_part_files_for_part` 一致）。
    ///
    /// 设计意图：复用 service 层的 `part_file::PartFileListQuery` DTO，把
    /// `owner_kind="ASSEMBLY"` 与 `owner_id=asm.id` 写死，handler 仅透传
    /// kind 过滤（暂未暴露；本端点为简单 list）。
    pub async fn list_assembly_files_inner<R: AssemblyRepoTrait>(
        &self,
        mut repo: R,
        assembly_id: i64,
        current: &CurrentUser,
    ) -> Result<crate::modules::part_file::vo::PartFileListOut, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::Inspector,
            Role::CncProgrammer,
        ])?;
        // 1. assembly 存在性
        let _asm = repo
            .get_by_id(assembly_id, false)
            .await
            .map_err(AppError::from)?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_ASSEMBLY_NOT_FOUND,
                    format!("assembly {assembly_id} 不存在"),
                )
            })?;
        // 2. 复用 list_by_owner（owner_kind='ASSEMBLY'），仅 kind='ASSEMBLY_MASTER'
        let rows = repo
            .list_part_files_by_owner("ASSEMBLY", assembly_id)
            .await
            .map_err(AppError::from)?;
        // 3. 投影到 PartFileOut（kind 过滤在 SQL 后再做，list_by_owner 不带 kind）
        let total = rows.len() as i64;
        let items: Vec<crate::modules::part_file::vo::PartFileOut> = rows
            .into_iter()
            .filter(|r| r.kind == "ASSEMBLY_MASTER")
            .map(|r| crate::modules::part_file::vo::PartFileOut {
                id: r.id,
                owner_id: r.part_id,
                owner_kind: "ASSEMBLY".to_string(),
                kind: r.kind,
                file_type: r.file_type,
                object_key: r.object_key,
                original_filename: r.original_filename,
                file_size: r.file_size,
                content_type: r.content_type,
                upload_status: r.upload_status,
                content_sha256: r.content_sha256,
                paired_file_id: r.paired_file_id,
                version: r.version,
                created_at: Some(r.created_at),
                created_by: r.created_by,
            })
            .collect();
        Ok(crate::modules::part_file::vo::PartFileListOut { items, total })
    }
}

// ---------- internal helpers ----------

/// `TAssembly` → `AssemblyOut`（start / cancel / 等端点返回）。
///
/// 与 `crud.rs::render_assembly_out` 同形；2026-09-22 D-3 决策：保留两份（不抽 pub fn）
/// 以避免跨子模块 `pub(crate)` 暴露——本子模块只服务 lifecycle 端点。
fn render_assembly_out(asm: TAssembly) -> AssemblyOut {
    AssemblyOut {
        id: asm.id,
        drawing_no: asm.drawing_no,
        name: asm.name,
        applicant_name: asm.applicant_name,
        customer_id: asm.customer_id,
        request_date: asm.request_date,
        planned_delivery_date: asm.planned_delivery_date,
        is_urgent: asm.is_urgent,
        status: asm.status,
        version: asm.version,
        serial_no: asm.serial_no,
        quantity: asm.quantity,
        unit_price: asm.unit_price,
        total_price: asm.total_price,
        order_no: asm.order_no,
        system_delivery_date: asm.system_delivery_date,
        note: asm.note,
        created_at: asm.created_at,
        updated_at: asm.updated_at,
    }
}

/// 把 client-supplied filename 清洗为 COS object key 安全字符串：
/// - 保留 ASCII 字母 / 数字 / `.` / `-` / `_`
/// - 其它字符（含中文 / 空格）替换为 `_`
/// - 长度上限 80 字符
fn sanitize_cos_filename(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.len() > 80 {
        out.truncate(80);
    }
    if out.is_empty() {
        out.push_str("file");
    }
    out
}

#[cfg(test)]
mod upload_helpers_tests {
    use super::*;

    #[test]
    fn sanitize_cos_filename_basic() {
        assert_eq!(sanitize_cos_filename("master.pdf"), "master.pdf");
        assert_eq!(sanitize_cos_filename("图纸 v2.pdf"), "___v2.pdf");
    }

    #[test]
    fn sanitize_cos_filename_empty_fallback() {
        assert_eq!(sanitize_cos_filename(""), "file");
        // 中文 → "__"（替换为下划线后非空，保留而非 fallback）
        assert_eq!(sanitize_cos_filename("中文"), "__");
    }
}
