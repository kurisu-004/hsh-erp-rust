# mail_digest 域 API（交期到期汇总邮件）

> **2026-10-13 新建域**。把「已逾期 + 未来 N 天系统交期到期」的**工单级**汇总成一封
> HTML 表格邮件，按配置的频率与时刻定时发出，也支持手动触发与预览。
>
> ## ⚠️ 2026-10-18 URL 硬切：前缀进了 `/cron` 域组
>
> | 旧路径 | 新路径 |
> |---|---|
> | `/api/v2/mail-digest/*` | **`/api/v2/cron/mail-digest/*`** |
>
> **旧路径 404，无 alias**（合仓内 `scan` / `files` / `print_template` / `wx` 一贯做法）。
> 需求出处（用户原话）：「api挂载到 /cron/wecom_daily 下，邮件的API也移动到 /cron 下」。
>
> `/cron` 是**定时任务域组**的统一 API 前缀，其下四个子域：
>
> | 子域 | 职责 | 模块 | URL |
> |---|---|---|---|
> | `mail` | **通道**（邮箱 + SMTP 预设） | `src/modules/scheduled_task/` | `/api/v2/cron/mail/*` |
> | `mail-digest` | **内容**（本文件） | `src/modules/mail_digest/` | `/api/v2/cron/mail-digest/*` |
> | `wecom` | **通道**（企业微信群机器人） | `src/modules/wecom/` | `/api/v2/cron/wecom/*` |
> | `wecom-daily` | **内容**（每日总结推送） | `src/modules/wecom_daily/` | `/api/v2/cron/wecom-daily/*` |
>
> ⚠️ **`wecom` / `wecom-daily` 两行在本轮只是登记**（2026-10-18 建表 + 开错误码段，
> 模块代码随下一批落地）；本文件描述的 6 条端点表逐字有效。
>
> ⚠️ **菜单 path 不跟着 API 前缀变**：菜单 seed 的 `path='/mail'`（本域与 `mail` 域
> 共用同一个二级菜单）与前端路由 `path: 'mail'` **保持不变**，变的只是后端 URL。
>
> 守卫：`tests/mail_digest/config.rs::old_mail_digest_prefix_returns_404`
> （实跑旧前缀断言 404；同用例先钉新前缀 200 作正向对照）。
>
> 代码位置：`src/modules/mail_digest/`（六件套）、后台调度 `src/task/mail_digest.rs`、
> 迁移 `migrations/20261013000000_001_mail_digest.sql`、菜单 `seeds/menu.sql`
> （`scheduled_task` → `mail`，仅 MANAGER，路径 `/mail`；⚠️ **2026-10-16 由
> `mail_group` → `mail_digest`（原路径 `/mail/digest`）改名**，旧行软删）。
>
> ⚠️ **SMTP 凭据走 `scheduled_task` 域的加密邮箱表**（**2026-10-16 整段替换**）。
> 授权码 AES-256-GCM 加密进 `t_mail_account.encrypted_secret`（主密钥 env
> `MAIL_ACCOUNT_SECRET`），本域只存「**选哪个邮箱**」（`t_mail_digest_config.sender_account_id`）
> + 收件人 / 内容口径 / 调度。整域契约见 [`docs/api/mail.md`](mail.md)。
>
>   - **不是明文**（备份 + 任何只读库权限的人都能拿到），也**不是散列** —— bcrypt 取不回
>     明文，而 SMTP 发信必须把授权码交给 lettre ⇒ 只能是**可逆**的对称加密。
>   - `MAIL_SMTP_HOST` / `_PORT` / `_TLS_MODE` / `_USERNAME` / `_PASSWORD` /
>     `MAIL_FROM_ADDRESS` / `_NAME` / `MAIL_DIGEST_ENABLED` **已全部删除、运行时不读**：
>     代码里没有任何 env 兜底路径，也没有启动自检 / 一次性引导导入。host / port / TLS 模式
>     由**预设**决定（网易 163 / QQ，均 465 隐式 TLS，见 `docs/api/mail.md` §3）。
>   - **域外依赖 = 1 个模块**：`scheduled_task`（读 `t_mail_account` 一行 +
>     `infra::crypto::open` 解密）。方向是 **mail_digest → scheduled_task**（内容域依赖
>     通道域），反向不存在。
>
> ⚠️ **未选定可用发信邮箱时后端照常启动**：定时任务打一行 info 后**不注册 job**（判据见
> `docs/api/mail.md` §4.5），手动触发返 `21701`。降级语义与被删除前的 env 方案逐条对齐，
> 只是判据从 env 换成了 DB。
>
> ⚠️ **2026-10-16 第二轮：批次子行 + 位置列 + 状态中文 + 本域自有 SQL**。用户需求（原文）
> 「①状态列发送时要 alias 为中文；②工单下方要紧跟着对应的所有批次；③增加一列位置，
> 显示每个批次所在的位置；④在邮件通知的域重写 SQL，不要复用其它域的，禁止出现 N+1 问题；
> ⑥表格列要对齐，支持打印」（⑤ COS 预览链接 / 批量 CI 本轮不做）。落在 §2.3（白名单
> 11 → 13 项）、§2.4（批次子行逐字段）、§4.4（工单-批次行结构）、§4.5（位置口径）、
> §4.6（列对齐与打印）、§4.7（取数恒 ≤ 3 条语句）、§5（状态中文与三副本同步）、§6（拆掉
> dashboard `DELIVERY_STATUSES` 共用）、§8.1（读 / 写的表清单）、§8.2（前端**零改动**）。

## 1. 端点表

**全部 6 条都是 Manager 独占**（角色守卫写在各 handler 第一行，逐字
`current.require_role(Role::Manager)?`）。前缀 `/api/v2/cron/mail-digest`。

| 方法 | 路径 | 权限 | 一句话 |
|---|---|---|---|
| `GET` | `/api/v2/cron/mail-digest/config` | Manager | 读配置**单例**；表里无行时返回全部默认值 + `version=0` + `id="0"`（**不是 404**） |
| `POST` | `/api/v2/cron/mail-digest/config` | Manager | **全量覆写**；body **必含** `version`（OCC）；成功后热重载 cron（失败不回滚配置） |
| `GET` | `/api/v2/cron/mail-digest/meta` | Manager | **内容与调度**侧的表单选项源：频率枚举 / 列白名单 / 取值范围 / 缺省值（⚠️ **2026-10-16 起不再含 `smtp` 段**） |
| `POST` | `/api/v2/cron/mail-digest/preview` | Manager | 按「当前配置 + 临时覆盖」渲染 HTML；**不发送、不落流水** |
| `POST` | `/api/v2/cron/mail-digest/send` | Manager | 手动触发：渲染 + 真发 + 落流水（**成败都落**） |
| `GET` | `/api/v2/cron/mail-digest/logs` | Manager | 发送历史分页（`sent_at DESC, id DESC`；`limit∈[1,200]` + `offset≥0`） |

- **只用 GET / POST**：本仓 HTTP 方法**零** PUT/PATCH/DELETE，故配置写入走
  `POST /config`（全量覆写）而非 `PUT /config/{id}`。
- **权威路由清单**：`src/modules/mail_digest/handler.rs::ROUTES`（lib 单测逐条比对
  `router()` 源码，加了 `.route()` 忘登记就在单测里当场红）。
- 挂载点：`src/modules/mod.rs::v2_router()` 的 `.nest("/cron/mail-digest", …)`
  （**`/cron` 父前缀下的两段**，2026-10-18 硬切；第 2 段仍 kebab-case 单段词
  `mail-digest`，域名 `mail_digest` 直接连字符化）。
  ⚠️ 与同族的 `.nest("/cron/mail", scheduled_task::router())`（邮箱通道域）**同段数但
  无冲突**：matchit 在**第 2 段**先静态判别（`mail-digest` 与 `mail` 互不为对方的前缀），
  `/cron/mail-digest/config` 与 `/cron/mail/meta` 在第 2 段就分道，不进入第 3 段比较。
  ⚠️ **matchit 注册顺序核对**（`/cron` 前缀族的四条理由，逐条见 `src/modules/mod.rs`
  `v2_router()` 内 `/cron` 段的注释）：① 每个子域**各自一个 `.nest()`**、没有合并
  router ⇒ 两张 matchit 表互不参与；② 第 2 段互不为前缀；③ 段数相同也在第 2 段分道；
  ④ 本域 6 条**全是静态段**（见下一条）。
  菜单也同批从 `mail_group → mail_digest`（`/mail/digest`）改成 `scheduled_task → mail`（`/mail`）。
- ⚠️ **6 条路由全是静态段**（无 `Path` 参数）⇒ 不存在「静态段必须先于 catch-all」的
  matchit 排序约束。后人加 `/{id}` 时要重新审视这一点。

## 2. `MailDigestConfig` 逐字段

### 2.1 `GET/POST /config` 出参（`ConfigOut`）

`id` 是 **JSON string**（雪花 ID 防 JS 精度截断）。

