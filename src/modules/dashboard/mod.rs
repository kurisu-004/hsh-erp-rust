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
    //!
    //! ## 探测口径（2026-10-07）
    //! 在**剥掉注释**的代码区里找「路径段 `<他域根>` + 双冒号 + 一个域名标识符」，
    //! 标识符不是本域即违规。相比「逐行找 `use crate::` 字面量」，这样写覆盖面大得多：
    //! - **不要求 `use` 前缀** —— `let s = crate::<根>::part::…::new();` 这种全限定
    //!   内联路径（Rust 里完全合法的写法）照样命中；
    //! - **不要求两段路径相邻** —— `use crate::{<他域根>::part::…}` 这种嵌套花括号
    //!   写法同样命中；
    //! - **注释不算代码** —— 为讲解规则而引用的外来路径不会误报。
    //!
    //! 边界：只挡「他域」依赖，**不挡** `crate::shared` / `crate::infra` /
    //! `crate::auth` / `crate::state` 等公共设施依赖（那些是刻意允许的）。
    //!
    //! 另有一条口径边界：匹配的是 `modules` 与 `::` **紧邻**的写法，Rust 允许在
    //! `modules` 与 `::` 之间插空白（`crate::modules :: part::…` 同样合法），本探测器
    //! 不覆盖这种形态。要覆盖它得上词法分析，规则本身是讲解材料，不值得为它加成本。

    use std::path::{Path, PathBuf};

    /// 跨域路径的根段 + 双冒号。刻意拆成两段 `concat!` —— 写死完整字面量会让本
    /// 文件自己也命中自己的探测器（自指误报）。同理，`SEG` 之后一律用 `format!`
    /// 拼装外来域名样例，源码里永远不出现「根段 + 外来域名」的完整字面量。
    const SEG: &str = concat!("modu", "les::");

    /// 本域唯一被允许出现的域名段。
    const OWN_DOMAIN: &str = "dashboard";

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

    /// 去掉注释内容、**保留字符位置**（用等量空格顶替，失败信息里的列号才对得上）。
    ///
    /// 跳过行注释（`//` / `///` / `//!`，含行尾注释）与块注释（`/* */` / `/** */`）；
    /// 字符串字面量内的 `//` **不**当注释（否则 `"http://…"` 会把整行后半截吃掉）。
    /// 返回 `(去注释后的行, 该行结束时是否仍处于块注释内)`。
    fn blank_comments(line: &str, mut in_block: bool) -> (String, bool) {
        let chars: Vec<char> = line.chars().collect();
        let mut out: Vec<char> = Vec::with_capacity(chars.len());
        let mut in_str = false;
        let mut escaped = false;
        let mut i = 0usize;
        while i < chars.len() {
            let c = chars[i];
            let next = chars.get(i + 1).copied();
            if in_block {
                if c == '*' && next == Some('/') {
                    out.push(' ');
                    out.push(' ');
                    i += 2;
                    in_block = false;
                    continue;
                }
                out.push(' ');
                i += 1;
                continue;
            }
            if in_str {
                out.push(c);
                if escaped {
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == '"' {
                    in_str = false;
                }
                i += 1;
                continue;
            }
            match (c, next) {
                ('/', Some('/')) => {
                    // 行注释（含 `///` / `//!`）：本行剩余部分整体置空
                    out.extend(std::iter::repeat_n(' ', chars.len() - i));
                    i = chars.len();
                }
                ('/', Some('*')) => {
                    out.push(' ');
                    out.push(' ');
                    i += 2;
                    in_block = true;
                }
                _ => {
                    if c == '"' {
                        in_str = true;
                    }
                    out.push(c);
                    i += 1;
                }
            }
        }
        (out.into_iter().collect(), in_block)
    }

    /// 一行代码里所有「引用了其它域」的片段，每项 = `(1 基列号, 域名段)`。
    ///
    /// 域名为空串也算违规：那对应「只引命名空间本身」与「glob 引入全部域」两种写法，
    /// 前者在本域拿不到任何专属符号、后者把其它域一并拖进来。
    fn foreign_domain_hits(code: &str) -> Vec<(usize, String)> {
        let mut hits = Vec::new();
        let mut from = 0usize;
        while let Some(rel) = code[from..].find(SEG) {
            let abs = from + rel;
            let rest = &code[abs + SEG.len()..];
            let seg: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if seg != OWN_DOMAIN {
                hits.push((code[..abs].chars().count() + 1, seg));
            }
            // `from` 前进到本次匹配末尾即可：域名段由标识符字符构成，不可能再嵌 `SEG`
            from = abs + SEG.len();
        }
        hits
    }

    /// 扫一份源码，返回所有违规 `(行号, 列号, 域名段)`（行号 1 基）。
    fn scan_source(src: &str) -> Vec<(usize, usize, String)> {
        let mut violations = Vec::new();
        let mut in_block = false;
        for (idx, raw) in src.lines().enumerate() {
            let (code, next_block) = blank_comments(raw, in_block);
            in_block = next_block;
            for (col, seg) in foreign_domain_hits(&code) {
                violations.push((
                    idx + 1,
                    col,
                    if seg.is_empty() {
                        "<空段/glob>".to_string()
                    } else {
                        seg
                    },
                ));
            }
        }
        violations
    }

    /// dashboard 域只允许 `crate::` 下的 `auth` / `infra` / `shared` / `state`
    /// 与本域自身；代码区里出现任何其它域的路径即失败。
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
            for (line, col, seg) in scan_source(&src) {
                violations.push(format!("  {rel}:{line}:{col}  →  {SEG}{seg}"));
            }
        }

        assert!(
            violations.is_empty(),
            "以下 {} 处让 dashboard 域依赖了其它域（跨域耦合会让「改一域崩另一域」）：\n{}\n\
             \n\
             规则：\
             \x20 * 允许 —— {} 域自身、auth / infra / shared / state；\
             \x20 * 禁止 —— 代码区（注释除外）里任何 `{SEG}<其它域>` 路径，含 `use` 前缀、\
             \x20   全限定内联路径、嵌套花括号写法、glob 四种形态。\n\
             \n\
             需要别的域的数据时，正确做法是像 statistics / admin 那样在本域 SQL 里\
             只读聚合（dashboard 的 5 张表见 docs/api/dashboard.md），而不是 import\
             别人的 service / repo。",
            violations.len(),
            violations.join("\n"),
            OWN_DOMAIN
        );
    }

    // ── 元测试：探测器本身必须能报警，否则「全绿」只是因为探测器瞎了 ──────────

    /// 正例：四种**代码区**写法都必须被抓到。
    /// 覆盖盲区 ① 嵌套花括号 ② 全限定内联路径 ③ glob ④ `pub use`。
    ///
    /// ⚠️ 带花括号的样例一律用数组 `.concat()` 拼、不用 `format!`：`format!` 里
    /// 想同时表达「字面花括号」与「具名参数」得写 `{{` / `}}` 转义，再把域名放到
    /// `{}` 内部就极易读错（曾把域名错放到括号**外**，样例退化成无意义代码）。
    #[test]
    fn guard_flags_foreign_domain_in_code_any_shape() {
        let brace = [
            "use crate::{",
            SEG,
            "statistics::repo::sql::count_overdue};",
        ]
        .concat();
        assert!(
            brace.contains('{') && brace.contains('}'),
            "样例里花括号丢了，测的就不是花括号分支：{brace}"
        );
        assert_eq!(
            scan_source(&brace),
            vec![(1, 13, "statistics".to_string())],
            "嵌套花括号写法不该漏过"
        );

        let brace_inline = ["let s = crate::{", SEG, "part::repo::PartRepo::new()};"].concat();
        assert_eq!(
            scan_source(&brace_inline),
            vec![(1, 17, "part".to_string())],
            "全限定 + 花括号混写不该漏过"
        );

        let inline = format!("let s = crate::{SEG}part::service::PartService::new();");
        assert_eq!(
            scan_source(&inline),
            vec![(1, 16, "part".to_string())],
            "全限定内联路径（无 `use` 前缀）不该漏过"
        );

        let glob = ["use crate::{", SEG, "*};"].concat();
        assert_eq!(
            scan_source(&glob),
            vec![(1, 13, "<空段/glob>".to_string())],
            "glob 引入会把所有域拖进来，不该漏过"
        );

        let with_vis = format!("pub use crate::{SEG}assembly::repo::AssemblyRepo;");
        assert_eq!(
            scan_source(&with_vis),
            vec![(1, 16, "assembly".to_string())],
            "`pub use` 形态不该漏过"
        );
    }

    /// 反例：本域引用、以及**注释里**提到的外来路径，一律不许报警
    /// （否则为讲解规则写一行文档注释就得红）。
    #[test]
    fn guard_stays_quiet_on_own_domain_and_comments() {
        let own = ["use crate::{", SEG, OWN_DOMAIN, "::vo::*};"].concat();
        assert!(
            scan_source(&own).is_empty(),
            "本域 import（含花括号写法）不该被报警：{own}"
        );

        let inline_own = format!("let x = crate::{SEG}{OWN_DOMAIN}::dto::DeliveryBasis::System;");
        assert!(
            scan_source(&inline_own).is_empty(),
            "本域全限定内联路径不该被报警：{inline_own}"
        );

        for comment in [
            ["/// 详见 crate::{", SEG, "statistics::repo::sql"].concat(),
            format!("//! 改口径时对照 crate::{SEG}part::repo"),
            format!("/* 历史包袱：crate::{SEG}part::repo */"),
            ["let n = 1; // 旧实现走 crate::{", SEG, "part::repo"].concat(),
            format!("/**\n * 举例：crate::{SEG}part::repo\n */\nlet n = 1;"),
            format!("//! 本段列的路径都指向 dashboard 域\nuse crate::{SEG}{OWN_DOMAIN}::vo::V;"),
        ] {
            assert!(
                scan_source(&comment).is_empty(),
                "注释里的外来路径不该被报警：{comment}"
            );
        }
    }

    /// 字符串字面量里的 `//` 不当注释 —— 否则 `"http://…"` 会把整行后半截吃掉，
    /// 真正写在同一行尾部的外来 import 就漏过了。
    #[test]
    fn guard_does_not_mistake_url_for_line_comment() {
        let src = format!("let url = \"http://x/y\";\nuse crate::{SEG}statistics::repo::R;");
        assert_eq!(
            scan_source(&src),
            vec![(2, 12, "statistics".to_string())],
            "URL 里的 `//` 被当成注释会让第 2 行的真实违规漏过"
        );
    }
}
