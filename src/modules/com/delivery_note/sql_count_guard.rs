//! com::delivery_note 域「只读端点 SQL 条数」的源码级护栏
//!
//! 2026-10-08 review 第 1 轮 M3 新增。
//!
//! ## 这个模块防的是什么
//!
//! 本域两条**纯读**新端点的全部结构性价值就是「固定 N 条 SQL」：
//! - `GET /scan/{serial_no}` —— 批次层一条 `part_id = ANY($1)` 取回整棵树，
//!   前端从「1 + 1 + M 个请求」降到 1 个请求；
//! - `GET /drivers` —— 一条 SQL 出候选下拉。
//!
//! 把 `ANY($1)` 改回 `for p in parts { query_batches(p) }` 是**功能完全正确**的
//! 改法：字段集合同形、held 总数守恒、几乎所有集成测试照样全绿，只有零件数放大时
//! 请求数悄悄涨回去 —— 集成测试每条用例只造 2~3 个零件，测不出来。
//!
//! ## 为什么是**结构断言**（数源码里的调用点）而不是运行时计数
//!
//! 运行时计数也可行（sqlx 的 `QueryLogger` 逐语句发 `target: "sqlx::query"` 的 DEBUG
//! 事件，装一个按该 target 计数的 subscriber 即可断言「2 子件与 10 子件两次请求的
//! 条数相等」），但计数要可靠归属到「本次请求」很别扭：
//! `tracing::subscriber::with_default` 只在当前线程生效，而集成测试是同进程内并行跑
//! 多个 case，得改用全局 subscriber + 按 span 归属计数，或 `Instrument` 把本请求的
//! dispatch 带进 tokio worker 线程 —— 后者在 `#[tokio::test]` 由 current_thread
//! 改成 multi_thread 时会静默少计，「条数相等」退化成 0 == 0 的假绿。
//!
//! ## 判定规则（4 条，缺一条护栏就有洞）
//!
//! 1. **禁 N+1**：扫 [`READ_PATH_SQL_SOURCES`] 列出的那几个文件（两条纯读端点的
//!    SQL 真源与调用链）的**代码区**（注释与字符串字面量内容先被空格化，行号保持
//!    不变），凡 `for` / `while` / `loop` 循环体（大括号配平范围内）内出现
//!    `sqlx::query`（含 `query_as!` / `query_scalar!` 等全部同族形式）即失败；
//!    ⚠️ **写路径不在扫描范围内**：`service/crud.rs::remove_batches` 按 `batch_ids`
//!    逐条 `UPDATE t_part_batch`（条数受单据行数上界约束、且是写操作不是读放大），
//!    把整个域扫进来会天天报它，而那不是本护栏要拦的东西。
//! 2. **一条 repo 方法 = 一条 SQL**：`repo/scan_tree.rs` 的 6 个 `pub async fn` 与
//!    `repo/driver.rs::list_drivers` 各恰好 1 处 `sqlx::query`；
//! 3. **调用点重数被钉死**：`service/scan_tree.rs` 的 `scan_tree` 函数体里每个 SQL
//!    发射点的出现次数等于 [`SCAN_TREE_SQL_CALL_SITES`] 登记的常量；
//! 4. **文档 ↔ 常量对账**：`repo/scan_tree.rs` 模块 doc 的条数表逐行等于
//!    [`SCAN_TREE_SQL_COUNTS`] 登记的常量。
//!
//! 规则 3+4 合起来才是「每个分支几条 SQL」的可执行版本：每条分支的条数 = 该分支
//! 命中的调用点个数 + 恒定的 4 条尾巴，登记在 [`SCAN_TREE_SQL_COUNTS`] 里；
//! 文档里写错数字、改代码加了查询、或两处不同步，三者中任意一个发生都会红。
//!
//! 规则 2 与规则 3 是刻意「会红」的：真要加第 7 条 SQL 时它会挡住，那正是它该做的
//! 事（加 SQL 必须同步 `repo/scan_tree.rs` 的条数表、`docs/api/delivery_note.md`
//! §3 的口径表与本模块的常量）。失败信息里已写明要改哪几处。
//!
//! ## 扫描范围
//!
//! 只扫 [`READ_PATH_SQL_SOURCES`]（两条纯读端点的 SQL 真源 + 调用链），不扫整个域：
//! 写路径确有「循环内发 SQL」的形态（`service/crud.rs::remove_batches` 按
//! `batch_ids` 逐条 UPDATE），把它算进来会让护栏天天报一条与「读放大」无关的噪声。
//!
//! ## 已知绕过口
//!
//! - 循环写在**宏**里 / `sqlx::query` 的调用点被 `include!` 拼进来（仓库内不存在）；
//! - 条件编译（`#[cfg(feature = …)]`）的两条分支各有一条查询：单看代码仍是
//!   「每个分支至多一条」，恒定性成立，故不算绕过口；
//! - `resolve_draft` 的早退分支（`parts` 为空时少 2 条）：已在本模块的常量与
//!   `repo/scan_tree.rs` 条数表里显式排除，不靠护栏兜。
//!
//! ## 探测器与 `prod::queue::board` 的分工
//!
//! 那条守「board 聚合不 import 别的域」，本条守「不按行数重复查 + 一共几条」。
//! 探测器同款（括号配平 + 关键字扫描 + 空格化），但各自实现：那份是 queue 域的私有
//! `#[cfg(test)] mod`，本域不该反向依赖 `prod` 的测试内私有物。

