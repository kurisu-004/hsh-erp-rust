//! prod::queue 队列板子模块（只读聚合，2026-10-08 新增）
//!
//! 两个只读端点的实现：
//! - `GET /api/v2/prod/queue/snapshot` —— 工序序列板
//! - `GET /api/v2/prod/queue/processes/{process_id}` —— 单工序板
//!
//! ## 为什么要独立成子模块而不是塞进 `repo/`
//!
//! 域隔离护栏 `assert_no_foreign_domain` 收的是**目录**路径。要让「board 聚合
//! SQL 零跨域依赖」这条规则可被 CI 执行，被扫描的代码必须自成一个目录。
//! queue 域整体**不适用**该护栏（它继承 worker_pool 的「经本域 trait 转发其它域
//! 单表查询」pattern，见 `repo/mod.rs` 顶部记档），但 board 这部分聚合 SQL 是
//! 纯只读的、完全可以零跨域 —— 把它圈出来单独守，比整域不守要强。
//!
//! ## SQL 条数（与工人数 / 批次数无关）
//! - `board_snapshot`：**3 条**（计数 / 工序元数据 / 待下发计数）
//! - `board_process_detail`：**4 条**（工序元数据 / 工人+工种 max_held /
//!   全部工人持有批次一次 ANY / 候选池）—— 「待下发」是工序无关的全局量，
//!   复用 `snapshot` 的 `pending_count`，不在下钻时重查
//!
//! 旧路径（`GET /pool/{process_id}` + 逐 worker `GET /pool/state`）在 10 个工人
//! 时要发 1 + 1 + 10 = 12 个 HTTP 请求；新路径恒定 1 个。

pub mod repo;
pub mod service;

pub use repo::QueueBoardRepo;
pub use service::QueueBoardService;

#[cfg(test)]
mod tests {
    //! 域隔离护栏：board 聚合代码不依赖其它域。
    //!
    //! 探测器实现（剥注释、根段 + 域路径前缀匹配、元测试）见
    //! [`crate::shared::domain_guard`]，本模块只负责传参 + 域专属指引。

    use std::path::Path;

    use crate::shared::domain_guard::assert_no_foreign_domain;

    /// board 聚合 SQL 需要的数据（工序 / 工人 / 工种 / 批次 / 工单 / 客户 /
    /// 申请人 / 货架）全部在 `board/repo.rs` 的 SQL 里聚合，代码区不应出现任何
    /// 其它域路径（含 `prod::batch`、`prod::process`、`prod::worker` 等
    /// 同父兄弟域）。
    #[test]
    fn board_aggregation_depends_on_no_other_domain() {
        assert_no_foreign_domain(
            "prod::queue",
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("src/modules/prod/queue/board"),
            "需要别的域的数据时，正确做法是像 dashboard 域那样在本模块 SQL 里只读聚合\
             （读 t_process / t_worker / t_work_type / t_work_type_process / \
             t_part_batch / t_part / t_customer / t_applicant / t_shelf），\
             而不是 import 别人的 service / repo。\
             queue 域的**写端点**（refill / move / dispatch）走 `QueueRepoTrait`\
             转发其它域单表查询是既有 pattern，但那是写路径，与本护栏无关 —— \
             护栏只扫本 `board/` 目录。",
        );
    }
}

