# print_template 域 API（送货单打印模板：存 + 绑 + 取字节）

> 本文件是 `print_template` 域的**唯一**契约来源。任何字段 / 端点变更必须同步本文件。
> 代码位置：`src/modules/print_template/`（`mod.rs` · `handler.rs` · `service.rs` · `dto.rs` · `vo.rs`）。
> 权威路由清单：`handler::ROUTES`（**4 条** —— 2026-10-15 起本域无上传端点），lib 单测
> `handler::tests::routes_declared_in_router` 逐条比对 `router()` 源码
> （解析器复用 `files::route_guard::routes_in_router_source`，**不**另抄一份）。

## 0. 需求与范围

**用户原话**：「增加模板上传的功能，模板支持 xlsx 格式，当前用于打印送货单。前端的模板管理页面增加上传模板和查看已有模板的功能。且可以绑定一级客户。送货单打印时自动选择已绑定的模板」。

### 0.1 本域**做**什么（4 条端点，全在 §1）

1. 管理员在「模板管理」页给某个**一级客户**替换 / 删除一份 xlsx 打印模板
   （**写入**走 `files` 域的统一链路，见 §1.0）；
2. 打印对话框按「送货单所属的一级客户」**自动解析出**该用哪份模板（端点 2）；
3. 前端据此**取模板字节**（端点 3），再用它渲染打印件。

### 0.2 本域**不做**什么（明确划界，防后人误接）

| 不做 | 理由 |
|---|---|
| **不恢复任何「渲染 / 生成打印件」端点** | 打印渲染一直在**前端本地**（hucre）。`POST /com/delivery/note/{id}/print` 与 `print-labels` 于 2026-10-08 整链路下线（见 `docs/api/delivery_note.md` §6.1），本轮**逐字不恢复**。本域只管「存 + 绑 + 取字节」三件事 |
| **不校验 xlsx 内容**（含 ZIP magic） | 统一链路后后端**拿不到字节**（confirm 只有 `head`），物理上答不了；结构校验由前端 `assertTemplateMatches` 承担（§3.2、§8.3 第 3 条） |
| **不绑二级客户** | 用户拍板「只绑一级客户」（§4.1） |
| **不做「传了份新模板」的通知 / 审批流** | 需求里没有；写入链路是**立即替换**语义 |

### 0.3 绑定关系 = 存在性（本域最核心的决策）

**不新建模板表**，复用 `files` 域自有的 `t_part_file`：`kind='DELIVERY_TEMPLATE'`、
`part_id` 列装的是**一级客户的 `t_customer.id`**。「该 L1 名下存在一行未软删的
`DELIVERY_TEMPLATE`」**本身就是绑定关系**。取舍全文见 §8.3 第 1–2 条。

## 1. 端点表（**4 个**）

| # | 方法 | 路径 | 权限 | 入参 | 响应 |
|---|---|---|---|---|---|
| 1 | GET | `/api/v2/print-templates` | `require_role(Manager)` | `customer_id?`、`limit?`（缺省 50，clamp `[1,200]`）、`offset?`（缺省 0，`max(0)`） | `PrintTemplateListOut` |
| 2 | GET | `/api/v2/print-templates/resolve` | 权限点 `delivery_note.print` | `customer_id`（**必填**，JSON string 形态雪花 id） | `PrintTemplateResolveOut`；未绑定 ⇒ `21109` / HTTP **422** |
| 3 | GET | `/api/v2/print-templates/{template_id}/content` | 权限点 `delivery_note.print` | 无 | **裸字节**（非 `R<T>`）+ xlsx MIME 头 |
| 4 | POST | `/api/v2/print-templates/{template_id}/delete` | `require_role(Manager)` | JSON body **必含 `version`**（OCC） | `R<()>` |

### 1.0 模板**写入**链路（2026-10-15：本域不再有上传端点）

模板行的写入**全部**经 `files` 域的统一收文件链路，本域不持任何收字节的端点：

```text
① POST /api/v2/files/objects/upload-intents
   body: { owner_kind: "CUSTOMER", owner_id: "<L1 雪花 id>",
           items: [{ client_ref, kind: "DELIVERY_TEMPLATE",
                     original_filename, content_type, file_size, content_sha256 }] }
   → 每项回 { already_exists, object_key, upload_url, content_type, expires_in, existing_file? }
② 前端 XHR PUT 字节到 upload_url（**不经 rust 进程**），头带 content_type
③ POST /api/v2/files/objects/confirm
   body: { owner_kind: "CUSTOMER", owner_id, kind: "DELIVERY_TEMPLATE",
           object_key, content_sha256, original_filename, file_size, content_type, ext }
   → R<PartFileOut>
```

- 两条写入口的角色闸门：`owner_kind='CUSTOMER'` ⇒ **仅 Manager**
  （`policy::write_roles_for`；CLERK 打任一条都是 40300）。
- `owner_id` 必须是**一级客户** ⇒ 否则 **21105 / HTTP 404**（§4.1）。
- 「替换」语义（一个 L1 至多一份活跃模板）由 confirm 事务内的
  **软删旧活跃行 + INSERT** 承接（§4.2）。
- ⚠️ **confirm 的出参是 `R<PartFileOut>`，没有 `customer_name`** —— 那是本域给
  `t_customer` 补的列。写完**刷新列表**（端点 1）才能拿到名字。
- ⚠️ 上传-intents 命中 CAS 时返回 `already_exists: true` + `existing_file`，前端
  **跳过 PUT 与 confirm**（秒传）。

### 1.1 权限分档（为什么读端点不是 MANAGER-only）

| 端点 | 守卫 | 默认放行集合 |
|---|---|---|
| 1 `GET /`（列表）· 4 `POST /{id}/delete`（软删） | `require_role(Manager)` | `MANAGER`（+ `ADMIN` 元角色短路） |
| 2 `GET /resolve` · 3 `GET /{id}/content` | 权限点 `delivery_note.print` | `MANAGER` / `CLERK` / `INSPECTOR` |

**读端点绝不能 MANAGER-only**：打送货单是 CLERK / INSPECTOR 的日常动作
（备货、点检、装车），管理面与作业面不是同一批人。权限点
`delivery_note.print` 的默认矩阵恰是那三个角色（`seeds/rbac.sql`），
`SHELF_ACCOUNT` **不在**矩阵内（工控机账号只该看图纸）。

⚠️ 2026-10-16 该权限点的描述从「前端-only，后端无打印端点，仅登记」改成
「打印对话框取模板 + 取 xlsx 字节的守卫点」。**授权矩阵逐字未变**，只改描述。

