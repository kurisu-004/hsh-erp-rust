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
//! | `CUSTOMER` | `DELIVERY_TEMPLATE`（**2026-10-16 新增**，见下一节） |
//!
//! fail-safe 方向：不在表内一律拒。将来给 `allowed_exts` 加新 kind（CAD_2D /
//! G_CODE / SETUP_SHEET 走 CNC 配对端点）时**不会**自动放行 —— 必须先在这里显式
//! 声明它挂哪种 owner。
//!
//! ## 2026-10-16 新增第三种 owner `CUSTOMER`（送货单打印模板）
//!
//! 需求：「增加模板上传的功能，模板支持 xlsx 格式，当前用于打印送货单……且可以
//! 绑定一级客户。送货单打印时自动选择已绑定的模板」。
//!
//! **模板字节不新建表**，复用本表的 `t_part_file`：`kind='DELIVERY_TEMPLATE'`，
//! `part_id` 列装的是**一级客户的 `t_customer.id`**，「该 L1 名下存在一行未软删的
//! `DELIVERY_TEMPLATE`」本身就是绑定关系。承载域是新建的 `print_template` 域
//! （`src/modules/print_template/`，URL `/api/v2/print-templates`，5 端点），
//! 完整契约见 `docs/api/print_template.md`。本域只出**九处**能力：
//! ① `kind` 白名单 / 扩展名 / file_type / content_type 四张表加了 `DELIVERY_TEMPLATE`；
//! ② `owner_kind='CUSTOMER'` 的闸门（`repo::customer_owner_exists`，⚠️ **含
//!    `parent_id IS NULL`** —— 只绑一级客户，2026-10-16 review 第 2 轮 MAJOR-A 补）；
//! ③ `owner_kind × kind` 配对表加 `CUSTOMER => DELIVERY_TEMPLATE`；
//! ④ 两条**写**入口的角色分派共用 `policy::write_roles_for`（形参是
//!    `WriteEntry` 入口枚举，**不是**可传错的常量集合），且 confirm 直接拒
//!    `owner_kind='CUSTOMER'`；
//! ⑤ 两条**取字节**端点（`content` / `url`）共用 `policy::is_file_content_kind`；
//! ⑥ 通用列表端点 `exclude_kind` 掉 `DELIVERY_TEMPLATE`；
//! ⑦ 软删分派表加 `DELIVERY_TEMPLATE => [Manager]`；
//! ⑧ `upload_file_for_owner` 的 CUSTOMER 臂（④ 的其中一半）；
//! ⑨ 取字节的**唯一** service 入口 `get_file_content_of_checked_row`（⚠️ 不判 kind，
//!    判据归调用方；review 第 2 轮 MINOR-A 把那个「有角色闸门、无 kind 闸门」的
//!    `get_file_content` **整条删除**了，见下）。
//! 逐条理由见下面的收紧表与已知偏差登记表。
//!
//! ### ⚠️ **六处**刻意收紧（新增第三种 owner 带来的；2026-10-16 review 第 1 轮补齐、
//! 第 2 轮再加一处 L1 判据）
//!
//! | 收紧点 | 位置 | 为什么 |
//! |---|---|---|
//! | 上传端点的角色面按 `owner_kind` 分派 | `policy::write_roles_for` + `service::upload_file_for_owner` | `CUSTOMER` 只放 **Manager**。⚠️ `owner_kind` / `kind` 都由 client 在 multipart `data` 里给，只在 `print_template` 域的 handler 上守 MANAGER 的话，CLERK 直接打 `POST /files/objects` 就能绕过模板管理页乱换别人的打印模板。`PART` / `ASSEMBLY` 的集合**逐字不变** |
//! | **两条写入口都收 CUSTOMER 的「只绑一级客户」** | `repo::customer_owner_exists` 的 `AND parent_id IS NULL`（review 第 2 轮 MAJOR-A 补） | ⚠️ 第 1 轮给 confirm 加「直接拒 CUSTOMER」的理由逐字是「该端点没有 L1 强制点，放行等于给 Manager 一条绕过 L1-only 的路」—— **同一句理由逐字适用于 `POST /files/objects`**，而首版在那边只收了角色、没收 L1 ⇒ Manager 能造出绑在二级客户上的孤儿模板行（详见已知偏差第 15 条的三条后果）。修法不是「再拒一次 CUSTOMER」（那会让 `POST /files/objects` 彻底不能写 CUSTOMER、破坏 `print_template` 域的复用路径），而是**把 L1 判据下沉进两道闸门共用的原语** |
//! | **confirm 端点（第二条写入口）同样分派 + 直接拒 CUSTOMER** | `policy::write_roles_for` + `service::bind_uploaded_file` | ⚠️ **2026-10-16 review 第 1 轮 MAJOR-1 补的漏**：首版只给 `upload_file_for_owner` 加了 CUSTOMER 分支，而 confirm 的角色闸门恒是 `M + C`、与 `owner_kind` 无关 ⇒ CLERK 打 `POST /files/objects/confirm` 能造出并替换掉任意客户的模板行。现两条入口**共用同一张分派表**（`policy::write_roles_for`，防「下次新增 owner_kind 又只改一条」），且 confirm 额外**拒 `owner_kind='CUSTOMER'`**（20104）—— 它没有本域那条「只绑一级客户」的强制点，放行就等于给 Manager 一条绕过 L1-only 的路，而它零个合法 CUSTOMER 调用方 |
//! | 软删分派表加 `DELIVERY_TEMPLATE => [Manager]` | `service::soft_delete_file` | 不加就是「写得进、删不掉」，与 2026-10-13 review 第 1 轮 MINOR-6 修的 `ASSEMBLY_MASTER` 同款坑 |
//! | **两条取字节端点共用** kind 白名单 | `policy::is_file_content_kind`（`content` + `/url` 都调） | `content` 端点**放 `SHELF_ACCOUNT`**（工控机预览图纸）且原先不校验 kind ⇒ 工控机账号能按 id 取到**任意客户的送货单打印模板**字节。⚠️ **2026-10-16 review 第 1 轮 MAJOR-2 补的漏**：首版只给 `content` 加了白名单，而 `/url` 的守卫是权限点 `part.drawing.print`（默认矩阵 M / C / I / CNC）、回的是**1 小时 COS 预签直链** ⇒ CLERK / INSPECTOR / CNC 完全绕得过去。守卫取白名单而非「排除 DELIVERY_TEMPLATE」，新增 kind 时忘了加 = 拒绝（fail-safe） |
//! | **通用列表端点永不返回模板行** | `service::list_files`（`list_with_filters` 的 `exclude_kind` 形参） | ⚠️ **2026-10-16 review 第 1 轮 MAJOR-3 补的漏**：`PartFileListQuery.kind` 缺省时**不带 kind 谓词**，而出参含 `owner_id`(= 一级客户 id) / `object_key` / `original_filename` / `content_sha256`，端点角色集合**含 `SHELF_ACCOUNT`** ⇒ 工控机账号不传 `kind` 就能列出全部客户的模板元数据并拿到 id（正是上面那条越权读链的「发现」环节）。⚠️ **无条件排除**（显式 `?kind=DELIVERY_TEMPLATE` 也是空）—— 只在缺省时排除的话，补一个 `kind` 参数就绕回来了（`kind` 是自由文本、不做枚举校验） |
//!
//! ### ⚠️ `CUSTOMER` 是**单向**的
//!
//! `PART` / `ASSEMBLY` 两臂刻意**不加** `DELIVERY_TEMPLATE` —— 否则能造出「零件挂
//! 送货单打印模板」这种**无人读**（`print_template` 域只按
//! `(customer_id, DELIVERY_TEMPLATE)` 查）的垃圾行。
//!
//! ### ⚠️ 扩展名 / file_type / content_type 三处的新增值
//!
//! `DELIVERY_TEMPLATE => &["xlsx"]`（**不收 `.xls`**：BIFF 二进制格式，前端
//! SheetJS / hucre 侧读它要走另一条兼容路径，需求原文就是 xlsx）；
//! `"xlsx" => "XLSX"`；content_type **只收**精确的
//! `application/vnd.openxmlformats-officedocument.spreadsheetml.sheet`，
//! **刻意不给 `application/octet-stream` 兜底**（同 pdf 那支的纪律）——
//! 收下它等于放行「改名成 `.xlsx` 的任意文件」。
//!
//! ## 角色矩阵（7 条存活端点；2026-10-10 新增 `Role::ShelfAccount` 的两条只读端点，
//! 2026-10-13 PR2 新增预览端点）
//!
//! 报工台（前端路由 `/scan/*`）的路由守卫就是 `SHELF_ACCOUNT` —— 车间工控机用的
//! 就是这个角色账号；它的图纸预览打的是本子模块的**列表** `GET /api/v2/files/objects`
//! 与**内容** `GET /api/v2/files/objects/{id}/content`。这两条原本只放 4 角色
//! （Manager / Clerk / Inspector / CncProgrammer）⇒ 工控机点预览必然 403（40300）。
//!
//! | 端点 | 角色集合 | 备注 |
//! |---|---|---|
//! | `POST /files/objects`（上传） | `PART`/`ASSEMBLY`：M / C / CNC；**`CUSTOMER`：仅 M** | 四合一；白名单取 4 条旧上传端点的**并集**。⚠️ 2026-10-16 起按 `owner_kind` 分派（`policy::write_roles_for`，模板只放 Manager，理由见「2026-10-16 新增第三种 owner」一节） |
//! | `POST /files/objects/confirm` | M / C；**`CUSTOMER`：仅 M，且随后直接 20104 拒收** | 逐字沿用旧 `POST /parts/{id}/files/confirm`（M+C）；⚠️ 2026-10-16 review 第 1 轮 MAJOR-1 补的漏 —— 它曾恒为 `M+C`、与 `owner_kind` 无关 |
//! | `GET /files/objects`（列表） | M / C / I / CNC / **SHELF** | 2026-10-10 放开 SHELF；⚠️ 2026-10-16 起**永不返回 `DELIVERY_TEMPLATE` 行**（显式传 `kind` 也是空） |
//! | `GET /files/objects/{id}/content` | M / C / I / CNC / **SHELF** | 2026-10-10 放开 SHELF；⚠️ 2026-10-16 起**加 kind 白名单**（拒 `DELIVERY_TEMPLATE` 等非文件类 kind），与 `/url` 共用 `policy::is_file_content_kind` |
//! | `GET /files/objects/{id}/url` | 权限点 `part.drawing.print`（默认矩阵 M / C / I / CNC） | **默认矩阵刻意不含 SHELF**，见下；⚠️ 2026-10-16 起**同样加 kind 白名单**（review 第 1 轮 MAJOR-2） |
//! | `GET /files/objects/{id}/preview` | M / C / I / CNC / **SHELF** | 2026-10-13 PR2 新增；⚠️ **含 SHELF 是对上一行的有意反转**，见下 |
//! | `POST /files/objects/{id}/delete` | 按 kind 派生（图纸类 + `ASSEMBLY_MASTER` M / C，G_CODE M / CNC，`DELIVERY_TEMPLATE` **仅 Manager**） | `ASSEMBLY_MASTER` 是 2026-10-13 review 第 1 轮 MINOR-6 补进分派的；`DELIVERY_TEMPLATE` 是 2026-10-16 新增（否则「写得进、删不掉」） |
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
//! ⚠️ `/url` 的守卫 2026-10-14 起是**权限点** `part.drawing.print`（默认矩阵
//! M / C / I / CNC），不是角色白名单；下面的论证对两者同样成立。
//!
//! `get_file_with_url` 回的是 **COS 预签直链**（`presigned_get_url`，
//! `url_expires_in_seconds = 3600`）：链接一旦到手就能**脱离本后端**直接 GET，
//! 也能整条外发给任何人 ⇒ 它等价于「把这 1 小时的 COS 读权限发出去」，而 RBAC
//! 在签发之后就不再参与。`GET /objects/{id}/content` 不同：字节流由本后端代理，
//! 每次访问都重新过一次 RBAC。工控机预览需要的是「看得到图纸字节」，content 已足够 ⇒ 放开
//! content 即达成目标，`/url` 无需、也不应一并放开。
//!
//! ### ⚠️⚠️ 为什么 `/preview` **反而**放开 SHELF（对上一节的**有意反转**，2026-10-13 PR2）
//!
//! 上一节论证「`/url` 不放开 SHELF」时，理由是「预签直链拿到就能脱离本后端直接 GET
//! 并外发」。新增的 `GET /objects/{id}/preview` 返回的是 COS `previews/` 前缀下的
//! **永久不过期 public-read 直链** —— **比 1 小时预签窗口更宽**（预签到期即失效，
//! 公开读永不过期）。按上一节的同款论证，它本该**更**不该放开。
//!
//! **本端点仍然放开 SHELF_ACCOUNT**，因为：
//!
//! 1. 报工台（前端路由 `/scan/*` 的守卫就是 `SHELF_ACCOUNT`）的工控机**必须**能看到
//!    图纸 —— 这是刚需，不是顺手放开的权限面；
//! 2. 预览图是**逐页 JPG**，不经后端代理、也不受源 PDF 的任何权限约束，
//!    但它承载的信息量与源 PDF 相同 ⇒ 放开它**没有扩大「谁能看图纸」这个集合**
//!    （`GET /objects` 列表与 `GET /objects/{id}/content` 早就放开了）；
//! 3. 补偿措施齐备：路径里**绝不带文件名**（文件名=图号、可枚举）、seed 是内容哈希、
//!    `previews/` 的 ACL 是**单独一条**、关掉它即断链而不影响源 PDF。
//!
//! ⚠️ **本段是留档，不是「设计如此」的辩解**：后人若要收紧，正确做法是**连同
//! `content` / 列表两条一起**重新评估（否则工控机立刻失去预览能力），
//! 而**不是**只把 `/preview` 关掉 —— 那只会让前端拿到 403 而不是更安全。
//!
//! **风险与补偿的完整表格、CDN 契约、以及「无法按零件关闭预览」这条反向操作风险**
//! 见 `preview/mod.rs` 的「安全取舍反转」一节 —— 本段只钉住「这是有意的、与 `/url`
//! 的差异是刻意的」这个事实。
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
//! | 6 | **`POST /files/objects/confirm` 的 size 上限由硬编码 300MB 改为读 `COS_MAX_FILE_SIZE`**（`.env.example` = 100MB ⇒ 按样例配置部署时**上限下降**）。副作用：`validate::check_file_size` 排在 step 2，**早于** step 3 的 `cos.head_object` 与 step 6 的 best-effort `delete_object` spawn ⇒ 100–300MB 区间的 STS 直传会在 confirm 被 21103 拒、**tmp 对象留在桶里**，只能等 COS lifecycle 回收 | **有意保留 + 已登记**（与 `POST /files/objects` 同读一份配置是本轮 MAJOR-1 的目的；review 第 2 轮 MINOR-C 补登记。⚠️ 降级/上限调整时注意这条副作用，且它与第 5 条**互不相干**：第 5 条是 part 域另一条写路径，既不读配置也不做 owner_kind 配对） |
//! | 7 | **`GET /files/objects` 的 `owner_kind` 过滤是空操作，且响应里的 `owner_kind` 从请求参数回显**（`service::render_out(&row, owner_kind)` 取形参、不取行上任何列 —— schema 压根没有 owner 类型列） | **有意保留 + 已登记**（读侧不构成隔离，见 `repo/sql.rs::list_with_filters` / `list_by_owner` 顶部注释；review 第 2 轮 MINOR-C 补登记。⚠️ **本条最易误导后人**：**写侧本轮新加了 `owner_kind` × `kind` 强制配对**（20104）而**读侧全无对应隔离** ⇒ 拿 `owner_kind=PART` 去查装配件 owner id 仍会返回 ASSEMBLY_MASTER 行，不要据响应里的 `owner_kind` 认定这些行真属该类型） |
//! | 8 | **`GET /files/objects/{id}/preview` 返回永久 public-read 直链并放开 `SHELF_ACCOUNT`**：比 `/url` 的 1 小时预签窗口更宽、且**无过期、无撤销**（除改 `previews/` 的 COS ACL），也无法按零件关闭 | **有意保留 + 已登记**（用户拍板选项 A；取舍全文与补偿表见 `preview/mod.rs` 的「安全取舍反转」一节。⚠️ 它与第 6 行「`owner_kind` 过滤是空操作」同源：读侧仍无 owner 级范围判定） |
//! | 9 | **`t_part_file` 表上没有 `owner_kind` 列 —— owner 语义只活在 API 层**：写侧由 `policy::owner_kind_allows_kind` 强制配对；读侧 `list_with_filters` / `list_by_owner` 的 `owner_kind` 形参**完全不进 SQL**；`render_out` 的 `owner_kind` 字段是**从请求参数回显**、不是从行上读出来的 | **有意保留 + 已登记**（与第 7 条同源。⚠️ 2026-10-16 加第三种 owner `CUSTOMER`（`DELIVERY_TEMPLATE`）后代价更明确：库里无法区分「零件 123 的图纸」与「客户 123 的打印模板」，二者共用 `part_id` 一个列空间。`print_template` 域因此**从不在通用列表端点上读这批行** —— 它只有自己那 5 条端点，每条都按 `(part_id, kind='DELIVERY_TEMPLATE')` 精确取，不依赖 `owner_kind` 过滤） |
//! | 10 | **`part_id` 一列现已承载**三种 owner：① `t_part.id`（DRAWING / 3D_MODEL / G_CODE / SETUP_SHEET / CAD_2D）、② `t_assembly.id`（ASSEMBLY_MASTER）、③ **`t_customer.id`（DELIVERY_TEMPLATE，2026-10-16）**。三者共用同一个雪花 ID 空间、**无映射表** | **有意保留 + 已登记**（用户拍板「不新建模板表」；新表要多一套软删 / 审计 / OCC 列，而模板字节本来就得走 COS + `t_part_file` 的 CAS key 与配额逻辑）。代价：`SELECT * FROM t_part_file WHERE part_id = $1` 这种**不带 kind** 的查询在语义上是跨三类 owner 的 —— 新增读路径**必须**带 `kind` 谓词，不得依赖 `part_id` 单列的隐含语义） |
//! | 11 | **`POST /files/objects/confirm` 曾能造 `DELIVERY_TEMPLATE` 行**（review 第 1 轮 MAJOR-1）：`owner_kind_allows_kind` 加 `CUSTOMER` 那轮只给 `upload_file_for_owner` 收了口，confirm 的角色闸门恒为 `M + C` 且 `assert_owner_exists` 的 CUSTOMER 臂不判 `parent_id IS NULL` ⇒ CLERK 能替换任意客户的模板 | **已闭合**（两条写入口共用 `policy::write_roles_for`；confirm 额外拒 `owner_kind='CUSTOMER'` ⇒ 20104。集成测试 `clerk_cannot_bypass_via_generic_confirm_endpoint` + `manager_cannot_write_template_via_confirm_endpoint` 从外部钉住） |
//! | 12 | **通用列表端点曾会列出模板行**（review 第 1 轮 MAJOR-3）：`PartFileListQuery.kind` 缺省时无 kind 谓词，端点角色集合含 `SHELF_ACCOUNT` ⇒ 工控机账号可列出全部客户的模板元数据（越权读链的「发现」环节）。⚠️ 该事实**本轮之前**也成立，只是那时表里还没有第三种 owner | **已闭合**（`service::list_files` 无条件传 `exclude_kind = Some("DELIVERY_TEMPLATE")`，rows 与 total 两条 SQL 同步；集成测试 `shelf_account_list_never_sees_delivery_template_rows`） |
//! | 13 | **`GET /files/objects/{id}/url` 曾无 kind 守卫**（review 第 1 轮 MAJOR-2）：守卫是权限点 `part.drawing.print`（默认矩阵 M/C/I/CNC）、回 1 小时 COS 预签直链 ⇒ CLERK / INSPECTOR / CNC 拿任意客户模板的字节，与 `content` 侧那道守卫形同虚设 | **已闭合**（`policy::is_file_content_kind` 上提，两条取字节端点共用；集成测试 `clerk_cannot_get_delivery_template_presigned_url` 带**对照组**—— 同一 token 打图纸行仍 200，证明判的是 kind） |
//! | 14 | **`files` 域的 kind 白名单与 `print_template` 域的 kind 校验是**两份**、错误码不同**：本域「白名单外的 kind」⇒ **21102**（HTTP 400，端点不服务这种类型）；模板域「不是 `DELIVERY_TEMPLATE`」⇒ **21101**（HTTP 404，这行在别的端点眼里不存在） | **有意保留 + 已登记**。两者面向的前端动作不同：21102 让前端提示「换个入口」，21101 让它刷新列表。别为了「统一」把任一侧改成另一侧的码 |
//! | 15 | **`POST /files/objects` 曾能把模板绑到**二级客户**上**（review 第 2 轮 MAJOR-A）：`assert_owner_exists` 的 CUSTOMER 臂只判存在、不判 `parent_id IS NULL`，而该端点的 `owner_kind` / `owner_id` 全由 client 给 ⇒ Manager 能造出一行绑在 L2 上的模板行。后果三条：① 破坏「只绑一级客户」不变式；② `uk_t_part_file_single` 是 `(part_id, kind)`、L1 行与 L2 行互不冲突 ⇒「一个 L1 至多一份活跃模板」被绕过，且给 L1 重传时 `soft_delete_active_by_part_kind(l1_id)` 清不掉 L2 那行；③ 该行 `resolve` 永远读不到 ⇒ **永不消费、永不清理**的孤儿配置行 | **已闭合**（L1 判据下沉进 `repo::customer_owner_exists`，谓词加 `AND parent_id IS NULL`，错误码沿用 21105。集成测试 `manager_cannot_bind_template_to_l2_customer_via_generic_endpoint` 从外部钉住，并断言**零副作用**：库里无行 + 桶里无对象 + 父 L1 未被碰） |
//! | 16 | **L1-only 的判据有**两道**、错误码不同**：本域兜底（`customer_owner_exists` → **21105** / HTTP 404）与 `print_template::service::l1_customer_name`（该域自己的前置强制点 → **20104** / HTTP 400） | **有意保留 + 已登记**。本域那道排在 `upload_file_for_owner` 内部、早于 COS PUT，管的是「有人绕过那个域的 handler 直调共用上传原语」；那域那道在 service 入口、零外部 IO，是 5 条端点的正常路径。**两道缺任一条都不成立** —— 别因为「files 域不该知道客户分两级」删掉本域那道（本域判的是 `parent_id IS NULL` 这个**列取值**，与它判 `t_part.assembly_id IS NULL` 同形） |
pub mod cnc;
pub mod dto;
pub mod handler;
pub mod model;
pub mod policy;
pub mod preview;
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
