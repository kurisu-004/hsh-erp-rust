# print_template 域 API（送货单打印模板：存 + 绑 + 取字节）

> 本文件是 `print_template` 域的**唯一**契约来源。任何字段 / 端点变更必须同步本文件。
> 代码位置：`src/modules/print_template/`（`mod.rs` · `handler.rs` · `service.rs` · `dto.rs` · `vo.rs`）。
> 权威路由清单：`handler::ROUTES`（5 条），lib 单测
> `handler::tests::routes_declared_in_router` 逐条比对 `router()` 源码
> （解析器复用 `files::route_guard::routes_in_router_source`，**不**另抄一份）。

## 0. 需求与范围

**用户原话**：「增加模板上传的功能，模板支持 xlsx 格式，当前用于打印送货单。前端的模板管理页面增加上传模板和查看已有模板的功能。且可以绑定一级客户。送货单打印时自动选择已绑定的模板」。

### 0.1 本域**做**什么（5 条端点，全在 §1）

1. 管理员在「模板管理」页给某个**一级客户**上传 / 替换 / 删除一份 xlsx 打印模板；
2. 打印对话框按「送货单所属的一级客户」**自动解析出**该用哪份模板（端点 3）；
3. 前端据此**取模板字节**（端点 4），再用它渲染打印件。

### 0.2 本域**不做**什么（明确划界，防后人误接）

| 不做 | 理由 |
|---|---|
| **不恢复任何「渲染 / 生成打印件」端点** | 打印渲染一直在**前端本地**（hucre）。`POST /com/delivery/note/{id}/print` 与 `print-labels` 于 2026-10-08 整链路下线（见 `docs/api/delivery_note.md` §6.1），本轮**逐字不恢复**。本域只管「存 + 绑 + 取字节」三件事 |
| **不做 xlsx 内部结构校验** | 后端刻意**不引入 `zip` / `calamine` 之类 crate**（§8.3 第 3 条） |
| **不绑二级客户** | 用户拍板「只绑一级客户」（§4.1） |
| **不做「传了份新模板」的通知 / 审批流** | 需求里没有；`POST /` 是**立即替换**语义 |

### 0.3 绑定关系 = 存在性（本域最核心的决策）

**不新建模板表**，复用 `files` 域自有的 `t_part_file`：`kind='DELIVERY_TEMPLATE'`、
`part_id` 列装的是**一级客户的 `t_customer.id`**。「该 L1 名下存在一行未软删的
`DELIVERY_TEMPLATE`」**本身就是绑定关系**。取舍全文见 §8.3 第 1–2 条。

## 1. 端点表（**5 个**）

| # | 方法 | 路径 | 权限 | 入参 | 响应 |
|---|---|---|---|---|---|
| 1 | POST | `/api/v2/print-templates` | `require_role(Manager)` | multipart：`data`（JSON 文本，**必填**）+ `file`（xlsx 字节，**必填**） | **201** + `PrintTemplateOut` |
| 2 | GET | `/api/v2/print-templates` | `require_role(Manager)` | `customer_id?`、`limit?`（缺省 50，clamp `[1,200]`）、`offset?`（缺省 0，`max(0)`） | `PrintTemplateListOut` |
| 3 | GET | `/api/v2/print-templates/resolve` | 权限点 `delivery_note.print` | `customer_id`（**必填**，JSON string 形态雪花 id） | `PrintTemplateResolveOut`；未绑定 ⇒ `21109` / HTTP **422** |
| 4 | GET | `/api/v2/print-templates/{template_id}/content` | 权限点 `delivery_note.print` | 无 | **裸字节**（非 `R<T>`）+ xlsx MIME 头 |
| 5 | POST | `/api/v2/print-templates/{template_id}/delete` | `require_role(Manager)` | JSON body **必含 `version`**（OCC） | `R<()>` |

### 1.1 权限分档（为什么读端点不是 MANAGER-only）

| 端点 | 守卫 | 默认放行集合 |
|---|---|---|
| 1 / 2 / 5 | `require_role(Manager)` | `MANAGER`（+ `ADMIN` 元角色短路） |
| 3 / 4 | 权限点 `delivery_note.print` | `MANAGER` / `CLERK` / `INSPECTOR` |

**读端点绝不能 MANAGER-only**：打送货单是 CLERK / INSPECTOR 的日常动作
（备货、点检、装车），管理面与作业面不是同一批人。权限点
`delivery_note.print` 的默认矩阵恰是那三个角色（`seeds/rbac.sql`），
`SHELF_ACCOUNT` **不在**矩阵内（工控机账号只该看图纸）。

⚠️ 2026-10-16 该权限点的描述从「前端-only，后端无打印端点，仅登记」改成
「打印对话框取模板 + 取 xlsx 字节的守卫点」。**授权矩阵逐字未变**，只改描述。

### 1.2 角色守卫下沉在 handler 第一行 + service 内复核

仓库惯例（`statistics` / `mail_digest` / `print_template` 同款）。端点 1 的守卫
还额外排在**读 multipart 之前** —— 避免未授权请求触发整包内存分配。

### 1.3 事务边界

| 端点 | 事务 | 理由 |
|---|---|---|
| 1 | `pool.begin()` → service → `tx.commit()` | **必须**有：「先软删旧行 + 再写新行」共用一个 tx，否则第二步失败会留下「该客户没有模板」的空洞（§4.2） |
| 2 / 3 | **不开**，`pool.acquire()` | 纯读 |
| 4 | **不开**，`pool.acquire()` | COS `get_object` 是外部 IO，包进 PG 事务会让连接被外部依赖长时间占住 |
| 5 | `pool.begin()` → service → `tx.commit()` → **commit 之后**才 spawn `cos.delete_object` | 「commit 在前、spawn 在后」（`files` 域 2026-09-15 review A2 纪律） |

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

