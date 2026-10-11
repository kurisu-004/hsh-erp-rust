-- ============================================================================
-- seeds/rbac.sql —— RBAC 基线种子：内置角色 + 权限点 + 系统角色授权基线
-- ============================================================================
-- 2026-10-14 新增（iam::rbac 子模块配套）。
-- 2026-10-15 第二批 17 个权限点（生产流转 + 外协 + 送货单 / 品检读），见第 2 节；
-- 同批把 `t_role_menu` 角色→菜单基线自 seeds/menu.sql 迁入本文件（§4）。
--
-- ## 与 seeds/menu.sql 的分工
--   * menu.sql 管「**菜单目录**」：t_menu 菜单树（部署配置，与前端路由耦合）；
--   * 本文件管「**角色 + 权限点 + 授权基线**」：t_role 角色定义 + t_permission
--     权限点 + t_role_permission 角色→权限点基线 + t_role_menu 角色→菜单
--     **首次基线**（§4）。
-- 两者都是声明式、幂等、启动钩子自动跑（应用顺序 menu → rbac → [admin]，见
-- `src/infra/seed.rs`）。菜单与权限点是两个独立的授权维度，不要互相推算。
--
-- ## 幂等语义（连跑两次结果一致、不报错）
--   * t_role / t_permission：`ON CONFLICT (code) WHERE deleted_at IS NULL
--     DO NOTHING`。冲突目标必须带 `WHERE deleted_at IS NULL` —— 与 migration
--     里的 partial 唯一索引 `uk_t_role_code` / `uk_t_permission_code` 逐字对应，
--     少了它 PG 不会把该索引当 arbiter，重跑直接报错。
--     DO NOTHING（而非 DO UPDATE）是有意的：管理端可以改系统角色的中文名 /
--     描述 / 启停状态，重跑 seed 不得把这些修改覆盖回基线。插入只负责「从无到有」。
--   * t_role_permission / t_role_menu：`WHERE NOT EXISTS` + `ON CONFLICT ...
--     DO NOTHING`。只补「该 (role, 授权对象) 从未有过任何行（含软删）」的组合，
--     **不复活**管理端软删过的授权行 —— 撤销授权必须在重启后仍然生效。
--     `NOT EXISTS` 还挡住另一种崩溃：同一组合的旧行已被软删（不在 partial 唯一
--     索引里，ON CONFLICT 命中不到），带静态 id 的 INSERT 会撞 PK；先判「这张表
--     里是否已有该组合」即绕开。
--
-- ## t_role_menu 基线语义（2026-10-15 接管）
--   * RBAC 面板（`POST /iam/roles/{id}/grants` 整包替换）是角色→菜单授权的唯一
--     持续管理者；seed 只负责**首次基线**：仅当该 (role, menu_id) 从未有过任何行
--     （含软删）时才补一条基线行。
--   * 面板的增 / 删重启后全部保留：seed 既不复活软删行，也不回补被移除的组合。
--
-- ## ID 段（与其它种子 / 生产雪花物理不相交）
--   * 9200000000000000+  本文件 t_role
--   * 9300000000000000+  本文件 t_permission
--   * 9400000000000000+  本文件 t_role_permission
--   * 9100000000000xxx   本文件 t_role_menu 派生段
--   * 9000000000000xxx   seeds/menu.sql 的 t_menu 静态段
--   * 900000000000000001 seeds/admin.sql
--   * ≥ 2.07e17          生产雪花 ID / fixture ID
--
-- ## ADMIN 元角色**不写任何授权行**
--   * `t_role_permission` 不写 ADMIN：通配在 `CurrentUser::require_permission`
--     里短路（`has_role(Role::Admin)` 恒真），写进去会把「元角色」降级成
--     「逐项勾选」，漏改一行即静默失效；
--   * `t_role_menu` 同样不写 ADMIN（见第 4 节）：ADMIN 的菜单由
--     `AccountService::menus_for_roles` / `RbacService::list_menus` 走全量查询。
--
-- ## `t_role.code` 与 Rust `Role` 枚举是两张表
-- 本文件的 6 个 code 与 `auth::rbac::Role` 枚举一一对应（枚举只覆盖这 6 项）；
-- 管理端新建的自定义角色只进 `t_role.code` varchar（并入 `t_user_role.role`），
-- **不进枚举** —— 授权判定时 `parse_role_string` 对它们返回 None，登录态角色
-- 列表走原始字符串。
-- ============================================================================