| 字段 | 类型 | 取值 / 来源 | 可空 |
|---|---|---|---|
| `id` | string | `t_mail_digest_config.id`（雪花）；**表空时恒 `"0"`** | 否 |
| `version` | number | OCC 版本；表空时恒 `0`（= 首次初始化提交值）；每次 `POST /config` 自增 1 | 否 |
| `enabled` | bool | 定时任务闸（**2026-10-16 起是唯一的一道 DB 闸**，原先的另一道是 env 的 `MAIL_DIGEST_ENABLED`，已删）。`false` 时不注册 cron job，但 `POST /send` 手动触发**仍可用** | 否 |
| `frequency` | string | `DAILY` / `WEEKLY` / `MONTHLY` / `MANUAL_ONLY`（无 DB ENUM，Rust 枚举校验；解析失败 → `21704`） | 否 |
| `send_hour` | number | `0..23`；**非** `MANUAL_ONLY` 时必填且校验。⚠️ `MANUAL_ONLY` 之下**不清空也不校验**（见 §8.3「实现与注释不符」第 4 条） | 是 |
| `send_minute` | number | `0..59`；同上 | 是 |
| `weekday` | number | `1..7`，**ISO：1=周一 .. 7=周日**；非 `WEEKLY` 恒 `null` | 是 |
| `day_of_month` | number | `1..28`（**刻意上限 28**，避开 2 月末与 30/31 日短月）；非 `MONTHLY` 恒 `null` | 是 |
| `recipients` | string[] | 收件人 To；**发送路径要求非空**（空 → `21704`，见 §8.3「实现与注释不符」第 1 条）。⚠️ **配置页允许存空数组**（编辑中途的正常形态） | 否（数组本身） |
| `cc_recipients` | string[] | 抄送 Cc；可空数组 | 否（数组本身） |
| `include_overdue` | bool | 是否**渲染**「已逾期」节；关掉时正文里**完全不出现**该节（2026-10-13 M5）。与 `include_upcoming` **不能同时 false**（→ `21704`） | 否 |
| `include_upcoming` | bool | 是否**渲染**「未来 N 天」节；关掉时**完全不出现**该节（2026-10-13 M5） | 否 |
| `upcoming_days` | number | 未来节窗口天数，**clamp 到 `1..60`**（不是拒绝），缺省 `14` | 否 |
| `visible_columns` | string[] | 正文表格列白名单，**封闭枚举**（§2.3）；未知项 → `21704`（**不静默丢弃**）；空数组在渲染层退回缺省 10 列 | 否 |
| `subject_template` | string | 主题模板；`null` = 用缺省主题。`{date}` / `{days}` 两个占位符，未知 `{xxx}` 原样保留 | 是 |
| `sender_account_id` | string | 发信邮箱（`t_mail_account.id`，雪花 **JSON string**）；`null` = 未选。`POST /config` 传不存在的/已软删的 id ⇒ `21706` | 是 |
| `smtp_configured` | bool | **2026-10-16 语义改写**：原先是「env 侧是否具备发信能力」（`MailConfig::is_enabled()`，env 已删），现为「`sender_account_id` 是否指向一个**存在且未软删**的邮箱」。前端据此灰掉「立即发送」。⚠️ **不**试解密（见 `docs/api/mail.md` §8.3 第 7 条） | 否 |
| `created_at` | string | `YYYY-MM-DD HH:MM:SS`（Asia/Shanghai naive）；**表空时是空串**（不是会跳动的假时间） | 否（可为空串） |
| `updated_at` | string | 同上 | 否（可为空串） |

### 2.2 `POST /config` 入参（`SaveConfigRequest`，全量覆写）

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `version` | number | **必填** | OCC 锚点；首次初始化传 `0`。**缺失 → HTTP 422 纯文本**（axum `Json` 提取器，不是业务信封） |
| `enabled` | bool | 否（缺省 `false`） | 同出参 |
| `frequency` | string | **必填** | 4 个预设之一 |
| `send_hour` / `send_minute` | number | 否 | 非 `MANUAL_ONLY` 时**必须给全**，缺一即 `21704` |
| `weekday` | number | 否 | `WEEKLY` 必填，否则 `21704`；其余频率**被清空**（见 §8.3「实现与注释不符」第 2 条） |
| `day_of_month` | number | 否 | `MONTHLY` 必填，否则 `21704`；其余频率被清空 |
| `recipients` | string[] | **必填** | 逐项校验：拒 CR/LF（`21703`）、缺 `@`、含空白/分隔符/尖括号、域名形态。⚠️ **允许空数组**（判非空只在发送路径，见 §8.3「实现与注释不符」第 1 条） |
| `cc_recipients` | string[] | 否（缺省 `[]`） | 同上校验 |
| `include_overdue` / `include_upcoming` | bool | 否（缺省 `true`） | 同时 false → `21704` |
| `upcoming_days` | number | 否（缺省 `14`） | clamp `1..60` |
| `visible_columns` | string[] | **必填** | §2.3 |
| `subject_template` | string | 否 | ≤200 **字**（`.chars().count()`，不是字节数）+ 拒 CR/LF |
| `sender_account_id` | string | 否（缺省 / `null` = 未选） | **JSON string**（雪花防精度截断；`deserialize_i64_opt` 只收字符串或 `null`，**发数字会 422**）。指向不存在**或已软删**的邮箱 ⇒ `21706` + 404。⚠️ `POST /config` 是**全量覆写**，不带这个键 = 把它清成 `null` |

**响应回填的是提交后的新 `version`**（读回最新行），前端据此做下一次提交 ——
不要拿「提交前」的 version 复用，否则立刻撞一次假的 OCC 冲突。

### 2.3 列白名单（`visible_columns`，封闭枚举，**13 项**）

⚠️ **顺序 = 展示顺序 = 缺省顺序**（`GET /meta` 的 `columns[]` 按此顺序下发，前端逐
`meta.columns[].key` 渲勾选框即可，不要在前端另存一份列名映射）。

| key | 中文表头 | 来源列 | 只在批次子行有值 |
|---|---|---|---|
| `serial_no` | 序列号 | `t_part.serial_no` / `t_assembly.serial_no`（可空 ⇒ 空串） | — |
| `drawing_no` | 图号 | 同名列 | — |
| `name` | 名称 | 同名列 | — |
| `batch_no` | 批次 | `t_part_batch.batch_no`，渲染成 `　第 N 批`（**2026-10-16 新增**） | ✅ |
| `quantity` | 数量 | `t_part.quantity` / `t_assembly.quantity`；**装配件行单位是「套」**，渲染成 `12 套`。批次子行渲批次自身件数、**不带「套」后缀** | — |
| `customer_name` | 客户 | `LEFT JOIN t_customer.name`，`ON` 里**刻意不加**软删闸门 ⇒ **软删客户名字照常带出**；只有客户行不存在（`customer_id` 悬空）才为空（取舍见 §8.1） | — |
| `status` | 状态 | 工单行 = `t_part/t_assembly.status` 查表（§5）；批次子行 = `t_part_batch.status` 查表，`is_repairing = true` 时恒「返修中」 | — |
| `location` | 位置 | 批次所在的 `t_part_batch.location` + 多态 holder 名（`t_shelf.code` / `t_worker.name` / `t_outsource_company.name`），文案见 §4.5。解析不出时 `—`（**2026-10-16 新增**） | ✅ |
| `system_delivery_date` | 系统交期 | `date`，格式 `YYYY-MM-DD` | — |
| `remaining_days` | 剩余天数 | `(交期 − today)` 天数；今天到期 = `0`，逾期为负 | — |
| `row_type` | 工单类型 | 渲染为「装配件」/「零件」 | — |
| `is_urgent` | 是否紧急 | 渲染为「是」/「否」 | — |
| `order_no` | 订单号 | `varchar(30)`，可空 | — |

**缺省集** = `ALL` 中**除 `row_type` / `is_urgent` / `order_no` 之外的全部**。⚠️ 刻意
**按名**描述而非「`ALL` 的前 N 个」：`DEFAULT_SET` 是显式列表、不依赖枚举顺序，往 `ALL`
中段插一个新变体只会让「前 N 个」那句措辞变陈旧、不会改变缺省集本身。个数另有机器钉住
（`tests/mail_digest/preview.rs` 的 `Some(10)`、`template.rs` 的 `ALL.len() == 13`），
措辞不钉数字。

⚠️ 这条缺省集是**四份副本**（2026-10-16 复核更正：原先写「三份」且第三份指错了对象），
改一处必须同步其余三处：

| # | 副本 | 同步方式 |
|---|---|---|
| ① | `dto.rs::VisibleColumn::DEFAULT_SET`（Rust 常量） | **真源** |
| ② | 迁移 `migrations/20261016000001_001_mail_digest_default_columns.sql` 的列 DEFAULT 字面量 | 测试逐字对拍 ① |
| ③ | `tests/mail_digest/main.rs::DEFAULT_COLUMNS`（fixture 的裸列表，不带 PG 数组花括号） | 测试逐字对拍 ① |
| ④ | `tests/mail_digest/config.rs` 的缺省断言（`GET /config` 缺省响应的 `visible_columns`） | 自身断言对拍运行时缺省 |

⇒ ②③ 由 `default_column_set_stays_in_sync_across_its_three_copies` 覆盖（它把 ② 从迁移
文件里解析出来与 ①③ 当场比对；测试名里的「三条」指的就是这三份，**不含 ④**）；④ 不参与
那次对拍，只由自己那条 `assert_eq!` 守「缺省响应 == ①」。**四条链最终都锚在 ①**。

⚠️ **按「文件」数是 4 份、按「字面量出现次数」数是 5 处**：② 那一份在同一条迁移里出现
**两次**（`SET DEFAULT` 一处 + 回填存量行的 `UPDATE` 一处）—— 将来若有人去改那条迁移
（它 append-only、本不该被改），改一处等于只改了一半。

⚠️ **勾掉 `batch_no` / `location` 整列就没有了**：列白名单是**逐列**的，管理员勾掉
`location` 时批次的位置信息在正文里没有任何出口。这是列白名单的固有行为，不是缺陷。

### 2.4 批次子行（`DigestBatchRow`，2026-10-16 新增）

正文里的行有两种：**工单行**（`DigestRow`，§2.3 上半张表）与**紧跟其下**的**批次子行**
（本节）。行单位仍是**工单级** —— 子行不计入 `overdue_count` / `upcoming_count`，也不
改变任何统计口径。

| 字段 | 类型 | 取值 / 来源 |
|---|---|---|
| `batch_no` | number | `t_part_batch.batch_no`（工单内序号，1 起） |
| `quantity` | number | `t_part_batch.quantity`（**件数**，不带「套」） |
| `status` | string | `t_part_batch.status`（`PartStatus` 字面量）⇒ 渲染时查表转中文；`is_repairing = true` ⇒ 恒「返修中」 |
| `is_repairing` | bool | `t_part_batch.is_repairing`（`REPAIRING` 状态码于 2026-10-01 删除后由该布尔承载） |
| `location` | string \| null | `t_part_batch.location`，**裸 varchar 无枚举约束**；5 个取值 = `OFFICE` / `PRODUCTION_SHELF` / `WORKER` / `INSPECTION_SHELF` / `OUTSOURCE_COMPANY` |
| `holder_name` | string \| null | 按 `location` 分桶解析：两种货架档取 `t_shelf.code`（**不是 `name`**）、`WORKER` 取 `t_worker.name`、`OUTSOURCE_COMPANY` 取 `t_outsource_company.name` |
| `owner_type` / `owner_id` | string / string | `PART` + `t_part.id` 或 `ASSEMBLY` + `t_assembly.id`；Rust 侧按这个二元组把批次装回工单行 |

