# Assembly CRUD 延后项

> 本文件追踪 `feature/assembly-crud` 分支合并到 master 后**故意延后**的工作项。
> 每条均已记录：**触发场景 / 暂未实现的原因 / 后续 PR 改什么 / 关联错误码**。
> 不要在本文件追加"修复计划"或代码片段——只追踪状态。

## 1. 业务功能延后

| # | 项 | 原因 | 后续 PR 改什么 | 关联错误码 |
|---|---|---|---|---|
| 1 | **PDF 文件未上传到 COS**（`AssemblyFileRef` 始终为空） | COS 上传是 part 域的横切关注点，assembly 不应重复实现；本 PR 仅做 PDF 页数校验 | 加 `POST /api/v2/assemblies/{id}/files` 端点，参考 `part::handler::upload_drawing` | — |
| 2 | **`applicant_name` / `order_no` / `note` 三态失效**（JSON `null` 不会清字段） | DTO 端设计为非三态 `Option<String>`，service 端用 `Some(...)` 包装丢失了 NULL-clear 语义 | 改 `AssemblyUpdateRequest` 这 3 个字段为 `Option<Option<String>>` + 复用 `deserialize_optional_optional_str` 模板（`dto.rs:176` `customer_id` 已有） | — |
| 3 | **`soft_delete` 不查 `delivery_note_id IS NULL`** | 装配体本身无 `delivery_note_id` 列（子件 `t_part` 才有），需要 JOIN children 校验，查询更复杂 | service 层加 pre-check：`SELECT 1 FROM t_part WHERE assembly_id = $1 AND delivery_note_id IS NOT NULL LIMIT 1` 触发 20307 | `BIZ_ASSEMBLY_HAS_SHIPMENT` (20307) — 已预留 |
| 4 | **PENDING→IN_PROCESS 状态转移无独立端点** | 本 PR 只暴露 create / update / soft_delete / cancel 4 个写端点；IN_PROCESS 状态由后续流程（车间扫码、装配开始）触发 | 加 `POST /api/v2/assemblies/{id}/start`（Manager+Clerk + 状态机守卫） | `BIZ_INVALID_TRANSITION` (20103) |
| 5 | **`acquire_serial` 在 part 和 assembly 重复实现** | 抽到 `shared::serial` 会引入跨域重构，超出本 PR 范围 | 抽 `crate::shared::serial::acquire(conn, prefix) -> String` | — |
| 6 | **`expand_customer_id_to_l2` 用 inline recursive CTE**，不依赖 `CustomerRepo::list_l2_ids_of_l1` | customer 域尚未合并到 master；本 PR 优先保证独立性 | customer 域合并后改为调 `CustomerRepo::list_l2_ids_of_l1` | — |
| 7 | **`list_by_assembly_id` 不返回子件 `current_batch_id`** | TPart 模型本 PR 未读 `current_batch_id`；前端如有需要后续补 | 在 `AssemblyChildOut` 加 `current_batch_id: Option<i64>` 字段 | — |

## 2. 错误码 / 错误码段

| # | 项 | 原因 | 后续 PR 改什么 |
|---|---|---|---|
| 8 | **错误码 20306 `BIZ_ASSEMBLY_CHILD_PRICE_LOCKED` 已声明但本 PR 未使用** | 子件 price 锁 0 是硬编码约束（service 没暴露解锁路径），错误码占位预留 | 加端点 `POST /api/v2/assemblies/{id}/reprice-children` 或 service 层显式调用 |
| 9 | **错误码 20307 `BIZ_ASSEMBLY_HAS_SHIPMENT` 已声明但未触发** | 见上表第 3 项 | 同上 |
| 10 | **`http_status()` 映射未对 20304-20308 单独配置** | 这些码当前走默认 400；与 part/customer 段惯例一致 | 无需改（已正确） |

## 3. 集成测试 / 验证

| # | 项 | 原因 | 后续 PR 改什么 |
|---|---|---|---|
| 11 | **`.sqlx/` 离线缓存未刷新** | master 上有 3 个预先坏掉的 `query!` 站点（`part_batch/repo.rs` / `part_file/repo.rs`），重跑 `sqlx_prepare.sh` 会从 25 错误恶化到 31 错误 | 单独 PR 修 master 的 3 个 pre-existing 坏站点，再重跑 `sqlx_prepare.sh` |
| 12 | **3 个 untracked `.sqlx/query-*.json`** | 本 PR 用 `sqlx::query()`（runtime）而非 `query!` 宏，不需 `.sqlx` 缓存 | CI 离线构建如需严格可手动重跑 `sqlx_prepare.sh` 后提交 |
| 13 | **HTTP 层集成测试未覆盖**（本 PR 6 个测试全走 service 直调） | 为避免 JWT/Redis 开销，并匹配 `applicant_api.rs` 现有惯例 | 后续 PR 加 HTTP 层 happy-path / 401 / 403 集成测试 |
| 14 | **跨 worktree 集成测试隔离** | 多个 worktree 同时跑集成测试会撞 5429 / 6380 端口（worktree skill §3.5 已记录） | 改 `tests/common/mod.rs` 接受 `TEST_DATABASE_URL` env 覆盖（master 已部分支持） ✅ DONE |

## 4. 文档 / 维护

| # | 项 | 原因 | 后续 PR 改什么 |
|---|---|---|---|
| 15 | **Brief 17 处错误已修正**（详见 plan-exec-coder 过程产物 `assembly-crud-on-master-notes.md`） | implementer 阶段逐条 fix，但 brief 设计阶段已固化偏差 | 后续 plan 作者参考 notes.md 减少重犯 |
| 16 | **API 文档无 multipart 请求示例** | 现有 `docs/api/assemblies.md` 仅文字描述 data + files 字段 | 补一段完整的 `curl -F` 示例 |
| 17 | **API 文档无 6 端点错误码完整矩阵** | 本 PR 文档只列了 203xx 段错误码，未交叉到 4xxxx HTTP 段 | 在每个端点章节加 HTTP 状态码 + 错误码交叉表 |

## 5. 跟踪状态

- 本文件**只追加，不修改既有条目**（如需修改请加新条目并说明原因）
- 状态字段：`TODO` / `WIP` / `DONE` / `WONTFIX`
- 每完成一项：在该项右侧加 `✅ DONE: <commit SHA>` 链接

## 6. 相关链接

- 分支：`feature/assembly-crud`（已合并到 master）
- 合并 commit：见 `git log --oneline --merges --grep="assembly CRUD"`
- API 文档：`docs/api/assemblies.md`
- 计划文档：`/Users/ren/Code/hsh-erp-rust/.claude/plans/2026-08-27-assembly-crud-on-master.md`（不入 git，过程产物）
- 实施笔记：`/Users/ren/Code/hsh-erp-rust/.claude/worktrees/assembly-crud-on-master/.claude/plans/assembly-crud-on-master-notes.md`（不入 git，过程产物）
