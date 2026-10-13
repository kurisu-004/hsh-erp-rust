//! part 域批量创建 handler
//!
//! 对应：
//! - `POST /api/v2/parts/batch` —— 批量创建（共享 `customer_id`）；per-item 失败
//!   不中断整体；支持 `drawing_file` / `model3d_file` 单 item 文件绑定（场景 A 直传
//!   COS 链路，2026-09-16 M2-B 新增）。
//!
//! ## 移除记录（2026-10-13：零消费方端点整条下线）
//!
//! - `POST /api/v2/parts/batch-with-pdfs` (multipart) —— **零消费方**：前端
//!   `src/api/parts/batch.ts` 里那处引用是**主动弃用注释**（原文：「入参没有
//!   `items` 数组、服务端自己造一个 `装配件-{今天}` 并按页数拆子件，**承载不了
//!   Tab 2 的逐行录入**。逐行建单一律走 `batchCreateParts` + 后置上传」），同文件
//!   6 个 `export async function` **无一**打它；`backend-python` 侧只有
//!   `.env.example` 的一行注释。随端点删除：`dto_crud::BatchWithPdfsRequest` 与
//!   `service::PartService::batch_with_pdfs`。
//!   ⚠️ **旧路径的响应形态：POST → `HTTP 405`，GET → `HTTP 400` 纯文本，两者都不是
//!   404**（实测钉死，守卫用例见 `tests/part/create_serial_price.rs` 的
//!   `batch_with_pdfs_endpoint_is_gone_and_old_paths_are_no_longer_routed`）：
//!   `batch-with-pdfs` 是 **1 段**静态路径，路由删除后落进 `/{part_id}` 那条
//!   catch-all —— 而**那条只注册了 GET** ⇒ matchit 先判路径再判方法，POST 直接
//!   `Method Not Allowed`；同一路径改用 GET 时方法对上了，才轮到 `Path<i64>` 去解
//!   `"batch-with-pdfs"` ⇒ `Invalid URL: ...` 400 纯文本（**没有业务信封**）。
//!   ⚠️ 同仓同款先例：`prod::worker` 的 `POST /verify-badge` 迁往 `prod::scan`
//!   之后，旧路径 `/prod/workers/verify-badge` 也是 405。⇒ **「1 段静态路径落进
//!   catch-all」有两种落点，取决于 catch-all 注册了哪些方法**，别当成一条规则。
//! - 连带失效的 WS 事件 `PART_BATCH_WITH_PDFS_CREATED` **自此无人发送**
//!   （与 `ROLLUP_RECOMPUTED` 同款死订阅，按产品决定本轮只登记、不动事件定义）。
//!
//! 2026-10-13 PR1b：`POST /api/v2/parts/{part_id}/files/confirm` **自本文件删除**，
//! 硬切为 `POST /api/v2/files/objects/confirm`（**无 alias，旧路径 404**），
//! handler 与 service 调用一并搬进 `files::part_file::handler`。⚠️ 本域仍保留
//! `POST /parts/batch` 的 **bindings 分支**（`service/batch.rs::
//! prepare_binding_head_copy`）—— 它的端点本体是「批量建零件」，文件只是可选入参，
//! 不是一条独立的文件端点，故刻意留在 part 域。
//!
//! ## 错误码（batch 的 bindings 分支）
//! - 21114 `BIZ_PART_FILE_TMP_OBJECT_MISSING` — head_object 失败（tmp 不存在）
//! - 21115 `BIZ_PART_FILE_SIZE_MISMATCH` — head size 与声明 size 不一致
//! - 21108 `BIZ_PART_FILE_DUPLICATE` — 同 part + kind + sha256 撞唯一索引
//!
//! ## 异步清理契约（2026-09-16 M2-B 第 2 轮）
//! - batch_create_parts：head/copy 成功的 tmp_keys **全部** 透出到
//!   `out.cleanup_tmp_keys`（无论 per-item DB 结果如何），handler commit 后
//!   **统一** spawn delete 兜底；commit 失败时也 spawn（DB 未持久化时 tmp 必须删）。
//! - confirm 端点（同款纪律）的 handler 已搬至 `files::part_file::handler::
//! confirm_file_object`：commit 后 spawn delete 单个 tmp_key。

use std::sync::Arc;

use axum::Json;
use axum::extract::State;

use crate::auth::rbac::CurrentUser;
use crate::modules::part::dto_crud::PartBatchCreateRequest;
use crate::modules::part::service::PartService;
use crate::modules::part::vo::PartBatchCreateOut;
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
                &mut *tx,
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
                PartService::batch_create_parts(&mut *tx, &state.snowflake, &req, &current).await;
            res.map(|out| (out, Vec::new()))
                .map_err(|e| (e, Vec::new()))
        };
    // 2026-09-16 M2-B review 第 2 轮 B1 修：必须在 tx.commit() 之前拿到 cleanup_tmp_keys，
    // 否则 tx.commit().await 持有 conn 时跨 .await 容易踩 sqlx 的 connection-held-across-await
    // 警告。Ok/Err 都先 move 出 keys 再 commit（Err 路径也透出 — M2-C 修）。
    //
    // **登记（2026-10-05）：Err 分支也 `commit()` 而不是 `rollback()`，是既有行为。**
    // 两个 service 入口的 Err 出口分两类：
    // - 循环之前（一条行都没写 ⇒ 「commit 空事务」与 rollback 等价）：
    //   `CustomerRepo::get_by_id` → 20102、`serial_prefix_for_customer` →
    //   20102 / 20308 / 20104、with_bindings 独有的第一遍 head/copy 失败；
    // - 循环之内：**两个入口的第二遍循环第一条语句都是 `shared::serial::acquire`**
    //   （20108 / 20105，位置在 per-item SAVEPOINT 之前），失败条件是 prefix 级
    //   （prefix 未在 `t_serial_counter` 注册 / 序列号池耗尽）⇒ 同 prefix 的第一次
    //   acquire 就会失败，0 行落库。循环内其余 Err 出口（`SAVEPOINT` /
    //   `ROLLBACK TO SAVEPOINT` / `RELEASE SAVEPOINT` 语句失败、INSERT 之后的
    //   detail 回读失败）全是 DB 语句级失败，等价于「数据库或连接出问题」，
    //   此时部分落库不是首要后果。
    //
    // **何时会被引爆**：往第二遍循环里加**业务级**的 per-item `?`（如金额校验、
    // 跨表引用校验）时，它可能在第 k 件（1 < k ≤ N）才失败 ⇒ 前 k-1 件的 INSERT
    // 会被 commit 掉而客户端收到错误信封。届时应把这里改成 `Err ⇒ tx.rollback()`，
    // 或让 service 用 savepoint 兜住 per-item 错误只往 `failed[]` 里塞。
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
