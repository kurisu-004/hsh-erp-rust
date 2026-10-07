//! 雪花 ID 构造护栏：把「进程内只从共享 generator 取号」钉成测试期事实
//!
//! 2026-10-09 新增（项目硬规约）。那一轮改造把 `tests/`（169 处）与 `src/` 的
//! `#[cfg(test)]` 块（33 处）里就地新建的 `SnowflakeIdGenerator` 全部迁到两个进程级
//! 共享 generator，1300 个测试零 `23505`。但**没有任何机制**阻止后人再写一个新的
//! —— 本仓此前正因为缺这道护栏，让 200 处本地 generator 积累了几个月。本文件补上它。
//!
//! ## 为什么「进程内新建 generator」是 bug 而不是风格问题
//! 位布局是 `ts << 22 | instance << 12 | seq`，其中 `last_ms` / `sequence` 属于
//! generator 的**实例私有**字段，而构造一律从 `last_ms = 0, sequence = 0` 起步。
//! 于是**任意两个 instance 相同、对象不同**的 generator，在同一毫秒各自取到第 j 个号，
//! 就发出**逐字节相同**的 id → `23505 duplicate key ... t_*_pkey`。碰撞判据不是
//! 「同一个 helper 调几次」，而是**两个不同 helper 各调一次**。
//!
//! 故「进程内唯一」的正确保证点是**对象共享**，不是 instance 编号：instance 只有
//! 10 bit = 1024 槽，本就该留给**跨进程**区分（lib 单测 binary 与 20 个集成测试
//! binary 各起各的进程，每个集成测试又有自己的库），而刻意填的字面量仍有 1/1024
//! 概率与本进程派生值相等，两个 fresh generator 的首个 id 又恰好都是 `seq = 0`。
//!
//! ## 判定口径
//! 在**代码字符**上匹配「类型名 + 可选空白 + `::` + 可选空白 + `new`」，白名单文件
//! 之外命中即失败，失败信息带 `file:line`。中间允许空白是刻意的（Rust 里
//! `Foo :: new` / `Foo:: new` 都合法，漏掉它们等于给后人留后门）。
//!
//! 判定只认**代码字符**这一类，于是三种「看起来像违规」的东西天然不误报：
//! - **注释 / 文档注释**：仓库现状是**注释里保留了大量历史说明**（形如「本文件已不再
//!   就地 `SnowflakeIdGenerator::new(...)`」），全仓 `tests/` + `src/` 里几十处命中
//!   全是这个形态。注释在匹配前已被等量空格化（行号不变，所以报错仍能给到 `file:line`）。
//! - **字符串 / raw string / 字符字面量**：本仓 doc 里大量用 `r#"…"#` 贴「旧写法」代码
//!   样例、错误提示文案里也会提到这个字面量，它们都不是构造。
//! - **只 `use` 类型**：`use …::snowflake::SnowflakeIdGenerator;`（`tests/**` 里遍地）
//!   不带 `::new`，不命中。
//!
//! ## 与 `shared/batch/status.rs` 的 `write_guard_tests` 的三处**刻意**差异
//!
//! 1. **不排除 `#[cfg(test)]` 块** —— 那条规则排除 `#[cfg(test)]` 是因为它要治的是
//!    **生产写路径**漏派生；本规则恰恰把 `src/` 里 `#[cfg(test)]` 内的就地构造列为
//!    治理对象（上一轮迁走的 33 处几乎全在 `#[cfg(test)]` 里）。故本文件不需要
//!    `cfg_test_regions` 那套括号配平逻辑。
//! 2. **匹配范围是「代码字符」而非整段文本** —— 那条规则的 needle（表名）**住在字符串
//!    里**（SQL 本来就是字符串），必须扫字符串正文；本规则的 needle 只可能出现在代码里，
//!    按字面量分类排除掉字符串既更准又更简单。
//! 3. **不静默降级** —— 未闭合注释 / 字符串这类畸形输入一律按代码扫，不试图猜（rustc
//!    在产物之前就拦住这类输入，护栏读到的真实源码一定是合法 Rust）。
//!
//! ## 为什么不直接复用 `write_guard_tests::scan_rust`（E2 的判断依据）
//! `scan_rust` 是 `src/shared/batch/status.rs` 里 `mod write_guard_tests` 的**私有**
//! 函数，要复用就得改那个文件（提到共享模块、或改成 `pub(crate)`）—— 本子任务的
//! 硬约束是**除本文件与 `src/shared/mod.rs` 外不许改任何其它文件**，故只能**参照 +
//! 等价实现**。三份实现的关系如实记档：
//! - 本文件：字节级分类 + 代码字符匹配（最严格的一份）。
//! - `shared/batch/status.rs` 的副本：字节级分类 + 扫整段文本（含字符串正文）。
//! - `shared/domain_guard.rs` 的 `blank_comments`：**逐行**处理、不做字面量分类，
//!   按其模块 doc 已登记的精度边界，字符串正文里的外来路径会被当代码扫 —— 本规则若
//!   直接用它，本仓 `r#"…"#` 里贴旧写法的 doc 会**成片误报**，故不可用。
//!
//! ⚠️ **与 `status.rs` 那份副本的一处刻意偏差（实测确认的缺陷，本轮不修那个文件）**：
//! `raw_string_end` 需要按**开引号处 `#` 的个数**（`r"`→0、`r#"`→1、`r##"`→2）找闭合，
//! 而 `status.rs` 那份从内容起点起算恒为 0，对 `r"…"` 恰好能闭合、对 **`r#"…"#`
//! 一路扫到 EOF 判为未闭合**（已用独立程序逐字复现验证）。对本规则而言「未闭合 → 内容
//! 按代码扫」= raw string 里贴的旧写法样例会被**误报**。本文件因此把
//! `raw_string_start` 的契约统一成「一律返回**内容**起点 + 开引号 `#` 个数」
//! （原实现两条分支一个给引号位置、一个给内容位置，本身就不自洽）。本护栏的 raw string
//! 精度由 `guard_detector_ignores_comments_strings_and_non_constructions` 钉住。
//!
//! ## 已知漏报盲区（**全部**登记在此 —— 未登记的漏报形态一律视为护栏缺陷）
//!
//! 1. **`Self::new(..)`**（写在 `impl SnowflakeIdGenerator` 块内）：没有类型名字面量，
//!    探测器无从找起。inherent impl 只能写在定义该类型的 crate 内，所以这条盲区的
//!    暴露面 = `src/` 里除 `src/infra/snowflake.rs` 之外新增 inherent impl 块；
//!    现状全仓只有 `src/infra/snowflake.rs` 一处 `impl SnowflakeIdGenerator`。
//!    只登记不修：要覆盖它得解析 impl 块。
//! 2. **未闭合块注释**会让文件剩余全部内容被当注释跳过，其后的真实构造漏过。属
//!    「rustc 已先拦下的畸形输入」，是「一律视为缺陷」的例外。
//!
//! ## 已知精度边界（只会漏报、不会误报，方向上是安全的）
//! - **字符串 / raw string 正文里的构造字面量不报警** —— 有意：doc 贴旧写法、错误提示
//!   文案、断言消息都属这一类。
//! - `#[cfg(test)]` 块内的构造**照样报警** —— 有意，见上文差异 1。

