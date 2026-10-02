-- ============================================================================
-- Migration 008: DIRECT 占位报价的 (part, company, process) 唯一性
-- ============================================================================
-- 2026-10-03 新增
--
-- 背景：
--   既有唯一索引 `uq_t_outsource_quote_approved_part_process (part_id, process_id)`
--   的谓词是 `deleted_at IS NULL AND status = 'APPROVED' AND is_direct = false`
--   —— **刻意排除** `is_direct = true`。这是必要的：`is_direct` 占位报价由
--   `BatchService::resolve_direct_quote_id`（src/modules/prod/batch/service/outsource.rs）
--   在免审批直发时自动创建，而审批报价的语义是「每 (零件, 工序) 最多一条 APPROVED」，
--   DIRECT 不走审批、也不该占用那个唯一键。
--
--   但同一条排除也带来缺口：除 `id` 主键外，`t_outsource_quote` 上**没有任何索引**
--   覆盖 DIRECT 行。于是同一个 (part_id, outsource_company_id, process_id) 三元组
--   在并发（双击 / 超时重试 / 两个批次同 tuple 直发）下会各 INSERT 一条等价的
--   0 元占位报价。`resolve_direct_quote_id` 的 `ON CONFLICT DO NOTHING` + 回查
--   只能抗住**已存在**的行，扛不住「本来就没有任何唯一约束」这件事。
--
-- 本迁移补上缺失的那一半约束：DIRECT 占位报价的业务语义就是「这次直发的价来源
-- 标记」，同一 (零件, 公司, 工序) 存在两条等价记录没有任何业务价值，禁掉它是
-- 纯收益。索引建成之后，上游的 `ON CONFLICT DO NOTHING` + 回查才真正成立
-- （Rust 侧零改动，回查分支从「理论不可达」变为「并发窗口内可达」）。
--
-- 谓词与既有 `uq_t_outsource_quote_approved_part_process` 对称：同样只约束
-- `APPROVED` + 未软删的活跃行，故
--   * 同一 (part, process) 的审批报价（is_direct=false）仍可与 DIRECT 占位共存
--     —— 这是 `is_direct` 列存在的意义，不被本迁移改变；
--   * 占位报价一旦被改价 / 改状态离开 `APPROVED`（例如对账补录单价后走别的状态），
--     立即退出本索引，可以为下一次直发让位。
--
-- ---------------------------------------------------------------------------
-- 冲突检测（**只读，不执行任何清理**）
-- ---------------------------------------------------------------------------
-- CREATE UNIQUE INDEX 遇到重复键会直接失败并中断整个 migration —— 这是想要的
-- 行为：宁可迁移失败让人来看，也不要静默删数据。apply 之前先跑一遍：
--
--   SELECT part_id, outsource_company_id, process_id, COUNT(*) AS n,
--          array_agg(id ORDER BY id) AS quote_ids
--     FROM public.t_outsource_quote
--    WHERE deleted_at IS NULL
--      AND status = 'APPROVED'
--      AND is_direct = true
--    GROUP BY part_id, outsource_company_id, process_id
--   HAVING COUNT(*) > 1;
--
-- 期望 0 行。非 0 时每组都是「同一个直发 tuple 多条 0 元占位报价」，人工保留
-- 其中 id 最大的一条（最新建的那条）、软删其余（`deleted_at = now()`）后重跑。
-- 软删即退出本索引谓词，是清理这类行的标准手段。

CREATE UNIQUE INDEX uq_t_outsource_quote_direct_part_company_process
    ON public.t_outsource_quote
    USING btree (part_id, outsource_company_id, process_id)
    WHERE deleted_at IS NULL
      AND status = 'APPROVED'
      AND is_direct = true;