⚠️ **`is_repairing` 在工单行上不存在**：`t_part` / `t_assembly` 上没有这一列 ⇒ 一行
返修中的件，**工单行显示「生产中」而它下面的批次子行显示「返修中」**。这是表结构事实，
不是漏实现。

⚠️ **装配件的批次挂在子件上**：`t_part_batch.part_id → t_part.id`，子件的
`t_part.assembly_id` 指向装配件 ⇒ 子件本身不单独成行，它的批次归到装配件行下面。

⚠️ **本结构不出现在任何 HTTP 出参里**：`/preview` 与 `/send` 返的仍是 §2.5 的
`{ subject, html, overdue_count, upcoming_count, columns }`，批次信息**只存在于
`html` 字符串内部**。前端不需要（也不能）按结构消费它。

### 2.5 `POST /preview` 与 `POST /send` 入参（`PreviewRequest`）

**不含 `version`**（这两个端点不改配置，OCC 对它们没有意义），且**全部字段可选** ——
空体 `{}` = 完全用当前配置。字段与 §2.2 同名同型（`Option<T>` 语义：`null` = 沿用当前值）。
合并后**重新跑一遍校验**（覆盖值可能让原本合法的配置变非法，如 `upcoming_days = -5`）。

出参：

| 端点 | 出参 | 说明 |
|---|---|---|
| `/preview` | `{ subject, html, overdue_count, upcoming_count, columns }` | `html` 是**完整 HTML 文档**（前端直接塞进 iframe / `v-html` 预览）；`columns` 是**实际参与渲染**的列 |
| `/send` | `SendResultOut` | 见 §2.6 |

### 2.6 `SendResultOut`（`POST /send` 出参 = 流水行视图）

| 字段 | 类型 | 说明 |
|---|---|---|
| `log_id` | string | `t_mail_digest_log.id`（雪花） |
| `config_version` | number | **发送当时**的配置版本（快照，不追认后续改动） |
| `trigger_source` | string | `SCHEDULED`（cron）/ `MANUAL`（本端点恒 `MANUAL`） |
| `status` | string | `SUCCESS` / `FAILED` |
| `subject` | string | 本次主题快照。⚠️ **`SCHEDULED` 的「准备阶段失败」行为空串**（压根没渲染，见下方说明） |
| `to_recipients` / `cc_recipients` | string[] | **快照**（不 join 配置表） |
| `overdue_count` / `upcoming_count` | number | 两节行数（工单级）；准备阶段失败恒 `0` |
| `html_bytes` | number | 正文字节数（UTF-8）；正文本体不入库，复现走 `/preview`。准备阶段失败恒 `0` |
| `sent_at` | string | `YYYY-MM-DD HH:MM:SS` |
| `error_message` | string | 失败原因（**截断到 500 字符**）；成功时 `null` |

⚠️ **关掉的节，那个 count 恒 `0`，而这个 `0` 不表示「没有该类工单」**（2026-10-13
review 第 2 轮 #2）：`include_overdue=false` 时 service 层**根本不查**逾期工单（省一次
SQL），行数因此恒 0。事后拿流水对数时**不能**把「逾期 0 条」读成「那天没有逾期工单」——
它只说明「这次没统计逾期」。同一份 0 与 `html_bytes` 的口径差异是两回事（后者是渲染
结果，前者是查询开关）。⇒ 流水表的两个 count **保持 0 是刻意的**，改它就得让 service
为关掉的节也去查一次库（把「省一次 SQL」这个设计意图反掉）。**正文里则连这个 0 都不
出现**：概览行只列**已启用**的两节（§4.8）——两个出口给同一件事两种口径，正是 M5 与
本条要消除的那类误读。回归：`template.rs::tests::overview_line_lists_only_enabled_sections`
+ `preview.rs::preview_section_toggles_hide_each_section`。

`GET /logs` 出参 = `{ items: SendResultOut[], total, limit, offset }`。
`total` 是匹配总数，**不受** `limit` 截断影响。

#### ⚠️ `SCHEDULED` 的**准备阶段失败也落流水**（2026-10-13 review 第 1 轮 M8 修复）

`service::send_scheduled` 分两段：**准备**（取连接 → 读配置并规范化 → 判收件人非空）与
**投递**（渲染 → 调 lettre → 落流水）。修复前准备段失败直接 `return`，`deliver` 根本没被
调用 ⇒ `t_mail_digest_log` 里**查不到任何东西**，「定时任务每天都在失败」只剩一行
`tracing::warn!`——而容器日志恰恰是最容易被轮转掉、最难按时间线回看的那一份。

现在准备段的两种失败也会补一行 `status='FAILED'`（`trigger_source='SCHEDULED'`）：

| 准备阶段失败 | `subject` / `html_bytes` / 两个 count | `to_recipients` 快照 |
|---|---|---|
| 配置解析失败（脏 `frequency` / 脏 `visible_columns`） | 空串 / `0` / `0` | **尽力**从原始行捞；读不到记空串 |
| 收件人为空 | 空串 / `0` / `0` | 从已解析配置取（即空串） |

⚠️ **唯一落不了的失败**是 `pool.acquire()` 本身——没有连接就没有地方写，此时只有
`tracing::warn!`。**手动路径（`MANUAL`）的行为不变**：`/send` 的失败以 HTTP 错误码返回给
调用方，且 21701 / 21703 / 21704 三条**校验/前置失败不落流水**（回归用例
`send_without_smtp_config_returns_21701_and_writes_no_log` 等三条钉住）——有调用方看得见
就不必在流水里重复。回归用例
`tests/mail_digest/send.rs::scheduled_prepare_failure_still_writes_failed_log`。

### 2.7 `GET /meta` 出参（⚠️ 2026-10-16 删掉 `smtp` 段）

| 字段 | 类型 | 说明 |
|---|---|---|
| `frequencies[]` | `{value, label, needs_time, needs_weekday, needs_day_of_month}` | 4 个预设；三个 `needs_*` 驱动表单的显示/隐藏（`MANUAL_ONLY.needs_time = false`） |
| `columns[]` | `{key, label}` | §2.3 白名单，**13 项**（⚠️ 2026-10-16 由 11 项加 `batch_no` / `location`；顺序 = 展示顺序 = 缺省顺序） |
| `defaults` | `ConfigDefaultsOut` | 缺省配置（**不含** `id` / `version` / 时间戳 / `smtp_configured`；**含** `sender_account_id`，缺省恒 `null`） |
| `upcoming_days_range` | `{min: 1, max: 60}` | 前端数值控件边界 |

**已删除的 `smtp` 段**原先带的是 env 侧 SMTP 公开信息，而那些值在「邮箱改为页面内配置」
之后**逐条换了归属**：

| 原 `smtp.*` | 新的去处 |
|---|---|
| `configured` | `ConfigOut.smtp_configured`（`GET /config`，纯 DB 判定） |
| `host` / `port` / `tls_mode` | `GET /api/v2/cron/mail/meta` 的 `presets[]`（`scheduled_task` 域，见 `docs/api/mail.md` §3） |
| `from_address` | `GET /api/v2/cron/mail/accounts` 的 `account` |
| `from_name` | `GET /api/v2/cron/mail/accounts` 的 `name` |

⚠️ 保留一个「已经没有任何生产方」的字段只会让前端照着它读一个恒空的对象。前端改造时
按上表逐条改，**不要**保留对 `smtp` 段的引用。

## 3. 频率枚举与 cron 映射

| `frequency` | 缺省 cron（6 段，**带秒**） | 需要的配套字段 |
|---|---|---|
| `DAILY` | `0 {send_minute} {send_hour} * * *` | `send_hour` + `send_minute` |
| `WEEKLY` | `0 {send_minute} {send_hour} * * {weekday - 1}` | 上述 + `weekday` |
| `MONTHLY` | `0 {send_minute} {send_hour} {day_of_month} * *` | 上述 + `day_of_month` |
| `MANUAL_ONLY` | **不产生 cron**（定时任务不注册 job） | 无（时刻字段**既不清空也不校验**，见 §2.1 与 §8.3「实现与注释不符」第 4 条） |

- **cron 是 6 段不是 5 段**：tokio-cron-scheduler 0.15 内部用 `croner`，解析器固定
  `Seconds::Required` + `dom_and_dow(true)`。写成常见的五段 `分 时 日 月 周` 会在
  `Job::new_async_tz` 里直接 `ParseSchedule` 失败（好在那是同步返回的 `Err`，
  不会等到「到点了没触发」才发现）。
- **`WEEKLY` 必须减 1**：本域 `weekday` 用 ISO（`1`=周一），cron 第 6 段是
  `0`=周日。漏减不会报错，只会把邮件**安静地挪到隔一天**（管理员选了周一、邮件每周二到，
  看日志也看不出问题）。lib 单测 `schedule::tests::weekly_cron_subtracts_one` 钉死两端
  （ISO 1 ⇒ cron 0、ISO 7 ⇒ cron 6）。
- **时区显式传 `Asia/Shanghai`**，不依赖 `TZ` 环境变量 ⇒ 容器 / 开发机 / CI 三处行为一致。

## 4. 口径表

### 4.1 与 dashboard 域**共用**的口径（谓词逐字同源，不要改）

| 判据 | 说明 |
|---|---|
| 「未交付」6 态集合 | `PENDING` / `PROGRAMMING` / `IN_PROCESS` / `OUTSOURCE` / `INSPECTION` / `READY_TO_SHIP`。⚠️ **2026-10-16 起是本域自有常量** `repo::DELIVERY_STATUSES`（原先 `use` dashboard 那份），见 §5 |
| **行单位 = 工单级** | `t_part` 侧带 `AND p.assembly_id IS NULL`（子件不重复计入），装配件单独查 `t_assembly` 且其行**替换**子件行 |
| **已交量守卫** | `NOT EXISTS (… FROM t_part_batch b WHERE b.part_id = … AND b.deleted_at IS NULL AND b.status IN ('DELIVERED','COMPLETED'))` |
| 软删闸门 | `p/a.deleted_at IS NULL` |

### 4.2 与 dashboard 域**刻意不同**的部分（需求决定的，不是漂移）

