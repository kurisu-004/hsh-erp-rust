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
--   * t_role_menu.id 用**派生 ID**（9100000000000xxx 段，见第 4 节头注释）——
--     绝不能用 `基址 + row_number() OVER ()`：那是"按行位置"发号，白名单里
--     加/删任何一个 code，后面所有 code 的位置全部位移，新行就撞上别人已占的
--     `t_role_menu_pkey`（2026-10-13 加 mail_group/mail_digest 时真实炸过一次）。
--     `ON CONFLICT (role, menu_id) WHERE deleted_at IS NULL` 只兜 partial 唯一索引，
--     兜不住 PK —— PK 是另一个 arbiter，撞了直接终止整条 INSERT
--   * t_role_menu 的授权是「进白名单即生效」：第 4.0 段先把白名单内已软删的授权
--     **复活**，再由 4.1-4.5 补真正缺失的行。收紧权限才走 4.7 段显式回收。
--   * 不自动删"未在本文件声明的菜单"——菜单下线必须**显式**列在第 3 节
--     "soft-delete 区段"（防止误删生产手工加的菜单）
--
-- 修改菜单 = 改本文件 + 重启 app（或 psql 手工跑一次）。不需要新 migration，
-- 不需要管 _sqlx_migrations.checksum。
--
-- 域内校验：
--   * t_menu.code 唯一（uk_t_menu_code WHERE deleted_at IS NULL）
--   * t_role_menu (role, menu_id) 唯一（uk_t_role_menu_role_menu WHERE deleted_at IS NULL）
--   * t_role_menu.id 是 PRIMARY KEY（t_role_menu_pkey），**不在**上面那条
--     partial 唯一索引的覆盖范围内 —— 派生 ID 时必须保证全局不撞
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
    -- 2026-10-13 新增：交期汇总邮件（mail_group 顶级 + mail_digest 子菜单）。
    -- 放进复活清单的理由同上：软删态下不会被 uk_t_menu_code 的 partial 索引命中，
    -- ON CONFLICT 失效后会直接 INSERT 撞 PK。
    'mail_group', 'mail_digest'
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
    -- 2026-10-13 新增：交期汇总邮件的一级菜单。sort_order=45 —— 刻意插在
    -- floor_group(40) 与 settings_root(50) 之间：一级菜单从左到右按 sort_order
    -- 排，「车间」是生产执行侧入口、「设置」是系统配置侧入口，邮件定时任务属于
    -- 两者之间的「运维自动化」，放 45 会正好卡在两组之间，两边都不挤。
    (9000000000000013, NULL, 'mail_group',           '定时任务/邮件', NULL,                'Clock',          45, true, 0, now(), 0, now(), 0),
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
    -- 换父分组不改授权：t_role_menu 仍是扁平 code 列表，workers_list 仅 MANAGER 持有。
    (9000000000000201, (SELECT id FROM t_menu WHERE code = 'production_group'  AND deleted_at IS NULL), 'workers_list',              '工人一览',      '/production/worker-list',     'User',         25, true, 0, now(), 0, now(), 0),
    -- 2026-09-29 新增：待品检（inspection_pending）从 order_group 移到 production_group；
    -- 原 id（9000000000000105）保留；sort_order 从 50 改为 30（在 worker_queue 之后）。
    (9000000000000105, (SELECT id FROM t_menu WHERE code = 'production_group'  AND deleted_at IS NULL), 'inspection_pending',        '待品检',        '/inspection/pending',          'CircleCheck',  30, true, 0, now(), 0, now(), 0),

    -- 2026-10-13 新增：mail_group 下。交期汇总邮件（域 mail_digest，6 端点
    -- 全部 MANAGER 独占）。path 与前端视图名一致（/mail/digest）。
    (9000000000000901, (SELECT id FROM t_menu WHERE code = 'mail_group'       AND deleted_at IS NULL), 'mail_digest',               '交期汇总邮件',  '/mail/digest',                 'Message',     10, true, 0, now(), 0, now(), 0),

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
--
-- ---------------------------------------------------------------------------
-- 【发号规则】t_role_menu.id 必须由「授权身份 (role, code)」唯一决定
-- ---------------------------------------------------------------------------
--   9100000000000000
--     + <角色序号> * 1000000000000
--     + md5('t_role_menu:' || role || ':' || code) 的低 48 bit
--
-- 角色序号：MANAGER=0 / CLERK=1 / INSPECTOR=2 / CNC_PROGRAMMER=3 / SHELF_ACCOUNT=4。
--
-- 为什么不能沿用旧写法 `900000000100{role}001 + row_number() OVER ()`：
--   ① row_number() OVER () **没有 ORDER BY**，PG 不保证行序 —— 实测走 Seq Scan
--      按物理序返回（EXPLAIN 确认），VACUUM / 换执行计划 / pg_restore 重建后
--      物理序就变，发号结果随之变。
--   ② 更致命的是「位置」本身会随白名单增删而**整体位移**：白名单里加一个 code，
--      排在它后面的每个 code 的序号都 +1，于是新行拿到别人的 id。
--   ③ `ON CONFLICT (role, menu_id) WHERE deleted_at IS NULL` 只声明了
--      uk_t_role_menu_role_menu 这一个 arbiter；`t_role_menu_pkey` 是另一个，
--      撞它不会被 DO NOTHING 吞掉，整条 INSERT 直接失败 → 整个 seed 回滚。
--
-- 2026-10-13 实证：4.1 MANAGER 白名单加 mail_group/mail_digest，两行按物理序
-- 落到 9000000001000030 / ...031，而 030 已被 MANAGER/workers_list 占用 →
-- `duplicate key value violates unique constraint "t_role_menu_pkey"`，
-- BEGIN/COMMIT 整段回滚，mail_group / mail_digest 连 t_menu 都没插进去。
--
-- 派生 ID 的性质：与白名单顺序、增删、其他角色**完全无关**，只由 (role, code)
-- 决定，因此反复跑 / 中途增删 code 都不会撞号。现有组合已实测
-- seed 内部 0 重复、与存量 0 撞号（tests/iam/menu_seed.rs 用例 ⑥ 每次运行复核）。md5 碰撞概率 ~1e-11，且真撞了是硬报错
-- （不会静默丢授权）—— 这是想要的行为。
--
-- ID 段物理不相交（避免与存量行、其它 ID 空间打架）：
--   - 9100000000000xxx   本文件派生段（~9.1e15）
--   - 9000000001000xxx   2026-10-13 之前的旧 seed 静态段（存量行仍用，不动）
--   - 9000000000000xxx   t_menu 静态段（~9.0e15）
--   - ≥ 2.07e17          生产雪花 ID / fixture ID（~2.2e17 与 9.0e18）