BEGIN;

-- ---------------------------------------------------------------------------
-- 1. t_role：内置角色（恰 6 行，is_system = true）
-- ---------------------------------------------------------------------------
-- 管理端可停用系统角色（ADMIN / MANAGER 除外 —— 两者停用 / 删除固定 400，
-- 见 rbac::service 模块头）、不可删除、不可改 code（RbacService::update_role 硬切）。
-- name / description 是角色面板展示文案，改这里对已存在的库不生效（DO NOTHING）。
-- 6 行与 Rust `Role` 枚举一一对应：ADMIN / MANAGER / CLERK / INSPECTOR /
-- CNC_PROGRAMMER / SHELF_ACCOUNT。
INSERT INTO t_role (
    id, code, name, description, is_system, is_active,
    version, created_at, created_by, updated_at, updated_by
) VALUES
    (9200000000000001, 'ADMIN',          '超级管理员', '元角色：代码级通配全部菜单与权限，不写授权数据；后端拒写其 grants', true, true, 0, now(), 0, now(), 0),
    (9200000000000002, 'MANAGER',        '管理员',     '日常管理：账号、角色面板、生产配置与全部业务操作',                   true, true, 0, now(), 0, now(), 0),
    (9200000000000003, 'CLERK',          '文员',       '业务操作：零件、送货单、外协收发、生产队列下发',                     true, true, 0, now(), 0, now(), 0),
    (9200000000000004, 'INSPECTOR',      '品检员',     '品检流转：待品检、外协收发、图纸打印',                               true, true, 0, now(), 0, now(), 0),
    (9200000000000005, 'CNC_PROGRAMMER', 'CNC 程序员', '待编程处理与图纸查看',                                               true, true, 0, now(), 0, now(), 0),
    (9200000000000006, 'SHELF_ACCOUNT',  '货架一体机', '扫码台专用账号，必须绑定具体货架',                                   true, true, 0, now(), 0, now(), 0)
ON CONFLICT (code) WHERE deleted_at IS NULL DO NOTHING;

