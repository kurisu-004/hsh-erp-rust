//! 域隔离护栏：把「本域不 import 其它域的 service / repo」从口头约定变成 CI 强制
//!
//! 2026-10-07 新增。只读跨域聚合域在本仓是既定 pattern（dashboard / statistics /
//! admin 都不 import 别域的 service / repo，而是各自在本域 SQL 里聚合），但口头
//! 约定挡不住回退，故用单测在源码层兜住。
//!
//! 已接入共 **7 处**，分两类：
//! - **整域接入 5 处**：`dashboard`（扫 `src/modules/dashboard`）、`iam`（扫
//!   `src/modules/iam`，含嵌套子模块 `iam::shelf`）、`prod::programming`（扫
//!   `src/modules/prod/programming`）、`prod::inspection`（扫
//!   `src/modules/prod/inspection`）—— 后两域是**嵌套域**，同父兄弟域
//!   （`prod::batch`）同样算跨域，由前缀匹配而非「首段相同即本域」判定 —— 以及
//!   `wx::production`（扫 `src/modules/wx/production`，2026-10-11 新增）。
//! - **非整域切片接入 3 处**：`prod::queue::board`（只扫
//!   `src/modules/prod/queue/board`）与 `prod::scan::listing`（只扫
//!   `src/modules/prod/scan/listing`）—— 这两个域整体**不适用**该护栏
//!   （queue 的写端点按既定 pattern 经本域 trait 转发他域单表查询；scan 是
//!   转发型域，其 `worker_scan` 必然 import 四处域），故各只圈出那块纯只读
//!   聚合 SQL 单独守；以及 `wx::part_list`（只扫
//!   `src/modules/wx/part_list`）—— `wx` 域**整体**不适用（它的 `login` 子模块
//!   必然 import `iam::service::account`），故只圈出 `part_list` 这块纯只读聚合
//!   切片（2026-10-11 新增）。
//!
//! 计数口径：**7 处 = 5 整域 + 3 非整域切片**。说「已接入的 N 域」时只数整域那 5
//! 处（切片不是域）；说「已接入 N 处调用」时才是 7。
//!
//! ## 探测口径
//! 在**剥掉注释**的代码区里找「跨域根段 + 双冒号 + 一个域路径」，该路径不是本域即违规。
//! 域路径按标识符逐段读，**末尾不接双冒号的那个标识符同样算一段**
//! （`use crate::<域根>::part;` 读到 `["part"]`）—— 所以「只 import 到域容器模块就停」的写法会按真实域路径报警，
//! 而不是被当成 glob 空段放过。
//!
//! 相比「逐行找 `use crate::` 字面量」，这样写覆盖面大得多：
//! - **不要求 `use` 前缀** —— `let s = crate::<域根>::part::…::new();` 这种全限定
//!   内联路径（Rust 里完全合法的写法）照样命中；
//! - **不要求两段路径相邻** —— `use crate::{<域根>::part::…}` 这种嵌套花括号
//!   写法同样命中；
//! - **注释不算代码** —— 为讲解规则而引用的外来路径不会误报。
//!
//! 剥注释走字符串状态机：普通字符串与 **raw string**（`r"…"` / `r#"…"#`，终止符是
//! `"` + `#`×n）内的 `"` 都不闭合状态，只有行尾的 `//` 才算行注释 —— 否则
//! `"http://…"` 与 `r#"…"//"#` 会把同行后半截（含真实跨域 import）整段吃掉
//! （raw string 支持 2026-10-07 新增）。
//!
//! 边界：只挡「他域」依赖，**不挡** `crate::shared` / `crate::infra` /
//! `crate::auth` / `crate::state` 等非域路径依赖（那些是刻意允许的公共设施）。
//!
//! ### 已知漏报盲区（**全部**登记在此 —— 未登记的漏报形态一律视为护栏缺陷）
//!
//! 探测器的触发点是**根段字面量本身**：先找到「根段 + `::`」，再把它后面的标识符逐段
//! 读成一个域路径做前缀匹配。下列形态都因为「读不出根段字面量」或「读不出连续的标识符
//! 段」而扫不到：
//!
//! 1. 段与段之间插空白：`use crate::<域根> :: part::…`（语法上完全合法）漏过。要覆盖
//!    它得上词法分析，规则本身是讲解材料，不值得为它加成本。同族的 `<域根>:: part::…`
//!    与 `…::part :: repo::…` 仍会命中，只是报出的域路径在空白处截断，前者显示为
//!    「空段/glob」。
//! 2. **不带根段字面量的裸路径引用**：`<域根>::<兄弟域>::service::…::new()`、`as` 改名
//!    后的 `<别名>::<兄弟域>::…`、域容器别名 `mods::<域>::…`、`macro_rules!` 体里的展开
//!    目标 —— 裸路径没有根段，探测器无从找起。
//!    边界要说清：本域源码目录内**任何**把外来域符号引入作用域的 `use` 必然带根段
//!    字面量（域都挂在 `crate::<域根>` 下），那条 `use` 自己就会被抓到；跨文件
//!    re-export 同理（目录树内全部 `.rs` 都扫）。所以这条盲区只在**引入语句位于扫描范围
//!    之外**时才可达 —— 他域文件 re-export 之后本域 `use` 那个模块、宏展开、构建期生成
//!    的代码。
//!    ⚠️ 上句「必然带根段字面量」有一个例外，就是**相对 `super::` 链**：模块路径既能用
//!    `crate::<域根>::…` 全限定写，也能从当前模块逐级 `super::` 上溯。嵌套域
//!    （`prod::inspection` 这类）的兄弟域就是 `../batch`，于是
//!    `use super::super::batch::service::BatchService;`（写在域内子模块文件里）/
//!    `use super::batch::…` / `use self::super::…`（写在域 `mod.rs` 里）三种相对写法
//!    都是合法的跨域引用，却一个根段字面量都没有 ⇒ 全部不命中（实测：对本域
//!    `prod::inspection` 扫这三个字符串，违规列表均为空）。相对路径不限于 `use`，
//!    内联的 `super::batch::service::X` 表达式同理。
//!    **只登记不修**：域间引用在本仓一律走 `crate::<域根>::` 全限定路径，现无此写法；
//!    而要覆盖它得让探测器解析模块树、把 `super::` 链按目录层级折叠回绝对路径，成本
//!    远超它在本仓的暴露面。
//! 3. **`use crate::<域根>;` 本身**（含 `as mods;`、含 `use crate::{<域根>};`）不命中：
//!    末尾没有 `::`，读不到段就当没看见。这一手把整个域容器拖进作用域，风险实打实，
//!    只是写法冷门；同一容器写成 glob（`use crate::<域根>::*;`）反而会被抓到。
//! 4. **未闭合块注释**（`/* …` / `/** …`）会让文件**剩余全部内容**被当注释跳过，其后的
//!    真实跨域 import 漏过。
//!
//! 第 4 条与 `crate::super::…` 属**输入本身编译不过的畸形形态**：前者是未闭合的块
//! 注释 / 普通字符串 / raw string（语法错），后者是 `super` 出现在非起始位置
//! （edition 2024 下 rustc 直接报 E0433）。两者都是 rustc 在产物之前就拦住、
//! 进不了被护栏扫的代码，这是本节「一律视为缺陷」的唯一例外。顺带说明：未闭合的普通
//! 字符串与 raw string **不**跨行吞（字符串状态不跨行传播），它们后面的 import 仍会
//! 被抓到。
//!
//! ### 已知精度边界（只会误报、不会漏报，方向上是安全的）
//! - **字符串正文**按代码扫：普通字符串字面量、以及**跨行** raw string（`r#"` 起、行内
//!   无终止符）正文里的外来域路径会被当代码报出来（行内闭合的 raw string 整段跳过、不报）。
//! - **字符字面量里的 `"`**（`let c = '"';`）会被当成字符串开始，同行的 `//` 于是不再算
//!   注释，注释里的外来路径因此被误报。极冷门，只登记不修。
//!
//! 两者都只会在「有人把外来域路径写进字符串正文 / 写成裸字符字面量」时触发，本仓两域
//! 无此写法。
//!
//! ## 本域标识符：`::` 拼接的域路径
//! `own_domain` 是域路径而非单段标识符，`"dashboard"` / `"prod::programming"` /
//! `"prod::inspection"` 三种形态都合法。判定规则是**前缀匹配**：从根段起逐段与本域路径比，全部相符即
//! 本域引用（含 `crate::<域根>::<本域>::…::repo` 这类子模块下钻）；在第 i 段分叉
//! 则报出到第 i 段为止的他域路径。
//!
//! 嵌套域必须走前缀匹配而非「首段相同即本域」：`prod::programming` 自引用写的是
//! `crate::<域根>::prod::programming::repo`，首段是父域 `prod`，若按首段判就会
//! 把 `prod::batch`（兄弟域）也一起放行，护栏等于没装。
//!
//! ## 用法（调用方在本域 `mod.rs` 的 `#[cfg(test)] mod tests` 里）
//! ```ignore
//! use std::path::Path;
//! use crate::shared::domain_guard::assert_no_foreign_domain;
//!
//! assert_no_foreign_domain(
//!     "dashboard",
//!     &Path::new(env!("CARGO_MANIFEST_DIR")).join("src/modules/dashboard"),
//!     "需要别的域的数据时，正确做法是像 statistics / admin 那样在本域 SQL 里只读聚合。",
//! );
//! ```
//!
//! 不连网、不连库，只读本域源码文件，故单测可在无 DB 环境跑。
//!
//! ⚠️ 设施本体是 `#[cfg(test)]` 项、不进 lib 产物，故**只有 `src/` 内的单元测试能用**
//! （`tests/` 下的集成测试引用它是 `could not find ...`，编译器注记
//! `found an item that was configured out`）。将来要写 `tests/` 集成测试时须改调用
//! 方式 —— 要么去掉 `shared/mod.rs` 里那行 `#[cfg(test)]`、接受它进生产 API 面，
//! 要么在 `tests/` 侧另写一份探测器。

