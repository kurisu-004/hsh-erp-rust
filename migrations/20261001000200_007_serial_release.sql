-- ============================================================================
-- Migration 007: 序列号释放 + uk_t_part_serial_no 谓词修复
-- ============================================================================
-- 2026-10-01 新增
--
-- 背景（两个互相咬合的缺陷）：
--
--   缺陷 A —— 序列号被永久占用：
--     `PartRepo::clear_part_serial_no_when_completed` 的 WHERE 条件是
--     **part 级**的 `status='COMPLETED'`，但 `PartService::complete` 一次只翻
--     **一条批次**。多批次工单完成其中一条时，`compute_part_target` 仍返回
--     `DELIVERED`（还有更慢的批次），那条 UPDATE 命中 0 行；更糟的是它被
--     `let _ =` 静默吞掉（`lifecycle.rs:301`），于是没有任何告警。序列号从此
--     长期挂在 `t_part.serial_no` 上，同一序列号无法再被新工单复用。
--     修复在 Rust 侧：释放逻辑下沉进 rollup（`status_gate` 的 step 4），
--     与「是否所有批次都完成」解耦。
--
--   缺陷 B —— 唯一索引谓词缺 `deleted_at`：
--     baseline:3517 的 `uk_t_part_serial_no` 谓词只有 `serial_no IS NOT NULL`，
--     而同族索引 `uk_t_assembly_serial_no`（baseline:3475）是
--     `deleted_at IS NULL AND serial_no IS NOT NULL`。差异后果：软删工单
--     仍占着序列号唯一键，作废工单无法把序列号让给新工单。
--
-- 顺序（不可交换）：
--   1) 先归档 + 释放存量已终态行的 serial_no（第 2 节）
--   2) 再重建索引（第 3 节）
--   释放在前是因为新谓词含 `status <> 'CANCELLED'`，而 RELEASED 的 UPDATE
--   不改 status —— 两个谓词互不覆盖，但先释放能让第 3 节的冲突检测结果
--   反映「真正需要人工介入的」重复键。
--
-- ---------------------------------------------------------------------------
-- 1. 冲突检测（**只读，不执行任何清理**）
-- ---------------------------------------------------------------------------
-- DROP + CREATE UNIQUE INDEX 若遇到下表列出的重复键会直接失败并中断整个
-- migration —— 这正是我们想要的：宁可迁移失败让人来看，也不要静默删数据。
-- 排障时在 apply 之前先跑一遍：
--
--   SELECT serial_no, COUNT(*) AS n,
--          array_agg(id ORDER BY id) AS part_ids,
--          array_agg(status ORDER BY id) AS statuses
--     FROM public.t_part
--    WHERE serial_no IS NOT NULL
--      AND deleted_at IS NULL
--      AND status <> 'CANCELLED'
--    GROUP BY serial_no
--   HAVING COUNT(*) > 1;
--
-- 期望 0 行。若非 0，说明**活跃非取消工单之间**序列号真的撞了
-- （数据本身有问题），需人工判定保留哪一条后重跑本迁移。
-- 注意：含 CANCELLED / 软删行的重复不在此列（新谓词已放行它们），
-- 故此查询的结果一定 ≤ 重建索引时报的冲突数。
--
-- ---------------------------------------------------------------------------
-- 2. 存量终态行释放序列号（子件先归档，父装配件直接清）
-- ---------------------------------------------------------------------------
-- 为何子件（t_part）要归档而父装配件（t_assembly）不归档：
--   `t_assembly` 没有事件表，而它的 `note` 列是**用户可编辑的业务字段**
--   （工单备注），拿它记系统动作会污染用户数据、且事后无法区分；
--   `t_part` 有 `t_part_event` 事件流水，归档成本低、可审计。
--   终态行（COMPLETED / CANCELLED）的序列号已无业务用途（已转交送货单或
--   作废），直接清空即可让新工单复用。
--
-- ⚠️ t_part_event.id 是 `bigint NOT NULL` 且**无默认值**，SQL 里无法生成
-- 雪花 ID，故用确定性方案 `(SELECT COALESCE(MAX(id),0) FROM t_part_event)
-- + ROW_NUMBER() OVER (ORDER BY id)`：
--   * 确定性：同一份数据重复执行得到同一批 id（本迁移整体不可重入，但
--     单看这段表达式它是纯函数，利于事后复现/审计）；
--   * 不与运行时冲突：MAX(id) 取的是**应用已发出的最大雪花 ID**，而
--     运行时雪花 ID 由 `SnowflakeIdGenerator`（41bit 毫秒时间戳 ×
--     instance × 12bit seq）持续**向上单调递增**，永远 > 现有 MAX(id)；
--     且本段占用的 [MAX+1, MAX+N] 区间在迁移完成后**永不复用**（雪花不会
--     回退到历史区间），故不存在与后续运行时事件 ID 撞车的可能。
--   * 单事务内 ROW_NUMBER() 对同一批行是唯一且连续，配合 MAX(id) 快照不碰撞。

INSERT INTO public.t_part_event (
    id, part_id, event_type, to_status, note, created_at
)
SELECT
    (SELECT COALESCE(MAX(id), 0) FROM public.t_part_event)
        + ROW_NUMBER() OVER (ORDER BY p.id) AS id,
    p.id,
    'SERIAL_RELEASED',
    p.status,
    '序列号释放归档：' || p.serial_no,
    now()
  FROM public.t_part p
 WHERE p.serial_no IS NOT NULL
   AND p.status IN ('COMPLETED', 'CANCELLED')
   AND p.deleted_at IS NULL;

UPDATE public.t_part
   SET serial_no  = NULL,
       updated_at = now()
 WHERE serial_no IS NOT NULL
   AND status IN ('COMPLETED', 'CANCELLED')
   AND deleted_at IS NULL;

UPDATE public.t_assembly
   SET serial_no  = NULL,
       updated_at = now()
 WHERE serial_no IS NOT NULL
   AND status IN ('COMPLETED', 'CANCELLED')
   AND deleted_at IS NULL;

-- ---------------------------------------------------------------------------
-- 3. 重建唯一索引（补 deleted_at / status 谓词）
-- ---------------------------------------------------------------------------

DROP INDEX public.uk_t_part_serial_no;

CREATE UNIQUE INDEX uk_t_part_serial_no
    ON public.t_part USING btree (serial_no)
    WHERE serial_no IS NOT NULL
      AND deleted_at IS NULL
      AND status <> 'CANCELLED';
