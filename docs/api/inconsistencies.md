# Rust 后端 vs Python myERP 后端接口差距报告

> 本文件由 plan `2026-08-27-split-api-docs-and-find-gaps.md` 自动生成（手动维护）。
> 权威源：Rust 后端 `src/modules/**/handler.rs` vs Python myERP `api/v1/*.py`。
> 端点数：Rust **~162**（19 域已实现 + 1 域占位 + 1 WS stub + 1 _e2e seed）/ Python **~169**（19 域 + 1 WS + 1 MCP）。
>
> 🔧 **本文件仅作差距清单**，不对应已落地的 Rust 实现；Rust 代码补齐由后续 plan 实施。

## 摘要

| 维度 | 数量 | 说明 |
|---|---:|---|
| 整域缺失（Python 有 Rust 无） | **1 域** | 仅 statistics（聚合读占位） |
| 部分缺失（part 域） | ~3 端点 | Python 46 vs Rust 49（Rust 端大部分补齐；剩余 print-drawing-* / scan 通用端点） |
| 部分缺失（其他域） | ~3 端点 | applicants 7 vs 5；assemblies 9 vs 8；少量边角未补 |
| Rust-only | **30+** | delivery-notes 新增 P3 scan + batch-detail；worker-pool（5 端点）；_e2e seed hook（11 端点）；auto-allocate；by-work-type / pickable / by-worker；pick-up；send-to-outsource；receive-from-outsource；repair-dispatch；complete-repair；scan-inspect；scan/deliver-part；match-by-excel-items；batch-with-pdfs；assembly start + files |
| 占位模块（路由挂载但 Router 空） | **2 域** | statistics + dashboard WS 握手 |
| WS stub（路径不一致） | 1 | Rust `/ws/dashboard` vs Python `/api/v1/ws/dashboard` |

---

## 1. 整域缺失（Python 有，Rust 整域未实现）

### 1.1 cnc_program 域 — Python 8 端点 / **Rust 2 已上线**

**Python 参考**：`/Users/ren/Code/myERP/api/v1/cnc_program.py`

| Method | Path | 说明 | Rust 状态 |
|---|---|---|---|
| POST | `/api/v1/parts/{id}/cnc-pair` | 上传 CNC 程式 + 对应零件 | ✅ 已实现为 `/api/v2/cnc-programs/pairs`（multipart） |
| GET | `/api/v1/parts/{id}/cnc-programs` | 列出零件的 CNC 程式 | ✅ 已实现为 `/api/v2/cnc-programs/parts/{part_id}` |
| POST | `/api/v1/parts/{id}/setup-sheets` | 上传 setup sheet | ✅ 与 cnc-pair 合并（multipart） |
| GET | `/api/v1/parts/{id}/setup-sheets` | 列出 setup sheet | ✅ 与 cnc-programs 合并 |
| GET | `/api/v1/cnc-programs/{file_id}/download-url` | 下载 URL 签发 | ✅ 由 `part_file/{file_id}/url` 提供 |
| GET | `/api/v1/cnc-programs/{file_id}/content` | 下载二进制 | ⚪ 当前未提供直下（走 COS 预签 URL） |
| DELETE | `/api/v1/cnc-programs/{file_id}` | 删除 | ⚪ 当前未提供（part_file 域待补） |

**Rust 状态**：`src/modules/cnc_program/` 完整 6 文件（model / repo / service / handler / dto / mod）；kind=`G_CODE` + kind=`SETUP_SHEET` 复用 `t_part_file`；详见 [`./cnc-programs.md`](./cnc-programs.md)。

---

### 1.2 outsource 三件套 — Python 19 端点 / **Rust 16 已上线**

**Python 参考**：`/Users/ren/Code/myERP/api/v1/outsource_company.py`（8 端点）+ `outsource_quote.py`（10 端点）+ `outsource_shipment.py`（1 端点）

| 段 | Python | Rust | 差 |
|---|---:|---:|---:|
| `outsource_company` | 8 | **7** | -1（list-companies-by-process 实现位置不同） |
| `outsource_quote` | 10 | **8** | -2（统计 / 批量查暂未暴露） |
| `outsource_shipment` | 1 | **1** | 0 |
| **合计** | **19** | **16** | **-3** |