| | dashboard | mail_digest |
|---|---|---|
| 每桶行数上限 | `LIMIT 30`（`DELIVERY_BUCKET_LIMIT`，大屏一屏放不下） | **无 LIMIT**（邮件必须给全量 —— 漏一条交期就是漏一个客户） |
| 取哪些列 | 原样 `p.customer_id`（客户名另有批量聚合） | `customer_id` 换成同一条 SQL 里 `LEFT JOIN` 来的 `customer_name`，**另多取** `drawing_no` / `order_no`（邮件要图号 / 订单号） |
| 未来节窗口 | `>= today`，**无上界** | **`[today, today+N)` 半开区间**，`N = upcoming_days` clamp `1..60` |
| 逾期节窗口 | （KPI 另算） | `system_delivery_date < today` —— **含今天之前、不含今天**（今天的交期属未来节的「今日」组） |
| 客户名 | 单独批量聚合（防 N+1） | 本 SQL 里 `LEFT JOIN` 一次带出 |
| 行单位 | 交期面板**工单级** / 柱状图与抽屉**件级** | **一律工单级**（本域不跟柱状图的件级） |

⇒ 故**不复用** `DeliveryRepo::list_orders_*`：复用就得先解掉它的 `LIMIT 30`
（等于改坏大屏）或在外面再分页（等于让 SQL 口径与 Rust 侧重叠，两边都要维护）。
⚠️ 2026-10-16 需求进一步要求「**在邮件通知的域重写 SQL，不要复用其它域的**」⇒ 连
`DELIVERY_STATUSES` 常量也不再 import，统计 SQL 现在对本域外**零符号依赖**。

### 4.3 四条硬约束（改 SQL 前逐条读）

1. **工单级行单位**：`t_part` 侧必须带 `AND p.assembly_id IS NULL`。
2. **`NOT EXISTS` 必须同时判 `b.deleted_at IS NULL` 且状态集含 `'COMPLETED'`**：
   force-complete 端点（`POST /prod/parts/{id}/force-complete` 与
   `POST /prod/assemblies/{id}/force-complete`）把批次强推 `COMPLETED`，守卫漏掉它
   会让**已完工**的工单继续出现在「逾期未交」邮件里。
3. **窗口下界来自形参 `today`，SQL 里绝不写 `CURRENT_DATE`**：DB 会话时区（测试容器
   UTC）与 `infra::clock::now_naive()` 的 Asia/Shanghai 是**两个时钟**，Shanghai
   00:00–08:00 期间 `CURRENT_DATE == today - 1`，于是「今天到期」的工单会落进逾期节、
   未来节整体偏移一天。
4. **统计取数恒 ≤ 3 条语句**（2026-10-16）：见 §4.7。

### 4.4 工单行 / 批次行的结构（2026-10-16 新增）

需求原文：「工单下方要紧跟着对应的**所有批次**」「增加一列位置，显示**每个批次**所在的
位置」⇒ 正文里一个工单下面挂若干**批次子行**，逐行展示。

- **子行紧跟工单行**，不是集中排在表尾：渲染时对每个工单行先打它自己、再打它的批次
  （`template::push_row_with_batches`）。若按工单渲完再统一渲批次，读者无从对应。
- **子行只填 4 列**：`批次` / `数量` / `状态` / `位置`。其余（序列号 / 图号 / 名称 /
  客户 / 交期 / 剩余天数 / 工单类型 / 是否紧急 / 订单号）**留空** —— 那些是工单级属性，
  填在子行上会让人以为这批件的交期与工单不同。
- **一个批次都没有的工单不渲子行**（子行是「这个工单名下有批次」的证据，空子行是噪声）。
- **行单位仍是工单级**：子行不计入 `overdue_count` / `upcoming_count`，也不参与任何
  统计（节的「N 条」是工单数）。
- **批次过滤**：**读全部批次、不按状态过滤** —— 用户要的是「所有批次」。⚠️ 与**工单行**的
  已交量守卫方向相反：守卫是 `NOT EXISTS（任一批次 `status IN ('DELIVERED','COMPLETED')`）`
  ⇒ **一件交过，整个工单行就不出现**（哪怕它还有没交的批次，那几批自然也不出现）；
  所以工单行一旦出现，它名下**不可能**有 `DELIVERED` / `COMPLETED` 批次，「不过滤状态」
  实际只对 `CANCELLED` 这类**非交割终态**批次有意义（照列）。
  ⚠️ **别把本条读成「已交的那几批也会跟在工单行下面列出来」—— 那个状态不可达**。
  回归：`tests/mail_digest/batches.rs::mixed_delivered_and_open_batches_follow_the_opposite_two_rules`。
- **批次序**：`ORDER BY owner_type, owner_id, batch_no`，Rust 侧分组时按到达顺序
  push，渲染层**不**二次排序（与工单行同一约定）。

### 4.5 位置列的口径（2026-10-16 新增）

`location` 是**裸 `varchar`（无 DB 枚举、无 Rust 枚举类型）**，5 个取值；holder 是
**多态**的（批次表上**没有** `worker_id` / `outsource_company_id` / `shelf_id` 列，
只有 `current_holder_id` + `location` 两个字段）。

| `location` | holder 取自 | 渲染文案 |
|---|---|---|
| `PRODUCTION_SHELF` | `t_shelf.code` | `货架 {code}` |
| `INSPECTION_SHELF` | `t_shelf.code` | `品检 {code}` |
| `WORKER` | `t_worker.name` | `{姓名}`（**不加前缀**，与其余三档不同，是前端既有口径） |
| `OUTSOURCE_COMPANY` | `t_outsource_company.name` | `外协 {公司名}` |
| `OFFICE` / `NULL` / 未知值 / holder 行已软删 | — | `—` |

- **三张 holder 表全部 `LEFT JOIN` + `deleted_at IS NULL` 放在 `ON` 条件里**（不是
  `WHERE`）：软删的货架 / 工人 / 外协厂**不该出现在邮件里**，而加了软删闸门之后
  `LEFT JOIN` 仍保留批次行、只是名字解析不出来（位置退化成 `—`）。写进 `WHERE` 会
  把整批批次行滤掉 —— 那比「位置空着」严重得多。
- ⚠️ **货架取 `code` 不是 `name`**（`t_shelf` 两列都在，既有口径用 `code`）。
- ⚠️ **终态批次的位置恒为 `—`**：迁移 `20261013000002_001_clear_terminal_batch_location.sql`
  把 `DELIVERED` / `COMPLETED` / `CANCELLED` 三态的 `location` 与 `current_holder_id`
  一起清了（出厂时工件已不在任何货架 / 工人 / 外协厂手里）。这不是取数漏了，是数据本来
  就没有。
- ⚠️ **`location` 没有枚举约束** ⇒ 将来多一个取值时的表现是「位置列显示 `—`」，不是
  「批次消失」（`CASE` 无匹配分支 ⇒ `NULL`）。

### 4.6 列对齐与打印（2026-10-16 新增，需求「表格列要对齐，支持打印」）

- **列对齐**：每张子表带 `<colgroup>`，逐 `<col width="N%" style="width:N%">`，
  外加 `table-layout:fixed`。宽度由 `VisibleColumn::width_weight` 的**权重**按
  「本次实际渲染的列集合」归一化后取整，**最后一列吸收余数** ⇒ **合计恒 100%**。
  ⚠️ 直接用权重原值会凑不出 100%（只勾 4 列时合计 41%）⇒ 表格整体塌掉、`fixed` 布局
  退回自适应，等于白做。
- **`<colgroup>` 每张子表各一份**：分组渲染会开多张子表，`<colgroup>` 是表内元素、
  不跨表共享。
- ⚠️ **表头每组一张、且与 `<colgroup>` 在同一张 `<table>` 里**（2026-10-16）：`<colgroup>`
  只对它**所在那张表**的列宽生效；表头若留在外层表里，那行 `<th>` 就由另一套布局算法定宽 ⇒
  「横向扫行对得上」在表头这一行不成立，需求⑥要的列对齐只剩数据行那几行。所以表头
  **每组重复一次是故意**的、**不是**冗余，别看到重复就「优化」回节级一行。回归：
  `template.rs::tests::header_row_shares_table_with_colgroup`。
- **不写 `@media print`**：邮件客户端（企业邮 / QQ 邮箱）会把 `<style>` 块**整块剥掉**，
  写进去等于什么都没写 ⇒ 全部样式**内联**。
- **底色在打印路径里会丢**：`background-color` 遇到部分打印路径 / Outlook 桌面版会被
  丢弃，故所有有底色的单元格额外内联 `print-color-adjust:exact;
  -webkit-print-color-adjust:exact`（两个前缀都写：旧 WebKit 只认 `-webkit-`）。
- **批次子行的区分不只靠颜色**：淡灰底（`bgcolor` 属性 + `background-color` 两处都写）
  之外，还有**「第 N 批」文案**与**首格全角空格缩进** —— 打印时底色整片丢掉，子行与
  工单行仍分得开。
- **不用 `float` / `position`**：两者在邮件与打印路径里都会产生不可预期的漂移。
- 组标题行的 `colspan` 取**真实列数**（13 列全开时是 `colspan="13"`，不是 `"*"`）。
- `bgcolor` 的**加急行**（`#fdf6ec`）与**批次子行**（`#f7f8fa`）是两套底色，语义不同。

### 4.7 取数恒 ≤ 3 条语句（2026-10-16 新增，需求「禁止出现 N+1 问题」）

一封信的取数语句数**恒 ≤ 3 条**，与工单数 / 批次数**无关**：

| # | 语句 | 形态 |
|---|---|---|
| 1 | `repo::list_overdue` | 字面量宏 `query_as!`，按 `system_delivery_date < today` 取工单 |
| 2 | `repo::list_upcoming` | 字面量宏 `query_as!`，按 `[today, today+N)` 取工单 |
| 3 | `repo::list_batches` | 字面量宏 `query_as!`，`part_ids` / `assembly_ids` 两个数组 `= ANY($1::bigint[])` / `= ANY($2::bigint[])`，一次拉回**全部**批次 |

- ⚠️ **「恒定 2 条」是错的说法**：节开关拦的是**「不能同时关」**（`21704`），**不是**
  「只能开一个」—— `include_overdue` / `include_upcoming` **默认同时为 true**（`config_body()`
  与迁移 DEFAULT 都是两节都开），于是**缺省形态就是序号 1/2/3 三条全发**。
  ⇒ **上界 3 条**：两节都开且查到工单时**恰好 3 条**；关掉一节 2 条；序号 3 在
  **两侧 id 都为空**（没查到工单）时不发。**恒定的是「与行数无关」，不是「条数是 2」**。
