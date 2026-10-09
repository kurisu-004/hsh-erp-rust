-- ============================================================================
-- 一次性数据回填：t_user.worker_id（账号 ↔ 工人绑定）
-- ============================================================================
-- 配套 schema 变更：migrations/20261011120000_001_add_user_worker_id.sql（2026-10-11）
--
-- ⚠️⚠️ **这是一次性数据回填，不是 schema 迁移，必须人工确认后手工执行。**
-- ⚠️⚠️ 它**不会**被 `sqlx::migrate!()` / app 启动钩子自动跑；仓库里没有、以后也不会
--       有任何自动执行它的路径。
--
-- ## 为什么单独成文件而不是写进 migration
--   migration = schema 变更（DDL），由 `_sqlx_migrations` 记账、版本链不可变；
--   本文件 = 存量数据修正（DML），跑一次即可，幂等重跑无副作用。
--   把 DML 塞进 migration 会让**每一个新建的库**（含 CI 每个测试用例 clone 出来的
--   template DB）都执行这段猜测逻辑，且把一次性动作永久钉进版本链。
--
-- ## 为什么放 scripts/sql/ 而不是 seeds/
--   `seeds/*.sql` 由 `src/infra/seed.rs` 启动钩子 `include_str!` 嵌进二进制自动执行
--   （`menu.sql` 无条件跑、`admin.sql` 由 `BOOTSTRAP_ADMIN_ENABLED` 门控）。
--   「按 username = badge_code 猜绑定」这种带推断性质的动作绝不能进启动路径。
--   `scripts/` 是本仓既有的**人工触发**运维脚本落点（`seed_apply.sh` /
--   `restore_from_backup.sh` 等），本文件与它们同族。
--
-- ## 执行方式（人工，先 dry-run）
--   ⚠️⚠️ **本文件已在首尾包裹 `BEGIN;` / `COMMIT;`**（2026-10-11 review 第 1 轮补）。
--   包裹之前本文件**没有任何显式事务**，而 psql 对无显式事务的脚本是**逐句
--   autocommit** —— 那时下面「把末尾 COMMIT 换成 ROLLBACK」的指引是**假的**：
--   psql 找不到名为 `COMMIT` 的语句，照着跑的「dry-run」会**真写库**。
--
--   # 1. dry-run（二选一）
--   # 1a. 最稳：把 §2 的 SELECT **单独复制**到 psql 里跑（纯读，无副作用），
--   #     确认清单符合预期后再走第 2 步。
--   # 1b. 整文件 dry-run：把**本文件末尾的 `COMMIT;` 换成 `ROLLBACK;`**（替换，不是
--   #     追加 —— 两个都留会报错），再用下面的命令整文件跑。首尾的
--   #     `BEGIN;` … `ROLLBACK;` 会把整份脚本包进一个事务，结尾回滚，不落库。
--   psql "$DATABASE_URL" -v ON_ERROR_STOP=1 \
--     -f scripts/sql/20261011_backfill_t_user_worker_id.sql
--
--   # 2. 真跑（幂等，重复执行无副作用）
--   psql "$DATABASE_URL" -v ON_ERROR_STOP=1 \
--     -f scripts/sql/20261011_backfill_t_user_worker_id.sql
--
--   ⚠️ `-v ON_ERROR_STOP=1` **不可省**：没有它，§3 的 UPDATE 一旦报错 psql 只打印
--   错误并**继续往下跑**末尾的 `COMMIT;`，把半截结果提交进库。
--
-- ## 推断规则（保守，宁可留 NULL 也不猜）
--   候选 = `t_user.username` 与 `t_worker.badge_code` **逐字相等**且双方均未软删，
--   且**有且仅有一条**候选工人。零命中 / 多命中一律不动，留 NULL 交人工处理。
--
--   实测（2026-10-11，主 checkout dev 库）按此规则的判定结果：
--     15105972335 翁美月 → 命中        18046244109 曾学辉 → 命中
--     18064554025 童敏华 → 命中        18250705779 黄道玉 → 命中
--     13350114794 陈燕   → **不命中**（t_worker 里工牌是 13359114794，号段笔误）
--     13606071983 曾文洪 → **不命中**（t_worker 无此人）
--     admin / 系统管理员 / hmi-a1 / hmi-b1 → **不命中**（系统账号，见下方排除名单）
--
--   为什么「username = badge_code」只是个**候选**而不是结论：它是本仓目前唯一的
--   可用线索，工牌号与登录账号在本部署里恰好都取手机号。但它已被上述「陈燕」一例
--   证伪过一次，所以本脚本只做**机械匹配 + 人工复核**，绝不替人做判断。