### 1.2 角色守卫下沉在 handler 第一行 + service 内复核

仓库惯例（`statistics` / `mail_digest` / `print_template` 同款）。⚠️ 2026-10-15 起
本域没有写端点，「读 multipart 之前」那条只对**读**端点不再适用（读端点是纯
查询，没有分配成本）。写入侧的角色闸门在 `files::part_file` 的两条通用写入口里
（`upload-intents` / `confirm`），见 §4.4。

### 1.3 事务边界

| 端点 | 事务 | 理由 |
|---|---|---|
| 1 `GET /`（列表）· 2 `GET /resolve` | **不开**，`pool.acquire()` | 纯读 |
| 3 `GET /{id}/content` | **不开**，`pool.acquire()` | COS `get_object` 是外部 IO，包进 PG 事务会让连接被外部依赖长时间占住 |
| 4 `POST /{id}/delete` | `pool.begin()` → service → `tx.commit()` → **commit 之后**才 spawn `cos.delete_object` | 「commit 在前、spawn 在后」（`files` 域的既有纪律） |

⚠️ **本域没有「先软删旧行 + 再写新行」的那个写端点**：替换语义整个落在
`POST /files/objects/confirm` 的**同一个事务**里（§1.0 三步的第 ③ 步），
那里 `pool.begin()` → `soft_delete_active_by_part_kind` → `INSERT` → `commit`
是一条不可分割的链，否则第二步失败会留下「该客户没有模板」的空洞（§4.2）。

### 1.4 matchit：无冲突（实测钉死）

`/resolve` 是 1 段静态段；本 router **没有** 1 段的 `Path<i64>` catch-all
（无 `/{template_id}` 端点）⇒ 静态段不会被吞。`/{template_id}/content` 与
`/{template_id}/delete` 是 2 段「第 1 段捕获、第 2 段静态」，两者位置平行、
彼此不争。集成测试 `resolve_and_static_paths_are_not_swallowed` 实打三发钉死
（**别只靠读源码判断** —— CLAUDE.md「路由声明规约」第 9 条的老踩坑就是「同段数
静态段被 catch-all 吞成 400」）。

### 1.5 旧路径：干净 404 空 body

| 路径 | 处置 |
|---|---|
| `/api/v2/com/delivery/template/*` | **从未存在**（送货单域 17 端点里没有模板端点）⇒ 干净 404 |
| `/api/v2/print-template/*`（少个 `s`） | 前端拼错 URL 的形态 ⇒ 干净 404，**刻意不设 alias** |

两者都是 **404 + 空 body**（不是 `R<T>` 信封，也不是 400 纯文本）：404 由 axum
fallback 产出，`route_layer` 只作用于已匹配路由 ⇒ 404 连鉴权层都不经过。

## 2. `PrintTemplateOut` 逐字段

三个读端点（1 / 2 / 3）复用同一形状。⚠️ **写入侧的响应不是这个 VO** —— 它是
`R<PartFileOut>`（见 §1.0 末）。

| 字段 | 类型 | 来源 / 口径 |
|---|---|---|
| `id` | **string** | `t_part_file.id`（雪花 ID 字符串化，防 JS 精度截断；`shared::types::serialize_i64`） |
| `customer_id` | **string** | `t_part_file.part_id` —— ⚠️ **列名叫 `part_id`，装的是 `t_customer.id`**（§8.3 第 2 条） |
| `customer_name` | string \| null | **本域自己的一条 `t_customer` 只读查询**（`deleted_at IS NULL` 时取 `name`）；查不到时为 `null`，**不报错**（§8.3 第 6 条）。⚠️ **前端 `printTemplateSchema` 是 `.strict()` 且必填它** —— 缺这个键会让「上传 / 列表 / 自动选用」三条消费路径**全部**抛 ZodError（前端 `.strict()` 直接拒整个响应，不是「显示成空」）。两条填充路径见 §2.4 |
| `kind` | string | 行上的真实列，恒为 `"DELIVERY_TEMPLATE"` |
| `file_type` | string | 行上的真实列，恒为 `"XLSX"`（`policy::file_type_for_ext("xlsx")`） |
| `original_filename` | string | 上传时的原始文件名（模板管理页列表列显示它） |
| `file_size` | number | `t_part_file.file_size`（字节） |
| `content_type` | string | 行上的真实列，恒为 xlsx 规范 MIME（上传闸门只收这一个精确值，§4.3） |
| `content_sha256` | string \| null | 内容 SHA-256 hex（64 字符小写） |
| `version` | number | **OCC 锚点**（`t_part_file.version`），端点 4 的 body 必须回传它 |
| `created_at` / `updated_at` | string | `t_part_file` 的两列，naive timestamp（Asia/Shanghai 写入） |

### 2.1 ⚠️ **刻意不出参**的 4 个字段及理由

| 不回显 | 理由 |
|---|---|
| `object_key` | 前端拿不到也不需要 COS 原始 key（端点 3 是后端代理字节）。**顺带避免把 CAS key 的内部结构变成对外契约** |
| `upload_status` | 写路径恒落 `READY`（`NewPartFile.upload_status = "READY"`）⇒ 恒定字段，零信息量 |
| `paired_file_id` | CNC 配对专用（`G_CODE ↔ SETUP_SHEET` 互指），模板恒 `NULL` |
| `created_by` / `updated_by` / `deleted_at` | 模板管理页不需要操作者名；软删态由 §4.2 的「替换」语义保证不外露 |

⚠️ **本 VO 刻意不复用 `files::part_file::vo::PartFileOut`**：后者的
`owner_kind` 字段是**从请求参数回显**的（`service::render_out(&row, owner_kind)`
取形参、不取行上任何列 —— 那处已登记在 `files/part_file/mod.rs` 的已知偏差第 7 /
9 条）。本域的读端点是**按 `(part_id, kind='DELIVERY_TEMPLATE')` 取回**的，
回显不出 `CUSTOMER`。照抄一份 VO 出来、再让 `owner_kind` 变成「要么对、要么空」
的字段，只会把那处偏差复制到本域。

### 2.2 `PrintTemplateResolveOut`（端点 2）

| 字段 | 类型 | 来源 |
|---|---|---|
| `template` | `PrintTemplateOut` | 见 §2 |
| `customer_name` | string \| null | **本域自己的一条 `t_customer` 只读查询**（`deleted_at IS NULL` 时取 `name`）；客户不存在 / 已软删时为 `null`，**不报错** —— 模板行本身还在（软删闸门只管 `t_part_file`），此时前端展示模板即可，不必因客户改名 / 下架而打不开打印对话框 |

