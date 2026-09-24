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
    'customer_management', 'order_group', 'pending_programming',
    'production_group', 'template_management', 'auth_group',
    'outsource_list', 'floor_group', 'settings_root',
    -- 子菜单
    'parts_list', 'parts_new', 'assemblies_list', 'delivery_notes_manage',
    'inspection_pending', 'delivery_dispatch', 'repair_receive',
    'workers_list', 'users_list', 'shelves_list',
    'customers_list', 'applicants_list',
    'outsource_companies_list', 'outsource_quotes_list', 'outsource_send_receive_list',
    'process_work_type', 'part_process_chain', 'worker_queue',
    'print_templates_designer'
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
    (9000000000000006, NULL, 'pending_programming',  '待编程一览', '/cnc/pending',        'Cpu',            25, true, 0, now(), 0, now(), 0),
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
    (9000000000000105, (SELECT id FROM t_menu WHERE code = 'order_group'         AND deleted_at IS NULL), 'inspection_pending',        '待品检',        '/inspection/pending',          'CircleCheck',  50, true, 0, now(), 0, now(), 0),
    (9000000000000106, (SELECT id FROM t_menu WHERE code = 'order_group'         AND deleted_at IS NULL), 'delivery_dispatch',         '送货',          '/delivery-dispatch',           'Van',          60, true, 0, now(), 0, now(), 0),
    (9000000000000107, (SELECT id FROM t_menu WHERE code = 'order_group'         AND deleted_at IS NULL), 'repair_receive',            '返修接收',      '/repair/receive',              'Tools',        65, true, 0, now(), 0, now(), 0),

    -- auth_group 下（含 025 把 shelves_list 从 floor_group 移入）
    (9000000000000201, (SELECT id FROM t_menu WHERE code = 'auth_group'         AND deleted_at IS NULL), 'workers_list',              '工人一览',      '/workers',                     'User',         10, true, 0, now(), 0, now(), 0),
    (9000000000000202, (SELECT id FROM t_menu WHERE code = 'auth_group'         AND deleted_at IS NULL), 'users_list',                '账号管理',      '/users',                       'List',         20, true, 0, now(), 0, now(), 0),
    (9000000000000203, (SELECT id FROM t_menu WHERE code = 'auth_group'         AND deleted_at IS NULL), 'shelves_list',              '货架管理',      '/shelves',                     'Platform',     30, true, 0, now(), 0, now(), 0),

    -- customer_management 下
    (9000000000000401, (SELECT id FROM t_menu WHERE code = 'customer_management' AND deleted_at IS NULL), 'customers_list',         '客户一览',      '/customers',                   'Connection',   10, true, 0, now(), 0, now(), 0),
    (9000000000000402, (SELECT id FROM t_menu WHERE code = 'customer_management' AND deleted_at IS NULL), 'applicants_list',        '申请人一览',    '/applicants',                  'User',         20, true, 0, now(), 0, now(), 0),

    -- outsource_list 下
    (9000000000000501, (SELECT id FROM t_menu WHERE code = 'outsource_list'    AND deleted_at IS NULL), 'outsource_companies_list',  '外协厂一览',    '/outsource/companies',         'OfficeBuilding', 10, true, 0, now(), 0, now(), 0),
    (9000000000000502, (SELECT id FROM t_menu WHERE code = 'outsource_list'    AND deleted_at IS NULL), 'outsource_quotes_list',     '报价一览',      '/outsource/quotes',            'Document',     20, true, 0, now(), 0, now(), 0),
    (9000000000000503, (SELECT id FROM t_menu WHERE code = 'outsource_list'    AND deleted_at IS NULL), 'outsource_send_receive_list','外协发送/接收','/outsource/send-receive',       'Promotion',    30, true, 0, now(), 0, now(), 0),

    -- production_group 下（018/021/024 累计）
    (9000000000000601, (SELECT id FROM t_menu WHERE code = 'production_group'  AND deleted_at IS NULL), 'process_work_type',         '工序工种',      '/production/process-work-type','Operation',     5, true, 0, now(), 0, now(), 0),
    (9000000000000602, (SELECT id FROM t_menu WHERE code = 'production_group'  AND deleted_at IS NULL), 'part_process_chain',        '制定工序',      '/production/process-design',   'SetUp',        10, true, 0, now(), 0, now(), 0),
    (9000000000000603, (SELECT id FROM t_menu WHERE code = 'production_group'  AND deleted_at IS NULL), 'worker_queue',              '生产队列',      '/workers/queue',              'Operation',    20, true, 0, now(), 0, now(), 0),

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
    'inspection_pending', 'delivery_dispatch', 'repair_receive',
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
    'production_group', 'process_work_type', 'part_process_chain', 'worker_queue',
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
    'production_group', 'process_work_type', 'part_process_chain', 'worker_queue',
    'outsource_send_receive_list',
    'inspection_pending', 'delivery_dispatch', 'repair_receive'
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

-- 4.6 清理：seed 删的菜单（settings_root + 3 子菜单 + assemblies_new 等）相关 role_menu 一并清
--     （即便菜单软删，role_menu 残留不影响功能，但显式清理便于审计）
DELETE FROM t_role_menu rm
USING t_menu m
WHERE rm.menu_id = m.id
  AND m.code IN ('settings_root', 'work_types_list', 'processes_list', 'work_type_processes_list')
  AND m.deleted_at IS NOT NULL;

COMMIT;