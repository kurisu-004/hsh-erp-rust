# migrations/archive/ —— 历史 migration 归档

> ⚠️ **本目录不参与 `sqlx::migrate!()` 扫描**。本目录下文件仅作历史参考，
> 不会被 sqlx 编译期宏加载，也不会被运行时执行。

## 来源

2026-09-25 sqlx 接管重构前的 29 个 migration 文件：
- `20260811100001_001_create_auth_menu_tables.sql` ~ `20260827090000_013_widen_assembly_serial_no_to_15.sql`
  —— 纯 DDL 建表 / 索引 / 改列
- `20260911093000_015_backfill_initial_part_batches.sql` —— 业务数据 backfill
- `20260911100000_016_add_work_type_max_held_minutes.sql` ~ `20260916140000_029_add_missing_indexes_and_drop_orphan_seq.sql`
  —— 列变更 + 新表 + 索引

## 为什么不直接删

- 历史回溯价值：review / 审计时可看每一步的演化
- 生产重建脚本 `scripts/rebuild_prod_from_backup.sh` 走的是另一条路径（pg_restore + schema delta），
  不依赖此目录

## 重组方案

29 个文件已合并为单文件 `migrations/20260925000000_001_baseline.sql`（已剔除 018/021/023/024/025 的
菜单 DML，菜单数据由 `seeds/menu.sql` 接管）。

**对比表**：

| 原文件 | 归宿 |
|---|---|
| 001-014 | DDL 进 baseline |
| 015 | CREATE SEQUENCE 进 baseline；INSERT 部分（数据 backfill）空跑（fresh DB 无 part） |
| 016-017, 019-020, 022, 026-029 | DDL 进 baseline |
| 018, 021, 023, 024, 025 | 菜单 DML **剥离** → `seeds/menu.sql` |

如果你需要从某个旧 migration 的某个变更反向追溯到 baseline 中的对应段落，
用 `grep -n '<table or column>' migrations/20260925000000_001_baseline.sql` 定位。