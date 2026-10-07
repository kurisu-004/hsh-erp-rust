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
//! 3. **畸形输入的后果按实际状态机如实记录**（2026-10-09 review 第 1 轮订正）——
//!    rustc 在产物之前就拦住这类输入，护栏读到的真实源码一定是合法 Rust，故这里没有
//!    「安全降级」可言。实测两条：**未闭合块注释** ⇒ 其后全部内容被当注释跳过
//!    （**不可见**，即漏报，已登记在「已知漏报盲区」）；**未闭合字符串 / raw string**
//!    ⇒ `raw_string_end` / `string_end` 返回 `None`，该字节退回按**代码**处理、其后
//!    内容照常扫描（**可见**，不漏）。初版此处把两者一并写成「按代码扫」，与实测相反。
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
//! 3. **类型别名 / `use` 重命名**（2026-10-09 review 第 1 轮 M1 补登记）：
//!    `use …::SnowflakeIdGenerator as G;` 后 `G::new(..)`、或
//!    `type G = SnowflakeIdGenerator;` 后 `G::new(..)`，源码里都不出现类型名字面量
//!    ⇒ 漏报。这是**真实漏报**（不是「不可能出现的输入」：起个短别名是自然写法），
//!    只登记不修：要覆盖得做名字解析。第 1 条（`Self::new`）是它的同类。
//!
//! ### 已排除项（不是绕过口，防后人重新打开）
//! - **`SnowflakeIdGenerator::default()` 不会绕过本规则** —— `src/infra/snowflake.rs`
//!   **没有** `impl Default`（`SnowflakeIdGenerator` 只有 `new` 一个构造函数），
//!   故这个输入编译不过。⚠️ 若将来给它加了 `impl Default`，本护栏会**立刻**变成
//!   漏报且无人察觉 —— 那时必须把 `Default::default` 一并纳入 needle。
//!
//! ## 规则 2：`src/**` 不得拉入第二个 generator（`no_lib_unit_test_pulls_in_a_second_generator`）
//!
//! 规则 1 治的是「就地 `new` 一个 generator」；但 lib 单测还有**第二条**把第二个
//! generator 拉进同一进程的路径，且它**不是**一处 `::new`、规则 1 抓不到：
//! `test-support` 自己也持有 generator（`pool_snowflake` / `shared_test_snowflake`
//! → `state.rs` 的三个 `test_state*` → `AppState.snowflake`），而 `test-support` 是
//! `[dev-dependencies]` 且 path-depends 回主 crate（dev-dependency 环，成因与后果见
//! `SANCTIONED` 的 doc），一旦 `src/**` 的 `#[cfg(test)]` 调了它的任一 generator 入口，
//! 同一个 `cargo test --lib` 进程里就会出现**两个各自独立派生 instance 的 generator**
//! —— instance 不保证不同 ⇒ 23505 可复发。
//!
//! 因此规则 2 采取**白名单式**判定：`src/**` 的代码区里出现 `hsh_erp_test_support`
//! 即失败，**只**放行两个不碰 generator 的入口（`::test_pool` / `::test_redis_url`）。
//! 选白名单而不是「列举危险符号」是刻意的：glob 导入（`use hsh_erp_test_support::*;`）
//! 与分组导入（`use hsh_erp_test_support::{test_app, test_pool};`）都不含那些符号的
//! 路径形态，按符号名列举会成片漏；而 test-support 将来新增的任何入口都可能是
//! generator 携带者，白名单天然覆盖。
//!
//! 现状核对：`src/**` 对 test-support 的引用只有 `test_pool` 3 处
//! （`shared/batch/guards.rs`、`prod/queue/service/{queue,dispatch}.rs`）与
//! `test_redis_url` 1 处（`wx/wecom_client.rs`，也是 dev-dep 环的制造者）。
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

