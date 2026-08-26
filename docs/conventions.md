# 开发规约（Engineering Conventions）

> 📌 本文件是 Rust 重构版 ERP 的工程实践硬约定。架构层的契约（事务、错误信封、JWT/RBAC、雪花 ID、WS、路由）见 [`CLAUDE.md`](../CLAUDE.md)；系统设计背景见 [`docs/architecture.md`](architecture.md)。本文件只约束**写代码时的纪律**：文件怎么切、SQL 怎么查、测试怎么写、注释怎么写。

**读者**：所有向 `src/` 提交代码的人（含 Claude Code / 其他 AI 助手）。**目标**：让任何新人只看注释与本规约，就能把每个 `pub` 项在业务里的角色、对接的 Python 文件、隐藏的坑搞清楚。

---

## §1 单文件职责（一个文件只做一件事）

**规则**：一个 `.rs` 文件只承载**一个内聚职责**。当一个文件同时混入以下 ≥3 类内容时，必须拆分：

| 类型 | 例子 |
|---|---|
| 读路径 | `repo/query.rs` 里的 SELECT/JOIN |
| 写路径 | `repo/mutate.rs` 里的 INSERT/UPDATE/DELETE |
| 业务编排 | `service/inner.rs` 里跨 repo 协调 |
| 子领域逻辑 | `service/scan.rs`、`service/print.rs`、`service/group.rs` |
| DTO / 模型转换 | `dto.rs`、`model.rs` |
| 工具函数 | `utils.rs`、`format.rs` |

**理由**：避免单文件超大（参 §2），让 PR diff 局部化（reviewer 一次只看一类变更），便于按职责 grep（"扫所有写路径" → `rg "INSERT|UPDATE" src/modules/*/repo/mutate.rs`）。

**正典**：`src/modules/delivery_note/` 已按职责切到极致：

```
delivery_note/
├── handler.rs                       (716)   HTTP 入口
├── dto.rs                                     请求/响应 DTO
├── model.rs                                   表行 + 域枚举
├── statemachine.rs                            内存 enum 状态机
├── print.rs / print_xml_patch.rs              打印模板 / XML 补丁
├── repo/
│   ├── mod.rs           ( 25)      re-export
│   ├── query.rs         (484)      所有 SELECT
│   └── mutate.rs        (281)      所有 INSERT/UPDATE/DELETE
└── service/
    ├── mod.rs           ( 23)      re-export
    ├── crud.rs          (447)      基础 CRUD
    ├── inner.rs         (632)      跨 repo 编排
    ├── lifecycle.rs     (544)      状态机驱动
    ├── group.rs         (305)      分组业务
    ├── scan.rs          (807)      扫码入单
    └── print.rs         ( 94)      打印协调
```

**反例**：`src/modules/part/service.rs`（1168 行）目前还把 CRUD、生命周期、关联关系全塞在一个文件——按 §2 是必拆项；目标拆为 `service/{crud, lifecycle, relation}.rs`。

---

## §2 文件行数上限（≤ 1000 行硬红线）

**规则**：单个 `.rs` 文件 ≤ **1000 行**（不含空行，不含 `#[cfg(test)] mod tests` 块）。一旦 ≥ 800 行，**必须**在 PR 描述里写明"已评估拆分，结论：xxx"。

**理由**：

- 人眼单屏阅读极限（IDE 默认 ~50 行/屏 ≈ 20 屏）
- 编译器增量构建友好（小文件改动后 cargo 只重编一个 crate 单元）
- code review 局部化（避免一个 PR 触碰半屏代码）

**当前已超线 / 接近超线的文件**（必拆清单，按本次规约执行）：

| 文件 | 行数 | 建议拆法 |
|---|---:|---|
| `src/modules/part/service.rs` | **1168** | `service/{crud, lifecycle, relation}.rs` |
| `src/modules/delivery_note/print.rs` | **1123** | `print/{template, xml_patch, barcode}.rs` |
| `src/shared/error.rs` | 924 | 当逼近 1000 时按 HTTP / 业务码 / Python 兼容拆 |
| `src/modules/part/repo.rs` | 951 | 同上，按 query / mutate 拆 |

