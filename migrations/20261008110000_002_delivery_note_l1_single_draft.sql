-- 2026-10-08：送货单建单判定键从 (customer_id, scope) 收敛为 (customer_id, status='DRAFT')。
--
-- 背景：原先「一个 L1 下建哪张送货单」由 `NoteScope` 三态决定 —— L1Wide / Group(gid)
-- / Leaf(cid)，每态各有一个部分唯一索引（uq_t_delivery_note_draft_group /
-- uq_t_delivery_note_draft_leaf），**只有 L1Wide 没有唯一索引**。效果是同一个 L1 名下
-- 3 个分组 + 2 个未分组 L2 最多能拆出 5~6 张 DRAFT 单。
--
-- 新规则：一个 L1 同时只允许一张 DRAFT 送货单。「同一天」只是描述默认行为
-- （新建时 delivery_date 默认 today.date()），**不是筛选条件** —— delivery_date 是
-- 可编辑字段，用户改到明天后继续扫码仍应加到同一张单。
--
-- ⚠️ 上线前必须先跑下面的重复检查；库里若已有同 L1 的多张 DRAFT，本 migration 会
-- RAISE EXCEPTION 并打印明细，需人工先把它们合并 / 作废（改 status 或软删）后再
-- apply。注意软删不算（判据带 deleted_at IS NULL），因此 soft-delete 过的草稿不挡路。

DO $$
DECLARE
    dupes TEXT;
BEGIN
    SELECT string_agg(format('customer_id=%s note_ids=%s', d.customer_id, d.ids), '; ')
      INTO dupes
      FROM (
        SELECT n.customer_id,
               array_agg(n.id ORDER BY n.id) AS ids
          FROM t_delivery_note n
         WHERE n.status = 'DRAFT'
           AND n.deleted_at IS NULL
         GROUP BY n.customer_id
        HAVING count(*) > 1
      ) d;

    IF dupes IS NOT NULL THEN
        RAISE EXCEPTION
            '存在同 L1 多张 DRAFT 送货单，需人工合并/作废后重跑本 migration：%', dupes;
    END IF;
END $$;

-- 新判定键的兜底：并发扫码时两个事务可能同时「查不到 DRAFT → 各建一张」，
-- 应用层判据（find_open_draft_by_l1）与数据库约束必须双保险。
CREATE UNIQUE INDEX IF NOT EXISTS uk_t_delivery_note_l1_open_draft
    ON public.t_delivery_note (customer_id)
    WHERE status = 'DRAFT' AND deleted_at IS NULL;

-- 原 3 个 scope 索引里的 2 个（Group / Leaf 各一）不再被任何查询使用。
-- ⚠️ 名字照抄 baseline 的实际索引名，不是「scope 语义」猜的名字。
DROP INDEX IF EXISTS public.uq_t_delivery_note_draft_group;
DROP INDEX IF EXISTS public.uq_t_delivery_note_draft_leaf;