-- ---------------------------------------------------------------------------
-- 4.0 复活已软删的授权行（2026-10-13 新增）
-- ---------------------------------------------------------------------------
-- 为什么需要：uk_t_role_menu_role_menu 是 **partial** 唯一索引
-- （`WHERE deleted_at IS NULL`）。一条被 4.7 段软删（deleted_at NOT NULL）的
-- 授权行不在该索引里 ⇒ 4.1-4.5 的 `ON CONFLICT (role, menu_id) WHERE
-- deleted_at IS NULL` **命中不到它** ⇒ 将来把这个 code 加回白名单时会插出
-- **第二行**，旧行还留着，授权簿记翻倍、审计对不上账。
--
-- 语义对称于第 0 节对 t_menu 的处理：「进白名单 = 生效；下线 = 显式回收」。
-- 所以先复活（把白名单内已软删的行拉回 live，version + 1），再由 4.1-4.5
-- 只补真正缺失的行。
--
-- 顺序要求：本段必须在 4.7 回收段**之前**跑完 —— 一个 code 同时在白名单和
-- 4.7 回收名单里的话，语义上「回收」是后手、最终应为软删。
--
-- `AND rm.deleted_at IS NOT NULL` 是幂等守卫：缺了它每次重跑都会给已 live 的
-- 行 bump version（与 4.7 段的 `AND rm.deleted_at IS NULL` 同一套道理）。
-- ---------------------------------------------------------------------------
UPDATE t_role_menu rm
SET deleted_at = NULL,
    updated_at = now(),
    version    = rm.version + 1
FROM t_menu m
WHERE rm.menu_id = m.id
  AND rm.deleted_at IS NOT NULL
  AND (rm.role, m.code) IN (
      -- 以下五组逐条镜像 4.1-4.5 段白名单（改白名单必须同步改这里）
      -- MANAGER（对应 4.1）
      ('MANAGER', 'home'),
      ('MANAGER', 'production_stats'),
      ('MANAGER', 'customer_management'),
      ('MANAGER', 'customers_list'),
      ('MANAGER', 'applicants_list'),
      ('MANAGER', 'order_group'),
      ('MANAGER', 'parts_list'),
      ('MANAGER', 'parts_new'),
      ('MANAGER', 'delivery_notes_manage'),
      ('MANAGER', 'inspection_pending'),
      ('MANAGER', 'repair_receive'),
      ('MANAGER', 'assemblies_list'),
      ('MANAGER', 'pending_programming'),
      ('MANAGER', 'production_group'),
      ('MANAGER', 'process_work_type'),
      ('MANAGER', 'part_process_chain'),
      ('MANAGER', 'worker_queue'),
      ('MANAGER', 'template_management'),
      ('MANAGER', 'print_templates_designer'),
      ('MANAGER', 'auth_group'),
      ('MANAGER', 'workers_list'),
      ('MANAGER', 'users_list'),
      ('MANAGER', 'roles_list'),
      ('MANAGER', 'shelves_list'),
      ('MANAGER', 'outsource_list'),
      ('MANAGER', 'outsource_companies_list'),
      ('MANAGER', 'outsource_quotes_list'),
      ('MANAGER', 'outsource_send_receive_list'),
      ('MANAGER', 'floor_group'),
      ('MANAGER', 'settings_root'),
      ('MANAGER', 'mail_group'),
      ('MANAGER', 'mail_digest'),
      -- CLERK（对应 4.2）
      ('CLERK', 'home'),
      ('CLERK', 'customer_management'),
      ('CLERK', 'customers_list'),
      ('CLERK', 'applicants_list'),
      ('CLERK', 'order_group'),
      ('CLERK', 'parts_list'),
      ('CLERK', 'parts_new'),
      ('CLERK', 'delivery_notes_manage'),
      ('CLERK', 'assemblies_list'),
      ('CLERK', 'production_group'),
      ('CLERK', 'worker_queue'),
      ('CLERK', 'outsource_list'),
      ('CLERK', 'outsource_companies_list'),
      ('CLERK', 'outsource_quotes_list'),
      ('CLERK', 'outsource_send_receive_list'),
      ('CLERK', 'repair_receive'),
      -- INSPECTOR（对应 4.3）
      ('INSPECTOR', 'home'),
      ('INSPECTOR', 'parts_list'),
      ('INSPECTOR', 'delivery_notes_manage'),
      ('INSPECTOR', 'assemblies_list'),
      ('INSPECTOR', 'production_group'),
      ('INSPECTOR', 'outsource_send_receive_list'),
      ('INSPECTOR', 'inspection_pending'),
      ('INSPECTOR', 'repair_receive'),
      -- CNC_PROGRAMMER（对应 4.4）
      ('CNC_PROGRAMMER', 'home'),
      ('CNC_PROGRAMMER', 'parts_list'),
      ('CNC_PROGRAMMER', 'pending_programming'),
      -- SHELF_ACCOUNT（对应 4.5）
      ('SHELF_ACCOUNT', 'home'),
      ('SHELF_ACCOUNT', 'scan_badge'),
      ('SHELF_ACCOUNT', 'floor_group')
  );