**Rust 状态**：`src/modules/outsource/` 完整 7 文件（含 statemachine.rs）；详见 [`./outsource-companies.md`](./outsource-companies.md) / [`./outsource-quotes.md`](./outsource-quotes.md) / [`./outsource-shipments.md`](./outsource-shipments.md)。

---

### 1.3 statistics 域 — Python 5 端点 / Rust 0（仍占位）

**Python 参考**：`/Users/ren/Code/myERP/api/v1/statistics.py`

| Method | Path | 说明 |
|---|---|---|
| GET | `/api/v1/statistics/overview` | 全局概览（按 status / work_type 聚合） |
| GET | `/api/v1/statistics/workers/{id}` | 单工人统计（持有批次 / 完成数） |
| GET | `/api/v1/statistics/work-types/{id}` | 单工种统计 |
| GET | `/api/v1/statistics/customers/{id}` | 单客户统计 |
| GET | `/api/v1/statistics/throughput` | 吞吐趋势（日 / 周 / 月） |

**Rust 状态**：`src/modules/statistics/` 仍是空 `Router::new()`；可复用 part / assembly / delivery_note 的 repo 聚合查询。

---

### 1.4 part_file 扩展 — Python 7 端点 / **Rust 3 已上线**

**Python 参考**：`/Users/ren/Code/myERP/api/v1/drawing.py`

| Method | Path | 说明 | Rust 状态 |
|---|---|---|---|
| POST | `/api/v1/parts/{id}/drawings` | 上传图纸（2D PDF/图片） | ✅ 已实现为 `/api/v2/parts/{part_id}/upload-drawing` |
| POST | `/api/v1/parts/{id}/3d-models` | 上传 3D 模型 | ✅ 已实现为 `/api/v2/parts/{part_id}/upload-3d-model` |
| POST | `/api/v1/parts/{id}/cad-files` | 上传 CAD 文件 | ✅ 已实现为 `/api/v2/part-files`（kind=`CAD_2D`） |
| GET | `/api/v1/files/{id}/download-url` | 下载 URL 签发 | ✅ 已实现为 `/api/v2/part-files/{file_id}/url` |
| GET | `/api/v1/files/{id}/content` | 直接下载二进制 | ⚪ 当前走 COS 预签 URL（302 重定向） |
| DELETE | `/api/v1/files/{id}` | 删除文件 | ⚪ 当前未提供直删端点（业务侧未要求） |

**Rust 状态**：`src/modules/part_file/` 完整 7 文件（model / repo / policy / service / handler / dto / mod）；详见 [`./files.md`](./files.md)。

---

## 2. part 域部分缺失 — Python 46 端点 / **Rust 49 端点**

**Python 参考**：`/Users/ren/Code/myERP/api/v1/part.py`（46 端点）
**Rust 当前**：[`./parts/index.md`](./parts/index.md)（**49 端点**，2026-09-14 Phase 1+2 补齐后已**超过** Python）

### 2.1 列表/筛选（Rust 已全补）

| Method | Path | Python | Rust |
|---|---|---|---|
| GET | `/api/v1/parts/pending-programming` | `pending_programming` | ✅ `/api/v2/parts/pending-programming` |
| GET | `/api/v1/parts/outsource-in-flight` | `outsource_in_flight` | ✅ `/api/v2/parts/outsource-in-flight` |
| GET | `/api/v1/parts/outsource-sendable` | `outsource_sendable` | ✅ `/api/v2/parts/outsource-sendable` |
| GET | `/api/v1/parts/inspection-batches` | `inspection_batches` | ✅ `/api/v2/parts/inspection-batches` |
| GET | `/api/v1/parts/repair-batches` | `repair_batches` | ✅ `/api/v2/parts/repair-batches` |
| GET | `/api/v1/parts/repairing-batches` | `repairing_batches` | ✅ `/api/v2/parts/repairing-batches` |
| GET | `/api/v1/parts/location-tree` | `location_tree` | ✅ `/api/v2/parts/location-tree` |

