//! part 域批量创建 + 直传 COS confirm handler
//!
//! 对应：
//! - `POST /api/v2/parts/batch` —— 批量创建（共享 `customer_id`）；per-item 失败
//!   不中断整体；支持 `drawing_file` / `model3d_file` 单 item 文件绑定（场景 A 直传
//!   COS 链路，2026-09-16 M2-B 新增）。
//! - `POST /api/v2/parts/batch-with-pdfs` (multipart) —— 多 PDF 自动派生子件
//!   （page1 = master + page2..N = children），2026-09-14 Phase 3 完整化。
//! - `POST /api/v2/parts/{part_id}/files/confirm` —— 直传 COS 链路的"提交绑定"端点：
//!   客户端 PUT 到 tmp 区成功后，调用本端点把 tmp 对象 copy 到 CAS key +
//!   INSERT `t_part_file` + 异步清理 tmp 对象。
//!
//! ## 错误码
//! - 21114 `BIZ_PART_FILE_TMP_OBJECT_MISSING` — head_object 失败（tmp 不存在）
//! - 21115 `BIZ_PART_FILE_SIZE_MISMATCH` — head size 与声明 size 不一致
//! - 21116 `STS_ISSUE_FAILED` — STS 凭证下发失败（confirm 路径暂未使用，留位）
//! - 21108 `BIZ_PART_FILE_DUPLICATE` — 同 part + kind + sha256 撞唯一索引
//!
//! ## 异步清理契约（2026-09-16 M2-B 第 2 轮）
//! - batch_create_parts：head/copy 成功的 tmp_keys **全部** 透出到
//!   `out.cleanup_tmp_keys`（无论 per-item DB 结果如何），handler commit 后
//!   **统一** spawn delete 兜底；commit 失败时也 spawn（DB 未持久化时 tmp 必须删）。
//! - confirm_part_file：commit 后 spawn delete 单个 tmp_key（CAS 已绑定，删 tmp 无害）。

use std::sync::Arc;

use axum::Json;
use axum::extract::{Multipart, Path, State};
use serde_json::json;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::ws_hub::WsEvent;
use crate::modules::part::dto::{BatchToInspectionRequest, BatchToXxxOut};
use crate::modules::part::dto_crud::{
    BatchWithPdfsRequest, PartBatchCreateOut, PartBatchCreateRequest, PartDetailOut,
};
use crate::modules::part::service::PartService;
use crate::modules::part_file::dto::{ConfirmFileIn, PartFileOut};
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

