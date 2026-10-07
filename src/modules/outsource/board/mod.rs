//! outsource 外协看板子模块（只读聚合，2026-10-09 新增）
//!
//! 两个只读端点的实现：
//! - `GET /api/v2/outsource-queue/snapshot` —— 工序序列板
//! - `GET /api/v2/outsource-queue/processes/{process_id}` —— 单工序板
//!
//! ## SQL 条数（与工序数 / 公司数 / 批次数无关）
//! - `snapshot`：**3 条**（候选侧分组 / 在途侧分组 / 工序元数据 `ANY`）
//! - `process_detail`：**4 条**（工序元数据 / 公司列白名单 / 该工序全部在外协批次
//!   一次取齐 / 候选卡）
//!
//! ## 为什么独立成子模块而不是塞进 `repo/sql.rs`
//!
//! 本仓对「固定 N 条 SQL」这类性能契约一律用**源码级护栏**钉住（与
//! `shared::batch::status::write_guard_tests`、`prod::queue::board::sql_count_guard_tests`
//! 同款：`cargo test --lib` 里扫源码、不连库）。护栏的扫描范围是一个目录 —— 圈出来
//! 的代码必须自成一个目录，「board 聚合 SQL 恒定条数」才可被 CI 执行。`prod/queue`
//! 已用同样手法（`prod/queue/board/`），本模块是它的第二份。
//!
//! 旧路径（`GET /outsource-pool/{process_id}` + 逐公司 `GET /outsource-pool/state`）在
//! M 家公司时要发 1 + M 个 HTTP 请求；新路径恒定 1 个。

pub mod repo;
pub mod service;

pub use repo::OutsourceQueueRepo;
pub use service::OutsourceQueueService;

#[cfg(test)]
mod sql_count_guard_tests {
    //! **SQL 条数恒定**的源码级护栏：看板聚合的每条 SQL 都必须**无条件执行或明确
    //! 短路执行一次**，不得出现在任何循环体内。
    //!
    //! ## 这个测试防的是什么
    //!
    //! 本子模块的全部价值就是「固定 N 条 SQL」—— 前端从「1 + M 个请求」降到 1 个
    //! 请求，靠的就是 `process_detail` 里那条**没有公司谓词**的在途批次查询
    //! （一次取齐该工序**全部**在外协批次）。把它改回
    //! `for company in companies { list_held(company, process) }` 是**功能完全正确**
    //! 的改法：字段集合同形、held 总数守恒、所有集成测试照样全绿，只有公司数放大时
    //! 请求数悄悄涨回去。
    //!
    //! 本条是**结构断言**（源码级），不是行为断言。运行时计数同样可行：sqlx 的
    //! `QueryLogger` 逐语句发 `target: "sqlx::query"` 的 DEBUG 事件（带
    //! `db.statement` / `rows_affected` / `rows_returned` / `elapsed`），装一个按该
    //! target 计数的 subscriber 便可断言「2 公司与 10 公司两次请求的条数相等」。这里
    //! 仍取结构断言，是因为计数要可靠归属到「本次请求」：
    //! `tracing::subscriber::with_default` 只在当前线程生效，而集成测试是同一进程内
    //! 并行跑多个 case，得改用全局 subscriber + 按 span 归属计数 —— 后者在
    //! `#[tokio::test]` 由 current_thread 改成 multi_thread 时会静默少计，「条数相等」
    //! 退化成 0 == 0 的假绿。PG 侧 `pg_stat_statements` 则要预热 + 装扩展才有意义，
    //! 同样不作为 CI 判据。
    //!
    //! ## 判定规则
    //!
    //! 1. 扫 `board/` 目录下全部 `.rs` 的**代码区**（注释与字符串字面量内容先被
    //!    空格化，行号保持不变），凡 `for` / `while` / `loop` 循环体（大括号配平
    //!    范围内）内出现**任何数据库往返调用点**即失败；
    //! 2. `process_detail` 函数体恰好 4 处查询调用点、`snapshot` 恰好 3 处 ——
    //!    钉住具体条数。
    //!
    //! 「数据库往返调用点」的判定见 [`QUERY_TERMINALS`]：sqlx 的三条构造路径
    //! （`sqlx::query` 族 / `sqlx::raw_sql` / `Executor::execute` 族）**都以调用
    //! Executor 的某个取行方法收尾，且恰好一次**，故取行方法是唯一同时「不漏形态」
    //! 与「不重复计」的标记。只按构造入口 grep 会漏掉 `raw_sql` 与
    //! `Executor::execute` —— 它们都不含 `sqlx::query` 子串，而那两条正是本仓在用的
    //! 写法（见 `prod::batch::service::transition` 与 `part::service::batch` 的
    //! SAVEPOINT 段）。取行方法另有 `::` 引出的 UFCS 形态
    //! （[`QUERY_TERMINALS_UFCS`]），只参与本规则的判定。
    //!
    //! 规则 2 是**刻意「会红」的**：真要加第 5 条聚合 SQL 时它会挡住，那正是它该做的
    //! 事（加 SQL 必须同步本目录两处 doc 与 `board/repo.rs` 的方法 doc）。失败信息里
    //! 已写明要改哪几处。
    //!
    //! **4 条 SQL 必须全部写在 `process_detail` 函数体内**（而不是拆成 4 个私有
    //! helper）：本护栏数的是函数体里的调用点，把其中一条挪进 helper 会让计数变成 3
    //! 而实际仍是 4 条 —— 那就是护栏本身在骗自己。
    //!
    //! ## 已知绕过口
    //!
    //! - 循环体内**调用私有 helper**，查询写在 helper 里（两条规则都看不到调用点的
    //!   归属）；本目录现有的 SQL 全部直写在聚合方法体内，helper 化必须同步把
    //!   「4 条 / 3 条」的计数一并搬进被扫的目录，否则规则 2 会立刻报数不符；
    //! - 循环写在**宏**里 / 调用点被 `include!` 拼进来（仓库内不存在）；
    //! - 条件编译（`#[cfg(feature = …)]`）的两条分支各有一条查询：单看代码仍是
    //!   「每个分支至多一条」，恒定性成立，故不算绕过口；
    //! - 「恒定」的定义是**不随工序数 / 公司数 / 批次数增长**。按维度分组后各发一条
    //!   的写法会落在规则 1 下（那是循环）而被拦；若将来确有按维度分组的必要，应改走
    //!   「一条 SQL + `GROUP BY` / 窗口函数」而不是放宽本护栏。

