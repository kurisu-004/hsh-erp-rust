-- ============================================================================
-- seeds/menu.sql —— 菜单树声明式种子（幂等，可反复跑）
-- ============================================================================
--
-- 应用方式：
--   1. App 启动钩子自动跑（见 src/infra/seed.rs）—— 默认开启
--   2. 手工：psql $DATABASE_URL -v ON_ERROR_STOP=1 -f seeds/menu.sql
--
-- 编写约定：
--   * 所有菜单走 INSERT ... ON CONFLICT (code) DO UPDATE，按 code 幂等
--   * parent_id 通过子查询按 code 反查，不写死雪花 ID（环境间 ID 可不同）
--   * t_menu.id 用静态 ID（9000000000xxx 段），便于识别"seed 灌的"行；
--     已存在的雪花 ID 行 ON CONFLICT 后保留原 ID，只更新其他字段
--   * t_role_menu 同样用静态 ID（9000001000xxx 段）
--   * 不自动删"未在本文件声明的菜单"——菜单下线必须**显式**列在第 3 节
--     "soft-delete 区段"（防止误删生产手工加的菜单）
--
-- 修改菜单 = 改本文件 + 重启 app（或 psql 手工跑一次）。不需要新 migration，
-- 不需要管 _sqlx_migrations.checksum。
--
-- 域内校验：
--   * t_menu.code 唯一（uk_t_menu_code WHERE deleted_at IS NULL）
--   * t_role_menu (role, menu_id) 唯一（uk_t_role_menu_role_menu WHERE deleted_at IS NULL）
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
    'customers_list', 'applicants_list',
    'outsource_companies_list', 'outsource_quotes_list', 'outsource_send_receive_list',
    'process_work_type', 'part_process_chain', 'worker_queue',
    'print_templates_designer',
    -- 2026-09-29 新增：复活待编程（pending_programming）和待品检（inspection_pending）子项，
    -- 它们从顶级/order_group 移到 production_group 下；软删态要复活才能命中 ON CONFLICT。
    'pending_programming'
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
    -- 换父分组不改授权：t_role_menu 仍是扁平 code 列表，workers_list 仅 MANAGER 持有。
    (9000000000000201, (SELECT id FROM t_menu WHERE code = 'production_group'  AND deleted_at IS NULL), 'workers_list',              '工人一览',      '/production/worker-list',     'User',         25, true, 0, now(), 0, now(), 0),
    -- 2026-09-29 新增：待品检（inspection_pending）从 order_group 移到 production_group；
    -- 原 id（9000000000000105）保留；sort_order 从 50 改为 30（在 worker_queue 之后）。
    (9000000000000105, (SELECT id FROM t_menu WHERE code = 'production_group'  AND deleted_at IS NULL), 'inspection_pending',        '待品检',        '/inspection/pending',          'CircleCheck',  30, true, 0, now(), 0, now(), 0),

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
--     uk_t_menu_code WHERE deleted_at IS NULL，硬删会与 4.6 段 `m.deleted_at IS NOT
--     NULL` 的 role_menu 回收、以及本文件 §0 的复活机制三方打架。
--     配套：§0 复活清单已移除该 code、§4.1 / §4.3 白名单已移除、4.6 段负责回收存量
--     role_menu 行。
UPDATE t_menu
SET deleted_at = now(),
    is_active  = false,
    updated_at = now(),
    version    = version + 1
WHERE code = 'delivery_dispatch' AND deleted_at IS NULL;

-- ============================================================================
-- 第 4 节：角色授权（t_role_menu）
-- ============================================================================
-- 注：assemblies_list 虽是 is_active=false 但仍挂在 CLERK/INSPECTOR/MANAGER 的
-- 授权表里（保留 prod 现状，未来如重新启用不必再补 role_menu）。

