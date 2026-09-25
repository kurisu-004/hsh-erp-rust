# seeds/ —— 声明式种子数据

> 2026-09-25 sqlx 接管后新增：菜单等"配置数据"从 migration 抽离到本目录，
> 由 `src/infra/seed.rs` 启动钩子自动应用。

## 当前内容

- `menu.sql` —— 菜单树（t_menu + t_role_menu）声明式种子
- `admin.sql` —— 可选初始管理员账号种子（2026-09-26 新增），由 `BOOTSTRAP_ADMIN_ENABLED` 门控

### 初始管理员 seed（可选）

> **安全警示**：默认关闭。生产绝不允许开启；首次启用后必须立刻改密 + 设回 `false` + 重启。

**文件**：`seeds/admin.sql`
**门控**：环境变量 `BOOTSTRAP_ADMIN_ENABLED`（默认 `false`）
**语义**：开启后，启动钩子会在 `seeds/menu.sql` 之后插入一条 `username=admin / password=changeme / role=MANAGER` 的用户及其 MANAGER 角色行（id 静态常量 `900000000000000001` / `900000000000000002`）。

**首次启用流程**（仅适用于从零建库后的首次启动）：

```bash
# 1. .env 临时开启
BOOTSTRAP_ADMIN_ENABLED=true

# 2. 启服务
cargo run  # 或 docker compose up -d rust-backend

# 3. 登录（http://localhost:8080 或 /api/v2/iam/login）
curl -X POST http://localhost:3000/api/v2/iam/login \
  -H 'Content-Type: application/json' \
  -d '{"username":"admin","password":"changeme"}'

# 4. 拿到 token 后立即改密（POST /api/v2/iam/change-password）

# 5. .env 设回 false，重启
BOOTSTRAP_ADMIN_ENABLED=false
cargo run
```

**幂等性**：

- `t_user.username` 是 partial unique（`WHERE deleted_at IS NULL`），`ON CONFLICT (username) WHERE deleted_at IS NULL DO NOTHING` —— 已存在用户保留不被改密
- `t_user_role`（`user_id, role, scope_type, scope_id` 唯一约束）走 `ON CONFLICT DO NOTHING`（不带 target，PG 推断）
- 重新跑 seed **不会**覆盖手工修改的 `password_hash`：场景 3 测试断言

**密码字面值来源**：

- bcrypt 哈希字面值复用 `test-support/fixtures/iam.sql:45-49` 同款（明文 `changeme`、cost=12），物理同源便于维护
- 明文 `changeme` 也对应 `src/modules/iam/service/account.rs::DEFAULT_RESET_PASSWORD`（管理员重置密码默认口令）

## 设计动机

旧模式：菜单变更 = 写新 migration 文件 → 算 SHA384 → 同步 `_sqlx_migrations.checksum` →
部署。每次都要担心：

- 硬编码 ID 撞号（migration 018 / 023 / 024 三个文件共用 `tmp_menu_migration_seq START 100000000`
  → 后跑文件 INSERT 拿到 100000000~10000000X，与 018 已写入的菜单 id 撞 `t_menu_pkey`）
- ON CONFLICT 守卫漏写导致重跑挂掉
- 修改已应用 migration 触发 `VersionMismatch` panic

新模式：菜单变更 = 改 `seeds/menu.sql` → 重启 app（或手工 `psql -f seeds/menu.sql`）。
零心智负担。

## 编写约定

- **INSERT ... ON CONFLICT (code) WHERE deleted_at IS NULL DO UPDATE**：
  按 `code` 幂等；多次跑结果一致
- **parent_id 通过子查询按 code 反查**，不写死雪花 ID（环境间 ID 可不同）
- **t_menu.id 用静态 ID**：`9000000000xxx` 段（15 位）。
  - 已存在的雪花 ID 行 `ON CONFLICT` 后保留原 ID，只更新其他字段
  - 与生产雪花 ID（~2×10¹⁷）和 fixture ID（`9_xxx_xxx_xxx_xxx_xxx_xxx`，18 位）物理不相交
- **t_role_menu.id 同样用静态 ID**：`9000001000xxx` 段
- **不自动删"未在文件声明的菜单"**：菜单下线必须显式列在第 3 节 soft-delete 区段
  （防止误删生产手工加的菜单）
- **`include_str!` 编译期嵌入**：`src/infra/seed.rs` 把整个 SQL 文件嵌进二进制，
  启动时直接 `sqlx::raw_sql` 执行，不走 IO

## 应用时机

启动时自动跑（默认开启）：

```rust
// src/main.rs:60
sqlx::migrate!("./migrations").run(&pool).await?;
seed::run_seeds(&pool).await?;  // 这里
```

手工触发入口：`scripts/seed_apply.sh`

```bash
psql $DATABASE_URL -v ON_ERROR_STOP=1 -f seeds/menu.sql
```

## 验证清单（新增/修改 seed 后必跑）

```bash
# 1. 编译期检查
SQLX_OFFLINE=true cargo check --all-targets

# 2. 启动跑一遍（手动起本地 dev DB）
cargo run  # 应无 error

# 3. 验证菜单树结构
psql $DATABASE_URL -c "SELECT count(*) FROM t_menu WHERE deleted_at IS NULL"
psql $DATABASE_URL -c "SELECT count(*) FROM t_role_menu WHERE deleted_at IS NULL"
psql $DATABASE_URL -c "SELECT code, sort_order FROM t_menu WHERE parent_id IS NULL AND deleted_at IS NULL ORDER BY sort_order"
```

## 不重跑 `sqlx_prepare`

seeds 用 `sqlx::raw_sql`，**不走** `query!` 编译期校验宏，因此改动 `seeds/menu.sql`
不需要重跑 `./scripts/sqlx_prepare.sh`。