use std::path::{Path, PathBuf};

/// `GET /scan/{serial_no}` 单次请求各分支的 SQL 条数登记表。
///
/// 与 `repo/scan_tree.rs` 模块 doc 的条数表、`docs/api/delivery_note.md` §3 的
/// 「SQL 条数」口径行**三处必须一致**。含义：命中段调用点数 + 恒定 4 条尾巴。
const SCAN_TREE_SQL_COUNTS: &[(&str, usize)] = &[
    ("独立件", 5),
    ("装配件子件（父装配件活跃）", 7),
    ("装配件子件（父装配件已软删）", 6),
    ("装配件条码", 7),
    ("两表皆未命中", 2),
];

/// 命中之后**无分支恒发**的 SQL 条数（`list_batches_by_part_ids` +
/// `list_entryable_batches_by_part_ids` + `l1_of` + `note_find_open_draft_by_l1`）。
const SCAN_TREE_TAIL_SQL: usize = 4;

/// `service/scan_tree.rs` 的 `scan_tree` 函数体里，各 SQL 发射点的出现次数。
///
/// 每项都等于 1 处 `sqlx::query` 的 repo 方法；`note_find_open_draft_by_l1` 与
/// `l1_of` 住在 `resolve_draft` 里，另有一条断言单独钉。
const SCAN_TREE_SQL_CALL_SITES: &[(&str, usize)] = &[
    // 命中探测：无论扫到零件还是未命中都发一次
    ("find_part_by_serial", 1),
    // 装配件条码回退（`part` 未命中时）
    ("find_assembly_by_serial", 1),
    // 扫到子件时取父装配件节点
    ("find_assembly_by_id", 1),
    // 2 处：父装配件活跃的子件树 / 装配件码树
    ("list_parts_by_assembly", 2),
    // 恒 1 处（命中段之后）
    ("list_batches_by_part_ids", 1),
    ("list_entryable_batches_by_part_ids", 1),
];

/// `resolve_draft` + `l1_of` 里两个跨域 / 跨 repo 调用点的出现次数。
const RESOLVE_DRAFT_SQL_CALL_SITES: &[(&str, usize)] = &[
    ("note_find_open_draft_by_l1", 1),
    ("CustomerRepo::get_by_id", 1),
];

// ===========================================================================
//  源码扫描探测器
// ===========================================================================

fn domain_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src/modules/com/delivery_note")
}

/// 把 Rust 源码换成「注释与字符串字面量内容已空格化」的等价源码。
///
/// 与 `prod::queue::board::sql_count_guard_tests` 同款策略：空格化而非整段删除，
/// **行号保持不变**，失败信息能给出精确 `file:line`。
fn blank_comments_and_strings(src: &str) -> Vec<u8> {
    let b = src.as_bytes();
    let mut out = b.to_vec();
    let mut i = 0usize;
    while i < b.len() {
        // 行注释
        if b[i] == b'/' && b.get(i + 1) == Some(&b'/') {
            while i < b.len() && b[i] != b'\n' {
                out[i] = b' ';
                i += 1;
            }
            continue;
        }
        // 块注释（Rust 支持嵌套）
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
        // raw string（`r"…"` / `r#"…"#` / `br#"…"#` / `cr#"…"#`）
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
            // 未闭合（源码一定编译得过，理论不可达）：按普通字符继续，避免死循环
        }
        // 普通字符串（`"` 起点；转义后跳过）
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
        .unwrap_or_else(|| panic!("源码里找不到 `fn {name}`（方法被改名或删了？）"));
    let open = code[at..]
        .iter()
        .position(|c| *c == b'{')
        .map(|p| at + p)
        .unwrap_or_else(|| panic!("`fn {name}` 后找不到 `{{`"));
    let close = matching_brace(code, open).unwrap_or_else(|| panic!("`fn {name}` 的大括号不配平"));
    (open + 1, close)
}