/// `POST /api/v2/parts/batch`
///
/// 批量创建（共享 `customer_id`）；per-item 失败不中断整体。
///
/// 2026-09-16 M2-B 新增：支持 `drawing_file` / `model3d_file` 单 item 文件绑定
/// （直传 COS 链路）。任一 binding head/copy 失败 → 整体报错回滚（用户重试整批）；
/// 其余 part 成功则按 per-item savepoint 模型部分成功。
pub async fn batch_create_parts(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<PartBatchCreateRequest>,
) -> Result<Json<R<PartBatchCreateOut>>, AppError> {
    use crate::auth::rbac::Role;
    current.require_any_role(&[Role::Manager, Role::Clerk])?;
    // 检查是否含任何 file binding（决定走 legacy / 扩展入口）
    let has_bindings = req
        .items
        .iter()
        .any(|i| i.drawing_file.is_some() || i.model3d_file.is_some());
    let mut tx = state.pool.begin().await?;
    // 2026-09-16 M2-C 修：service 签名改为 `Result<Out, (AppError, Vec<String>)>`
    // 让 Err 路径也透出已成功 head/copy 的 tmp_keys（first-pass 收集），handler
    // 无论 Ok/Err 都 spawn delete 兜底（避免 service 重复 spawn）。
    let out_result: Result<(PartBatchCreateOut, Vec<String>), (AppError, Vec<String>)> =
        if has_bindings {
            // 走带 bindings 的扩展入口：head/copy 在 tx 之外（service 内 pool 直连），
            // tx 内只做 part / batch / part_file INSERT；commit 后由本 handler spawn
            // 批量 delete_object(tmp_keys) 兜底。
            // service 签名：成功 (Out, keys) / 失败 (AppError, keys)。
            PartService::batch_create_parts_with_bindings(
                &mut tx,
                &state.snowflake,
                state.cos.clone(),
                &state.config.cos.upload_prefix,
                &state.config.cos.tmp_prefix,
                &req,
                &current,
            )
            .await
        } else {
            // legacy 路径 service 直接返 cleanup_tmp_keys=Vec::new()（见
            // batch_create_parts_legacy），spawn 条件 `!cleanup_keys.is_empty()` 自动跳过。
            // legacy 路径无 IO，Err 不可能带 keys —— 直接 spread。
            let res =
                PartService::batch_create_parts(&mut tx, &state.snowflake, &req, &current).await;
            res.map(|out| (out, Vec::new()))
                .map_err(|e| (e, Vec::new()))
        };
    // 2026-09-16 M2-B review 第 2 轮 B1 修：必须在 tx.commit() 之前拿到 cleanup_tmp_keys，
    // 否则 tx.commit().await 持有 conn 时跨 .await 容易踩 sqlx 的 connection-held-across-await
    // 警告。Ok/Err 都先 move 出 keys 再 commit（Err 路径也透出 — M2-C 修）。
    let (cleanup_keys, commit_result): (Vec<String>, Result<(), sqlx::Error>) = match &out_result {
        Ok((_out, keys)) => {
            let commit = tx.commit().await;
            (keys.clone(), commit)
        }
        Err((_e, keys)) => {
            let commit = tx.commit().await;
            (keys.clone(), commit)
        }
    };
    // spawn 异步批量清理 tmp 对象 —— **无论** commit 成功 / 失败 / service Err 均触发：
    // - commit 成功 → 删 tmp 无害（CAS 端已有对象引用 cas_key）；
    // - commit 失败 → DB 未持久化，tmp 必须删（防孤儿，下次重传可复用 key）；
    // - service Err（head/copy 中途失败）→ first-pass 已成功的 tmp 也必须删。
    // Best-effort：失败仅 warn，不影响 API 返回。
    if has_bindings && !cleanup_keys.is_empty() {
        let cos = state.cos.clone();
        tokio::spawn(async move {
            for key in cleanup_keys {
                if let Err(e) = cos.delete_object(&key).await {
                    tracing::warn!(
                        tmp_key = %key,
                        error = %e,
                        "batch_create_parts COS tmp 异步清理失败（best-effort）"
                    );
                }
            }
        });
    }
    // commit 错误优先于 service Err 返回（让客户端看到真正的失败原因）
    commit_result.map_err(AppError::from)?;
    match out_result {
        Ok((out, _keys)) => Ok(Json(R::ok(out))),
        Err((e, _keys)) => Err(e),
    }
}

