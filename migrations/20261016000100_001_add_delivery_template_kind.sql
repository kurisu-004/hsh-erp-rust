-- ============================================================================
-- 20261016000100_001_add_delivery_template_kind —— t_part_file 增 DELIVERY_TEMPLATE
-- ============================================================================
-- 需求（用户原话）：「增加模板上传的功能，模板支持 xlsx 格式，当前用于打印送货单。
--       前端的模板管理页面增加上传模板和查看已有模板的功能。且可以绑定一级客户。
--       送货单打印时自动选择已绑定的模板」。
--
-- 归属域：`print_template`（`src/modules/print_template/`，URL 挂 `/api/v2/print-templates`）。
--         ⚠️ 本迁移**不给该域建表**：模板文件字节复用 `files` 域自有的
--         `t_part_file`，「某一级客户名下存在一行未软删的 `DELIVERY_TEMPLATE`」
--         **本身就是绑定关系**（详见 `docs/api/print_template.md` §8.3 第 2 条）。
--
-- 与本仓既有约定的关系（全部照 CLAUDE.md「DB 约定」）：
--   * append-only —— 本文件是**新增**迁移，不改任何已应用文件（改已应用文件会触发
--     sqlx `VersionMismatch` panic）。
--   * 无物理外键 —— 不加 FK 到 `t_customer`：`part_id` 装的是一级客户的
--     `t_customer.id`，存在性与「必须是一级客户（`parent_id IS NULL`）」由
--     `print_template::service` 校验（与 `t_part_file` 既有的 `t_part` /
--     `t_assembly` owner 校验同款）。
--   * 无 DB ENUM —— `kind` 仍是 `varchar(20)` + CHECK 约束（沿用既有形态，
--     本轮只是往 CHECK 的枚举里加一个值）。
--   * 软删 —— 带 `deleted_at`；唯一索引以 `deleted_at IS NULL` 为前提。
--   * 雪花主键 —— `id` 由 App 侧 `SnowflakeIdGenerator` 生成，不在 DB 里取号。
--
-- ⚠️ **为什么 `DROP` 再 `ADD` 而不是在约束上追加**：PG 的 CHECK 约束不支持
--   `ALTER TABLE … ADD CONSTRAINT` 同名替换，也不支持「往既有 CHECK 里加一项」
--   的增量语法。唯一两条路是重建约束，或删掉 CHECK 换成 domain / enum —— 后者
--   会连带改列类型、影响 `TPartFile.kind` 的 `FromRow` 投影与全部既有行，重建
--   约束是影响面最小的一条。
--
-- ⚠️ **`uk_t_part_file_single` 必须一并重建**：它是 partial unique index，
--   谓词里**枚举了 kind 白名单**（DRAWING / 3D_MODEL / ASSEMBLY_MASTER / CAD_2D），
--   不含 `DELIVERY_TEMPLATE`。不改它的话「一个 L1 只有一份活跃模板」这条不变式
--   就没有 DB 层兜底 ⇒ 同客户可传多份模板，`get_by_part_kind` 的 `LIMIT 1` 会
--   随机命中其中一份。**改动方向是把它**加**进白名单（不是删掉该索引）** ——
--   保留「单文件类 kind 至多一条活跃行」这条不变式，是
--   `print_template` 上传流程「先软删旧行、再写新行」这个顺序被逼死的原因。
--
-- ⚠️ **`kind varchar(20)` 装得下**：`DELIVERY_TEMPLATE` 是 17 个字符，
--   连同既有最长的 `ASSEMBLY_MASTER`（14）都在 20 以内，**不需要改列宽**。
--
-- ⚠️ **`uk_t_part_file_part_kind_sha` 不在本轮改动范围**：它是
--   `(part_id, kind, content_sha256)` 上的另一个 partial unique index，
--   **谓词不含 kind 白名单** ⇒ `DELIVERY_TEMPLATE` 天然被它覆盖（同 L1 同内容
--   只留一条活跃行）。这是想要的：重传同一份模板是幂等的。
--   ⚠️ 但注意上传流程里 `soft_delete_active_by_part_kind` 先跑、CAS 去重查询
--   `get_by_owner_kind_sha` 后跑且带 `deleted_at IS NULL`，所以「重传同一份」
--   走的是「软删旧行 + 重新 PUT + INSERT 新行」，CAS 命中的省流优化在
--   替换语义下取不到 —— 代价是一次幂等的 COS PUT（key 是内容寻址的），
--   登记在 `docs/api/print_template.md` §8.3 第 3 条。
-- ============================================================================

ALTER TABLE public.t_part_file DROP CONSTRAINT ck_t_part_file_kind;
ALTER TABLE public.t_part_file ADD CONSTRAINT ck_t_part_file_kind
 CHECK (kind IN ('DRAWING','3D_MODEL','G_CODE','SETUP_SHEET',
                    'ASSEMBLY_MASTER','CAD_2D','DELIVERY_TEMPLATE'));

COMMENT ON COLUMN public.t_part_file.kind IS
    'DRAWING / 3D_MODEL / G_CODE / SETUP_SHEET / ASSEMBLY_MASTER / CAD_2D / DELIVERY_TEMPLATE（2026-10-16 新增：送货单打印模板 xlsx，owner 是一级客户）';

COMMENT ON COLUMN public.t_part_file.part_id IS
    'polymorphic owner id，按 kind 判别第三种 owner：kind=DELIVERY_TEMPLATE 时装的是**一级客户**的 t_customer.id（parent_id IS NULL，由 print_template::service 校验）；kind=ASSEMBLY_MASTER 时是 t_assembly.id；其余是 t_part.id。⚠️ 表上**没有** owner_kind 列，owner 语义只在 API 层（见 files/part_file/mod.rs 的已知偏差登记）';

DROP INDEX public.uk_t_part_file_single;
CREATE UNIQUE INDEX uk_t_part_file_single ON public.t_part_file (part_id, kind)
    WHERE deleted_at IS NULL
      AND kind IN ('DRAWING','3D_MODEL','ASSEMBLY_MASTER','CAD_2D','DELIVERY_TEMPLATE');