### 2.3 `PrintTemplateListOut`（端点 1）

| 字段 | 类型 | 口径 |
|---|---|---|
| `items` | `PrintTemplateOut[]` | `created_at DESC`（与 `files` 域 `list_with_filters` 逐字同款）；**只含未软删行** |
| `total` | number | 独立 COUNT，**不受** `limit` / `offset` 截断影响 |

### 2.4 `customer_name` 的两条填充路径（⚠️ 与 N+1 无关的取舍）

两个**读**端点走**两条不同的填充路径**：

| 端点 | 怎么填 | 语句数 |
|---|---|---|
| 1 `GET /`（列表） | `service::customer_names` —— **一条 `WHERE id = ANY($1)`** 一次取回全部客户名，在内存里 join（`HashMap` 查表） | 与返回行数无关（见 §8.3） |
| 2 `GET /resolve` | `service::customer_name` —— 与 §2.2 那个顶层字段**同一条查询、同一个值**，一个 `clone()` 后分别填两处 | 逐字未变 |

⚠️ **写入侧（`POST /files/objects/confirm`）不填 `customer_name`**：它回的是
`R<PartFileOut>`（files 域的文件行投影，18 字段，**没有**这一列）。前端写完模板
**刷新列表**（端点 1）才能拿到名字。

⚠️ **`customer_name`（行内）与 §2.2 的 `customer_name`（resolve 顶层）是两个字段、
刻意重复**：前端在「模板管理列表」读行内那个、在「打印对话框」读顶层那个
（其 schema 已按 `{template, customer_name}` 嵌套写死）。删掉任一个都会再破一次前端，
让前端在两处各判一次 null 也不划算（同一份数据、同一种查法）。

⚠️ **端点 1（列表）绝不逐行查名**：那正是本仓明令禁止的 N+1（功能上完全正确、测试照样
全绿，只有语句数随行数线性涨）。守卫见 `src/modules/print_template/service.rs`
模块 doc 的「取数恒 ≤ 3 条语句」一节与该文件末尾的 `mod query_guard`（lib 单测，
读源码：循环体内出现 `sqlx::query` 即红 + 两条 helper 各恰好一条查询 + 列表端点
必须走批量那条）。集成测试
`customer_name_is_present_on_upload_list_and_resolve` /
`customer_name_is_null_after_customer_soft_deleted` 钉的是**出参取值**
（命中 / `null`），语句数由那道源码护栏钉。

## 3. 写入契约（`files` 域统一链路）

### 3.1 请求形态

见 §1.0 的三步。**本域没有 multipart 端点了**（`POST /api/v2/print-templates`
于 2026-10-15 下线，旧路径 POST 返回 **405** —— 路径本身还在，端点 1 的 GET
挂在同一个 `.route("/", get(h))` 上）。

### 3.2 ⚠️ 「这份 xlsx 是不是真 xlsx」**不再由后端校验**（有意弱化）

原有的四道轻校验（空字节 / 扩展名 / 大小 / **ZIP magic**）随 multipart 端点
一起下线。扩展名、大小两道**由两条通用写入口继续把**（`upload-intents` 与
`confirm` 都判 `policy::allowed_exts("DELIVERY_TEMPLATE") == ["xlsx"]`、MIME
精确匹配、`file_size` 上下限）；**ZIP magic 与 xlsx 内部结构这两道彻底没有
后端了** ——

原因：统一链路后字节由前端直传 COS，`confirm` 只拿到 `head`（size / etag），
**拿不到内容**。这不是取舍偏好而是**能力边界**：要答这个问题就得把整份字节
再拉回 rust 进程，那正是本次要消除的代理上传。

职责移交前端：选文件时用 hucre 逐格解析（`assertTemplateMatches`），解析不过
就不发起 upload-intents ⇒ 后端收不到「不是 xlsx 的 .xlsx」。

⚠️ **代价（有意登记）**：绕过前端直接打这两条通用端点的话，一份非 xlsx 的
`.xlsx` 能存进库，报错点后移到前端渲染时。
⚠️ **不要**在另一个时机「顺手补回来」—— 那会造出第二个失败面；若将来真要在
后端校验结构，必须**先**与前端 `assertTemplateMatches` 对齐判据清单。

### 3.3 旧端点的请求形态与四道轻校验（**已下线**）

⚠️ 这一节留档是为了让读旧代码 / 旧文档的人立刻知道「那套契约已经不在了」。
**当前契约是 §1.0 + §3.2**，不要照这一节实现。

| # | 旧判据（`POST /api/v2/print-templates`，multipart） | 现在由谁判 |
|---|---|---|
| 1 | `bytes.is_empty()` → `40001` / 422 | `upload-intents` + `confirm`：`file_size > 0`（40001） |
| 2 | 扩展名 ≠ xlsx → `21102` / 400 | `upload-intents` + `confirm`：`policy::allowed_exts`（21102） |
| 3 | `bytes.len() > COS_MAX_FILE_SIZE` → `21103` / 400 | 同上（21103） |
| 4 | 前 4 字节 ≠ `PK\x03\x04` → `21102` / 400 | **无人判** —— 移交前端（§3.2） |

旧 multipart 形态：`data`（JSON 文本，`{"customer_id":"<雪花 id>"}`）+ `file`
（xlsx 字节）。`owner_kind` / `kind` 当时由服务端钉死；**现在由前端在
upload-intents 的 body 里显式给出**（`"CUSTOMER"` / `"DELIVERY_TEMPLATE"`），
服务端用 `policy::owner_kind_allows_kind` 强制配对。

## 4. 口径表

### 4.1 绑定粒度：只绑一级客户（`t_customer.parent_id IS NULL`）

用户拍板。送货单的 `customer_path` 是 L1 / L2 两级，但打印抬头、地址、抬头行数
与顺序是**按客户**的：L2 与其父 L1 往往是同一套抬头，绑到 L2 会立刻出现「同一个
客户名下两个 L2 模板打架」。

写侧**刻意不区分**三种失败情形 —— 「不存在 / 已软删 / 不是一级客户」一律返
**`20104 BIZ_INVALID_VALUE`**（HTTP 400），message 里带原始 id 让前端自己改选。
**不要**为了「让前端知道到底是哪个原因」而拆成三个错误码：那需要前端为同一个
用户动作再发一次请求去查。