### 2.2 批次管理（Rust 已全补）

| Method | Path | Python | Rust |
|---|---|---|---|
| GET | `/api/v1/parts/{id}/batches` | list | ✅ `/api/v2/parts/{part_id}/batches` |
| POST | `/api/v1/parts/{id}/batches/split` | split | ✅ `/api/v2/parts/{part_id}/batches/split` |
| POST | `/api/v1/parts/{id}/batches/{batch_id}/cancel` | cancel | ✅ `/api/v2/parts/{part_id}/batches/{batch_id}/cancel` |

### 2.3 状态机扩展（Rust 已全补）

| Method | Path | Python | Rust |
|---|---|---|---|
| POST | `/api/v1/parts/{id}/place-on-shelf` | place-on-shelf | ✅ |
| POST | `/api/v1/parts/{id}/recall-to-pending` | recall-to-pending | ✅ |
| POST | `/api/v1/parts/{id}/send-to-programming` | send-to-programming | ✅ |
| POST | `/api/v1/parts/{id}/recall-to-programming` | recall-to-programming | ✅ |
| POST | `/api/v1/parts/{id}/release-from-programming` | release-from-programming | ✅ |
| POST | `/api/v1/parts/{id}/send-to-outsource` | send-to-outsource | ✅ |
| POST | `/api/v1/parts/{id}/receive-from-outsource` | receive-from-outsource | ✅ |
| POST | `/api/v1/parts/{id}/receive-from-outsource-to-inspection` | receive-to-inspection | ✅ |
| POST | `/api/v1/parts/{id}/repair-dispatch` | repair-dispatch | ✅ |
| POST | `/api/v1/parts/{id}/start-repair` | start-repair | ✅ |
| POST | `/api/v1/parts/{id}/complete-repair` | complete-repair | ✅ |

### 2.4 扫码台（Rust 已全补 + Rust-only）

| Method | Path | Python | Rust |
|---|---|---|---|
| POST | `/api/v1/parts/scan` | 通用扫码 | ⚪ 当前走 `worker-scan`（功能更细） |
| POST | `/api/v1/parts/pick-up` | 领取件 | ⚪ 当前走 `worker-scan` + `/pick-up` B 方案 |
| POST | `/api/v1/parts/scan/deliver-part` | 扫码发货 | ✅ |
| GET | `/api/v1/parts/by-work-type/{work_type_id}` | 按工种查 | ✅ |
| GET | `/api/v1/parts/pickable-by-work-type/{work_type_id}` | 按工种查可领取 | ✅ |
| GET | `/api/v1/parts/by-worker/{worker_id}` | 按工人查持有 | ✅ |

### 2.5 打印（仍差 2 端点）

| Method | Path | Python | Rust |
|---|---|---|---|
| GET | `/api/v1/parts/{id}/print-drawing` | 打印单件图纸 | ❌ 缺失 |
| POST | `/api/v1/parts/print-drawing-batch` | 批量打印图纸 | ❌ 缺失 |

### 2.6 流程辅助（Rust 已全补）

| Method | Path | Python | Rust |
|---|---|---|---|
| POST | `/api/v1/parts/match-by-excel-items` | Excel 行匹配 | ✅ |
| POST | `/api/v1/parts/batch-update-order-info` | 批量更新订单信息 | ✅ |
| GET | `/api/v1/parts/{id}/events` | 工单事件时间线 | ✅ |
| POST | `/api/v1/parts/batch-with-pdfs` | 多页 PDF 树形创建 | ✅ |

> **剩余 ~3 个端点**：(a) `/parts/{id}/print-drawing` 单件图纸打印；(b) `/parts/print-drawing-batch` 批量图纸打印；(c) `/parts/scan` 通用扫码（与 worker-scan 功能重叠，留待业务侧决策是否保留）。

---

## 3. 其他域部分缺失

### 3.1 applicants — Python 7 端点 / Rust 5 端点（差 2）

| Method | Path | 用途 |
|---|---|---|
| GET | `/api/v1/applicants/search` | 模糊搜索（按姓名 / 电话） |
| POST | `/api/v1/applicants/bulk-get-or-create` | 批量按名查找或创建 |