端点 1 / 2 / 3 复用同一形状。

| 字段 | 类型 | 来源 / 口径 |
|---|---|---|
| `id` | **string** | `t_part_file.id`（雪花 ID 字符串化，防 JS 精度截断；`shared::types::serialize_i64`） |
| `customer_id` | **string** | `t_part_file.part_id` —— ⚠️ **列名叫 `part_id`，装的是 `t_customer.id`**（§8.3 第 2 条） |
| `customer_name` | string \| null | **本域自己的一条 `t_customer` 只读查询**（`deleted_at IS NULL` 时取 `name`）；查不到时为 `null`，**不报错**（§8.3 第 6 条）。⚠️ **前端 `printTemplateSchema` 是 `.strict()` 且必填它** —— 缺这个键会让上传（201 也一样）/ 列表 / 自动选用三条路径**全部**抛 ZodError（2026-10-16 review 第 2 轮 C-1 补）。三条填充路径见 §2.4 |
| `kind` | string | 行上的真实列，恒为 `"DELIVERY_TEMPLATE"` |
| `file_type` | string | 行上的真实列，恒为 `"XLSX"`（`policy::file_type_for_ext("xlsx")`） |
| `original_filename` | string | 上传时的原始文件名（模板管理页列表列显示它） |
| `file_size` | number | `t_part_file.file_size`（字节） |
| `content_type` | string | 行上的真实列，恒为 xlsx 规范 MIME（上传闸门只收这一个精确值，§4.3） |
| `content_sha256` | string \| null | 内容 SHA-256 hex（64 字符小写） |
| `version` | number | **OCC 锚点**（`t_part_file.version`），端点 5 的 body 必须回传它 |
| `created_at` / `updated_at` | string | `t_part_file` 的两列，naive timestamp（Asia/Shanghai 写入） |

### 2.1 ⚠️ **刻意不出参**的 4 个字段及理由

| 不回显 | 理由 |
|---|---|
| `object_key` | 前端拿不到也不需要 COS 原始 key（端点 4 是后端代理字节）。**顺带避免把 CAS key 的内部结构变成对外契约** |
| `upload_status` | 写路径恒落 `READY`（`NewPartFile.upload_status = "READY"`）⇒ 恒定字段，零信息量 |
| `paired_file_id` | CNC 配对专用（`G_CODE ↔ SETUP_SHEET` 互指），模板恒 `NULL` |
| `created_by` / `updated_by` / `deleted_at` | 模板管理页不需要操作者名；软删态由 §4.2 的「替换」语义保证不外露 |

⚠️ **本 VO 刻意不复用 `files::part_file::vo::PartFileOut`**：后者的
`owner_kind` 字段是**从请求参数回显**的（`service::render_out(&row, owner_kind)`
取形参、不取行上任何列 —— 那处已登记在 `files/part_file/mod.rs` 的已知偏差第 7 /
9 条）。本域 5 条端点里只有端点 1 走那条路径，端点 3 / 4 是**按 id 取回**的，
回显不出 `CUSTOMER`。照抄一份 VO 出来、再让 `owner_kind` 变成「要么对、要么空」
的字段，只会把那处偏差复制到本域。

### 2.2 `PrintTemplateResolveOut`（端点 3）

| 字段 | 类型 | 来源 |
|---|---|---|
| `template` | `PrintTemplateOut` | 见 §2 |
| `customer_name` | string \| null | **本域自己的一条 `t_customer` 只读查询**（`deleted_at IS NULL` 时取 `name`）；客户不存在 / 已软删时为 `null`，**不报错** —— 模板行本身还在（软删闸门只管 `t_part_file`），此时前端展示模板即可，不必因客户改名 / 下架而打不开打印对话框 |

### 2.3 `PrintTemplateListOut`（端点 2）

| 字段 | 类型 | 口径 |
|---|---|---|
| `items` | `PrintTemplateOut[]` | `created_at DESC`（与 `files` 域 `list_with_filters` 逐字同款）；**只含未软删行** |
| `total` | number | 独立 COUNT，**不受** `limit` / `offset` 截断影响 |

### 2.4 `customer_name` 的三条填充路径（⚠️ 与 N+1 无关的取舍）

端点 1 / 2 / 3 走**三条不同的填充路径**（review 第 2 轮 C-1）：

| 端点 | 怎么填 | 语句数 |
|---|---|---|
| 1 `POST /` | `service::l1_customer_name` —— **L1 强制点那条查询顺带把 `name` 取回来**（谓词本来就带 `parent_id IS NULL`）⇒ 零额外往返。⚠️ 「是不是 L1」与「叫什么」压在同一个 `Option` 上，安全前提是 **`t_customer.name` 是 NOT NULL**（baseline 迁移）—— 若日后改成可空，这道门会把「无名 L1 客户」误拒成 20104 | 与改前逐字相同 |
| 2 `GET /` | `service::customer_names` —— **一条 `WHERE id = ANY($1)`** 一次取回全部客户名，在内存里 join（`HashMap` 查表） | +1（见 §8.3 第 16 条） |
| 3 `GET /resolve` | `service::customer_name` —— 与 §2.2 那个顶层字段**同一条查询、同一个值**，一个 `clone()` 后分别填两处 | 与改前逐字相同 |

