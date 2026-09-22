# vo/ 子目录模板（PR1 沉淀自 iam，PR4 全域推广）

> 2026-09-22 PR1 文档。本模板冻结自 `src/modules/iam/vo/`，是 PR4 全 14 域 vo/ 推广的复制源。
> 改动此文件需在 PR 描述里声明（不向后兼容改动 PR4 已复制域）。

## 定位与边界

iam 域在 2026-09-22 完成 `dto/` + `vo/` 拆分后，**DTO 与 VO 各自独占子目录**：

| 子目录 | 角色 | serde 行为 | axum 出现位置 |
|---|---|---|---|
| `dto/` | HTTP 请求入参（axum extractor 反序列化目标） | 仅 `Deserialize` | `Json<T>`, `Query<T>`, `Path<T>` |
| `vo/` | HTTP 响应出参（统一信封 `R<T>` 序列化目标） | 仅 `Serialize` | `Json(R::ok(...))` |

**为什么需要 vo/ 子目录**（而非继续沿用 `model.rs` / `dto.rs` 一锅端）：

1. **DTO/VO 边界硬约束**：DTO 出现 `Json<T>` 反序列化、VO 出现 `R::ok(...)` 序列化是必然，但**禁止**同一 struct 同时 derive `Serialize + Deserialize`——前者控制字段别名/缺省/三态，后者控制校验/默认值/null 语义，一旦双向 derive，DTO/VO 演化相互耦合（增删字段、修改类型都得双向兼容）。
2. **domain enum 不污染出参**：表行 `model.rs` 保留 `bigint` 雪花 ID、`NaiveDateTime`、DB 内部 enum；vo 必须把 `i64` 雪花 ID 标 `#[serde(serialize_with = "crate::shared::types::serialize_i64")]` 转字符串防 JS 精度截断。这层转换属于序列化边界，不该被 `model.rs` 共享。
3. **可读性**：vo 仅含 5-7 个 `*Out` 类型（按端点语义拆文件），grep "UserOut" 一秒定位；混在 `dto.rs` 里既不像 DTO 也不像 VO 的中间态类型会越积越多。

## vo/mod.rs 骨架

```rust
//! <域>域响应 VO（HTTP 返回值隔离层）
//!
//! 仅含 handler 返回的 output 类型；入参类型见 `super::dto`。
//! 按端点语义拆为 `<feature>.rs`（每个 feature 一个文件，含 1-3 个相关 *Out）。
//!
//! ## 与 `super::dto` 的边界
//! VO **禁止** 出现在 axum extractor 反序列化侧——`serde::Deserialize` 不实现；
//! 只用于 service 组装 + handler `Json(R::ok(...))` 返回值序列化。

pub mod <feature>;
// pub mod <feature_2>;

pub use <feature>::{<OutA>, <OutB>};
// pub use <feature_2>::<OutC>;
```

### vo/<feature>.rs 骨架

```rust
//! <域>域 <feature> 端点响应 VO

use chrono::NaiveDateTime;
use serde::Serialize;

/// 列表出参（含分页元信息）
///
/// 若对齐 `shared::response::Page<T>`（仅 total+items），直接用 `Page<T>`；
/// 需保留 limit/offset 给前端翻页回显时（如 iam `UserListOut`），自定义 4 字段。
#[derive(Debug, Clone, Serialize)]
pub struct <List>Out {
    pub items: Vec<<Entity>Out>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

/// 单条实体出参（详情 / 创建 / 更新 / 删除响应）
#[derive(Debug, Clone, Serialize)]
pub struct <Entity>Out {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub id: i64,
    pub version: i32,
    // ... 业务字段（String / bool / Option<...> / NaiveDateTime）
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub created_by: Option<i64>,
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub updated_by: Option<i64>,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

/// 关联子实体出参（如 iam `UserRoleOut`）
#[derive(Debug, Clone, Serialize)]
pub struct <Relation>Out {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub id: i64,
    pub version: i32,
    // ... 关联字段
    /// 雪花 ID 在 VO 中以字符串形式承载；None → JSON `null`。
    /// 不要写 `Option<i64>` + `serialize_i64_opt` —— 必须先 `to_string()`。
    pub scope_id: Option<String>,
}
```

