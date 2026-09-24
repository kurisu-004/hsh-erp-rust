# 数据库迁移规范（sqlx migrate）

本目录由 `sqlx::migrate!()` 宏在编译期与运行时扫描，**只认顶层 `*.sql`**，子目录（如 `archive/`）不参与扫描。

## 当前状态（2026-09-25 sqlx 接管后）

- `20260925000000_001_baseline.sql` —— 全量 schema baseline（从原 001-029 合并）
- `archive/` —— 原 29 个 migration 文件归档，仅历史参考，不再演进
- `seeds/` —— 声明式种子数据（菜单等配置数据），与 migrations/ 分开管理

**新 schema 变更必须追加新 migration**（`<13位时间戳>_<顺序>_<描述>.sql`），**不再修改** 已有文件。修改已应用 migration 会触发 `VersionMismatch` panic。

## 命名

`<13 位时间戳>_<顺序>_<简短描述>.sql`，例如 `20261015120001_add_process_chain_template.sql`。

## 表设计规范（沿用 myERP Python 项目）

- **无物理外键**：跨表引用是普通 `bigint` 列 + 索引，存在性/级联由 service 层校验。
- **无 DB ENUM**：`status` 等枚举字段统一 `varchar(N)`，合法性由 Rust enum 在 service 层校验。
- **乐观锁**：每张业务表都有 `version integer NOT NULL DEFAULT 0`。UPDATE 时显式
  `WHERE id = $1 AND version = $2`，影响行数 0 则视为冲突（HTTP 409 / `code::VERSION_CONFLICT`）。
- **软删除**：`deleted_at timestamp NULL`；查询统一 `WHERE deleted_at IS NULL`。
- **审计字段**：`created_at`、`created_by`、`updated_at`、`updated_by`，使用 `timestamp` 类型。
- **主键**：统一雪花 `bigint NOT NULL`，App 侧 `SnowflakeIdGenerator::next_id()` 生成。
- **时间字段**：DB 列存 naive `timestamp`（不带时区），应用层用 `crate::infra::clock::now_naive()` 写入
  Asia/Shanghai。

## Baseline 模式（2026-09-25 起）

- `001_baseline.sql` 是 sqlx 接管点，包含完整 final schema
- 生产重建：`pg_restore prod backup` → 应用 schema delta（015-029 的 schema 部分）
  → `TRUNCATE _sqlx_migrations` → `INSERT (20260925000000, <baseline_checksum>)`
  → 跑 `seeds/menu.sql`
- baseline checksum 改动必须同步更新 DB `_sqlx_migrations.checksum`（同下"修改已迁移文件"流程）
- 由于 baseline 是单文件，**实际生产中** baseline 一旦应用就**永不修改**；所有 schema 变更走追加

## 编译期 SQL 检查

业务实现阶段使用 sqlx 编译期宏 `query!` / `query_as!`：

```bash
# 1. 起本地 PG
docker compose up -d postgres-dev

# 2. 跑 baseline 迁移（首次部署；sqlx::migrate!() 启动时也会自动跑）
psql $DATABASE_URL -v ON_ERROR_STOP=1 -f migrations/20260925000000_001_baseline.sql

# 3. 在开发库上生成离线元数据
./scripts/sqlx_prepare.sh

# 4. CI / Docker 构建时设置
SQLX_OFFLINE=true cargo build --release
```

## 修改已迁移文件

`sqlx::migrate!()` 在运行时/编译期都校验 `_sqlx_migrations.checksum` 与迁移文件内容 SHA384 的一致性。
**任何对已迁移文件的修改**（包括注释、空行、空白）都会改变 SHA384，导致 `VersionMismatch(<version>)` panic。

### baseline 单文件场景（2026-09-25 后）

baseline 既然是合并单文件，**绝不修改**。所有变更走追加新 migration。新 migration 必须
**与已有 schema 兼容**（additive only；如 drop column，写迁移把数据拷到新列 + drop 旧列）。

### 备选方案：drop + 重建测试 DB

若改动影响多个迁移文件或不确定：

```bash
docker exec test psql -U hsh_test -d postgres -c 'DROP DATABASE postgres_rust_test'
# 下次 cargo test 会自动重建
```

**警告**：drop 会破坏其它 worktree 共享 DB 状态；多 worktree 跑测试时改用单行 UPDATE。