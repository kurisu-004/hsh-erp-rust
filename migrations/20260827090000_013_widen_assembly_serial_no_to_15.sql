-- Migration 013: widen t_part.serial_no and t_assembly.serial_no to varchar(15)
--
-- 装配体子件 serial_no 模式 "{asm_serial}-{i:02d}"，最长 8+1+2 = 11 字符
-- (e.g. "F0000001-01")；原 varchar(8) 不够装。本迁移放宽到 varchar(15)，留 4 字符 buffer。
-- 备注：t_assembly 自身的 serial_no 仍是 8 字符 ("F0000001")；一并改是为统一 schema 形状，
-- 后续若 asm 也想带后缀也无需再迁移。

ALTER TABLE t_part ALTER COLUMN serial_no TYPE varchar(15);
ALTER TABLE t_assembly ALTER COLUMN serial_no TYPE varchar(15);
