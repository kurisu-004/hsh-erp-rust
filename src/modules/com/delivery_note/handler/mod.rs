//! com::delivery_note 域 HTTP handler 总入口
//!
//! 按业务域拆分为多文件（沿 `part/handler/{crud,batch,lifecycle,inspection}.rs` 范本）：
//! - `crud.rs` —— 基础 CRUD：list / get / update / remove-batches / soft-delete /
//!   batch-detail / candidate-parts / pickup-pending + P1 送货分组 CRUD
//! - `lifecycle.rs` —— 状态机转换：submit / recall / pickup-scan / pickup
//! - `print.rs` —— 打印：print / print-labels（读本单批次算装配件可出货套数后
//!   BFF 转发到 python 执行渲染）
//! - `scan.rs` —— 扫码入单：scan（`/scan` 端点）+ attach-batches（弹窗提交时附挂批次）
//!
//! ## ⚠️ 路由注册顺序是**硬约束**（matchit 要求静态段优先）
//! `note_router()` 里 1 段静态路径（`/batch-detail`、`/scan`、`/candidate-parts`、
//! `/pickup-pending`）**必须先于** `/{id}` 注册，否则 axum 会在 nest 构建期直接
//! panic（不是运行期 404）。新增 1 段静态端点时照本段顺序插入，且同步
//! [`ROUTES`]。
//!
//! ## 约定（2026-09-22 D-5 + review 第 1 轮）
//! - 事务边界在 handler：`state.pool.begin()` → 借 `&mut *tx` 喂给 service → 显式
//!   `tx.commit()`；提前 return（`?`）时 `Transaction` 的 Drop 自动回滚。
//! - **service 形参 by-value trait**（iam 严格范本）：handler 借 `&mut *tx` 给
//!   `state.delivery_note_service.xxx(&mut *tx, ...)` 或 `&mut *conn` 给读端点。
//! - **handler 三形态**：
//!   - ① 纯写端点 `pool.begin() → service → commit`；
//!   - ② 写 + post-commit Redis / WS（broadcast 落 handler，service 不持有 WsHub）`pool.begin() → service → commit → state.ws_hub.broadcast(...)`；
//!   - ③ 读端点（list_*/get_*）`pool.acquire() → service`，不开事务。
//! - 统一响应信封：`Result<Json<R<T>>, AppError>`。
//! - 权限在 service 层（`current.require_any_role(...)`）；handler 这里只解析
//!   query / path / body。
//!
//! ## axum 0.8 path 占位符
//! `matchit` 风格 `{id}`（不再用 axum 0.7 的 `:id`）。

pub mod crud;
pub mod lifecycle;
pub mod print;
pub mod scan;

use axum::Router;
use axum::routing::{get, post};
use std::sync::Arc;

use crate::state::AppState;

/// 本域路由表（挂载点见 `com::mod.rs` 的 `.nest("/delivery", ...)`；
/// 本表只登记 `/note` 段，`/group` 段的 4 条见 [`GROUP_ROUTES`]）。
///
/// ⚠️ **静态段必须早于 `/{id}`**（见本文件模块 doc 的顺序硬约束）。axum 的
/// `matchit` 路由树要求静态分支先于参数分支匹配，否则 nest 构建期 panic。
///
/// 单测 `routes_declared_in_router` 逐条比对本表与 [`note_router`] 源码 —— 往
/// `note_router()` 加一条 `.route(...)` 而忘了登记本表，该测试立刻红。
pub const ROUTES: &[&str] = &[
    "GET /batch-detail",
    "POST /scan",
    "GET /candidate-parts",
    "GET /pickup-pending",
    "GET /",
    "POST /",
    "GET /{id}/events",
    "POST /{id}/update",
    "POST /{id}/add-parts",
    "POST /{id}/attach-batches",
    "POST /{id}/remove-parts",
    "POST /{id}/submit",
    "POST /{id}/recall",
    "POST /{id}/pickup-scan",
    "POST /{id}/pickup",
    "POST /{id}/soft-delete",
    "POST /{id}/print",
    "POST /{id}/print-labels",
    "GET /{id}",
];

/// 送货分组段路由表（`/api/v2/com/delivery/group`），与 [`ROUTES`] 同形登记。
pub const GROUP_ROUTES: &[&str] = &[
    "GET /",
    "POST /",
    "POST /{id}/update",
    "POST /{id}/soft-delete",
];

/// 送货单段路由（相对 `/api/v2/com/delivery/note`）。
pub fn note_router() -> Router<Arc<AppState>> {
    Router::new()
        // ---- ★静态段必须早于 /{id} ----
        .route("/batch-detail", get(crud::batch_get_delivery_notes))
        .route("/scan", post(scan::scan_delivery_note))
        .route("/candidate-parts", get(crud::list_candidate_parts))
        .route("/pickup-pending", get(crud::list_pickup_pending))
        .route(
            "/",
            get(crud::list_delivery_notes).post(crud::create_delivery_note),
        )
        .route("/{id}/events", get(crud::list_delivery_note_events))
        .route("/{id}/update", post(crud::update_delivery_note))
        .route("/{id}/add-parts", post(crud::add_delivery_note_parts))
        .route("/{id}/attach-batches", post(scan::attach_batches))
        .route("/{id}/remove-parts", post(crud::remove_delivery_note_parts))
        .route("/{id}/submit", post(lifecycle::submit_delivery_note))
        .route("/{id}/recall", post(lifecycle::recall_delivery_note))
        .route("/{id}/pickup-scan", post(lifecycle::pickup_scan))
        .route("/{id}/pickup", post(lifecycle::pickup_delivery_note))
        .route("/{id}/soft-delete", post(crud::soft_delete_delivery_note))
        .route("/{id}/print", post(print::print_delivery_note))
        .route("/{id}/print-labels", post(print::print_labels))
        .route("/{id}", get(crud::get_delivery_note))
}