#[cfg(test)]
mod sql_count_guard_tests {
    //! **SQL 条数恒定**的源码级护栏：board 聚合的每条 SQL 都必须**无条件执行或
    //! 明确短路执行一次**，不得出现在任何循环体内。
    //!
    //! ## 这个测试防的是什么
    //!
    //! 本子模块的全部价值就是「固定 N 条 SQL」—— 前端从「1 + 1 + M 个请求」
    //! 降到 1 个请求，靠的就是 `board_process_detail` 里那条
    //! `current_holder_id = ANY($1::bigint[])`（一次取齐全部工人的持有批次）。
    //! 把 `ANY($1)` 改回 `for w in workers { query_held(w) }` 是**功能完全正确**的
    //! 改法：字段集合同形、held 总数守恒、所有集成测试照样全绿，只有工人数放大时
    //! 请求数悄悄涨回去。
    //!
    //! 集成测试数不了 SQL 条数（sqlx 0.9 不再为 `sqlx::query` 发 tracing 事件，
    //! PG 侧 `pg_stat_statements` 又要预热 + 扩展才有意义），所以只能退回源码级
    //! 护栏 —— 与 `shared::batch::status::write_guard_tests` 同款做法。
    //!
    //! ## 判定规则
    //!
    //! 1. 扫 `board/` 目录下全部 `.rs` 的**代码区**（注释与字符串字面量内容先被
    //!    空格化，行号保持不变），凡 `for` / `while` / `loop` 循环体（大括号配平
    //!    范围内）内出现 `sqlx::query`（含 `query_scalar` / `query_as` / `query!` /
    //!    `raw_sql` 等全部同族形式）即失败；
    //! 2. `board_process_detail` 函数体恰好 4 处 `sqlx::query`、
    //!    `board_snapshot` 恰好 3 处 —— 钉住具体条数。
    //!
    //! 规则 2 是刻意「会红」的：真要加第 5 条聚合 SQL 时它会挡住，那正是它该做的
    //! 事（加 SQL 必须同步 `docs/api/queue.md` 的条数表与 `board/repo.rs` 的方法
    //! doc）。失败信息里已写明要改哪几处。
    //!
    //! ## 已知绕过口
    //!
    //! - 循环写在**宏**里 / `sqlx::query` 的调用点被 `include!` 拼进来（仓库内不存在）；
    //! - 条件编译（`#[cfg(feature = …)]`）的两条分支各有一条查询：单看代码仍是
    //!   「每个分支至多一条」，恒定性成立，故不算绕过口；
    //! - 「恒定」的定义是**不随工人数 / 批次数增长**。按工种、按货架分组后各发一条
    //!   的写法会落在规则 1 下（那是循环）而被拦；若将来确有按维度分组的必要，
    //!   应改走「一条 SQL + `GROUP BY` / 窗口函数」而不是放宽本护栏。
    //!
    //! ## 与 `domain_guard` 的分工
    //!
    //! 两条护栏住在同一个 `board/` 目录，守两件不同的事：那条守「不 import 别的
    //! 域」，本条守「不按行数重复查」。探测器因此不同（那条是路径前缀匹配、本条是
    //! 括号配平 + 关键字扫描），故各自实现。

    use std::path::{Path, PathBuf};

    /// `board_process_detail` 期望的 `sqlx::query` 调用点数。
    const DETAIL_QUERIES: usize = 4;
    /// `board_snapshot` 期望的 `sqlx::query` 调用点数。
    const SNAPSHOT_QUERIES: usize = 3;

