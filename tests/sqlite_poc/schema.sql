-- 2026-09-22 新增：SQLite POC 最小 t_user 表（**POC 简化版**，与 src/modules/iam/repo/sql.rs 的 PG 版并存、不共享 SQL）。
--
-- 关键差异（SQLite vs Postgres）：
--   * 雪花 ID：INTEGER PRIMARY KEY（PG 是 bigint + serial/seq）；service 仍按 i64 雪花 ID 写入。
--   * 时间：TEXT ISO8601（PG 是 naive timestamp）；与 chrono::NaiveDateTime 通过 sqlx-sqlite 默认映射走。
--   * 软删：与 PG 一致用 deleted_at TEXT NULL。
--   * is_active：INTEGER 0/1（PG 是 boolean；本表用 INTEGER 兼容 SQLite FromRow<bool>）。
--   * **不要** SERIAL / nextval / ::regclass（SQLite 无序列），**不要** ENUM 类型（SQLite 无原生 enum，用 TEXT + Rust enum 校验对齐 PG 范式）。
--
-- 本文件**不是** PG 迁移；migrations/ 不动；POC 用 sqlx::query() 手动执行（不走 sqlx::migrate!，见 oneshot_demo.rs 注释）。
CREATE TABLE IF NOT EXISTS t_user (
    id            INTEGER PRIMARY KEY,
    username      TEXT    NOT NULL UNIQUE,
    password_hash TEXT    NOT NULL,
    full_name     TEXT    NOT NULL,
    phone         TEXT,
    is_active     INTEGER NOT NULL DEFAULT 1,
    last_login_at TEXT,
    version       INTEGER NOT NULL DEFAULT 0,
    created_at    TEXT    NOT NULL,
    created_by    INTEGER,
    updated_at    TEXT    NOT NULL,
    updated_by    INTEGER,
    deleted_at    TEXT
);

CREATE INDEX IF NOT EXISTS ix_t_user_username ON t_user (username);
