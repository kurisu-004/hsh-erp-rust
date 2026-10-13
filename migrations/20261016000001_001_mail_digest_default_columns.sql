-- ============================================================================
-- 20261016000001_001_mail_digest_default_columns —— 正文列白名单加入「批次」与「位置」
-- ============================================================================
-- 归属域：`mail_digest`（`src/modules/mail_digest/`）。
--
-- 需求（用户原话，2026-10-16）：
--   2. 工单下方要紧跟着对应的所有批次；3. 增加一列「位置」，显示每个批次所在的位置。
--
-- 本 migration 只做一件事：把 `t_mail_digest_config.visible_columns` 的**列 DEFAULT**
-- 从 8 列扩到 10 列（新增 `batch_no` 与 `location`），并**回填**存量未软删行。
--
-- ⚠️ **为什么必须新开迁移而不是改 `20261013000000_001_mail_digest.sql`**：
--     迁移是 append-only（`migrations/README.md`），改已应用的文件会让
--     `sqlx::migrate!` 抛 `VersionMismatch` 且让已部署库与新库分叉。故本条是
--     **追加**的 `ALTER … SET DEFAULT` + `UPDATE`。
--
-- ⚠️ **回填会静默改掉管理员已存的勾选**（刻意决定）：存量行里存的是当初的 8 列
--     白名单，而用户明确要求正文里能看到批次与位置 —— 只对**缺省值**生效的话，
--     已经保存过配置的环境永远看不到这两列（列白名单一旦被管理员存过，就与列 DEFAULT
--     无关了）。故此处**无条件覆写**未软删行的 `visible_columns`。
--     代价：管理员手工去掉过某列的勾选会被重新加回，且该列现在**可以**再手工勾掉
--     （`POST /config` 全量覆写），影响面仅限本次升级那一次。
--     登记见 `docs/api/mail_digest.md` §8.3。
--
-- 幂等：重复执行结果相同（`SET DEFAULT` 幂等；`UPDATE` 是把同一份字面量写回去）。
-- ============================================================================

-- ① 列默认值（影响此后**不带该列**的 INSERT）
ALTER TABLE t_mail_digest_config
    ALTER COLUMN visible_columns SET DEFAULT
        '{serial_no,drawing_no,name,batch_no,quantity,customer_name,status,location,system_delivery_date,remaining_days}';

-- ② 回填存量行（`deleted_at IS NULL` = 活着的配置单例；软删行是历史快照，不动）
UPDATE t_mail_digest_config
   SET visible_columns =
        '{serial_no,drawing_no,name,batch_no,quantity,customer_name,status,location,system_delivery_date,remaining_days}'
 WHERE deleted_at IS NULL;

COMMENT ON COLUMN t_mail_digest_config.visible_columns IS
    '正文表格列白名单（varchar(30)[]）；取值域 = mail_digest::dto::VisibleColumn 枚举，未知项返 21704；2026-10-16 起缺省 10 列（新增 batch_no / location）';