pub mod dto;
pub mod handler;
pub mod repo;
pub mod service;
pub mod vo;

use crate::state::AppState;
use axum::Router;
use axum::routing::get;
use std::sync::Arc;

/// `/ws/*` 入口（WebSocket）：当前唯一端点 `/ws/dashboard`
///
/// 在 `modules::ws_router()` 下挂 `/ws` 前缀（不带 `/api/v2`）。
pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/dashboard", get(handler::ws_dashboard))
}

/// `/api/v2/dashboard/*` HTTP 端点。
///
/// 三个端点都是只读，任何已登录用户可访问（无角色闸门），故不逐个挂
/// `CurrentUser` 之外的授权层。
pub fn http_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/snapshot", get(handler::get_snapshot))
        .route("/upcoming-delivery", get(handler::get_upcoming_delivery))
        .route("/delivery-orders", get(handler::get_delivery_orders))
}

#[cfg(test)]
mod tests {
    //! 域隔离护栏：把「dashboard 域不依赖其它域」从口头约定变成 CI 强制。

    use std::path::{Path, PathBuf};

    /// 被检查的跨域 import 前缀拆成两段 `concat!` 拼接。
    ///
    /// 写死完整字面量会让本文件自己也命中自己的检测器（自指误报）——两段分开写，
    /// 源码里就不存在可匹配的完整前缀。
    const PREFIX_A: &str = concat!("use crate::", "modules::");

    /// 本域唯一被允许出现的域名段。
    const OWN_DOMAIN: &str = concat!("dash", "board");

    /// 递归列 `dir` 下的全部 `.rs`（目录深度不限）。
    fn collect_rs(dir: &Path, acc: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut entries: Vec<_> = entries.flatten().map(|e| e.path()).collect();
        // 排序只为让失败信息稳定可读
        entries.sort();
        for path in entries {
            if path.is_dir() {
                collect_rs(&path, acc);
            } else if path.extension().is_some_and(|e| e == "rs") {
                acc.push(path);
            }
        }
    }

    /// 返回一行里所有「引用了其它域」的列号（1 基）。
    fn foreign_domain_columns(line: &str) -> Vec<usize> {
        let mut hits = Vec::new();
        let mut from = 0usize;
        while let Some(at) = line[from..].find(PREFIX_A) {
            let abs = from + at;
            let rest = &line[from + at + PREFIX_A.len()..];
            let segment: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if segment != OWN_DOMAIN {
                hits.push(abs);
            }
            from = abs + PREFIX_A.len();
        }
        hits
    }

    /// dashboard 域只允许 `crate::` 下的 `auth` / `infra` / `shared` / `state`
    /// 与 `crate::modules::dashboard::*`；引用任何其它 `crate::modules::<他域>` 即失败。
    #[test]
    fn dashboard_domain_depends_on_no_other_domain() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut files = Vec::new();
        collect_rs(&root.join("src/modules/dashboard"), &mut files);
        assert!(
            !files.is_empty(),
            "扫描不到任何 .rs（CARGO_MANIFEST_DIR={}），护栏本身失效",
            root.display()
        );

        let mut violations: Vec<String> = Vec::new();
        for path in &files {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");
            let Ok(src) = std::fs::read_to_string(path) else {
                continue;
            };
            for (idx, line) in src.lines().enumerate() {
                for col in foreign_domain_columns(line) {
                    violations.push(format!("  {rel}:{}:{}", idx + 1, col));
                }
            }
        }

        assert!(
            violations.is_empty(),
            "以下 {} 处让 dashboard 域依赖了其它域（跨域耦合会让「改一域崩另一域」）：\n{}\n\
             \n\
             规则：\
             \x20 * 允许 —— {} 域自身、auth / infra / shared / state；\
             \x20 * 禁止 —— 任何 `modules::<其它域>` 的 import。\n\
             \n\
             需要别的域的数据时，正确做法是像 statistics / admin 那样在本域 SQL 里\
             只读聚合（dashboard 的 5 张表见 docs/api/dashboard.md），而不是 import\
             别人的 service / repo。",
            violations.len(),
            violations.join("\n"),
            OWN_DOMAIN
        );
    }

    /// 元测试：护栏本身必须能报警，否则「全绿」只是因为探测器瞎了。
    #[test]
    fn guard_detector_actually_flags_a_foreign_domain() {
        let bad = format!("{PREFIX_A}statistics::repo::sql::count_overdue");
        assert_eq!(foreign_domain_columns(&bad), vec![0]);

        let own = format!("{PREFIX_A}{OWN_DOMAIN}::snapshot::DashboardService");
        assert!(
            foreign_domain_columns(&own).is_empty(),
            "本域 import 不该被报警：{own}"
        );
    }
}
