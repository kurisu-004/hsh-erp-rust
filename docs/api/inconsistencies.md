# Rust 后端 vs Python myERP 后端接口差距报告

> 本文件由 plan `2026-08-27-split-api-docs-and-find-gaps.md` 自动生成（手动维护）。
> 权威源：Rust 后端 `src/modules/**/handler.rs` vs Python myERP `api/v1/*.py`。
> 端点数：Rust **~86**（12 域已实现 + 4 域占位 + 1 WS stub）/ Python **~169**（19 域 + 1 WS + 1 MCP）。
>
> 🔧 **本文件仅作差距清单**，不对应已落地的 Rust 实现；Rust 代码补齐由后续 plan 实施。

## 摘要

| 维度 | 数量 | 说明 |
|---|---:|---|
| 整域缺失（Python 有 Rust 无） | 4 域 | cnc_program / outsource / statistics / part_file 扩展 |
| 部分缺失（part 域） | ~28 端点 | Python 46 vs Rust 18 |
| 部分缺失（其他域） | ~5 端点 | applicants 7 vs 5；assemblies 9 vs 6 |
| Rust-only | ~7 端点 | delivery-notes 新增 P3 scan + batch-detail；worker-pool；delivery-groups |
| 占位模块（路由挂载但 Router 空） | 4 域 | cnc_program / outsource / part_file / statistics |
| WS stub（路径不一致） | 1 | Rust `/ws/dashboard` vs Python `/api/v1/ws/dashboard` |

---

## 1. 整域缺失（Python 有，Rust 整域未实现）

### 1.1 cnc_program 域 — Python 8 端点 / Rust 0

**Python 参考**：`/Users/ren/Code/myERP/api/v1/cnc_program.py`

| Method | Path | 说明 |
|---|---|---|
| POST | `/api/v1/parts/{id}/cnc-pair` | 上传 CNC 程式 + 对应零件 |
| GET | `/api/v1/parts/{id}/cnc-programs` | 列出零件的 CNC 程式 |
| POST | `/api/v1/parts/{id}/setup-sheets` | 上传 setup sheet |
| GET | `/api/v1/parts/{id}/setup-sheets` | 列出 setup sheet |
| GET | `/api/v1/cnc-programs/{file_id}/download-url` | 下载 URL 签发 |
| GET | `/api/v1/cnc-programs/{file_id}/content` | 下载二进制 |
| DELETE | `/api/v1/cnc-programs/{file_id}` | 删除 |
| （+1 internal） | — | Python `cnc_pair_router` 内部端点 |

**Rust 状态**：`src/modules/cnc_program/` 三件套（handler / service / repo）全为占位；`src/modules/mod.rs` 挂载空 `Router::new()`。

**🔧 待实施**：新建 cnc_program 域（model / repo / service / handler / dto）+ 数据库迁移（`t_cnc_program` / `t_setup_sheet` 表）+ COS 集成（与 part_file 共享）。

---

### 1.2 outsource 三件套 — Python 19 端点 / Rust 0

**Python 参考**：`/Users/ren/Code/myERP/api/v1/outsource_company.py`（8 端点）+ `outsource_quote.py`（10 端点）+ `outsource_shipment.py`（1 端点）

**Rust 状态**：`src/modules/outsource/` 三件套全为占位；空 Router。

**🔧 待实施**：完整外协域（公司 / 报价 / 发货 三子域），涉及新错误码段 212xx / 213xx / 215xx。

---

### 1.3 statistics 域 — Python 5 端点 / Rust 0

**Python 参考**：`/Users/ren/Code/myERP/api/v1/statistics.py`

| Method | Path | 说明 |
|---|---|---|
| GET | `/api/v1/statistics/overview` | 全局概览（按 status / work_type 聚合） |
| GET | `/api/v1/statistics/workers/{id}` | 单工人统计（持有批次 / 完成数） |
| GET | `/api/v1/statistics/work-types/{id}` | 单工种统计 |
| GET | `/api/v1/statistics/customers/{id}` | 单客户统计 |
| GET | `/api/v1/statistics/throughput` | 吞吐趋势（日 / 周 / 月） |

**Rust 状态**：`src/modules/statistics/` 三件套全为占位；空 Router。

**🔧 待实施**：纯查询域，可直接复用 part / assembly / delivery_note 的 repo 聚合查询。

---

### 1.4 part_file 扩展（drawing/cad） — Python 7 端点 / Rust 1 端点

**Python 参考**：`/Users/ren/Code/myERP/api/v1/drawing.py`

