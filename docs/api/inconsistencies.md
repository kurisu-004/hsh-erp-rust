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
| 部分缺失（part 域） | ~1 端点 | Python 46 vs Rust 26（Rust 端大部分补齐；剩余 scan 通用端点） |
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
| GET/POST | `/api/v2/parts/{id}/cnc-programs`, `/setup-sheets`, `/cnc-pair`, `/cad-files` | file upload / list | ❌ 已移除兼容（2026-09-29），仅保留 `/api/v2/part-files/parts/{id}/<file>...` canonical 第二入口 |

**Rust 状态**：`src/modules/cnc_program/` 完整 6 文件（model / repo / service / handler / dto / mod）；kind=`G_CODE` + kind=`SETUP_SHEET` 复用 `t_part_file`；详见 [`./cnc-programs.md`](./cnc-programs.md)。

---

### 1.2 outsource 三件套 + sendable — Python 19 端点 / **Rust 20 已上线**

**Python 参考**：`/Users/ren/Code/myERP/api/v1/outsource_company.py`（8 端点）+ `outsource_quote.py`（10 端点）+ `outsource_shipment.py`（1 端点）

| 段 | Python | Rust | 差 |
|---|---:|---:|---:|
| `outsource_company` | 8 | **8** | 0（2026-10-03 补 `GET /{id}/sent-parts` 对账页读侧；list-companies-by-process 实现位置不同） |
| `outsource_quote` | 10 | **9** | -1（统计 / 批量查暂未暴露；2026-10-03 补 `GET /quotable-parts` picker） |
| `outsource_shipment` | 1 | **2** | +1（2026-10-03 在 `shipment_router` 上另加 `GET /in-flight`，属 Rust-only 读侧） |
| `outsource_sendable`（Rust-only） | 0 | **1** | +1（`GET /api/v2/outsource-sendable`，独立顶层前缀，APPROVAL / DIRECT 双模式） |
| **合计** | **19** | **20** | **+1**（Rust 另有 sendable 与 shipment `in-flight` 两条 Python 没有的） |

> Rust 侧计数口径：`src/modules/outsource/handler.rs` 的 4 个 router 工厂逐条点
> method 级注册（`"/"` 路径的 `get(...).post(...)` 记 2）= company 8 + quote 9
> + shipment 2 + sendable 1 = **20**。

**Rust 状态**：`src/modules/outsource/` 完整 18 个 `.rs`（模块自身 `mod.rs` +
handler / dto / model / statemachine / repo/{mod,sql} / service/{mod,company,quote,shipment,sendable}
/ vo/{mod,company,quote,quotable,sendable,shipment}）；
详见 [`./outsource-companies.md`](./outsource-companies.md) / [`./outsource-quotes.md`](./outsource-quotes.md) /
[`./outsource-shipments.md`](./outsource-shipments.md) / [`./outsource-sendable.md`](./outsource-sendable.md)。

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

## 2. part 域部分缺失 — Python 46 端点 / **Rust 26 端点**

**Python 参考**：`/Users/ren/Code/myERP/api/v1/part.py`（46 端点）
**Rust 当前**：[`./parts/index.md`](./parts/index.md)（**26 端点**，method 级口径）
逐条清单见 [`./parts/index.md`](./parts/index.md) 端点列表；端点总表
[`./index.md`](./index.md) 与 [`./DRIFT_REPORT.md`](./DRIFT_REPORT.md) 同一口径。

> **计数口径 = method 级注册数**：`src/modules/part/mod.rs` 逐个数
> `get(handler::…)` / `post(handler::…)`（`route("/")` 上的 `get().post()` 记 2 条）。
> 当前 **26**。等价推导：表登记 55 行 − 27 行已迁出（25 条 `t_part_batch` 子资源
> + 2 条外协读端点）− 2 行已下线（`send-to-programming` / `recall-to-programming`）
> = 26。
> ⚠️ 本文件与 [`./DRIFT_REPORT.md`](./DRIFT_REPORT.md) 早期版本里的 48 / 49 / 50
> 是**另一种口径**（`.route()` 调用数，漏掉 `route("/")` 上的第 2 个 method），
> 且未扣除 2026-10-02 迁往 prod 域的 25 条。

