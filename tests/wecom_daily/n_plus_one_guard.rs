//! wecom_daily 域「取数恒 ≤ 6 条语句」的**源码级**护栏（2026-10-18）
//!
//! ## 这个文件防的是什么
//!
//! 本域的统计取数是一条硬承诺（照 `mail_digest` 域同款）：一次日报的语句数**恒 ≤ 6
//! 条**，与工单数 / 送货单数 / 工人数无关。把
//! `for r in workers { repo::worker_rows(conn, today).await? }` 改成「逐节循环查」是
//! **功能完全正确**的改法：数字守恒、渲染结果逐字一致、集成测试全绿（每条用例只造
//! 2~3 行），只有数据量放大时语句数才悄悄涨回去 —— 而日报恰好是「一天一次、量很大」
//! 的生产形态。
//!
//! ## 为什么是**源码扫描**而不是运行时计数
//!
//! 与 `mail_digest::n_plus_one_guard`、`com::delivery_note::sql_count_guard` 同款取舍：
//! sqlx 的 `QueryLogger` 按语句发 `target: "sqlx::query"` 的事件，装 subscriber 计数
//! 看似可行，但要把计数可靠归属到「这一次渲染」很别扭 —— `with_default` 只在当前线程
//! 生效而集成测试同进程并行跑多个 case；改用全局 subscriber 又会把别的 case 的语句算
//! 进来（多线程下还会静默少计，「条数相等」退化成 `0 == 0` 的假绿）。
//!
//! ## ⚠️ 本护栏的已知局限（**照抄 mail_digest 那份盲区清单**，明写出来，免得日后
//! 有人当它是完备证明 —— 本仓口径：**未登记的漏报形态一律视为护栏缺陷**，遇到新形态
//! 先补登记再改判定）：
//!
//! 1. **对源码形态敏感**：把循环写进宏、或把 `repo::` 调用交给另一个函数再在循环里
//!    调它，都能绕过。仓内不存在这种写法（`fetch_rows` 是本域唯一的取数编排点）。
//! 2. 只扫 `fetch_rows` 一个函数体：`repo.rs` 里的 SQL 定义本身天然是「一条函数一条
//!    SQL」，而写路径（`update_config` / `insert_log` 等）不在本护栏范围内 —— 它们是
//!    **写**不是读放大。
//! 3. 判定的是**调用点**而不是语句条数：`fetch_rows` 顶层有 6 个 `repo::` 调用点
//!    （第一节 2 + 第二节 1 + 第三节 2 + 第四节 1），**恒 ≤ 6 条**来自「关掉的节
//!    一条都不查」这条事实，由 `fetch_rows` 的 doc 登记。
//!    ⚠️ **「恒定 2 条」这类说法在本域是错的**：节开关拦的是「不能同时关」
//!    （`21804`），**不是**「只能开一个」，四节都开是缺省形态（那时恰好 6 条）。
//! 4. **迭代器链**：`rows.iter().map(|r| repo::worker_rows(…)).collect()` 里**没有
//!    `for` 关键字**，循环体扫不到。
//! 5. **`use` 之后的裸调用**：`use crate::…::repo::worker_rows;` 再在循环里写
//!    `worker_rows(…)`，调用点**没有 `repo::` 字面量**可匹配。
//! 6. **并发扇出**：`tokio::try_join!` / `futures::join_all` 在循环体里并发发起多条
//!    查询，逐条语句数照样线性涨，而两条护栏都绿。
//!
//! ## 变异验证
//!
//! 本护栏**刻意会红**：往 `fetch_rows` 里加一个**含 `repo::` 调用的 `for` 循环**即
//! 失败（2026-10-18 落地时实测过，见回执）。⚠️ 同一变异下
//! `fetch_rows_has_exactly_six_repo_call_sites` **不红**（它先把循环内的计数扣掉），
//! 两条护栏互为补充、缺一不可 —— 见该用例的 doc。

use std::path::{Path, PathBuf};

/// 被扫描的源文件（相对 `src/modules/wecom_daily/`）。
const SERVICE_RS: &str = "service.rs";

/// 被扫描的函数名（`fetch_rows` 是本域唯一把 `repo::` 编排起来的地方）。
const FN_FETCH_ROWS: &str = "fetch_rows";

/// `fetch_rows` 顶层**恰好**应有的 `repo::` 调用点数（第一节 2 + 第二节 1 + 第三节 2
/// + 第四节 1）。
const EXPECTED_TOP_LEVEL_REPO_CALLS: usize = 6;

/// 循环体内禁出现的调用前缀（任一命中即失败）。
const FORBIDDEN_IN_LOOP: &[&[u8]] = &[b"repo::", b"sqlx::query"];

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

