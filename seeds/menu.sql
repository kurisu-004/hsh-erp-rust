-- ============================================================================
-- seeds/menu.sql —— 菜单树声明式种子（幂等，可反复跑）
-- ============================================================================
--
-- 应用方式：
--   1. App 启动钩子自动跑（见 src/infra/seed.rs）—— 默认开启
--   2. 手工：psql $DATABASE_URL -v ON_ERROR_STOP=1 -f seeds/menu.sql
--
-- 职责范围（2026-10-15 起）：
--   * 本文件只声明 `t_menu` 菜单目录 —— 部署配置，与前端路由耦合；
--   * 角色→菜单授权基线（t_role_menu）与已下线菜单的授权清理见
--     `seeds/rbac.sql`（§4 首次基线 / §5 清理授权残留），本文件不再写
--     t_role_menu。派生 ID 规则随授权段迁至该文件。
--   * 应用顺序必须是本文件在前、rbac.sql 在后：rbac.sql §5 的清理依赖
--     本文件 §3 的菜单软删结果。
--
-- 编写约定：
--   * 所有菜单走 INSERT ... ON CONFLICT (code) DO UPDATE，按 code 幂等
--   * parent_id 通过子查询按 code 反查，不写死雪花 ID（环境间 ID 可不同）
--   * t_menu.id 用静态 ID（9000000000xxx 段），便于识别"seed 灌的"行；
--     已存在的雪花 ID 行 ON CONFLICT 后保留原 ID，只更新其他字段
--   * 不自动删"未在本文件声明的菜单"——菜单下线必须**显式**列在第 3 节
--     "soft-delete 区段"（防止误删生产手工加的菜单）
--
-- 修改菜单 = 改本文件 + 重启 app（或 psql 手工跑一次）。不需要新 migration，
-- 不需要管 _sqlx_migrations.checksum。
--
-- 域内校验：
--   * t_menu.code 唯一（uk_t_menu_code WHERE deleted_at IS NULL）
-- ============================================================================

BEGIN;

-- ============================================================================
-- 第 0 节：复活软删行（保证 ON CONFLICT (code) WHERE deleted_at IS NULL 可命中）
-- ============================================================================
-- t_menu 的 code 唯一索引是 `uk_t_menu_code WHERE deleted_at IS NULL`（partial
-- unique index）。如果某 menu 行已被软删（deleted_at NOT NULL），新 seed 跑时
-- 不会被该 partial index 索引到，ON CONFLICT 也不会触发，会直接 INSERT 撞 PK。
--
-- 解法：先把所有"seed 声明过的 code"的软删行复活（deleted_at=NULL），让后续
-- ON CONFLICT 正常 upsert；section 3 软删区段会再次把需要下线的标记回 deleted_at。
UPDATE t_menu
SET deleted_at = NULL,
    updated_at = now(),
    version    = version + 1
