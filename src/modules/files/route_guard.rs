//! files 域「`router()` 源码 ↔ `ROUTES` 常量」比对器的**共用实现**（2026-10-13
//! review 第 2 轮 N-3 / N-4）。
//!
//! ## 为什么抽到这里
//!
//! files 域有 **3 个** `router()` 工厂，各自都带一张人工维护的 `ROUTES` 表：
//!
//! | 文件 | 前缀 | `ROUTES` 条数 |
//! |---|---|---:|
//! | `handler.rs` | `/api/v2/files` | 1（`sts-tmp-keys`） |
//! | `part_file/handler.rs` | `/api/v2/files/objects` | 7 |
//! | `part_file/cnc/handler.rs` | `/api/v2/files/cnc` | 1 |
//!
//! N-4 指出 `cnc` 那条此前是**硬编码 `contains(...)` + `len() == 1`** ⇒ 加第二条
//! cnc 路由**不会红**，等于没有守卫。三处各抄一份解析器又会把「解析规则」变成
//! 三份可漂移的散文 —— 与 `repo::ALL_METHODS` 那条教训同款。故抽成本模块一处，
//! 三处 `mod tests` 共同调用。
//!
//! ## 解析规则（够用即可，不引构建期依赖）
//!
//! - 从 `pub fn <fn_name>()` 之后开始（⚠️ **必须切到该函数的收尾大括号**：
//!   `include_str!` 拿到的是整个文件，`mod tests` 自己的 doc 里也出现 `.route(`
//!   字面量，不切会把测试代码当路由表解析）；
//! - 以 `.route(` 为锚点，读紧随其后的字符串字面量作 path；
//! - 从该字面量末尾到**下一个** `.route(` / `.nest(`（或函数末）之间，出现
//!   `get(` 记 GET、出现 `post(` 记 POST；两者都出现则**各记一条**（`cnc` 之外的
//!   `"/"` 就是这种 `get(h).post(h)`）；出现别的 MethodRouter 直接 panic。
//!
//! ⚠️ **`.nest()` 不计入**：本域的 `.nest("/objects", part_file::router())` 是
//! 「把子模块的表挂上来」，其路由清单属于**子模块**自己的 `ROUTES`（已在子模块
//! 登记），在这里再记一遍就是重复计数。

/// 从 `src` 中解析 `pub fn <fn_name>()` 内的全部路由，返回 `["METHOD /path", …]`。
///
/// `fn_name` 不含 `()`，例如 `"router"`。
#[cfg(test)]
pub(crate) fn routes_in_router_source(src: &str, fn_name: &str) -> Vec<String> {
    let anchor = format!("pub fn {fn_name}()");
    let body = src
        .split_once(anchor.as_str())
        .unwrap_or_else(|| panic!("本文件必须含 `{anchor}`"))
        .1;
    // ⚠️ 必须切到该函数的收尾大括号，否则会把后续的 mod tests / 注释当成路由表。
    //    用「**独占一行**的 `}`」定位（而不是裸 `\n}\n`）—— 后者对带缩进的收尾
    //    大括号不成立，而函数体缩进层级随格式化工具 / 嵌套层级变化。
    let body = &body[..find_closing_brace(body, fn_name)];

    let mut out = Vec::new();
    let mut rest = body;
    while let Some(at) = rest.find(".route(") {
        rest = &rest[at + ".route(".len()..];
        let after_ws = rest.trim_start();
        assert!(
            after_ws.starts_with('"'),
            ".route( 后必须紧跟字符串字面量 path；实测：{after_ws:.40}"
        );
        let after_quote = &after_quote_of(after_ws);
        let (path, tail) = after_quote;
        // 分段到**下一个** `.route(` 或 `.nest(`（后者不计入，但会打断分段）
        let mut seg_end = tail.len();
        for marker in [".route(", ".nest("] {
            if let Some(i) = tail.find(marker) {
                seg_end = seg_end.min(i);
            }
        }
        let seg = &tail[..seg_end];
        assert!(
            !seg.contains("put(") && !seg.contains("delete(") && !seg.contains("patch("),
            "files 域各 router 只用 get/post；实测到别的 MethodRouter：{seg:.60}"
        );
        if seg.contains("get(") {
            out.push(format!("GET {path}"));
        }
        if seg.contains("post(") {
            out.push(format!("POST {path}"));
        }
        rest = &tail[seg_end..];
    }
    out
}

/// 取出 `"path"` 字面量的内容与其后的剩余部分（跳过闭合引号）。
#[cfg(test)]
fn after_quote_of(after_ws: &str) -> (&str, &str) {
    let after_quote = &after_ws[1..];
    let end = after_quote.find('"').expect("path 字面量必须有闭合引号");
    (&after_quote[..end], &after_quote[end..])
}

/// 定位 `fn <fn_name>()` 函数体的收尾大括号，返回其**起始下标**。
///
/// 做法：从 0 开始按**花括号配平**（跳过字符串字面量里的括号），配平回 0 的那个
/// 位置就是收尾大括号。比「找独占一行的 `}`」更稳 —— 后者在函数体带缩进、
/// 或收尾括号与其他内容同行时会失配（本模块自己的第 1 版就栽在这）。
#[cfg(test)]
fn find_closing_brace(body: &str, fn_name: &str) -> usize {
    let bytes = body.as_bytes();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i] as char;
        match c {
            '"' => in_str = !in_str,
            '{' if !in_str => depth += 1,
            '}' if !in_str => {
                depth -= 1;
                if depth == 0 {
                    return i;
                }
            }
            _ => {}
        }
        i += 1;
    }
    panic!("fn {fn_name}() 必须有配平的收尾大括号");
}

#[cfg(test)]
mod tests {
    use super::routes_in_router_source;

    /// 解析器自身的行为：多方法路由记两条、`.nest()` 不计入、缺 path 字面量时 panic。
    #[test]
    fn parser_handles_multi_method_and_ignores_nest() {
        let src = r#"
            pub fn router() -> Router<Arc<AppState>> {
                Router::new()
                    .route("/", get(a).post(b))
                    .nest("/objects", other::router())
                    .route("/{id}/x", get(c))
            }
        "#;
        assert_eq!(
            routes_in_router_source(src, "router"),
            vec!["GET /", "POST /", "GET /{id}/x"]
        );
    }

    /// 函数名不存在时 panic（避免「写错名字 ⇒ 静默返回空 vec ⇒ 断言 0 == 0 通过」）。
    #[test]
    #[should_panic(expected = "本文件必须含")]
    fn parser_panics_on_unknown_fn_name() {
        routes_in_router_source("pub fn router() {}", "router_v2");
    }
}
