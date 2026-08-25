//! P4：送货单 Excel 打印（print_xlsx）。CPU 密集部分包 `spawn_blocking`。

use std::collections::HashMap;
use std::path::Path;

use sqlx::PgPool;

use crate::auth::rbac::{CurrentUser, Role};
use crate::modules::customer::repo::CustomerRepo;
use crate::shared::error::{code, AppError};

use super::inner::{customer_not_found, get_with_parts};
use super::super::print::{self, PrintRequest};

use super::DeliveryNoteService;

impl DeliveryNoteService {
    /// 打印送货单 → xlsx bytes（含 `line_item_ids` 子集支持，labels 路径复用同入口）
    ///
    /// 流程：
    /// 1. 开 tx → `get_with_parts(note_id)` 拿 detail（head + line_items）；
    /// 2. 加载 L1 客户（取 `serial_prefix` → 模板 key）；
    /// 3. 派生 `PrintRequest` → `tokio::task::spawn_blocking` 调同步
    ///    `print::render_note` / `render_labels`（CPU 密集 umya 工作）；
    /// 4. tx.commit()（print 只读 tx，commit 不做事）。
    ///
    /// `custom_order` / `merge_assemblies` / `merge_quantities` / `line_item_ids`
    /// 全在 print.rs 内校验（事务前不进入 DB）。
    ///
    /// 备注：`is_labels` 由 `line_item_ids.is_some()` 推断；label 渲染走
    /// `print::render_labels`，否则 `print::render_note`。
    #[allow(clippy::too_many_arguments)]
    pub async fn print_xlsx(
        pool: &PgPool,
        note_id: i64,
        custom_order: Option<Vec<i64>>,
        merge_assemblies: bool,
        merge_quantities: HashMap<i64, i32>,
        line_item_ids: Option<Vec<i64>>,
        template_dir: &Path,
        current: &CurrentUser,
    ) -> Result<(Vec<u8>, char), AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;

        let mut tx = pool.begin().await?;
        let detail = get_with_parts(&mut tx, note_id).await?;

        let l1 = CustomerRepo::get_by_id(&mut *tx, detail.head.customer_id, false)
            .await?
            .ok_or_else(|| customer_not_found(detail.head.customer_id))?;
        let prefix_upper = l1
            .serial_prefix
            .as_deref()
            .and_then(|s| s.chars().next())
            .map(|c| c.to_ascii_uppercase())
            .ok_or_else(|| {
                AppError::biz_with_status(
                    code::BIZ_DELIVERY_TEMPLATE_NOT_CONFIGURED,
                    format!(
                        "customer {} ({}) 缺序列号前缀；请先在客户编辑里填 L1 serial_prefix (A-Z)",
                        l1.id, l1.name
                    ),
                    axum::http::StatusCode::BAD_REQUEST,
                )
            })?;

        let template_path = print::template_path(prefix_upper, template_dir)?;
        let is_labels = line_item_ids.is_some();
        let req = PrintRequest {
            prefix: prefix_upper,
            template_path,
            note_detail: detail,
            custom_order,
            merge_assemblies,
            merge_quantities,
            line_item_ids,
        };
        let req_for_blocking = req;

        let out = tokio::task::spawn_blocking(move || -> Result<(Vec<u8>, char), AppError> {
            if req_for_blocking.is_labels() {
                print::render_labels(&req_for_blocking)
            } else {
                print::render_note(&req_for_blocking)
            }
        })
        .await
        .map_err(|e| AppError::internal(format!("print spawn_blocking join: {e}")))??;

        let _ = is_labels;
        tx.commit().await?;
        Ok(out)
    }
}