/// `POST /api/v2/parts/{part_id}/files/confirm`
///
/// 直传 COS 链路的"提交绑定"端点：客户端 PUT 到 tmp 区成功后，调用本端点把
/// tmp 对象 copy 到 CAS key + INSERT `t_part_file` + 异步清理 tmp 对象。
///
/// 流程：
/// 1. 权限（service 内 `require_any_role(Manager + Clerk)`，按 kind 派生）
/// 2. 入参校验（kind / sha / filename / size / content_type）
/// 3. `PartFileService::bind_uploaded_file` —— head/copy 在 pool（不在 tx），
///    tx 内只做 soft_delete 旧 + INSERT 新
/// 4. **commit 后** spawn 异步 `cos.delete_object(tmp_key)` 兜底清理（与
///    `soft_delete_part_file` 模式一致，避免 commit 失败却触发 COS 删除）
///
/// 2026-09-16 M2-B 新增。
pub async fn confirm_part_file(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(part_id): Path<i64>,
    Json(req): Json<ConfirmFileIn>,
) -> Result<Json<R<PartFileOut>>, AppError> {
    let (out, tmp_key) = crate::modules::part_file::service::PartFileService::bind_uploaded_file(
        &state.pool,
        &state.snowflake,
        state.cos.clone(),
        &state.config.cos.upload_prefix,
        &state.config.cos.tmp_prefix,
        part_id,
        &req.kind,
        &req.tmp_key,
        &req.content_sha256,
        &req.original_filename,
        req.file_size,
        &req.content_type,
        &current,
    )
    .await?;
    // commit 已由 service 内 `tx.commit()` 完成；这里 spawn 异步清理 tmp
    let cos = state.cos.clone();
    tokio::spawn(async move {
        if let Err(e) = cos.delete_object(&tmp_key).await {
            tracing::warn!(
                tmp_key = %tmp_key,
                error = %e,
                "confirm_part_file COS tmp 异步清理失败（已绑定到 part_file，不影响 API 返回）"
            );
        }
    });
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/parts/batch-with-pdfs` (multipart)
///
/// page1 = master part + page2..N = auto-created children parts；
/// page_count == 1 仅创建 master；page_count == 0 仅创建 master 无 serial。
pub async fn batch_with_pdfs(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    mut multipart: Multipart,
) -> Result<Json<R<PartDetailOut>>, AppError> {
    let mut json_body: Option<BatchWithPdfsRequest> = None;
    let mut pdf_files: Vec<Vec<u8>> = Vec::new();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::validation(format!("multipart 解析失败: {e}")))?
    {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "json" => {
                let text = field
                    .text()
                    .await
                    .map_err(|e| AppError::validation(format!("json 字段读取失败: {e}")))?;
                json_body =
                    Some(serde_json::from_str(&text).map_err(|e| {
                        AppError::validation(format!("json 字段 JSON 解析失败: {e}"))
                    })?);
            }
            "pdf" | "file" => {
                let data = field
                    .bytes()
                    .await
                    .map_err(|e| AppError::validation(format!("pdf 字段读取失败: {e}")))?
                    .to_vec();
                pdf_files.push(data);
            }
            _ => {
                return Err(AppError::validation(format!(
                    "multipart 未知字段: '{name}'（仅接受 'json' / 'pdf' / 'file'）"
                )));
            }
        }
    }
    let req = json_body.ok_or_else(|| AppError::validation("multipart 缺少 'json' 字段"))?;
    let mut tx = state.pool.begin().await?;
    let out =
        PartService::batch_with_pdfs(&mut tx, &state.snowflake, &req, &pdf_files, &current).await?;
    tx.commit().await?;
    state.ws_hub.broadcast(WsEvent::DashboardEvent {
        kind: "PART_BATCH_WITH_PDFS_CREATED".into(),
        payload: json!({ "part_id": out.part.id.to_string() }),
    });
    Ok(Json(R::ok(out)))
}

// 仅为消除未用导入警告（类型由各 handler 签名自然带回）
#[allow(unused_imports)]
use crate::modules::part::dto_crud::PartDetailOut as _PartDetailOutShim;

/// `POST /api/v2/parts/batch-to-inspection`
///
/// 批量送检（共享品检架 + per-item to_inspection_core）。
///
/// 行为：
/// - 权限：`Manager` 或 `Inspector`
/// - 入参：`{ target_inspection_shelf_id, items: [...] }`
/// - 业务流转：service `batch_to_inspection`（共享外层事务 + per-item 独立 core）
/// - WS 广播：commit 后 `BATCH_TO_INSPECTION`
/// - 响应：`{ submitted, failed }`
pub async fn batch_to_inspection(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<BatchToInspectionRequest>,
) -> Result<Json<R<BatchToXxxOut>>, AppError> {
    current.require_any_role(&[Role::Manager, Role::Inspector])?;
    let mut tx = state.pool.begin().await?;
    let out = PartService::batch_to_inspection(&mut tx, &state.snowflake, req, &current).await?;
    tx.commit().await?;
    let mut seen_assemblies = std::collections::HashSet::new();
    for item in &out.submitted {
        if let Some(aid) = item.synced_assembly_id
            && seen_assemblies.insert(aid)
        {
            state.ws_hub.broadcast(WsEvent::DashboardEvent {
                kind: "ASSEMBLY_UPDATED".into(),
                payload: json!({ "assembly_id": aid.to_string() }),
            });
        }
    }
    state.ws_hub.broadcast(WsEvent::DashboardEvent {
        kind: "BATCH_TO_INSPECTION".into(),
        payload: json!({
            "submitted": out.submitted.len(),
            "failed": out.failed.len(),
        }),
    });
    Ok(Json(R::ok(out)))
}