BEGIN;

-- ============================================================================
-- §1 排除名单（系统 / 非工人账号，宁可漏绑不可错绑）
-- ============================================================================
-- 下面这几个 username 是系统账号，不是车间工人：即便将来有人在 t_worker 里建出同名
-- 工牌，也**不应**把系统账号绑上去。
-- ⚠️ 新增系统账号时记得同步这里；漏了会被 §2 的「纯数字」条件挡住大部分情况，
--    但显式名单是第二道保险。

-- ============================================================================
-- §2 预览：这些账号会被绑定（先看清单，再决定要不要跑 §3）
-- ============================================================================
-- 判据逐条列在下方 WHERE 里，与 §3 的 UPDATE **逐字一致**（改一处必须同步另一处）。

SELECT u.id                                AS user_id,
       u.username                          AS username,
       u.full_name                         AS full_name,
       w.id                                AS worker_id,
       w.badge_code                        AS badge_code,
       w.name                              AS worker_name
FROM t_user u
JOIN t_worker w
  ON w.badge_code = u.username
 AND w.deleted_at IS NULL
WHERE u.deleted_at IS NULL
  AND u.worker_id IS NULL
  -- 只认「逐字相等」：账号名必须就是工牌号（等值 JOIN 已隐含，此处显式重复以便阅读）
  AND u.username = w.badge_code
  -- 「有且仅有一条」：多命中宁可不绑
  AND (SELECT count(*) FROM t_worker w2
        WHERE w2.badge_code = u.username AND w2.deleted_at IS NULL) = 1
  -- 排除系统 / 非工人账号
  AND u.username <> ALL (ARRAY['admin', '系统管理员', 'hmi-a1', 'hmi-b1'])
  -- 保守闸门：只处理「长得像工牌号」（纯数字）的账号。
  -- 本部署的 badge_code 就是 11 位手机号；这一条把 admin / hmi-* 这类命名型账号
  -- 一并挡在外面，即使 §1 的显式名单没跟上也不会错绑。
  AND u.username ~ '^[0-9]+$'
ORDER BY u.username;

-- ============================================================================
-- §3 绑定（幂等：只碰 worker_id IS NULL 的行，重复执行无副作用）
-- ============================================================================
-- ⚠️ 本段**不**动 version / updated_at / updated_by，理由：
--   本列目前**没有任何 app 写端点**会写它（建账号 / 改账号的 DTO 都还没有
--   worker_id 入口），所以不存在 lost-update 风险；而 bump version 会让所有正
--   拿着旧 version 编辑这些账号的客户端凭空吃到 409，与本次改动要修的「恒返 0」
--   问题毫无关系。等 B2 真正把 worker_id 接进写端点后，绑定关系就改由 OCC 路径维护。

UPDATE t_user u
SET worker_id = w.id
FROM t_worker w
WHERE w.badge_code = u.username
  AND w.deleted_at IS NULL
  AND u.username = w.badge_code
  AND u.deleted_at IS NULL
  AND u.worker_id IS NULL
  AND (SELECT count(*) FROM t_worker w2
        WHERE w2.badge_code = u.username AND w2.deleted_at IS NULL) = 1
  AND u.username <> ALL (ARRAY['admin', '系统管理员', 'hmi-a1', 'hmi-b1'])
  AND u.username ~ '^[0-9]+$'
RETURNING u.id, u.username, u.full_name, u.worker_id;

-- ============================================================================
-- §4 核对：跑完后手工看一眼这两张表
-- ============================================================================
-- 已绑定：
--   SELECT u.username, u.full_name, u.worker_id, w.badge_code, w.name
--   FROM t_user u JOIN t_worker w ON w.id = u.worker_id
--   WHERE u.worker_id IS NOT NULL AND u.deleted_at IS NULL ORDER BY u.username;
--
-- 仍未绑定（这些需要人工判断，不能靠推断）：
--   SELECT id, username, full_name, is_active FROM t_user
--   WHERE worker_id IS NULL AND deleted_at IS NULL ORDER BY username;

-- ⚠️ dry-run 时把下面这行**替换**成 `ROLLBACK;`（不是追加），见文件头「执行方式」。
COMMIT;
