//! wecom_daily 域取数「**恒 O(1)**」的**源码级**护栏（2026-10-23 重写）
//!
//! ## 这个文件防的是什么
//!
//! 本域的统计取数守一条不变量：**语句数与返回行数无关**。日报是「一天一次、量很大」
//! 的生产形态，把
//! `for r in workers { repo::worker_rows(conn, from, to).await? }` 改成「逐节循环
//! 查」是**功能完全正确**的改法：数字守恒、渲染结果逐字一致、集成测试全绿（每条用例
//! 只造 2~3 行），只有数据量放大时语句数才悄悄涨回去。
//!
//! ⚠️ **护栏不钉死「恰好 N 条」**：条数是推出来的（每节把整组聚合压进一条语句），
//! 而任何一次「第三节多一条查询」都会让它当场过期 —— 过期即噪声，噪声即被忽略。
//! 真正该守住的是**那条例外形态本身**：循环体内的取数。
//!
//! ## 扫描范围
//!
//! `service.rs` 的**全部函数**（`#[cfg(test)] mod tests` 之前的那一半），而不只是
//! `fetch_rows`：新增取数函数时**不用**记得把它加进白名单，扫不到才会变成「要人记得
//! 改护栏」的那种规矩。
//!
//! ## 为什么是**源码扫描**而不是运行时计数
//!
//! 与 `mail_digest::n_plus_one_guard`、`com::delivery_note::sql_count_guard` 同款取舍：
//! sqlx 的 `QueryLogger` 按语句发 `target: "sqlx::query"` 的事件，装 subscriber 计数
//! 看似可行，但要把计数可靠归属到「这一次渲染」很别扭 —— `with_default` 只在当前线程
//! 生效而集成测试同进程并行跑多个 case；改用全局 subscriber 又会把别的 case 的语句算
//! 进来（多线程下还会静默少计，「条数相等」退化成 `0 == 0` 的假绿）。
//!
//! ## ⚠️ 本护栏的已知局限（**明写出来，免得日后有人当它是完备证明** —— 本仓口径：
//! **未登记的漏报形态一律视为护栏缺陷**，遇到新形态先补登记再改判定）：
//!
//! 1. **对源码形态敏感**：把循环写进宏、或把 `repo::` 调用交给另一个函数再在循环里
//!    调它，都能绕过。仓内不存在这种写法（`fetch_rows` 是本域唯一的取数编排点）。
//! 2. 只扫 `service.rs`，且只扫 `#[cfg(test)] mod tests` **之前**的那一半：`repo.rs`
//!    里的 SQL 定义本身天然是「一条函数一条 SQL」，而写路径（`update_config` /
//!    `insert_log` 等）不在本护栏范围内 —— 它们是**写**不是读放大。
//! 3. **迭代器链**：`rows.iter().map(|r| repo::worker_rows(…)).collect()` 里**没有
//!    `for` 关键字**，循环体扫不到。
//! 4. **`use` 之后的裸调用**：`use crate::…::repo::worker_rows;` 再在循环里写
//!    `worker_rows(…)`，调用点**没有 `repo::` 字面量**可匹配。
//! 5. **并发扇出**：`tokio::try_join!` / `futures::join_all` 在循环体里并发发起多条
//!    查询，逐条语句数照样线性涨，而本护栏绿。
//!
//! ## 变异验证
//!
//! 本护栏**刻意会红**：往 `service.rs` 任一循环体里加一行 `repo::xxx(conn, …)` 即
//! 失败（2026-10-23 落地时实测过，见回执）。

use std::path::{Path, PathBuf};

/// 被扫描的源文件（相对 `src/modules/wecom_daily/`）。
const SERVICE_RS: &str = "service.rs";

/// 扫描的**下界标记**：`#[cfg(test)] mod tests` 从这里开始的内容不扫。
///
/// ⚠️ 为什么砍掉单测模块：单测里的循环是**造 fixture** 的（`for` 里插数据），扫它
/// 只会把「测试代码允许循环」这条事实写成一串白名单例外 —— 而生产代码里那半边才是
/// 真正要守的地方。marker 找不到就 panic（见主护栏），不会静默通过。
const SCAN_STOPS_AT: &str = "mod tests";