- **批次查询是「一条语句、两个 UNION ALL 分支」**：PART 支 `t_part_batch ⋈ t_part`，
  ASSEMBLY 支 `t_part_batch ⋈ t_part c ⋈ t_assembly a`（装配件的批次挂在**子件**上，
  该支必须两跳）。**holder 三向解析在同一条 SQL 里完成** —— 三张 holder 表都是
  `LEFT JOIN`，`ON` 条件里用 `location` 分桶（`b.location = 'WORKER'` 等）限定谁参与匹配，
  `CASE b.location` 取对应那一列的 `name/code`。⇒ 位置文案不需要第二次查询、不需要在
  Rust 侧再解多态 holder。
- **两侧 id 数组可以有一个是空数组**：`= ANY('{}')` 恒 false，那一支贡献 0 行、另一半
  照常工作。**这是常态不是边界情况**（绝大多数工单不是装配件；「两节都关」则两侧都空）。
- **Rust 侧只做内存分组**：`service::fetch_rows` 先查主行 → 收集两组 id → 跑**一次**
  批次查询 → 按 `(owner_type, owner_id)` 分组装进 `DigestRow::batches`。
  ⚠️ **循环体内不得调 `repo::`** —— 这条由 `tests/mail_digest/n_plus_one_guard.rs` 的
  源码级护栏钉住（扫 `fetch_rows` 的 `for` / `while` / `loop` 体，命中 `repo::` /
  `sqlx::query` 即红，并另有一条钉住顶层恰好 3 个 `repo::` 调用点）。逐工单循环查批次
  是**功能完全正确**的写法（集成测试照样全绿），只有语句数会随工单数线性涨，所以只能靠
  源码护栏守。
- **不因为「先查后过滤」而省语句**：工单行的已交量守卫在 SQL 里做（`NOT EXISTS`），
  不是查回来再过滤 —— 否则会多一轮往返。

### 4.8 渲染口径

- **主题**：缺省 `交期汇总 YYYY-MM-DD（未来N天）`；`subject_template` 支持
  `{date}` / `{days}`，未知 `{xxx}` 原样保留。主题里的 CR/LF 在写侧就拒（`21704`）。
- **正文**：全**行内样式**、无 `<style>` 块 / 无外链 / 无图片（企业邮与 QQ 邮箱默认拦
  远程内容），表格布局（不吃 flex/grid），关键底色 `bgcolor` 与 `background-color` 两处都写。
- **XSS**：图号 / 名称 / 客户名 / 订单号 / 序列号 / 批次号 / 工人姓名 / 外协公司名
  **每一个插值点**都过 `html_escape`（`& < > " '` 五个字符，`&` 必须**第一个**替换，
  否则 `&lt;` 会被二次转义成 `&amp;lt;`）。
- ⚠️ **状态列输出中文，不输出英文原字面量**（2026-10-16 新增，需求「状态列发送时要
  alias 为中文」）：工单行查 `t_part` / `t_assembly.status`，批次子行查
  `t_part_batch.status`。文案表见 §5。**未知值原样回显**（不静默变空串 —— 静默空串会让
  「这批件状态坏了」在邮件里完全看不出来）。回归：
  `template.rs::tests::status_column_is_chinese_and_unknown_falls_back_to_literal`。
- ⚠️ **节开关真的作用于正文（2026-10-13 review 第 1 轮 M5 修复）**：`include_overdue=false`
  时正文里**完全不出现**「已逾期」那一节（标题行 + 空态文案一并消失），未来节同理。
  修复前 `render()` 无条件渲染两节、关掉的节因为没查库而拿到空切片，于是信里照样出现
  「已逾期（系统交期早于今日）（0 条）/ 本节无符合条件的工单」——管理员关掉逾期节却在信里
  看到一条空节，第一反应是「统计口径是不是坏了」，这是会直接变成工单的问题。
   回归用例：`tests/mail_digest/preview.rs::preview_section_toggles_hide_each_section`
   + `template.rs` 的 lib 单测 `closed_section_is_not_rendered_at_all` /
   `closed_upcoming_section_is_not_rendered_at_all`。
- ⚠️ **概览行只列已启用的节（2026-10-13 review 第 2 轮 #2 修复）**：M5 把空节修掉之后，
   概览行仍无条件打「逾期 N 条 / 未来 M 条」，而关掉的节 service 层不查库 ⇒ N 恒 0 ⇒
  管理员关掉逾期节，信的第一行还是「…逾期 **0 条** / 未来 12 条」。那 0 与 M5 修掉的
  空节是**同一种误读**（看起来像统计口径坏了），只是从整节缩到了一个数字。现在关掉的节
   在概览行**整段不出现**；两节都关（配置层已被 `21704` 拒）时概览行给一句
   「（本邮件未启用任何内容节）」。
  ⚠️ 「未来窗口 = N 天」是**统计口径**、恒出现，与未来节开关**无关** —— 别把它与
   「未来 N 条」混为一谈。回归：`template.rs::tests::overview_line_lists_only_enabled_sections`。
- **空态**：至少渲染了一节时，空态由**那一节自己**的「本节无符合条件的工单」承担；**不**
  再补一句全局的「无符合条件的工单」（修复前两节都开着且都无行时，正文里会连续出现
  5 段同义文案，2026-10-13 m2）。单节无行 ⇒ 「本节无符合条件的工单」，**不是**空表格
  （无数据时连表头都不渲染）。两节都关在配置层已被 `21704` 拒掉，渲染层对纯函数调用仍
  有一句「本邮件未启用任何内容节」的兜底。
- **按 `system_delivery_date` 分组**：组标题带「已逾期 N 天」/「今天到期」/「明天到期」
   /「剩余 N 天」。SQL 已按 `(交期 ASC, id ASC)` 排好，渲染层**不**二次排序（避免口径分叉）。
   组标题的 `colspan` 取**真实列数**（2026-10-13 m1 修复：原为字面量 `"*"`，而 HTML 的
   `colspan="*"` 非法、客户端一律回落到 1，标题行于是只盖住第一列）。**白名单全开时是
   `colspan="13"`**。
- **批次子行的渲染**：结构见 §4.4、位置文案见 §4.5、列宽与打印见 §4.6。回归：
  `template.rs::tests::batch_sub_rows_follow_their_own_order_row` /
  `batch_sub_row_fills_only_four_columns` / `print_attributes_present_and_no_style_block` /
  `colgroup_widths_sum_to_one_hundred` / `colspan_matches_column_count_with_all_columns` /
  `header_row_shares_table_with_colgroup`。

## 5. 状态域约定（无编译期保障）

### 5.1 「未交付」6 态（筛选谓词用）

- 后端常量：⚠️ **2026-10-16 起是本域自有常量** `mail_digest::repo::DELIVERY_STATUSES`
  （原先 `use dashboard::repo::delivery::DELIVERY_STATUSES`）。6 态字面量与 dashboard
  逐字相同，但**不再共用同一份符号** —— 用户要求「在邮件通知的域重写 SQL，不要复用其它域的」。
- 前端对应：大屏柱状图 `UpcomingDeliveryChart.vue` 的 `LAYERS[].statuses` ——
  `top`（4 态）+ `middle`（2 态）合起来正是这 6 态，`bottom`（`DELIVERED`）额外多一个。
- 三者是**人工同步**关系（Rust ↔ Rust ↔ TS），没有任何编译期约束，漂了不会编译失败，
  只会让「逾期数」与「柱状图层数」互相矛盾（现象是数字对不上、极难定位）。改任一处必须
  同步另两处。
- `repo.rs` 里把 `DELIVERY_STATUSES` 转成 `Vec<String>`（`LazyLock`）是因为 `query_as!`
  **宏**要求参数是具体可绑类型，`&[&str; 6]` 绑不上 `$N::varchar[]`；dashboard 的同类
  查询走运行时 `sqlx::query(..).bind(..)`，可以直绑。
- `t_assembly` 侧只可能命中其中 4 态（`AssemblyStatus` 无 `PROGRAMMING` / `OUTSOURCE`）
  —— 这是有意的：白名单按 part 状态域取全集，装配件侧对不上的两个自然不参与，
  **不需要**第二份常量。

### 5.2 状态**中文标签**（2026-10-16 新增，需求「状态列发送时要 alias 为中文」）

正文状态列输出中文，来源是 `mail_digest::dto::part_status_label` / `batch_status_label`：

| 原值 | 中文 |
|---|---|
| `PENDING` | 待生产 |
| `PROGRAMMING` | 编程中 |
| `IN_PROCESS` | 生产中 |
| `INSPECTION` | 待品检 |
| `READY_TO_SHIP` | 待送货 |
| `DELIVERED` | 已送货 |
| `REPAIRING` | 返修中 |
| `OUTSOURCE` | 外协中 |
| `COMPLETED` | 已完成 |
| `CANCELLED` | 已取消 |

- ⚠️ **未知值原样回显**（不静默变空串 / 不变 `未知`）：fail-loud 是这里刻意选的形态。
- ⚠️ **`REPAIRING` 已在 2026-10-01 从状态枚举删除**，语义由 `t_part_batch.is_repairing`
  布尔承载；`shared::batch::status_machine` 仍留一个过渡兼容分支能解析该字面量，故
  标签表保留这一行（存量行仍可能读出这个值）。批次子行则**优先看布尔**
  （`is_repairing = true` ⇒ 「返修中」）；工单行只能查表（`t_part` 上没有该列）⇒
  一行返修中的件，工单行显示「生产中」而它的子行显示「返修中」。
- ⚠️ **文案是「订单管理语义」**（与零件一览页一致），**不是统计页那套**
  （前端 `src/constants/partStatus.ts::STATUS_LABEL`，那是另一组文案）。
- ⚠️ **三副本人工同步**：后端 `dto::part_status_label`、前端
  `src/types/parts.ts::ORDER_STATUS_LABEL`、前端 `src/constants/partStatus.ts::STATUS_LABEL`。
  前两份**要求逐字一致**（同一批件在不同页面显示两种中文状态是最难查的一类问题）；
  第三份是统计语义、**刻意不同**，不要拿它当本表的依据。Rust ↔ TS 之间没有任何编译期
  约束。

