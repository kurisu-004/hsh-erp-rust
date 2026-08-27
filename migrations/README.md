# 数据库迁移规范（sqlx migrate）

本目录由 `sqlx::migrate!()` 宏在编译期与运行时扫描，**只认 `*.sql` 文件**，README 不影响迁移。

## 命名

`<13 位时间戳>_<顺序>_<简短描述>.sql`，例如 `20260710120001_create_part_table.sql`。

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

## 编译期 SQL 检查

业务实现阶段使用 sqlx 编译期宏 `query!` / `query_as!`：

```bash
# 1. 起本地 PG（参见 scripts/dev_db.sh）
./scripts/dev_db.sh

# 2. 执行迁移
cargo run -- migrations up   # 或在 main 启动时自动 sqlx::migrate!().run()

# 3. 在开发库上生成离线元数据
./scripts/sqlx_prepare.sh
# 该脚本执行 cargo sqlx prepare -- --lib，结果写入 .sqlx/query-*.json

# 4. CI / Docker 构建时设置
SQLX_OFFLINE=true cargo build --release
```

## 分支策略（可选）

Python 项目原有 alembic `schema` + `prod_data` 双分支。Rust 端如需：

- `migrations/` 放所有 schema 迁移
- `seeds/` 放初始种子数据（不在 `sqlx::migrate!` 扫描范围内，由独立命令加载）

实施阶段根据需要细化。

## 修改已迁移文件

`sqlx::migrate!()` 在运行时/编译期都校验 `_sqlx_migrations.checksum` 与迁移文件内容 SHA384 的一致性。
**任何对已迁移文件的修改**（包括注释、空行、空白）都会改变 SHA384，导致 `VersionMismatch(<version>)` panic。

### 同步 DB checksum

修改已迁移文件后，必须同步更新对应 DB 行的 checksum（仅测试 DB；dev DB 通过 `cargo run` 启动时 sqlx 自动 migrate 不需手动改）：

```bash
# 1. 算新文件的 SHA384
sha384sum migrations/<timestamp>_<seq>_<desc>.sql

# 2. 在测试 DB 上 UPDATE 单行（单 row 单 column，严格 scope）
docker exec test psql -U hsh_test -d postgres_rust_test -c "UPDATE _sqlx_migrations SET checksum = decode('<sha384_hex>', 'hex') WHERE version = <timestamp>"
# 例：UPDATE 1
```

### 替代方案：drop + 重建测试 DB

若改动影响多个迁移文件或不确定：

```bash
docker exec test psql -U hsh_test -d postgres -c 'DROP DATABASE postgres_rust_test'
# 下次 cargo test 会自动重建
```

**警告**：drop 会破坏其它 worktree 共享 DB 状态；多 worktree 跑测试时改用方案 1（单行 UPDATE）。