**常见切分维度**（按本仓库已出现的 pattern 排序）：

1. **读 vs 写** → `repo/query.rs` + `repo/mutate.rs`
2. **业务子域** → `service/scan.rs` + `service/print.rs` + `service/group.rs`
3. **模板 / 补丁 / 校验** → `print/template.rs` + `print/xml_patch.rs` + `print/validate.rs`
4. **HTTP 语义类** → `error/http.rs` + `error/biz.rs` + `error/python_compat.rs`

**禁止**：

- ❌ 用 `mod foo { ... }` 内嵌超大模块来逃避文件行数（本质还是同一文件）
- ❌ 把 `#[cfg(test)] mod tests` 算进 1000 行上限（测试应该充分写）

---

## §3 SQL 严禁发生 N+1

**规则**：

1. ❌ 禁止在循环里逐行 SELECT：
   ```rust
   for id in part_ids {
       let p = sqlx::query_as!(TPart, "SELECT * FROM t_part WHERE id = $1", id)
           .fetch_one(&mut *tx).await?;   // 每条 ID 一次 DB 往返
       results.push(p);
   }
   ```
2. ✅ 多 ID 批量取数必须走 `XxxRepo::list_by_ids(executor, ids: &[i64], include_deleted: bool) -> Vec<T>`（用 `WHERE id = ANY($1)`）：
   ```rust
   let parts = PartRepo::list_by_ids(&mut *tx, &part_ids, false).await?;
   ```
3. ✅ 列表 / 详情页聚合查询用 `JOIN` 一次性带出所有需要的表，不要先查主表再循环查关联表。
4. ✅ 单元素 vs 多元素 placeholder 自适应（参考 `push_status_filter` 的三态写法）。

**理由**：N+1 在 PG 上表现为 RT P99 突发飙升 + 连接池耗尽 + 锁竞争加剧。一次 `WHERE id = ANY($1)` 比 N 次单查快 10-100 倍（取决于 RTT）。

**正典**：

- `src/modules/part/repo.rs:55-80` —— `PartRepo::list_by_ids`：空切片短路返回 `Vec::new()`，多 ID 走 `ANY($1)`。
  ```rust
  pub async fn list_by_ids<'e, E: PgExecutor<'e>>(
      executor: E,
      ids: &[i64],
      include_deleted: bool,
  ) -> Result<Vec<TPart>, sqlx::Error> {
      if ids.is_empty() {
          return Ok(Vec::new());
      }
      sqlx::query_as!(TPart, "SELECT ... FROM t_part WHERE id = ANY($1) AND ...", ids, include_deleted)
          .fetch_all(executor).await
  }
  ```
- `src/modules/delivery_note/repo/query.rs:468-485` —— `push_status_filter` 注释堪称"placeholder 处理教科书"：
  ```rust
  /// 向 QueryBuilder 追加 `statuses` 过滤子句。
  ///
  /// - 空切片：什么都不追加
  /// - 单元素：`AND status = $N`，绑定该值
  /// - 多元素：`AND status = ANY($N::text[])`，绑定 Vec<String>
  pub(super) fn push_status_filter(qb: &mut QueryBuilder<Postgres>, statuses: &[&str]) { ... }
  ```
- `src/modules/part_batch/repo.rs:9` 注释 —— 已落地：
  ```
  //! - `list_with_part_by_delivery_note` —— 同上，JOIN t_part 一次拿齐（防 N+1）
  ```

**repo 函数签名约定**：接收 `impl PgExecutor<'_>`，可同时接受 `&PgPool` / `&mut PgConnection` / `&mut Transaction`。**例外**：方法需在同一事务内连发多条 SQL 时收 `&mut PgConnection`（因 `PgExecutor` 不能 move 多次），并在该函数 `///` 注释里说明，见 `src/modules/part/repo.rs:20-22`。

