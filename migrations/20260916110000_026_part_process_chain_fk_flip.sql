-- Migration 026: 工艺链 FK 方向翻转 —— t_part.process_chain_id 指向 t_part_process_chain.id
-- 2026-09-16 PR-1 process-chain-fk-flip
--
-- 背景（2026-09-16 需求）：
--   旧方向是 `t_part_process_chain.part_id UNIQUE`（migration 017），1:1 绑定零件。
--   前端进入「工序制定」页面时需要批量区分「已制定 / 未制定工序」的零件 —— 旧方向
--   必须对每个 part 反查链表，part 列表无法一次查询完成区分。翻转为
--   `t_part.process_chain_id` 后，列表按 `process_chain_id IS NULL` 即可区分；
--   点击零件后再按 chain id 加载对应工序信息。
--
-- 变更（与 017 配套阅读）：
--   1. t_part ADD COLUMN process_chain_id bigint NULL
--      （逻辑 FK → t_part_process_chain.id；本项目约定无物理 FK）
--   2. 回填：把旧方向 part_id 上的活跃绑定回写到 t_part.process_chain_id
--   3. 普通部分索引 ix_t_part_process_chain_id（加速「按链反查 part / 非空筛选」）
--   4. 部分唯一索引 uq_t_part_process_chain：活跃 part 间 1:1 强约束
--      （NULL 不参与唯一；part 软删时 service 级联 unlink 让出槽位）
--   5. t_part_process_chain DROP COLUMN part_id
--      （PG 自动带出该列上的 UNIQUE 约束与其索引）
--
-- 幂等：ADD COLUMN / CREATE INDEX / DROP COLUMN 均带 IF [NOT] EXISTS；回填 UPDATE 天然幂等。

-- 1. 新列
ALTER TABLE public.t_part
    ADD COLUMN IF NOT EXISTS process_chain_id bigint;

COMMENT ON COLUMN public.t_part.process_chain_id IS
    '逻辑 FK → t_part_process_chain.id（无物理 FK）；NULL = 未制定工艺链；活跃 part 间 1:1（uq_t_part_process_chain）';

-- 2. 回填旧方向上的活跃绑定（仅双方均未软删）
UPDATE public.t_part p
SET process_chain_id = c.id
FROM public.t_part_process_chain c
WHERE c.part_id = p.id
  AND c.deleted_at IS NULL
  AND p.deleted_at IS NULL;

-- 3. 普通部分索引
CREATE INDEX IF NOT EXISTS ix_t_part_process_chain_id
    ON public.t_part(process_chain_id)
    WHERE process_chain_id IS NOT NULL;

-- 4. 1:1 强约束（部分唯一：仅活跃 part 参与）
CREATE UNIQUE INDEX IF NOT EXISTS uq_t_part_process_chain
    ON public.t_part(process_chain_id)
    WHERE process_chain_id IS NOT NULL AND deleted_at IS NULL;

-- 5. 删除旧方向列（带出 part_id 上的 UNIQUE 约束与索引）
ALTER TABLE public.t_part_process_chain
    DROP COLUMN IF EXISTS part_id;