/// 送货分组段路由（相对 `/api/v2/com/delivery/group`）。
///
/// 权限 Manager + Clerk + Inspector（与送货单段同一组角色，见
/// `service/group.rs`）。`/{id}/update` / `/{id}/soft-delete` 的 `{id}` 收
/// `Path<i64>`（分组 id 直传，无雪花序列化问题）。
pub fn group_router() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/",
            get(crud::p1_list_delivery_groups).post(crud::p1_create_delivery_group),
        )
        .route("/{id}/update", post(crud::p1_update_delivery_group))
        .route(
            "/{id}/soft-delete",
            post(crud::p1_soft_delete_delivery_group),
        )
}

// ===========================================================================
//  Unit tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::{GROUP_ROUTES, ROUTES};

    /// 从 `router()` 函数体源码里抠出全部 `METHOD /path`。
    ///
    /// 解析规则（对本文件的书写方式足够，且不引入构建期依赖）：
    /// - 以 `.route(` 为锚点，跳过空白后读一个 `"…"` 字面量作 path；
    /// - 从该字面量末尾到**下一个** `.route(`（或函数体末）之间出现 `get(` 记
    ///   GET、出现 `post(` 记 POST；两者同时出现（`.route("/", get(..).post(..))`）
    ///   就吐两条 —— 登记表是「一方法一条」，多方法路由拆开登记。
    fn routes_in(src: String) -> Vec<String> {
        let mut out = Vec::new();
        let mut rest = src.as_str();
        while let Some(at) = rest.find(".route(") {
            rest = &rest[at + ".route(".len()..];
            let after_ws = rest.trim_start();
            assert!(
                after_ws.starts_with('"'),
                ".route( 后必须紧跟字符串字面量 path；实测：{after_ws:.40}"
            );
            let after_quote = &after_ws[1..];
            let end = after_quote.find('"').expect("path 字面量必须有闭合引号");
            let path = &after_quote[..end];
            let tail = &after_quote[end..];
            let seg_end = tail.find(".route(").unwrap_or(tail.len());
            let seg = &tail[..seg_end];
            if seg.contains("get(") {
                out.push(format!("GET {path}"));
            }
            if seg.contains("post(") {
                out.push(format!("POST {path}"));
            }
            assert!(
                seg.contains("get(") || seg.contains("post("),
                "`.route(\"{path}\")` 后既无 get( 也无 post(：{seg:.60}"
            );
            rest = &tail[seg_end..];
        }
        out
    }

    /// 只取 `note_router` 的函数体源码：`pub fn note_router()` 起，到
    /// `pub fn group_router()` 之前为止。
    fn note_router_src() -> String {
        fn_body(
            "pub fn note_router() -> Router",
            "pub fn group_router() -> Router",
        )
    }

    /// 只取 `group_router` 的函数体源码：`pub fn group_router()` 起，到本文件
    /// 末尾的「Unit tests」横幅之前为止（横幅用 `// ===` 起行，不会误伤函数体）。
    fn group_router_src() -> String {
        fn_body("pub fn group_router() -> Router", "// ===")
    }

    /// 取 `start_marker` 第一次出现之后、`end_marker` 第一次出现之前的那段源码。
    ///
    /// 两个 marker 都必须是**在本文件中唯一且顺序固定**的字符串：marker 没找到或
    /// 顺序反了就 panic（静默取错区间会让比对测试变成「比对了一坨无关文本仍
    /// 通过 / 失败」，失去牙齿）。
    fn fn_body(start_marker: &str, end_marker: &str) -> String {
        let src = include_str!("mod.rs");
        let after_start = src
            .split_once(start_marker)
            .unwrap_or_else(|| panic!("本文件必须有 `{start_marker}`"))
            .1;
        let end = after_start
            .find(end_marker)
            .unwrap_or_else(|| panic!("`{start_marker}` 之后必须能找到 `{end_marker}`"));
        after_start[..end].to_string()
    }

    /// [`ROUTES`] 必须与 `note_router()` 源码**逐条**一致（不多、不少、同序）。
    ///
    /// 这是登记表系列测试真正有牙齿的地方：`ROUTES` 是人工 const，往
    /// `note_router()` 加一条 `.route(...)` 而忘了登记时，靠这条比对立刻红。
    #[test]
    fn routes_declared_in_router() {
        for (label, actual, want) in [
            ("note", routes_in(note_router_src()), ROUTES),
            ("group", routes_in(group_router_src()), GROUP_ROUTES),
        ] {
            assert_eq!(
                actual.len(),
                want.len(),
                "`{label}_router()` 注册了 {} 条路由，登记表只列了 {} 条（新增端点忘了登记？）\n\
                 actual: {:?}\n登记表: {:?}",
                actual.len(),
                want.len(),
                actual,
                want,
            );
            for (got, expected) in actual.iter().zip(want.iter()) {
                assert_eq!(
                    got,
                    expected,
                    "`{label}` 段路由表第 {} 条不一致",
                    want.len()
                );
            }
        }
    }

    /// 登记表内无重复条目（手抄错行号时立刻发现）。
    #[test]
    fn routes_have_no_duplicates() {
        for (label, table) in [("note", ROUTES), ("group", GROUP_ROUTES)] {
            let mut seen = std::collections::BTreeSet::new();
            for route in table {
                assert!(!route.is_empty(), "`{label}` 段有空路由条目");
                assert!(seen.insert(*route), "`{label}` 段 `{route}` 重复了");
            }
        }
    }
}