/// 相对 `src/` 的可读路径（失败信息用）。
fn rel(path: &Path) -> String {
    path.strip_prefix(env!("CARGO_MANIFEST_DIR"))
        .unwrap_or(path)
        .display()
        .to_string()
}

// ===========================================================================
//  规则 1：循环体内禁 SQL
// ===========================================================================

/// 两条纯读端点的 SQL 真源与调用链（相对 `src/modules/com/delivery_note/`）。
///
/// 规则 1 的扫描范围。改 SQL 落点（repo ↔ service 之间搬家）时要同步这里。
const READ_PATH_SQL_SOURCES: &[&str] = &[
    "repo/scan_tree.rs",
    "repo/driver.rs",
    "service/scan_tree.rs",
    "service/shippable_sets.rs",
    "handler/scan.rs",
];

#[test]
fn no_sqlx_query_inside_loop_body() {
    let mut files = Vec::new();
    for rel_path in READ_PATH_SQL_SOURCES {
        let p = domain_dir().join(rel_path);
        assert!(
            p.is_file(),
            "READ_PATH_SQL_SOURCES 里的 `{rel_path}` 不存在（文件被改名？）"
        );
        files.push(p);
    }

    let mut violations: Vec<String> = Vec::new();
    for path in &files {
        let src = std::fs::read_to_string(path).expect("读 delivery_note 源码");
        let code = blank_comments_and_strings(&src);
        for kw in ["for", "while", "loop"] {
            let mut from = 0usize;
            while let Some(at) = find_keyword(&code, from, kw.as_bytes()) {
                from = at + kw.len();
                let Some(open) = code[at..].iter().position(|c| *c == b'{').map(|p| at + p) else {
                    continue;
                };
                let Some(close) = matching_brace(&code, open) else {
                    continue;
                };
                if count_occurrences(&code[open..=close], b"sqlx::query") > 0 {
                    violations.push(format!(
                        "{}:{}  `{kw}` 循环体内出现 `sqlx::query`",
                        rel(path),
                        line_of(&code, at)
                    ));
                }
            }
        }
    }
    assert!(
        violations.is_empty(),
        "送货单域只读端点禁止按行数重复查询（下面是违规点）：\n{}\n\
         正确改法是把 `ANY($1)` 拉成一条 SQL；确需分组查询时先改本域的条数登记表，\
         不要绕过这条护栏。",
        violations.join("\n")
    );
}

// ===========================================================================
//  规则 2：一条 repo 方法 = 一条 SQL
// ===========================================================================

/// `repo/scan_tree.rs` 逐方法应当恰好 1 处 `sqlx::query` 的方法名清单。
const SCAN_TREE_REPO_METHODS: &[&str] = &[
    "find_part_by_serial",
    "find_assembly_by_serial",
    "find_assembly_by_id",
    "list_parts_by_assembly",
    "list_batches_by_part_ids",
    "list_entryable_batches_by_part_ids",
];

#[test]
fn each_scan_tree_repo_method_issues_exactly_one_sql() {
    let path = domain_dir().join("repo/scan_tree.rs");
    let src = std::fs::read_to_string(&path).expect("读 repo/scan_tree.rs");
    let code = blank_comments_and_strings(&src);

    for name in SCAN_TREE_REPO_METHODS {
        let (lo, hi) = fn_body(&code, name);
        let n = count_occurrences(&code[lo..hi], b"sqlx::query");
        assert_eq!(
            n,
            1,
            "{}:{} `DeliveryScanRepo::{name}` 里有 {n} 处 `sqlx::query`，期望恰好 1 处。\
             合并成一条（或拆成两个方法并同步条数登记表）时，请同步改：\
             (1) 本文件的该方法 doc、(2) `repo/scan_tree.rs` 模块 doc 的条数表、\
             (3) `docs/api/delivery_note.md` §3 的「SQL 条数」口径行、\
             (4) `com::delivery_note::sql_count_guard_tests` 的 \
             `SCAN_TREE_REPO_METHODS` 与 `SCAN_TREE_SQL_COUNTS`。",
            rel(&path),
            line_of(&code, lo),
        );
    }

    // 兜底：这张 repo 文件里不允许出现登记表之外的新方法（新增方法忘了登记 ⇒ 这里红）
    let declared = count_occurrences(&code, b"pub async fn");
    assert_eq!(
        declared,
        SCAN_TREE_REPO_METHODS.len(),
        "{} 里 `pub async fn` 有 {declared} 个，而 `SCAN_TREE_REPO_METHODS` 只登记了 {} 个 ——\
         新增/删除 repo 方法时必须同步登记表并重算 §3 的条数表。",
        rel(&path),
        SCAN_TREE_REPO_METHODS.len(),
    );
}

