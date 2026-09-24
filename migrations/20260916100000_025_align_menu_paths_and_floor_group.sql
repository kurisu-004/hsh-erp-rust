-- Migration 025: 修复菜单 path 对齐前端 router + 「货架管理」移入「权限管理」 + 移除「车间」 + 升级「扫码台」为顶级
-- 2026-09-16 menu-fix 修复
--
-- 背景：
--   1. part_process_chain 当前 path='/parts/process-chains'，但前端 router 已重命名为
--      '/production/process-design'（frontend/router/index.ts:251-265，2026-09-11 改名），
--      菜单点击 → vue-router resolve 失败 → 守卫降级到 findFirstMenuPath 跳错页。
--   2. print_templates_designer 当前 path='/print-templates/designer'，前端 router
--      2026-09-14 合并为单 '/print-templates'（frontend/router/index.ts:359-369），
--      同样菜单点击失败。
--   3. 「车间」(floor_group) 下「货架管理」(shelves_list) 业务上归「权限管理」更合理；
--      「扫码台」(scan_badge) 升级为顶级菜单（与 home / customer_management 同级），
--      因为扫码台是 HMI 一体机登录后看到的唯一入口，从「车间」二级提到顶级更直接。
--
-- 变更（纯 UPDATE，0 行 INSERT，不引入新静态 id）：
--   a. part_process_chain.path           → '/production/process-design'
--   b. print_templates_designer.path     → '/print-templates'
--   c. shelves_list.parent               → auth_group.id（sort_order=30）
--   d. scan_badge.parent                 → NULL（升级顶级；sort_order=13）
--   e. floor_group.is_active             → FALSE（停用一级菜单，保留行便于审计）
--
-- 幂等：
--   - 所有 UPDATE 加 WHERE 守卫（path <> '...' / parent_id IS NOT NULL / parent_id <> ...
--     / is_active = TRUE）确保重跑不写脏数据
--   - 乐观锁：每个 UPDATE 都 `version = version + 1, updated_at = now()`
-- 2026-09-24 清理：无显式事务包裹，由 sqlx::migrate! 默认按文件粒度加事务
--   （单文件失败自动回滚；嵌套 BEGIN/COMMIT 在 PG 18+ 会从 NOTICE 升级成 ERROR，
--    且无任何原子性收益）。
--
-- 角色授权影响：
--   - parent_id / sort_order / path 变更不影响 t_role_menu（role_menu 只记 role + menu_id，
--     不记 parent_id 或 path），parent 切换后角色授权自动跟随
--   - scan_badge 升顶级后：SHELF_ACCOUNT 用户侧栏能看到「扫码台」一级入口，
--     与 router /scan 段的 allowRoles=['SHELF_ACCOUNT'] 兜底一致
--   - 无需新增 t_role_menu 行

-- a. 修复「制定工序」menu.path（router: /production/process-design）
UPDATE public.t_menu
SET path       = '/production/process-design',
    version    = version + 1,
    updated_at = now()
WHERE code = 'part_process_chain'
  AND deleted_at IS NULL
  AND path <> '/production/process-design';

-- b. 修复「模板编辑」menu.path（router: /print-templates）
UPDATE public.t_menu
SET path       = '/print-templates',
    version    = version + 1,
    updated_at = now()
WHERE code = 'print_templates_designer'
  AND deleted_at IS NULL
  AND path <> '/print-templates';

-- c. 把「货架管理」从「车间」移到「权限管理」下；sort_order 给 30
--    （auth_group 已有 workers_list=10 / users_list=20）
UPDATE public.t_menu
SET parent_id  = (SELECT id FROM public.t_menu
                  WHERE code = 'auth_group' AND deleted_at IS NULL LIMIT 1),
    sort_order = 30,
    version    = version + 1,
    updated_at = now()
WHERE code = 'shelves_list'
  AND deleted_at IS NULL
  AND parent_id <> (SELECT id FROM public.t_menu
                    WHERE code = 'auth_group' AND deleted_at IS NULL LIMIT 1);

-- d. 「扫码台」升级顶级菜单：parent_id = NULL；sort_order = 13
--    顶级排序现状：home=10 / customer_management=15，scan_badge=13 排在二者之间
UPDATE public.t_menu
SET parent_id  = NULL,
    sort_order = 13,
    version    = version + 1,
    updated_at = now()
WHERE code = 'scan_badge'
  AND deleted_at IS NULL
  AND parent_id IS NOT NULL;

-- e. 停用「车间」一级菜单（保留行便于审计 / 未来物理删；与 021 settings_root 风格一致）
UPDATE public.t_menu
SET is_active  = FALSE,
    version    = version + 1,
    updated_at = now()
WHERE code = 'floor_group'
  AND deleted_at IS NULL
  AND is_active = TRUE;