    /// 把 Rust 源码换成「注释与字符串字面量内容已空格化」的等价源码。
    ///
    /// 与 `shared::batch::status::write_guard_tests` 里那份同款策略：空格化而非
    /// 整段删除，**行号保持不变**，失败信息能给出精确 `file:line`。
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
                let body = i + 1;
                let mut hashes = 0usize;
                let mut p = body;
                while b.get(p) == Some(&b'#') {
                    hashes += 1;
                    p += 1;
                }
                if b.get(p) == Some(&b'"') {
                    p += 1;
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
                                break;
                            }
                        }
                        if b[p] != b'\n' {
                            out[p] = b' ';
                        }
                        p += 1;
                    }
                    if p < b.len() {
                        continue;
                    }
                }
                // 未闭合（源码一定编译得过，理论不可达）：当普通字符继续，避免死循环
            }
            // 普通字符串（`"` 或 `b"` 的引号起点）
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
        while let Some(rel) = code[from..].windows(kw.len()).position(|w| w == kw) {
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

    /// 递归列 `dir` 下全部 `.rs`。
    fn collect_rs(dir: &Path, acc: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut entries: Vec<_> = entries.flatten().map(|e| e.path()).collect();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                collect_rs(&path, acc);
            } else if path.extension().is_some_and(|e| e == "rs") {
                acc.push(path);
            }
        }
    }

    /// 取 `fn <name>` 之后第一个 `{`…`}` 的**函数体**区间。
    fn fn_body(code: &[u8], name: &str) -> (usize, usize) {
        let needle = format!("fn {name}");
        let at = find_keyword(code, 0, needle.as_bytes())
            .unwrap_or_else(|| panic!("board 源码里找不到 `fn {name}`"));
        let open = code[at..]
            .iter()
            .position(|c| *c == b'{')
            .map(|p| at + p)
            .unwrap_or_else(|| panic!("`fn {name}` 后找不到 `{{`"));
        let close =
            matching_brace(code, open).unwrap_or_else(|| panic!("`fn {name}` 的大括号不配平"));
        (open + 1, close)
    }

    fn count_occurrences(hay: &[u8], needle: &[u8]) -> usize {
        if needle.is_empty() || hay.len() < needle.len() {
            return 0;
        }
        hay.windows(needle.len()).filter(|w| *w == needle).count()
    }

    /// 循环体里不得有 `sqlx::query`。
    #[test]
    fn no_sqlx_query_inside_loop_body() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/modules/prod/queue/board");
        let mut files = Vec::new();
        collect_rs(&dir, &mut files);
        assert!(!files.is_empty(), "board 目录下列不到 .rs（路径写错了？）");

        let mut violations: Vec<String> = Vec::new();
        for path in &files {
            let src = std::fs::read_to_string(path).expect("读 board 源码");
            let code = blank_comments_and_strings(&src);
            for kw in ["for", "while", "loop"] {
                let mut from = 0usize;
                while let Some(at) = find_keyword(&code, from, kw.as_bytes()) {
                    from = at + kw.len();
                    // 跳过 `for` 用作生命周期 / 标识符一部分的情形（find_keyword 已处理）
                    let Some(open) = code[at..].iter().position(|c| *c == b'{').map(|p| at + p)
                    else {
                        continue;
                    };
                    // 关键字与 `{` 之间出现 `;` / `)` 之外的非法结构时按「不是循环体」处理
                    let Some(close) = matching_brace(&code, open) else {
                        continue;
                    };
                    let body = &code[open..=close];
                    if count_occurrences(body, b"sqlx::query") > 0 {
                        violations.push(format!(
                            "{}:{}  `{kw}` 循环体内出现 `sqlx::query`",
                            path.strip_prefix(env!("CARGO_MANIFEST_DIR"))
                                .unwrap_or(path)
                                .display(),
                            line_of(&code, at),
                        ));
                    }
                }
            }
        }
        assert!(
            violations.is_empty(),
            "board 聚合禁止按行数重复查询（下面是违规点）：\n{}",
            violations.join("\n")
        );
    }

    /// 两个聚合方法的 `sqlx::query` 调用点数被钉死。
    ///
    /// 与上一条互补：上一条禁「循环内查」（恒定性的**结构**保证），本条钉「一共几条」
    /// （恒定性的**数值**保证）。加一条聚合 SQL 必须同步 `board/repo.rs` 的方法
    /// doc 与 `docs/api/queue.md` 的条数表。
    #[test]
    fn aggregate_query_counts_are_pinned() {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("src/modules/prod/queue/board/repo.rs");
        let src = std::fs::read_to_string(&path).expect("读 board/repo.rs");
        let code = blank_comments_and_strings(&src);

        let (d_lo, d_hi) = fn_body(&code, "board_process_detail");
        let detail_q = count_occurrences(&code[d_lo..d_hi], b"sqlx::query");
        assert_eq!(
            detail_q,
            DETAIL_QUERIES,
            "board/repo.rs:{} `board_process_detail` 的 `sqlx::query` 调用点数是 {detail_q}，\
             期望 {DETAIL_QUERIES}。新增/删除聚合 SQL 时请同步改：\
             (1) 本文件该方法的 doc（编号 1..{DETAIL_QUERIES} 与「固定 N 条」）、\
             (2) `board/mod.rs` 与 `board/handler/board.rs` 的条数陈述、\
             (3) `docs/api/queue.md` 的条数表、\
             (4) `src/modules/prod/queue/board/mod.rs::sql_count_guard_tests` 的 \
             `DETAIL_QUERIES`。",
            line_of(&code, d_lo),
        );

        let (s_lo, s_hi) = fn_body(&code, "board_snapshot");
        let snapshot_q = count_occurrences(&code[s_lo..s_hi], b"sqlx::query");
        assert_eq!(
            snapshot_q,
            SNAPSHOT_QUERIES,
            "board/repo.rs:{} `board_snapshot` 的 `sqlx::query` 调用点数是 {snapshot_q}，\
             期望 {SNAPSHOT_QUERIES}。改法同上（对应 `SNAPSHOT_QUERIES`）。",
            line_of(&code, s_lo),
        );
    }
}