**Code review 检查项**（每个 PR 都要过）：

- `rg "fetch_one|fetch_optional" src/modules/*/service/*.rs` —— 出现循环里调用 = 红色警报
- `rg "for .* in .* \{$" -A 5 src/modules/*/service/*.rs | rg "fetch_"` —— 邻接 fetch_one 的 for 循环 = 必查

---

## §4 单元测试覆盖

### §4.1 覆盖率原则

| 代码类别 | 是否强制覆盖 | 怎么覆盖 |
|---|---|---|
| 纯函数（无 IO、无全局可变状态） | ✅ **必须 100% 行覆盖** | 同文件 `#[cfg(test)] mod tests` |
| 含 DB / 网络 / 时钟 / Redis / WS | ❌ 不强制覆盖率 | `tests/<module>_api.rs` 集成测试 |
| 不可注入依赖的复杂分支 | 注释豁免 | 在 `///` 里写"为何无法单测" |

**纯函数**包括（但不限）：

- 状态机迁移判定（`statemachine.rs` 的 `can_transition_to`）
- 错误码映射（`code.rs` 常量 → `AppError` 构造）
- DTO ↔ model 转换（`From<T> for U`、`TryFrom`）
- 字符串 / 数值 / 时间工具函数（雪片 ID 拼接、`format_xsd_date`、`compute_age`）
- 所有 `impl Display` / `impl FromStr`
- 业务规则判定（如"工单数量必须 ≥ 1 且 ≤ 999"）

**含 IO 的代码**（handler / service / repo / 异步任务）**不**强制单测覆盖率，但必须有**集成测试**守护 happy path + 至少一个 error path。

### §4.2 测试放置约定

| 测试类型 | 位置 | 理由 | 正典 |
|---|---|---|---|
| 纯逻辑 inline 测试 | 同文件 `#[cfg(test)] mod tests { ... }` | 便于测私有 fn，零编译开销 | `src/infra/snowflake.rs`（10 个测试）、`src/shared/error.rs`（9 个测试） |
| 涉及 sqlx / handler / Redis / WS | `tests/<module>_api.rs`（黑盒 HTTP） | 隔离编译，最贴近生产路径 | `tests/part_api.rs`、`tests/delivery_note_api.rs`、`tests/auth_api.rs` |
| 共享 fixture（DB 池 / Redis 清理 / AppState 构造） | `tests/common/mod.rs` | 避免每个测试文件重写 | `tests/common/mod.rs` 的 `ensure_database_exists` / `test_pool` / `clean_db` / `clean_redis` |

### §4.3 命名与结构

- 测试函数名用 snake_case，描述**期望行为**而非实现：
  ```rust
  #[test]
  fn next_id_returns_zero_sequence_when_clock_advances() { ... }

  #[test]
  fn list_by_ids_short_circuits_on_empty_input() { ... }
  ```
- 一个 `#[test]` 一个断言目标；多分支用 `rstest` 或多 `#[test]`。
- 不依赖 `tokio::time`、全局 `Instant::now()`、随机数——必要时显式注入时钟或种子（参考 `infra/clock.rs`）。

### §4.4 后续任务（不在本次范围）

待接入：

- `cargo-llvm-cov`（dev-dependency，CI step：`cargo llvm-cov --lcov --output-path lcov.info`）
- `.github/workflows/ci.yml`（check + clippy + test + sqlx_prepare + coverage gate）

---

## §5 函数 / 结构体 / 枚举注释规范

### §5.1 总原则

注释的目标读者是**没读过这个文件的人**——只看注释，**5 分钟内**就应该知道：

1. 这个函数 / 类型在业务里扮演什么角色？
2. 它对接 Python myERP 的哪一块？
3. 有什么坑（panic 条件、副作用、所有权、与 Python 的差异）？

**禁止**：

- ❌ 复读签名：`/// 获取用户` + `pub fn get_user(id: i64) -> User`
- ❌ 无信息量的 `// TODO`（应改为 `// TODO: 2026-Q4 接入 LDAP 时改为查询 LDAP 而不是 mock`
- ❌ 仅描述实现而非意图的注释