/// 循环体内禁出现的调用前缀（任一命中即失败）。
///
/// ⚠️ `sqlx::query` 本身已是 `sqlx::query_as` / `sqlx::query_scalar` / `sqlx::query!`
/// 的前缀，这里再逐个列出来只为**失败信息可读**：命中时文案直接写出是哪一种形态，
/// 而不是让人自己数前缀长度。
const FORBIDDEN_IN_LOOP: &[&[u8]] = &[
    b"repo::",
    b"sqlx::query",
    b"sqlx::query_as",
    b"sqlx::query_scalar",
];

fn domain_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src/modules/wecom_daily")
}

/// 把 Rust 源码换成「注释与字符串字面量内容已空格化」的等价源码。
///
/// ⚠️ 必须空格化**而不是删行**：护栏的失败信息要给出精确 `file:line`，删行会让行号失真。
/// 与 `tests/mail_digest/n_plus_one_guard.rs` 的实现**刻意各自私有**（不反向依赖，
/// 也不共用一个 helper crate —— 两份复制品各跟各自域的演进走）。
fn blank_comments_and_strings(src: &str) -> Vec<u8> {
    let b = src.as_bytes();
    let mut out = b.to_vec();
    let mut i = 0usize;
    while i < b.len() {
        if b[i] == b'/' && b.get(i + 1) == Some(&b'/') {
            while i < b.len() && b[i] != b'\n' {
                out[i] = b' ';
                i += 1;
            }
            continue;
        }
        if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
            let mut depth = 1usize;
            while i < b.len() {
                if b[i] == b'\n' {
                    i += 1;
                    continue;
                }
                if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                    depth += 1;
                    out[i] = b' ';
                    out[i + 1] = b' ';
                    i += 2;
                    continue;
                }
                if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                    depth -= 1;
                    out[i] = b' ';
                    out[i + 1] = b' ';
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                    continue;
                }
                out[i] = b' ';
                i += 1;
            }
            continue;
        }
        // raw string（`r"…"` / `r#"…"#` 及其 b/c 前缀变体）
        let is_raw = b[i..].starts_with(b"r\"")
            || b[i..].starts_with(b"r#\"")
            || b[i..].starts_with(b"br\"")
            || b[i..].starts_with(b"br#\"")
            || b[i..].starts_with(b"cr\"")
            || b[i..].starts_with(b"cr#\"");
        if is_raw {
            let mut p = i + 1;
            let mut hashes = 0usize;
            while b.get(p) == Some(&b'#') {
                hashes += 1;
                p += 1;
            }
            if b.get(p) == Some(&b'"') {
                p += 1;
                let mut closed = false;
                while p < b.len() {
                    if b[p] == b'"' {
                        let mut h = 0usize;
                        while b.get(p + 1 + h) == Some(&b'#') {
                            h += 1;
                        }
                        if h == hashes {
                            for slot in out.iter_mut().take(p + h + 1).skip(i) {
                                *slot = b' ';
                            }
                            i = p + h + 1;
                            closed = true;
                            break;
                        }
                    }
                    if b[p] != b'\n' {
                        out[p] = b' ';
                    }
                    p += 1;
                }
                if closed {
                    continue;
                }
            }
        }
        if b[i] == b'"' {
            let mut j = i + 1;
            while j < b.len() {
                if b[j] == b'\\' {
                    j += 2;
                    continue;
                }
                if b[j] == b'"' || b[j] == b'\n' {
                    break;
                }
                j += 1;
            }
            let end = j.min(b.len().saturating_sub(1));
            for slot in out.iter_mut().take(end + 1).skip(i + 1) {
                if *slot != b'\n' {
                    *slot = b' ';
                }
            }
            i = end + 1;
            continue;
        }
        i += 1;
    }
    out
}

fn is_ident(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// 「`for` / `while` / `loop` 关键字 + 词边界」的下标。
fn find_keyword(code: &[u8], from: usize, kw: &[u8]) -> Option<usize> {
    let mut from = from;
    while from < code.len() {
        let rel = code[from..].windows(kw.len()).position(|w| w == kw)?;
        let at = from + rel;
        from = at + 1;
        let prev_ok = at == 0 || !is_ident(code[at - 1]);
        let next_ok = !code.get(at + kw.len()).is_some_and(|c| is_ident(*c));
        if prev_ok && next_ok {
            return Some(at);
        }
    }
    None
}

/// 从 `open`（必为 `{`）起做括号配平，返回配平大括号的下标。
fn matching_brace(code: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut k = open;
    while k < code.len() {
        match code[k] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(k);
                }
            }
            _ => {}
        }
        k += 1;
    }
    None
}