## 6. 移除记录

**无端点下线、无字段删除、无 VO 裁剪**（本域 2026-10-13 新立）。⚠️ 但有一条**代码级**
的共用被移除，必须登记（它不是 HTTP 契约、不影响任何客户端）：

| 移除项 | 说明 |
|---|---|
| ⚠️ **与 dashboard 共用 `DELIVERY_STATUSES` 常量**（2026-10-16） | `repo.rs` 原先 `use crate::modules::dashboard::repo::DELIVERY_STATUSES`（「一处改、两处生效」）。按用户要求「**在邮件通知的域重写 SQL，不要复用其它域的**」，改为**本域自有常量** `mail_digest::repo::DELIVERY_STATUSES`，6 态字面量与 dashboard 逐字相同。**后果**：那 6 态从「编译期共用一个符号」退化为**三处人工同步**（本域常量 / dashboard 常量 / 前端 `LAYERS[].statuses`），见 §5.1。⚠️ 改判据不改 wire —— 本域的 HTTP 出参、入参、错误码**一个字都没变** |

已知的「刻意**没有**做的事」，记录在此以防后人来「补齐」：

| 没做的事 | 原因 |
|---|---|
| `PUT` / `PATCH` / `DELETE` 端点 | 全仓零这三种 HTTP 方法 |
| SMTP 凭据入库 | 跟着备份走 + 任何有只读库权限的人都能捞走 |
| 流水表软删 / `version` 列 | 只增的审计流水；软删会让「定时任务是不是在偷偷重发」永远查不清 |
| 分角色（不止 Manager） | 端点会对**真实收件人发信**，属配置型 + 有外发副作用的操作，只归管理员 |
| `send` 的幂等键 / 频控 | 见 §8.3「本域设计上的已知取舍」第 3 条 |
| 批次信息进 HTTP 出参 | 批次只存在于 `html` 字符串内部（§2.4），不给前端造第二个消费入口 |
| COS 预览链接 / 批量 CI | 需求原文第 5 条，**本轮不做** |

## 7. 与 WS 的关系

- **本域不发任何 WS 事件**：6 个端点里唯一的写端点 `POST /config` 改的是本域自有表，
  前端的其它页面不会因它失效。
- **本域不消费 WS 事件**：没有失效通道。配置页的新鲜度靠前端自身的 query 策略
  （挂载 refetch + 保存后显式 invalidate），与全仓其它只读/配置型域一致。
- ⚠️ 若将来要把「定时任务发信了」推到前端，**必须**新增 `WsEvent` 变体并在本节登记 ——
  不要借用现有的 `DashboardEvent` 变体（那会让大屏去 invalidate 一个与它无关的面板）。

## 8. 表依赖与前端配套

### 8.1 读 / 写的表（**清单即下表**；2026-10-16 批次与位置列新增 3 张 holder 表）

| 用途 | 表 | 本域关系 |
|---|---|---|
| 配置单例（读 + 写） | `t_mail_digest_config` | **owned**（`T*` 全列投影在 `model.rs`；**2026-10-16 新增 `sender_account_id` 一列**，无物理外键） |
| 发信通道（读一行 + 解密） | `t_mail_account` | **只读**（跨域，`scheduled_task` 域 owned；`service::resolve_sender`） |
| 发送流水（只增） | `t_mail_digest_log` | **owned**（刻意无 `deleted_at` / `version`） |
| 工单与系统交期 | `t_part` | 只读聚合（跨域） |
| 装配件与系统交期 | `t_assembly` | 只读聚合（跨域） |
| 已交量守卫 + **批次子行** | `t_part_batch` | 只读聚合（跨域）。⚠️ 2026-10-16 起**不再只是 `NOT EXISTS` 的存在性检查**，而是批次行的主表（`batch_no` / `quantity` / `status` / `is_repairing` / `location` / `current_holder_id` 六列） |
| 客户名 | `t_customer` | `LEFT JOIN`，**刻意不加** `AND c.deleted_at IS NULL` ⇒ **软删客户的名字照常带出**。取舍是「客户名以 `id` 为准、不受软删影响」（闸门放 `ON` 里本来也只会清空名字、不会剔除工单行，所以「不该剔除工单」**不是**这里的理由）；本列为空只因**客户行不存在**（`customer_id` 悬空）。与 dashboard 域同款 |
| **货架 `code`** | `t_shelf` | **2026-10-16 新增**。`LEFT JOIN` + `deleted_at IS NULL` 在 `ON` 里；两种货架档取的是 **`code` 不是 `name`** |
| **工人 `name`** | `t_worker` | **2026-10-16 新增**。同上（`location = 'WORKER'` 时参与匹配） |
| **外协公司 `name`** | `t_outsource_company` | **2026-10-16 新增**。同上（`location = 'OUTSOURCE_COMPANY'` 时参与匹配） |

⚠️ **张数以本节表格为准，不在正文写死**：别域表 = 统计用的 `t_part` / `t_assembly` /
`t_part_batch` / `t_customer` 四张，加位置解析用的三张 holder 表（`t_shelf` /
`t_worker` / `t_outsource_company`），再加通道域那一行 `t_mail_account`；自有表两张。
三张 holder 表**不是新依赖**，它们只在 `repo::list_batches` 一条 SQL 里以 `LEFT JOIN` 出现
（§4.7），且全部是**展示用附表**
（同 `prod::inspection` 的既有取舍：给不给软删闸门由「该不该出现在邮件里」决定，本域对
三张都判软删 —— 软删的货架 / 工人 / 外协厂**不该**出现在邮件里）。

- **域外依赖 = 1 个模块**（2026-10-16 由 0 变 1）：`scheduled_task`（邮箱通道域）。
  本域 import 了它的 `dto::MailAccountPreset` 与 `repo::get_account`，**只读**那一张表。
  方向是 **mail_digest → scheduled_task**（内容域依赖通道域）；反向不存在（通道域对
  内容域零代码引用，其「是否被引用」检查是自己写的 SQL，见 `docs/api/mail.md` §8.1）。
- **域隔离护栏（`assert_no_foreign_domain`）：不适用**（2026-10-16 复核过）。两类依赖
  都不是「图省事 import 别人的 service」：① 统计 SQL 完全在本域内写全、只读聚合别域的表
  （既定 pattern）；② 通道域是**通道域**、本域是**内容域**，「发信要拿通道的凭据」是这个
  切分的必然结果，且正确做法（自己写 SQL 查那一行、自己解密）本域已经做到了。
  ⚠️ 反向的护栏装在 `scheduled_task` 域。
- ⚠️ **env 侧的 SMTP 凭据已彻底废弃**（2026-10-16）：`MAIL_DIGEST_ENABLED` / `MAIL_SMTP_*`
  / `MAIL_FROM_*` **已从 `infra/config.rs` 删除、运行时不读**，`.env.example` 里也一并删除。
  现在 env 侧只剩**一个**与本域相关的键：`MAIL_ACCOUNT_SECRET`（授权码加密主密钥，
  由 `scheduled_task` 域与 `infra::crypto` 消费，本域只在解密时用到它）。

### 8.2 前端配套改动清单

| 落点（`frontend` 仓） | 说明 |
|---|---|
| 路由 / 视图 | ⚠️ **2026-10-16 改**：路径 `/mail/digest` → **`/mail`**（菜单已在 `seeds/menu.sql` 改成 `scheduled_task`「定时任务」→ `mail`「邮件」，`sort_order=10`）。页面上方用 **tab** 区分邮件类型，**当前只有「交期汇总」一种** |
| api 层 | 6 个函数：`fetchConfig` / `saveConfig` / `fetchMeta` / `previewMail` / `sendMail` / `fetchLogs` |
| 权限 | 菜单仅授予 `MANAGER`；页面路由守卫按同一角色判定 |
| 预览 | `/preview` 的 `html` 是完整 HTML 文档，**直接插 iframe**（`srcdoc` + `sandbox`）而**不要** `v-html` 塞进主文档 —— 后端已做转义，但 iframe 少一层依赖主文档上下文的暴露面 |
| 枚举 | **不要在前端另存一份频率 / 列名映射**：一律取 `GET /meta` 的 `frequencies[]` / `columns[]`（`label` 是后端给的中文表头） |
| ⚠️ **`columns[]` 多了两个 key，前端不需要改代码**（2026-10-16） | 本轮给列白名单加了 `batch_no`（表头「批次」）与 `location`（表头「位置」），`GET /meta` 的 `columns[]` 从 11 项变 **13 项**。**前端零改动**：`visible_columns` 的 schema 是 `z.array(z.string())`（不是枚举），`MailContentSection` 逐 `meta.columns[].key` 渲勾选框 ⇒ 新 key 自动出现、自动可勾、勾上就生效。⚠️ 别在前端补一份列名常量表（那是 §8.2「枚举」那条明令禁止的），也别把 key 数量写死成 11 |
| **发信邮箱下拉** | ⚠️ **2026-10-16 新增**：选项源取 `GET /api/v2/cron/mail/accounts`（`scheduled_task` 域）；提交时 `sender_account_id` 按 **JSON string** 发（发数字会 422）。⚠️ `POST /config` 是**全量覆写**，不带这个键 = 清成 `null`（= 定时任务不再注册 job） |
| 「立即发送」按钮 | 按 `smtp_configured === false` 灰掉并提示「**未选择发信邮箱**」（⚠️ 不再是「去配 `MAIL_SMTP_*`」—— 那套 env 已删） |
| `GET /meta` 的 `smtp` 段 | ⚠️ **已删除**（2026-10-16），逐条去处见 §2.7 的对照表。**不要**保留对它的引用 |
| 错误处理 | `21701`（503）「发信通道不可用」与 `21702`（502）「SMTP 挂了」**排查方向相反**，UI 文案必须区分，不要都显示成「发送失败」；`21706`（邮箱不存在/已删，保存配置时出现） |
| 雪花 ID | `ConfigOut.id` / `ConfigOut.sender_account_id` / `SendResultOut.log_id` 是 **JSON string**，不要按 `number` 声明 |

### 8.3 已知偏差登记