### 完整样例：iam `UserOut`（参考 `src/modules/iam/vo/account.rs`）

```rust
/// 用户角色出参。`shelf_code` / `shelf_name` 仅 SHELF_ACCOUNT 角色非空。
#[derive(Debug, Clone, Serialize)]
pub struct UserRoleOut {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub id: i64,
    pub version: i32,
    pub role: String,
    pub scope_type: Option<String>,
    pub scope_id: Option<String>,
    pub shelf_code: Option<String>,
    pub shelf_name: Option<String>,
}

/// 用户详情出参（含角色列表）
#[derive(Debug, Clone, Serialize)]
pub struct UserOut {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub id: i64,
    pub version: i32,
    pub username: String,
    pub full_name: String,
    pub phone: Option<String>,
    pub is_active: bool,
    pub last_login_at: Option<NaiveDateTime>,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
    pub roles: Vec<UserRoleOut>,
}

/// 用户列表出参。
///
/// 字段与顺序对齐 Python `schema/user.py::UserListOut`——即 `items, total, limit, offset`
/// 四个字段。前端会回显 `limit`/`offset` 做翻页，故不裁剪为 `{total, items}`；
/// 也因此不直接复用 `shared::response::Page<T>`（那是只有 total+items 的通用结构）。
#[derive(Debug, Clone, Serialize)]
pub struct UserListOut {
    pub items: Vec<UserOut>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}
```

## 字段 `serialize_i64` 应用清单

雪花 ID 在 Rust 端是 `i64`，在 JSON 端是字符串（`"123456789012345678"`）。
统一 helper `crate::shared::types::serialize_i64` / `serialize_i64_opt`，
**禁止**自己写 `v.to_string()`。

| 字段语义 | 必须标 | 说明 |
|---|---|---|
| 主键 `id` | 是 | 雪花 ID；JSON 字符串 |
| 外键 `*_id`（如 `user_id`、`parent_id`、`role_id`、`scope_id`、`shelf_id`） | 是 | 同上；**注意**：vo 层 `*_id: Option<String>`，**不是** `Option<i64>`——雪花 ID 一律以字符串承载（参考 `iam/vo/menu.rs::parent_id`） |
| 审计 `created_by` / `updated_by` | 是 | `Option<i64>` 走 `serialize_i64`（None → JSON `null`） |
| 乐观锁 `version` | 否 | `i32` 直接走默认序列化（数值小、不会溢出 JS） |
| `count` / `total` / `limit` / `offset` / `page` | 否 | 同上，纯分页计数 |
| `amount` / `quantity` / `price` / `weight` | 否 | 业务数值；如量级 ≤ 2^53 保持数值（如 `f64` 公斤重），超长 ID 才走字符串 |
| 业务日期 / 枚举字符串 / 业务码 | 否 | 全部按默认 serde 行为（`NaiveDateTime` 走 ISO8601） |

> **判定口诀**：i64 + 语义是"ID/审计人" → 必标；其他 i64 → 不标。

## handler 替换模式 4 种

handler 在写端点统一 `Json(R::ok(...))` 之前需要把 service 返回值装入 vo。
按 service 返回类型分 4 种模式：

### 模式 1：service 返回 `Out`，handler 末尾 `to_vo` 装换（iam 本域的范式）

适用：service 返回的是 `repo::model::*`（表行）或内部聚合 struct（不是 vo 类型），
handler 在 commit 后做 `to_vo(...)` 装换。

```rust
/// POST /api/v2/iam/users → 201 —— 纯写端点
pub async fn create_user(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<UserCreateRequest>,
) -> Result<(StatusCode, Json<R<UserOut>>), AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .account_service
        .create_user(&mut *tx, &req, &current)
        .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(R::ok(out))))
}
```

其中 `account_service.create_user` 内部直接返回 `UserOut`（已组装好的 vo），
handler 不必再 `to_vo(...)`——这是 iam 范本（service 末尾组装 vo）。
PR4 推广时**首选模式 1**，避免 vo 装换散落在 handler。

### 模式 2：service 直接返回 `Vo`（handler 零装换）

适用：service 内部已经把 `repo::model::*` 抽成 vo（如 iam `UserOut`、`UserRoleOut`），
handler 仅 `R::ok(out)`。