⚠️ **`customer_name`（行内）与 §2.2 的 `customer_name`（resolve 顶层）是两个字段、
刻意重复**：前端在「模板管理列表」读行内那个、在「打印对话框」读顶层那个
（其 schema 已按 `{template, customer_name}` 嵌套写死）。删掉任一个都会再破一次前端，
让前端在两处各判一次 null 也不划算（同一份数据、同一种查法）。

⚠️ **端点 2 绝不逐行查名**：那正是本仓明令禁止的 N+1（功能上完全正确、测试照样
全绿，只有语句数随行数线性涨）。守卫见 `src/modules/print_template/service.rs`
模块 doc 的「取数恒 ≤ 3 条语句」一节与该文件末尾的 `mod query_guard`（lib 单测，
读源码：循环体内出现 `sqlx::query` 即红 + 三条 helper 各恰好一条查询 + 列表端点
必须走批量那条）。集成测试
`customer_name_is_present_on_upload_list_and_resolve` /
`customer_name_is_null_after_customer_soft_deleted` 钉的是**出参取值**
（命中 / `null`），语句数由那道源码护栏钉。

## 3. 端点 1 的上传契约（multipart）

### 3.1 multipart 形态

```
Content-Type: multipart/form-data; boundary=…

--…
Content-Disposition: form-data; name="data"
（无 filename，纯文本字段）

{"customer_id":"<一级客户的雪花 id，JSON string 形态>"}

--…
Content-Disposition: form-data; name="file"; filename="甲公司送货单模板.xlsx"
Content-Type: application/vnd.openxmlformats-officedocument.spreadsheetml.sheet

PK\x03\x04…（xlsx 字节）
```

- 循环写法逐字照抄 `files::part_file::handler::upload_part_file`（`field.name()`
  分发 + `field.text()` / `field.bytes()`），**未识别的字段 drain 掉忽略**
  （多塞一个隐藏字段不该让整个请求失败）。
- ⚠️ `data` 里**只有 `customer_id` 一个键**：`owner_kind` / `kind` 由服务端钉死
  （`dto::OWNER_KIND` / `dto::TEMPLATE_KIND`），**不接受客户端指定**。与 `files` 域
  通用上传原语的 `UploadFileData`（三个键全由 client 给）恰好相反 —— 那边是个
  多用途原语，这边是单一用途端点，把那两个字段暴露出去只会多出「传错组合」的
  失败形态。
- ⚠️ **本域前端只走这一次性 multipart，不扩展 STS 两段式**：没有
  `POST /print-templates/confirm` 端点（§8.3 第 3 条）。

### 3.2 四道轻校验（**本域自己的**、在调 service 之前）

顺序即错误码优先级。全部**零事务、零 COS、零 DB**：

| # | 判据 | 错误码 | HTTP |
|---|---|---|---|
| 1 | `bytes.is_empty()` | `40001 VALIDATION_ERROR` | 422 |
| 2 | `ext_of(filename) != "xlsx"` | `21102 BIZ_PART_FILE_BAD_TYPE` | 400 |
| 3 | `bytes.len() > config.cos.max_file_size`（`COS_MAX_FILE_SIZE`，`.env.example` = 100MB） | `21103 BIZ_PART_FILE_TOO_LARGE` | 400 |
| 4 | 前 4 字节 ≠ `PK\x03\x04`（ZIP magic） | `21102 BIZ_PART_FILE_BAD_TYPE` | 400 |

⚠️ **第 4 道只挡「改名成 `.xlsx` 的非 zip」**，不解析 zip 内部结构
（§8.3 第 3 条）。magic bytes 就是 zip 的前 4 字节。

⚠️ **第 1 道是 40001 而不是 21103**：空文件是「无效输入」不是「太大」——
与 `files` 域那两道 size 守卫的取舍逐字一致。

⚠️ **第 2 道判据是「扩展名」不是「MIME」**：MIME 由 `files` 域的
`policy::expected_content_types_for_ext("xlsx")` 在**更晚一步**
（`upload_file_for_owner` 内部）校验，本域先做扩展名那道是为了**早失败**。

## 4. 口径表

### 4.1 绑定粒度：只绑一级客户（`t_customer.parent_id IS NULL`）

用户拍板。送货单的 `customer_path` 是 L1 / L2 两级，但打印抬头、地址、抬头行数
与顺序是**按客户**的：L2 与其父 L1 往往是同一套抬头，绑到 L2 会立刻出现「同一个
客户名下两个 L2 模板打架」。

写侧**刻意不区分**三种失败情形 —— 「不存在 / 已软删 / 不是一级客户」一律返
**`20104 BIZ_INVALID_VALUE`**（HTTP 400），message 里带原始 id 让前端自己改选。
**不要**为了「让前端知道到底是哪个原因」而拆成三个错误码：那需要前端为同一个
用户动作再发一次请求去查。

⚠️ **这条不变式有**两道**强制点、错误码不同**（2026-10-16 review 第 2 轮 MAJOR-A）：
本域这一侧（`service::l1_customer_name`，前置、零外部 IO）⇒ **20104**；
`files` 域那侧（`PartFileRepoTrait::customer_owner_exists`，管「绕过本域 handler
直调共用上传原语」那条路，如 `POST /files/objects`）⇒ **21105 / HTTP 404**。
⇒ 前端若看到 **21105**，含义是「你给的那个 id 不能当模板的 owner」，处理方式与
20104 相同（改选客户）。**不要**在 21105 上做「重试」或「换个入口」—— 没有别的
入口能写 CUSTOMER（confirm 直接拒）。逐条后果与回归见 §4.4 与 §8.3 第 9 / 10 条。

