//! `files::part_file` 子模块 —— `t_part_file` 表的整域
//!
//! 对应 Python myERP：
//! - api/v1/part_file.py
//! - service/part_file_service.py
//! - repository/part_file_repository.py
//! - model/part_file.py
//! - schema/part_file.py
//!
//! 2026-09-14 Phase 3：补齐 service / handler / DTO；polymorphic owner（PART / ASSEMBLY）；
//! SHA-256 CAS 去重 + COS 预签下载 URL。
//!
//! 2026-09-22 对齐 iam 范式：
//! - `repo.rs` → `repo/{mod, sql}.rs`：ZST `PartFileRepo` + 胖 trait `PartFileRepoTrait`。
//!   `PartFileRepo` 静态方法签名零 diff（跨模块调用方零修改）；`PartFileRepoTrait` 含
//!   12 方法（sql 6 + 跨域 owner 校验 2 + 软删 2 + cnc_pair 2），对 `&mut PgConnection`
//!   直接实现。
//! - `service.rs`：`PartFileService` 成为带字段结构（`Arc<SnowflakeIdGenerator>` +
//!   `Arc<dyn CosClient>`），方法签名 `<R: PartFileRepoTrait>(&self, mut repo: R, ...)`
//!   by-value；handler 借 `&mut *tx` / `&mut *conn` 喂给 trait。
//!
//! ## ⚠️ 2026-10-13 自独立 `part_file` 域收编进 `files` 域
//!
//! - 源码 `src/modules/part_file/` → `src/modules/files/part_file/`（`git mv`，内容随迁移改动）。
//! - URL 由 `/api/v2/part-files/*` **硬切**为 `/api/v2/files/objects/*`，
//!   **无 alias，旧前缀 404**。原因是 URL 语义从「零件文件」改为「文件对象」——
//!   `t_part_file` 的 owner 本来就是多态的（`owner_kind ∈ {PART, ASSEMBLY}`），
//!   `/part-files` 这个名字与实际承载面不符。
//! - 端点 12 条 → **4 条**（详见 `handler.rs` 模块 doc 的移除记录表）。
//!
//! ## 2026-10-13 PR1b：写路径收编进来（6 条旧端点 → 2 条）
//!
//! 端点 **4 条 → 6 条**。旧路径**全部无 alias、一律 404**，逐条清单见
//! `handler.rs` 模块 doc：
//!
//! | 旧 URL | 新 URL |
//! |---|---|
//! | `POST /api/v2/parts/{part_id}/upload-drawing` | `POST /api/v2/files/objects` |
//! | `POST /api/v2/parts/{part_id}/upload-3d-model` | `POST /api/v2/files/objects` |
//! | `POST /api/v2/assemblies/{assembly_id}/files` | `POST /api/v2/files/objects` |
//! | `POST /api/v2/part-files/`（PR1a 下线，本轮复活并通用化） | `POST /api/v2/files/objects` |
//! | `POST /api/v2/parts/{part_id}/files/confirm` | `POST /api/v2/files/objects/confirm` |
//! | `GET /api/v2/assemblies/{assembly_id}/files` | 删（0 调用方） |
//!
//! ⇒ 本子模块自此是**全仓唯一**持有 `t_part_file` 写入路径的域。
//!
//! ### COS key 前缀：代码侧统一 `obj/`，**容器部署下仍未统一**
//!
//! 写入路径的 CAS key 前缀读 `cfg.cos.upload_prefix`（代码缺省 `obj/`）。此前
//! `upload_file_for_owner` 把前缀硬编码成空串（新对象落桶根）、part 域旧上传读
//! 配置（生产 `.env` 配 `drawings/`），同一张表的对象分散在两处。
//!
//! ⚠️ 2026-10-13 review 第 1 轮 MAJOR-2：**闭环没合上** ——
//! `docker-compose{,-local,-staging}.yml` 的 `${COS_UPLOAD_PREFIX:-drawings/}`
//! 共 6 处仍在**根仓**（backend-rust 仓外，本 PR 结构上无法触及），
//! **容器部署下前缀仍落 `drawings/`**；代码回退值 `obj/` 只在**不经 compose** 的
//! `cargo run` 场景生效。根仓那 6 处待单独提交，本仓不要写成「前缀已统一」。
//!
//! ⚠️ **不需要数据迁移，更不要跑 `src/bin/cos_key_migrate.rs`**：
//! `t_part_file.object_key` 存的是**完整 key**，读端点
//! `PartFileService::resolve_effective_key` 先 `head_object(db_object_key)`
//! 优先命中 ⇒ 历史对象原地不动，本轮只让**新上传**统一落配置里的前缀。
//!
//! ### ⚠️ 装配件上传的**响应形状变更**
//!
//! 旧 `POST /api/v2/assemblies/{id}/files` 回 `Vec<AssemblyFileRef>`
//! （`{id, original_filename, page_count}`），`POST /files/objects` 回
//! `PartFileOut`（18 字段），**`page_count` 键消失**。无信息损失：那条上传路径
//! 本就把 `page_count` 硬编码成 `None`（`assembly/service/lifecycle.rs`），
//! PDF 页数由读时 `lopdf` 重算。
//!
//! ⚠️ 另一处行为差异（去重判据）见 `service.rs::upload_file_for_owner` 的对照表；
//! 装配件侧的实际影响是：`uk_t_part_file_single` 含 `ASSEMBLY_MASTER` ⇒
//! **一个装配件只有一条活跃的 ASSEMBLY_MASTER**，第二个**内容不同**的 PDF 会被
//! 撞唯一索引（service 映射成 21108 `BIZ_PART_FILE_DUPLICATE`；旧实现这里是裸
//! `sqlx::Error` → 500）。旧 handler 名义上支持「一次传多个 file」，实际上第 2 个
//! 就已经会失败 —— 合并只是把那个 500 换成了一个业务码。
//!
//! ### ⚠️ **零件侧**上传的响应形状与状态码变更（2026-10-13 PR1b）
//!
//! 上一节只登记了 assembly 那一条；part 侧同样变了三处，此前**三处 doc 都查不到**
//! （2026-10-13 review 第 1 轮 MINOR-5 补登记）：
//!
//! | 项 | 旧 `POST /parts/{id}/upload-drawing` / `upload-3d-model` | 新 `POST /files/objects` |
//! |---|---|---|
//! | HTTP 状态码 | **200 OK** | **201 Created** |
//! | 出参类型 | `R<TPartFile>`（16 列全量） | `R<PartFileOut>`（18 字段窄投影） |
//! | owner 键 | `part_id` | `owner_id`（+ 新增 `owner_kind` 回显） |
//! | 丢字段 | — | `updated_at` / `updated_by` / `deleted_at` |
//! | 新字段 | — | `owner_kind` |
//!
//! 三条旧 URL **均无 alias、旧路径 404**。前端适配点：状态码别再判 200、响应里
//! 的 `part_id` 改读 `owner_id`。
//!
//! ### ⚠️ **计划外**：`ASSEMBLY_MASTER` 在 confirm 路径上的接受面
//!
//! `policy::allowed_exts` 为 multipart 上传新增 `ASSEMBLY_MASTER`（取值 `&["pdf"]`，
//! 与被删的 `AssemblyService::upload_assembly_files` 判定逐字等价）**顺带**打开了
//! `POST /files/objects/confirm` 对该 kind 的接受面 —— `dto::validate::check_kind`
//! 复用同一张白名单。四合一的本意只是让装配件 PDF 能走上传端点，confirm 侧一并
//! 放开是复用的副作用、不是设计意图。
//!
//! **处置：保留**（两条写路径对 kind 的接受面一致，是同一张白名单的必然结果；
//! 收紧要另开按 kind 的白名单，那与本轮合并的目的相反），但**在此登记**，
//! 避免后人把它当成「设计如此」。配套：`bind_uploaded_file` 同步加了
//! `owner_kind` × `kind` 配对校验（`service.rs::assert_owner_kind_matches_kind`），
//! 所以 confirm 造不出「part + ASSEMBLY_MASTER」这种永不可删的行。
//!
//! ## `owner_kind` × `kind` 配对由服务端强制（2026-10-13 review 第 1 轮 MINOR-3）
//!
//! 四合一之前，配对是**结构上**保证的：`upload-drawing` 把 owner 钉在 path 段、
//! kind 硬编码；`POST /assemblies/{id}/files` 反之。合并成一个端点后两个字段都来自
//! multipart `data`，`allowed_exts` 只按 `kind` 判 ⇒ 「`owner_kind=PART` +
//! `kind=ASSEMBLY_MASTER`」这种组合会被放行，造出**永不可删**的 part 行。
//!
//! 现在由 [`policy::owner_kind_allows_kind`] 在两条写路径的**最前面**（纯内存判断，
//! 不花 DB 往返）拒绝，错误码 20104 `BIZ_INVALID_VALUE`。允许对：
//!
//! | `owner_kind` | 允许的 `kind` |
//! |---|---|
//! | `PART` | `DRAWING` / `3D_MODEL` |
//! | `ASSEMBLY` | `ASSEMBLY_MASTER` |
//!
//! fail-safe 方向：不在表内一律拒。将来给 `allowed_exts` 加新 kind（CAD_2D /
//! G_CODE / SETUP_SHEET 走 CNC 配对端点）时**不会**自动放行 —— 必须先在这里显式
//! 声明它挂哪种 owner。
//!
//! ## 角色矩阵（6 条存活端点；2026-10-10 新增 `Role::ShelfAccount` 的两条只读端点）
//!
//! 报工台（前端路由 `/scan/*`）的路由守卫就是 `SHELF_ACCOUNT` —— 车间工控机用的
//! 就是这个角色账号；它的图纸预览打的是本子模块的**列表** `GET /api/v2/files/objects`
//! 与**内容** `GET /api/v2/files/objects/{id}/content`。这两条原本只放 4 角色
//! （Manager / Clerk / Inspector / CncProgrammer）⇒ 工控机点预览必然 403（40300）。
//!
//! | 端点 | 角色集合 | 备注 |
//! |---|---|---|
//! | `POST /files/objects`（上传） | M / C / CNC | 四合一；白名单取 4 条旧上传端点的**并集** |
//! | `POST /files/objects/confirm` | M / C | 逐字沿用旧 `POST /parts/{id}/files/confirm` |
//! | `GET /files/objects`（列表） | M / C / I / CNC / **SHELF** | 2026-10-10 放开 SHELF |
//! | `GET /files/objects/{id}/content` | M / C / I / CNC / **SHELF** | 2026-10-10 放开 SHELF |
//! | `GET /files/objects/{id}/url` | M / C / I / CNC | **刻意不含 SHELF**，见下 |
//! | `POST /files/objects/{id}/delete` | 按 kind 派生（图纸类 + `ASSEMBLY_MASTER` M / C，G_CODE M / CNC） | `ASSEMBLY_MASTER` 是 2026-10-13 review 第 1 轮 MINOR-6 补进分派的 |
//!
//! ### ⚠️ 上传四合一的**唯一一处权限面放大**（2026-10-13 review 第 1 轮存疑点 4 订正）
//!
//! 4 条旧上传端点的白名单并不一致：`upload-drawing` / `upload-3d-model` 是 M+C
//! （`CRUD_PART_ROLES`）、`assemblies/{id}/files` 是 M+C、PR1a 下线又复活的
//! `POST /part-files/` 是 M+C+CNC。合并成一个端点后白名单取**并集** M+C+CNC。
//!
//! **到底放大了什么**（别笼统写成「CNC 现在也能传图纸 / 3D 模型」——那句话不准确）：
//! - **DRAWING / 3D_MODEL：没有放大**。CNC 在 PR1b 之前就能传这两类（旧
//!   `POST /part-files/` 的白名单本就是 M+C+CNC，`policy::allowed_exts` 当时也
//!   已经认这两个 kind）。变的只是**入口 URL**，能力没变。
//! - **`ASSEMBLY_MASTER`：这是唯一真实新增的能力面**。该 kind 的扩展名白名单是
//!   本轮为让装配件 PDF 走本端点才加的，加了之后 CNC 也就跟着能写装配件整图了
//!   —— 旧 `POST /assemblies/{id}/files` 是 M+C-only。
//!
//! 要收紧就按 kind 派生（但那会让「一个端点一种 kind」的白名单重新分裂，与本轮
//! 合并的目的相反）—— **本轮不收紧**，理由见 `handler.rs` 的同名注记。
//!
//! ### 2026-10-13：零件维度三条嵌套列表端点下线 ⇒ 不再有「两层闸门取交集」
//!
//! 原表里另有三条 `GET /part-files/parts/{id}/{files,cnc-programs,setup-sheets}`
//! （handler 级 4 角色）比 service 的 `list_files`（5 角色）严一档，取交集 ⇒ 4 角色。
//! **这三条已随零调用方端点清理整体下线**（见 `handler.rs` 移除记录），故本域现在
//! 不再有「handler 级闸门与 service 白名单分叉」这件事：每个端点只有一层闸门，且
//! 那层闸门就在 service 里。角色矩阵回到「一个端点一行、集合唯一」的状态。
//!
//! ### 为什么 `POST /files/objects` 不放开 SHELF
//!
//! 写路径沿用 2026-10-10 的边界：SHELF_ACCOUNT 只放开**只读**端点（列表 + content），
//! 上传 / confirm / 软删三条写路径一律拒（回归用例
//! `tests/files/part_file.rs::rbac_shelf_account_cannot_upload_or_delete`）。
//!
//! ### 为什么 `/url` 不跟着放开
//!
//! `get_file_with_url` 回的是 **COS 预签直链**（`presigned_get_url`，
//! `url_expires_in_seconds = 3600`）：链接一旦到手就能**脱离本后端**直接 GET，
//! 也能整条外发给任何人 ⇒ 它等价于「把这 1 小时的 COS 读权限发出去」，而 RBAC
//! 在签发之后就不再参与。`get_file_content` 不同：字节流由本后端代理，每次访问都
//! 重新过一次 RBAC。工控机预览需要的是「看得到图纸字节」，content 已足够 ⇒ 放开
//! content 即达成目标，`/url` 无需、也不应一并放开。
//!
//! ### ⚠️ 已知安全面（**产品已拍板的取舍，不是待办**）
//!
//! `t_part_file` 的 owner 是**多态**的（`owner_kind ∈ {PART, ASSEMBLY}` +
//! `owner_id`），`GET /files/objects` 允许客户端任意指定 `owner_id` 过滤。放开
//! SHELF_ACCOUNT 后，**工控机账号能拉取任意 `owner_id` 的文件**，没有货架级 /
//! 工单级的收窄 —— 本域的权限模型止步于「角色」，不含 `CurrentUser.shelf_ids` 那一层
//! 范围判定。这是为「工控机看图纸」这个刚需做的显式取舍；真要收窄需要先给
//! `t_part_file` 加 owner 侧的范围索引（哪张单 / 哪个货架），属独立立项。
//!
//! 本域不在 `docs/api/` 的整域覆盖清单内（该目录只覆盖部分域，判据见
//! `CLAUDE.md` 的「docs/api/ 目录约定」一节；`ls docs/api/` 复核，无 `files.md` /
//! `part_file.md`），故角色集合与端点清单的真源就是本模块 doc +
//! `parent::mod.rs` 模块 doc + 各 service / handler 端点 doc。
//!
//! ## ⚠️ 已知偏差登记（对应 `CLAUDE.md`「docs/api/ 目录约定」§1 的同名必留节，
//! 本域不在 `docs/api/` 覆盖清单内，故在此承载）
//!
//! | # | 偏差 | 状态 |
//! |---|---|---|
//! | 1 | **COS key 前缀容器部署下仍未统一**：根仓 `docker-compose{,-local,-staging}.yml` 的 `${COS_UPLOAD_PREFIX:-drawings/}` 共 6 处未改 ⇒ 走 compose 部署时新上传仍落 `drawings/`，代码缺省 `obj/` 只在 `cargo run` 生效 | **待根仓单独提交**（review 第 1 轮 MAJOR-2） |
//! | 2 | **confirm 路径顺带接受 `ASSEMBLY_MASTER`**：`allowed_exts` 加该 kind 后 `dto::validate::check_kind` 一并放行 confirm | **有意保留 + 已登记**（见上文「计划外」一节；review 第 1 轮 MINOR-5） |
//! | 3 | **`ASSEMBLY_MASTER` 曾「能造不能删」**：软删 kind 分派漏了该 kind | **已闭合**（review 第 1 轮 MINOR-6；缺口本身是旧的，`master` 上分派逐字相同，不是 PR1b 的回归） |
//! | 4 | **上传端点角色面并集含 CNC** | **有意保留 + 已登记**（真实新增面只有「CNC 能写 ASSEMBLY_MASTER」一项；见上文「唯一一处权限面放大」） |
//! | 5 | **`POST /parts/batch` 的 bindings 分支仍写死 300MB 上限**（`part/service/batch.rs::prepare_binding_head_copy`）：与本轮修掉的 `bind_uploaded_file` 硬编码同源，但那是 part 域的端点、本 PR 未触碰 | **未修**（要修需把 config 穿进 part 域 batch service，属另一次改动；此处登记以免后人以为「全仓都已读 `COS_MAX_FILE_SIZE`」） |
pub mod cnc;
pub mod dto;
pub mod handler;
pub mod model;
pub mod policy;
pub mod repo;
pub mod service;
pub mod vo;

use crate::state::AppState;
use axum::Router;
use std::sync::Arc;

/// 本子模块路由工厂（转发式：路由表在 `handler::router()`）。
///
/// ⚠️ **挂载点不在 `modules::v2_router()`**：本域作为 `files` 域的嵌套子模块，
/// 由 `files::handler::router()` 的 `.nest("/objects", …)` 挂载 ⇒ 完整路径
/// `/api/v2/files/objects/*`。这一处挂载登记在 `src/modules/mod.rs::v2_router()` 上方
/// `.nest("/files", files::router())` 的注释里。
pub fn router() -> Router<Arc<AppState>> {
    handler::router()
}
