-- ============================================================================
-- 2026-10-10：t_part_batch.delivery_seq —— 批次「加入送货单的先后顺序」
-- ============================================================================
-- 背景：送货单详情的零件列表要求按**加入本单的先后**排序，而既有排序键
-- `pb.id ASC` 是**批次创建顺序**：只有「拆批」路径产生新批次 id（反映扫码
-- 时刻），整批直接挂单的批次用的是它建批时的 id。于是「先扫 A、后扫 B，
-- 但 A 的批次建得更早」会把 A 排到前面，与用户看到的扫码次序相反。
--
-- 为什么新开一列而不是复用既有列：
--   t_part_batch 整表没有 seq / added_at / added_seq 之类可表达「挂单时刻」的
--   列（placed_at 已在 2026-09-16 migration 028 随批次 step 化删除）；
--   `delivery_note_id` 是外键而非关联行，也没有 `t_delivery_note_line`
--   这样的行表可承载单内次序。故新增本列。
--
-- ⚠️ 与 `sort_order` 语义**不同**，不要混用：本仓既有的 `sort_order`
-- （iam 菜单 / 外协公司 / 外协工序能力清单）全是**配置显示序**——人工维护的
-- 静态排列；本列是**业务事实**——由挂单写点自动递增、摘单时清空，只在
-- 「这一张送货单」这一张单的语境内有意义。
--
-- 不变式（src 侧写点见 `PartBatchRepo::attach_to_note` 与
-- `delivery_note` 域的两处摘单写点）：
--   挂单（attach）→ COALESCE(MAX(delivery_seq),0)+1（同单内从 1 起递增）
--   摘单 / 单据软删 → 置 NULL（与 delivery_note_id = NULL 同点同事务）
--   ⇒ `delivery_seq IS NULL ⟺ delivery_note_id IS NULL`
--
-- 回填：按 `ROW_NUMBER() OVER (PARTITION BY delivery_note_id ORDER BY id)`
-- 给当前已挂单的行赋值，取值与上线前的 `pb.id ASC` **逐行一致**，部署后无
-- 视觉跳变。

ALTER TABLE public.t_part_batch
    ADD COLUMN delivery_seq bigint;

COMMENT ON COLUMN public.t_part_batch.delivery_seq IS
  '批次加入当前送货单的次序（本单内从 1 起递增）；NULL = 未挂单。'
  '注意与 sort_order 语义不同：sort_order 是配置显示序（人工维护的静态排列），'
  '本列是挂单时自动递增的业务事实，且只在「这一张送货单」的语境内有意义。';

UPDATE public.t_part_batch
SET delivery_seq = rn
FROM (
    SELECT id,
           ROW_NUMBER() OVER (PARTITION BY delivery_note_id ORDER BY id) AS rn
    FROM public.t_part_batch
    WHERE delivery_note_id IS NOT NULL
) AS ranked
WHERE t_part_batch.id = ranked.id
  AND t_part_batch.delivery_seq IS NULL;