⚠️ **本节分五组，编号在每组内各自从 1 重新开始**（2026-10-13 review 第 1 轮 M4：
本节原先是一串 1..15 的连续编号，正文里却按「偏差 1/2/3/4」这种裸数字引用它们，
而那些偏差实际是第 10/11/12/13 条 —— 按数字跳过去落到的是 tokio-cron-scheduler 的 API
结论与 217xx 段位决策上，完全无关）。**正文引用一律写成「§8.3『组名』第 N 条」**，
组名五选一：

| 组名 | 覆盖 |
|---|---|
| 「依赖库版本相关的实测结论」 | 1–4 |
| 「本域设计上的已知取舍」 | 1–11（⚠️ 2026-10-16 新增第 9–11 条：迁移回填 / N+1 护栏局限 / 勾掉整列就没了） |
| 「实现与注释不符」 | 1–6（改实现前先改文档或先改测试；第 5 条已修） |
| 「覆盖空洞」 | 1–3（⚠️ 2026-10-16 新增第 3 条：SMTP 层失败造法换过一次） |
| 「与任务书的差异」 | 1–4（⚠️ 2026-10-16 新增第 3–4 条：需求第 5 条不做 / 多一列 `batch_no`） |

⚠️ 组数与条目数在 2026-10-13 review 第 2 轮又动过两次，都是**上一轮自己引入的自相矛盾**：
先给「实现与注释不符」加了第 5 条却没同步表里的 `1–4` 与紧邻标题的「四处」（M5-#5），
随后本轮按 C6 补「与任务书的差异」这一组 ⇒ 组数 3 → 4 → 5。**表里的区间与标题里的
中文数字是同一条信息的两份副本，改一处必须同步另一处** —— 这次两条都漏了。

**依赖库版本相关的实测结论（2026-10-13，实读 crate 源码核对，不是凭记忆）：**

1. **`tokio-cron-scheduler` 0.15 的 API 与旧教程不同**：`JobScheduler::new()` 在 0.15 是
   **`async fn`**（0.14 及以前是同步 `new()`）⇒ 用 `tokio::sync::OnceCell::get_or_try_init`
   承担异步初始化；`Scheduler::new(CoordinationMode)` 与
   `job.run_at(ScheduleType::Cron(..))` 在 0.15 **不存在**（0.15 用
   `new_async` / `new_async_tz` / `new_once` 三个构造器），照旧签名写会编译不过。
2. **`Job::new_async()` 的默认值是 UTC，不是本地时区**（源码 `Self::new_async_tz(schedule,
   Utc, run)`）。不传时区不是「按本地时间跑」而是「按 UTC 跑」⇒ 上海 8:30 的邮件会在
   UTC 8:30（上海下午 16:30）发出，且**不报错**。实现显式传
   `chrono_tz::Asia::Shanghai`（连带效果：`chrono-tz` 必须进主依赖；不再依赖 `TZ`）。
3. **lettre 0.11 没有 `charset` feature**（0.10 时代才有，charset 在 0.11 无条件编译）；
   **`TlsParameters` 没有 `Default()`**（只有 `new(domain)` / `new_rustls(domain)`，
   本仓走 rustls 栈 ⇒ 必须 `new_rustls(host)`，参数是 SNI 域名、填错会握手失败）；
   **`STARTTLS` 只能映射到 `Tls::Opportunistic`**（服务端不支持会**降级明文**继续发，
   凭据可能裸传）⇒ **默认档选 `WRAPPED`**（隐式 TLS，连接建不起来根本不会发凭据）。
4. **错误码段最终用 `217xx`**：任务书原定 `215xx`，但该段 2026-09-13 已被
   `outsource::shipment` 占用（`21501..21504`）；`216xx`（上传会话）2026-09-28 随域下线
   **释放但不复用**（老客户端 / 旧日志可能残留 216xx 映射，撞码比撞段更隐蔽）。

**本域设计上的已知取舍：**

1. **渲染无行数上限**：SQL 不分页，一封邮件可能包含窗口内全部逾期工单。已把
   `html_bytes` 记进流水以便事后评估；实测字节数过大时需重估（加 `LIMIT` 或分节
   多封信），**不要**在渲染层静默截断 —— 那会让收件人以为「就这些」。
2. **多副本部署不防重**：与既有 `task::auto_complete` 同一现状，部署 N 个副本就可能在
   同一时刻发 N 封信。要防重需要分布式锁 / 领导者选举，登记为独立重构。⚠️ **不要**用
   「加个 DB 唯一索引当半个防重」——那会让第二个副本发信失败被记成流水里的 `FAILED`，
   污染审计。
3. **`/send` 无频控**：它会对**真实收件人**发信。当前唯一的护栏是
   `MANAGER` 角色 + `t_mail_digest_config.enabled` 闸 + 「已选定可用发信邮箱」。
   （⚠️ 2026-10-16：原写作「+ `MAIL_DIGEST_ENABLED` env 总闸」，那套 env 已删。）
4. **`mailer::send` 返回裸 `String` 而不是 `AppError`**：同一段失败文本有两个去处
   （`t_mail_digest_log.error_message` 与 HTTP `message`）。若在这里就包成
   `AppError`，落库拿到的是 `Display`（带 `[21702] ` 前缀）⇒ 流水里会存下
   「[21702] [21702] …」这种重复前缀。返回裸文本后，落库存裸文本、HTTP 层再加码。
5. **热重载失败不回滚配置**：`POST /config` 成功后调 `task::mail_digest::reschedule`，
   它**永不 panic、永不把错误抛给调用方**（失败只 `warn!`）。配置已经落库是对的，
   只是这一次的调度没换上；下一次改配置 / 重启会再试。
6. ⚠️ **`21704` 是「配置不可用」的汇总裁决码，不是一因一码**（2026-10-13 review 第 1
    轮 m12）：同一个码目前承载**四种**互不相干的成因 ——
    ① `POST /config` 的配置组合非法（枚举 / 列白名单 / 时段 / 周几 / 几号）；② 两节都关；
    ③ `upcoming_days` 越界（仅 `/preview`、`/send` 路径不 clamp 而报错）；④ `deliver` /
    定时准备阶段发现收件人为空。⇒ **前端只能读 `message` 文本区分**，按码分支的写法在
    本域不成立。新增第五种成因时请一并更新本条，不要以为撞了同一个码就是同一个问题。
7. ⚠️ **`GET /logs` 每页都跑一次 `SELECT COUNT(*)`**（2026-10-13 review 第 1 轮 m8）：
   `repo::list_logs` 为了让 `total` 不受 `limit` 截断影响，除了取当页还要全表计数。
   该表**只增不减**（审计流水，无软删），也没有任何分页索引 ⇒ 行数涨上去后 `COUNT(*)`
   会成为这条端点的第一个瓶颈（随页码线性变慢）。**优化方向是它的第一个候选**，不是
   「先加索引」—— 索引救不了全表计数。若将来引入保留期/归档，优先做那件事。
8. ⚠️ **同一时刻只有一个 job，靠的是进程内互斥、不是分布式锁**（2026-10-13 review 第 2 轮
   M1 修复）：`task::mail_digest::swap_job` 用一把 `tokio::sync::Mutex` 把
   「摘旧 → 读库 → 注册新」整段串行。**必要性**：两个 `POST /config` 并发（axum 的
   handler 本就并发）时，原先只有「取 job id」那一个动作受保护，于是 A 取走旧 id、
   B 拿到 `None` 直接返回、两份配置各自注册出一个 job 而槽里只记后一个 ⇒ 先注册那个
   **永远摘不掉**，每天在旧时刻多发一封，直到进程重启。
   ⚠️ **与本组第 2 条是两层不同的问题，不要互相替代**：本条管**单进程内**
   「调度器里恒 ≤ 1 个 job」；第 2 条管**多副本**「N 个进程各发各的」，仍需分布式锁 /
   领导者选举。用这条冒充多副本防重（或反过来拿第 2 条当「进程内可以不互斥」的
   理由）都会漏掉真实故障。回归：`task::mail_digest::tests::concurrent_swap_leaves_exactly_one_job`。
9. ⚠️ **迁移会静默改掉管理员已存的列勾选**（2026-10-16，刻意决定）：迁移
   `20261016000001_001_mail_digest_default_columns.sql` 除了改列 DEFAULT，还
   **无条件回填**存量未软删行的 `visible_columns`（旧的 8 列 → 新的 10 列）。
   **必要性**：列白名单一旦被管理员存过就与列 DEFAULT 无关 —— 只改 DEFAULT 的话，
   已经保存过配置的环境永远看不到 `batch_no` / `location` 两列，而用户明确要求正文里
   能看到批次与位置。
   **代价**：管理员手工去掉过某列的勾选会被**重新加回**；该列现在**可以**再手工勾掉
   （`POST /config` 全量覆写）。影响面仅限升级那一次。⚠️ 别把回填删掉「让配置更尊重
   用户」—— 那会让本轮需求在已存配置的环境里静默失效。
10. ⚠️ **「无 N+1」是源码级护栏，不是运行时计数**（2026-10-16）：
    `tests/mail_digest/n_plus_one_guard.rs` 扫 `service::fetch_rows` 的源码，断言
    `for` / `while` / `loop` 体内不出现 `repo::` / `sqlx::query`，并另钉一条「顶层恰好
    3 个 `repo::` 调用点」。
    **为什么不装 sqlx `QueryLogger` 计数**：`tracing::subscriber::with_default` 只在当前
    线程生效、集成测试同进程并行跑多个 case；改全局 subscriber 又会把别的 case 算进来
    （多线程下还会静默少计，「条数相等」退化成 `0 == 0` 的假绿）。
    **已知局限**（明写出来，别当它是完备证明 —— 本仓口径：**未登记的漏报形态一律视为
    护栏缺陷**，遇到新形态先补登记再改判定）：① 对源码形态敏感 —— 把循环写进宏、
    或把 `repo::` 调用包进另一个函数再在循环里调它，都能绕过（仓内不存在这种写法）；
    ② 只扫 `fetch_rows` 一个函数体（写路径 `update_config` 等不在范围内 —— 它们是**写**
    不是读放大）；③ 判定的是**调用点**而不是语句条数，「恒 ≤ 3 条」还依赖「关掉的节
    不查」（上界来自三处调用点，两节都开时恰好 3 条）+「无工单时不发批次查询」这两条
    事实（`fetch_rows` 的 doc 已登记）；④ **迭代器链**——`rows.iter().map(|r| repo::
    list_batches(…)).collect()` 里没有 `for` 关键字，整个循环体扫不到；⑤ **`use` 之后的
    裸调用**——`use crate::…::repo::list_batches;` 再在循环里写 `list_batches(…)`，
    调用点没有 `repo::` 字面量可匹配；⑥ **并发扇出**——`tokio::try_join!` /
    `futures::join_all` 在循环体里并发发起多条查询，逐条语句数照样线性涨而两条护栏都绿。