use std::path::{Path, PathBuf};

/// 扫描根（相对 `CARGO_MANIFEST_DIR`）—— 相对 crate 根的**显式清单**，不从仓库根递归。
///
/// 显式列举的后果是 `target/` / `.sqlx/` / `.claude/worktrees/` 天然不在范围内。
/// `.claude/worktrees/` 尤其重要：那里存着每一个 worktree 的**完整源码副本**，按仓库根
/// 递归会把别的分支的 `tests/` 一并扫进来，而本护栏判的应当是**本 worktree** 的代码。
///
/// `test-support/src` 是硬规约 `src/` + `tests/` 之外的**超集**，加它有两个理由：
/// 白名单里 `test-support/src/pool.rs` 这条只有在它被扫描时才有意义（否则白名单条目是
/// 死的），而它恰恰是「就地新建 generator」最有诱惑力的地方（集成测试的唯一 ID 源就住
/// 在那儿，一眼看过去「再 new 一个给某个 fixture 用」非常自然）。
const SCAN_ROOTS: &[&str] = &["src", "tests", "test-support/src"];

/// 全仓被允许就地新建 `SnowflakeIdGenerator` 的文件（相对 crate 根）→ **为什么合法**。
///
/// ⚠️ **新增条目前必须先回答**：这条新路径属于**哪一类 binary** —— 集成测试
/// （`tests/**`）/ lib 单测（`src/**` 的 `#[cfg(test)]`）/ 生产代码 / 被测对象自身？
/// 答不出来就说明它不该就地构造，应该走 `shared_test_snowflake()`。
///
/// 前两条**并存是设计而非疏漏**：`test-support` 是 `[dev-dependencies]` 且 path-depends
/// 回主 crate，构成 dev-dependency 环，编译 lib 单测目标时同一个二进制里会链进**两份**
/// `hsh_erp_rust`，两份是各自独立的 crate 实例 ⇒ 两个 `SnowflakeIdGenerator` 是**两个
/// 不同的类型**，把 test-support 的那个传给收本 crate 类型的形参即 `E0308`。两类 binary
/// 是不同进程，跨进程撞号只可能通过 Redis 显形，而 Redis 已由 `RedisConfig::key_prefix`
/// 按进程隔离，故「进程内各有一个唯一源」已经足够。
const SANCTIONED: &[(&str, &str)] = &[
    (
        "src/shared/test_snowflake.rs",
        "lib 单测 binary 的**唯一** ID 源（与 `test-support/src/pool.rs` 并存是设计而非疏漏，见本常量 doc 的 dev-dependency 环说明）",
    ),
    (
        "test-support/src/pool.rs",
        "集成测试 binary 的**唯一** ID 源（`TEST_SNOWFLAKE_GEN` → `Arc` → `shared_test_snowflake()`）",
    ),
    (
        "src/main.rs",
        "**生产**代码的 instance 来源，不是测试脚手架",
    ),
    (
        "src/infra/snowflake.rs",
        "`SnowflakeIdGenerator` **自身的位布局单元测试**（含 `MAX_INSTANCE` panic 边界），是被测对象不是脚手架",
    ),
];