### 2.1 列表/筛选（Rust 已全补）

> 下表 `Path` 列是 **Python v1 主仓的现行路径**（`/api/v1/*`），不受 2026-10-02 的
> Rust 侧路由迁移影响；Rust 对应路径见末列。（§2.1–§2.6 各表 `Path` 列均为 Python v1 现行路径）

| Method | Path | Python | Rust |
|---|---|---|---|
| GET | `/api/v1/parts/pending-programming` | `pending_programming` | ✅ `/api/v2/parts/pending-programming` |
| GET | `/api/v1/parts/outsource-in-flight` | `outsource_in_flight` | ✅ 2026-10-03 迁往 outsource 域 → `/api/v2/outsource-shipments/in-flight`（旧 `/api/v2/parts/outsource-in-flight` **已下线、无 alias，实际返回 400**：旧实现返回通用 `PartListItem`，与前端外协域字段需求不匹配；成因见 § 9.2 的 catch-all 说明） |
| GET | `/api/v1/parts/outsource-sendable` | `outsource_sendable` | ✅ 2026-10-03 迁往 outsource 域 → `/api/v2/outsource-sendable`（旧 `/api/v2/parts/outsource-sendable` **已下线、无 alias，实际返回 400**：旧实现返回通用 `PartListItem`，缺 `send_mode` / `company_options` / `quote_id`；成因见 § 9.2 的 catch-all 说明） |
| GET | `/api/v1/parts/inspection-batches` | `inspection_batches` | ✅ `/api/v2/prod/batches/inspection` |
| GET | `/api/v1/parts/repair-batches` | `repair_batches` | ✅ `/api/v2/prod/batches/repair` |
| GET | `/api/v1/parts/repairing-batches` | `repairing_batches` | ✅ `/api/v2/prod/batches/repairing` |
| GET | `/api/v1/parts/location-tree` | `location_tree` | ✅ `/api/v2/parts/location-tree` |

### 2.2 批次管理（Rust 已全补）

| Method | Path | Python | Rust |
|---|---|---|---|
| GET | `/api/v1/parts/{id}/batches` | list | ✅ `/api/v2/parts/{part_id}/batches` |
| POST | `/api/v1/parts/{id}/batches/split` | split | ✅ `/api/v2/prod/batches/{batch_id}/split` |
| POST | `/api/v1/parts/{id}/batches/{batch_id}/cancel` | cancel | ✅ `/api/v2/prod/batches/{batch_id}/cancel` |

### 2.3 状态机扩展（Rust 已全补）

| Method | Path | Python | Rust |
|---|---|---|---|
| POST | `/api/v1/parts/{id}/place-on-shelf` | place-on-shelf | ✅ `/api/v2/prod/batches/{batch_id}/place-on-shelf` |
| POST | `/api/v1/parts/{id}/recall-to-pending` | recall-to-pending | ✅ `/api/v2/prod/batches/{batch_id}/recall-to-pending` |
| POST | `/api/v1/parts/{id}/send-to-programming` | send-to-programming | ✅ |
| POST | `/api/v1/parts/{id}/recall-to-programming` | recall-to-programming | ✅ |
| POST | `/api/v1/parts/{id}/release-from-programming` | release-from-programming | ✅ `/api/v2/prod/batches/{batch_id}/release-from-programming` |
| POST | `/api/v1/parts/{id}/send-to-outsource` | send-to-outsource | ✅ `/api/v2/prod/batches/{batch_id}/send-to-outsource` |
| POST | `/api/v1/parts/{id}/receive-from-outsource` | receive-from-outsource | ✅ `/api/v2/prod/batches/{batch_id}/receive-from-outsource` |
| POST | `/api/v1/parts/{id}/receive-from-outsource-to-inspection` | receive-to-inspection | ✅ `/api/v2/prod/batches/{batch_id}/receive-from-outsource-to-inspection` |
| POST | `/api/v1/parts/{id}/repair-dispatch` | repair-dispatch | ✅ `/api/v2/prod/batches/{batch_id}/repair-dispatch` |
| POST | `/api/v1/parts/{id}/start-repair` | start-repair | ✅ `/api/v2/prod/batches/{batch_id}/start-repair` |
| POST | `/api/v1/parts/{id}/complete-repair` | complete-repair | ✅ `/api/v2/prod/batches/{batch_id}/complete-repair` |