⚠️ **2026-10-15：这条不变式现在只有**一道**强制点、错误码是 21105** ——
本域的上传端点下线后（模板写入改走 §1.0 的两条通用写入口），前置的
`service::l1_customer_name` 那个 20104 强制点随之删除；L1 判据只存在于
`PartFileRepoTrait::customer_owner_exists`（谓词含 `parent_id IS NULL`），
`upload-intents` 与 `confirm` **共用**它。
⇒ 前端看到 **21105** 的含义是「你给的那个 id 不能当模板的 owner」，处理方式是
改选客户。**不要**在 21105 上做「重试」或「换个入口」。

### 4.2 「替换」语义的实现（顺序**被唯一索引逼死**）

`uk_t_part_file_single (part_id, kind) WHERE deleted_at IS NULL AND kind IN
(…, 'DELIVERY_TEMPLATE')` 表达的是「**一个 L1 至多一份活跃模板**」。所以「替换」
在 DB 层只能这样实现：

这两步都在 **`POST /files/objects/confirm`**（`bind_uploaded_file`）的同一个事务里：

```
① soft_delete_active_by_part_kind(customer_id, by, "DELIVERY_TEMPLATE")
② create_part_file(同一个 &mut PgConnection, …)
③ commit
```

- **必须先软删**：先 INSERT 后软删 ⇒ 撞 `uk_t_part_file_single`（23505 ⇒ `21108`）。
- **必须共用同一个 tx**：否则 ① 成功、② 失败会留下「该客户**没有**模板」的空洞，
  而用户明明只想换一份。
- **共用 tx 的好处**：② 内部任何一步失败（扩展名 / content_type / INSERT）都会把
  ① 一起回滚 ⇒ 语义正确。
- ⚠️ **同内容重传不享受秒传省流**：① 刚把旧行软删，② 的 CAS 查重（`deleted_at
  IS NULL`）必然落空 ⇒ 重新 INSERT 一行。同内容的结果是「多一行同 key 的活跃
  行」，不是「复用旧行」。
- **删除 = 解绑**：`POST /{id}/delete` 之后该行 `deleted_at` 非空 ⇒ `resolve` 立刻
  返 `21109`。行**仍在库里**（软删，审计留存），`version` 自增。

### 4.3 格式白名单（三张表逐格）

| 表 | `DELIVERY_TEMPLATE` / `xlsx` 的取值 | 刻意不做的事 |
|---|---|---|
| `policy::allowed_exts` | `&["xlsx"]` | **不收 `.xls`**（BIFF 二进制格式，前端 SheetJS / hucre 侧读它要走另一条兼容路径，需求原文就是 xlsx） |
| `policy::file_type_for_ext` | `"XLSX"` | — |
| `policy::expected_content_types_for_ext` | 仅 `application/vnd.openxmlformats-officedocument.spreadsheetml.sheet` | **不给 `application/octet-stream` 兜底**（同 pdf 那支的纪律）—— 收下它等于放行「改名成 `.xlsx` 的任意文件」，而模板要被前端 SheetJS / hucre 直接解析，伪装进来只会在渲染阶段炸 |

### 4.4 `owner_kind × kind` 配对 + 「谁能写」收口（服务端强制）

| `owner_kind` | 允许的 `kind` |
|---|---|
| `PART` | `DRAWING` / `3D_MODEL` / `G_CODE` / `SETUP_SHEET` |
| `ASSEMBLY` | `ASSEMBLY_MASTER` |
| **`CUSTOMER`** | **`DELIVERY_TEMPLATE`** |

⚠️ **`CUSTOMER` 是单向的**：`PART` / `ASSEMBLY` 两臂刻意**不加**
`DELIVERY_TEMPLATE` —— 否则两条通用写入口能造出「零件挂送货单打印模板」这种
**无人读**的垃圾行。

#### 「谁能写 `DELIVERY_TEMPLATE`」必须在**共用上传原语**里收口，且**两条写入口共用一张表**

`t_part_file` 有**两条**写入口，`owner_kind` / `kind` 在两条里都**由 client 给**
（upload-intents / confirm 的 JSON body）⇒ 只在 `print_template` 域的 handler 上
守 MANAGER 是不够的（本域已无写端点，守的是 files 域那两条）。收口点是
`policy::write_roles_for(owner_kind, 入口)`：

| 写入口 | 非 CUSTOMER 的角色集合 | `CUSTOMER` |
|---|---|---|
| `POST /files/objects/upload-intents`（签发预签 PUT URL） | M / C / CNC | **仅 Manager**，且 `owner_id` 必须是**一级客户**（§4.1）⇒ 绑 L2 / 不存在 / 已软删都返 **21105 / HTTP 404** |
| `POST /files/objects/confirm`（登记 + 可选 CNC 配对） | M / C；`G_CODE` / `SETUP_SHEET` 为 M / CNC | **仅 Manager**（⚠️ 2026-10-15 起**允许**写 CUSTOMER —— 打印模板直传化走的就是它） |

⚠️ **`policy::write_roles_for` 的形参是 `WriteEntry` 入口枚举、不是可传错的常量**
（2026-10-16 review 第 2 轮 MINOR-B）：原签名 `write_roles_for(owner_kind, default_roles)`
把「这条入口的非 CUSTOMER 集合」当**位置参数**传 ⇒ 把 confirm 那处的
`CONFIRM_ROLES` 抄成 `UPLOAD_ROLES` 是静默的权限面放开（同文件、同类型、clippy
不报、lib 单测仍全绿）。现调用方只传「我是哪条入口」，常量由函数内部选，
**传错常量这件事在类型层面不存在**。

⚠️ **两条入口的角色闸门共用同一张表**，且 CUSTOMER 恒收窄为仅 Manager ——
少任一条就等于给 CLERK 留了一条「绕开本域、直接打通用端点」的路（§4.1 的
L1 判据同理）。

⚠️ **L1 判据必须落在两条入口共用的原语里**。曾经只判「存在且未软删」，
于是 **Manager 能造出一行绑在二级客户上的模板行**。三条后果：
① 破坏「只绑一级客户」这条用户拍板的不变式；
② `uk_t_part_file_single` 是 `(part_id, kind)`、L1 行与 L2 行互不冲突 ⇒「一个 L1
   至多一份活跃模板」在 (L1, L2) 这一对上被绕过，且给 L1 重新上传时
   `soft_delete_active_by_part_kind(l1_id)` **清不掉 L2 那行**；
③ 该行 `resolve` 永远读不到（送货单的 `customer_id` 是 L1）⇒ 变成**永不消费、
   永不被清理**的孤儿配置行。

⇒ 修法是把 L1 判据下沉进两道闸门共用的原语：
`PartFileRepoTrait::customer_owner_exists` 的谓词含 `AND parent_id IS NULL`
（返 **21105**）。