-- 4.1 MANAGER：除「扫码台」（HMI 专用）外几乎全权
INSERT INTO t_role_menu (id, role, menu_id, version, created_at, created_by, updated_at, updated_by)
SELECT
    9000000001000001 + row_number() OVER (),
    'MANAGER',
    m.id,
    0, now(), 0, now(), 0
FROM t_menu m
WHERE m.code IN (
    'home', 'production_stats',
    'customer_management', 'customers_list', 'applicants_list',
    'order_group', 'parts_list', 'parts_new', 'delivery_notes_manage',
    'inspection_pending', 'repair_receive',
    'assemblies_list',
    'pending_programming',
    'production_group', 'process_work_type', 'part_process_chain', 'worker_queue',
    'template_management', 'print_templates_designer',
    'auth_group', 'workers_list', 'users_list', 'shelves_list',
    'outsource_list', 'outsource_companies_list', 'outsource_quotes_list', 'outsource_send_receive_list',
    'floor_group', 'settings_root'
)
AND m.deleted_at IS NULL
ON CONFLICT (role, menu_id) WHERE deleted_at IS NULL DO NOTHING;

-- 4.2 CLERK：业务操作员（无 auth_group 域、无 template_management）
INSERT INTO t_role_menu (id, role, menu_id, version, created_at, created_by, updated_at, updated_by)
SELECT
    9000000001001001 + row_number() OVER (),
    'CLERK',
    m.id,
    0, now(), 0, now(), 0
FROM t_menu m
WHERE m.code IN (
    'home',
    'customer_management', 'customers_list', 'applicants_list',
    'order_group', 'parts_list', 'parts_new', 'delivery_notes_manage',
    'assemblies_list',
    -- 2026-10-05 权限收紧：CLERK 收回 process_work_type / part_process_chain
    -- （配置型菜单，改为 MANAGER 专有），保留 production_group（worker_queue 需要）与
    -- worker_queue 本身。已存在的授权行由 4.7 段显式软删回收，本段只管增量。
    'production_group', 'worker_queue',
    'outsource_list', 'outsource_companies_list', 'outsource_quotes_list', 'outsource_send_receive_list',
    'repair_receive'
)
AND m.deleted_at IS NULL
ON CONFLICT (role, menu_id) WHERE deleted_at IS NULL DO NOTHING;

-- 4.3 INSPECTOR：品检员（无 order_group、auth_group、template_management，但保留外协收发）
INSERT INTO t_role_menu (id, role, menu_id, version, created_at, created_by, updated_at, updated_by)
SELECT
    9000000001002001 + row_number() OVER (),
    'INSPECTOR',
    m.id,
    0, now(), 0, now(), 0
FROM t_menu m
WHERE m.code IN (
    'home',
    'parts_list', 'delivery_notes_manage',
    'assemblies_list',
    -- 2026-10-05 权限收紧：INSPECTOR 收回 production_group 下的 3 个子菜单
    -- （process_work_type / part_process_chain / worker_queue）——前两个是生产管理
    -- **配置型**菜单，worker_queue（生产执行看板）不是配置型，一并收回是因为品检
    -- 不参与生产执行。品检不应看到生产管理的这些入口。
    -- production_group 本身**必须保留**——其子菜单 inspection_pending（待品检）要挂上去，
    -- 丢了会变孤儿节点被 build_menu_tree 提升为顶级。
    'production_group',
    'outsource_send_receive_list',
    'inspection_pending', 'repair_receive'
)
AND m.deleted_at IS NULL
ON CONFLICT (role, menu_id) WHERE deleted_at IS NULL DO NOTHING;

-- 4.4 CNC_PROGRAMMER：编程员（仅首页 + 零件 + 待编程）
INSERT INTO t_role_menu (id, role, menu_id, version, created_at, created_by, updated_at, updated_by)
SELECT
    9000000001003001 + row_number() OVER (),
    'CNC_PROGRAMMER',
    m.id,
    0, now(), 0, now(), 0
