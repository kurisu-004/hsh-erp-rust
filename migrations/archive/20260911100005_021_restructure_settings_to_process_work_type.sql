-- Migration 021: 菜单重排 —— 删除「设置」菜单，新建「工序工种」到「生产管理」
--
-- 业务背景（2026-09-12）：原「设置」菜单下的 3 个子菜单（工种管理 / 工序管理 /
-- 工种-工序映射）合并为一个 tabbed shell，统一归到「生产管理 → 工序工种」下。
-- 删除旧菜单 + 关联 role_menu；新建 process_work_type 菜单 + 授权。
--
-- 幂等处理：
-- - 删除用 UPDATE 软删（保留 deleted_at 历史），is_active=false 让 router guard
--   拒绝放行；后续清理窗口再物理删
-- - 删除 t_role_menu 用 DELETE WHERE menu_id IN (...) —— 软删后菜单不可见，
--   role_menu 残留无副作用
-- - 插入新菜单用 INSERT ... WHERE NOT EXISTS；role_menu 用 CROSS JOIN +
--   NOT EXISTS 兜底

-- 1. 删除 3 个子菜单的 t_role_menu 关联
DELETE FROM t_role_menu
WHERE menu_id IN (
    SELECT id FROM t_menu
    WHERE code IN ('work_types_list', 'processes_list', 'work_type_processes_list')
      AND deleted_at IS NULL
);

-- 2. 软删 3 个子菜单（保留历史；后续可物理清理）
UPDATE t_menu
SET deleted_at = now(), is_active = false, updated_at = now()
WHERE code IN ('work_types_list', 'processes_list', 'work_type_processes_list')
  AND deleted_at IS NULL;

-- 3. 软删设置菜单本身
UPDATE t_menu
SET deleted_at = now(), is_active = false, updated_at = now()
WHERE code = 'settings_root'
  AND deleted_at IS NULL;

-- 4. 插入新菜单「工序工种」到「生产管理」下（idempotent）
INSERT INTO t_menu (
    id, parent_id, code, title, path, icon, sort_order,
    is_active, version, created_by, updated_by
)
SELECT
    nextval('public.t_menu_id_seq'::regclass),
    (SELECT id FROM t_menu WHERE code = 'production_group' AND deleted_at IS NULL),
    'process_work_type',
    '工序工种',
    '/production/process-work-type',
    'Operation',
    5,
    true, 0, 0, 0
WHERE EXISTS (
    SELECT 1 FROM t_menu WHERE code = 'production_group' AND deleted_at IS NULL
)
AND NOT EXISTS (
    SELECT 1 FROM t_menu WHERE code = 'process_work_type' AND deleted_at IS NULL
);

-- 5. 授权：MANAGER + CLERK + INSPECTOR
INSERT INTO t_role_menu (id, role, menu_id, version, created_by, updated_by)
SELECT nextval('public.t_role_menu_id_seq'::regclass), r.role, m.id, 0, 0, 0
FROM (VALUES ('MANAGER'), ('CLERK'), ('INSPECTOR')) AS r(role)
CROSS JOIN (
    SELECT id FROM t_menu
    WHERE code = 'process_work_type' AND deleted_at IS NULL
) AS m
WHERE NOT EXISTS (
    SELECT 1 FROM t_role_menu rm
    WHERE rm.role = r.role
      AND rm.menu_id = m.id
      AND rm.deleted_at IS NULL
);