-- ---------------------------------------------------------------------------
-- 2. t_permission：权限点（按钮级操作码）
-- ---------------------------------------------------------------------------
-- 点分形态 `<域>.<操作>`。权限点的角色矩阵（第 3 节）逐格照抄迁移前的
-- 端点守卫 / 前端放行集合，不新增也不收紧：
--   * `part.drawing.print`  ← part::handler::print 原 require_any_role
--     （MANAGER / CLERK / INSPECTOR / CNC_PROGRAMMER；CNC 岗要看图纸编程序）；
--     2026-10-14 同点接管 part_file::service::get_file_with_url（图纸下载直链，
--     原白名单逐格相同；cnc-programs/{id}/download-url alias 随之一并走点）
--   * `queue.dispatch`      ← prod::queue::service::dispatch::dispatch_batch 原
--     require_any_role（MANAGER / CLERK）；auto-dispatch 预览 2026-10-15 回改并入
--     同点（见 docs/api/queue.md §1）；list_pending 归第二批 `queue.view`
--   * `delivery_note.print` ← **前端-only** 权限点：后端打印端点已于 2026-10-08
--     下线（打印改前端本地 hucre 生成），后端零守卫可迁；矩阵照现有前端
--     `frontend/src/utils/deliveryNotePermissions.ts::canPrint` 的放行集合
--     （MANAGER / CLERK / INSPECTOR）登记，供前端 hasPermission 使用。
--
-- 2026-10-15 第二批 17 点（用户定稿矩阵，与第 3 节逐格对应）：
--   * `production.view`           ← work_type / process / shelf_process 三个域的
--     读守卫（M,C,CNC,SA,I）与 process_chain 的 `require_any_read`
--     （M,C,I,CNC）合并为一个读点；**SA 并入是有意并集**（后者的原集合没有
--     SA，统一后多了它，用户定稿）。
--   * `production.config`         ← `POST /prod/work-types/{id}/processes` 与
--     `POST /prod/shelf-processes/{shelf_id}` 两条**原先零守卫**的配置写路径
--     补闸（M）。
--   * `queue.view`                ← list_pending / 队列板 snapshot + 工序详情
--     （M,C,I，原 require_any_role 逐格相同；auto-dispatch 预览**不**归本点，
--     2026-10-15 回改同 `queue.dispatch`，见 docs/api/queue.md §1）。
--   * `queue.recall`              ← recall_to_pending（M,C，原集合相同）。
--   * `queue.allocate`            ← refill / move_batch / auto_allocate_for_process
--     三条**原 Manager 独占**的人工投放写路径；⚠️ **有意放宽为 M,C**（用户
--     定稿，原 require_role(Manager) 放开给 Clerk）。
--   * `batch.split` / `batch.cancel` / `batch.deliver` ← 批次结构操作与终态
--     （M,C，原集合相同；`complete` 含后台 auto_complete 的系统身份，
--     见 `task::auto_complete::system_current_user` 的 permissions）。
--   * `batch.transition`          ← to-ship / to-process / scan-inspect /
--     to-inspection 单件+批量（M,I，原 TO_XXX_ROLES 逐格相同）。
--   * `batch.repair`              ← 返修读两条 + complete-repair /
--     repair-dispatch / start-repair（M,C,I，原集合相同）。
--   * `inspection.view`           ← 待品检 scan / queue（M,I，原 READ_ROLES 相同）。
--   * `delivery_note.view`        ← 送货单列表 `GET /com/delivery/note` 原为
--     M,C,I,CNC；⚠️ **有意收紧去 CNC**（用户定稿）；详情 / batch-detail 两条
--     原零守卫，补闸后并入本点。
--   * `outsource.view`            ← 外协看板读 + 报价读（M,C,I，原集合相同）+
--     公司读原为 M,C,CNC,I；⚠️ **公司读有意收紧去 CNC**（用户定稿）。
--   * `outsource.quote.edit`      ← 报价建 / 可报价零件 / 提交 / 删（M,C，原
--     集合相同；可报价零件列表并入本点是报价工作流的辅助读，不推动
--     `outsource.view` 的角色集合）。
--   * `outsource.quote.approve`   ← approve / reject（M，原 require_role(Manager)
--     逐格相同）。
--   * `outsource.move`            ← `POST /outsource-queue/move` 发送+回收单点
--     （M,C,I，原集合相同）。
--   * `outsource.company.manage`  ← 公司建 / 改 / 删（M,C，原集合相同）。
--
-- 明确**不迁**（保持现状，后续批次处理）：`place-on-shelf` /
-- `release-from-programming`（废弃端点）；`scan/deliver` / `worker-scan` /
-- `pick-up` / `verify-badge`（设备流）；RBAC 8 端点（`require_role(Manager)`
-- 设计如此）；dashboard / wx / statistics / mail_digest / iam 其余。
INSERT INTO t_permission (
    id, code, name, domain, description, is_active,
    version, created_at, created_by, updated_at, updated_by
) VALUES
    (9300000000000001, 'part.drawing.print',  '零件图纸打印', 'part',          '零件图纸打印转发端点 + 图纸下载直链（part-files/{id}/url）', true, 0, now(), 0, now(), 0),
    (9300000000000002, 'queue.dispatch',      '生产队列下发', 'queue',         '待下发批次的批量下发写路径',                     true, 0, now(), 0, now(), 0),
    (9300000000000003, 'delivery_note.print', '送货单打印',   'delivery_note', '前端本地生成送货单打印件（后端无打印端点，仅登记）', true, 0, now(), 0, now(), 0),
    -- ---- 2026-10-15 第二批 17 点 ----
    (9300000000000004, 'production.view',           '生产基础数据查看', 'production',    '工序 / 工种 / 货架工序映射 / 工艺链的只读端点（用户定稿：SA 并入是有意并集）', true, 0, now(), 0, now(), 0),
    (9300000000000005, 'production.config',         '生产基础数据配置', 'production',    '工种↔工序、货架↔工序映射的整组替换写路径', true, 0, now(), 0, now(), 0),
    (9300000000000006, 'queue.view',                '生产队列查看',     'queue',         '待下发列表 / 队列板聚合（snapshot + 工序详情）', true, 0, now(), 0, now(), 0),
    (9300000000000007, 'queue.recall',              '生产队列召回',     'queue',         '待下发召回（IN_PROCESS / PROGRAMMING → PENDING）', true, 0, now(), 0, now(), 0),
    (9300000000000008, 'queue.allocate',            '生产队列投放',     'queue',         'refill / move / auto-allocate 三条人工投放写路径（用户定稿：自 Manager 放宽至 Clerk）', true, 0, now(), 0, now(), 0),
    (9300000000000009, 'batch.split',               '批次拆分',         'batch',         '顶层共用端点 POST /api/v2/batches/split', true, 0, now(), 0, now(), 0),
    (9300000000000010, 'batch.transition',          '批次流转',         'batch',         'to-ship / to-process / scan-inspect / to-inspection（单件 + 批量）', true, 0, now(), 0, now(), 0),
    (9300000000000011, 'batch.deliver',             '批次交付',         'batch',         'deliver / complete（含后台 auto_complete 的系统身份）', true, 0, now(), 0, now(), 0),
    (9300000000000012, 'batch.cancel',              '批次取消',         'batch',         '批次级取消（/{id}/cancel）', true, 0, now(), 0, now(), 0),
    (9300000000000013, 'batch.repair',              '批次返修',         'batch',         '返修读两条 + start-repair / complete-repair / repair-dispatch', true, 0, now(), 0, now(), 0),
    (9300000000000014, 'inspection.view',           '待品检查看',       'inspection',    '待品检队列 + 扫码三层树', true, 0, now(), 0, now(), 0),
    (9300000000000015, 'delivery_note.view',        '送货单查看',       'delivery_note', '列表 / 详情 / 批量详情（用户定稿：列表收紧去 CNC）', true, 0, now(), 0, now(), 0),
    (9300000000000016, 'outsource.view',            '外协查看',         'outsource',     '外协看板读 + 报价读 + 公司读（用户定稿：公司读收紧去 CNC）', true, 0, now(), 0, now(), 0),
    (9300000000000017, 'outsource.quote.edit',      '外协报价编辑',     'outsource',     '报价建 / 可报价零件 / 提交 / 删', true, 0, now(), 0, now(), 0),
    (9300000000000018, 'outsource.quote.approve',   '外协报价审批',     'outsource',     '报价 approve / reject', true, 0, now(), 0, now(), 0),
    (9300000000000019, 'outsource.move',            '外协收发货',       'outsource',     'POST /outsource-queue/move（发送 + 回收同一端点）', true, 0, now(), 0, now(), 0),
    (9300000000000020, 'outsource.company.manage',  '外协公司管理',     'outsource',     '外协公司建 / 改 / 删', true, 0, now(), 0, now(), 0)