> **🔧 待实施**：补 2 端点。Rust applicants 域仅 CRUD，未含搜索 / 批量 upsert。

### 3.2 assemblies — Python 9 端点 / **Rust 8 端点**（差 1）

| Method | Path | Python | Rust |
|---|---|---|---|
| POST | `/api/v1/assemblies/{id}/upload-pdf` | 上传 PDF | ⚪ 与 multipart create 合并 |
| POST | `/api/v1/assemblies/{id}/cancel` | cancel | ✅ |
| POST | `/api/v1/assemblies/{id}/children` | 详情页添加单个子件 | ⚪ 当前不支持（创建时一次性派生） |
| GET | `/api/v1/parts/{part_id}/assembly` | 子件反查父装配件 | ⚪ 当前走 `GET /parts/{id}` 详情 |
| GET | `/api/v1/assemblies/{id}/files` | 列出 PDF 文件 | ✅（含 `files` 字段，2026-09-14 Phase 3 落地） |
| POST | `/api/v1/assemblies/{id}/start` | start | ✅（2026-09-14 Phase 3 落地） |

> **剩余 ~1 个端点**：`/assemblies/{id}/children` 详情页添加单个子件（业务未要求独立端点）。

---

## 4. Rust-only 端点（Python 无）

| Method | Path | 模块 | 说明 |
|---|---|---|---|
| POST | `/api/v2/delivery-notes/scan` | delivery_notes | P3 扫码建单（find-or-create 草稿） |
| GET | `/api/v2/delivery-notes/batch-detail` | delivery_notes | 批量详情（按 id 列表） |
| POST | `/api/v2/parts/batch-to-ship` | part | 批量通过品检（Python 仅单件 `/parts/{id}/to-ship`） |
| POST | `/api/v2/parts/batch-to-inspection` | part | 批量送检（Python 仅单件 `/parts/{id}/to-inspection`） |
| GET / POST | `/api/v2/delivery-groups` | delivery_groups | 送货分组（Rust 新增域） |
| POST | `/api/v2/delivery-groups/{id}/update` / `soft-delete` | delivery_groups | 同上 |
| GET | `/api/v2/prod/worker-pool/state` | worker_pool | 工人池状态查询（Rust 新增域） |
| POST | `/api/v2/prod/admin/worker-pool/refill` | worker_pool | 管理员手动 refill |
| POST | `/api/v2/prod/admin/worker-pool/remove` | worker_pool | 管理员手动 remove |
| POST | `/api/v2/prod/admin/worker-pool/auto-allocate` | worker_pool | auto-allocate 批量分配（Phase 2，2026-09-12） |
| POST | `/api/v2/parts/by-work-type/{wt}` / `pickable-by-work-type/{wt}` / `by-worker/{w}` | part | 工种/工人视角列表（Phase 2） |
| POST | `/api/v2/parts/{id}/pick-up` | part | B 方案手动 pick-up 兜底（Phase 2） |
| POST | `/api/v2/parts/{id}/send-to-outsource` / `receive-from-outsource` | part | 派发外协 / 外协回收（Phase 1） |
| POST | `/api/v2/parts/{id}/repair-dispatch` / `complete-repair` | part | 维修派发 / 完成（Phase 1） |
| POST | `/api/v2/parts/{id}/scan-inspect` | part | 扫码品检（Phase 1） |
| POST | `/api/v2/parts/scan/deliver-part` | part | 扫码发货（Phase 1） |
| POST | `/api/v2/parts/match-by-excel-items` / `batch-update-order-info` / `batch-with-pdfs` | part | 流程辅助（Phase 1） |
| POST | `/api/v2/assemblies/{id}/start` | assembly | 装配件启动（Phase 3 deferred #4） |
| POST | `/api/v2/assemblies/{id}/files` | assembly | 装配件独立 PDF 上传（Phase 3 deferred #1） |
| POST/GET | `/api/v2/_e2e/*`（11 端点） | _e2e | seed hook（2026-09-14，dev/test 默认） |

