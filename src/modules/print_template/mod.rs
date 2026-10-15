//! `print_template` 域 —— 送货单打印模板（存 + 绑 + 取字节），**2026-10-16 新建**
//!
//! 需求（用户原话）：「增加模板上传的功能，模板支持 xlsx 格式，当前用于打印送货单。
//! 前端的模板管理页面增加上传模板和查看已有模板的功能。且可以绑定一级客户。
//! 送货单打印时自动选择已绑定的模板」。
//!
//! ## 端点（**4 条**，挂在 `/api/v2/print-templates`，由 [`router`] 桥接）
//!
//! | # | Method | Path | 守卫 | 说明 |
//! |---|---|---|---|---|
//! | 1 | `GET` | `/` | `require_role(Manager)` | 列表，可选 `?customer_id=` + `limit` / `offset` |
//! | 2 | `GET` | `/resolve` | 权限点 `delivery_note.print` | `?customer_id=<L1>` → 模板；未绑定 ⇒ `21109` |
//! | 3 | `GET` | `/{template_id}/content` | 权限点 `delivery_note.print` | 后端代理 xlsx 字节（裸字节，非 `R<T>`） |
//! | 4 | `POST` | `/{template_id}/delete` | `require_role(Manager)` | 软删，body **必含 `version`**（OCC）⇒ 40901 |
//!
//! ⚠️ **本域没有写端点**：模板的写入走 `files` 域的统一收文件链路
//! （`POST /files/objects/upload-intents`（`owner_kind=CUSTOMER` +
//! `kind=DELIVERY_TEMPLATE`）→ 前端直传 COS → `POST /files/objects/confirm`），
//! 「一个 L1 至多一份活跃模板」的替换语义由 confirm 事务内的「软删旧行 + INSERT」
//! 承接。完整流程见 `docs/api/print_template.md` §1.0。
//!
//! **权威路由清单是 `handler::ROUTES`（4 条）**，lib 单测
//! `handler::tests::routes_declared_in_router` 用
//! `crate::modules::files::route_guard::routes_in_router_source` 逐条比对 `router()`
//! 源码（复用该共用比对器，**不**复制一份）。
//!
//! ## URL 与硬切记录
//!
//! 本域**全新**，**无任何旧路径、无 alias**。`/api/v2/print-templates` 与前端菜单
//! 的**子项** `print_templates_designer`（`template_management` →「模板管理」下的
//! 「模板编辑」）的 `path='/print-templates'` **逐字一致**（刻意不写成 kebab-case 的
//! 别名词）。
//! ⚠️ 2026-10-16 review 第 1 轮 MINOR-4 订正：一级菜单 `template_management`
//! 自己的 `path` 是 **NULL**（目录项、不指向页面，见 `seeds/menu.sql` §1）——
//! 本域对应的**只是**那个子菜单项，别把 path 记在目录项名下。
//!
//! 两条**曾经出现过、但从未落到 Rust 侧**的相近前缀，本仓同样不存在，
//! 一律**干净 404**（集成测试 `tests/print_template/resolve.rs` 有钉死用例）：
//! - `/api/v2/com/delivery/template/*` —— 送货单域内的模板路径，该域 17 端点里
//!   从来没有过模板端点（见 `docs/api/delivery_note.md` §6 的移除记录）；
//! - `/api/v2/print-template/*`（**少个 `s`**）—— 前端拼错 URL 时的形态，不设 alias。
//!
//! ## ⚠️ 为什么不新建模板表（本域最核心的决策）
//!
//! 模板的**存在性**与**绑定关系**是同一件事：「某一级客户名下存在一行未软删的
//! `kind='DELIVERY_TEMPLATE'` 的 `t_part_file`」**本身就是绑定关系**。据此：
//!
//! | 如果新建模板表 | 本域的实际做法 |
//! |---|---|
//! | 要多一套 `id / version / created_* / updated_* / deleted_at` 列 | 全部复用 `t_part_file` 既有列（`version` 直接当 OCC 锚） |
//! | 模板字节仍得落 COS、仍得算 CAS key、仍受 `COS_MAX_FILE_SIZE` 约束 | 一行不加（复用 `util::cos_key::build_cas_key` + `files` 域两条写入口） |
//! | 「一个 L1 至多一份活跃模板」要另建唯一索引 | 复用 `uk_t_part_file_single`（2026-10-16 迁移把 `DELIVERY_TEMPLATE` 加进它的 kind 白名单） |
//! | 删除要写两张表 + 处理两套软删语义 | 只有一张表的一次软删 |
//!
//! 代价是**读侧必须始终带 `kind` 谓词**（`t_part_file.part_id` 一个列空间现承载
//! 三种 owner：零件 / 装配件 / 一级客户），登记在 `docs/api/print_template.md`
//! §8.3 与 `files/part_file/mod.rs` 的「已知偏差登记」第 10 条。本域 4 条端点
//! **全部**按 `(part_id, kind='DELIVERY_TEMPLATE')` 取，**从不**依赖通用列表端点。
//!
//! ## 绑定粒度：只绑一级客户（`t_customer.parent_id IS NULL`）
//!
//! 用户拍板。送货单的 `customer_path` 是 L1 / L2 两级，但打印抬头、地址、抬头行
//! 数与顺序是**按客户**的：L2 与其父 L1 往往是同一套抬头，绑到 L2 会立刻出现
//! 「同一个客户名下两个 L2 模板打架」。
//!
//! ⚠️ **L1-only 只有**一道**强制点，错误码 21105**：
//! `PartFileRepoTrait::customer_owner_exists`（谓词含 `AND parent_id IS NULL`），
//! 它是 `files` 域两条写入口**共用**的闸门 —— 三种失败情形（不存在 / 已软删 /
//! 不是一级客户）**刻意不区分**，message 里带原始 id 让前端改选。
//! ⇒ 前端看到 21105（HTTP 404）的含义是「你给的那个 id 不能当模板的 owner」，
//! 处理方式是**改选客户**，不要在 21105 上做「重试」或「换个入口」。
//!
//! ⚠️ **别因为「files 域不该知道客户分两级」把谓词去掉** —— files 域判的是
//! `parent_id IS NULL` 这个**列取值**，与它判 `t_part.assembly_id IS NULL`
//! （散件）同形。完整取舍见 `docs/api/print_template.md` §4.1。
//!
//! ## 守卫：两条读端点**绝不能** MANAGER-only
//!
//! 打印对话框是 **CLERK / INSPECTOR** 在用（备货、点检、装车都要打单），不是管理
//! 员的活。故端点 2（resolve）/ 3（content）用**权限点** `delivery_note.print`
//! （默认矩阵恰是 MANAGER / CLERK / INSPECTOR，见 `seeds/rbac.sql`），端点
//! 1（列表）/ 4（软删）用 `require_role(Manager)`（模板是配置，改它会影响别人
//! 出货单上的抬头）。
//!
//! ⚠️ 这**不是** RBAC 面板那套「面板是权限真源、用权限点守会自锁」的情形 ——
//! 面板自身才需要角色兜底（`docs/api/iam.md` §9），本域不是权限真源。
//!
//! ## 本域**刻意不装** `assert_no_foreign_domain` 域隔离护栏
//!
//! 与 `prod::scan` / `wx` 同款「**转发型**域」：本域的 `t_part_file` 读写**全部**经
//! `files::part_file`（`get_file_row` / `get_file_content_for_row` /
//! `soft_delete_file` / `list_with_filters`；写入经该域的两条写入口），
//! 这是设计（复用 `t_part_file`，见上表）而不是越界。
//! 硬装这道护栏只会逼出「把 files 域的写法复制一份到本域」这种更糟的结果。
//! 本域对 `t_customer` 的三处读（`parent_id IS NULL` 强制点**顺带取名**、`resolve`
//! 的客户名、列表的批量客户名）SQL 都写在本域 service 里，属既定的「只读跨域
//! 聚合」pattern（`dashboard` / `statistics` / `prod::inspection` 同款），
//! **不 import** `com::customer` 任何符号。⚠️ 列表那处**必须**是一条
//! `id = ANY($1)`（不许逐行查）：见 `service.rs` 模块 doc 的「取数恒 ≤ 3 条语句」
//! 一节与文件末尾的 `mod query_guard` 源码级护栏。
//!
//! ⚠️ 端点 3 取字节走的是 files 域最底层的 `get_file_content_for_row`
//! （**不判权限、不判 kind、不查库**）而不是 `get_file_content_of_checked_row`：权限点
//! `delivery_note.print` 由本域 handler 判（本域刻意不套 files 域那张 5 角色表
//! —— 一个只持该权限点、不在表内的账号不该被二次拒绝），kind 校验在本域 service
//! 里比 `row.kind`（错误码 **21101**，files 域的白名单侧是 **21102**，刻意不同）。
//!
//! ## 结构（**转发式**，见 CLAUDE.md「路由声明规约」第 9 条）
//!
//! `mod.rs` 只放 `pub fn router()`，路由表在 `handler::router()`。
//! **不建 `repo.rs` / `model.rs`** —— 本域没有自有表：
//! - `t_part_file` 的读写走 `files::part_file` 的既有 `PartFileRepoTrait`
//!   （`get_by_part_kind` / `get_by_id` / `soft_delete_file` / `list_with_filters`
//!   都是既有方法，逐字复用）；
//! - `t_customer` 的只读查询直接写在 `service.rs`（运行时 `sqlx::query_scalar` /
//!   `sqlx::query_as`，**不进 `query!` 宏**、不需要 `.sqlx/` 缓存）。
//!
//! 范本：本域自己就是「复用 `t_part_file`、无 repo 层」的样板。
//!
//! ## 完整契约
//!
//! [`docs/api/print_template.md`](../../../../docs/api/print_template.md)
//! —— 端点全表 / 逐字段 / 口径表 / 移除记录 / WS 关系 / 前端配套 / 已知偏差登记。

use std::sync::Arc;

use axum::Router;

use crate::state::AppState;

pub mod dto;
pub mod handler;
pub mod service;
pub mod vo;

/// 本域 router 工厂（转发式：路由表在 [`handler::router`]）。
///
/// 挂载点：`src/modules/mod.rs::v2_router()` 的 `.nest("/print-templates", …)`
/// —— 顶层 nest、单段、kebab-case，与前端菜单 `path='/print-templates'` 逐字一致。
pub fn router() -> Router<Arc<AppState>> {
    handler::router()
}