WHERE deleted_at IS NOT NULL
  AND code IN (
    -- 顶级菜单
    'home', 'production_stats', 'scan_badge',
    'customer_management', 'order_group',
    'production_group', 'template_management', 'auth_group',
    'outsource_list', 'floor_group', 'settings_root',
    -- 子菜单
    'parts_list', 'parts_new', 'assemblies_list', 'delivery_notes_manage',
    -- 2026-10-08 下线 delivery_dispatch：端点已删，故意留在复活清单外
    'inspection_pending', 'repair_receive',
    'workers_list', 'users_list', 'shelves_list',
    -- 2026-10-14 新增：RBAC 角色面板（auth_group 子菜单，仅 MANAGER）
    'roles_list',
    'customers_list', 'applicants_list',
    'outsource_companies_list', 'outsource_quotes_list', 'outsource_send_receive_list',
    'process_work_type', 'part_process_chain', 'worker_queue',
    'print_templates_designer',
    -- 2026-09-29 新增：复活待编程（pending_programming）和待品检（inspection_pending）子项，
    -- 它们从顶级/order_group 移到 production_group 下；软删态要复活才能命中 ON CONFLICT。
    'pending_programming',
    -- 2026-10-16 换 code：'mail_group' / 'mail_digest' → 'scheduled_task' / 'mail'
    -- （一级「定时任务」/ 二级「邮件」，见下面 §1 / §2 两段）。
    --
    -- ⚠️ **两个旧 code 必须从复活清单里移除**，否则本节会把它们复活，而 §3.6 又要
    -- 把它们软删 —— 两段互相抵消，seed 反复跑会在「活/删」之间来回跳。更重要的是：
    -- `ON CONFLICT (code) WHERE deleted_at IS NULL` 的冲突目标是 **code**，改 code 后
    -- 旧行**不参与冲突判定** ⇒ 新行是 INSERT 而非 UPDATE，`id` 必须换新值（见 §1 / §2
    -- 的两条 ⚠️）。旧行只由 §3.6 单向软删，不需要复活。
    --
    -- 2026-10-13 复活它们时的理由（软删态下不被 partial 索引命中、ON CONFLICT 失效
    -- 会撞 PK）对本轮的新 code **不适用**：新 code 从来没有过软删行。
    'scheduled_task', 'mail',
    -- 2026-10-18 新增：scheduled_task 下第三个子项「企业微信」（`wecom`），
    -- 与 §2 那条 INSERT 配对登记。此刻该 code 还没有任何行（含软删行）⇒
    -- 本节的 UPDATE 对它是 no-op；列在这里是为了让「本 seed 声明过的 code」
    -- 清单完整 —— 将来若有人下线再复活这个菜单，它已被覆盖。
    'wecom'
  );

-- ============================================================================
-- 第 1 节：顶级菜单（parent_id = NULL）
-- ============================================================================
INSERT INTO t_menu (
    id, parent_id, code, title, path, icon, sort_order, is_active,
    version, created_at, created_by, updated_at, updated_by
) VALUES
    (9000000000000001, NULL, 'home',                 '首页',       '/dashboard',          'House',          10, true, 0, now(), 0, now(), 0),
    (9000000000000002, NULL, 'production_stats',     '生产统计',   '/statistics',         'DataAnalysis',   12, true, 0, now(), 0, now(), 0),
    (9000000000000003, NULL, 'scan_badge',           '扫码台',     '/scan/badge',         'Promotion',      13, true, 0, now(), 0, now(), 0),
    (9000000000000004, NULL, 'customer_management',  '客户管理',   NULL,                  'OfficeBuilding', 15, true, 0, now(), 0, now(), 0),
    (9000000000000005, NULL, 'order_group',          '订单管理',   NULL,                  'Tickets',        20, true, 0, now(), 0, now(), 0),
    -- 2026-09-29 移除：pending_programming（id=6）从顶级菜单移到 production_group 子菜单；
    -- 旧行保留 id（9000000000000006），ON CONFLICT 走 sub-menu 段更新 parent_id + title。
    (9000000000000007, NULL, 'production_group',     '生产管理',   NULL,                  'Operation',      27, true, 0, now(), 0, now(), 0),
    (9000000000000008, NULL, 'template_management',  '模板管理',   NULL,                  'Document',       28, true, 0, now(), 0, now(), 0),
    (9000000000000009, NULL, 'auth_group',           '权限管理',   NULL,                  'Key',            30, true, 0, now(), 0, now(), 0),
    (9000000000000010, NULL, 'outsource_list',       '外协管理',   NULL,                  'Promotion',      35, true, 0, now(), 0, now(), 0),
    (9000000000000011, NULL, 'floor_group',          '车间',       NULL,                  'Tools',          40, true, 0, now(), 0, now(), 0),
    -- 2026-10-16 新增：「定时任务」一级菜单（sort_order=45，位置与理由沿用 2026-10-13
    -- 那版 `mail_group`：一级菜单从左到右按 sort_order 排，「车间」是生产执行侧入口、
    -- 「设置」是系统配置侧入口，定时任务属于两者之间的「运维自动化」，45 正好卡在中间）。
    --
    -- ⚠️ **用新 id `9000000000000014`，不复用旧行的 `...013`**：`ON CONFLICT (code)
    -- WHERE deleted_at IS NULL` 的冲突目标是 **code**；本行 code 由 `mail_group` 改成
    -- `scheduled_task`，旧行（id=...013，code=`mail_group`）**不参与**冲突判定 ⇒ 本行
    -- 走的是 INSERT。若复用 id=...013，会撞 `t_menu_pkey` 让整条 seed 回滚；
    -- 若不软删旧行，还会留下一条同 title 的孤儿行。
    (9000000000000014, NULL, 'scheduled_task',       '定时任务',     NULL,                'Clock',          45, true, 0, now(), 0, now(), 0),
    (9000000000000012, NULL, 'settings_root',        '设置',       NULL,                  'Setting',        50, true, 0, now(), 0, now(), 0)