/// 取 `fn <name>` 之后第一个 `{`…`}` 的**函数体**区间（不含大括号本身）。
fn fn_body(code: &[u8], name: &str) -> (usize, usize) {
    let needle = format!("fn {name}");
    let at = find_keyword(code, 0, needle.as_bytes())
        .unwrap_or_else(|| panic!("源码里找不到 `fn {name}`（函数被改名或删了？）"));
    let open = code[at..]
        .iter()
        .position(|c| *c == b'{')
        .map(|p| at + p)
        .unwrap_or_else(|| panic!("`fn {name}` 后找不到 `{{`"));
    let close = matching_brace(code, open).unwrap_or_else(|| panic!("`fn {name}` 的大括号不配平"));
    (open + 1, close)
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

/// 收集 `fetch_rows` 函数体内所有循环体：
/// `(关键字下标, 关键字位置, '{' 位置, 配平 '}' 位置)`。
///
/// 两条护栏**共用**这一个扫描：同一种源码形态只能有一个判定口径，各写一遍迟早漂。
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

/// ★ **主护栏**：`fetch_rows` 的循环体内不得出现 `repo::` / `sqlx::query`。
#[test]
fn no_repo_call_inside_loop_body_of_fetch_rows() {
    let path = domain_dir().join(SERVICE_RS);
    let src = std::fs::read_to_string(&path).expect("读 wecom_daily/service.rs");
    let code = blank_comments_and_strings(&src);
    let (lo, hi) = fn_body(&code, FN_FETCH_ROWS);

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
         正确改法是让每条 SQL 一次算完整组聚合（第一节 1 条四聚合、第二节 1 条两聚合、\
         第三节各 1 条、第四节 1 个 GROUP BY），循环里只允许做内存操作。",
        violations.join("\n")
    );
}

/// 数出「`repo::<标识符>(`」形态的**调用点**（类型标注 `Vec<repo::X>` 不计）。
fn count_repo_calls(code: &[u8], lo: usize, hi: usize) -> usize {
    let mut n = 0usize;
    let mut at = lo;
    while at < hi {
        let Some(rel) = code[at..hi].windows(6).position(|w| w == b"repo::") else {
            break;
        };
        let pos = at + rel;
        let mut p = pos + 6;
        while p < hi && code[p] == b' ' {
            p += 1;
        }
        if p < hi && is_ident(code[p]) {
            while p < hi && is_ident(code[p]) {
                p += 1;
            }
            if p < hi && code[p] == b'(' {
                n += 1;
            }
        }
        at = pos + 6;
    }
    n
}

/// 收集 `fetch_rows` 函数体内所有循环体的大括号区间（`[open, close]`，含大括号）。
fn loop_spans(code: &[u8], lo: usize, hi: usize) -> Vec<(usize, usize)> {
    loop_bodies(code, lo, hi)
        .into_iter()
        .map(|(_, _, open, close)| (open, close))
        .collect()
}

/// 合并重叠 / 包含的区间。
///
/// ⚠️ **嵌套循环时内层区间被外层区间包含**，不合并就会把同一处 `repo::` 调用点扣两次
/// ⇒ 减法下溢。合并后是正确求和，`checked_sub` 只留作兜底。
fn merged_spans(mut spans: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    spans.sort_unstable();
    let mut out: Vec<(usize, usize)> = Vec::new();
    for (s, e) in spans {
        match out.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => out.push((s, e)),
        }
    }
    out
}

/// `fetch_rows` 顶层恰好 6 个 `repo::` **调用点**
/// （`overview_events` / `in_process_count` / `delivery_note_summary` /
/// `count_overdue` / `count_upcoming` / `worker_rows`）。
///
/// 括号：顶层 = 不在任何循环体内的调用点（与上一条互为补充：那一条扫循环内，
/// 这一条钉总量）。**恒 ≤ 6 条**靠的是「关掉的节一条 SQL 都不发」——
/// 四节都开（缺省）时恰好 6 条，关一节少一条。
///
/// ⚠️ **本条单独不够**：把一条 `repo::` 调用挪进循环体时，总量与循环内计数**同时
/// +1**、两者相减后顶层仍是 6 ⇒ 本条**照样绿**。真正变红的是
/// `no_repo_call_inside_loop_body_of_fetch_rows`。两条必须**成对**存在。
#[test]
fn fetch_rows_has_exactly_six_repo_call_sites() {
    let path = domain_dir().join(SERVICE_RS);
    let src = std::fs::read_to_string(&path).expect("读 wecom_daily/service.rs");
    let code = blank_comments_and_strings(&src);
    let (lo, hi) = fn_body(&code, FN_FETCH_ROWS);

    let in_loop: usize = merged_spans(loop_spans(&code, lo, hi))
        .iter()
        .map(|(s, e)| count_repo_calls(&code, *s, *e))
        .sum();
    let all = count_repo_calls(&code, lo, hi);
    let top_level = all.checked_sub(in_loop).unwrap_or_else(|| {
        panic!(
            "{}:{} 循环体计数 {in_loop} 大于总调用点数 {all} —— 区间合并漏了？",
            rel(&path),
            line_of(&code, lo)
        )
    });
    assert_eq!(
        top_level,
        EXPECTED_TOP_LEVEL_REPO_CALLS,
        "{}:{} `fetch_rows` 顶层有 {top_level} 处 `repo::` 调用点，期望 {} 处\
         （overview_events / in_process_count / delivery_note_summary / count_overdue /\
         count_upcoming / worker_rows）。改取数结构时请同步本常量、`service.rs::fetch_rows`\
         的 doc、docs/api/wecom-daily.md §4「取数恒 ≤ 6 条语句」。",
        rel(&path),
        line_of(&code, lo),
        EXPECTED_TOP_LEVEL_REPO_CALLS,
    );
}
