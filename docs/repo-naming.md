# repo 方法命名规约（Repo Method Naming Conventions）

> 📌 本规约适用于所有 `src/modules/*/repo/` 中的 SQL 真源 + trait 方法。架构层契约（事务边界 / 错误信封 / 胖 trait 范式）见 [`../CLAUDE.md`](../CLAUDE.md) §事务与 repo 范式；本文件只负责**方法名怎么起**。

**读者**：所有向 `src/modules/*/repo/` 提交代码的人（含 Claude Code / 其他 AI 助手）。

**目标**：repo trait 方法命名**自描述**——只看方法名就知道「返回什么、操作哪张表、按什么条件、是否带关联」，无需打开 SQL 就能判断能不能复用、要不要传 ID、要不要带 `with_deleted`。

**正典落地**：`iam` 域 `IamRepo`（17 方法）于 2026-09-22 全面按本规约重命名，作为本规约的完整对照参考。

---

## §1 命名表（场景驱动）

按「业务场景」决定前缀与命名公式——这是写新方法时的第一查表。

| 场景 | 前缀 | 命名公式 | 示例 |
|---|---|---|---|
| JOIN 过滤，返回主实体 | `get_` / `list_` | `动词_主实体_by_条件` | `list_roles_by_user_id` |
| JOIN 拼 DTO，单条 | `get_` | `get_主实体_with_关联` | `get_user_with_roles` |
| JOIN 拼 DTO，列表 | `list_` | `list_主实体_with_关联` | `list_orders_with_items` |
| 复杂视图 / 报表 | `query_` | `query_视图名` | `query_dashboard_summary` |
| 计数 | `count_` | `count_实体_by_条件` | `count_users_by_role` |
| 分组统计 | `count_` / `stat_` | `stat_实体_by_维度` | `count_users_by_role` |
| 求和 / 聚合 | `sum_` / `aggregate_` | `aggregate_实体_stats` | `aggregate_order_stats` |
| 关系存在性 | `exists_` / `has_` | `has_关系(参数)` | `has_role(user, role)` |
| 多表写入（业务） | 业务动词 | `assign/revoke_对象_to/from_对象` | `assign_roles_to_user` |
| 级联操作 | 业务动词 | `动词_主对象_cascade` | `delete_user_cascade` |

---

## §2 命名表（动词 → 返回类型）

按「返回类型」决定动词——这是看旧方法判断要不要重命名时的对照表。

| 场景 | 推荐动词 | 返回类型 |
|---|---|---|
| 按主键查单条 | `get_xxx_by_id` / `find_xxx_by_id` | `Result<Option<T>>` |
| 按条件查单条 | `get_xxx_by_yyy` | `Result<Option<T>>` |
| 查列表（无分页） | `list_xxx` | `Result<Vec<T>>` |
| 查列表（带分页 / 过滤） | `query_xxx` / `search_xxx` | `Result<Page<T>>` |
| 插入 | `create_xxx` / `insert_xxx` | `Result<T>` 或 `Result<InsertResult>` |
| 更新 | `update_xxx` | `Result<T>` 或 `Result<u64>` |
| 删除（物理） | `delete_xxx_by_id` | `Result<u64>` |
| 删除（软删） | `soft_delete_xxx` / `archive_xxx` | `Result<()>` |
| 判存在 | `exists_xxx_by_yyy` | `Result<bool>` |
| 计数 | `count_xxx` | `Result<i64>` |
| UPSERT | `upsert_xxx` / `save_xxx` | `Result<T>` |

---

## §3 三个硬约束

### §3.1 **永远带实体名**

`t_user` 表上的方法不能叫 `get_by_id`——必须叫 `get_user_by_id`。理由：

1. trait 是胖 trait（合并本域所有实体方法，见 [`../CLAUDE.md`](../CLAUDE.md) §事务与 repo 范式），跨实体方法共享同一命名空间
2. service 拿到的 `repo: R` 形参**不带类型提示**（泛型擦除），方法名是唯一识别维度
3. 测试 / doc comment 中看到 `repo.list_by_user(u.id)` 无法判断是返回 User 还是 UserRole

### §3.2 **专用名不背通用名前缀**

通用名（会和别的表方法撞车的）→ 加 `entity_` 前缀；专用名（已经自我消歧的）→ 不加前缀。判定规则：

| 命名 | 类型 | 例子 |
|---|---|---|
| `get_by_id` / `create` / `update` / `soft_delete` | 通用（4 表都会写） | ❌ 必须改：→ `get_user_by_id` / `create_user` / ... |
| `list_by_user_id` / `get_user_with_roles` / `has_role` | 专用（限定了表 / 关联 / 关系） | ✅ 不要再加 `entity_` 前缀 |