use std::path::{Path, PathBuf};

/// 跨域路径的根段 + 双冒号。刻意拆成两段 `concat!` —— 写死完整字面量会让本文件
/// 自己也命中自己的探测器（自指误报），且日后把本文件纳入扫描范围就会立刻炸。
/// 同理，`ROOT_SEG` 之后一律用 `format!` / 数组 `.concat()` 拼装外来域名样例，
/// 源码里永远不出现「根段 + 外来域名」的完整字面量。
const ROOT_SEG: &str = concat!("modu", "les::");

/// 根段后接的不是标识符（`crate::<域根>::*` 这类 glob 写法）时的展示名
const EMPTY_SEG_LABEL: &str = "<空段/glob>";

/// 域隔离护栏：扫描 `src_dir` 下全部 `.rs`，代码区里出现任何**他域**路径即 panic。
///
/// `own_domain` —— 本域路径，`::` 拼接（`"dashboard"` / `"prod::programming"`）；
/// `src_dir` —— 本域源码目录（推荐 `env!("CARGO_MANIFEST_DIR")` + `join(相对路径)`）；
/// `data_hint` —— 域专属的「需要别的域数据时正确做法是什么」指引，出现在失败信息里。
///
/// panic 而非返回 `Result`：它是 CI 闸门，调用方都是单测，失败必须炸得直接可读。
pub fn assert_no_foreign_domain(own_domain: &str, src_dir: &Path, data_hint: &str) {
    let own = own_segments(own_domain);

    let mut files = Vec::new();
    collect_rs(src_dir, &mut files);
    assert!(
        !files.is_empty(),
        "扫描不到任何 .rs（{dir}），护栏本身失效",
        dir = src_dir.display()
    );

    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut violations: Vec<String> = Vec::new();
    for path in &files {
        let rel = path
            .strip_prefix(manifest)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        let Ok(src) = std::fs::read_to_string(path) else {
            continue;
        };
        for (line, col, seg) in scan_source(&src, &own) {
            violations.push(format!("  {rel}:{line}:{col}  →  {ROOT_SEG}{seg}"));
        }
    }

    // 规则块用 `join` 拼而非在字面量里续行：Rust 的 `\` 续行会把下一行的缩进一起吃掉，
    // 排版成一段、失败信息挤成一行，反而看不清「允许什么 / 禁止什么」。
    let rules = [
        "规则：".to_string(),
        format!(
            "  * 允许 —— {own_domain} 域自身（可下钻其子模块）、auth / infra / shared / state \
             等公共设施；"
        ),
        format!(
            "  * 禁止 —— 代码区（注释除外）里任何 `{ROOT_SEG}<其它域>` 路径，含 `use` 前缀、\
             全限定内联路径、嵌套花括号写法、glob 四种形态。"
        ),
    ]
    .join("\n");

    assert!(
        violations.is_empty(),
        "以下 {n} 处让 {own_domain} 域依赖了其它域（跨域耦合会让「改一域崩另一域」）：\n\
         {list}\n\n{rules}\n\n{data_hint}",
        n = violations.len(),
        list = violations.join("\n"),
    );
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

/// 去掉注释内容、**保留字符位置**（用等量空格顶替，失败信息里的列号才对得上）。
///
/// 跳过行注释（`//` / `///` / `//!`，含行尾注释）与块注释（`/* */` / `/** */`）；
/// 字符串字面量内的 `//` **不**当注释（否则 `"http://…"` 会把整行后半截吃掉）。
/// raw string 按「`"` + `#`×n」的终止符整体跳过（否则 `r#"…"//"#` 里内嵌的 `"` 提前
/// 闭合字符串状态，同行后半截的真实跨域 import 会被当行注释吃掉）。
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
                // raw string（2026-10-07 新增）：`r"` / `r#"` / `r##"`
                // 整体跳到终止符，其内的 `"` 不参与字符串状态判定
                if c == 'r'
                    && let Some(hashes) = raw_string_hashes(&chars, i)
                {
                    let end = raw_string_end(&chars, i + 1 + hashes, hashes);
                    out.extend(&chars[i..end]);
                    i = end;
                    continue;
                }
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

/// `chars[i..]` 是否是 raw string 起点，是则返回 `"` 之前的 `#` 个数。
///
/// `r#foo` 是裸标识符而非 raw string，靠「`#` 之后必须紧跟 `"`」区分开。
fn raw_string_hashes(chars: &[char], i: usize) -> Option<usize> {
    let hashes = chars
        .get(i + 1..)?
        .iter()
        .take_while(|c| **c == '#')
        .count();
    (chars.get(i + 1 + hashes) == Some(&'"')).then_some(hashes)
}

/// raw string 终止位置（含终止的 `"` 与 `#`×n）；本行内未闭合则返回行尾。
fn raw_string_end(chars: &[char], body: usize, hashes: usize) -> usize {
    (body + 1..chars.len())
        .find(|&j| chars[j] == '"' && chars[j + 1..].iter().take(hashes).all(|c| *c == '#'))
        .map_or(chars.len(), |j| j + 1 + hashes)
}

/// 把 `own_domain` 拆成段序列，并校验每段都是合法标识符。
///
/// 校验失败直接 panic（而不是「解析不出段就当没段」）：静默降级会让护栏变成永远
/// 全绿的空壳，比不装护栏更危险。
fn own_segments(own_domain: &str) -> Vec<String> {
    let segs: Vec<String> = own_domain.split("::").map(str::to_string).collect();
    let valid = !segs.is_empty()
        && segs.iter().all(|s| {
            !s.is_empty()
                && s.chars().all(|c| c.is_alphanumeric() || c == '_')
                && !s.chars().next().is_some_and(|c| c.is_ascii_digit())
        });
    assert!(
        valid,
        "本域标识符 {own_domain:?} 非法：须是 `::` 拼接的标识符路径（如 dashboard / prod::programming）"
    );
    segs
}

/// 从 `ROOT_SEG` 之后的 `abs` 位置起，吃掉连续的「标识符 :: 标识符 :: …」，
/// 返回 `(域路径段序列, 匹配长度)`；遇到非标识符内容即停。
///
/// **末尾那个不接双冒号的标识符也算一段**（`crate::<域根>::prod;` → `["prod"]`）：
/// 不算的话失败信息只能打「空段/glob」，把「只 import 了域容器模块、后面靠全限定
/// 路径取兄弟域符号」这种最该拦的写法报成了 glob 引入，看不出真实问题。
fn take_domain_path(code: &str, abs: usize) -> (Vec<String>, usize) {
    let rest = &code[abs..];
    let mut segs = Vec::new();
    let mut consumed = 0usize;
    loop {
        let ident: String = rest[consumed..]
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if ident.is_empty() {
            break;
        }
        let after = consumed + ident.len();
        if rest[after..].starts_with("::") {
            segs.push(ident);
            consumed = after + 2;
        } else {
            segs.push(ident);
            return (segs, after);
        }
    }
    (segs, consumed)
}

/// 一行代码里所有「引用了其它域」的片段，每项 = `(1 基列号, 他域路径)`。
///
/// 他域路径取到**分叉段为止**（`["prod","batch","service"]` 对 `prod::programming`
/// 报 `prod::batch`），空串也算违规：那对应「只引命名空间本身」与「glob 引入全部域」
/// 两种写法，前者在本域拿不到任何专属符号、后者把其它域一并拖进来。
fn foreign_domain_hits(code: &str, own: &[String]) -> Vec<(usize, String)> {
    let mut hits = Vec::new();
    let mut from = 0usize;
    while let Some(rel) = code[from..].find(ROOT_SEG) {
        let abs = from + rel;
        let (segs, consumed) = take_domain_path(code, abs + ROOT_SEG.len());
        if let Some(seg) = first_divergence(&segs, own) {
            hits.push((
                code[..abs].chars().count() + 1,
                if seg.is_empty() {
                    EMPTY_SEG_LABEL.to_string()
                } else {
                    seg
                },
            ));
        }
        // `from` 前进到本次匹配末尾即可：域路径段由标识符构成，不可能再嵌 `ROOT_SEG`；
        // `consumed == 0`（如 `<域根>::` 后不是标识符）时也已至少跨过 `ROOT_SEG` 本身
        from = abs + ROOT_SEG.len() + consumed;
    }
    hits
}

/// `segs` 与本域路径 `own` 逐段比对，返回**到分叉段为止的他域路径**（`None` = 本域引用）。
///
/// 前缀匹配：`["prod","programming","repo"]` 对 `["prod","programming"]` 无分叉（子模块
/// 下钻仍是本域）；`["prod","batch","service"]` 在第 2 段分叉，报 `prod::batch`。
fn first_divergence(segs: &[String], own: &[String]) -> Option<String> {
    for i in 0..own.len().min(segs.len()) {
        if segs[i] != own[i] {
            return Some(segs[..=i].join("::"));
        }
    }
    if segs.len() >= own.len() {
        // 逐段相符：本域路径本身，或本域路径之下挂着的子模块（`…::<本域>::…`）
        return None;
    }
    // 路径在本域路径**中途**就断了（如 `crate::<域根>::prod::{…}` 对 prod::programming）
    // —— 它会拖进本域之外的一切，报出实际走到的路径
    Some(segs.join("::"))
}

/// 扫一份源码，返回所有违规 `(行号, 列号, 他域路径)`（行号 1 基）。
fn scan_source(src: &str, own: &[String]) -> Vec<(usize, usize, String)> {
    let mut violations = Vec::new();
    let mut in_block = false;
    for (idx, raw) in src.lines().enumerate() {
        let (code, next_block) = blank_comments(raw, in_block);
        in_block = next_block;
        for (col, seg) in foreign_domain_hits(&code, own) {
            violations.push((idx + 1, col, seg));
        }
    }
    violations
}

#[cfg(test)]
mod tests {
    use super::{EMPTY_SEG_LABEL, ROOT_SEG, own_segments, scan_source};

    /// 元测试用的本域标识符（探测器的「自己人」长什么样，与调用方无关）
    const OWN: &str = "dashboard";

    // ── 元测试：探测器本身必须能报警，否则「全绿」只是因为探测器瞎了 ──────────

    /// 正例：四种**代码区**写法都必须被抓到。
    /// 覆盖盲区 ① 嵌套花括号 ② 全限定内联路径 ③ glob ④ `pub use`。
    ///
    /// ⚠️ 带花括号的样例一律用数组 `.concat()` 拼、不用 `format!`：`format!` 里
    /// 想同时表达「字面花括号」与「具名参数」得写 `{{` / `}}` 转义，把域名塞进
    /// `{}` 内部极易读错、样例会退化成无意义代码。
    #[test]
    fn guard_flags_foreign_domain_in_code_any_shape() {
        let own = own_segments(OWN);
        let brace = [
            "use crate::{",
            ROOT_SEG,
            "statistics::repo::sql::count_overdue};",
        ]
        .concat();
        assert!(
            brace.contains('{') && brace.contains('}'),
            "样例里花括号丢了，测的就不是花括号分支：{brace}"
        );
        assert_eq!(
            scan_source(&brace, &own),
            vec![(1, 13, "statistics".to_string())],
            "嵌套花括号写法不该漏过"
        );

        let brace_inline = [
            "let s = crate::{",
            ROOT_SEG,
            "part::repo::PartRepo::new()};",
        ]
        .concat();
        assert_eq!(
            scan_source(&brace_inline, &own),
            vec![(1, 17, "part".to_string())],
            "全限定 + 花括号混写不该漏过"
        );

        let inline = format!("let s = crate::{ROOT_SEG}part::service::PartService::new();");
        assert_eq!(
            scan_source(&inline, &own),
            vec![(1, 16, "part".to_string())],
            "全限定内联路径（无 `use` 前缀）不该漏过"
        );

        let glob = ["use crate::{", ROOT_SEG, "*};"].concat();
        assert_eq!(
            scan_source(&glob, &own),
            vec![(1, 13, EMPTY_SEG_LABEL.to_string())],
            "glob 引入会把所有域拖进来，不该漏过"
        );

        let with_vis = format!("pub use crate::{ROOT_SEG}assembly::repo::AssemblyRepo;");
        assert_eq!(
            scan_source(&with_vis, &own),
            vec![(1, 16, "assembly".to_string())],
            "`pub use` 形态不该漏过"
        );
    }

    /// 反例：本域引用、以及**注释里**提到的外来路径，一律不许报警
    /// （否则为讲解规则写一行文档注释就得红）。
    #[test]
    fn guard_stays_quiet_on_own_domain_and_comments() {
        let own = own_segments(OWN);
        let own_own = ["use crate::{", ROOT_SEG, OWN, "::vo::*};"].concat();
        assert!(
            scan_source(&own_own, &own).is_empty(),
            "本域 import（含花括号写法）不该被报警：{own_own}"
        );

        let inline_own = format!("let x = crate::{ROOT_SEG}{OWN}::dto::DeliveryBasis::System;");
        assert!(
            scan_source(&inline_own, &own).is_empty(),
            "本域全限定内联路径不该被报警：{inline_own}"
        );

        for comment in [
            ["/// 详见 crate::{", ROOT_SEG, "statistics::repo::sql"].concat(),
            format!("//! 改口径时对照 crate::{ROOT_SEG}part::repo"),
            format!("/* 历史包袱：crate::{ROOT_SEG}part::repo */"),
            ["let n = 1; // 旧实现走 crate::{", ROOT_SEG, "part::repo"].concat(),
            format!("/**\n * 举例：crate::{ROOT_SEG}part::repo\n */\nlet n = 1;"),
            format!("//! 本段列的路径都指向本域\nuse crate::{ROOT_SEG}{OWN}::vo::V;"),
        ] {
            assert!(
                scan_source(&comment, &own).is_empty(),
                "注释里的外来路径不该被报警：{comment}"
            );
        }
    }

    /// 字符串字面量里的 `//` 不当注释 —— 否则 `"http://…"` 会把整行后半截吃掉，
    /// 真正写在同一行尾部的外来 import 就漏过了。
    #[test]
    fn guard_does_not_mistake_url_for_line_comment() {
        let own = own_segments(OWN);
        let src = format!("let url = \"http://x/y\";\nuse crate::{ROOT_SEG}statistics::repo::R;");
        assert_eq!(
            scan_source(&src, &own),
            vec![(2, 12, "statistics".to_string())],
            "URL 里的 `//` 被当成注释会让第 2 行的真实违规漏过"
        );
    }

    /// raw string（`r#"…"#`）里的 `"` 不闭合字符串状态 —— 否则内嵌的 `"` 提前闭合后，
    /// 紧随其后的 `//` 被当行注释，同行真实跨域 import 就漏过了。
    #[test]
    fn guard_does_not_mistake_raw_string_end_for_string_end() {
        let own = own_segments(OWN);
        let src = format!("let s = r#\"a\"//\"#; use crate::{ROOT_SEG}statistics::repo::R;");
        assert_eq!(
            scan_source(&src, &own),
            vec![(1, 31, "statistics".to_string())],
            "raw string 里的 `\"` 提前闭合会让同行的真实违规漏过：{src}"
        );

        let multiline =
            format!("let s = r#\"行内没终止符\n继续\"#; use crate::{ROOT_SEG}part::repo::R;");
        assert_eq!(
            scan_source(&multiline, &own),
            vec![(2, 18, "part".to_string())],
            "跨行 raw string 的第二行同样不该漏过：{multiline}"
        );

        // 裸标识符 `r#foo` 不是 raw string，不能被当成 raw string 起点而吞掉后面的代码
        let raw_ident = format!("let x = r#fn; use crate::{ROOT_SEG}part::repo::R;");
        assert_eq!(
            scan_source(&raw_ident, &own),
            vec![(1, 26, "part".to_string())],
            "裸标识符不该被误判成 raw string 起点：{raw_ident}"
        );
    }

    /// 嵌套域（本域路径 2 段）的前缀匹配：只放行本域路径之下的一切，
    /// **兄弟域（同父不同段）必须报警** —— 这是嵌套域护栏的全部价值所在。
    #[test]
    fn guard_matches_nested_own_domain_by_prefix() {
        let own = own_segments("prod::programming");

        let self_ref = [
            "use crate::{",
            ROOT_SEG,
            "prod::programming::repo::{ProgrammingRepo};",
        ]
        .concat();
        assert!(
            scan_source(&self_ref, &own).is_empty(),
            "嵌套域自引用不该被报警：{self_ref}"
        );

        let sibling = [
            "use crate::{",
            ROOT_SEG,
            "prod::batch::service::BatchService};",
        ]
        .concat();
        assert_eq!(
            scan_source(&sibling, &own),
            vec![(1, 13, "prod::batch".to_string())],
            "兄弟域（prod 下的另一个子域）属于跨域，不该放行"
        );

        let top_level = format!("use crate::{ROOT_SEG}part::repo::PartRepo;");
        assert_eq!(
            scan_source(&top_level, &own),
            vec![(1, 12, "part".to_string())],
            "顶层他域不该放行"
        );

        // 兄弟域裸路径（不套花括号）：分叉段在第 2 段，与上面的花括号写法同一条出口
        let sibling_bare = format!("use crate::{ROOT_SEG}prod::batch::x;");
        assert_eq!(
            scan_source(&sibling_bare, &own),
            vec![(1, 12, "prod::batch".to_string())],
            "兄弟域裸路径不该放行：{sibling_bare}"
        );

        // ⚠️ 「路径在本域路径中途就断」这条出口专治**整个他域被拖进来**的写法：
        // 只 import 到 `prod` 就停、后面靠 `prod::…` 全限定路径取兄弟域的符号。
        // 哪天有人把 `segs.len() >= own.len()` 判反，这里会静默漏报而其它元测试全绿，
        // 故必须留锁。
        let parent_brace = [
            "use crate::{",
            ROOT_SEG,
            "prod::{batch::service::BatchService}};",
        ]
        .concat();
        assert_eq!(
            scan_source(&parent_brace, &own),
            vec![(1, 13, "prod".to_string())],
            "只 import 到父域 + 花括号会把父域之外的一切拖进来，不该放行：{parent_brace}"
        );

        let parent_bare = format!("use crate::{ROOT_SEG}prod;");
        assert_eq!(
            scan_source(&parent_bare, &own),
            vec![(1, 12, "prod".to_string())],
            "只 import 到父域会把父域之外的一切拖进来，不该放行：{parent_bare}"
        );

        // 本域自身的整体 import 仍放行（本域标识符也是一等的模块路径）
        let own_bare = format!("use crate::{ROOT_SEG}prod::programming;");
        assert!(
            scan_source(&own_bare, &own).is_empty(),
            "import 本域自身不该被报警：{own_bare}"
        );
    }

    /// 本域标识符非法时必须 panic（静默降级会让护栏变成永远全绿的空壳）。
    /// 传含空格的一类：`"prod ::"` 的第二段是空段，不是合法标识符。
    #[test]
    #[should_panic(expected = "非法")]
    fn guard_rejects_illegal_own_domain() {
        let _ = own_segments("prod ::");
    }
}