⚠️ **两条入口的非 CUSTOMER 集合刻意不同**：upload-intents 是 M+C+CNC，confirm 是
M+C（另有 CNC 配对两种 kind 的 M+CNC 分支）—— 合并成一张表会顺带把 confirm 的
图纸面放开，那是**权限面放开**，与本轮方向相反。

回归网（全部从 HTTP 外部打，`tests/print_template/resolve.rs`）：
`clerk_cannot_bypass_via_generic_upload_intents_endpoint`（打 upload-intents ⇒ 403）+
`manager_bypass_control_group_really_reaches_the_generic_endpoints`（**对照组**：同一
body 换 Manager + 换**真 L1** ⇒ 走完 upload-intents + confirm 并真的落库，证明被拒
的确实是角色闸门）+
`manager_cannot_bind_template_to_l2_customer_via_generic_endpoint`（同一 body 只把
`owner_id` 换成 L2 ⇒ **21105**，并断言零副作用：L2 名下无行 + 桶里无对象 +
父 L1 未被碰）+
`clerk_cannot_bypass_via_generic_confirm_endpoint`（confirm ⇒ 403）+
`manager_can_write_template_via_confirm_endpoint`（confirm + Manager ⇒ **200 且落库**，
钉「模板写入只剩 confirm 这一条路」）。

#### ⚠️ 通用读端点也不许漏（review 第 1 轮 MAJOR-2 / MAJOR-3）

| 端点 | 守卫 | 对 `DELIVERY_TEMPLATE` 行 |
|---|---|---|
| `GET /files/objects/{id}/url` | 权限点 `part.drawing.print`（默认 M / C / I / CNC） | **21102** —— 共用 `policy::is_file_content_kind` 白名单 |
| `GET /files/objects/{id}/content` | 5 角色（含 `SHELF_ACCOUNT`） | **21102** —— 同一张白名单 |
| `GET /files/objects`（列表） | 5 角色（含 `SHELF_ACCOUNT`） | **永不返回**（显式 `?kind=DELIVERY_TEMPLATE` 也是空） |

首版只守了 `content`：`/url` 回的是 **1 小时有效的 COS 预签直链**（拿到即可脱离本后端
直接下载、可外传），CLERK / INSPECTOR / CNC 完全绕得过去；列表端点则把
「一级客户 id + object_key + 文件名 + sha256」这组元数据漏给了工控机账号（越权读链的
**发现**环节）。⇒ **凡是能暴露 `t_part_file` 行的通用端点，都要显式决策模板行的去留**。

### 4.5 分页 / 排序

- 缺省 `limit = 50`，clamp `[1, 200]`（`dto::LIST_LIMIT_MAX`）；
- `offset` 缺省 0，`max(0)`；
- 排序 `created_at DESC`；
- ⚠️ `list_with_filters` 的 `owner_kind` 形参**不进 SQL**（表上没有 owner 列），
  所以 **`kind` 谓词是列表端点唯一的隔离依据**。集成测试
  `list_excludes_non_template_rows` 专门造一行「`part_id` 恰好等于客户号」的
  `DRAWING` 行来钉这一点。
- ⚠️ **`exclude_kind` 形参**（2026-10-16 review 第 1 轮 MAJOR-3 新增，`None` / `None`）：
  它是 files 域通用列表端点排除模板行的入口，**本域必须传 `None`**（本端点要的正是
  这一种 kind）。两条路径互相独立，**不要**「顺手把两边统一成同一张表」。

### 4.6 雪花 id 一律是 JSON string

本仓无 DB ENUM、无物理外键，雪花主键出入参统一 **string**
（`shared::types::serialize_i64`，防 JS 精度截断）。`customer_id` 传数字会
`20104`（**不是** 422 纯文本 —— 前端要拿到 `{code, message}` 才能弹提示）。

## 5. 状态域约定（无编译期保障）

本域**没有**状态机。

- `t_part_file.deleted_at`：`NULL` = 活跃（= 已绑定）；非 `NULL` = 已解绑（软删）。
- `t_part_file.upload_status`：本域写入恒为 `READY`，**不读**。
- `t_part_file.version`：整数 OCC 锚点，INSERT 时 `0`，每次软删 `+1`。
- ⚠️ 这三者**都在 `files` 域的表上、语义由 `files` 域的 CHECK / 索引 / 代码定义**。
  本域**没有**自己的枚举常量可依赖，改这些取值要去 `files` 域 / migration 改。

### 5.1 前端可依赖的字面量

| 字面量 | 出现处 |
|---|---|
| `"DELIVERY_TEMPLATE"` | 出参 `kind`；DB CHECK 约束第 7 个值 |
| `"XLSX"` | 出参 `file_type` |
| `"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"` | 出参 `content_type` + 端点 3 的 `Content-Type` 响应头 + 两条通用写入口的 content_type 白名单（三处同值） |
| `"CUSTOMER"` | 仅内部（`dto::OWNER_KIND`），**不出现在任何出参** |

## 6. 移除记录

### 6.1 后端：**本域从未有过**、且本轮**逐字不恢复**的东西

| 项 | 说明 |
|---|---|
| `POST /com/delivery/note/{id}/print` | **2026-10-08** 随送货单域打印链路下线（`handler/print.rs` 227 行 + `PyBackendClient::forward_delivery_note_print` 一并删）。详见 `docs/api/delivery_note.md` §6.1 / §6.3。渲染一直在前端 hucre，本域**不恢复** |
| `POST /com/delivery/note/{id}/print-labels` | 同上 |
| `t_delivery_note_template` / `t_delivery_template`（任何形态的模板表） | 本域**从不建模板表**（§0.3）。这是「为什么不新建表」那条决策的实体 —— 后人若要建表，必须先推翻 §8.3 第 1–2 条 |
| `t_part_file_templates` / `t_print_template` 之类的新表 | 同上 |
| `owner_kind` **列**（`t_part_file` 上的） | 不加（§8.3 第 2 条） |

### 6.2 前端侧历史（不在本仓，仅记录以免后人重复造）

| 时间 | 项 | 说明 |
|---|---|---|
| 2026-09-24 前 | `PrintTemplateDesigner.vue` | 随前端 `x-data-spreadsheet` 弃用而**整文件删除**。本轮**不恢复**它 —— 模板管理改走后端存储 + 前端普通上传控件，不做在线表格设计器 |

### 6.3 本域**没有**提供的东西（明确不承诺，防后人误接）