```rust
/// GET /api/v2/iam/users/{id} —— 读端点，acquire 不开事务
pub async fn get_user(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<UserOut>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = state
        .account_service
        .get_user(&mut *conn, id, &current)
        .await?;
    Ok(Json(R::ok(out)))
}
```

### 模式 3：list 装换 `Vec<Out> → Vec<Vo>`

适用：service 返回 `Vec<repo::model::Row>`（列表），vo 类型需要 `serialize_i64` 或字段裁剪，
handler 末尾 `let vo: Vec<Vo> = rows.into_iter().map(to_vo).collect();`。

```rust
pub async fn list_entities(
    State(state): State<Arc<AppState>>,
) -> Result<Json<R<Vec<EntityOut>>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let rows = state.service.list_entities(&mut *conn).await?;
    let vos: Vec<EntityOut> = rows.into_iter().map(to_entity_out).collect();
    Ok(Json(R::ok(vos)))
}

fn to_entity_out(r: EntityRow) -> EntityOut {
    EntityOut {
        id: r.id,
        version: r.version,
        // ... 业务字段映射
    }
}
```

### 模式 4：`Page<T>` 嵌套（`Page<R<T>>`）

适用：分页列表需要 `total + items`（无需 `limit/offset` 字段回显），
复用 `shared::response::Page<T>`。

```rust
pub async fn list_entities_paged(
    State(state): State<Arc<AppState>>,
    Query(q): Query<EntityListQuery>,
) -> Result<Json<R<Page<EntityOut>>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let (total, rows) = state.service.list_entities_paged(&mut *conn, &q).await?;
    let items: Vec<EntityOut> = rows.into_iter().map(to_entity_out).collect();
    Ok(Json(R::ok(Page::new(total, items))))
}
```

> 注意：iam `UserListOut` 因前端要回显 `limit/offset`，未复用 `Page<T>`——这是**显式偏离**，
> 已在 `iam/vo/account.rs::UserListOut` 注释中说明。PR4 推广时遇到类似情况按需自定义。

## DTO/VO 边界硬约束

```text
【硬约束 - 不可违反】

1. DTO（dto/）仅 derive Deserialize，禁止加 Serialize
2. VO（vo/）仅 derive Serialize，禁止加 Deserialize
3. 禁止同一 struct 同时 derive Serialize + Deserialize（双向耦合破坏演化）
4. VO 不出现在 axum extractor（Json<T>/Query<T>/Path<T>）反序列化侧
5. DTO 不出现在 Json(R::ok(...)) 序列化侧
```

验证方法：

- `cargo clippy` 配合 `cargo udeps` 双重检查（VO 不该有 `deserialize_*` 函数被引用）
- `grep -rE "Deserialize" src/modules/<域>/vo/` → 应只输出无关项（注释中提及"禁止 Deserialize"）
- `grep -rE "Serialize" src/modules/<域>/dto/` → 应只输出无关项

## PR4 推广检查清单（复制本模板时逐项过）

- [ ] 复制 `vo/mod.rs` 骨架，按端点语义拆 `<feature>.rs`
- [ ] 每个 `<feature>.rs` 至少 1 个 `<Entity>Out`，i64 ID/审计人字段标 `serialize_i64`
- [ ] `grep "deserialize" src/modules/<域>/vo/` 仅匹配注释
- [ ] handler 按 4 种模式替换（**首选模式 2：service 直返 vo**，避免 handler 散落 `to_*`）
- [ ] `cargo clippy --all-targets -- -D warnings` 通过
- [ ] `cargo test` 端到端覆盖（route 200 + VO JSON 序列化 OK + i64 雪花 ID 是字符串）

## 不允许的写法

```rust
// ❌ 双向 derive（破坏演化）
#[derive(Debug, Serialize, Deserialize)]
pub struct UserOut { /* ... */ }

// ❌ vo 出现在 extractor
pub async fn handler(Json(req): Json<UserOut>) -> ... { }

// ❌ dto 出现在 R::ok
pub async fn handler() -> Result<Json<R<UserCreateRequest>>, AppError> { }

// ❌ vo 字段类型写 Option<i64> + serialize_i64_opt
pub struct UserRoleOut {
    #[serde(serialize_with = "crate::shared::types::serialize_i64_opt")]
    pub scope_id: Option<i64>,  // 应是 Option<String>
}
```