    use std::path::{Path, PathBuf};

    /// `process_detail` 期望的查询调用点数。
    const DETAIL_QUERIES: usize = 4;
    /// `snapshot` 期望的查询调用点数。
    const SNAPSHOT_QUERIES: usize = 3;

    /// 一次数据库往返的**终点**标记（`Executor` trait 的取行方法，调用形式为方法
    /// 语法 `.fetch_all(` 等）。
    ///
    /// 集合内**互不为子串**（`.fetch(` 不会命中 `.fetch_all(`），且每个查询调用点
    /// 恰好命中其中一项 ⇒ 「命中总数」= 「查询条数」，不需要去重。
    const QUERY_TERMINALS: [&[u8]; 8] = [
        b".execute(",
        b".execute_many(",
        b".fetch(",
        b".fetch_all(",
        b".fetch_many(",
        b".fetch_one(",
        b".fetch_optional(",
        b".fetch_optional_many(",
    ];

    /// [`QUERY_TERMINALS`] 的 **UFCS 形态**（以 `::` 而非 `.` 引出取行方法，如
    /// `sqlx::Executor::execute(&mut *conn, sql)`）。
    ///
    /// 只参与规则 1 的「出现即失败」判定，不参与条数计数 —— 同一次往返在源码里只以
    /// 一种形态出现，但两条 needle 同时启用会让计数与判定的语义分叉。
    const QUERY_TERMINALS_UFCS: [&[u8]; 8] = [
        b"::execute(",
        b"::execute_many(",
        b"::fetch(",
        b"::fetch_all(",
        b"::fetch_many(",
        b"::fetch_one(",
        b"::fetch_optional(",
        b"::fetch_optional_many(",
    ];

    /// 查询的**构造入口**标记，只用于规则 1（循环体内出现即失败），不进规则 2 的
    /// 计数 —— 构造入口与终点会在同一条链上各命中一次，同时计数就翻倍了。
    ///
    /// `sqlx::query` 一个 needle 就覆盖 `query` / `query_scalar` / `query_as` /
    /// `query!` 宏（全是它的前缀）；`sqlx::raw_sql` 是另一族前缀，必须单列。
    const QUERY_ENTRYPOINTS: [&[u8]; 2] = [b"sqlx::query", b"sqlx::raw_sql"];