ON CONFLICT (code) WHERE deleted_at IS NULL DO UPDATE SET
    parent_id  = EXCLUDED.parent_id,
    title      = EXCLUDED.title,
    path       = EXCLUDED.path,
    icon       = EXCLUDED.icon,
    sort_order = EXCLUDED.sort_order,
    is_active  = EXCLUDED.is_active,
    updated_at = now(),
    version    = t_menu.version + 1;

-- ============================================================================
-- 第 2 节：子菜单（parent_id 按 code 反查，不写死）
-- ============================================================================
INSERT INTO t_menu (
    id, parent_id, code, title, path, icon, sort_order, is_active,
    version, created_at, created_by, updated_at, updated_by
) VALUES
    -- order_group 下
    (9000000000000101, (SELECT id FROM t_menu WHERE code = 'order_group'         AND deleted_at IS NULL), 'parts_list',                '零件一览',      '/parts',                       'Box',          10, true, 0, now(), 0, now(), 0),
    (9000000000000102, (SELECT id FROM t_menu WHERE code = 'order_group'         AND deleted_at IS NULL), 'parts_new',                 '新建零件',      '/parts/new',                   'Plus',         20, true, 0, now(), 0, now(), 0),
    (9000000000000103, (SELECT id FROM t_menu WHERE code = 'order_group'         AND deleted_at IS NULL), 'assemblies_list',           '装配件一览',    '/assemblies',                  'Connection',   30, true, 0, now(), 0, now(), 0),
    (9000000000000104, (SELECT id FROM t_menu WHERE code = 'order_group'         AND deleted_at IS NULL), 'delivery_notes_manage',     '送货单',        '/delivery-notes',              'Document',     30, true, 0, now(), 0, now(), 0),
    -- 2026-09-29 移除：inspection_pending 从 order_group 移到 production_group 下；sort_order 50 → 30。
    (9000000000000107, (SELECT id FROM t_menu WHERE code = 'order_group'         AND deleted_at IS NULL), 'repair_receive',            '返修接收',      '/repair/receive',              'Tools',        65, true, 0, now(), 0, now(), 0),

    -- auth_group 下（含 025 把 shelves_list 从 floor_group 移入）
    -- 2026-10-04 移除：workers_list 从 auth_group 移到 production_group 下（前端视图
    -- 搬到 frontend/src/views/production/，路由随之前端搬到 /production/worker-list）。
    (9000000000000202, (SELECT id FROM t_menu WHERE code = 'auth_group'         AND deleted_at IS NULL), 'users_list',                '账号管理',      '/users',                       'List',         20, true, 0, now(), 0, now(), 0),
    (9000000000000203, (SELECT id FROM t_menu WHERE code = 'auth_group'         AND deleted_at IS NULL), 'shelves_list',              '货架管理',      '/shelves',                     'Platform',     30, true, 0, now(), 0, now(), 0),
    -- 2026-10-14 新增：RBAC 角色面板（角色 / 权限点 / 授权整包，iam::rbac 的 8 条
    -- 端点）。sort_order=40 接在 shelves_list=30 之后（与前端 dummy 模式侧栏的
    -- users → shelves → roles 顺序一致）；path 与前端路由 /iam/roles 一致。
    (9000000000000204, (SELECT id FROM t_menu WHERE code = 'auth_group'         AND deleted_at IS NULL), 'roles_list',                '角色权限',      '/iam/roles',                   'Lock',         40, true, 0, now(), 0, now(), 0),

    -- customer_management 下
    (9000000000000401, (SELECT id FROM t_menu WHERE code = 'customer_management' AND deleted_at IS NULL), 'customers_list',         '客户一览',      '/customers',                   'Connection',   10, true, 0, now(), 0, now(), 0),
    (9000000000000402, (SELECT id FROM t_menu WHERE code = 'customer_management' AND deleted_at IS NULL), 'applicants_list',        '申请人一览',    '/applicants',                  'User',         20, true, 0, now(), 0, now(), 0),

    -- outsource_list 下
    (9000000000000501, (SELECT id FROM t_menu WHERE code = 'outsource_list'    AND deleted_at IS NULL), 'outsource_companies_list',  '外协厂一览',    '/outsource/companies',         'OfficeBuilding', 10, true, 0, now(), 0, now(), 0),
    (9000000000000502, (SELECT id FROM t_menu WHERE code = 'outsource_list'    AND deleted_at IS NULL), 'outsource_quotes_list',     '报价一览',      '/outsource/quotes',            'Document',     20, true, 0, now(), 0, now(), 0),
    (9000000000000503, (SELECT id FROM t_menu WHERE code = 'outsource_list'    AND deleted_at IS NULL), 'outsource_send_receive_list','外协发送/接收','/outsource/send-receive',       'Promotion',    30, true, 0, now(), 0, now(), 0),

    -- production_group 下（018/021/024 累计 + 2026-09-29 新增 pending_programming / inspection_pending + 2026-10-04 迁入 workers_list）
    (9000000000000601, (SELECT id FROM t_menu WHERE code = 'production_group'  AND deleted_at IS NULL), 'process_work_type',         '工序工种',      '/production/process-work-type','Operation',     5, true, 0, now(), 0, now(), 0),
    (9000000000000602, (SELECT id FROM t_menu WHERE code = 'production_group'  AND deleted_at IS NULL), 'part_process_chain',        '制定工序',      '/production/process-design',   'SetUp',        10, true, 0, now(), 0, now(), 0),
    -- 2026-09-29 新增：待编程（原顶级菜单 id=6，title 改「待编程」）挂在 production_group 下；
    -- 用原 id（9000000000000006）保证 ON CONFLICT 命中；sort_order=15（在 process_work_type=5 / part_process_chain=10 之后、worker_queue=20 之前）。
    (9000000000000006, (SELECT id FROM t_menu WHERE code = 'production_group'  AND deleted_at IS NULL), 'pending_programming',       '待编程',        '/cnc/pending',                'Cpu',          15, true, 0, now(), 0, now(), 0),
    -- 2026-10-04：path 随前端视图搬到 /production/worker-queue（sort_order 不变）。
    (9000000000000603, (SELECT id FROM t_menu WHERE code = 'production_group'  AND deleted_at IS NULL), 'worker_queue',              '生产队列',      '/production/worker-queue',    'Operation',    20, true, 0, now(), 0, now(), 0),
    -- 2026-10-04 新增（自 auth_group 迁入）：工人档案管理，路由随前端视图搬到
    -- /production/worker-list；sort_order=25（插在 worker_queue=20 与 inspection_pending=30 之间）。
    -- 换父分组不改授权：授权基线仍是扁平 code 列表（见 seeds/rbac.sql §4），workers_list 仅 MANAGER 持有。
    (9000000000000201, (SELECT id FROM t_menu WHERE code = 'production_group'  AND deleted_at IS NULL), 'workers_list',              '工人一览',      '/production/worker-list',     'User',         25, true, 0, now(), 0, now(), 0),
    -- 2026-09-29 新增：待品检（inspection_pending）从 order_group 移到 production_group；
    -- 原 id（9000000000000105）保留；sort_order 从 50 改为 30（在 worker_queue 之后）。
    (9000000000000105, (SELECT id FROM t_menu WHERE code = 'production_group'  AND deleted_at IS NULL), 'inspection_pending',        '待品检',        '/inspection/pending',          'CircleCheck',  30, true, 0, now(), 0, now(), 0),

    -- 2026-10-16 新增：scheduled_task 下。**邮件**（页面上方用 tab 区分邮件类型，
    -- 当前只有「交期汇总」一种；邮箱账号与授权码在页内配置）。
    --
    -- ⚠️ **新 id `9000000000000902`**：与 §1 那条同理 —— code 由 `mail_digest` 改成
    -- `mail`，旧行（id=...901）不参与 `ON CONFLICT (code)` 判定，复用 id 会撞 pkey。
    -- path 与前端路由一致（`/mail`）。
    (9000000000000902, (SELECT id FROM t_menu WHERE code = 'scheduled_task'   AND deleted_at IS NULL), 'mail',                      '邮件',          '/mail',                        'Message',     10, true, 0, now(), 0, now(), 0),
    -- 2026-10-18 新增：scheduled_task 下。**企业微信**（群机器人 webhook 通道 +
    -- 每日总结推送内容；页面上方同样用 tab 区分推送类型）。
    --
    -- ⚠️ **id 必须是 `9000000000000903`**：`...902` 已被「邮件」占、上面 §1 那段
    -- 里 `...013` 是**已软删**的旧 `mail_group` 行（物理占用，绝不可复用 ——
    -- id 是唯一能把「软删」与「新增」区分开的标识），`...001..014` 顶级段也已占满。
    -- `...903` 是 `9000…090x` 这一段里 `scheduled_task` 下的唯一空闲位。
    --
    -- ⚠️ `path='/wecom'` 必须与前端 router 的 `path: 'wecom'` **逐字一致**：
    -- 不一致时前端路由守卫会把用户踢走（现象是「点菜单闪一下又回到别的页」）。
    -- ⚠️ **菜单 path 不跟着 API 前缀变**：后端 API 已硬切到 `/api/v2/cron/wecom*`
    -- （2026-10-18），菜单 path 仍是 `/wecom` —— 两者是不同层的东西。
    --
    -- icon `ChatDotRound`：`@element-plus/icons-vue@2.3.1` 确实导出该图标；
    -- 前端 `src/layouts/components/menuIcons.ts` 的 `ICON_MAP` 同步补一条
    -- （与后端 seed 锁步，前端那一半不在本仓库）。
    -- 授权：仅 MANAGER，见 `seeds/rbac.sql` §4.1。
    (9000000000000903, (SELECT id FROM t_menu WHERE code = 'scheduled_task'   AND deleted_at IS NULL), 'wecom',                     '企业微信',      '/wecom',                       'ChatDotRound', 20, true, 0, now(), 0, now(), 0),

    -- template_management 下（023）
    (9000000000000701, (SELECT id FROM t_menu WHERE code = 'template_management' AND deleted_at IS NULL), 'print_templates_designer','模板编辑',     '/print-templates',             'Document',     10, true, 0, now(), 0, now(), 0)