FROM t_menu m
WHERE m.code IN ('home', 'parts_list', 'pending_programming')
  AND m.deleted_at IS NULL
ON CONFLICT (role, menu_id) WHERE deleted_at IS NULL DO NOTHING;

-- 4.5 SHELF_ACCOUNT：HMI 一体机扫码台
INSERT INTO t_role_menu (id, role, menu_id, version, created_at, created_by, updated_at, updated_by)
SELECT
    9000000001004001 + row_number() OVER (),
    'SHELF_ACCOUNT',
    m.id,
    0, now(), 0, now(), 0
FROM t_menu m
WHERE m.code IN ('home', 'scan_badge', 'floor_group')
  AND m.deleted_at IS NULL
ON CONFLICT (role, menu_id) WHERE deleted_at IS NULL DO NOTHING;

-- 4.6 清理：seed 删的菜单（settings_root + 3 子菜单 + assemblies_new + 2026-10-08
--     下线的 delivery_dispatch 等）相关 role_menu 一并清
--     （即便菜单软删，role_menu 残留不影响功能，但显式清理便于审计）
DELETE FROM t_role_menu rm
USING t_menu m
WHERE rm.menu_id = m.id
  AND m.code IN ('settings_root', 'work_types_list', 'processes_list', 'work_type_processes_list',
                 'delivery_dispatch')
  AND m.deleted_at IS NOT NULL;

-- ============================================================================
-- 4.7 角色授权回收（2026-10-05 权限收紧：工序工种 / 制定工序改 MANAGER 专有、
--     生产队列改 MANAGER + CLERK，品检三项全收回）
-- ============================================================================
-- ⚠️ 第 4 节的授权是**纯增量**（ON CONFLICT ... DO NOTHING），从白名单里删 code
--    **不会**回收生产库已存在的 t_role_menu 行。收紧权限必须在本节显式列出来。
--    与第 3 节 t_menu soft-delete 同一哲学：变更显式化，不做「白名单之外一律删」。
--
-- 本次收紧（用户需求：工序工种/制定工序 → MANAGER；生产队列 → MANAGER+CLERK；
-- 品检三个都看不到）——覆盖两个角色，不止品检：
--   CLERK     − process_work_type、part_process_chain（CLERK 保留 worker_queue）
--   INSPECTOR − process_work_type、part_process_chain、worker_queue
--   INSPECTOR 仍保留 production_group（其 inspection_pending 子菜单需要）
--
-- 软删而非硬删：留审计，与 t_menu 一致；`AND rm.deleted_at IS NULL` 保证重跑
-- 幂等、不每次启动都 bump version。
-- ⚠️ `version = rm.version + 1` 必须带表名限定：本 UPDATE 带 `FROM t_menu m`，而
--    两张表都有 version 列，不限定会被 PG 判为 ambiguous 直接报错。
--
-- 2026-10-05（review 第 1 轮 NIT-2）：本段**刻意不加** `AND m.deleted_at IS NULL`
--    （4.1-4.5 各段都有，与此处不对称）。原因：① 菜单若已软删，其授权行留着也无害——
--    渲染层 SQL 已按 `m.deleted_at IS NULL` 过滤，且第 4.6 段会硬删 settings_root 等
--    特定 code 的 role_menu 行；② 回收段求的是「最大化覆盖」，宁可多软删一条不可见
--    的授权，也不依赖「菜单永远不会被软删」这个上游不变式。
UPDATE t_role_menu rm
SET deleted_at = now(),
    updated_at = now(),
    version    = rm.version + 1
FROM t_menu m
WHERE rm.menu_id = m.id
  AND rm.deleted_at IS NULL
  AND (rm.role, m.code) IN (
      ('CLERK',     'process_work_type'),
      ('CLERK',     'part_process_chain'),
      ('INSPECTOR', 'process_work_type'),
      ('INSPECTOR', 'part_process_chain'),
      ('INSPECTOR', 'worker_queue')
  );

COMMIT;