| 不提供 | 说明 |
|---|---|
| 写入用的 upload-intents / confirm 端点（在本域的 `/api/v2/print-templates/*` 之外） | 模板写入走 `files` 域统一链路（§1.0）；本域**不代理**它们 |
| WS 事件 | 见 §7 |
| 模板历史 / 版本回滚 | 写入是立即替换，旧行虽在库里（软删）但**没有**端点能把它恢复成活跃态 |
| 按客户维度的模板**删除**（不解绑即删） | 只有 `POST /{id}/delete`（软删 = 解绑）。重新绑定 = 再走一遍 §1.0 |
| xlsx 内部结构校验 / 转换（.xls→.xlsx） | §8.3 第 3 条 |

## 7. 与 WS 的关系

**本域不发任何 WS 事件**（4 条端点全部无 `ws_hub.broadcast` 调用）。

理由与后果：

- 模板是**低频变更**的配置资源（一年可能改几次），不是「每分钟都在动」的状态；
- 打印对话框的模板解析走**每次打开时的一次 HTTP 调用**（端点 2）⇒
  「自动选择已绑定的模板」天然是新鲜的，不需要事件推送；
- ⚠️ **代价（已登记，不修）**：若将来同页开了「模板管理列表」又同时开着打印对话框，
  别的管理员改了该客户的模板后，前端的列表**不会自动刷新** —— 前端要么用
  TanStack Query 的 `staleTime` + 重新聚焦时 refetch，要么手动刷新页面。
  真要推送的话，新事件 kind 应挂在**送货单**语义下（`DELIVERY_NOTE_*`）而不是
  新开一类，理由同 `docs/api/delivery_note.md` §7。

## 8. 表依赖与前端配套

### 8.1 表依赖（本域**没有自有表**）

| 表 | 用途 | 写? |
|---|---|---|
| `t_part_file` | 模板字节索引 + 绑定关系载体（`kind='DELIVERY_TEMPLATE'`，`part_id` = L1 客户 id） | ✅ 写入经 `POST /files/objects/confirm`（软删 + INSERT）、端点 4 软删；端点 1 / 2 / 3 读 |
| `t_customer` | ① 端点 2（resolve）的单条 `customer_name`；② 端点 1（列表）的**批量**客户名（一条 `id = ANY($1)`）。⚠️ **L1-only 判据不在本文件** —— 它在 `files::part_file` 的 `customer_owner_exists`（谓词含 `parent_id IS NULL`） | ❌ 纯读 |
| COS 对象存储 | 模板字节（key 由 `util::cos_key::build_cas_key` 生成，内容寻址） | 写入侧 PUT（**前端直传**，不经 rust）/ 读端点 3 取字节；DELETE（端点 4，commit 后异步） |

⚠️ **`files` 域是全仓唯一持有 `t_part_file` 写入路径的域**（见 `CLAUDE.md`
「`src/modules/files/*` 子模块清单」）。本域的写入**全部经它的
`PartFileService`**，不自己写 SQL 改那张表 —— 本域唯一的自有 SQL 是
`service.rs` 里那**两条**对 `t_customer` 的只读查询（运行时 `sqlx::query_scalar` /
`sqlx::query_as`，**不进 `query!` 宏**、不需要 `.sqlx/` 离线缓存）；⚠️ **列表端点的
取数恒 ≤ 3 条语句、与模板条数无关**（§8.3 第 16 条，含源码级护栏的位置）。

⚠️ **本域刻意不装 `assert_no_foreign_domain` 域隔离护栏**：它是**转发型**域
（同 `prod::scan` / `wx`），字节读写全部经 `files::part_file` 是设计而非越界。
硬装这道护栏只会逼出「把 files 域的写法复制一份到本域」这种更糟的结果。

### 8.2 前端配套改动清单

1. **api 层新增一个常量** `/print-templates`（4 条端点，路径逐字见 §1）。
   菜单**子项** `print_templates_designer`（「模板管理」→ 目录 `template_management`
   → 子项「模板编辑」）的 `path` 已是 `/print-templates`，
   `seeds/menu.sql` **无需改动**。
   ⚠️ 一级目录 `template_management` 自己的 `path` 是 **NULL**（目录项、不指向
   页面）—— 别拿它当本域路由的依据（2026-10-16 review 第 1 轮 MINOR-4 订正）。
2. **模板管理页**（新建或改造）：列表调端点 1（`?customer_id=` + 分页）、
   上传走 §1.0 的三步（upload-intents → PUT → confirm，成功看 **200**）、
   删除调端点 4（**必须回传 `version`**）。
   ⚠️ **`PrintTemplateOut` 带 `customer_name`**，且它是 `printTemplateSchema` 的
   **必填**键（`.strict()`）⇒ 后端少这个键时前端会在**解析响应时**抛 ZodError
   （列表恒空），不是「显示成空」。客户已软删时该字段是 `null`，前端按
   「（已删除的客户）」渲染即可。
   ⚠️ **写入侧的响应（`R<PartFileOut>`）没有 `customer_name`** ⇒ 上传成功后要
   **刷新列表**才能显示客户名。
   ⚠️ **ZIP magic / xlsx 结构由前端校验**（§3.2）：解析不过就别发起
   upload-intents。
3. **打印对话框**：先调端点 2 拿模板与 `customer_name`：
   - `21109` ⇒ 提示「该客户还没配模板」，并**保留手动上传兜底**（产品决议，
     不做成硬阻断）；
   - 命中 ⇒ 调端点 3 取字节，用它渲染打印件。
   ⚠️ 端点 3 的 `customer_name` **出现两次**：顶层一个、`template` 行内一个，
   **刻意重复**（同一条查询、同一个值）—— 两个都别删（§2.4）。
4. **权限点**：端点 2 / 3 用 `delivery_note.print`（前端 `hasPermission` 的
   输入）。端点 1 / 4 是 Manager-only，前端按钮显隐按角色判。
5. ⚠️ **写入侧成功是 200**（`POST /files/objects/confirm`）。
6. ⚠️ **雪花 id 是 string**：`customer_id` / `id` / `template_id` 出入参统一
   字符串形态，路径段拼 `encodeURIComponent` 不需要（纯数字）但**别用 `Number`
   转**。
7. ⚠️ **端点 3 返裸字节**、不进 JSON 信封 —— `fetch` 拿到的就是 `ArrayBuffer`，
   不要 `res.json()`。
8. ⚠️ **端点 4 的 body 缺 `version` ⇒ HTTP 422 纯文本**（不是业务信封）。
   前端不能靠 `code` 分支处理这个 422，只能当「请求体不合法」。