    /// 把 Rust 源码换成「注释与字符串字面量内容已空格化」的等价源码。
    ///
    /// 与 `prod::queue::board::sql_count_guard_tests` /
    /// `shared::batch::status::write_guard_tests` 里那两份同款策略：空格化而非整段
    /// 删除，**行号保持不变**，失败信息能给出精确 `file:line`。
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
            .unwrap_or_else(|| panic!("board/repo.rs 里找不到 `fn {name}`"));
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

    /// 代码区内的**查询条数** = [`QUERY_TERMINALS`] 命中总数（见该常量的 doc：
    /// 标记互不为子串，故无需去重）。
    fn count_queries(code: &[u8]) -> usize {
        QUERY_TERMINALS
            .iter()
            .map(|n| count_occurrences(code, n))
            .sum()
    }

    /// 循环体内是否出现「构造入口」标记（[`QUERY_ENTRYPOINTS`]）。
    fn has_query_entrypoint(code: &[u8]) -> bool {
        QUERY_ENTRYPOINTS
            .iter()
            .any(|n| count_occurrences(code, n) > 0)
    }

    /// 循环体内是否出现 UFCS 形态的取行方法（[`QUERY_TERMINALS_UFCS`]）。
    fn has_query_terminal_ufcs(code: &[u8]) -> bool {
        QUERY_TERMINALS_UFCS
            .iter()
            .any(|n| count_occurrences(code, n) > 0)
    }