ON CONFLICT (code) WHERE deleted_at IS NULL DO NOTHING;

-- ---------------------------------------------------------------------------
-- 3. t_role_permission：系统角色 → 权限点授权矩阵
-- ---------------------------------------------------------------------------
-- 逐格等于迁移前的角色守卫集合（见第 2 节注释），SHELF_ACCOUNT 除
-- `production.view` 外全不授；ADMIN 不写行（元角色通配）。第二批 17 点里
-- 三处与迁移前守卫**有意不同**（用户定稿）：
--   * `queue.allocate` 由 Manager 独占放宽为 M,C（refill / move / auto-allocate）；
--   * `delivery_note.view` 去 CNC（列表原 M,C,I,CNC）、`outsource.view` 的公司读
--     去 CNC（原 M,C,CNC,I）；
--   * `production.view` 是两条读集合的并集（work_type/process/shelf_process 原
--     带 SA、process_chain 原不带），故对 process_chain 而言多授 SA。
--
-- `WHERE NOT EXISTS` + `ON CONFLICT ... DO NOTHING`：只补缺失的基线行，**不复活**
-- 管理端已撤销的授权（撤销必须在重启后仍然生效），也避开「已软删行仍在、静态 id
-- 撞 PK」的坑（partial 唯一索引命中不到软删行的说明见文件头）。
INSERT INTO t_role_permission (
    id, role_code, permission_code, version, created_at, created_by, updated_at, updated_by
)
SELECT v.id, v.role_code, v.permission_code, 0, now(), 0, now(), 0
FROM (VALUES
    -- part.drawing.print：照 print.rs / get_file_with_url 原 require_any_role 的角色集合（两处逐格相同）
    (9400000000000001::bigint, 'MANAGER',        'part.drawing.print'),
    (9400000000000002::bigint, 'CLERK',          'part.drawing.print'),
    (9400000000000003::bigint, 'INSPECTOR',      'part.drawing.print'),
    (9400000000000004::bigint, 'CNC_PROGRAMMER', 'part.drawing.print'),
    -- queue.dispatch：照 dispatch_batch 原 require_any_role 的角色集合
    (9400000000000005::bigint, 'MANAGER',        'queue.dispatch'),
    (9400000000000006::bigint, 'CLERK',          'queue.dispatch'),
    -- delivery_note.print：前端-only，矩阵照前端 canPrint 放行集合
    (9400000000000007::bigint, 'MANAGER',        'delivery_note.print'),
    (9400000000000008::bigint, 'CLERK',          'delivery_note.print'),
    (9400000000000009::bigint, 'INSPECTOR',      'delivery_note.print'),
    -- ---- 2026-10-15 第二批 17 点（id 顺序照第 2 节的点序逐格展开）----
    -- production.view：work_type / process / shelf_process 读（M,C,CNC,SA,I）
    (9400000000000010::bigint, 'MANAGER',        'production.view'),
    (9400000000000011::bigint, 'CLERK',          'production.view'),
    (9400000000000012::bigint, 'CNC_PROGRAMMER', 'production.view'),
    (9400000000000013::bigint, 'SHELF_ACCOUNT',  'production.view'),
    (9400000000000014::bigint, 'INSPECTOR',      'production.view'),
    -- production.config：两条配置写路径补闸（M）
    (9400000000000015::bigint, 'MANAGER',        'production.config'),
    -- queue.view：list_pending + 队列板（M,C,I）
    (9400000000000016::bigint, 'MANAGER',        'queue.view'),
    (9400000000000017::bigint, 'CLERK',          'queue.view'),
    (9400000000000018::bigint, 'INSPECTOR',      'queue.view'),
    -- queue.recall：召回（M,C）
    (9400000000000019::bigint, 'MANAGER',        'queue.recall'),
    (9400000000000020::bigint, 'CLERK',          'queue.recall'),
    -- queue.allocate：refill / move / auto-allocate（用户定稿放宽 M→C）
    (9400000000000021::bigint, 'MANAGER',        'queue.allocate'),
    (9400000000000022::bigint, 'CLERK',          'queue.allocate'),
    -- batch.split：拆批（M,C）
    (9400000000000023::bigint, 'MANAGER',        'batch.split'),
    (9400000000000024::bigint, 'CLERK',          'batch.split'),
    -- batch.transition：to-XXX 流（M,I）
    (9400000000000025::bigint, 'MANAGER',        'batch.transition'),
    (9400000000000026::bigint, 'INSPECTOR',      'batch.transition'),
    -- batch.deliver：deliver / complete（M,C）
    (9400000000000027::bigint, 'MANAGER',        'batch.deliver'),
    (9400000000000028::bigint, 'CLERK',          'batch.deliver'),
    -- batch.cancel：批次取消（M,C）
    (9400000000000029::bigint, 'MANAGER',        'batch.cancel'),
    (9400000000000030::bigint, 'CLERK',          'batch.cancel'),
    -- batch.repair：返修读写（M,C,I）
    (9400000000000031::bigint, 'MANAGER',        'batch.repair'),
    (9400000000000032::bigint, 'CLERK',          'batch.repair'),
    (9400000000000033::bigint, 'INSPECTOR',      'batch.repair'),
    -- inspection.view：待品检读（M,I）
    (9400000000000034::bigint, 'MANAGER',        'inspection.view'),
    (9400000000000035::bigint, 'INSPECTOR',      'inspection.view'),
    -- delivery_note.view：列表收紧去 CNC + 详情/batch-detail 补闸（M,C,I）
    (9400000000000036::bigint, 'MANAGER',        'delivery_note.view'),
    (9400000000000037::bigint, 'CLERK',          'delivery_note.view'),
    (9400000000000038::bigint, 'INSPECTOR',      'delivery_note.view'),
    -- outsource.view：看板 + 报价读 + 公司读（公司读收紧去 CNC，M,C,I）
    (9400000000000039::bigint, 'MANAGER',        'outsource.view'),
    (9400000000000040::bigint, 'CLERK',          'outsource.view'),
    (9400000000000041::bigint, 'INSPECTOR',      'outsource.view'),
    -- outsource.quote.edit：报价建/可报价零件/提交/删（M,C）
    (9400000000000042::bigint, 'MANAGER',        'outsource.quote.edit'),
    (9400000000000043::bigint, 'CLERK',          'outsource.quote.edit'),
    -- outsource.quote.approve：approve / reject（M）
    (9400000000000044::bigint, 'MANAGER',        'outsource.quote.approve'),
    -- outsource.move：收发货单点（M,C,I）
    (9400000000000045::bigint, 'MANAGER',        'outsource.move'),
    (9400000000000046::bigint, 'CLERK',          'outsource.move'),
    (9400000000000047::bigint, 'INSPECTOR',      'outsource.move'),
    -- outsource.company.manage：公司建/改/删（M,C）
    (9400000000000048::bigint, 'MANAGER',        'outsource.company.manage'),
    (9400000000000049::bigint, 'CLERK',          'outsource.company.manage')
) AS v(id, role_code, permission_code)
WHERE NOT EXISTS (
    SELECT 1 FROM t_role_permission rp
    WHERE rp.role_code = v.role_code AND rp.permission_code = v.permission_code
)
ON CONFLICT (role_code, permission_code) WHERE deleted_at IS NULL DO NOTHING;