/// 被探测的构造表达式，**刻意**用 `concat!` 拆开：本文件源码里永不出现连续的完整字面量，
/// 否则护栏扫到自己的文件时会**自指误报**（同 `shared/batch/status.rs` 的
/// `NEEDLE_TABLE_WORD`、`shared/domain_guard.rs` 的 `ROOT_SEG` 的同一处理）。
const NEEDLE_EXPR: &str = concat!("Snowflake", "IdGenerator", "::", "new");
/// 匹配用的类型名（要求左边界不是标识符字符，`FooWrapper` 不算命中）。
const NEEDLE_TYPE: &str = concat!("Snowflake", "IdGenerator");
/// 匹配用的分隔符与函数名。二者都是通用词、且只有**紧跟在类型名之后**才判命中，
/// 故直接写字面量即可 —— 必须拆 `concat!` 的只有 `NEEDLE_EXPR`（那行含类型名）。
const NEEDLE_COLONS: &str = "::";
const NEEDLE_NEW: &str = "new";

/// 字符分类（`scan_rust` 产出）。
const KIND_OTHER: u8 = 0; // 空白 / 注释（注释已被等量空格化）
const KIND_CODE: u8 = 1; // 代码字符
const KIND_STR: u8 = 2; // 字符串 / raw string / 字符字面量（含引号本身）

