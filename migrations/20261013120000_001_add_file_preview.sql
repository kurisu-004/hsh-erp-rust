-- 2026-10-13 files 域：COS CI 预览任务表（PR2）
--
-- 背景：`files::part_file::preview` 子模块（`GET /api/v2/files/objects/{id}/preview`）
-- 要把 PDF 逐页光栅化成 JPG 交给前端翻页。渲染本身跑在 python 侧的 CI（腾讯云 CI
-- 按**页**计费，且是异步任务），rust 侧只做三件事：
--   ① 按内容寻址算出稳定的 `preview_seed`；
--   ② 惰性提交 / 惰性轮询 CI job；
--   ③ 把 job 状态与页数记在本表。
--
-- **本轮刻意零后台任务、零新增 cron**：状态推进完全由前端下一次调用本端点时驱动
-- （`PENDING & 到期 → forward_ci_describe`）。没有无人触达时的推进 ⇒ 也就没有
-- 「进程重启后 job 卡在 PENDING 永不收敛」这类定时器问题；代价是**没人调就没人推进**
-- （见 `src/modules/files/part_file/preview/mod.rs` 的状态机小节）。
--
-- ## ⚠️ 为什么独立成表，而不是给 `t_part_file` 加列
--
-- `t_part_file` 的去重键是 `uk_t_part_file_single (part_id, kind)` +
-- `uk_t_part_file_part_kind_sha (part_id, kind, content_sha256)` —— **按工单去重，
-- 不是全局**。同一份 PDF 挂在 N 个零件下就有 N 行。
--
-- 而预览是**按内容寻址**的：CI 按页计费，同一份 PDF 的 N 份拷贝只该跑 **1 次** CI、
-- 产出 1 份 `previews/<seed>/` 目录，N 行 `t_part_file` 共用它。若把预览状态挂成
-- `t_part_file` 的列，状态就会随行分裂成 N 份，谁先跑完算谁的，另一份永远停在
-- `PENDING`。独立表 + `preview_seed` 唯一 ⇒ 一份内容恒一份状态。
--
-- `uk_t_file_preview_seed` 的第二个用途是**并发闸门**：两个请求同时 cache miss 时，
-- 后到者 INSERT 撞唯一索引（`ON CONFLICT DO NOTHING` ⇒ 0 行）⇒ 走「已有 PENDING」
-- 分支，**不会重复提交 CI job**（重复提交 = 重复计费）。
--
-- ## 列语义
--
-- - `preview_seed char(64)`：内容寻址种子，**32 位 hex**（sha256 前 16 字节）。
--   ⚠️ **路径里绝不能带文件名** —— `t_part_file.object_key` 是 `{sha16}_{原文件名}`，
--   而文件名是图号（如 `A00123_rev2.pdf`），**可枚举**。COS `previews/` 前缀是
--   public-read（预览直链永久不过期，见 `preview/mod.rs` 的安全取舍一节），路径里
--   出现图号等于把图号清单公开。种子恒为纯哈希。
-- - `char(64)` 而非 `varchar`：定长 64 是 sha256 hex 的全宽，本列只存前 32 位，
--   取 64 是为了给「将来改存完整 sha256」留位而不必改列型 / 重写索引。
-- - `ci_job_id`：python 侧 CI job 标识；`NULL` = 尚未提交成功。
-- - `state`：`PENDING` / `READY` / `FAILED`（varchar(20)，无 DB ENUM —— 本仓规约）。
-- - `next_poll_at`：惰性轮询节流戳。`PENDING` 且 `now() < next_poll_at` ⇒ 直接返
--   PENDING，**不打 python**。
--
-- ## 软删 / 审计
--
-- 与全仓其它业务表同形：`deleted_at IS NULL` 参与全部唯一索引与查询；审计四列齐全。
-- ⚠️ 本表**刻意没有 `version` 列**：它的每次写入都是「当前状态 → 下一个状态」的
-- 单向迁移（幂等重放同一迁移安全），并发控制靠 `uk_t_file_preview_seed` + `state`
-- 谓词，不走 OCC。全仓「所有带 version 的表的写端点必须收客户端 version」那条
-- 约束对本表不适用（表上没有 version）。

CREATE TABLE public.t_file_preview (
    id             bigint       NOT NULL,
    -- 内容寻址种子，32 位 hex（sha256 前 16 字节）。详见上方「为什么独立成表」小节
    -- 关于「路径里不能带文件名」的安全约束。
    preview_seed   char(64)     NOT NULL,
    -- 源对象在 COS 的完整 key（`t_part_file.object_key` 的原样副本）。CI 需要它去
    -- 拉源 PDF；单独存一份是为了 python 回调 / 排障时不必 JOIN 回 t_part_file。
    src_object_key varchar(500) NOT NULL,
    -- python 侧 CI job id；NULL = 尚未提交成功（提交失败即整条记录被删，见下）。
    ci_job_id      varchar(64),
    -- PENDING / READY / FAILED（本仓无 DB ENUM，合法性由 Rust 侧状态机保证）。
    state          varchar(20)  NOT NULL DEFAULT 'PENDING',
    -- CI 报的总页数；READY 时 > 0，PENDING 时 0。
    page_count     integer      NOT NULL DEFAULT 0,
    -- CI 失败原因（FAILED 时非空），仅回显给前端，不含栈。
    error_message  varchar(500),
    -- 惰性轮询节流戳：now() < next_poll_at ⇒ 本次调用直接返 PENDING，不打 python。
    next_poll_at   timestamp,
    created_at timestamp NOT NULL DEFAULT now(),
    updated_at timestamp NOT NULL DEFAULT now(),
    created_by bigint, updated_by bigint, deleted_at timestamp
);

-- 内容寻址 + 并发闸门（详见上方小节第二段）。
CREATE UNIQUE INDEX uk_t_file_preview_seed
    ON public.t_file_preview (preview_seed) WHERE deleted_at IS NULL;

-- 供「哪些 job 到期可轮询」这类运维查询走索引；本仓当前无后台任务消费它
-- （惰性轮询按主键查），保留是为将来接 cron / 排障 SQL 预留。
CREATE INDEX idx_t_file_preview_poll
    ON public.t_file_preview (next_poll_at) WHERE state = 'PENDING' AND deleted_at IS NULL;

COMMENT ON TABLE public.t_file_preview IS
    'COS CI 预览任务：按内容寻址（preview_seed）而非按 t_part_file 行去重，同一份 PDF 跨零件只跑一次 CI（CI 按页计费）';
COMMENT ON COLUMN public.t_file_preview.preview_seed IS
    '内容寻址种子（sha256 前 16 字节的 32 位 hex）。⚠️ 纯哈希、绝不带文件名：previews/ 前缀 public-read，文件名（图号）可枚举';
COMMENT ON COLUMN public.t_file_preview.state IS
    'PENDING / READY / FAILED。FAILED 不终止，下次调用自愈重启为 PENDING';