### §5.2 文件级 `//!`

**规则**：每个模块根文件首行必须有 `//!` 块，至少包含三要素：

1. **模块名**（一行，概括）
2. **对应 Python myERP 文件路径**（与 `docs/architecture.md` 的映射表对齐）
3. **当前阶段标记**（P0 / P1 / P2 / P3 / P4 / F / F2 之一）

可选附加：分段契约（如错误码）、与 Python 的冲突解决记录、特殊约定（如 repo 签名）。

**正典**：`src/shared/error.rs:1-21`：

```rust
//! 应用错误类型 + 错误码常量段
//! 对应 Python myERP/core/error_code.py + exception*.py
//!
//! ## 错误码分段契约（与 Python 前端保持兼容）
//! - `0`          成功
//! - `4xxxx`      HTTP 语义 ...
//! - `5xxxx`      系统错误 ...
//! - `2xxxx`      业务域错误 ...
//!
//! ### 与 Python 的差异（冲突解决记录）
//! - `20109` 在 Python 中被双重占用；Rust 中保留给 `BIZ_PART_BATCH_NOT_FOUND`，...
//!
//! 业务实现阶段直接 `AppError::biz(code::BIZ_..., "...")`；常量在 `code` 模块内集中维护。
```

### §5.3 `pub fn` 的 `///`

**规则**：三段式（业务目的 → 关键差异 → 副作用 / 所有权）。

**正典**：`src/infra/snowflake.rs:62-67`：

```rust
/// 生成下一个雪花 ID。
///
/// 与 Python `SnowflakeGenerator.__next__` 的差异：本实现在同一毫秒用尽
/// 4096 个 sequence 后会 `sleep(1ms)` 并重试，而 Python 返回 `None`。
/// 这是为了让 Rust 端业务调用方不必处理 `Option`。
pub fn next_id(&self) -> i64 { ... }
```

要点：

- 第一段：一句业务目的（说人话，如"生成下一个雪花 ID"而非"实现雪花算法"）
- 第二段：与 Python / 其它实现的差异（这是新成员最容易踩坑的地方）
- 第三段（可选）：panic 条件、阻塞（`sleep`）、锁（`Mutex`）、不可重入等副作用

### §5.4 `pub struct` 的 `///`

**规则**：

- 类型本身 `///` 一段话概括
- **每个字段都要 `///`**，至少说清业务含义；序列化形态（如 `i64` → string）、单位（ms / s / bytes）、Python 字段名映射（如适用）都要写明

**正典**：`src/auth/rbac.rs:57-77` 的 `Claims`：

```rust
/// access token 业务载荷（与 Python JWT payload 对齐）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    /// Python v1 token 把 `sub` 写成 `str(user_id)`，Rust v2 历史上期望 `i64`；
    /// `deserialize_sub_or_int` 同时接受两种形式（数字 / 数字串）。
    #[serde(deserialize_with = "deserialize_sub_or_int")]
    pub sub: i64,
    pub username: String,
    pub roles: Vec<Role>,
    /// `ShelfAccount` 角色可访问的货架 ID 列表；非该角色为空。
    pub shelf_ids: Vec<i64>,
    ...
}
```

字段注释要点：

- 单位（`timestamp_ms_since_epoch: u64` vs `timestamp: i64`）
- 序列化形态（`pub id: i64` 在 JSON 里是 string，见 `shared/types.rs`）
- Python v1/v2 字段映射（`alias = "type"`、`deserialize_with` 等）
- 业务不变量（如 `quantity >= 0`）

### §5.5 `pub enum` 的 `///`

**规则**：

- 类型本身 `///` 一段话
- **每个变体都要 `///`**，说明：业务含义 + 触发场景 + （若是状态枚举）对应的状态机迁移

**正典**：`src/auth/rbac.rs:43-55`（`Role` 枚举——字段注释 + `#[serde(rename)]` 标明 JSON 形态）：