#[test]
fn drivers_list_issues_exactly_one_sql() {
    let path = domain_dir().join("repo/driver.rs");
    let src = std::fs::read_to_string(&path).expect("读 repo/driver.rs");
    let code = blank_comments_and_strings(&src);
    let (lo, hi) = fn_body(&code, "list_drivers");
    let n = count_occurrences(&code[lo..hi], b"sqlx::query");
    assert_eq!(
        n,
        1,
        "{}:{} `DeliveryDriverRepo::list_drivers` 里有 {n} 处 `sqlx::query`，期望恰好 1 处\
         （`GET /drivers` 的全部结构性价值就是这一条）。拆成多条时请同步 \
         `docs/api/delivery_note.md` §1.3 与本护栏。",
        rel(&path),
        line_of(&code, lo),
    );
}

// ===========================================================================
//  规则 3：调用点重数被钉死
// ===========================================================================

#[test]
fn scan_tree_sql_call_sites_are_pinned() {
    let path = domain_dir().join("service/scan_tree.rs");
    let src = std::fs::read_to_string(&path).expect("读 service/scan_tree.rs");
    let code = blank_comments_and_strings(&src);

    let (lo, hi) = fn_body(&code, "scan_tree");
    let body = &code[lo..hi];
    for (name, want) in SCAN_TREE_SQL_CALL_SITES {
        let got = count_occurrences(body, name.as_bytes());
        assert_eq!(
            got,
            *want,
            "{}:{} `scan_tree` 里 `{name}` 出现 {got} 次，期望 {want} 次。\
             多一次 = 每个分支多一条 SQL；少一次 = 少取了数据。改代码时请同步：\
             (1) `repo/scan_tree.rs` 模块 doc 的条数表、\
             (2) `docs/api/delivery_note.md` §3 的「SQL 条数」口径行、\
             (3) 本模块的 `SCAN_TREE_SQL_CALL_SITES` 与 `SCAN_TREE_SQL_COUNTS`。",
            rel(&path),
            line_of(&code, lo),
        );
    }

    // `resolve_draft` + `l1_of` 两个函数体合计的调用点
    let (r_lo, r_hi) = fn_body(&code, "resolve_draft");
    let (l_lo, l_hi) = fn_body(&code, "l1_of");
    let tail = [&code[r_lo..r_hi], &code[l_lo..l_hi]].concat();
    for (name, want) in RESOLVE_DRAFT_SQL_CALL_SITES {
        let got = count_occurrences(&tail, name.as_bytes());
        assert_eq!(
            got,
            *want,
            "{}:{} `resolve_draft` / `l1_of` 合计里 `{name}` 出现 {got} 次，期望 {want} 次。\
             这两条构成每条命中分支恒发的 {SCAN_TREE_TAIL_SQL} 条尾巴里的 2 条，改动时请同步 \
             `repo/scan_tree.rs` 模块 doc 与本模块常量。",
            rel(&path),
            line_of(&code, r_lo),
        );
    }
}

// ===========================================================================
//  规则 4：文档 ↔ 常量对账
// ===========================================================================

/// 取 `repo/scan_tree.rs` 模块 doc 的条数表：`| 标签 | … | N |` 形式的行。
///
/// 只解析末列是纯数字的行 —— 条数表是文档里**唯一**带「末列为裸数字」的行组，
/// 其余 `//!` 行（含「恒 4 条的尾巴」那段散文）都取不到。
fn doc_count_rows(src: &str) -> Vec<(String, usize)> {
    let mut out = Vec::new();
    for line in src.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("//! | ") else {
            continue;
        };
        let Some(cells) = rest.strip_suffix(" |") else {
            continue;
        };
        let last = match cells.rsplit("|").next() {
            Some(c) => c.trim(),
            None => continue,
        };
        let Ok(n) = last.parse::<usize>() else {
            continue;
        };
        let label = cells
            .split('|')
            .next()
            .unwrap_or_default()
            .trim()
            .to_string();
        out.push((label, n));
    }
    out
}