### 4.2 「替换」语义的实现（顺序**被唯一索引逼死**）

`uk_t_part_file_single (part_id, kind) WHERE deleted_at IS NULL AND kind IN
(…, 'DELIVERY_TEMPLATE')` 表达的是「**一个 L1 至多一份活跃模板**」。所以「替换」
在 DB 层只能这样实现：

```
① soft_delete_active_by_part_kind(customer_id, by, "DELIVERY_TEMPLATE")
② upload_file_for_owner(同一个 &mut PgConnection, …)
③ commit
```

- **必须先软删**：先 INSERT 后软删 ⇒ 撞 `uk_t_part_file_single`（23505 ⇒ `21108`）。
- **必须共用同一个 tx**：否则 ① 成功、② 失败会留下「该客户**没有**模板」的空洞，
  而用户明明只想换一份。
- **共用 tx 的好处**：`upload_file_for_owner` 内部任何一步失败（扩展名 /
  content_type / COS PUT / INSERT）都会把 ① 一起回滚 ⇒ 语义正确。
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
| `PART` | `DRAWING` / `3D_MODEL` |
| `ASSEMBLY` | `ASSEMBLY_MASTER` |
| **`CUSTOMER`** | **`DELIVERY_TEMPLATE`（仅本域）** |

⚠️ **`CUSTOMER` 是单向的**：`PART` / `ASSEMBLY` 两臂刻意**不加**
`DELIVERY_TEMPLATE` —— 否则 `POST /files/objects` 能造出「零件挂送货单打印模板」
这种**无人读**的垃圾行。

#### 「谁能写 `DELIVERY_TEMPLATE`」必须在**共用上传原语**里收口，且**两条写入口共用一张表**

`t_part_file` 有**两条**写入口，`owner_kind` / `kind` 在两条里都**由 client 给**
（multipart `data` / confirm 的 JSON body）⇒ 只在 `print_template` 域的 handler 上
守 MANAGER 是不够的。收口点是 `policy::write_roles_for(owner_kind, 默认集合)`：

| 写入口 | 非 CUSTOMER 的角色集合 | `CUSTOMER` |
|---|---|---|
| `POST /files/objects`（multipart 上传） | M / C / CNC（4 条旧上传端点的并集） | **仅 Manager，且 `owner_id` 必须是**一级客户**（§4.1）⇒ 绑 L2 / 不存在 / 已软删都返 **21105 / HTTP 404** |
| `POST /files/objects/confirm`（STS 直传回执） | M / C（逐字沿用旧 `POST /parts/{id}/files/confirm`） | **仅 Manager，且随后直接 20104 拒收** |

⚠️ **`policy::write_roles_for` 的形参是 `WriteEntry` 入口枚举、不是可传错的常量**
（2026-10-16 review 第 2 轮 MINOR-B）：原签名 `write_roles_for(owner_kind, default_roles)`
把「这条入口的非 CUSTOMER 集合」当**位置参数**传 ⇒ 把 confirm 那处的
`CONFIRM_ROLES` 抄成 `UPLOAD_ROLES` 是静默的权限面放开（同文件、同类型、clippy
不报、lib 单测仍全绿）。现调用方只传「我是哪条入口」，常量由函数内部选，
**传错常量这件事在类型层面不存在**。

⚠️ **2026-10-16 首版只给第一条收了角色**（review 第 1 轮 MAJOR-1）：confirm 的角色
闸门恒是 `M + C`、与 `owner_kind` 无关 ⇒ **CLERK 打 `POST /files/objects/confirm`
能造出并替换掉任意客户的模板行**。现已共用同一张分派表，且 confirm **额外直接拒
`owner_kind='CUSTOMER'`**（20104）：它没有本域那两条业务不变式（只绑一级客户 /
替换语义），放行等于给 Manager 一条绕过 L1-only 的路，而它**零个合法 CUSTOMER
调用方**。

⚠️ ⚠️ **2026-10-16 第 2 轮 MAJOR-A：第一条入口的 L1 判据也没收口**。上一条那两段
理由逐字适用于 `POST /files/objects` —— 首版在那边只收了角色（仅 Manager）、
`assert_owner_exists` 的 CUSTOMER 臂**只判存在**，于是 **Manager 能造出一行绑在
二级客户上的模板行**（201 + 真落库）。三条后果：
① 破坏「只绑一级客户」这条用户拍板的不变式；
② `uk_t_part_file_single` 是 `(part_id, kind)`、L1 行与 L2 行互不冲突 ⇒「一个 L1
   至多一份活跃模板」在 (L1, L2) 这一对上被绕过，且给 L1 重新上传时
   `soft_delete_active_by_part_kind(l1_id)` **清不掉 L2 那行**；
③ 该行 `resolve` 永远读不到（送货单的 `customer_id` 是 L1）⇒ 变成**永不消费、
   永不被清理**的孤儿配置行。

⇒ 修法**不是**再拒一次 `CUSTOMER`（那会让 `POST /files/objects` 彻底不能写
CUSTOMER、连本域复用 `upload_file_for_owner` 的正常路径也一起打死），而是**把 L1
判据下沉进两道闸门共用的原语**：`PartFileRepoTrait::customer_owner_exists` 的谓词
加 `AND parent_id IS NULL`（files 域那侧返 **21105**）。本域这一侧的 20104 强制点
（`service::l1_customer_name`，前置、零外部 IO）**保留不动** —— 两道错误码不同、
位置不同，**缺任一条都不成立**。