-- ---------------------------------------------------------------------------
-- 4. t_role_menu：系统角色 → 菜单**首次基线**
-- ---------------------------------------------------------------------------
-- 语义（与第 3 节 t_role_permission 同款）：`WHERE NOT EXISTS` 只补「该
-- (role, menu_id) 从未有过任何行（含软删）」的组合；面板的增 / 删重启后全部保留
-- —— seed 不复活软删行、不回补被移除的组合（理由见文件头「t_role_menu 基线语义」）。
--
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
-- seed 内部 0 重复、与存量 0 撞号（tests/iam/menu_seed.rs 的
-- `seed_fills_missing_baseline` 每次运行复核）。md5 碰撞概率 ~1e-11，且真撞了是
-- 硬报错（不会静默丢授权）—— 这是想要的行为。
--
-- ID 段物理不相交（避免与存量行、其它 ID 空间打架）：
--   - 9100000000000xxx   本文件派生段（~9.1e15）
--   - 9000000001000xxx   2026-10-13 之前的旧 seed 静态段（存量行仍用，不动）
--   - 9000000000000xxx   t_menu 静态段（~9.0e15）
--   - ≥ 2.07e17          生产雪花 ID / fixture ID（~2.2e17 与 9.0e18）

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
  AND NOT EXISTS (
      SELECT 1 FROM t_role_menu rm
      WHERE rm.role = 'MANAGER' AND rm.menu_id = m.id
  )
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
    -- worker_queue 本身。这两个 code 已从白名单移除：存量授权由面板接管，
    -- 本段只补从未有过的组合（seed 不回收、不复活）。
    'production_group', 'worker_queue',
    'outsource_list', 'outsource_companies_list', 'outsource_quotes_list', 'outsource_send_receive_list',
    'repair_receive'
)
AND m.deleted_at IS NULL
  AND NOT EXISTS (
      SELECT 1 FROM t_role_menu rm
      WHERE rm.role = 'CLERK' AND rm.menu_id = m.id
  )
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
  AND NOT EXISTS (
      SELECT 1 FROM t_role_menu rm
      WHERE rm.role = 'INSPECTOR' AND rm.menu_id = m.id
  )
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
  AND NOT EXISTS (
      SELECT 1 FROM t_role_menu rm
      WHERE rm.role = 'CNC_PROGRAMMER' AND rm.menu_id = m.id
  )
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
  AND NOT EXISTS (
      SELECT 1 FROM t_role_menu rm
      WHERE rm.role = 'SHELF_ACCOUNT' AND rm.menu_id = m.id
  )
ON CONFLICT (role, menu_id) WHERE deleted_at IS NULL DO NOTHING;

-- ---------------------------------------------------------------------------
-- 5. 已下线菜单的授权清理
-- ---------------------------------------------------------------------------
-- 本段不是授权决策，只是清理**已软删菜单**的 t_role_menu 残留（settings_root
-- 及其 3 个子菜单、2026-10-08 下线的 delivery_dispatch 等）。
-- 菜单软删由 seeds/menu.sql §3 完成，故本文件必须在 menu.sql 之后应用
-- （启动钩子的装载顺序亦然）。
-- 硬删而非软删：这些菜单本身已下线，其授权行没有保留价值；t_role_menu 的
-- partial 唯一索引只覆盖 live 行，硬删不会与 §4 的补种路径打架。
DELETE FROM t_role_menu rm
USING t_menu m
WHERE rm.menu_id = m.id
  AND m.code IN ('settings_root', 'work_types_list', 'processes_list', 'work_type_processes_list',
                 'delivery_dispatch')
  AND m.deleted_at IS NOT NULL;

COMMIT;