**反例**（早期 iam 域曾出现）：`role_list_by_user` —— `_by_user` 已限定了查询条件，再加 `role_` 反而把同 section 的专用名 `exists_same_scope`（也是限定了关系比较）显得特殊。新版按本规约统一为 `list_user_roles_by_user_id`（动词_主实体_by_条件）+ `has_user_role_with_scope`（has_关系）。

### §3.3 **service 调用点的 `.method_name(` 也要同步**

trait 方法重命名时，**所有 service 内的调用方**必须同步更新。grep 模板：

```bash
rg "\.(old_method_name)\(" src/modules/<域>/service/ src/modules/_e2e/handler.rs
rg "<short_module_name>::(old_method_name)\(" tests/<域>_repo.rs tests/_e2e_*.rs
```

**顺序一定 `.soft_delete(` 先于 `.role_soft_delete(`**——避免新名 `soft_delete_user` 被旧名 `soft_delete` 二次匹配腐蚀成 `soft_delete_user_user`。同理 `create(` 先于 `role_create(`。

---

## §4 iam 域落地对照（2026-09-22 重命名）

按本规约 17 个 trait 方法 + SQL free fn 全部重命名，作为本规约的完整正典：

| 表 | 旧名 | 新名 | 依据 |
|---|---|---|---|
| t_user | `get_by_id` | `get_user_by_id` | §2 按主键查单条 |
| t_user | `get_by_username` | `get_user_by_username` | §2 按条件查单条 |
| t_user | `list_with_filters` | `list_users_with_filters` | §2 列表（带过滤） |
| t_user | `count_with_filters` | `count_users_with_filters` | §2 计数 |
| t_user | `create` | `create_user` | §2 插入 |
| t_user | `update_partial` | `update_user_partial` | §2 更新 |
| t_user | `soft_delete` | `soft_delete_user` | §2 删除（软删） |
| t_user | `touch_login` | `touch_user_last_login_at` | §1 业务动词 |
| t_user | `increment_refresh_token_version` | `increment_user_refresh_token_version` | §1 业务动词 |
| t_user | `update_password_and_rotate` | `update_user_password_and_rotate` | §1 业务动词 |
| t_user_role | `list_by_user` | `list_user_roles_by_user_id` | §1 JOIN 过滤 |
| t_user_role | `role_get_by_id` | `get_user_role_by_id` | §2 按主键查单条 |
| t_user_role | `exists_same_scope` | `has_user_role_with_scope` | §1 关系存在性 |
| t_user_role | `role_create` | `create_user_role` | §2 插入 |
| t_user_role | `role_soft_delete` | `soft_delete_user_role` | §2 删除（软删） |
| t_menu | `list_active_for_roles` | `list_active_menus_by_roles` | §1 JOIN 过滤 |
| t_shelf | `shelf_get_by_id` | `get_shelf_by_id` | §2 按主键查单条 |

**关键 takeaway**：

- t_user 表上原本 8 个方法全部 1:1 加 `_user_` / `_user` 中缀，**无任何特例**
- t_user_role 表上原本 3 个 `role_` 前缀方法 → 按 §2 表去掉 `role_`、改成完整 `verb_entity` 形态
- t_shelf 表上原本 `shelf_get_by_id` → 改成 `get_shelf_by_id`，**前缀名从「表名」变成「中缀名」**——这是统一形式的关键

---

## §5 反模式（Code review 必查）

```bash
# 1. 任何 trait 方法名不带实体名（违反 §3.1）
rg "fn (get|list|count|create|update|soft_delete|has|exists)\b" src/modules/*/repo/mod.rs

# 2. 通用名残留（违反 §3.2）
rg "fn (get_by_id|create|update|soft_delete|delete)\(" src/modules/*/repo/

# 3. service 用了旧名（重命名残留）
rg "\.(get_by_id|get_by_username|list_with_filters|count_with_filters|update_partial|soft_delete|touch_login|increment_refresh_token_version|update_password_and_rotate|list_by_user|exists_same_scope|list_active_for_roles|shelf_get_by_id)\(" src/modules/*/service/
```

---

## §6 一句话速记

| 维度 | 硬约束 |
|---|---|
| 实体名 | 必须出现在方法名里（§3.1） |
| 前缀 | 通用名加 `entity_` 前缀，专用名不加（§3.2） |
| 同步 | trait 重命名必同步 service / test / doc comment 调用点（§3.3） |
| 模板 | grep 三件套进 PR description（§5） |