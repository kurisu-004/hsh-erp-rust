-- 017: 创建工艺链两张表（header + step）
-- 2026-09-11 part-worker-pool-federated-rocket 方案
--
-- 目的：把"每个 part 绑定一份多步工艺链"做实。
-- - header（t_part_process_chain）：1:1 绑定 part（UNIQUE part_id），含 name / version / note / 审计字段
-- - step（t_process_chain_step）：多步子表，逻辑指向 chain_id（无物理 FK），含 process_id / sort_order / estimated_minutes
--
-- 设计要点（与方案一致）：
-- 1. 1:1 binding：`part_id UNIQUE`，DB 强约束
-- 2. sort_order 稀疏 10/20/30：中间插入只需一个 UPDATE
-- 3. 部分唯一索引 `(chain_id, sort_order) WHERE deleted_at IS NULL`：软删 step 不占 sort 槽位
-- 4. 软删：`deleted_at IS NULL` 守卫；service 层 UPDATE 时走 OCC
-- 5. 软删步骤由 service `upsert_chain` 在事务内做整组替换（先删后插 + chain.version++）

CREATE TABLE public.t_part_process_chain (
    id          bigint PRIMARY KEY,
    part_id     bigint NOT NULL UNIQUE,    -- 1:1 binding
    name        varchar(64) NOT NULL DEFAULT '默认工艺',
    version     int NOT NULL DEFAULT 0,
    note        text,
    created_at  timestamp NOT NULL DEFAULT now(),
    created_by  bigint NOT NULL,
    updated_at  timestamp NOT NULL DEFAULT now(),
    updated_by  bigint NOT NULL,
    deleted_at  timestamp
);

COMMENT ON COLUMN public.t_part_process_chain.version IS
    '乐观锁版本号；每次 UPDATE 自增；冲突抛 40901 VERSION_CONFLICT';

CREATE INDEX ix_part_process_chain_deleted
    ON public.t_part_process_chain(deleted_at);

CREATE TABLE public.t_process_chain_step (
    id                bigint PRIMARY KEY,
    chain_id          bigint NOT NULL,      -- 逻辑指向 t_part_process_chain.id（无 FK）
    sort_order        int NOT NULL,         -- 稀疏 10/20/30
    process_id        bigint NOT NULL,      -- 逻辑指向 t_process.id
    estimated_minutes int NOT NULL CHECK (estimated_minutes >= 0),
    version           int NOT NULL DEFAULT 0,
    created_at        timestamp NOT NULL DEFAULT now(),
    created_by        bigint NOT NULL,
    updated_at        timestamp NOT NULL DEFAULT now(),
    updated_by        bigint NOT NULL,
    deleted_at        timestamp
);

COMMENT ON COLUMN public.t_process_chain_step.version IS
    '乐观锁版本号；每次 UPDATE 自增；冲突抛 40901 VERSION_CONFLICT';

-- 部分唯一索引：同一 chain 内未软删步骤的 sort_order 不可重复
CREATE UNIQUE INDEX uq_chain_step_chain_order
    ON public.t_process_chain_step(chain_id, sort_order)
    WHERE deleted_at IS NULL;

-- 链 id 上的部分索引（service `get_by_part` / upsert 内的 step list_by_chain 用）
CREATE INDEX ix_chain_step_chain
    ON public.t_process_chain_step(chain_id)
    WHERE deleted_at IS NULL;