11. **勾掉 `batch_no` / `location` 整列就没有了**（2026-10-16）：列白名单是**逐列**的，
    管理员勾掉 `location` 时批次的位置信息在正文里没有任何出口。⚠️ 这是列白名单的固有
    行为、不是缺陷 —— 不要为此在渲染层加「无视白名单的强制列」（那会让「可勾选」这个
    承诺失效）。回归：`template.rs::tests::dropping_batch_column_removes_batch_number_only`。

**实现与注释/直觉不符的六处（测试已把实际行为钉死，改实现前先改文档或先改测试）：**
**（2026-10-13 review 第 1 轮扩到五条：新增第 5 条是 M5「节开关不作用于正文」，
该项已在该轮修掉，保留在登记里是为了说明「这条曾经是偏差」以及防回归；
2026-10-16 增到六条：新增第 6 条是迁移里的 `enabled` 列注释已过期。）**

1. ⚠️ **`POST /config` 接受空 `recipients`**：`service::validate_save_request` 的模块 doc
    与 `NormalizedConfig::default` 的注释都写着「空数组会被 `POST /config` 的校验（21704）
    当场拦下」，**与实现不符** —— 实际只有**发送路径**（`deliver`）才判非空。配置页先存
    一份空配置（编辑中途的正常形态）是允许的。回归用例
    `config_accepts_empty_recipients_but_send_rejects_it`。
2. ⚠️ **非专属频率多填 `weekday` / `day_of_month` 是「清空」而不是 `21704`**：
    `DAILY` / `MANUAL_ONLY` / `MONTHLY` 之下 `weekday` 会被主动写 NULL（避免脏值被读侧
    照单全收地拼 cron），代价是「多填了字段」这件事**不**在写侧暴露。回归用例
    `daily_with_weekday_field_is_cleared_not_rejected`。
3. ⚠️ **「两节都关」一律 `21704`**，不会渲出「无符合条件的工单」：`/config`、`/preview`、
    `/send` 三处都拒。「无符合条件的工单」那句文案只在「节开着但窗口内无行」时出现。回归用例
    `preview_with_both_sections_off_returns_21704`。
4. ⚠️ **`MANUAL_ONLY` 之下 `send_hour` / `send_minute` 既不清空也不校验**：migration 的列
    注释写的是「`MANUAL_ONLY` → 四个字段全部不参与，写侧一律清 NULL」，但写侧只清
    `weekday` / `day_of_month`，`send_hour` / `send_minute` 是**原样透传**的，
    `validate_time` 对 `MANUAL_ONLY` 直接早退 ⇒ 连 `0..23` 范围都不查（能存进 99）。
    危害有限（`MANUAL_ONLY` 永不产生 cron，这两个字段不会被读），但**文档与列注释
    均需按实现订正**。回归用例 `manual_only_keeps_send_time_unchecked`。
5. ✅ **【已于 2026-10-13 review 第 1 轮 M5 修复】节开关曾不作用于正文**：
    `include_overdue=false` 时 `render()` 仍无条件渲染「已逾期」节，只是因为 service 层
    关掉的节不查库、拿到空切片，于是正文里出现「已逾期（系统交期早于今日）（0 条）/
    本节无符合条件的工单」。§2.1 当时把它描述成「是否**渲染**节」、需求原文是
    「内容可选 ① 节开关」，而这一半没兑现。**现状**：渲染层按 `cfg.include_*` 决定渲不
    渲染，关掉的节整段不出现（§4.8）。登记在此是为了防回归
    ——`preview.rs::preview_section_toggles_hide_each_section` 与 `template.rs` 的两条
    `closed_*_section_is_not_rendered_at_all` 单测各钉一半。
6. ⚠️ **迁移 `migrations/20261013000000_001_mail_digest.sql` 里 `enabled` 列的注释已过期**
    （2026-10-16）：列注释写「定时任务总闸（**第二道闸**；第一道是 env 的
    `MAIL_DIGEST_ENABLED`，见 `MailConfig::is_enabled`）」，而 `MAIL_DIGEST_ENABLED` 与
    `MailConfig` 已于本轮**整轮删除** ⇒ 现在**只有一道闸**（这一列自己），注册判据见
    `docs/api/mail.md` §4.5。⚠️ **迁移是 append-only，不得改**（改了就让已部署的库与新库
    分叉，而差异不会自动收敛），故只在文档里登记：**别按列注释推演「有两道闸、关掉 env
    那道就能停发」** —— 没有 env 那道闸，停发只能靠 `enabled = false` 或
    `frequency = MANUAL_ONLY`。

**覆盖空洞：**

1. **`/send` 的成功路径（`status='SUCCESS'` 的流水行 / 真实投递）无集成测试**：
    测试环境没有可达的 SMTP，且刻意**不**引入 lettre 的 mock transport（那会给生产依赖树
    加一层只为测试存在的构造）。当前替代覆盖：渲染 + 统计 SQL 由 `/preview` 全部用例覆盖
    （同一段 `template::render` + 同一对统计 SQL）；落流水由「必失败」路径覆盖
    （落流水在调 lettre **之后**、**无论成败**，两分支共用同一段代码）；TLS 三档映射表与
    收件人解析由 `mailer.rs` 的 lib 单测覆盖。补齐需要一个本地假 SMTP 或进程内 mock
    transport，登记为后续增强。
2. **`21705 BIZ_MAIL_CONFIG_NOT_FOUND` 是保留位，当前无生产方**：`GET /config` 在表空时
   返回默认值 + `version=0`，不返本码；`status_from_code` 已把它映射到 404 以备将来使用。
3. ⚠️ **「SMTP 层失败」的集成测试造法在 2026-10-16 换过一次**（登记，不修）：host / port
   由**编译期预设表**固定（网易 163 / QQ 均 465），原先「把 SMTP 指向 `127.0.0.1:1`」
   这种**确定性连不通**的造法**不再可复现** —— 而真去连 `smtp.163.com:465` 会在有外网
   的机器上「连得上但认证失败」、在无外网的机器上「DNS 失败」，两端都失败但**耗时不可控**
   （防火墙丢包时会等到 TCP SYN 超时）。现改用**零网络 IO** 的确定性失败：邮箱的
   自定义名里含 `<` / `>` ⇒ `mailer::send` 构造 `From` 头时 `Mailbox` 解析必然失败。
   `deliver` 里「`mailer::send` 返回 `Err` → 记 FAILED 流水 → 返 `21702`」的两段代码
   **逐字未变**，故守的不变量不变。完整登记见 `docs/api/mail.md` §8.3 第 8 条。
   ⚠️ 用例名 `send_to_unreachable_smtp_returns_21702_and_records_failed_log` **沿用旧名**
   （doc 里已写清新造法），改名会让「这条用例在 git 历史里改过什么」更难追。

**与任务书的差异：**

⚠️ 本组登记的是「实现与任务书原文不一致、且**刻意**这么实现」的地方 —— 不是缺陷，
但**后人拿任务书当验收标准会判错**。错误码段位（任务书 `215xx` → 实现 `217xx`）的差异
登记在「依赖库版本相关的实测结论」第 4 条，不在本组重复。

1. **`GET /config` 出参是 19 字段，任务书写的是 15**（2026-10-16 从 18 更新：本分支给
   `ConfigOut` 加了 `sender_account_id`，§2.1 的字段表随之 18 → 19 行，两处必须一起改）。
   ⚠️ **只登记数量差、不点名差哪 4 个**：任务书原文没有随代码进仓，逐个字段名去比对
   只能靠回忆，那正是最容易写错的一种「看似精确的记录」。已核实的是**实现侧这 19 个
   字段没有一个是冗余**（逐字段理由见 §2.1）：OCC 与展示所需的 `id` / `version` /
   `created_at` / `updated_at`、调度六元组、收件人两组、节开关与窗口、列白名单、
   主题模板、发信邮箱 `sender_account_id`、以及前端用来灰按钮的 `smtp_configured`。⇒
   **前端以 §2.1 为准**；若要追平任务书的 15 个字段，正确做法是**删实现**，而不是改文档
   说「本来就没这些」。
2. ⚠️ **`GET /meta` 的 `smtp` 段已于 2026-10-16 整体删除**，本条**不再是「字段数差异」**：
   任务书里的那 4 个字段、以及实现侧多出的 `tls_mode` 与 `from_name`，连同整段一起去掉了
   —— 逐条换到了别处，对照表见 §2.7。⚠️ **别把本条读成「`smtp` 段还在、只是字段数与
   任务书不同」**：那是本条登记**之前**的状态，保留在此只为防一件事 —— 有人拿任务书当
   验收标准，跑过去在 `/meta` 上找一个已经删掉的段、然后以为实现「漏做了一部分」。
   判定一律以 §2.7 为准；前端改造时**不要**保留对 `smtp` 段的引用
   （那段的内容里**从来**不含 username / password）。

3. ⚠️ **需求第 5 条（COS 预览链接 / 批量 CI）本轮不做**（2026-10-16）：任务书列了
   「…」的第 5 条，本轮**实现范围不含它** —— 邮件正文没有图纸预览链接、也没有触发批量
   CI 的动作。⚠️ 拿任务书逐条验收会在这一条判错；剩余条目（状态中文 / 批次子行 / 位置列 /
   本域重写 SQL / 禁 N+1 / 列对齐与打印）都已落地，索引见 §2.3、§4.4–§4.7。

4. ⚠️ **多出一列 `batch_no`（表头「批次」），任务书没要求**（2026-10-16）：用户原话只说
   「工单下方要紧跟着对应的所有批次」「增加一列位置」。批次子行若没有批次号，一行只有
   「数量 / 状态 / 位置」，读者分不清它是第几批 ⇒ 补了这一列。⚠️ 它与 `location` 一样
   **只对批次子行有值、工单行留空**，且**同样可勾可去勾**（§8.3「本域设计上的已知取舍」
   第 11 条）—— 不想看的可以勾掉。