    fn board_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src/modules/outsource/board")
    }

    /// 循环体里不得有任何数据库往返调用点。
    #[test]
    fn no_sqlx_query_inside_loop_body() {
        let mut files = Vec::new();
        collect_rs(&board_dir(), &mut files);
        assert!(!files.is_empty(), "board 目录下列不到 .rs（路径写错了？）");

        let mut violations: Vec<String> = Vec::new();
        for path in &files {
            let src = std::fs::read_to_string(path).expect("读 board 源码");
            let code = blank_comments_and_strings(&src);
            for kw in ["for", "while", "loop"] {
                let mut from = 0usize;
                while let Some(at) = find_keyword(&code, from, kw.as_bytes()) {
                    from = at + kw.len();
                    let Some(open) = code[at..].iter().position(|c| *c == b'{').map(|p| at + p)
                    else {
                        continue;
                    };
                    let Some(close) = matching_brace(&code, open) else {
                        continue;
                    };
                    let body = &code[open..=close];
                    if count_queries(body) > 0
                        || has_query_entrypoint(body)
                        || has_query_terminal_ufcs(body)
                    {
                        violations.push(format!(
                            "{}:{}  `{kw}` 循环体内出现数据库往返调用（\
                             命中取行方法 / `sqlx::query` / `sqlx::raw_sql`）",
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
            "看板聚合禁止按行数重复查询（下面是违规点）：\n{}",
            violations.join("\n")
        );
    }

    /// 两个聚合方法的查询条数被钉死。
    ///
    /// 与上一条互补：上一条禁「循环内查」（恒定性的**结构**保证），本条钉「一共几条」
    /// （恒定性的**数值**保证）。加一条聚合 SQL 必须同步 `board/repo.rs` 的方法
    /// doc、`board/mod.rs` 的条数陈述与本文件的两个常量。
    #[test]
    fn detail_queries_are_pinned() {
        let path = board_dir().join("repo.rs");
        let src = std::fs::read_to_string(&path).expect("读 board/repo.rs");
        let code = blank_comments_and_strings(&src);

        let (d_lo, d_hi) = fn_body(&code, "process_detail");
        let detail_q = count_queries(&code[d_lo..d_hi]);
        assert_eq!(
            detail_q,
            DETAIL_QUERIES,
            "board/repo.rs:{} `process_detail` 的查询条数是 {detail_q}，\
             期望 {DETAIL_QUERIES}。新增/删除聚合 SQL 时请同步改：\
             (1) 本文件该方法的 doc（编号 1..{DETAIL_QUERIES} 与「固定 N 条」）、\
             (2) `board/mod.rs` 与 `handler/board.rs` 的条数陈述、\
             (3) 本文件的 `DETAIL_QUERIES`。\
             注意：把某条 SQL 挪进私有 helper 会让本计数失真而不改实际条数 —— \
             {DETAIL_QUERIES} 条必须全部写在函数体内。",
            line_of(&code, d_lo),
        );

        let (s_lo, s_hi) = fn_body(&code, "snapshot");
        let snapshot_q = count_queries(&code[s_lo..s_hi]);
        assert_eq!(
            snapshot_q,
            SNAPSHOT_QUERIES,
            "board/repo.rs:{} `snapshot` 的查询条数是 {snapshot_q}，\
             期望 {SNAPSHOT_QUERIES}。改法同上（对应 `SNAPSHOT_QUERIES`）。",
            line_of(&code, s_lo),
        );
    }
}

#[cfg(test)]
mod held_count_guard_tests {
    //! `companies[].held_count == held_batches.len()` 不变量守卫。
    //!
    //! 前端按 `held_count` 渲染列头徽标、按 `held_batches` 渲染卡片，两者不一致时
    //! 表现为「徽标写 3、列里只有 2 张卡」，运营无法判断是漏件还是显示 bug —— 而
    //! 这类不一致是**静默**的（没有报错，`200` 照返）。
    //!
    //! 本条直接对生产路径上的纯函数
    //! [`to_companies`](super::service::to_companies) 断言恒等式（不是在测试里重写一遍
    //! 装配逻辑 —— 那样只会验证测试自己）。
    use super::repo::{CompanyRow, HeldBatchRow};
    use super::service::to_companies;

    fn company(company_id: i64, name: &str) -> CompanyRow {
        CompanyRow {
            company_id,
            name: name.to_string(),
        }
    }

    fn held(company_id: i64, batch_id: i64) -> HeldBatchRow {
        HeldBatchRow {
            company_id,
            batch_id,
            part_id: 9_000_000_000_000_000_002,
            batch_no: 1,
            quantity: 5,
            serial_no: None,
            drawing_no: "DWG-1".into(),
            name: "NAME-1".into(),
            system_delivery_date: None,
            planned_delivery_date: None,
            is_urgent: false,
            customer_name: None,
            parent_customer_name: None,
            applicant_name: None,
            batch_location: "OUTSOURCE_COMPANY".into(),
            note: None,
            batch_version: 3,
            has_cnc_program: false,
            sent_at: None,
            price: None,
            receive_next_process_id: 0,
            receive_next_process_name: None,
        }
    }

    /// 核心断言：`held_count` 恒等于内联批次数，**跨多家公司、且输入故意交错**。
    ///
    /// 交错输入是这条用例的一半价值：它顺带证明分组不依赖 SQL 的 ORDER BY（真的靠
    /// 「同公司行连续」来切分的话，交错输入会算错）。
    #[test]
    fn held_count_matches_held_batches_len() {
        let companies = to_companies(
            vec![company(100, "A"), company(200, "B"), company(300, "C")],
            vec![
                held(100, 1),
                held(200, 2),
                held(100, 3),
                held(300, 4),
                held(200, 5),
            ],
        );
        assert_eq!(companies.len(), 3);
        for c in &companies {
            assert_eq!(
                c.held_count,
                c.held_batches.len() as i64,
                "company {} 的 held_count 与 held_batches 不一致",
                c.company_id
            );
        }
        let by_id = |cid: i64| {
            companies
                .iter()
                .find(|c| c.company_id == cid.to_string())
                .unwrap()
                .held_batches
                .len()
        };
        assert_eq!(by_id(100), 2);
        assert_eq!(by_id(200), 2);
        assert_eq!(by_id(300), 1);
    }

    /// 一批也没在途 ⇒ `held_count = 0`、`held_batches = []`，且公司列**不得消失**
    /// （空公司列是前端要的拖拽目标）。
    #[test]
    fn empty_company_column_survives_with_zero_held() {
        let companies = to_companies(vec![company(100, "A")], Vec::new());
        assert_eq!(companies.len(), 1);
        assert_eq!(companies[0].held_count, 0);
        assert!(companies[0].held_batches.is_empty());
    }

    /// 分组到「没有对应公司列」的组（holder 指向已停用 / 已解映射的公司）被丢弃 ——
    /// 它不会凭空造出一列公司列。
    #[test]
    fn held_for_unlisted_company_is_dropped() {
        let companies = to_companies(vec![company(100, "A")], vec![held(999, 7)]);
        assert_eq!(companies.len(), 1);
        assert_eq!(companies[0].held_count, 0);
        assert!(companies[0].held_batches.is_empty());
    }
}
