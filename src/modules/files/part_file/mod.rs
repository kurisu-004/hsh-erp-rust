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
//! ## ⚠️ 上传链路统一（2026-10-15）：两条写入口，字节不过 rust 进程
//!
//! | 端点 | 作用 |
//! |---|---|
//! | `POST /files/objects/upload-intents` | 校验 + 按 `content_sha256` 派生正式 CAS key + 签发预签 PUT URL；CAS 命中活跃行则秒传（不签 URL） |
//! | `POST /files/objects/confirm` | head 校验（存在 / size）+ 软删旧活跃行 + INSERT +（可选）CNC 配对 |
//!
//! 本域**没有**任何收字节的端点：multipart 代理（`POST /files/objects`）与 python
//! STS 转发（`POST /api/v2/files/sts-tmp-keys`）均已下线。
//!
//! ## ⚠️ 新链路的信任取舍（务必读完再改这三条端点）
//!
//! **① 内容 `content_sha256` 服务端不校验。** S3 预签 PUT 走 **UNSIGNED-PAYLOAD**
//! （SigV4 的 `x-amz-content-sha256` 固定为 `UNSIGNED-PAYLOAD`），payload hash
//! **无法绑进签名** ⇒ 后端既拿不到字节、也无法重算 sha。`content_sha256` 因此是
//! 「客户端声明」，服务端只从两个方向约束它：
//! - **key 由它派生**：`upload-intents` 用它派生 CAS key 并签 URL；`confirm` 用
//!   (sha256, original_filename) **重新派生一遍**并与入参 `object_key` 逐字比对，
//!   不一致即 **21114** ⇒ 前端不能拿任意 `object_key` 登记别人的对象。
//! - **同 owner + kind + 同 sha 只能有一条活跃行**：`get_by_owner_kind_sha` 在
//!   upload-intents（秒传短路）与 confirm（并发兜底）各查一次。
//!
//! **② size 双检。** 入参侧 `validate::check_file_size`（`>0` 且
//! ≤ `COS_MAX_FILE_SIZE`）+ COS `head_object` 的实测 size 与入参逐字比对
//! （不符即 **21115**）；`head` 为 0 字节同样按「对象没落地」处理（**21114**）。
//!
//! **④ ⚠️ CAS 秒传的判据已从「服务端算的 sha」变成「客户端声明的 sha」。**
//! 上面 ① 的第一段说的是「服务端**既拿不到字节、也无法重算 sha**」——但**秒传**
//! 这一条路上，服务端连一次 `head` 都不做：`grant_upload_intents` 拿
//! `get_by_owner_kind_sha(owner_id, kind, &it.content_sha256)` 一命中就回
//! `already_exists: true` + `existing_file`，前端**跳过 PUT 与 confirm**。也就是说
//! 判据本身就是那条**不可验证**的 `content_sha256` 列。
//!
//! 威胁模型：攻击者只要让**某 owner 名下某 kind 的活跃行**的 `content_sha256`
//! 列写着某个值 X（对象里其实不是 X 也行 —— 他直传任意字节、confirm 时声明 X），
//! 此后任何**真上传内容 X** 的用户调 upload-intents 都会拿到秒传响应、
//! 跳过 PUT，最终交付的是攻击者的字节。要成立需要攻击者**预先知道**受害者将
//! 上传内容的 sha（未公开、逐文件不同）⇒ 现实可行窗口窄，但不是零。
//!
//! 可选缓解（本轮**未做**，取舍理由见下）：秒传前对 `existing_file` 的
//! `object_key` 做一次 `head_object`，用实测 size 与入库的 `file_size` 交叉校验。
//! **未做的理由**：`head` 要发网络请求（与「秒传一次 PUT 都不发」的收益直接冲突），
//! 且 size 只能交叉校验、**验不出内容**（一个字节数相同的恶意对象照样通过）。
//! ⇒ 它不是「修好了」，只是把「全靠客户端声明」挪近一格。别当成等价替换。
//!
//! ⚠️ **CAS key 不含 owner**（`build_cas_key(prefix, sha, filename)` 两段模板）：
//! 同一份内容 + 同一文件名在**任何 owner** 名下派生出的 key 相同，秒传/直传都
//! 命中同一个对象。这是内容寻址的正常性质（省一份存储），但也意味着「拿到一张
//! 能写某 key 的签名 URL」与「能绑到哪个 owner」是两件事 —— owner 归属由
//! `t_part_file` 行决定，不由 key 决定。
//!
//! **③ CUSTOMER 的角色口径。** `owner_kind='CUSTOMER'`（送货单打印模板）**两条**
//! 写入口都只放 **Manager**（`policy::write_roles_for`，共用同一张表）；
//! 且 owner 存在性走 `repo::customer_owner_exists`（含 `parent_id IS NULL`）⇒
//! 只绑**一级**客户，绑二级客户得 **21105**。
//!
//! ### ⚠️ 前端配套改动清单（2026-10-15 两条写入口的 DTO 逐字段变更）
//!
//! 两条写入口的入参 / 出参与前端此前的形态**逐字段不同**（前端仓
//! `src/types/part_file.ts` / `src/api/files/objects.ts` 的旧 zod schema 直接
//! 反序列化会失败）。CLAUDE.md 要求「修改 DTO 字段 / 错误码必须同步更新对应域
//! 的文档与注释」，本表就是那份登记：
//!
//! | 端点 | 方向 | 旧（前端现有） | 新（后端当前） |
//! |---|---|---|---|
//! | `upload-intents` | 入参 | `{owner_part_id?, files: [{kind, filename, file_size, content_sha256, content_type}]}` | `{owner_kind, owner_id, items: [{client_ref, kind, original_filename, content_type, file_size, content_sha256}]}` |
//! | `upload-intents` | 出参 | `{tmp_key}` | `{already_exists, object_key?, upload_url?, content_type?, expires_in, existing_file?}` |
//! | `confirm` | 入参 | `tmp_key` | **`object_key`**（硬切，无兼容层；缺它 ⇒ 422 纯文本） |
//! | `confirm` | 出参 | — | `R<PartFileOut>`（18 字段；无 `customer_name`） |
//!
//! ⚠️ **秒传项必须容忍缺字段**：`already_exists: true` 时出参**没有**
//! `object_key` / `upload_url` / `content_type`（`skip_serializing_if` 省略），
//! 前端 schema 若把前三个设成必填 ⇒ 秒传响应直接校验失败。
//!
//! ⚠️⚠️ **`file_size` / `owner_id` / `pair_with_file_id` 入参必须是 JSON 字符串**：
//! 后端走 `shared::types::deserialize_i64`，实现是
//! `String::deserialize(d)?.parse::<i64>()` ⇒ **只收字符串**；传裸 number 会在
//! axum `Json` 提取器层炸 ⇒ **HTTP 422 纯文本**，不是 `R<T>` 信封，前端
//! `envelopeResponseInterceptor` 拿不到 `{code,message}`。
//!
//! ⚠️ **`POST /parts/batch` 的 `drawing_file` / `model3d_file` 必须停发**：该 DTO
//! 没有开 `deny_unknown_fields` ⇒ 继续发会 **200 + 零副作用**（文件绑定被静默
//! 丢弃，服务端无任何信号）。这与 confirm 的 `tmp_key`→`object_key` 硬切
//! （缺字段 ⇒ 422）方向**相反**，是两条不同的兼容策略，别混记。
//!
//! 契约全表见 `docs/api/print_template.md` §1.0（送货单打印模板侧的写入链路是
//! 本域两条写入口的一个使用者，`owner_kind='CUSTOMER'` + `kind='DELIVERY_TEMPLATE'`）。
//!
//! ### COS key 前缀：代码侧统一 `obj/`，**容器部署下仍未统一**
//!
//! 写入路径的 CAS key 前缀读 `cfg.cos.upload_prefix`（代码缺省 `obj/`），由
//! `util::cos_key::build_cas_key` 派生；读端点的 legacy fallback 用**同一份**配置。
//!
//! ⚠️ **容器部署下前缀仍可能落 `drawings/`**：`docker-compose{,-local,-staging}.yml`
//! 的 `${COS_UPLOAD_PREFIX:-drawings/}` 共 6 处仍在**根仓**（backend-rust 仓外，
//! 本仓结构上无法触及），代码缺省值只在**不经 compose** 的 `cargo run` 场景生效。
//! 本仓不要在文档里声称「前缀已统一」。
//!
//! ⚠️ **不需要数据迁移，更不要跑 `src/bin/cos_key_migrate.rs`**：
//! `t_part_file.object_key` 存的是**完整 key**，读端点
//! `PartFileService::resolve_effective_key` 先 `head_object(db_object_key)`
//! 优先命中 ⇒ 历史对象原地不动，只是新上传改落配置里的前缀。
//!
//! ### ⚠️ 两条写入口对「同 owner + kind 换内容」的行为
//!
//! 两条入口都是「先软删旧活跃行 + 再 INSERT」⇒ **换内容 = 换行**（旧行
//! `deleted_at` 非空）。`uk_t_part_file_single` 是 `(part_id, kind)` 的部分唯一
//! 约束，所以「一个 owner + kind 至多一条活跃行」在所有 kind 上都成立。
//!
//! 两条入口对 CAS 命中的处理**不同**，这是有意的：
//! - `upload-intents` 命中 ⇒ **秒传**（`already_exists: true` + `existing_file`），
//!   一次 PUT 都不发；
//! - `confirm` 命中 ⇒ 正常流程不该走到（前端拿到秒传就跳过 confirm 了），走到
//!   说明是**并发双 confirm**，此时复用既有行（再 INSERT 会撞唯一索引），
//!   若同时带 `pair_with_file_id` 则仍把配对做掉。
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
//! | `PART` | `DRAWING` / `3D_MODEL` / `CAD_2D` / `G_CODE` / `SETUP_SHEET`（`G_CODE` / `SETUP_SHEET` 是 CNC 配对的两种文件，由 confirm 的 `pair_with_file_id` 承接；`CAD_2D` = DWG / DXF 源文件） |
//! | `ASSEMBLY` | `ASSEMBLY_MASTER` |
//! | `CUSTOMER` | `DELIVERY_TEMPLATE`（送货单打印模板） |
//!
//! fail-safe 方向：不在表内一律拒。将来给 `allowed_exts` 加新 kind 时**不会**自动
//! 放行 —— 必须先在这里显式声明它挂哪种 owner。
//!
//! ## 2026-10-16 新增第三种 owner `CUSTOMER`（送货单打印模板）
//!
//! 需求：「增加模板上传的功能，模板支持 xlsx 格式，当前用于打印送货单……且可以
//! 绑定一级客户。送货单打印时自动选择已绑定的模板」。
//!
//! **模板字节不新建表**，复用本表的 `t_part_file`：`kind='DELIVERY_TEMPLATE'`，
//! `part_id` 列装的是**一级客户的 `t_customer.id`**，「该 L1 名下存在一行未软删的
//! `DELIVERY_TEMPLATE`」本身就是绑定关系。承载域是新建的 `print_template` 域
//! （`src/modules/print_template/`，URL `/api/v2/print-templates`，**4 端点** ——
//! 写入不在它自己那儿，走本域两条写入口），
//! 完整契约见 `docs/api/print_template.md`。本域只出**九处**能力：
//! ① `kind` 白名单 / 扩展名 / file_type / content_type 四张表加了 `DELIVERY_TEMPLATE`；
//! ② `owner_kind='CUSTOMER'` 的闸门（`repo::customer_owner_exists`，⚠️ **含
//!    `parent_id IS NULL`** —— 只绑一级客户，2026-10-16 review 第 2 轮 MAJOR-A 补）；
//! ③ `owner_kind × kind` 配对表加 `CUSTOMER => DELIVERY_TEMPLATE`；
//! ④ 两条**写**入口（upload-intents / confirm）的角色分派共用
//!    `policy::write_roles_for`（形参是 `WriteEntry` 入口枚举，**不是**可传错的
//!    常量集合；confirm 另经 `confirm_roles_for` 按 kind 分派 CNC 配对两种文件）；
//! ⑤ 两条**取字节**端点（`content` / `url`）共用 `policy::is_file_content_kind`；
//! ⑥ 通用列表端点 `exclude_kind` 掉 `DELIVERY_TEMPLATE`；
//! ⑦ 软删分派表加 `DELIVERY_TEMPLATE => [Manager]`；
//! ⑧ 取字节的**带闸门** service 入口 `get_file_content_of_checked_row`（⚠️ 不判 kind，
//!    判据归调用方；review 第 2 轮 MINOR-A 把那个「有（当前不拦任何人的）角色闸门、
//!    无 kind 闸门」的 `get_file_content` **整条删除**了，见下）。
//!    ⚠️ **本域取字节的服务层入口其实有两条**（2026-10-16 review 第 3 轮 NIT-5 订正：
//!    原文写「唯一」不成立）：`get_file_content_of_checked_row`（带角色闸门，即 ⑨）与
//!    `get_file_content_for_row`（**零判定**：不判权限、不判 kind，`print_template`
//!    域端点 4 在 Rust 侧自己判完 kind 再调它）。`get_file_content_for_row` 是最底层的
//!    取字节原语、不属于本域为 `DELIVERY_TEMPLATE` 新增的能力，故不进这份清单；
//!    但别把「唯一」当成本域只有一条取字节路径 —— 它有两条。
//! 逐条理由见下面的收紧表与已知偏差登记表。
//!
//! ### ⚠️ **六处**刻意收紧（新增第三种 owner 带来的；2026-10-16 review 第 1 轮补齐、
//! 第 2 轮再加一处 L1 判据）
//!
//! | 收紧点 | 位置 | 为什么 |
//! |---|---|---|
//! | 写入口的角色面按 `owner_kind` 分派 | `policy::write_roles_for` + `service::grant_upload_intents` / `service::bind_uploaded_file` | `CUSTOMER` 只放 **Manager**。⚠️ `owner_kind` / `kind` 都由 client 在 multipart `data` 里给，只在 `print_template` 域的 handler 上守 MANAGER 的话，CLERK 直接打 `POST /files/objects` 就能绕过模板管理页乱换别人的打印模板。`PART` / `ASSEMBLY` 的集合**逐字不变** |
//! | **两条写入口都收 CUSTOMER 的「只绑一级客户」** | `repo::customer_owner_exists` 的 `AND parent_id IS NULL` | 收窄角色**不等于**收窄 owner：Manager 本就能写 CUSTOMER，若不判 L1，他能把模板绑到二级客户上（那行永不被 `resolve` 读到 ⇒ 孤儿配置行）。判据下沉进两道闸门共用的 repo 原语 ⇒ **绑二级客户恒得 21105** |
//! | **confirm 端点（第二条写入口）同样按 `owner_kind` 分派** | `policy::confirm_roles_for` + `service::bind_uploaded_file` | 两条写入口**共用** `write_roles_for` 派出的 CUSTOMER 收窄（仅 Manager），防「下次新增 owner_kind 又只改一条入口」 |
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
//! ## 角色矩阵（7 条存活端点）
//!
//! 报工台（前端路由 `/scan/*`）的路由守卫就是 `SHELF_ACCOUNT` —— 车间工控机用的
//! 就是这个角色账号；它的图纸预览打的是本子模块的**列表** `GET /api/v2/files/objects`
//! 与**内容** `GET /api/v2/files/objects/{id}/content`。这两条原本只放 4 角色
//! （Manager / Clerk / Inspector / CncProgrammer）⇒ 工控机点预览必然 403（40300）。
//!
//! | 端点 | 角色集合 | 备注 |
//! |---|---|---|
//! | `POST /files/objects/upload-intents` | `PART`/`ASSEMBLY`：M / C / CNC；**`CUSTOMER`：仅 M** | 按 `owner_kind` 分派（`policy::write_roles_for(…, WriteEntry::Upload)`），两条写入口共用同一张表 |
//! | `POST /files/objects/confirm` | M / C；`G_CODE` / `SETUP_SHEET`：M / CNC；**`CUSTOMER`：仅 M** | `policy::confirm_roles_for(owner_kind, kind)`：CNC 配对两种 kind 另给 CNC 岗位（CNC 程序历来由 CNC 岗位写：M+CNC、不含 Clerk），其余走 `WriteEntry::Confirm`（`CAD_2D` 走这一支） |
//! | `GET /files/objects`（列表） | M / C / I / CNC / **SHELF** | 2026-10-10 放开 SHELF；⚠️ 2026-10-16 起**永不返回 `DELIVERY_TEMPLATE` 行**（显式传 `kind` 也是空） |
//! | `GET /files/objects/{id}/content` | M / C / I / CNC / **SHELF** | 2026-10-10 放开 SHELF；⚠️ 2026-10-16 起**加 kind 白名单**（拒 `DELIVERY_TEMPLATE` 等非文件类 kind），与 `/url` 共用 `policy::is_file_content_kind` |
//! | `GET /files/objects/{id}/url` | 权限点 `part.drawing.print`（默认矩阵 M / C / I / CNC） | **默认矩阵刻意不含 SHELF**，见下；⚠️ 2026-10-16 起**同样加 kind 白名单**（review 第 1 轮 MAJOR-2） |
//! | `GET /files/objects/{id}/preview` | M / C / I / CNC / **SHELF** | 2026-10-13 PR2 新增；⚠️ **含 SHELF 是对上一行的有意反转**，见下 |
//! | `POST /files/objects/{id}/delete` | 按 kind 派生（图纸类 + `ASSEMBLY_MASTER` M / C，G_CODE M / CNC，`DELIVERY_TEMPLATE` **仅 Manager**） | `ASSEMBLY_MASTER` 是 2026-10-13 review 第 1 轮 MINOR-6 补进分派的；`DELIVERY_TEMPLATE` 是 2026-10-16 新增（否则「写得进、删不掉」） |
//!
//! ### ⚠️ 写侧对各 kind 的接受面（当前事实）
//!
//! `allowed_exts` 是**一张**白名单，`upload-intents` / `confirm` / 软删分派三条路径
//! 共用它 ⇒ 某 kind 一旦进了白名单（**且** `owner_kind_allows_kind` 放了它），
//! 写侧就能造该 kind 的行。
//!
//! - `DRAWING` / `3D_MODEL` / `ASSEMBLY_MASTER` / `CAD_2D`：M+C+CNC 可写
//!   （与图纸 / 3D 模型 / 装配件整图 / CAD 源文件的既有权限面一致）。
//! - `G_CODE` / `SETUP_SHEET`：confirm 侧 M+CNC（`confirm_roles_for`）；
//!   `upload-intents` 侧 M+C+CNC。
//! - `DELIVERY_TEMPLATE`：**仅 Manager**（两条写入口都是）。
//!
//! `soft_delete_file` 的 kind 分派逐条对应（`G_CODE` M+CNC、`DELIVERY_TEMPLATE`
//! 仅 Manager），故不存在「写得进、删不掉」的 kind。
//! ⚠️ **两张写侧表必须同步改**（`allowed_exts` 与 `owner_kind_allows_kind`）：
//! 任缺一张，那个 kind 在两条入口都传不上来 —— `CAD_2D` 就这么丢过一次
//! （DB / 读侧 / 删侧全都支持它，只有写侧两张表漏了）。lib 单测
//! `policy::tests::every_allowed_ext_kind_is_mounted_on_some_owner` 钉住这条。
//!
//! ### 2026-10-13：零件维度三条嵌套列表端点下线 ⇒ 不再有「两层闸门取交集」
//!
//! 原表里另有三条 `GET /part-files/parts/{id}/{files,cnc-programs,setup-sheets}`
//! （handler 级 4 角色）比 service 的 `list_files`（5 角色）严一档，取交集 ⇒ 4 角色。
//! **这三条已随零调用方端点清理整体下线**（见 `handler.rs` 移除记录），故本域现在
//! 不再有「handler 级闸门与 service 白名单分叉」这件事：每个端点只有一层闸门，且
//! 那层闸门就在 service 里。角色矩阵回到「一个端点一行、集合唯一」的状态。
//!
//! ### 为什么写路径不放开 SHELF
//!
//! SHELF_ACCOUNT 只放开**只读**端点（列表 + content + preview）：upload-intents /
//! confirm / 软删三条写路径一律拒（回归用例
//! `tests/files/part_file.rs::rbac_shelf_account_cannot_upload_or_delete`）。
//!
//! ### 为什么 `/url` 不跟着放开
//!
//! ⚠️ `/url` 的守卫 2026-10-14 起是**权限点** `part.drawing.print`（默认矩阵
//! M / C / I / CNC），不是角色白名单；下面的论证对两者同样成立。
//!
//! `get_file_with_url` 回的是 **COS 预签直链**（`presigned_get_url`，有效期读
//! `COS_PRESIGN_EXPIRE`、缺省 3600s）：链接一旦到手就能**脱离本后端**直接 GET，
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
//! | 3 | **`ASSEMBLY_MASTER` 曾「能造不能删」**：软删 kind 分派漏了该 kind | **已闭合**（软删分派现已逐条对应 `allowed_exts` 的每个 kind） |
//! | 4 | **写入口对 `PART` / `ASSEMBLY` 的角色面含 CNC**（M+C+CNC） | **有意保留 + 已登记**（见上文「写侧对各 kind 的接受面」：CNC 历来能写图纸 / 3D 模型 / CNC 程序） |
//! | 5 | —— （原登记的「`POST /parts/batch` bindings 分支写死 300MB 上限」随该分支整体删除） | **已闭合**（2026-10-15：bindings 分支与 `prepare_binding_head_copy` 一并删除，`POST /parts/batch` 回归纯建单） |
//! | 6 | **`confirm` 的 size 上限读 `COS_MAX_FILE_SIZE`**（`.env.example` = 100MB ⇒ 按样例配置部署时**上限**按该配置走）。⚠️ 副作用：入参 size 校验排在 `head_object` **之前** ⇒ 前端 PUT 了一个 100–300MB 的对象而配置只放行 100MB 时，confirm 报 21103 且该对象**留在桶里**（没有后置清理可做 —— 它就在正式 key 上），只能等 COS lifecycle 回收 | **有意保留 + 已登记**（先拒后查，少一次外部 IO） |
//! | 7 | **`GET /files/objects` 的 `owner_kind` 过滤是空操作，且响应里的 `owner_kind` 从请求参数回显**（`service::render_out(&row, owner_kind)` 取形参、不取行上任何列 —— schema 压根没有 owner 类型列） | **有意保留 + 已登记**（读侧不构成隔离，见 `repo/sql.rs::list_with_filters` / `list_by_owner` 顶部注释；review 第 2 轮 MINOR-C 补登记。⚠️ **本条最易误导后人**：**写侧本轮新加了 `owner_kind` × `kind` 强制配对**（20104）而**读侧全无对应隔离** ⇒ 拿 `owner_kind=PART` 去查装配件 owner id 仍会返回 ASSEMBLY_MASTER 行，不要据响应里的 `owner_kind` 认定这些行真属该类型） |
//! | 8 | **`GET /files/objects/{id}/preview` 返回永久 public-read 直链并放开 `SHELF_ACCOUNT`**：比 `/url` 的 1 小时预签窗口更宽、且**无过期、无撤销**（除改 `previews/` 的 COS ACL），也无法按零件关闭 | **有意保留 + 已登记**（用户拍板选项 A；取舍全文与补偿表见 `preview/mod.rs` 的「安全取舍反转」一节。⚠️ 它与第 6 行「`owner_kind` 过滤是空操作」同源：读侧仍无 owner 级范围判定） |
//! | 9 | **`t_part_file` 表上没有 `owner_kind` 列 —— owner 语义只活在 API 层**：写侧由 `policy::owner_kind_allows_kind` 强制配对；读侧 `list_with_filters` / `list_by_owner` 的 `owner_kind` 形参**完全不进 SQL**；`render_out` 的 `owner_kind` 字段是**从请求参数回显**、不是从行上读出来的 | **有意保留 + 已登记**（与第 7 条同源。⚠️ 2026-10-16 加第三种 owner `CUSTOMER`（`DELIVERY_TEMPLATE`）后代价更明确：库里无法区分「零件 123 的图纸」与「客户 123 的打印模板」，二者共用 `part_id` 一个列空间。`print_template` 域因此**从不在通用列表端点上读这批行** —— 它只有自己那 **4** 条端点（列表 / resolve / content / delete；写入不在本域），每条都按 `(part_id, kind='DELIVERY_TEMPLATE')` 精确取，不依赖 `owner_kind` 过滤） |
//! | 10 | **`part_id` 一列现已承载**三种 owner：① `t_part.id`（DRAWING / 3D_MODEL / G_CODE / SETUP_SHEET / CAD_2D）、② `t_assembly.id`（ASSEMBLY_MASTER）、③ **`t_customer.id`（DELIVERY_TEMPLATE，2026-10-16）**。三者共用同一个雪花 ID 空间、**无映射表** | **有意保留 + 已登记**（用户拍板「不新建模板表」；新表要多一套软删 / 审计 / OCC 列，而模板字节本来就得走 COS + `t_part_file` 的 CAS key 与配额逻辑）。代价：`SELECT * FROM t_part_file WHERE part_id = $1` 这种**不带 kind** 的查询在语义上是跨三类 owner 的 —— 新增读路径**必须**带 `kind` 谓词，不得依赖 `part_id` 单列的隐含语义） |
//! | 11 | **`POST /files/objects/confirm` 曾能造 `DELIVERY_TEMPLATE` 行**：两条写入口的角色闸门一度不共用，CLERK 能经 confirm 造出并替换任意客户的模板行 | **已闭合**（两条写入口共用 `policy::write_roles_for` / `confirm_roles_for`，`CUSTOMER` 恒收窄为仅 Manager；集成测试 `clerk_cannot_bypass_via_generic_confirm_endpoint` 从外部钉住） |
//! | 12 | **通用列表端点曾会列出模板行**（review 第 1 轮 MAJOR-3）：`PartFileListQuery.kind` 缺省时无 kind 谓词，端点角色集合含 `SHELF_ACCOUNT` ⇒ 工控机账号可列出全部客户的模板元数据（越权读链的「发现」环节）。⚠️ 该事实**本轮之前**也成立，只是那时表里还没有第三种 owner | **已闭合**（`service::list_files` 无条件传 `exclude_kind = Some("DELIVERY_TEMPLATE")`，rows 与 total 两条 SQL 同步；集成测试 `shelf_account_list_never_sees_delivery_template_rows`） |
//! | 13 | **`GET /files/objects/{id}/url` 曾无 kind 守卫**（review 第 1 轮 MAJOR-2）：守卫是权限点 `part.drawing.print`（默认矩阵 M/C/I/CNC）、回 1 小时 COS 预签直链 ⇒ CLERK / INSPECTOR / CNC 拿任意客户模板的字节，与 `content` 侧那道守卫形同虚设 | **已闭合**（`policy::is_file_content_kind` 上提，两条取字节端点共用；集成测试 `clerk_cannot_get_delivery_template_presigned_url` 带**对照组**—— 同一 token 打图纸行仍 200，证明判的是 kind） |
//! | 14 | **`files` 域的 kind 白名单与 `print_template` 域的 kind 校验是**两份**、错误码不同**：本域「白名单外的 kind」⇒ **21102**（HTTP 400，端点不服务这种类型）；模板域「不是 `DELIVERY_TEMPLATE`」⇒ **21101**（HTTP 404，这行在别的端点眼里不存在） | **有意保留 + 已登记**。两者面向的前端动作不同：21102 让前端提示「换个入口」，21101 让它刷新列表。别为了「统一」把任一侧改成另一侧的码 |
//! | 15 | **模板曾能被绑到**二级客户**上**：`owner_kind` / `owner_id` 全由 client 给，而 owner 存在性只判「存在且未软删」 ⇒ Manager 能造出一行绑在 L2 上的模板行。后果三条：① 破坏「只绑一级客户」不变式；② `uk_t_part_file_single` 是 `(part_id, kind)`、L1 行与 L2 行互不冲突 ⇒「一个 L1 至多一份活跃模板」被绕过；③ 该行 `resolve` 永远读不到 ⇒ 孤儿配置行 | **已闭合**（L1 判据下沉进 `repo::customer_owner_exists`，谓词含 `AND parent_id IS NULL`，错误码沿用 21105。集成测试从外部钉住，并断言**零副作用**：库里无行 + 桶里无对象 + 父 L1 未被碰） |
//! | 16 | **「xlsx 内容是不是真 xlsx」不再由后端校验**：统一链路后 confirm 只拿到 `head`（size / etag），拿不到字节 ⇒ ZIP magic 判据在**后端物理上无法回答**。该职责移交前端（选文件时用 hucre 逐格解析） | **有意弱化 + 已登记**。代价：绕过前端直打两条通用端点时，一份非 xlsx 的 `.xlsx` 能存进库，报错点后移到前端渲染时。取舍全文见 `print_template/dto.rs` 末尾与 `docs/api/print_template.md`。⚠️ 别在另一个时机「补全」回来 —— 那会造出第二个失败面 |
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
