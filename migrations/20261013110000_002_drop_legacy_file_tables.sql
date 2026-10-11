-- 2026-10-13 files 域重组：删除 legacy 死表
--
-- 背景：`t_cnc_program` / `t_drawing_file` 自 2026-07-10 起已被 `t_part_file`
-- 取代（backend-python `core/model/part_file.py` 有同一批注）。Rust 侧对这两张表
-- **零 INSERT / 零 UPDATE / 零 SELECT**——它们只在 baseline migration 的
-- 「7. File 遗留」分组里以 DDL 形态存在（`migrations/20260925000000_001_baseline.sql`
-- 的分节注释也点名了这两张），业务代码里的 `CncProgramService` / `PartFileService`
-- 一律走 `t_part_file`。
--
-- 为什么现在才 DROP：2026-10-13 的 files 域重组把 `part_file` + `cnc_program` 两域
-- 收编进 `files` 域并清理了 12 条零调用方端点，`cnc_program` 这个名字在全仓（含
-- 数据库层）彻底消失，此时删表才不会给后人留「这里应该还有东西」的错觉。
--
-- ⚠️ append-only：本文件是**新增**的 migration，不修改任何已有 migration。
-- ⚠️ `scripts/restore_from_backup.sh` 无需同步改动 —— 它的漂移检测走
-- 「目标库（= migrations/ HEAD）没有该表」分支自动跳过，不会因本次 DROP 而硬失败。

-- t_cnc_program：序列 t_cnc_program_id_seq 由 `OWNED BY t_cnc_program.id` 绑定，
-- DROP TABLE 会连带删掉序列。
DROP TABLE IF EXISTS public.t_cnc_program;

DROP TABLE IF EXISTS public.t_drawing_file;