| Method | Path | 说明 | Rust 状态 |
|---|---|---|---|
| POST | `/api/v1/parts/{id}/drawings` | 上传图纸（2D PDF/图片） | ✅ 已实现为 `/api/v2/parts/{part_id}/upload-drawing` |
| POST | `/api/v1/parts/{id}/3d-models` | 上传 3D 模型 | ❌ 缺失 |
| POST | `/api/v1/parts/{id}/cad-files` | 上传 CAD 文件 | ❌ 缺失 |
| GET | `/api/v1/files/{id}/download-url` | 下载 URL 签发 | ❌ 缺失 |
| GET | `/api/v1/files/{id}/content` | 直接下载二进制 | ❌ 缺失 |
| DELETE | `/api/v1/files/{id}` | 删除文件 | ❌ 缺失 |
| （+1 internal） | — | `file_router` 内部端点 | — |

**Rust 状态**：`src/modules/part_file/` 三件套为占位（仅有 3 个 repo 壳函数）；空 Router。

**🔧 待实施**：补 6 端点（除 upload-drawing 外）；共享 part_file repo 的 COS 集成。

---

## 2. part 域部分缺失 — Python 46 端点 / Rust 18 端点（差 28）

**Python 参考**：`/Users/ren/Code/myERP/api/v1/part.py`（46 端点）
**Rust 当前**：`docs/api/parts/index.md`（18 端点）

### 2.1 列表/筛选（缺 ~10 端点）

| Method | Path | Python handler | 用途 |
|---|---|---|---|
| GET | `/api/v1/parts/pending-programming` | `pending_programming` | 待编程列表 |
| GET | `/api/v1/parts/outsource-in-flight` | `outsource_in_flight` | 外协在途 |
| GET | `/api/v1/parts/outsource-sendable` | `outsource_sendable` | 可发外协 |
| GET | `/api/v1/parts/inspection-batches` | `inspection_batches` | 品检批次列表 |
| GET | `/api/v1/parts/repair-batches` | `repair_batches` | 维修批次列表 |
| GET | `/api/v1/parts/repairing-batches` | `repairing_batches` | 维修中批次列表 |
| GET | `/api/v1/parts/location-tree` | `location_tree` | 库位树（按 shelf 维度） |

### 2.2 批次管理（缺 3 端点）

| Method | Path | 用途 |
|---|---|---|
| GET | `/api/v1/parts/{id}/batches` | 列出 part 下所有批次 |
| POST | `/api/v1/parts/{id}/batches/split` | 拆分批次 |
| POST | `/api/v1/parts/{id}/batches/{batch_id}/cancel` | 取消批次 |

### 2.3 状态机扩展（缺 11 端点）

| Method | Path | 触发流转 |
|---|---|---|
| POST | `/api/v1/parts/{id}/place-on-shelf` | 上架 |
| POST | `/api/v1/parts/{id}/recall-to-pending` | 召回至 PENDING |
| POST | `/api/v1/parts/{id}/send-to-programming` | 派发编程 |
| POST | `/api/v1/parts/{id}/recall-to-programming` | 召回编程 |
| POST | `/api/v1/parts/{id}/release-from-programming` | 编程完成释放 |
| POST | `/api/v1/parts/{id}/send-to-outsource` | 派发外协 |
| POST | `/api/v1/parts/{id}/receive-from-outsource` | 外协回收入库 |
| POST | `/api/v1/parts/{id}/receive-from-outsource-to-inspection` | 外协回收 → 品检 |
| POST | `/api/v1/parts/{id}/repair-dispatch` | 派发维修 |
| POST | `/api/v1/parts/{id}/start-repair` | ✅ 已实现（part lifecycle） |
| POST | `/api/v1/parts/{id}/complete-repair` | 完成维修 |

### 2.4 扫码台（缺 6 端点）

| Method | Path | 用途 |
|---|---|---|
| POST | `/api/v1/parts/scan` | 通用扫码（返回 part + 当前状态） |
| POST | `/api/v1/parts/pick-up` | 领取件（worker-scan 的旧版） |
| POST | `/api/v1/parts/scan/deliver-part` | 扫码发货 |
| GET | `/api/v1/parts/by-work-type/{work_type_id}` | 按工种查 part |
| GET | `/api/v1/parts/pickable-by-work-type/{work_type_id}` | 按工种查可领取 part |
| GET | `/api/v1/parts/by-worker/{worker_id}` | 按工人查持有 part |

### 2.5 打印（缺 2 端点）

| Method | Path | 用途 |
|---|---|---|
| POST | `/api/v1/parts/{id}/print-drawing` | 打印单件图纸 |
| POST | `/api/v1/parts/print-drawing-batch` | 批量打印图纸 |

### 2.6 流程辅助（缺 3 端点）