⚠️ **两条入口的非 CUSTOMER 集合刻意不同、刻意不合并成一张固定表**：合并会顺带把
confirm 放宽成含 CNC —— 那是**权限面放开**，与本轮要修的方向相反。

回归网（全部从 HTTP 外部打，`tests/print_template/resolve.rs`）：
`clerk_cannot_bypass_via_generic_upload_endpoint`（真打 `/files/objects` ⇒ 403）+
`manager_bypass_control_group_really_reaches_the_generic_endpoint`（**对照组**：同一 body
换 Manager + 换**真 L1** ⇒ 201，证明被拒的确实是角色闸门且请求真的到了那个端点）+
`manager_cannot_bind_template_to_l2_customer_via_generic_endpoint`（**MAJOR-A**：同一 body
只把 `owner_id` 换成 L2 ⇒ **21105**，并断言零副作用：L2 名下无行 + 桶里无对象 +
父 L1 未被碰）+
`clerk_cannot_bypass_via_generic_confirm_endpoint`（confirm ⇒ 403）+
`manager_cannot_write_template_via_confirm_endpoint`（confirm + Manager ⇒ 20104）。

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
| `"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"` | 出参 `content_type` + 端点 4 的 `Content-Type` 响应头 + 端点 1 的 content_type 白名单（三处同值） |
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
| `POST /confirm`（STS 两段式上传） | 本域只有一次性 multipart（§3.1）。⚠️ **连带效果**：`files` 域的 `dto::validate::check_kind` 复用 `allowed_exts`，所以 `POST /files/objects/confirm` **顺带**接受 `DELIVERY_TEMPLATE` —— 这是复用的副作用、不是设计意图，登记在 §8.3 第 3 条 |
| WS 事件 | 见 §7 |
| 模板历史 / 版本回滚 | `POST /` 是立即替换，旧行虽在库里（软删）但**没有**端点能把它恢复成活跃态 |
| 按客户维度的模板**删除**（不解绑即删） | 只有 `POST /{id}/delete`（软删 = 解绑）。重新绑定 = 再 `POST /` 一次 |
| xlsx 内部结构校验 / 转换（.xls→.xlsx） | §8.3 第 3 条 |

## 7. 与 WS 的关系

**本域不发任何 WS 事件**（5 条端点全部无 `ws_hub.broadcast` 调用）。

理由与后果：

- 模板是**低频变更**的配置资源（一年可能改几次），不是「每分钟都在动」的状态；
- 打印对话框的模板解析走**每次打开时的一次 HTTP 调用**（端点 3）⇒
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
| `t_part_file` | 模板字节索引 + 绑定关系载体（`kind='DELIVERY_TEMPLATE'`，`part_id` = L1 客户 id） | ✅ 端点 1 / 5 写（软删 + INSERT），端点 2 / 3 / 4 读 |
| `t_customer` | ① 端点 1 的 **L1-only 强制点**（`parent_id IS NULL`，**顺带取回客户名**）；② 端点 3 的 `customer_name`；③ 端点 2 的**批量**客户名（一条 `id = ANY($1)`） | ❌ 纯读 |
| COS 对象存储 | 模板字节（key 由 `util::cos_key::build_cas_key` 生成，内容寻址） | ✅ PUT（端点 1）/ DELETE（端点 5，commit 后异步） |

⚠️ **`files` 域是全仓唯一持有 `t_part_file` 写入路径的域**（见 `CLAUDE.md`
「`src/modules/files/*` 子模块清单」）。本域的写入**全部经它的
`PartFileService`**，不自己写 SQL 改那张表 —— 本域唯一的自有 SQL 是
`service.rs` 里那**三条**对 `t_customer` 的只读查询（运行时 `sqlx::query_scalar` /
`sqlx::query_as`，**不进 `query!` 宏**、不需要 `.sqlx/` 离线缓存）；⚠️ **列表端点的
取数恒 ≤ 3 条语句、与模板条数无关**（§8.3 第 16 条，含源码级护栏的位置）。

⚠️ **本域刻意不装 `assert_no_foreign_domain` 域隔离护栏**：它是**转发型**域
（同 `prod::scan` / `wx`），字节读写全部经 `files::part_file` 是设计而非越界。
硬装这道护栏只会逼出「把 files 域的写法复制一份到本域」这种更糟的结果。

### 8.2 前端配套改动清单

1. **api 层新增一个常量** `/print-templates`（5 条端点，路径逐字见 §1）。
   菜单**子项** `print_templates_designer`（「模板管理」→ 目录 `template_management`
   → 子项「模板编辑」）的 `path` 已是 `/print-templates`，
   `seeds/menu.sql` **无需改动**。
   ⚠️ 一级目录 `template_management` 自己的 `path` 是 **NULL**（目录项、不指向
   页面）—— 别拿它当本域路由的依据（2026-10-16 review 第 1 轮 MINOR-4 订正）。
2. **模板管理页**（新建或改造）：列表调端点 2（`?customer_id=` + 分页）、
   上传调端点 1（multipart，成功看 **201** 而不是 200）、
   删除调端点 5（**必须回传 `version`**）。
   ⚠️ **2026-10-16 review 第 2 轮 C-1**：`PrintTemplateOut` 现在**带
   `customer_name`**，且它是 `printTemplateSchema` 的**必填**键（`.strict()`）
   ⇒ 后端少这个键时前端会在**解析响应时**抛 ZodError（上传 201 也一样、列表恒空），
   不是「显示成空」。客户已软删时该字段是 `null`，前端按「（已删除的客户）」渲染即可。
