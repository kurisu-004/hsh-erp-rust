-- Migration 020: t_process 加 color 字段
--
-- 业务背景（2026-09-12）：车间调度员希望为每道工序配置颜色码，前端工序卡片按色
-- 区分。Element Plus el-color-picker color-format="hex8" 默认输出 `#RRGGBBAA`
-- 9 字符含 alpha，与 production_management 模块的工序工种 tabbed 页配套。
--
-- 旧行默认 NULL（前端读为默认色 / 无色）。VARCHAR(9) 强约束避免误存长字符串。

ALTER TABLE t_process ADD COLUMN color VARCHAR(9);

COMMENT ON COLUMN t_process.color IS
    '前端工序卡片颜色（hex 含 alpha）；格式 #RRGGBBAA，9 字符。NULL = 未设置。';