/// 被允许就地新建 `SnowflakeIdGenerator` 的文件里，**额外**记「命中处数上限」
/// （相对 crate 根的路径 → 上限）。
///
/// 2026-10-09（review 第 1 轮 M2）：`SANCTIONED` 是**整文件**放行，所以这几个文件里
/// **将来新增**的构造不会被规则 1 抓到 —— 而 `src/main.rs` 恰恰是生产环境里最危险的一种
/// 漂移（为了「某个模块单独发号」再建一个 generator ⇒ 同进程两个 generator ⇒ 跨表撞号）。
/// 上限把口子收窄成「改动会被发现、且必须显式改这个常量」。取值由
/// `sanctioned_files_stay_within_their_hit_cap` 用本探测器**实测**得出，不是估的。
///
/// **2026-10-09（review 第 2 轮 N2）补齐两个「唯一源」文件**：原先只给
/// `src/main.rs` / `src/infra/snowflake.rs` 记上限，恰恰漏掉了护栏最该守的两处 ——
/// `test-support/src/pool.rs` 与 `src/shared/test_snowflake.rs`。漏掉它们的理由是
/// 「按定义就该有构造点」，但那个推理只对白名单放行的**范围**成立，放行**范围**恰恰
/// 是问题的成因：将来若有人在这两个文件里**新建第二个** generator（旧的还在用），
/// 规则 1（白名单外禁 `::new`）与规则 2（`src/**` 禁拉入 test-support 的 generator
/// 入口）**都抓不到** —— 因为新构造点就住在白名单文件内部。
///
/// ⚠️ **这两条的语义是「恰好一个」而不是「至多 N 个」**：`SANCTIONED_HIT_CAPS` 记的是
/// 「**这个文件总共几处构造**」，对 `src/main.rs` / `src/infra/snowflake.rs` 而言多一处
/// 只是「多了一个 generator 对象」；但这两个文件是**唯一源** —— 集成测试 binary 与
/// lib 单测 binary 各进程域**唯一**那个取号对象。多一个构造点 = 同一进程里出现**两条
/// ID 流**，而这正是 2026-10-09 那一轮改造要消灭的东西（两个 fresh generator 都从
/// `last_ms=0, sequence=0` 起步，同 instance + 背靠背毫秒 ⇒ 逐字节相同的 id ⇒
/// `23505`）。故必须把上限压到当前实测值，不给它留「再加一个也行」的余量。
const SANCTIONED_HIT_CAPS: &[(&str, usize)] = &[
    // 生产：唯一一处 —— `main.rs` 的 `Arc::new(SnowflakeIdGenerator::new(config…))`
    ("src/main.rs", 1),
    // 被测对象自身：位布局 / sequence 回绕 / epoch / `MAX_INSTANCE` panic 边界等 8 处单测
    ("src/infra/snowflake.rs", 8),
    // 2026-10-09（N2）：集成测试 binary 的**唯一** ID 源（`TEST_SNOWFLAKE_GEN` 的
    // `get_or_init` 里那一句）。**恰好一个** —— 第二个即两条 ID 流。
    ("test-support/src/pool.rs", 1),
    // 2026-10-09（N2）：lib 单测 binary 的**唯一** ID 源（`SHARED_TEST_SNOWFLAKE` 的
    // `get_or_init` 里那一句）。**恰好一个** —— 第二个即两条 ID 流。
    ("src/shared/test_snowflake.rs", 1),
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

/// 规则 2 的 needle：**test-support 这个 crate 名本身**，同样用 `concat!` 拆开
/// （理由同上：本文件的失败文案里会出现完整字面量，不拆会自指误报）。
const NEEDLE_TS_CRATE: &str = concat!("hsh_erp_", "test_support");
/// 规则 2 放行的两个入口（`NEEDLE_TS_CRATE` 之后必须紧跟其中之一 + 非标识符边界）。
///
/// 白名单而非黑名单的理由见模块 doc「规则 2」一节。⚠️ 新增条目前必须回答：
/// 这个 test-support 入口**会不会构造或取用一个 generator**？会 → 不得进本表。
const TS_ALLOWED_ENTRIES: &[&str] = &["::test_pool", "::test_redis_url"];
/// 规则 2 的白名单文件（相对 crate 根）—— 只放行「**提及**」test-support 的位置：
/// `test_snowflake.rs` 的模块 doc 论证为什么不能用它；本文件的失败文案要告诉人正确写法。
const TS_SANCTIONED: &[(&str, &str)] = &[
    (
        "src/shared/test_snowflake.rs",
        "模块 doc 里论证「为什么 lib 单测不能用 test-support 的同名函数」（doc 与测试都在本文件内）",
    ),
    (
        "src/shared/snowflake_guard.rs",
        "失败文案里给出两个允许入口与正确写法（护栏自身，不是被治理对象）",
    ),
];

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
            // 2026-10-09（review 第 1 轮 B1）：`depth` 必须从 **0** 起步，让**开界本身**
            // 也由下面的内层循环统一计数（`/*` 会命中「嵌套开始」分支）。原先写 `1`
            // 会把开界重复计一次 ⇒ 实际从 2 起步 ⇒ 配平的 `*/` 只降回 1、
            // `if depth == 0 { break }` 永不触发 ⇒ 一路扫到 EOF，把该文件里**此后全部
            // 内容**当成注释跳过（`/* note */` 这种单层块注释就能让整条规则对该文件失效）。
            // 现状：扫描范围 502 个 `.rs` 里 50 个含真实块注释。
            // ⚠️ `src/shared/batch/status.rs` 的 `write_guard_tests` 里有一份逐字相同的旧
            // 拷贝，仍带这个缺陷（对本轮规则无影响、对 batch 护栏当前也无误报），
            // 已登记为 follow-up，本轮刻意不改那个文件。
            let mut depth = 0usize;
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

/// 规则 2 的判定：返回 `src/**` 代码区里所有「引用 test-support 且**不是**两个允许入口」
/// 的**代码行**（1 基行号）。
///
/// 判定口径（与规则 1 同源的字面量纪律）：
/// - 左边界不是标识符字符（`…_hsh_erp_test_support` 不算命中）；
/// - `NEEDLE_TS_CRATE` 之后允许任意空白再接 `::`（Rust 允许 `crate :: path`，漏掉等于
///   给后人留后门）；
/// - 紧跟的两个允许入口（`::test_pool` / `::test_redis_url`）放行，且要求其后不是标识符
///   字符（`::test_pool_helper` 不算放行）；
/// - 注释 / 字符串 / raw string 里的提及不报警（doc 里论证「为什么不用它」是允许的）。
fn test_support_entry_lines(src: &str) -> Vec<usize> {
    let (out, kind) = scan_rust(src);
    let mut hits = Vec::new();
    let mut from = 0usize;
    while let Some(at) = find_word_at(&out, from, NEEDLE_TS_CRATE.as_bytes()) {
        from = at + 1;
        if kind.get(at) != Some(&KIND_CODE) {
            continue;
        }
        let p = skip_ws(&out, at + NEEDLE_TS_CRATE.len());
        let allowed = TS_ALLOWED_ENTRIES.iter().any(|entry| {
            let e = entry.as_bytes();
            if !out[p..].starts_with(e) {
                return false;
            }
            !out.get(p + e.len()).is_some_and(|c| is_ident(*c))
        });
        if allowed {
            continue;
        }
        hits.push(1 + out[..at].iter().filter(|c| **c == b'\n').count());
    }
    hits
}

/// 规则 2 的「扫描 + 判定 + 报错」收拢（供测试直接喂假源码）。
fn ts_violations_in(src: &str, rel: &str) -> Vec<String> {
    test_support_entry_lines(src)
        .into_iter()
        .map(|line| format!("  {rel}:{line}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        NEEDLE_EXPR, NEEDLE_TS_CRATE, NEEDLE_TYPE, SANCTIONED, SANCTIONED_HIT_CAPS, SCAN_ROOTS,
        TS_ALLOWED_ENTRIES, TS_SANCTIONED, collect_rs, construction_lines, ts_violations_in,
        violations_in,
    };
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

        // 5) 2026-10-09（review 第 1 轮 B1 回归）：**块注释之后**的真实构造必须照样抓到。
        //    这两条是 B1 的守门元测试 —— 初版块注释状态机把开界重复计一次，导致
        //    `/* … */`（含单层）之后的内容全部被当注释跳过、整条规则对该文件静默失效，
        //    而当时的元测试用例后面跟的是 `fn demo() {}`（内部无构造）所以照样通过。
        //    单层：`depth` 从 0 起步后，配平的 `*/` 恰好把它降回 0。
        let block_then_real = format!(
            "/* 历史写法：{NEEDLE_EXPR} */\nfn demo() {{\n    let _g = {NEEDLE_EXPR}(0, 1);\n}}\n"
        );
        assert_eq!(violations_in(&block_then_real, "t.rs"), vec!["  t.rs:3"]);

        //    嵌套：内层 `/* */` 只把 `depth` 加一减一，**不**该让外层的配平失效。
        let nested_then_real = format!(
            "/* a /* b {NEEDLE_EXPR} */ c */\nfn demo() {{\n    let _g = {NEEDLE_EXPR}(0, 1);\n}}\n"
        );
        assert_eq!(violations_in(&nested_then_real, "t.rs"), vec!["  t.rs:3"]);

        //    块注释里的字面量仍然不报警（回归：修 B1 不能把块注释变成"什么都报"）
        let block_only = "/* a /* b */ c */\nfn demo() {}\n".to_string();
        assert!(violations_in(&block_only, "t.rs").is_empty());
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

    // ── 规则 1 的白名单收紧：整文件放行的四个文件另记「命中处数上限」（M2 / N2）────

    /// `SANCTIONED_HIT_CAPS` 里的文件命中数不得超过上限。
    ///
    /// 为什么需要：`SANCTIONED` 是整文件放行，所以白名单文件里若将来**再开一个**
    /// generator，规则 1 不会响。两类后果：
    /// - `src/main.rs`：生产环境最危险的漂移（为了「某个模块单独发号」再建一个 ⇒
    ///   同进程两个 generator ⇒ 跨表撞号）；
    /// - `test-support/src/pool.rs` / `src/shared/test_snowflake.rs`（2026-10-09 N2
    ///   补齐）：这两个是**唯一源**文件，多一个构造点就是**两条 ID 流**，而它们恰好
    ///   是规则 1（白名单外）与规则 2（禁拉入 test-support generator 入口）都够不着
    ///   的位置 —— 新构造点就住在白名单文件内部。
    ///
    /// 两条断言与 `SANCTIONED_HIT_CAPS` 文档一致：`hits.len() <= cap`（上限）+
    /// `!hits.is_empty()`（防上限表无声腐烂 —— 构造点被删光后条目就该移除，而不是
    /// 永远躺在一张没人看的表里）。
    #[test]
    fn sanctioned_files_stay_within_their_hit_cap() {
        let root = root();
        for (rel, cap) in SANCTIONED_HIT_CAPS {
            let path = root.join(rel);
            let src = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("读不到 {rel}（上限表指向的文件必须存在）：{e}"));
            let hits = construction_lines(&src);
            assert!(
                hits.len() <= *cap,
                "{rel} 里有 {} 处就地新建（上限 {cap}，行号 {hits:?}）。\n\
                 若这是**有意**新增的，请同步调高 `SANCTIONED_HIT_CAPS` 里的上限并写明缘由；\n\
                 若不是，说明有人在白名单文件里新开了 generator —— 这正是整文件白名单掩盖不了的\
                 那一类漂移（生产代码里的第二个 generator ⇒ 同进程跨表撞号；两个**唯一源**文件\
                 （`test-support/src/pool.rs` / `src/shared/test_snowflake.rs`）里的第二个 ⇒\
                 同一进程两条 ID 流）。",
                hits.len()
            );
            assert!(
                !hits.is_empty(),
                "{rel} 当前 0 处命中：上限条目已失去意义，请从 `SANCTIONED_HIT_CAPS` 移除它"
            );
        }
    }

    // ── 规则 2：`src/**` 不得拉入第二个 generator ────────────────────────────────

    /// `src/**` 的代码区里只允许两个**不碰 generator** 的 test-support 入口。
    #[test]
    fn no_lib_unit_test_pulls_in_a_second_generator() {
        let root = root();
        let mut acc = Vec::new();
        collect_rs(&root.join("src"), &mut acc);
        assert!(
            !acc.is_empty(),
            "扫不到 `src/**/*.rs`（CARGO_MANIFEST_DIR={}），规则 2 本身失效",
            root.display()
        );

        let mut violations: Vec<String> = Vec::new();
        for path in &acc {
            let rel = rel_of(path, &root);
            if TS_SANCTIONED.iter().any(|(p, _)| *p == rel) {
                continue;
            }
            let Ok(src) = std::fs::read_to_string(path) else {
                continue;
            };
            violations.extend(ts_violations_in(&src, &rel));
        }

        assert!(
            violations.is_empty(),
            "{msg}",
            msg = ts_failure_message(violations.len(), &violations.join("\n"))
        );
    }

    /// 规则 2 的白名单条目不许指向已删除的文件。
    #[test]
    fn ts_whitelist_entries_all_exist() {
        let root = root();
        let missing: Vec<&str> = TS_SANCTIONED
            .iter()
            .map(|(p, _)| *p)
            .filter(|p| !root.join(p).is_file())
            .collect();
        assert!(
            missing.is_empty(),
            "规则 2 的白名单里这些文件已不存在，请清理 `TS_SANCTIONED` 条目：\n  {}\n\
             （白名单腐烂 = 真正该被拦的引用会被误放行）",
            missing.join("\n  ")
        );
    }

    /// 元测试（正例）：会拉进第二个 generator 的四种真实写法一律抓到，行号逐个报对。
    #[test]
    fn ts_detector_flags_generator_carrying_entries() {
        // 1) 最直接的一种：拿 test-support 的 AppState 工厂
        let state = format!("fn demo() {{\n    use {NEEDLE_TS_CRATE}::state::test_state;\n}}\n");
        assert_eq!(ts_violations_in(&state, "t.rs"), vec!["  t.rs:2"]);

        // 2) 直接调共享 generator（类型不同会 E0308，但一旦有人加 `as` 或换签名就成真事故）
        let shared =
            format!("fn demo() {{\n    let _g = {NEEDLE_TS_CRATE}::shared_test_snowflake();\n}}\n");
        assert_eq!(ts_violations_in(&shared, "t.rs"), vec!["  t.rs:2"]);

        // 3) 分组导入 —— **不含** `::state::` 形态，按符号名列举会成片漏
        let grouped = format!("use {NEEDLE_TS_CRATE}::{{test_app, test_pool}};\nfn demo() {{}}\n");
        assert_eq!(ts_violations_in(&grouped, "t.rs"), vec!["  t.rs:1"]);

        // 4) glob 导入 —— 把 `pool_snowflake` / `shared_test_snowflake` 全带进作用域
        let glob = format!("use {NEEDLE_TS_CRATE}::*;\nfn demo() {{}}\n");
        assert_eq!(ts_violations_in(&glob, "t.rs"), vec!["  t.rs:1"]);

        // 5) `::` 前后有空白（Rust 允许），不能给后人留后门
        let spaced = format!("use {NEEDLE_TS_CRATE} :: state :: test_state;\n");
        assert_eq!(ts_violations_in(&spaced, "t.rs"), vec!["  t.rs:1"]);

        // 6) 本 crate 自己的同名函数**不是**违规（`src/**` 里遍地，必须放行）
        let own =
            "fn demo() {\n    let _g = crate::shared::test_snowflake::shared_test_snowflake();\n}\n"
                .to_string();
        assert!(ts_violations_in(&own, "t.rs").is_empty());
    }

    /// 元测试（反例）：两个允许入口、注释 / doc / 字符串里的提及，都不报警。
    #[test]
    fn ts_detector_allows_pool_and_redis_url_and_prose() {
        // 1) 两个允许入口（现状 `src/**` 对 test-support 的全部 4 处引用）
        for entry in TS_ALLOWED_ENTRIES {
            let ok = format!("use {NEEDLE_TS_CRATE}{entry};\n");
            assert!(
                ts_violations_in(&ok, "t.rs").is_empty(),
                "允许入口 {entry} 被误报"
            );
        }
        let called =
            format!("fn demo() {{\n    let _u = {NEEDLE_TS_CRATE}::test_redis_url();\n}}\n");
        assert!(ts_violations_in(&called, "t.rs").is_empty());

        // 2) 名字更长的东西不是允许入口（`::test_pool_helper` 不该被放行）
        let longer = format!("use {NEEDLE_TS_CRATE}::test_pool_helper;\n");
        assert_eq!(ts_violations_in(&longer, "t.rs"), vec!["  t.rs:1"]);

        // 3) 前缀更长的 crate 名不是命中（`my_hsh_erp_test_support`）
        let prefixed = format!("use my_{NEEDLE_TS_CRATE}::state::test_state;\n");
        assert!(ts_violations_in(&prefixed, "t.rs").is_empty());

        // 4) 行注释 / 文档注释 / 字符串 / raw string 里的提及（论证性文字）不报警
        let line_comment = format!("// 2026-10-09：不要用 {NEEDLE_TS_CRATE}::state，dev-dep 环\n");
        assert!(ts_violations_in(&line_comment, "t.rs").is_empty());
        let doc_comment = format!("//! 不能用 {NEEDLE_TS_CRATE}::state::test_state\n");
        assert!(ts_violations_in(&doc_comment, "t.rs").is_empty());
        let string = format!("fn demo() {{\n    let m = \"{NEEDLE_TS_CRATE}::state\";\n}}\n");
        assert!(ts_violations_in(&string, "t.rs").is_empty());
        let raw = format!("fn demo() {{\n    let d = r#\"{NEEDLE_TS_CRATE}::state\"#;\n}}\n");
        assert!(ts_violations_in(&raw, "t.rs").is_empty());
    }

    /// 规则 2 的失败信息要给出「哪两个入口被允许 + 正确写法 + 成因」。
    #[test]
    fn ts_failure_message_carries_guidance() {
        let msg = ts_failure_message(1, "  src/demo.rs:42");
        assert!(msg.contains("src/demo.rs:42"), "缺 file:line：{msg}");
        assert!(
            msg.contains("crate::shared::test_snowflake::shared_test_snowflake()"),
            "缺正确写法：{msg}"
        );
        assert!(msg.contains("::test_pool"), "缺允许入口：{msg}");
        assert!(msg.contains("::test_redis_url"), "缺允许入口：{msg}");
        assert!(msg.contains("23505"), "缺后果说明：{msg}");
        assert!(
            msg.contains("dev-dependency 环"),
            "缺成因（为什么 lib 单测不能用它）：{msg}"
        );
    }

    /// 规则 2 的失败信息正文。
    fn ts_failure_message(n: usize, list: &str) -> String {
        let mut lines = vec![
            format!(
                "以下 {n} 处让 `src/**`（即 lib 单测 binary）引用了 `{NEEDLE_TS_CRATE}` 的\
                 非白名单入口："
            ),
            list.to_string(),
            String::new(),
            "为什么违规 —— `test-support` 自己持有 generator（`pool_snowflake` /"
                .to_string(),
            "`shared_test_snowflake` → `state.rs` 的 `test_state*` → `AppState.snowflake`）。而它是"
                .to_string(),
            "`[dev-dependencies]` 且 path-depends 回主 crate，构成 **dev-dependency 环**：编译"
                .to_string(),
            "`cargo test --lib` 时同一个二进制里链进**两份** `hsh_erp_rust`。一旦 lib 单测碰了"
                .to_string(),
            "它的 generator 入口，同一进程里就有**两个各自独立派生 instance 的 generator** ——"
                .to_string(),
            "instance 不保证不同 ⇒ 同 instance + 同毫秒 + 同 seq ⇒ **逐字节相同的 id** ⇒"
                .to_string(),
            "`23505 duplicate key ... t_*_pkey`。注意这条**不是**「就地 `new`」，规则 1 抓不到，"
                .to_string(),
            "所以单独立一条规则。".to_string(),
            String::new(),
            "允许的入口只有两个（它们都不构造 / 不取用 generator），见 `TS_ALLOWED_ENTRIES`："
                .to_string(),
        ];
        for entry in TS_ALLOWED_ENTRIES {
            lines.push(format!("  * `{NEEDLE_TS_CRATE}{entry}`"));
        }
        lines.push(
            "  * 需要 generator 时 → `crate::shared::test_snowflake::shared_test_snowflake()`"
                .to_string(),
        );
        lines.push(
            "    （lib 单测进程内唯一的那个对象；⚠️ 不能用 test-support 的同名函数 —— \
             dev-dependency 环让该二进制里链进两份 `hsh_erp_rust`，两个 \
             `SnowflakeIdGenerator` 是**不同类型**，传参即 E0308）"
                .to_string(),
        );
        lines.push(String::new());
        lines.push(format!(
            "白名单（`src/shared/snowflake_guard.rs::TS_SANCTIONED`，共 {} 项）：",
            TS_SANCTIONED.len()
        ));
        for (path, why) in TS_SANCTIONED {
            lines.push(format!("  * `{path}` —— {why}"));
        }
        lines.push(
            "⚠️ 新增允许入口前**必须先回答**：这个 test-support 入口会不会构造或取用一个 \
             generator？会 → 不得进 `TS_ALLOWED_ENTRIES`。"
                .to_string(),
        );
        lines.join("\n")
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