3. **打印对话框**：先调端点 3 拿模板与 `customer_name`：
   - `21109` ⇒ 提示「该客户还没配模板」，并**保留手动上传兜底**（产品决议，
     不做成硬阻断）；
   - 命中 ⇒ 调端点 4 取字节，用它渲染打印件。
   ⚠️ 端点 3 的 `customer_name` **出现两次**：顶层一个、`template` 行内一个，
   **刻意重复**（同一条查询、同一个值）—— 两个都别删（§2.4）。
4. **权限点**：端点 3 / 4 用 `delivery_note.print`（前端 `hasPermission` 的
   输入）。端点 1 / 2 / 5 是 Manager-only，前端按钮显隐按角色判。
5. ⚠️ **状态码别再判 200**：端点 1 成功是 **201 Created**。
6. ⚠️ **雪花 id 是 string**：`customer_id` / `id` / `template_id` 出入参统一
   字符串形态，路径段拼 `encodeURIComponent` 不需要（纯数字）但**别用 `Number`
   转**。
7. ⚠️ **端点 4 返裸字节**、不进 JSON 信封 —— `fetch` 拿到的就是 `ArrayBuffer`，
   不要 `res.json()`。
8. ⚠️ **端点 5 的 body 缺 `version` ⇒ HTTP 422 纯文本**（不是业务信封）。
   前端不能靠 `code` 分支处理这个 422，只能当「请求体不合法」。

### 8.3 已知偏差登记