fn line_of(code: &[u8], at: usize) -> usize {
    1 + code[..at].iter().filter(|c| **c == b'\n').count()
}

fn count_occurrences(hay: &[u8], needle: &[u8]) -> usize {
    if needle.is_empty() || hay.len() < needle.len() {
        return 0;
    }
    hay.windows(needle.len()).filter(|w| *w == needle).count()
}

/// 被扫描区间 = `[0, `mod tests` 之前)`。
///
/// ⚠️ **扫整段而不是「逐个函数」**：这一段里的每个循环都属于某个函数（Rust 不允许
/// 文件级的裸循环语句），逐个函数定位只是把同一件事写两遍 —— 而两份写法迟早会对
/// 「某个函数名改没改」产生分歧。找不到 marker 直接 panic（响亮失败），不静默通过。
fn production_region(code: &[u8]) -> (usize, usize) {
    let hi = find_keyword(code, 0, SCAN_STOPS_AT.as_bytes()).unwrap_or_else(|| {
        panic!(
            "{} 里找不到 `{}` —— 护栏的扫描下界没了（单测模块被改名/删除？）",
            rel(&domain_dir().join(SERVICE_RS)),
            SCAN_STOPS_AT
        )
    });
    (0, hi)
}

/// 相对 crate 根的可读路径（失败信息用）。
fn rel(path: &Path) -> String {
    path.strip_prefix(env!("CARGO_MANIFEST_DIR"))
        .unwrap_or(path)
        .display()
        .to_string()
}

/// 三个循环关键字（下标 = [`loop_bodies`] 里的 `kw_idx`，只用于失败文案）。
const LOOP_KWS: [&str; 3] = ["for", "while", "loop"];

/// 收集 `[lo, hi)` 区间内所有循环体：
/// `(关键字下标, 关键字位置, '{' 位置, 配平 '}' 位置)`。
///
/// 大括号不配平的（源码残缺）循环直接丢弃 —— 那时整体解析都不可信，响亮失败交给
/// 编译器。
fn loop_bodies(code: &[u8], lo: usize, hi: usize) -> Vec<(usize, usize, usize, usize)> {
    let mut found = Vec::new();
    for (kw_idx, kw) in LOOP_KWS.iter().copied().enumerate() {
        let mut from = lo;
        while from < hi {
            let Some(at) = find_keyword(&code[from..hi], 0, kw.as_bytes()).map(|p| from + p) else {
                break;
            };
            from = at + kw.len();
            let Some(open) = code[at..hi].iter().position(|c| *c == b'{').map(|p| at + p) else {
                continue;
            };
            let Some(close) = matching_brace(code, open) else {
                continue;
            };
            if close < hi {
                found.push((kw_idx, at, open, close));
            }
        }
    }
    found
}

/// ★ **主护栏**：`service.rs` 生产代码里的**任何**循环体内都不得出现
/// `repo::` 调用或 `sqlx::query` / `query_as` / `query_scalar`。
///
/// 这是「取数恒 O(1)」那条不变量的**唯一**执行点：语句数与行数无关，等价于「循环体
/// 里不取数」（取数都在函数体顶层，一个开关一次）。⚠️ **刻意不钉死具体条数** —— 条数
/// 是推出来的，写进常量只会在下一次「某节多一条查询」时立刻过期，而过期的断言等于
/// 没有断言。
#[test]
fn no_repo_call_inside_any_loop_body_of_service() {
    let path = domain_dir().join(SERVICE_RS);
    let src = std::fs::read_to_string(&path).expect("读 wecom_daily/service.rs");
    let code = blank_comments_and_strings(&src);
    let (lo, hi) = production_region(&code);

    let mut violations: Vec<String> = Vec::new();
    for (kw_idx, at, open, close) in loop_bodies(&code, lo, hi) {
        for forbidden in FORBIDDEN_IN_LOOP {
            let n = count_occurrences(&code[open..=close], forbidden);
            if n > 0 {
                violations.push(format!(
                    "{}:{}  `{}` 循环体内出现 {n} 处 `{}`",
                    rel(&path),
                    line_of(&code, at),
                    LOOP_KWS[kw_idx],
                    String::from_utf8_lossy(forbidden)
                ));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "wecom_daily 取数禁止按行数重复查询（下面是违规点）：\n{}\n\
         正确改法是让每条 SQL 一次算完整组聚合，循环里只允许做内存操作。",
        violations.join("\n")
    );
}