| Method | Path | 用途 |
|---|---|---|
| POST | `/api/v1/parts/match-by-excel-items` | Excel 行匹配 part（导入辅助） |
| POST | `/api/v1/parts/batch-update-order-info` | 批量更新订单信息 |
| GET | `/api/v1/parts/{id}/events` | 工单事件时间线（Rust delivery-note 已有 events，part 域缺失） |
| POST | `/api/v1/parts/batch-with-pdfs` | 多页 PDF 树形创建（assembly 派单场景） |

> **🔧 待实施**：part 域 28 个端点补齐涉及状态机大幅扩展 + new error codes (e.g. 20120+ PROGRAMMING_NOT_READY)。

---

## 3. 其他域部分缺失

### 3.1 applicants — Python 7 端点 / Rust 5 端点（差 2）

| Method | Path | 用途 |
|---|---|---|
| GET | `/api/v1/applicants/search` | 模糊搜索（按姓名 / 电话） |
| POST | `/api/v1/applicants/bulk-get-or-create` | 批量按名查找或创建 |

> **🔧 待实施**：补 2 端点。Rust applicants 域仅 CRUD，未含搜索 / 批量 upsert。

### 3.2 assemblies — Python 9 端点 / Rust 6 端点（差 3）

| Method | Path | 用途 |
|---|---|---|
| POST | `/api/v1/assemblies/{id}/upload-pdf` | 上传 PDF（与 create-multipart 解耦，独立端点） |
| POST | `/api/v1/assemblies/{id}/cancel` | ✅ 已实现为 `/api/v2/assemblies/{assembly_id}/cancel` |
| GET | `/api/v1/assemblies/{id}/children` | 列出子件（Rust 在 detail 中已返回 children） |
| GET | `/api/v1/parts/{part_id}/assembly` | **子件反查父装配件**（Rust 缺失） |
| GET | `/api/v1/assemblies/{id}/files` | 列出 PDF 文件（Rust 缺失，`files` 字段恒为空） |

> **🔧 待实施**：(a) `/parts/{part_id}/assembly` 反查端点；(b) `/assemblies/{id}/files` 列出文件（需 part_file 域支持）。

---

## 4. Rust-only 端点（Python 无）

| Method | Path | 模块 | 说明 |
|---|---|---|---|
| POST | `/api/v2/delivery-notes/scan` | delivery_notes | P3 扫码建单（find-or-create 草稿） |
| GET | `/api/v2/delivery-notes/batch-detail` | delivery_notes | 批量详情（按 id 列表） |
| GET / POST | `/api/v2/delivery-groups` | delivery_groups | 送货分组（Rust 新增域） |
| POST | `/api/v2/delivery-groups/{id}/update` / `soft-delete` | delivery_groups | 同上 |
| GET | `/api/v2/worker-pool/state` | worker_pool | 工人池状态查询（Rust 新增域） |
| POST | `/api/v2/admin/worker-pool/refill` | worker_pool | 管理员手动 refill |
| POST | `/api/v2/admin/worker-pool/remove` | worker_pool | 管理员手动 remove |

> 这些是 Rust 重构时**主动设计差异**（非缺失），无需向 Python 对齐。

---

## 5. Rust 占位模块（路由挂载但 Router 空）

| 模块 | 路由前缀 | handler.rs 函数数 | 状态 |
|---|---|---:|---|
| `cnc_program` | `/api/v2/cnc-programs` | 0 | mod.rs 是空 `Router::new()` |
| `outsource` | `/api/v2/outsource` | 0 | 同上 |
| `part_file` | `/api/v2/part-files` | 0 | 仅 3 个 repo 壳函数 |
| `statistics` | `/api/v2/statistics` | 0 | 空 Router |
| `dashboard` (WS) | `/ws/dashboard` | 0 | `ws_handler_stub` 空函数 |

> **🔧 待实施**：4 域三件套 + 1 WS handler 实现。优先级建议 `statistics` < `part_file` < `cnc_program` < `outsource` < `dashboard`。

---

## 6. WebSocket 路径不一致

| 维度 | Rust | Python |
|---|---|---|
| 路径 | `/ws/dashboard` | `/api/v1/ws/dashboard` |
| 实现状态 | 空 stub（`ws_handler_stub`） | 完整实现（`api/v1/ws.py::ws_dashboard`，JWT 校验通过 query `token=` 或 Authorization header） |
| 事件类型 | 已定义 `WsEvent` 枚举（`src/infra/ws_hub.rs`） | 实测可用 |

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
4. **WS 路径决策**：第 6 节列出两条路径任选其一，决定后修改 `src/modules/dashboard/handler.rs` 的 `Router::route("/ws/dashboard", ...)` 与 `src/main.rs::nest("/ws", ...)`，并同步更新 `docs/api/websocket.md`。