> 表中 ✅ 路径为 **2026-10-02 后的现行路径**：以单个批次为操作对象的端点已自
> `/api/v2/parts/*` 迁往 `/api/v2/prod/batches/*`（锚点 `part_id` → `batch_id`，硬切换无
> alias）。留在 part 域的是多批次动作（`/parts/{part_id}/cancel` / `force-complete`）
> 与非批次动作。逐条清单见
> [`./production/batches.md#2026-10-02-t_part_batch-子资源迁入`](./production/batches.md#2026-10-02-t_part_batch-子资源迁入)。

### 2.4 扫码台（Rust 已全补 + Rust-only）

| Method | Path | Python | Rust |
|---|---|---|---|
| POST | `/api/v1/parts/scan` | 通用扫码 | ⚪ 当前走 `worker-scan`（功能更细） |
| POST | `/api/v1/parts/pick-up` | 领取件 | ⚪ 当前走 `worker-scan` + `/prod/batches/{batch_id}/pick-up` B 方案 |
| POST | `/api/v1/parts/scan/deliver-part` | 扫码发货 | ✅ `/api/v2/prod/batches/scan/deliver` |
| GET | `/api/v1/parts/by-work-type/{work_type_id}` | 按工种查 | ✅ |
| GET | `/api/v1/parts/pickable-by-work-type/{work_type_id}` | 按工种查可领取 | ✅ |
| GET | `/api/v1/parts/by-worker/{worker_id}` | 按工人查持有 | ✅ |

### 2.5 打印（Rust 已全补 —— 纯 BFF 转发）

| Method | Path | Python | Rust |
|---|---|---|---|
| GET | `/api/v1/parts/{id}/print` | 打印单件图纸 | ✅ `/api/v2/parts/{part_id}/print-drawing`（**v2/v1 路径不同名**） |
| POST | `/api/v1/parts/print-batch` | 批量打印图纸 | ✅ `/api/v2/parts/print-drawing-batch`（**v2/v1 路径不同名**） |

> 两端点只做「Rust 鉴权 + RBAC → 转发」，PDF 生成由 Python 执行（Rust 侧不渲染）；
> 契约见 [`./parts/print.md`](./parts/print.md)。

### 2.6 流程辅助（Rust 已全补）

| Method | Path | Python | Rust |
|---|---|---|---|
| POST | `/api/v1/parts/match-by-excel-items` | Excel 行匹配 | ✅ |
| POST | `/api/v1/parts/batch-update-order-info` | 批量更新订单信息 | ✅ |
| GET | `/api/v1/parts/{id}/events` | 工单事件时间线 | ✅ |
| POST | `/api/v1/parts/batch-with-pdfs` | 多页 PDF 树形创建 | ✅ |

> **剩余 ~1 个端点**：`/parts/scan` 通用扫码（与 worker-scan 功能重叠，留待业务侧决策是否保留）。

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
| POST | `/api/v2/prod/batches/to-ship` | prod::batch | 静态批量通过品检（Python 仅单件 to-ship） |
| POST | `/api/v2/prod/batches/to-inspection` | prod::batch | 静态批量送检（Python 仅单件 to-inspection） |
| GET / POST | `/api/v2/delivery-groups` | delivery_groups | 送货分组（Rust 新增域） |
| POST | `/api/v2/delivery-groups/{id}/update` / `soft-delete` | delivery_groups | 同上 |
| GET | `/api/v2/prod/worker-pool/state` | worker_pool | 工人池状态查询（Rust 新增域） |
| POST | `/api/v2/prod/admin/worker-pool/refill` | worker_pool | 管理员手动 refill |
| POST | `/api/v2/prod/admin/worker-pool/remove` | worker_pool | 管理员手动 remove |
| POST | `/api/v2/prod/admin/worker-pool/auto-allocate` | worker_pool | auto-allocate 批量分配（Phase 2，2026-09-12） |
| POST | `/api/v2/parts/by-work-type/{wt}` / `pickable-by-work-type/{wt}` / `by-worker/{w}` | part | 工种/工人视角列表（Phase 2） |
| POST | `/api/v2/prod/batches/{batch_id}/pick-up` | prod::batch | B 方案手动 pick-up 兜底（Phase 2） |
| POST | `/api/v2/prod/batches/{batch_id}/send-to-outsource` / `receive-from-outsource` | prod::batch | 派发外协 / 外协回收（Phase 1） |
| POST | `/api/v2/prod/batches/{batch_id}/repair-dispatch` / `complete-repair` | prod::batch | 维修派发 / 完成（Phase 1） |
| POST | `/api/v2/prod/batches/{batch_id}/scan-inspect` | prod::batch | 扫码品检（Phase 1） |
| POST | `/api/v2/prod/batches/scan/deliver` | prod::batch | 扫码发货（Phase 1） |
| POST | `/api/v2/parts/match-by-excel-items` / `batch-update-order-info` / `batch-with-pdfs` | part | 流程辅助（Phase 1；仍留 part 域） |
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

## 2026-09-23 docs/api/ drift 报告 + 修复（in-progress）

- **报告**：[docs/api/DRIFT_REPORT.md](./DRIFT_REPORT.md)（主代理主 checkout 直接 commit，2026-09-23）
- **修复（已做）**：parts/ 域补 12 个缺失端点 + 新增 batch.md（2026-09-23）
- **修复（未做）**：production/* 错误码复核 / statistics.md 错误码段 / customers/applicants/shelves vo/ 字段一致性核对（drift 报告 P2/P3 项）
- **决策**：P0 parts 已全部补齐。P2/P3 留给后续 doc-drift 修复。


---

## 9. Rust 内部契约变更登记（非 Python 对齐差异）

本节登记 **Rust 前后端之间**的契约变更（Python myERP 不参与，故不属于第 1~7 节）。

### 9.1 2026-10-02：25 条 t_part_batch 子资源路由迁往 prod 域

`t_part_batch` 是生产执行单元，其 OCC / `status_gate` rollup / 状态机本体整体归 prod 域，
25 条「以单个批次为操作对象」的路由随之从 `/api/v2/parts/*` 迁到
`/api/v2/prod/batches/*`（**URL 硬切换，无 alias**，旧路径 404）。逐条清单见
[`./production/batches.md`](./production/batches.md#2026-10-02-t_part_batch-子资源迁入)。

**判据**：操作对象是**多个批次**或**根本不是批次**的端点留在 part 域 ——
`POST /parts/{part_id}/cancel`（翻转该 part 全部活跃批次）、
`POST /parts/{part_id}/force-complete`（全部非 CANCELLED 批次）、
`POST /parts/{part_id}/soft-delete`、`GET /parts/{part_id}/batches`，
以及全部 CRUD / 文件 / 各类 list 端点。

**BREAKING 变更**：

| 维度 | 变更前 | 变更后 |
|---|---|---|
| 19 条子资源的路径锚点 | `{part_id}` + body 内 `batch_id` | `{batch_id}` 路径参数，**body 内 `batch_id` 字段删除** |
| 3 条静态批量 / 事件（`to-ship` / `to-inspection` / `worker-scan`） | 无 Path | 仍无 Path，**请求体逐字不变**（`items[].batch_id` / `batch_id?` 保留） |
| 3 条集合读 | `GET /parts/{inspection,repair,repairing}-batches` | `GET /prod/batches/{inspection,repair,repairing}` |
| WS 事件 `kind` 字符串 | `PART_TO_SHIP` / `PART_TO_INSPECTION` / `PART_TO_PROCESS` / `BATCH_TO_*` / `WORKER_SCAN_*` | **不变** |

**错误码语义变更**（本节即该变更的登记处）：

| code | 变更前 | 变更后 |
|---|---|---|
| 20109 `BIZ_PART_BATCH_NOT_FOUND` | 传一个「**不属于该 part** 的 `batch_id`」→ 20109（靠 SQL 的 `AND part_id = $2` 判定） | **退化为**「批次不存在 / 已软删 / 状态不是流转起点」。`batch_id` 全局唯一即锚点，「跨 part 批次」不再是可表达的场景，SQL 里的 `AND part_id = $2` 冗余断言随之删除。留在 part 域的 `cancel` / `force-complete` 操作对象是该 part 的多个批次、不接受 `batch_id` 入参，故不返回 20109 |
| 20101 `BIZ_PART_NOT_FOUND` | 传了不存在的 `part_id` | **仍可达，语义不变** —— 只能经由「批次的 part 已软删」触发（service 按 `batch_id` 反查 part 后判软删） |

> 归口文档：[`./parts/inspection.md`](./parts/inspection.md) § 错误码参考（含各端点
> 「错误码」小节）、[`./parts/lifecycle.md`](./parts/lifecycle.md)、
> [`./production/batches.md`](./production/batches.md#错误码语义变更20109--20101)。

### 9.2 2026-10-03：外协 4 个读端点补齐 + 2 条错形状端点下线

前端外协模块（`hsh-erp/frontend` 的 `src/views/outsource/`）是照着一套后端从未实现的
契约写的，导致 3 个线上可见故障。本次一次性补齐读侧。

**新增 4 个 list 端点**（全部 200 OK，`R<{items,total,limit,offset}>` 分页信封）：

| Method | Path | 行粒度 | 修的故障 |
|---|---|---|---|
| GET | `/api/v2/outsource-companies/{id}/sent-parts` | shipment | 「外协对账」页 **404**（路由从未注册；写侧 reconcile-update 一直存在） |
| GET | `/api/v2/outsource-quotes/quotable-parts` | 未下发零件（一零件一行） | 报价一览页每次进报 **400**（请求被 `quote_router` 的 `/{id}`（`Path<i64>`）吞掉 → `PathRejection`）+「新建报价」picker 恒空 |
| GET | `/api/v2/outsource-shipments/in-flight` | shipment（OUTSOURCING） | 在途 tab 空白 |
| GET | `/api/v2/outsource-sendable` | 活跃批次（`current_process_id` 指向外协工序） | 可发送 tab 全灰 |

**下线 2 条错形状端点**（`src/modules/part/**`，**URL 硬切换，无 alias**）：

| 变更前 | 变更后 | 下线原因 |
|---|---|---|
| `GET /api/v2/parts/outsource-in-flight` | `GET /api/v2/outsource-shipments/in-flight` | 旧实现返回通用 `PartListItem`，缺批次级 `version` / `quantity` / 外协公司 / 客户路径 |
| `GET /api/v2/parts/outsource-sendable` | `GET /api/v2/outsource-sendable` | 旧实现返回通用 `PartListItem`，缺 `send_mode` / `company_options` / `quote_id` / `source_status`，无法表达 DIRECT 模式 |

⚠️ 旧路径的**实际 HTTP 状态码是 400 而非 404**：part 域的 `Router` 注册了
`/{part_id}`（`Path<i64>`）catch-all，matchit 静态段优先、参数段兜底 ⇒ 任何
未注册的 1 段静态路径都先落到 `/{part_id}`，再由 `Path` extractor 拒绝非数字段
（axum 0.8 的 `ErrorKind::ParseError` → 400，body 形如
`Invalid URL: Cannot parse '...' to a i64`）。这与「任意从不存在的单段路径」
（如 `/api/v2/parts/zzz-not-a-real-endpoint`）的行为**完全一致** —— 即 400 无法
区分「端点被删」与「端点从不存在」，比 404 信息量更少但也不泄漏更多信息，
故本轮不改跨域路由语义。旧 handler 已彻底删除
（`part/service/phase1/outsource.rs` 整文件移除），不再有任何 outsource 专用处理。

**其它契约变更**：

- `OutsourceInFlightItem` 的 `batch_id` / `batch_no` / `quantity` / `outsource_company_id` /
  `sent_at` 由 `Option<T>` 收成必填（新驱动 SQL 是 INNER JOIN 主导）；`version` / `quantity`
  语义改为取 **`t_part_batch`**（不是 `shipment`）—— 前端拿它们当 `receive-from-outsource`
  的 OCC 锚与部分接收 max 值。
- **删除死 VO** `ApprovedForSendItem` / `ApprovedForSendListOut`（零调用方；表达不了
  DIRECT 模式），取代者 `OutsourceSendableItem`。
- **2026-10-03 追加**：`/outsource-sendable` 的 `next_process_id` 更名为
  `current_process_id`（判据从「货架枚举 + 工艺链求交」换成
  `t_part_batch.current_process_id`）；`/outsource-quotes/quotable-parts` 的行粒度
  同时收成「一零件一行」，`shelf_id` / `shelf_code` / `next_process_id` /
  `next_process_name` 四个字段删除（报价工序改由前端从 OUTSOURCE 工序表自选）。
  两个端点**不再要求同源**。
- `customer_path` 改为真算（2026-10-03）：`OutsourceQuoteOut` 经
  `part_map_for_quote` 扩 2 列带 L1/L2 客户名，`OutsourceShipmentOut`（`reconcile-update`
  写端点出参）经新增的 repo 方法 `part_customer_names` —— 后者的两条
  `LEFT JOIN t_customer` 与 list 侧 `OutsourceSentPartRow` 逐条一致，故同一条 shipment
  经写端点回读与经 `sent-parts` 读到的 `customer_path` 恒相同（此前是「列表有值、
  编辑回读变空」的不一致）。

> 归口文档：[`./outsource-companies.md`](./outsource-companies.md) /
> [`./outsource-quotes.md`](./outsource-quotes.md) /
> [`./outsource-shipments.md`](./outsource-shipments.md) /
> [`./outsource-sendable.md`](./outsource-sendable.md) /
> [`./parts/lifecycle.md`](./parts/lifecycle.md)。

### 9.3 2026-10-04：报工台放回页的工序链适配（`by-worker` 出参增量）

报工台「放回」要判定三态：链内有下一道 ⇒ 免填并提示下一道；当前工序是链内最后一道 ⇒
提示「加工完成后请送检」；无链 / 位置漂移 / 位置有歧义 ⇒ 弹工序选择框。放回页的唯一
数据源是 `GET /api/v2/parts/by-worker/{worker_id}`，本次在该端点的行出参上补齐判据。

**新增 4 个字段**（`PartListItem`，**仅本端点填**，其余 6 处返回点恒默认值）：

| 字段 | 类型 | 取值 |
|---|---|---|
| `chain_state` | string | 三值互斥枚举：`"NONE"` / `"NEXT"` / `"TAIL"`（`rename_all = "UPPERCASE"`） |
| `chain_next_process_id` | string (i64) | 下一道工序 id；非可空 + `"0"` 兜底（与 `outsource-pool` 的 `receive_next_process_id` 同款约定） |
| `chain_next_process_name` | string? | 下一道工序名（`t_process.name`） |
| `chain_current_process_name` | string? | 当前工序名（`TAIL` 提示点名用） |

三值用**一个枚举**而不是两个 bool：两个 bool 会产生「可免填 + 是链尾」这类自相矛盾的
组合，前端必须自己排优先级，而排错的后果是静默把工件投到错误工序。

**同一取行 SQL 一并补齐的两列投影**（同为本端点填）：

- `batch_id` / `batch_version` —— 本端点的行本来就是「批次行」，但出参 VO 只有 part 级
  字段，放回页拿不到批次 id 就发不出写请求。这两个字段的口径已改为「pickable 与
  by-worker 都填」。
- `process_chain_id` —— 取行 SQL 现在投影该列，VO 不再硬编码 `None`。

**派生口径的三条硬约束**（读侧自己说了不算，必须与写侧同源 / 必须自己识别歧义）：

1. **锚链内「当前工序的位置」必须按 `b.current_process_id` 重新定位**，不能拿
   `b.current_process_step_id` 的 `sort_order` 当位置 —— worker-scan 的 RETURNED 分支
   只写 `current_process_id`、不推进 step 指针（见
   [`./parts/inspection.md`](./parts/inspection.md) worker-scan 业务流转节），
   多工序链批次第 2 次放回时指针仍停在首次定位那一步，按位置推进会把**当前工序自己**
   当成下一道返回，而 `chain_state` 仍在说「可免填」⇒ 静默错值比拒收更难发现。
2. **「下一道」按 `sort_order > 当前 ORDER BY sort_order ASC LIMIT 1` 取**，与写侧
   `prod::process_chain::repo::query::next_step_in_chain` 逐条同形。**不能**用
   `sort_order = 当前 + 1`：`sort_order` 的**密度不由读侧决定**，写侧只保证链内
   `sort_order` 互不重复（`upsert_chain` 校验 + `uq_chain_step_chain_order` 兜底），
   稠密 0-based（前端 `usePartProcessDesign` 保存时拍平）与稀疏 `10/20/30` 两种密度
   都能落库；`+ 1` 只在稠密下正确，`>` 对两种密度都成立。
3. **链内同一 `process_id` 重复 ⇒ 显式落 `NONE`**。`t_process_chain_step` 只有
   `uq_chain_step_chain_order (chain_id, sort_order) WHERE deleted_at IS NULL` 一个
   唯一约束，**没有** `(chain_id, process_id)` 唯一约束；写侧 `upsert_chain` 也只校验
   `sort_order` 重复、不校验 `process_id` 重复 ⇒ 重复工序的链后端照收。此时按
   `process_id` 定位当前 step 会扇出多行（一行派生 `NEXT → 当前工序自己`、另一行派生
   `TAIL`），让 `LIMIT 1` 静默取其一就是拿「绝不能把当前工序自己当成下一道」这条承诺
   去赌 PG 的行序。取行 SQL 用 `(count(*) OVER ())` 带出链内命中数，命中 >1 时显式落
   `NONE` 并门控全部派生列。

> 归口文档：[`./parts/lifecycle.md`](./parts/lifecycle.md)
> `GET /api/v2/parts/by-worker/{worker_id}` 节（字段表 + 三值语义表 + 派生口径）、
> [`./parts/index.md`](./parts/index.md) 的 `PartListItem` 字段表与前端配套改动清单。

### 9.4 2026-10-04 登记：外协 `list_held` 的「下一道」仍用 `sort_order + 1`

**本次不修**，登记以免下一个读 `OutsourcePoolRepo::list_held` 的人把它当正典抄。

`src/modules/outsource/repo/sql.rs` 的 `list_held` 取「下一道」写的是
`nsp.sort_order = cur2.sort_order + 1`，其 doc 用唯一索引
`uq_chain_step_chain_order (chain_id, sort_order)` 论证「下一道唯一无歧义」——
**这是 non-sequitur**：该索引只保证一个 `sort` 槽位唯一，不蕴含下一道落在 `+1`。
同样的口径已扩散到 [`./outsource-pool.md`](./outsource-pool.md)。

与 §9.3 的 `by-worker` 派生是同源问题的两侧：`by-worker` 走 `>` 是因为要与写侧
`next_step_in_chain` 同形、且对稠密 / 稀疏两种密度都成立；`list_held` 的 `+ 1` 只在
稠密下成立，一旦库里出现稀疏 `sort_order` 的链（`docs/api/production/process-chain.md`
记的正是稀疏口径），外协看板的接收提示就会把「还有下一道」说成链尾。

本次不修的理由：

- 该行在本次改动之前就存在、本次 diff 未触碰；
- 当前真实写路径（前端保存时把 `sort_order` 拍平成稠密 0-based）下结果是**潜在**
  缺陷而非活跃 bug；
- ⚠️ **别把 [`./production/process-chain.md`](./production/process-chain.md) 当密度
  依据**：那份文档记的写入口径（`sort_order` 默认稀疏 `10/20/30`、中间插入取
  `(prev+next)/2`、精度耗尽走 `reorder_with_step_size` 批量重排）与真实写路径不符 ——
  前端工序链编辑页保存时经 `upsertSteps` / `reorderSteps` 把 `sort_order` 按下标**拍平为
  稠密 0-based**，文档提到的重排路径也未实装触发条件。文档漂移本身不在本次修复范围，
  但它正是稀疏口径的传播源：读侧无论库里是哪种密度都只能用 `>`（与写侧正典
  `next_step_in_chain` 同形），不能用 `+ 1`；
- 外协出参把「无下一道」与「链不可解析」塌成同一个 `chain_resolvable = false`
  （与 `by-worker` 的三值不同），改它要连带复核文档 3 处 + 约 499 行测试期望；
- 属另一个变更，应当独立成一次带回归测试的修复。