/// 把 Rust 源码切成「注释已空格化」的文本 + 每字节分类。
///
/// 注释空格化而不是整段删除，是为了让**行号保持不变**，失败信息里才能给出精确的
/// `file:line`。
///
/// 与 `shared/batch/status.rs` 的同名函数的关系见模块 doc「为什么不直接复用」一节：
/// 逐字参照其状态机（行注释 / 块注释含嵌套 / raw string 与 byte string 前缀 /
/// 普通字符串含转义 / 字符字面量 vs 生命周期），**并修掉其 raw string `#` 计数缺陷**。
fn scan_rust(src: &str) -> (Vec<u8>, Vec<u8>) {
    let b = src.as_bytes();
    let mut out = b.to_vec();
    let mut kind = vec![KIND_OTHER; b.len()];
    let mut i = 0usize;
    while i < b.len() {
        let c = b[i];
        // ---- 行注释（含 `//` / `///` / `//!`）----
        if c == b'/' && b.get(i + 1) == Some(&b'/') {
            while i < b.len() && b[i] != b'\n' {
                out[i] = b' ';
                i += 1;
            }
            continue;
        }
        // ---- 块注释（Rust 支持嵌套）----
        if c == b'/' && b.get(i + 1) == Some(&b'*') {
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
        // ---- raw string / byte string 前缀（`r"` `r#"` `b"` `br#"`，可叠加 c）----
        if let Some((lit_start, content_start, hashes)) = raw_string_start(b, i) {
            let Some(close) = raw_string_end(b, content_start, hashes) else {
                // 理论不可达（源码一定编译得过）；保守当普通代码处理，避免死循环
                kind[i] = KIND_CODE;
                i += 1;
                continue;
            };
            for slot in kind.iter_mut().take(close + 1).skip(lit_start) {
                *slot = KIND_STR;
            }
            i = close + 1;
            continue;
        }
        // ---- 普通字符串（含 `b"..."` 的引号起点）----
        if c == b'"' {
            let end = string_end(b, i + 1).unwrap_or(b.len().saturating_sub(1));
            for slot in kind.iter_mut().take(end + 1).skip(i) {
                *slot = KIND_STR;
            }
            i = end + 1;
            continue;
        }
        // ---- 字符字面量 vs 生命周期（`'a` vs `'x'`）----
        if c == b'\'' {
            let is_char = match b.get(i + 1) {
                // `'\\n'` 形式：反斜杠开头一定不是生命周期
                Some(b'\\') => true,
                // `'x'` 恰好三字节
                Some(_) if b.get(i + 2) == Some(&b'\'') => true,
                _ => false,
            };
            if is_char {
                let end = string_end(b, i + 1).unwrap_or(b.len().saturating_sub(1));
                for slot in kind.iter_mut().take(end + 1).skip(i) {
                    *slot = KIND_STR;
                }
                i = end + 1;
                continue;
            }
            // 生命周期标注（`&'a mut T`）→ 普通代码
            kind[i] = KIND_CODE;
            i += 1;
            continue;
        }
        if !c.is_ascii_whitespace() {
            kind[i] = KIND_CODE;
        }
        i += 1;
    }
    (out, kind)
}

/// 从 `from` 起找普通字符串 / 字符字面量的闭合引号下标（`from` 指向开引号**之后**）。
///
/// 未闭合（行尾 / 文件尾都没闭合）返回 `None` —— 那种输入编译不过，由 rustc 先拦下。
fn string_end(b: &[u8], from: usize) -> Option<usize> {
    let mut j = from;
    while j < b.len() {
        match b[j] {
            b'\\' => j += 2,
            c if c == b'"' || c == b'\'' => return Some(j),
            b'\n' => return None,
            _ => j += 1,
        }
    }
    None
}

/// 判断 `b[i..]` 是否是 raw string / byte string 的开头。
///
/// 返回 `(字面量起始下标, 内容起始下标, 开引号的 `#` 个数)`；`r` / `b` / `c` 前缀本身
/// 算字面量（一并标成 `KIND_STR` 即可，prefix 里没有括号）。两条分支的契约统一成
/// 「一律给**内容**起点」—— 原实现一条给引号位置、一条给内容位置，才导致
/// `raw_string_end` 的 `#` 计数在 `r#"…"#` 上恒为 0（见模块 doc）。
fn raw_string_start(b: &[u8], i: usize) -> Option<(usize, usize, usize)> {
    let mut p = i;
    // 前缀可任意组合（`br` / `cr` / `rb` …），只要最终紧跟 `"` 或 `r#`
    while matches!(b.get(p), Some(b'r') | Some(b'b') | Some(b'c')) {
        p += 1;
    }
    let is_raw = b.get(p) == Some(&b'r') && p > i;
    let q = if is_raw { p + 1 } else { p };
    match b.get(q) {
        Some(b'"') => Some((i, q + 1, 0)),
        Some(b'#') => {
            let mut h = q;
            while b.get(h) == Some(&b'#') {
                h += 1;
            }
            if b.get(h) == Some(&b'"') {
                Some((i, h + 1, h - q))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// 从 raw string 的**内容**起点找闭合 `"` + `#`×n；返回闭合序列的最后一个下标。
fn raw_string_end(b: &[u8], content_start: usize, hashes: usize) -> Option<usize> {
    let mut p = content_start;
    while p < b.len() {
        if b[p] == b'"' {
            let mut h = 0usize;
            while b.get(p + 1 + h) == Some(&b'#') {
                h += 1;
            }
            if h == hashes {
                return Some(p + h);
            }
            p += 1 + h;
            continue;
        }
        p += 1;
    }
    None
}

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while b.get(i).is_some_and(|c| c.is_ascii_whitespace()) {
        i += 1;
    }
    i
}

fn find_sub(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    (from..=hay.len() - needle.len()).find(|&s| &hay[s..s + needle.len()] == needle)
}

fn is_ident(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$'
}

/// 从 `from` 起找「前一个字符不是标识符字符」的子串，返回**绝对**下标。
///
/// （`shared/batch/status.rs` 的同名辅助函数收的是切片、边界判断相对切片首字节；
/// 本文件用绝对下标，故 `from` 之后的首字节也不会被误判成「前一个字符」。）
fn find_word_at(hay: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    let mut at = from;
    while let Some(rel) = find_sub(hay, needle, at) {
        let prev_ok = rel == 0 || !is_ident(hay[rel - 1]);
        if prev_ok {
            return Some(rel);
        }
        at = rel + 1;
    }
    None
}

/// 返回一个文件里所有「就地新建 `SnowflakeIdGenerator`」的**代码行**（1 基行号）。
fn construction_lines(src: &str) -> Vec<usize> {
    let (out, kind) = scan_rust(src);
    let mut hits = Vec::new();
    let mut from = 0usize;
    while let Some(at) = find_word_at(&out, from, NEEDLE_TYPE.as_bytes()) {
        from = at + 1;
        // 只认代码字符 ⇒ 注释（已空格化）与字面量（已分类）自动排除
        if kind.get(at) != Some(&KIND_CODE) {
            continue;
        }
        // `Type` + 可选空白 + `::` + 可选空白 + `new`（要求右边界不是标识符字符，
        // 排除 `…::newer`）
        let p = skip_ws(&out, at + NEEDLE_TYPE.len());
        if !out[p..].starts_with(NEEDLE_COLONS.as_bytes()) {
            continue;
        }
        let q = skip_ws(&out, p + NEEDLE_COLONS.len());
        if !out[q..].starts_with(NEEDLE_NEW.as_bytes()) {
            continue;
        }
        let after = q + NEEDLE_NEW.len();
        if out.get(after).is_some_and(|c| is_ident(*c)) {
            // `SnowflakeIdGenerator::newer` 之类，不是构造
            continue;
        }
        hits.push(1 + out[..at].iter().filter(|c| **c == b'\n').count());
    }
    hits
}

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

/// 把「扫描 + 判定 + 报错」整段收拢，方便测试直接喂假源码（见元测试）。
fn violations_in(src: &str, rel: &str) -> Vec<String> {
    construction_lines(src)
        .into_iter()
        .map(|line| format!("  {rel}:{line}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{NEEDLE_EXPR, NEEDLE_TYPE, SANCTIONED, SCAN_ROOTS, collect_rs, violations_in};
    use std::path::{Path, PathBuf};

    fn root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
    }

    /// 相对 crate 根、正斜杠分隔的相对路径（失败信息与白名单常量共用这个口径）。
    fn rel_of(path: &Path, root: &Path) -> String {
        path.strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    }

    #[test]
    fn no_local_generator_construction_outside_whitelist() {
        let root = root();
        // 每个扫描根都必须扫到文件：某个根改名 / 缺失时护栏必须炸，而不是静默放行
        let mut files: Vec<PathBuf> = Vec::new();
        for dir in SCAN_ROOTS {
            let mut acc = Vec::new();
            collect_rs(&root.join(dir), &mut acc);
            assert!(
                !acc.is_empty(),
                "扫描根 `{dir}` 下没有任何 .rs（CARGO_MANIFEST_DIR={}），护栏本身失效",
                root.display()
            );
            files.append(&mut acc);
        }

        let mut violations: Vec<String> = Vec::new();
        for path in &files {
            let rel = rel_of(path, &root);
            if SANCTIONED.iter().any(|(p, _)| *p == rel) {
                continue;
            }
            let Ok(src) = std::fs::read_to_string(path) else {
                continue;
            };
            violations.extend(violations_in(&src, &rel));
        }

        assert!(
            violations.is_empty(),
            "{msg}",
            msg = failure_message(violations.len(), &violations.join("\n"))
        );
    }

    /// 白名单条目不许指向已删除的文件（否则白名单会无声腐烂，少了一处真正的治理点）。
    #[test]
    fn whitelist_entries_all_exist() {
        let root = root();
        let missing: Vec<&str> = SANCTIONED
            .iter()
            .map(|(p, _)| *p)
            .filter(|p| !root.join(p).is_file())
            .collect();
        assert!(
            missing.is_empty(),
            "白名单里这些文件已不存在，请清理 `SANCTIONED` 条目：\n  {}\n\
             （白名单腐烂 = 少了一处真正的治理点，且新构造点会被误放行）",
            missing.join("\n  ")
        );
    }

    // ── 元测试：探测器本身必须能报警，否则「全绿」只是因为探测器瞎了 ──────────

    /// 正例：代码区里的构造一律抓到，且行号逐个报对。
    #[test]
    fn guard_detector_flags_a_local_generator_construction() {
        // 1) 最典型的真实形态：`Arc::new(...)` 包一层
        let arc = format!(
            "fn demo() {{\n    let g = std::sync::Arc::new({NEEDLE_EXPR}(1_577_836_800_000, 1));\n}}\n"
        );
        assert_eq!(violations_in(&arc, "t.rs"), vec!["  t.rs:2"]);

        // 2) 参数被 rustfmt 拆行，needle 仍在首行
        let multiline = format!(
            "fn demo() {{\n    let g = {NEEDLE_EXPR}(\n        1_577_836_800_000,\n        1,\n    );\n}}\n"
        );
        assert_eq!(violations_in(&multiline, "t.rs"), vec!["  t.rs:2"]);

        // 3) `::` 前后有空白（Rust 允许），不能给后人留后门
        let spaced = format!("fn demo() {{\n    let g = {NEEDLE_TYPE} :: new(1, 1);\n}}\n");
        assert_eq!(violations_in(&spaced, "t.rs"), vec!["  t.rs:2"]);

        // 4) 同一文件多处要**逐个**报行号，不是只报第一处
        let two = format!(
            "fn a() {{\n    let _x = {NEEDLE_EXPR}(1, 1);\n}}\nfn b() {{\n    let _y = {NEEDLE_EXPR}(1, 2);\n}}\n"
        );
        assert_eq!(violations_in(&two, "t.rs"), vec!["  t.rs:2", "  t.rs:5"]);
    }

    /// 反例：注释 / 字面量 / 只 import 类型 / 更长标识符，都不该报警。
    #[test]
    fn guard_detector_ignores_comments_strings_and_non_constructions() {
        // 1) 行注释（本仓 doc 里几十处「已不再就地 …」全是这个形态）
        let line_comment = format!(
            "fn demo() {{\n    // 本文件已不再就地 {NEEDLE_EXPR}，改用进程级共享 generator\n}}\n"
        );
        assert!(violations_in(&line_comment, "t.rs").is_empty());

        // 2) 块注释，含嵌套
        let block_comment = format!(
            "/*\n   历史写法：{NEEDLE_EXPR}\n   /* 嵌套里再提一次 {NEEDLE_EXPR} */\n*/\nfn demo() {{}}\n"
        );
        assert!(violations_in(&block_comment, "t.rs").is_empty());

        // 3) 文档注释（`//!` / `///`）—— 本仓模块 doc 的主形态
        let doc_comment =
            format!("//! 根因：{NEEDLE_EXPR} 每次都把 sequence 归零\nfn demo() {{}}\n");
        assert!(violations_in(&doc_comment, "t.rs").is_empty());

        // 4) 普通字符串（错误提示 / 断言消息里的字面量，不是构造）
        let string = format!("fn demo() {{\n    let msg = \"请勿 {NEEDLE_EXPR}\";\n}}\n");
        assert!(violations_in(&string, "t.rs").is_empty());

        // 5) raw string（本仓用 `r#"…"#` 贴旧写法代码样例）
        let raw = format!("fn demo() {{\n    let doc = r#\"旧写法：{NEEDLE_EXPR}\"#;\n}}\n");
        assert!(violations_in(&raw, "t.rs").is_empty());

        // 6) raw string 里含 `//` 时，其后的真实构造仍要被抓到
        //    （这一条同时钉住 raw string 的 `#` 计数与「`//` 不当注释」的优先级）
        let raw_then_real = format!(
            "fn demo<'a>(_x: &'a str) {{\n    let doc = r#\"http://a {NEEDLE_EXPR}\"#; let _g = {NEEDLE_EXPR}(1, 1);\n}}\n"
        );
        assert_eq!(violations_in(&raw_then_real, "t.rs"), vec!["  t.rs:2"]);

        // 7) 只 `use` 类型（`tests/**` 里遍地），不带 `::new` 不算构造
        let only_use = format!("use hsh_erp_rust::infra::snowflake::{NEEDLE_TYPE};\n");
        assert!(violations_in(&only_use, "t.rs").is_empty());

        // 8) 更长的标识符 / 更长的函数名，都不是构造
        let longer = format!(
            "fn demo() {{\n    let _a = {NEEDLE_TYPE}Wrapper::new(1, 1);\n    let _b = {NEEDLE_TYPE}::newer(1, 1);\n}}\n"
        );
        assert!(violations_in(&longer, "t.rs").is_empty());
    }

    /// 失败信息的内容也要有测试兜住：`file:line` 齐、两个正确写法都在、白名单依据在。
    #[test]
    fn failure_message_carries_guidance() {
        let msg = failure_message(1, "  tests/demo.rs:42");
        assert!(msg.contains("tests/demo.rs:42"), "缺 file:line：{msg}");
        assert!(msg.contains("shared_test_snowflake()"), "缺正确写法：{msg}");
        assert!(msg.contains("23505"), "缺后果说明：{msg}");
        assert!(
            msg.contains("test-support/src/pool.rs"),
            "缺白名单依据：{msg}"
        );
        assert!(
            msg.contains("哪一类 binary"),
            "缺新增白名单的前置问题：{msg}"
        );
    }

    /// 失败信息正文（主护栏与 `failure_message_carries_guidance` 共用，避免两处漂移）。
    fn failure_message(n: usize, list: &str) -> String {
        let mut lines = vec![
            format!("以下 {n} 处在白名单之外就地新建了雪花 ID 生成器（`{NEEDLE_EXPR}`）："),
            list.to_string(),
            String::new(),
            "为什么违规 —— 位布局 `ts << 22 | instance << 12 | seq` 里，`last_ms` / `sequence` 是"
                .to_string(),
            "generator 的**实例私有**字段，而构造一律从 `last_ms = 0, sequence = 0` 起步。于是任意"
                .to_string(),
            "两个「instance 相同、对象不同」的 generator 在同一毫秒各自取第 j 个号，会发出**逐字节相同**"
                .to_string(),
            "的 id → `23505 duplicate key ... t_*_pkey`。碰撞判据不是「同一个 helper 调几次」，而是"
                .to_string(),
            "**两个不同 helper 各调一次**。所以进程内唯一性的保证点是**对象共享**，不是 instance 编号"
                .to_string(),
            "（instance 只有 10 bit = 1024 槽，本就该留给跨进程区分；刻意填的字面量仍有 1/1024 概率"
                .to_string(),
            "与本进程派生值相等，而两个 fresh generator 的首个 id 恰好都是 `seq = 0`）。".to_string(),
            String::new(),
            "正确写法 —— 先确认自己在**哪一类 binary** 里，再二选一：".to_string(),
            "  * `tests/**`（集成测试 binary）→ `hsh_erp_test_support::shared_test_snowflake()`"
                .to_string(),
            "    （存量写法 `pool_snowflake().lock().unwrap().next_id()` 是它的兼容薄壳，同一个对象）"
                .to_string(),
            "  * `src/**` 的 `#[cfg(test)]`（lib 单测 binary）→"
                .to_string(),
            "    `crate::shared::test_snowflake::shared_test_snowflake()`".to_string(),
            "    ⚠️ 不要在 lib 单测里用 test-support 的同名函数：dev-dependency 环让该二进制里链进两份".to_string(),
            "    `hsh_erp_rust`，两个 `SnowflakeIdGenerator` 是**不同类型**，传参即 E0308。".to_string(),
            String::new(),
            format!("白名单（`src/shared/snowflake_guard.rs::SANCTIONED`，共 {} 项）：", SANCTIONED.len()),
        ];
        for (path, why) in SANCTIONED {
            lines.push(format!("  * `{path}` —— {why}"));
        }
        lines.push(
            "⚠️ 新增白名单条目前**必须先回答**：这条新路径属于哪一类 binary（集成测试 / lib 单测 /"
                .to_string(),
        );
        lines.push(
            "生产代码 / 被测对象自身）？答不出来就说明它不该就地构造，应走上面两个取号入口之一。"
                .to_string(),
        );
        lines.join("\n")
    }
}