-- 4.1 MANAGER：除「扫码台」（HMI 专用）外几乎全权
INSERT INTO t_role_menu (id, role, menu_id, version, created_at, created_by, updated_at, updated_by)
SELECT
    9100000000000000 + 0 * 1000000000000
        + (('x' || substr(md5('t_role_menu:MANAGER:' || m.code), 1, 15))::bit(60)::bigint % 1000000000000),
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
    -- 2026-10-14 新增：RBAC 角色面板。**仅 MANAGER** —— 它的 8 条端点全部
    -- require_role Manager 硬切（面板是权限真源，用权限点守它会自锁），
    -- 菜单可见性必须跟端点守卫逐格一致。
    'roles_list',
    'outsource_list', 'outsource_companies_list', 'outsource_quotes_list', 'outsource_send_receive_list',
    'floor_group', 'settings_root',
    -- 2026-10-13 新增：交期汇总邮件。**仅 MANAGER** —— 4.2 CLERK / 4.3 INSPECTOR /
    -- 4.4 CNC_PROGRAMMER / 4.5 SHELF_ACCOUNT 四段**都不加**这两个 code。理由与
    -- 2026-10-05 收紧 process_work_type / part_process_chain 同款：邮件定时任务
    -- 会**对真实收件人发信**，属配置型 + 有外发副作用的操作，只归管理员。
    'mail_group', 'mail_digest'
)
AND m.deleted_at IS NULL
ON CONFLICT (role, menu_id) WHERE deleted_at IS NULL DO NOTHING;

-- 4.2 CLERK：业务操作员（无 auth_group 域、无 template_management）
INSERT INTO t_role_menu (id, role, menu_id, version, created_at, created_by, updated_at, updated_by)
SELECT
    9100000000000000 + 1 * 1000000000000
        + (('x' || substr(md5('t_role_menu:CLERK:' || m.code), 1, 15))::bit(60)::bigint % 1000000000000),
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
    9100000000000000 + 2 * 1000000000000
        + (('x' || substr(md5('t_role_menu:INSPECTOR:' || m.code), 1, 15))::bit(60)::bigint % 1000000000000),
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
    9100000000000000 + 3 * 1000000000000
        + (('x' || substr(md5('t_role_menu:CNC_PROGRAMMER:' || m.code), 1, 15))::bit(60)::bigint % 1000000000000),
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
    9100000000000000 + 4 * 1000000000000
        + (('x' || substr(md5('t_role_menu:SHELF_ACCOUNT:' || m.code), 1, 15))::bit(60)::bigint % 1000000000000),
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
-- ⚠️ 4.0 复活段会把白名单内的授权拉回 live，其中包含 ('MANAGER','settings_root')。
--    但 §3.1 已把该菜单软删 ⇒ 4.1 的 `m.deleted_at IS NULL` 过滤掉它，不会重插；
--    本段的硬删负责把存量行（含被 4.0 复活的那一条）清干净。同事务内完成，
--    净效果仍是「这些授权不存在」，无残留。
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
--
-- 2026-10-13：4.0 复活段引入后，本段与它构成一对「复活 / 回收」，**顺序不可调换**
--    —— 4.0 在前、4.7 在后，于是「既在 4.1-4.5 白名单里、又在 4.7 回收名单里」的
--    code 最终状态是软删（回收是后手）。当前两份名单无交集，此说明只是把隐含契约
--    写明，防止将来有人调换段落顺序。
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