| # | 偏差 | 状态 |
|---|---|---|
| 1 | **不新建模板表** —— 模板的存在性与绑定关系是**同一件事**（一行未软删的 `DELIVERY_TEMPLATE`）。代价：`t_part_file` 要同时承载「文件元数据」与「配置实体绑定」两种语义 | **用户拍板 + 已登记**。收益：新表要自带 `id / version / created_* / updated_* / deleted_at` 一整套列，而字节本来就得走 COS + `t_part_file` 的 CAS key 与配额逻辑；「一个 L1 至多一份活跃模板」直接复用 `uk_t_part_file_single`。⚠️ 后人要建表，必须先推翻本条 |
| 2 | **`t_part_file` 没有 `owner_kind` 列**，`part_id` 一列现已承载**三种 owner**：① `t_part.id`（DRAWING / 3D_MODEL / G_CODE / SETUP_SHEET / CAD_2D）、② `t_assembly.id`（ASSEMBLY_MASTER）、③ **`t_customer.id`（DELIVERY_TEMPLATE）**。三者共用同一个雪花 ID 空间、**无映射表** | **有意保留 + 已登记**（同 `files/part_file/mod.rs` 已知偏差第 9 / 10 条）。⚠️ **后果**：`WHERE part_id = $1` 这种**不带 kind** 的查询在语义上是跨三类 owner 的 —— **新增读路径必须带 `kind` 谓词**。本域 5 条端点：① 端点 1（上传后 `get_file_row` 复读）、② 端点 2（列表，**SQL kind 谓词**）、③ 端点 3（resolve，`get_by_part_kind`）**三条带 SQL kind 谓词**；端点 4 在 Rust 侧比 `row.kind`（`service::get_template_content`，非 SQL 谓词）、端点 5 靠 `soft_delete_file` 的 kind→角色分派 —— **两条按 kind 分派或校验** |
| 3 | **后端不引 zip crate、不做 xlsx 内部结构校验**；只校验「前 4 字节是 `PK\x03\x04`」。xlsx 内部结构（有没有 `xl/workbook.xml`、命名空间、`print_area` / `print_titles` 是否被 hucre 认得）由前端已有的 `assertTemplateMatches` 把关 | **有意保留 + 已登记**。收益：零新依赖、零 CVE 面、零「后端解压用户上传的 zip」的 zip-bomb 面。代价：① 一份结构改坏的模板能**存进库**（报错点后移到前端渲染时）；② 「后端能不能解析 xlsx」这件事**不能**在集成测试里断言 —— 本域的测试用「最小 zip 头 + EOCD」字节，它**不是**能被 SheetJS 打开的工作簿；③ ⚠️ **`POST /files/objects/confirm` 顺带接受 `owner_kind="CUSTOMER"` + `kind="DELIVERY_TEMPLATE"`**（`check_kind` 复用同一张白名单）—— 副作用而非设计意图。**2026-10-16 review 第 1 轮 MAJOR-1 已把它彻底堵死**：`bind_uploaded_file` 现在 ① 角色闸门与上传入口共用 `policy::write_roles_for`（`CUSTOMER` ⇒ 仅 Manager）、② **随后直接拒 `owner_kind="CUSTOMER"` ⇒ 20104**。⇒ 现 confirm **写不出**任何模板行（既 CLERK 写不出、Manager 也写不出）。⚠️ 要在后端补结构校验，须**先**与前端 `assertTemplateMatches` 对齐判据清单，别零散加 |
| 4 | **孤儿 COS 对象**：端点 1 的 COS PUT 发生在事务内（沿用 `files` 域现状）；事务回滚后新对象已 PUT 出去、有意**不**加补偿删除 | **有意保留 + 已登记**。缓解：对象 key 是**内容寻址**的（`{prefix}{sha16}_{safe_filename}`），同内容重传 PUT 到**同一个 key**（幂等覆盖），CAS 去重命中时压根不再 PUT ⇒ 真正的孤儿只有「传了内容 X、事务回滚、内容 X 从此没人再传」这一种。⚠️ 顺带效果：因 `soft_delete_active_by_part_kind` 先跑、而 `upload_file_for_owner` 的 CAS 去重查询 `get_by_owner_kind_sha` 带 `deleted_at IS NULL`，**「重传同一份」走的是「重新 PUT（幂等）+ INSERT 新行」**，省流优化在替换语义下取不到 |
| 5 | **`21109` 的 HTTP 状态码 422 是本次新选的** | **用户拍板 + 已登记**。该码在 wire 上**未变**（仍是 21109），变的只是「谁产出它」：python 转发时代由 python 产出、rust 透传（Rust 侧零生产点），现由 `print_template` 域自产。`status_from_code` 里**显式登记**为 `UNPROCESSABLE_ENTITY`（原先显式登记的是 `BAD_REQUEST`，且 `(20000..30000) => BAD_REQUEST` 的兜底也会把它压成 400）。取 422 的理由：请求本身完全合法（客户存在、是一级客户、无越权），**不满足的是「这个客户的配置」** |
| 6 | **`customer_name` 查不到时给 `null` 而不是报错** | **有意保留**。模板行本身还在（软删闸门只管 `t_part_file`），此时前端展示模板即可，不必因客户改名 / 下架而打不开打印对话框 |
| 7 | **列表端点是 Manager-only**，而端点 3 / 4 对 CLERK / INSPECTOR 开放 —— 同一份数据的「读」与「列举」权限面不同 | **有意保留**。列表含**全部**客户的抬头信息（管理面）；端点 3 / 4 是单客户定向取（作业面，且必须先知道客户 id） |
| 8 | **`POST /files/objects/confirm` 曾能造 / 替换模板行**（review 第 1 轮 MAJOR-1）：首版只给 `upload_file_for_owner` 收了口，confirm 的角色闸门恒为 `M + C` 且**不判 L1-only** ⇒ CLERK 能替别人换模板 | **已闭合**（两条写入口共用 `policy::write_roles_for`；confirm 额外拒 `owner_kind='CUSTOMER'` ⇒ 20104，**走 confirm 这条路 Manager 也写不出**。回归见 §4.4 的五条用例，含「Manager 拿同一 body 走 `/files/objects` + 真 L1 成功」的对照组。⚠️ **别把本条读成「Manager 在任何入口都写不出」**：第 2 轮 MAJOR-A 见下一条） |
| 9 | **`POST /files/objects` 曾能把模板绑到二级客户上**（review 第 2 轮 MAJOR-A）：该端点 CUSTOMER 侧已收窄到仅 Manager，但 `assert_owner_exists` 的 CUSTOMER 臂**只判存在**、不判 `parent_id IS NULL` ⇒ Manager 走它能造出一行绑在 L2 上的模板（201 + 真落库）。三条后果：破坏「只绑一级客户」不变式 / `uk_t_part_file_single` 按 `(part_id, kind)` 判 ⇒ L1 与 L2 两行互不冲突、「一个 L1 至多一份活跃模板」被绕过且 `soft_delete_active_by_part_kind(l1_id)` 清不掉 L2 那行 / 该行 `resolve` 永远读不到 ⇒ 永不消费、永不清理的孤儿配置行 | **已闭合**（L1 判据下沉进共用原语 `PartFileRepo::customer_owner_exists`：`AND parent_id IS NULL`，错误码沿用同族 **21105 / HTTP 404**）。⚠️ 修法**刻意不是**「对 CUSTOMER 直接 20104」—— 那会把本域复用 `upload_file_for_owner` 的正常路径一起打死；两条入口的形态保持不对称（confirm 拒 CUSTOMER、`/files/objects` 收 CUSTOMER 但只收一级）。回归：`manager_cannot_bind_template_to_l2_customer_via_generic_endpoint`（断言零副作用：L2 无行 + 桶里无对象 + 父 L1 未被碰） |
| 10 | **「L1-only」有**两道**强制点、错误码不同**：本域 `service::l1_customer_name`（前置、零外部 IO）⇒ **20104**，与 `PartFileRepo::customer_owner_exists`（files 域兜底，管「绕过本域 handler 直调共用上传原语」）⇒ **21105** | **有意保留 + 已登记**。同一份判据、两个位置两个错误码：写侧刻意不区分「不存在 / 已软删 / 不是一级」三种情形（前端只需改选），而 files 域那侧要管的是「这个 owner 能不能挂文件」。**两道都要留**，别因为「files 域不该知道客户分两级」删掉后者（本域判的是 `parent_id IS NULL` 这个**列取值**，与它判 `t_part.assembly_id IS NULL` 同形） |
| 11 | **通用读端点曾会泄漏模板**（review 第 1 轮 MAJOR-2 / MAJOR-3）：`GET /files/objects/{id}/url`（1 小时预签直链，权限点矩阵 M/C/I/CNC）与 `GET /files/objects`（列表，含 `SHELF_ACCOUNT`）首版都不过滤 `DELIVERY_TEMPLATE` | **已闭合**（两条取字节端点共用 `policy::is_file_content_kind` 白名单 ⇒ 21102；列表端点**无条件** `exclude_kind` ⇒ 恒空）。回归见 §4.4 末表与 `tests/print_template/resolve.rs` 的两条用例 |
| 12 | **files 域与本域的 kind 守卫是两份、错误码不同**：files 域「白名单外的 kind」⇒ **21102 / HTTP 400**；本域「不是 `DELIVERY_TEMPLATE`」⇒ **21101 / HTTP 404** | **有意保留 + 已登记**（同 `files/part_file/mod.rs` 已知偏差第 14 条）。前端动作不同：21102 提示「换个入口」，21101 触发刷新列表。别为了统一而改任一侧 |
| 13 | **测试替身 `MockCos` 掩盖三条路径**（review 第 1 轮 MINOR-6）：① 真实 COS 的**错误分类** —— 替身对缺失 key 一律返 `BIZ_PART_FILE_UPLOAD_FAILED`，真 COS 是 `NoSuchKey`；② **etag 语义**（替身是拼出来的字符串，无条件写 / 部分写、真 COS 的 etag 参与一致性判断）；③ **「CAS 命中跳过 COS PUT」那条省流路径本 binary 完全没跑**（替换语义下 `soft_delete_active_by_part_kind` 先跑、CAS 查询带 `deleted_at IS NULL` ⇒ 取不到，与第 4 条同源） | **有意保留 + 已登记**。2026-10-16 已把本 binary 原来自造的**第三个** COS 替身（`MemoryCos`）删掉、能力上提到 `test-support` 的 `MockCos`（`put` 真存字节 / `delete` 真删字节），三条残余偏差属替身固有限制，不是回归。⚠️ 端点 5 的 COS 清理断言改用 `MockCos::stored_bytes`（「对象确实没了」）而非只看 `delete_calls` |
| 14 | **`GET /print-templates/{id}/content` 的 kind 校验在 Rust 侧而非 SQL 谓词**（`service::get_template_content` 比 `row.kind`）；端点 5 连 kind 都不比，只靠 `soft_delete_file` 的 kind→角色分派 | **有意保留 + 已登记**。理由：这两条路径**先取行**（端点 4 为了判「零外部 IO」、端点 5 为了拿 `object_key` 与派角色），行已在手里，比 `row.kind` 与 SQL 谓词等价，且少一次往返。别把它们「统一」成 SQL kind 谓词 |
| 15 | **行 VO 的 `customer_name` 缺了会让本域对着真前端 100% 不可用**（review 第 2 轮 C-1）：前端 `printTemplateSchema` 是 `.strict()` 且必填它，而首版 `PrintTemplateOut` 没有 ⇒ 上传即便返 **201** 也抛 ZodError、列表每行都抛（恒空）、页面「客户」列恒显「（已删除的客户）」。首版的 42 条用例全绿却钉不到它（断言只看自己写的那几个键） | **已闭合**（行 VO 补 `customer_name: Option<String>`，三条填充路径见 §2.4；新增两条集成测试钉**取值**：命中 / 客户软删后 `null`。⚠️ **列表不许逐行查名** —— 那正是 N+1；语句数与源码护栏见下一条）
| 16 | **列表端点的取数恒 ≤ 3 条语句（rows 1 + total 1 + 客户名 1；`rows` 为空时 2），与模板条数 / 客户数无关** | **有意保留 + 登记**（review 第 2 轮 C-1 随 `customer_name` 一并引入）。客户名走**一条 `WHERE id = ANY($1)`** 一次取回后在内存里 join（`HashMap`），**绝不**在 `rows.iter()` 里逐行查 —— 逐行查的写法**功能完全正确、集成测试照样全绿**，只有语句数会线性涨，所以只能靠 `src/modules/print_template/service.rs` 末尾 `mod query_guard` 的**源码级** lib 单测钉（循环体内出现 `sqlx::query` 即红 + 三条 helper 各恰好一条查询 + 列表端点必须走批量那条）。⚠️ **该护栏的已知漏报形态**：「循环里调别的函数、那个函数里发查询」这类**跨函数的间接 N+1** `no_query_inside_any_loop` 看不见，由同一模块里的 `list_templates_joins_customer_names_in_memory` 兜（它显式禁止列表端点调单条取名的 `customer_name(`）—— 但**新增第四个会发查询的 helper** 时要顺手扩那条断言。两条断言都经变异验证（手改成 N+1 后当场红），登记在 `query_guard` 的模块 doc 里。⚠️ 逐端点的确切语句数（端点 1 = 7 等）写在 `service.rs` 模块 doc 的表里，那里也注明「会过期、要确切数字自己读，别当契约」 |
| 17 | **`files::part_file::service::get_file_content` 已整条删除**（review 第 2 轮 MINOR-A）：它有角色闸门（5 角色）但**无 kind 闸门**，正是第 1 轮 MAJOR-2 关掉的那条绕行面，却以 `pub` 形态留在原地 —— 生产侧早已改走 `get_file_content_of_checked_row`，只剩两个测试在调 | **已闭合**（方法删除，两个测试调用方改成「`get_file_row` 取行 → `get_file_content_of_checked_row`」的生产同款两步）。⚠️ 本域不依赖它（端点 4 走最底层的 `get_file_content_for_row`），所以删除对其它域零影响。**别**因为「顺手」把它加回来 |
| 18 | **`policy::write_roles_for` 的默认集合形参换成 `WriteEntry` 入口枚举**（review 第 2 轮 MINOR-B）：原签名把「这条入口的非 CUSTOMER 角色集合」当**位置参数**传 ⇒ 把 confirm 那处的 `CONFIRM_ROLES` 抄成 `UPLOAD_ROLES` 是**静默的权限面放开**（同文件、同类型、clippy 不报、lib 单测仍全绿） | **有意保留 + 登记**。调用方现在只传「我是哪条入口」（`WriteEntry::Upload` / `WriteEntry::Confirm`），常量由函数内部选 ⇒ **传错常量在类型层面不存在**。⚠️ 代价：新增一条写入口时要加一个变体（那本来就该显眼 —— 新入口必须显式决定角色面） |