ON CONFLICT (code) WHERE deleted_at IS NULL DO UPDATE SET
    parent_id  = EXCLUDED.parent_id,
    title      = EXCLUDED.title,
    path       = EXCLUDED.path,
    icon       = EXCLUDED.icon,
    sort_order = EXCLUDED.sort_order,
    is_active  = EXCLUDED.is_active,
    updated_at = now(),
    version    = t_menu.version + 1;

-- ============================================================================
-- 第 3 节：菜单下线（显式软删；不自动删未声明菜单）
-- ============================================================================
-- 3.1 settings_root 及其 3 个子菜单（021 重构：合并到 production_group → 工序工种）
UPDATE t_menu
SET deleted_at = now(),
    is_active  = false,
    updated_at = now(),
    version    = version + 1
WHERE code IN ('settings_root', 'work_types_list', 'processes_list', 'work_type_processes_list')
  AND deleted_at IS NULL;

-- 3.2 floor_group 停用（025：保留行便于审计，is_active=false；不软删）
UPDATE t_menu
SET is_active  = false,
    updated_at = now(),
    version    = version + 1
WHERE code = 'floor_group' AND deleted_at IS NULL AND is_active = true;

-- 3.3 assemblies_new 已在 prod 软删，seed 显式声明保持软删状态（幂等兜底）
UPDATE t_menu
SET is_active  = false,
    updated_at = now(),
    version    = version + 1