> 这些是 Rust 重构时**主动设计差异**（非缺失），无需向 Python 对齐。

---

## 5. Rust 占位模块（路由挂载但 Router 空）

| 模块 | 路由前缀 | handler.rs 函数数 | 状态 |
|---|---|---:|---|
| `statistics` | `/api/v2/statistics` | 0 | 5 个聚合读端点占位 |
| `dashboard` (WS) | `/ws/dashboard` | 0 | `ws_handler_stub` 空函数（待握手实现） |

> **🔧 待实施**：2 项（statistics + dashboard WS 握手）。优先级建议 `dashboard WS` < `statistics`。

---

## 6. WebSocket 路径不一致

| 维度 | Rust | Python |
|---|---|---|
| 路径 | `/ws/dashboard` | `/api/v1/ws/dashboard` |
| 实现状态 | 空 stub（`ws_handler_stub`），hub 已注册全部业务事件 | 完整实现（`api/v1/ws.py::ws_dashboard`，JWT 校验通过 query `token=` 或 Authorization header） |
| 事件类型 | 已定义 `WsEvent` 枚举（`src/infra/ws_hub.rs`）+ 全部业务事件已注册 | 实测可用 |

> **🔧 待实施**：(a) 把 WS 路径从 `/ws/dashboard` 改为 `/api/v2/ws/dashboard` 与 Python 对齐（或保留差异并文档化原因）；(b) 实现 `ws_handler_stub`：JWT 验签 + Redis session 校验 + 注册到 hub + 首连 `DashboardSnapshot` 下发。

---

## 7. MCP 端点（Rust 仓库无，Python 仓库有）

不在本仓库（Rust CLAUDE.md 已声明）。Python myERP 仓库有 `/api/mcp` 子应用（MCP server，streamable-HTTP）：

- `GET /api/mcp/parts/due` — `query_parts_due`
- `GET /api/mcp/parts/by-serial/{serial_no}` — `get_part_by_serial`
- `GET /api/mcp/files/{file_id}/content` — `get_drawing_content`（仅 HTTP 下载，非 MCP tool）

> **🔧 待决策**：是否在 Rust 仓库新建 `mcp-server` 子 crate？建议作为独立 plan 决策。

---

## 8. 维护约定

1. **新增端点时**：Rust handler 实施完 → 同步 `docs/api/<mod>.md` 或 `docs/api/<mod>/index.md` + 子文件 → 在 PR 描述点出 `docs/api/` 有变更。
2. **本文件** `inconsistencies.md` 应**每两周**（或重大端点新增后）刷新一次；删除已补齐端点，添加新发现差异。
3. **Rust-only 端点**（第 4 节）不可删除；如有调整需同步 Python（如果 Python 后续追齐）。
4. **WS 路径决策**：第 6 节列出两条路径任选其一，决定后修改 `src/modules/dashboard/mod.rs:16` 的 `.route("/dashboard", ...)` 与 `src/main.rs:104` 的 `.nest("/ws", ...)`，并同步更新 `docs/api/websocket.md`。

---

> ✅ 2026-08-28: 子件状态聚合已实现 — assembly auto-sync via inspection flow
>
> ✅ 2026-09-14: Phase 1/2/3 大批端点上线 — part 49 / assembly 8 / cnc_program 2 / part_file 3 / outsource 16；part 域部分缺失从 32 缩至 ~3；占位模块从 4 域缩至 2 域。

## 2026-09-23 PR11/12 docs/api/ drift 报告 + 修复（in-progress）

- **报告**：[docs/api/DRIFT_REPORT.md](./DRIFT_REPORT.md)（主代理主 checkout 直接 commit，2026-09-23 commit `1dca185`）
- **修复（已做）**：parts/ 域补 12 个缺失端点 + 新增 batch.md（commit `cc7654b`）
- **修复（未做）**：production/* 错误码 review / statistics.md 错误码段 / customers/applicants/shelves PR4 vo/ 字段一致性核对（drift 报告 P2/P3 项）
- **决策**：PR12 已基本完成（P0 parts 全部补齐）。P2/P3 留给后续 doc-drift PR。