### 8.3 已知偏差登记

| # | 偏差 | 状态 |
|---|---|---|
| 1 | **不新建模板表** —— 模板的存在性与绑定关系是**同一件事**（一行未软删的 `DELIVERY_TEMPLATE`）。代价：`t_part_file` 要同时承载「文件元数据」与「配置实体绑定」两种语义 | **用户拍板 + 已登记**。收益：新表要自带 `id / version / created_* / updated_* / deleted_at` 一整套列，而字节本来就得走 COS + `t_part_file` 的 CAS key 与配额逻辑；「一个 L1 至多一份活跃模板」直接复用 `uk_t_part_file_single`。⚠️ 后人要建表，必须先推翻本条 |
| 2 | **`t_part_file` 没有 `owner_kind` 列**，`part_id` 一列现已承载**三种 owner**：① `t_part.id`（DRAWING / 3D_MODEL / G_CODE / SETUP_SHEET / CAD_2D）、② `t_assembly.id`（ASSEMBLY_MASTER）、③ **`t_customer.id`（DELIVERY_TEMPLATE）**。三者共用同一个雪花 ID 空间、**无映射表** | **有意保留 + 已登记**（同 `files/part_file/mod.rs` 已知偏差第 9 / 10 条）。⚠️ **后果**：`WHERE part_id = $1` 这种**不带 kind** 的查询在语义上是跨三类 owner 的 —— **新增读路径必须带 `kind` 谓词**。本域 4 条端点：① 端点 1（列表，**SQL kind 谓词** `kind = 'DELIVERY_TEMPLATE'`）、② 端点 2（resolve，`get_by_part_kind`）**两条带 SQL kind 谓词**；端点 3 在 Rust 侧比 `row.kind`（`service::get_template_content`，非 SQL 谓词）、端点 4 靠 `soft_delete_file` 的 kind→角色分派 —— **两条按 kind 分派或校验**。写入侧的 `get_by_part_kind` 同款 SQL 谓词在 `files::part_file` 内 |
| 3 | **后端完全不校验 xlsx 内容**（含 ZIP magic 与内部结构）：统一链路后 `confirm` 只拿到 `head`（size / etag）、**拿不到字节** ⇒ 这类问题在后端**物理上无法回答**。结构合法性由前端 `assertTemplateMatches` 把关 | **有意弱化 + 已登记**（§3.2 有完整取舍）。代价：绕过前端直打两条通用写入口时，一份非 xlsx 的 `.xlsx` 能**存进库**（报错点后移到前端渲染时）。⚠️ 集成测试 `non_zip_xlsx_is_no_longer_rejected_by_backend` 把「后端不再拒绝」**钉成事实** —— 想在后端补结构校验的人必须先读它 ⚠️ 要补就**先**与前端 `assertTemplateMatches` 对齐判据清单，别零散加 |
| 4 | **孤儿 COS 对象**：confirm 失败时前端已经 PUT 出去的对象留在桶里，服务端**不做**补偿删除 | **有意保留 + 已登记**。对象 key 是**内容寻址**的（`{prefix}{sha16}_{safe_filename}`），同内容重传 PUT 到**同一个 key**（幂等覆盖）；真正的孤儿只有「传了内容 X、事务失败、内容 X 从此没人再传」这一种。又因 `soft_delete_active_by_part_kind` 先跑、而 confirm 的 CAS 查重（`get_by_owner_kind_sha`）带 `deleted_at IS NULL`，**「重传同一份」走的是「重新登记一行」**，省流优化在替换语义下取不到 |
| 5 | **`21109` 的 HTTP 状态码 422 是本次新选的** | **用户拍板 + 已登记**。该码在 wire 上**未变**（仍是 21109），变的只是「谁产出它」：python 转发时代由 python 产出、rust 透传（Rust 侧零生产点），现由 `print_template` 域自产。`status_from_code` 里**显式登记**为 `UNPROCESSABLE_ENTITY`（原先显式登记的是 `BAD_REQUEST`，且 `(20000..30000) => BAD_REQUEST` 的兜底也会把它压成 400）。取 422 的理由：请求本身完全合法（客户存在、是一级客户、无越权），**不满足的是「这个客户的配置」** |
| 6 | **`customer_name` 查不到时给 `null` 而不是报错** | **有意保留**。模板行本身还在（软删闸门只管 `t_part_file`），此时前端展示模板即可，不必因客户改名 / 下架而打不开打印对话框 |
| 7 | **列表端点是 Manager-only**，而端点 2 / 3 对 CLERK / INSPECTOR 开放 —— 同一份数据的「读」与「列举」权限面不同 | **有意保留**。列表含**全部**客户的抬头信息（管理面）；端点 2 / 3 是单客户定向取（作业面，且必须先知道客户 id） |
| 8 | **写入侧曾能被 CLERK 用来造 / 替换模板行**：两条写入口的角色闸门一度不共用 | **已闭合**（共用 `policy::write_roles_for` / `confirm_roles_for`，`CUSTOMER` 恒收窄为仅 Manager）。回归见 §4.4 的五条用例，含「Manager 拿同一 body 走通用链路 + 真 L1 成功落库」的对照组 |
| 9 | **写入侧曾能把模板绑到二级客户上**：`owner_id` 由 client 给，而 owner 存在性只判「存在且未软删」⇒ Manager 能造出一行绑在 L2 上的模板。三条后果：破坏「只绑一级客户」不变式 / `uk_t_part_file_single` 按 `(part_id, kind)` 判 ⇒ L1 与 L2 两行互不冲突、「一个 L1 至多一份活跃模板」被绕过且 `soft_delete_active_by_part_kind(l1_id)` 清不掉 L2 那行 / 该行 `resolve` 永远读不到 ⇒ 永不消费、永不清理的孤儿配置行 | **已闭合**（L1 判据下沉进共用原语 `PartFileRepoTrait::customer_owner_exists`：谓词含 `parent_id IS NULL`，错误码 **21105 / HTTP 404**）。回归：`manager_cannot_bind_template_to_l2_customer_via_generic_endpoint`（断言零副作用：L2 无行 + 桶里无对象 + 父 L1 未被碰） |
| 10 | **「L1-only」强制点只剩一道**（`PartFileRepoTrait::customer_owner_exists` ⇒ 21105）：本域上传端点下线后，`service::l1_customer_name` 那道 20104 前置检查随之删除 | **有意简化 + 已登记**（§4.1）。前端只需按 21105 改选客户。⚠️ 若本域将来恢复自己的写入端点，**要一并把 L1 前置检查加回来**，否则「同一个动作两种错误码」的前端分支会重新分叉 |
| 11 | **通用读端点曾会泄漏模板**（review 第 1 轮 MAJOR-2 / MAJOR-3）：`GET /files/objects/{id}/url`（1 小时预签直链，权限点矩阵 M/C/I/CNC）与 `GET /files/objects`（列表，含 `SHELF_ACCOUNT`）首版都不过滤 `DELIVERY_TEMPLATE` | **已闭合**（两条取字节端点共用 `policy::is_file_content_kind` 白名单 ⇒ 21102；列表端点**无条件** `exclude_kind` ⇒ 恒空）。回归见 §4.4 末表与 `tests/print_template/resolve.rs` 的两条用例 |
| 12 | **files 域与本域的 kind 守卫是两份、错误码不同**：files 域「白名单外的 kind」⇒ **21102 / HTTP 400**；本域「不是 `DELIVERY_TEMPLATE`」⇒ **21101 / HTTP 404** | **有意保留 + 已登记**（同 `files/part_file/mod.rs` 已知偏差第 14 条）。前端动作不同：21102 提示「换个入口」，21101 触发刷新列表。别为了统一而改任一侧 |
| 13 | **测试替身 `MockCos` 掩盖三条路径**（review 第 1 轮 MINOR-6）：① 真实 COS 的**错误分类** —— 替身对缺失 key 一律返 `BIZ_PART_FILE_UPLOAD_FAILED`，真 COS 是 `NoSuchKey`；② **etag 语义**（替身是拼出来的字符串，无条件写 / 部分写、真 COS 的 etag 参与一致性判断）；③ **「CAS 命中跳过 COS PUT」那条省流路径本 binary 完全没跑**（替换语义下 `soft_delete_active_by_part_kind` 先跑、CAS 查询带 `deleted_at IS NULL` ⇒ 取不到，与第 4 条同源） | **有意保留 + 已登记**。2026-10-16 已把本 binary 原来自造的**第三个** COS 替身（`MemoryCos`）删掉、能力上提到 `test-support` 的 `MockCos`（`put` 真存字节 / `delete` 真删字节），三条残余偏差属替身固有限制，不是回归。⚠️ 端点 4 的 COS 清理断言改用 `MockCos::stored_bytes`（「对象确实没了」）而非只看 `delete_calls` |
| 14 | **`GET /print-templates/{id}/content` 的 kind 校验在 Rust 侧而非 SQL 谓词**（`service::get_template_content` 比 `row.kind`）；端点 4 连 kind 都不比，只靠 `soft_delete_file` 的 kind→角色分派 | **有意保留 + 已登记**。理由：这两条路径**先取行**（端点 3 为了判「零外部 IO」、端点 4 为了拿 `object_key` 与派角色），行已在手里，比 `row.kind` 与 SQL 谓词等价，且少一次往返。别把它们「统一」成 SQL kind 谓词 |
| 15 | **行 VO 的 `customer_name` 缺了会让本域对着真前端 100% 不可用**（review 第 2 轮 C-1）：前端 `printTemplateSchema` 是 `.strict()` 且必填它，而首版 `PrintTemplateOut` 没有 ⇒ 上传即便返 **201** 也抛 ZodError、列表每行都抛（恒空）、页面「客户」列恒显「（已删除的客户）」。首版的 42 条用例全绿却钉不到它（断言只看自己写的那几个键） | **已闭合**（行 VO 补 `customer_name: Option<String>`，三条填充路径见 §2.4；新增两条集成测试钉**取值**：命中 / 客户软删后 `null`。⚠️ **列表不许逐行查名** —— 那正是 N+1；语句数与源码护栏见下一条）
| 16 | **列表端点的取数恒 ≤ 3 条语句（rows 1 + total 1 + 客户名 1；`rows` 为空时 2），与模板条数 / 客户数无关** | **有意保留 + 登记**。客户名走**一条 `WHERE id = ANY($1)`** 一次取回后在内存里 join（`HashMap`），**绝不**在 `rows.iter()` 里逐行查 —— 逐行查的写法**功能完全正确、集成测试照样全绿**，只有语句数会线性涨，所以只能靠 `src/modules/print_template/service.rs` 末尾 `mod query_guard` 的**源码级** lib 单测钉（循环体内出现 `sqlx::query` 即红 + 两条 helper 各恰好一条查询 + 列表端点必须走批量那条）。⚠️ **该护栏的已知漏报形态**：「循环里调别的函数、那个函数里发查询」这类**跨函数的间接 N+1** `no_query_inside_any_loop` 看不见，由同一模块里的 `list_templates_joins_customer_names_in_memory` 兜（它显式禁止列表端点调单条取名的 `customer_name(`）—— 但**新增第三个会发查询的 helper** 时要顺手扩那条断言。两条断言都经变异验证（手改成 N+1 后当场红），登记在 `query_guard` 的模块 doc 里。⚠️ 逐端点的确切语句数（端点 1 = 3 等）写在 `service.rs` 模块 doc 的表里，那里也注明「会过期、要确切数字自己读，别当契约」 |
| 17 | **`files::part_file::service::get_file_content` 已整条删除**（review 第 2 轮 MINOR-A）：它有角色闸门（5 角色）但**无 kind 闸门**，正是第 1 轮 MAJOR-2 关掉的那条绕行面，却以 `pub` 形态留在原地 —— 生产侧早已改走 `get_file_content_of_checked_row`，只剩两个测试在调 | **已闭合**（方法删除，两个测试调用方改成「`get_file_row` 取行 → `get_file_content_of_checked_row`」的生产同款两步）。⚠️ 本域不依赖它（端点 3 走最底层的 `get_file_content_for_row`），所以删除对其它域零影响。**别**因为「顺手」把它加回来 |
| 18 | **`policy::write_roles_for` 的默认集合形参换成 `WriteEntry` 入口枚举**（review 第 2 轮 MINOR-B）：原签名把「这条入口的非 CUSTOMER 角色集合」当**位置参数**传 ⇒ 把 confirm 那处的 `CONFIRM_ROLES` 抄成 `UPLOAD_ROLES` 是**静默的权限面放开**（同文件、同类型、clippy 不报、lib 单测仍全绿） | **有意保留 + 登记**。调用方现在只传「我是哪条入口」（`WriteEntry::Upload` / `WriteEntry::Confirm`），常量由函数内部选 ⇒ **传错常量在类型层面不存在**。⚠️ 代价：新增一条写入口时要加一个变体（那本来就该显眼 —— 新入口必须显式决定角色面） |