#[test]
fn scan_tree_doc_table_matches_registered_counts() {
    let path = domain_dir().join("repo/scan_tree.rs");
    let src = std::fs::read_to_string(&path).expect("读 repo/scan_tree.rs");

    // 模块 doc = 文件开头那段**连续**的 `//!`（遇到第一条非 `//!` 行即止），
    // 避免把下面各方法的 `///` doc 也吃进来。
    let doc_end = src
        .lines()
        .position(|l| !l.trim_start().starts_with("//!"))
        .unwrap_or(src.lines().count());
    let doc: String = src.lines().take(doc_end).collect::<Vec<_>>().join("\n");

    let rows = doc_count_rows(&doc);
    assert_eq!(
        rows.len(),
        SCAN_TREE_SQL_COUNTS.len(),
        "`repo/scan_tree.rs` 模块 doc 的条数表解析出 {} 行，与登记的 {} 行不符 —— \
         两处必须逐条对应（改一处忘了另一处）。行数：{rows:?}",
        rows.len(),
        SCAN_TREE_SQL_COUNTS.len(),
    );
    for ((doc_label, doc_n), (want_label, want_n)) in rows.iter().zip(SCAN_TREE_SQL_COUNTS) {
        assert_eq!(
            doc_label, want_label,
            "条数表行标签与登记表不符（文档：{doc_label} / 登记：{want_label}）"
        );
        assert_eq!(
            doc_n, want_n,
            "分支 `{doc_label}` 的 SQL 条数：文档写 {doc_n}、登记写 {want_n}。\
             逐条数一遍 `scan_tree` 的该分支并同时改正两处 + \
             `docs/api/delivery_note.md` §3 的口径行。"
        );
    }
}

/// `SCAN_TREE_TAIL_SQL` 必须等于登记表里「非未命中分支」的最小值减去它们共有的
/// 命中段调用数。
///
/// 这条把「尾巴 4 条」与「逐分支条数」绑在一起：改 `SCAN_TREE_TAIL_SQL` 却忘了
/// 同步 `SCAN_TREE_SQL_COUNTS`（或反之）时立刻红，而不是等到线上请求数变了才被发现。
#[test]
fn tail_constant_is_consistent_with_registered_counts() {
    // 除「两表皆未命中」外的 4 条分支都含恒定尾巴；未命中在命中段就 return Err。
    let with_tail: Vec<usize> = SCAN_TREE_SQL_COUNTS
        .iter()
        .filter(|(label, _)| *label != "两表皆未命中")
        .map(|(_, n)| *n)
        .collect();
    assert_eq!(
        with_tail.len(),
        4,
        "`SCAN_TREE_SQL_COUNTS` 应当恰好 4 条分支带恒定尾巴（未命中那条在命中段就返回错误）"
    );
    // 独立件是最短的一条：命中段 1 条 + 尾巴。
    assert_eq!(
        with_tail[0] - SCAN_TREE_TAIL_SQL,
        1,
        "「独立件」分支的条数应等于命中段 1 条 + 恒定尾巴 {SCAN_TREE_TAIL_SQL} 条"
    );
    // 其余分支的命中段条数必须落在 1..=3（装配件码那条含一次未命中探测，共 3）。
    for (label, n) in SCAN_TREE_SQL_COUNTS
        .iter()
        .filter(|(l, _)| *l != "两表皆未命中")
    {
        let hit = n - SCAN_TREE_TAIL_SQL;
        assert!(
            (1..=3).contains(&hit),
            "分支 `{label}` 的条数 {n} 减掉尾巴 {SCAN_TREE_TAIL_SQL} 得命中段 {hit} 条，\
             超出 1..=3 的范围 —— 请复核 `repo/scan_tree.rs` 的条数表"
        );
    }
    // 装配件条码分支的命中段 = 「装配件子件（父活跃）」的命中段（都含 find_part_by_serial
    // 探测 + find_assembly_* + list_parts_by_assembly）。
    assert_eq!(
        SCAN_TREE_SQL_COUNTS[1].1, SCAN_TREE_SQL_COUNTS[3].1,
        "「装配件子件（父装配件活跃）」与「装配件条码」两条分支命中段调用点数应相同（各 3 条）"
    );
}