WHERE code = 'assemblies_new' AND deleted_at IS NOT NULL AND is_active = true;

-- 3.4 assemblies_list 保留 is_active=false（prod 已停用；不在前端菜单树显示但保留
--     role_menu 关系以备未来重新启用）
--     seed 显式兜底，避免手工恢复：
UPDATE t_menu
SET is_active  = false,
    updated_at = now(),
    version    = version + 1
WHERE code = 'assemblies_list' AND deleted_at IS NULL AND is_active = true;

-- 3.5 delivery_dispatch 整条下线（2026-10-08）：com::delivery_note 域把「待司机领取
--     一览」（GET /pickup-pending）与「送货台逐件扫码核销」（POST /{id}/pickup-scan）
--     两条端点删掉了，前端目标页 /delivery-dispatch 仍在（删页属前端仓范围）⇒ 不下线
--     的话是「菜单能点进去、页面能打开、每个请求都 404」的活条目。
--     走「seed 不再声明 + 显式软删」而不是硬删：t_menu.code 的唯一索引是
--     uk_t_menu_code WHERE deleted_at IS NULL，硬删会与本文件 §0 的复活机制、
--     以及 seeds/rbac.sql §5 的授权清理（按 m.deleted_at IS NOT NULL 回收残留）
--     三方打架。
--     配套：§0 复活清单已移除该 code；授权基线（seeds/rbac.sql §4）不再声明它，
--     其存量授权残留由 rbac.sql §5 清理。
UPDATE t_menu
SET deleted_at = now(),
    is_active  = false,
    updated_at = now(),
    version    = version + 1
WHERE code = 'delivery_dispatch' AND deleted_at IS NULL;

-- 3.6 旧的「定时任务/邮件」两个菜单 code 整条下线（2026-10-16）：一级 code
--     `mail_group` → `scheduled_task`、二级 code `mail_digest` → `mail`。
--     走的仍是「seed 声明新 code + 显式软删旧 code」这条路（与 3.5 同款）而不是硬删：
--     `t_menu.code` 的唯一索引是 uk_t_menu_code WHERE deleted_at IS NULL，硬删会与
--     本文件 §0 的复活机制、以及 seeds/rbac.sql §5 的授权清理（按 m.deleted_at IS NOT
--     NULL 回收残留）三方打架。
--     配套：§0 复活清单已**移除**这两个旧 code（复活它们再软删是两段互相抵消）；
--     授权基线（rbac.sql §4.1）不再声明它们，存量授权残留由 rbac.sql §5 清理。
--     ⚠️ 旧的 `path='/mail/digest'` 一并失效：前端路由同批改到 `/mail`。
UPDATE t_menu
SET deleted_at = now(),
    is_active  = false,
    updated_at = now(),
    version    = version + 1
WHERE code IN ('mail_group', 'mail_digest') AND deleted_at IS NULL;

COMMIT;