```rust
/// 五角色 RBAC 角色集合，与 Python `RoleEnum` 一一对应。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum Role {
    /// 经理：全权限。
    #[serde(rename = "MANAGER")] Manager,
    /// 职员：日常业务。
    #[serde(rename = "CLERK")] Clerk,
    /// 检验员：送检 / 入库相关。
    #[serde(rename = "INSPECTOR")] Inspector,
    /// CNC 程序员：程序相关。
    #[serde(rename = "CNC_PROGRAMMER")] CncProgrammer,
    /// 货架账号：受 `shelf_ids` 限制的子集权限。
    #[serde(rename = "SHELF_ACCOUNT")] ShelfAccount,
}
```

**状态枚举**（如 `DeliveryNoteStatus`）每个变体必须标明"从哪个状态来"与"能去哪个状态"，冗余但安全。

### §5.6 私有 helper 函数

**规则**：不需要 `///`，但**算法分支需要 `// 解释为什么这样做`**，特别是与 Python 行为不一致的分支（防止后人"修复"成 Python 行为而引入 regression）。

正典：`src/infra/snowflake.rs:77`：

```rust
// now == last_ms：递增 sequence，并显式 mask 保证 ≤ 0xFFF。
let seq = g.sequence.wrapping_add(1) & MAX_SEQUENCE;
```

### §5.7 Section banner

**规则**：长文件（>500 行）必须用 `// === xxx ===` 或 `//! ## xxx` 分节，便于跳转。

**正典**：`src/modules/part/handler.rs:5-17`：

```rust
//! ## 约定
//! - 事务边界在 handler：...
//! - 统一响应信封：...
//! - 权限在 handler...
//!
//! ## Phase F 路由（挂在 `/parts`）
//! - `POST /batch-pass-inspection`         —— ...
//!
//! 路由顺序敏感：`/batch-pass-inspection` 必须在 `/{part_id}/pass-inspection` 之前
//! 注册，否则 axum 会把 `batch-pass-inspection` 解析成 `part_id`。
```

---

## §6 配套 TODO（不在本次范围，留作后续独立任务）

下列项本次不实施，但本规约生效后即应排期：

- [ ] 拆分 `src/modules/part/service.rs`（1168 → 按 `crud/lifecycle/relation` 拆）
- [ ] 拆分 `src/modules/delivery_note/print.rs`（1123 → 按 `template/xml_patch/barcode` 拆）
- [ ] 拆分 `src/modules/part/repo.rs`（951 → 按 `query/mutate` 拆，与 delivery_note 对齐）
- [ ] 接入 `cargo-llvm-cov` 为 dev-dependency
- [ ] 新增 `.github/workflows/ci.yml`：cargo check / clippy / test / sqlx_prepare / coverage gate
- [ ] 新增 `clippy.toml`：`cognitive_complexity_threshold = 30`、`too_many_arguments_threshold = 8`、`type_complexity_threshold = 250`
- [ ] 新增 `rustfmt.toml`：`max_width = 100`、`imports_granularity = "Crate"`
- [ ] 为 13 个 stub 业务域补 0 行业务代码（按 `docs/architecture.md` §7 路线图）
- [ ] PR 模板（`.github/PULL_REQUEST_TEMPLATE.md`）含"已读 [docs/conventions.md](../docs/conventions.md)"勾选项

---

## §7 一句话速记

| 维度 | 硬约束 |
|---|---|
| 单文件职责 | 只承载一类内聚职责；混 ≥3 类即拆 |
| 文件行数 | ≤ 1000 行；≥800 行 PR 须说明拆分决策 |
| SQL | 禁止 N+1；批量取数用 `WHERE id = ANY($1)` |
| 测试 | 纯函数 100% 行覆盖；含 IO 靠集成测试 |
| 注释 | `pub` 项必写 `///`；新成员只读注释即可上手 |
| 工具链 | 默认 rustfmt/clippy；覆盖率与 CI 后续接入 |