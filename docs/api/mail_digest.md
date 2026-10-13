# mail_digest 域 API（交期到期汇总邮件）

> **2026-10-13 新建域**。把「已逾期 + 未来 N 天系统交期到期」的**工单级**汇总成一封
> HTML 表格邮件，按配置的频率与时刻定时发出，也支持手动触发与预览。
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

## 1. 端点表

**全部 6 条都是 Manager 独占**（角色守卫写在各 handler 第一行，逐字
`current.require_role(Role::Manager)?`）。前缀 `/api/v2/mail-digest`。

| 方法 | 路径 | 权限 | 一句话 |
|---|---|---|---|
| `GET` | `/api/v2/mail-digest/config` | Manager | 读配置**单例**；表里无行时返回全部默认值 + `version=0` + `id="0"`（**不是 404**） |
| `POST` | `/api/v2/mail-digest/config` | Manager | **全量覆写**；body **必含** `version`（OCC）；成功后热重载 cron（失败不回滚配置） |
| `GET` | `/api/v2/mail-digest/meta` | Manager | **内容与调度**侧的表单选项源：频率枚举 / 列白名单 / 取值范围 / 缺省值（⚠️ **2026-10-16 起不再含 `smtp` 段**） |
| `POST` | `/api/v2/mail-digest/preview` | Manager | 按「当前配置 + 临时覆盖」渲染 HTML；**不发送、不落流水** |
| `POST` | `/api/v2/mail-digest/send` | Manager | 手动触发：渲染 + 真发 + 落流水（**成败都落**） |
| `GET` | `/api/v2/mail-digest/logs` | Manager | 发送历史分页（`sent_at DESC, id DESC`；`limit∈[1,200]` + `offset≥0`） |

- **只用 GET / POST**：本仓 HTTP 方法**零** PUT/PATCH/DELETE，故配置写入走
  `POST /config`（全量覆写）而非 `PUT /config/{id}`。
- **权威路由清单**：`src/modules/mail_digest/handler.rs::ROUTES`（lib 单测逐条比对
  `router()` 源码，加了 `.route()` 忘登记就在单测里当场红）。
- 挂载点：`src/modules/mod.rs::v2_router()` 的 `.nest("/mail-digest", …)`（顶层 nest，
  kebab-case 单段，与 `/part-files` / `/cnc-programs` / `/outsource-queue` 同款）。
  ⚠️ **2026-10-16**：同一次提交里新增了同族的 `.nest("/mail", scheduled_task::router())`
  （邮箱通道域，路径 `/mail`）。两者无冲突：matchit 按段数 + 静态段优先匹配，
  `/mail/meta`（2 段）不会落到 `/mail-digest`（1 段静态段）上。菜单也同批从
  `mail_group → mail_digest`（`/mail/digest`）改成 `scheduled_task → mail`（`/mail`）。
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
| `visible_columns` | string[] | 正文表格列白名单，**封闭枚举**（§2.3）；未知项 → `21704`（**不静默丢弃**）；空数组在渲染层退回缺省 8 列 | 否 |
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

### 2.3 列白名单（`visible_columns`，封闭枚举）

| key | 中文表头 | 来源列 |
|---|---|---|
| `serial_no` | 序列号 | `t_part.serial_no` / `t_assembly.serial_no`（可空 ⇒ 空串） |
| `drawing_no` | 图号 | 同名列 |
| `name` | 名称 | 同名列 |
| `quantity` | 数量 | `t_part.quantity` / `t_assembly.quantity`；**装配件行单位是「套」**，渲染成 `12 套` |
| `customer_name` | 客户 | `LEFT JOIN t_customer.name`（软删客户退化为空串，**不**剔除该工单） |
| `status` | 状态 | 6 态白名单之一（装配件侧只可能命中其中 4 态，见 §5） |
| `system_delivery_date` | 系统交期 | `date`，格式 `YYYY-MM-DD` |
| `remaining_days` | 剩余天数 | `(交期 − today)` 天数；今天到期 = `0`，逾期为负 |
| `row_type` | 工单类型 | 渲染为「装配件」/「零件」 |
| `is_urgent` | 是否紧急 | 渲染为「是」/「否」 |
| `order_no` | 订单号 | `varchar(30)`，可空 |

**缺省 8 列** = 前 8 个（不含 `row_type` / `is_urgent` / `order_no`）。

### 2.4 `POST /preview` 与 `POST /send` 入参（`PreviewRequest`）

**不含 `version`**（这两个端点不改配置，OCC 对它们没有意义），且**全部字段可选** ——
空体 `{}` = 完全用当前配置。字段与 §2.2 同名同型（`Option<T>` 语义：`null` = 沿用当前值）。
合并后**重新跑一遍校验**（覆盖值可能让原本合法的配置变非法，如 `upcoming_days = -5`）。

出参：

| 端点 | 出参 | 说明 |
|---|---|---|
| `/preview` | `{ subject, html, overdue_count, upcoming_count, columns }` | `html` 是**完整 HTML 文档**（前端直接塞进 iframe / `v-html` 预览）；`columns` 是**实际参与渲染**的列 |
| `/send` | `SendResultOut` | 见 §2.5 |

### 2.5 `SendResultOut`（`POST /send` 出参 = 流水行视图）

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
出现**：概览行只列**已启用**的两节（§4.4）——两个出口给同一件事两种口径，正是 M5 与
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

### 2.6 `GET /meta` 出参（⚠️ 2026-10-16 删掉 `smtp` 段）

| 字段 | 类型 | 说明 |
|---|---|---|
| `frequencies[]` | `{value, label, needs_time, needs_weekday, needs_day_of_month}` | 4 个预设；三个 `needs_*` 驱动表单的显示/隐藏（`MANUAL_ONLY.needs_time = false`） |
| `columns[]` | `{key, label}` | §2.3 白名单（顺序 = 展示顺序 = 缺省顺序） |
| `defaults` | `ConfigDefaultsOut` | 缺省配置（**不含** `id` / `version` / 时间戳 / `smtp_configured`；**含** `sender_account_id`，缺省恒 `null`） |
| `upcoming_days_range` | `{min: 1, max: 60}` | 前端数值控件边界 |

**已删除的 `smtp` 段**原先带的是 env 侧 SMTP 公开信息，而那些值在「邮箱改为页面内配置」
之后**逐条换了归属**：

| 原 `smtp.*` | 新的去处 |
|---|---|
| `configured` | `ConfigOut.smtp_configured`（`GET /config`，纯 DB 判定） |
| `host` / `port` / `tls_mode` | `GET /api/v2/mail/meta` 的 `presets[]`（`scheduled_task` 域，见 `docs/api/mail.md` §3） |
| `from_address` | `GET /api/v2/mail/accounts` 的 `account` |
| `from_name` | `GET /api/v2/mail/accounts` 的 `name` |

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

### 4.1 与 dashboard 域**共用**的部分（逐字同源，不要改）

| 判据 | 说明 |
|---|---|
| `DELIVERY_STATUSES` | 6 态白名单（`PENDING` / `PROGRAMMING` / `IN_PROCESS` / `OUTSOURCE` / `INSPECTION` / `READY_TO_SHIP`），**共用同一常量** `dashboard::repo::delivery::DELIVERY_STATUSES` |
| **行单位 = 工单级** | `t_part` 侧带 `AND p.assembly_id IS NULL`（子件不重复计入），装配件单独查 `t_assembly` 且其行**替换**子件行 |
| **已交量守卫** | `NOT EXISTS (… FROM t_part_batch b WHERE b.part_id = … AND b.deleted_at IS NULL AND b.status IN ('DELIVERED','COMPLETED'))` |
| 软删闸门 | `p/a.deleted_at IS NULL` |

### 4.2 与 dashboard 域**刻意不同**的部分（需求决定的，不是漂移）

| | dashboard | mail_digest |
|---|---|---|
| 每桶行数上限 | `LIMIT 30`（`DELIVERY_BUCKET_LIMIT`，大屏一屏放不下） | **无 LIMIT**（邮件必须给全量 —— 漏一条交期就是漏一个客户） |
| 取哪些列 | 9 列（无 `drawing_no` / `order_no`） | 10 列（邮件要图号 / 订单号） |
| 未来节窗口 | `>= today`，**无上界** | **`[today, today+N)` 半开区间**，`N = upcoming_days` clamp `1..60` |
| 逾期节窗口 | （KPI 另算） | `system_delivery_date < today` —— **含今天之前、不含今天**（今天的交期属未来节的「今日」组） |
| 客户名 | 单独批量聚合（防 N+1） | 本 SQL 里 `LEFT JOIN` 一次带出 |
| 行单位 | 交期面板**工单级** / 柱状图与抽屉**件级** | **一律工单级**（本域不跟柱状图的件级） |

⇒ 故**不复用** `DeliveryRepo::list_orders_*`：复用就得先解掉它的 `LIMIT 30`
（等于改坏大屏）或在外面再分页（等于让 SQL 口径与 Rust 侧重叠，两边都要维护）。

### 4.3 三条硬约束（改 SQL 前逐条读）

1. **工单级行单位**：`t_part` 侧必须带 `AND p.assembly_id IS NULL`。
2. **`NOT EXISTS` 必须同时判 `b.deleted_at IS NULL` 且状态集含 `'COMPLETED'`**：
   force-complete 端点（`POST /prod/parts/{id}/force-complete` 与
   `POST /prod/assemblies/{id}/force-complete`）把批次强推 `COMPLETED`，守卫漏掉它
   会让**已完工**的工单继续出现在「逾期未交」邮件里。
3. **窗口下界来自形参 `today`，SQL 里绝不写 `CURRENT_DATE`**：DB 会话时区（测试容器
   UTC）与 `infra::clock::now_naive()` 的 Asia/Shanghai 是**两个时钟**，Shanghai
   00:00–08:00 期间 `CURRENT_DATE == today - 1`，于是「今天到期」的工单会落进逾期节、
   未来节整体偏移一天。

### 4.4 渲染口径

- **主题**：缺省 `交期汇总 YYYY-MM-DD（未来N天）`；`subject_template` 支持
  `{date}` / `{days}`，未知 `{xxx}` 原样保留。主题里的 CR/LF 在写侧就拒（`21704`）。
- **正文**：全**行内样式**、无 `<style>` 块 / 无外链 / 无图片（企业邮与 QQ 邮箱默认拦
  远程内容），表格布局（不吃 flex/grid），关键底色 `bgcolor` 与 `background-color` 两处都写。
- **XSS**：图号 / 名称 / 客户名 / 订单号 / 序列号**每一个插值点**都过 `html_escape`
  （`& < > " '` 五个字符，`&` 必须**第一个**替换，否则 `&lt;` 会被二次转义成 `&amp;lt;`）。
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
  `colspan="*"` 非法、客户端一律回落到 1，标题行于是只盖住第一列）。

## 5. 状态域约定（无编译期保障）

- 后端常量：`dashboard::repo::delivery::DELIVERY_STATUSES`（6 态）。
- 前端对应：大屏柱状图 `UpcomingDeliveryChart.vue` 的 `LAYERS[].statuses` ——
  `top`（4 态）+ `middle`（2 态）合起来正是这 6 态，`bottom`（`DELIVERED`）额外多一个。
- 两者是**人工同步**关系，Rust 常量与 TS 字面量之间没有任何编译期约束，漂了不会编译失败，
  只会让「逾期数」与「柱状图层数」互相矛盾（现象是数字对不上、极难定位）。改任一侧必须同步另一侧。
- ⚠️ **本域不引入第二份状态常量**：`repo.rs` 里把 `DELIVERY_STATUSES` 转成
  `Vec<String>` 是为了让 `query_as!` 宏能绑 `varchar[]`（宏要求具体可绑类型），
  取值仍来自 dashboard 那份常量，**一处改、两处生效**。
- `t_assembly` 侧只可能命中其中 4 态（`AssemblyStatus` 无 `PROGRAMMING` / `OUTSOURCE`）
  —— 这是有意的：白名单按 part 状态域取全集，装配件侧对不上的两个自然不参与，
  **不需要**第二份常量。

## 6. 移除记录

**本域 2026-10-13 新立，到目前为止没有任何移除项**（无端点下线、无字段删除、无 VO 裁剪）。

已知的「刻意**没有**做的事」，记录在此以防后人来「补齐」：

| 没做的事 | 原因 |
|---|---|
| `PUT` / `PATCH` / `DELETE` 端点 | 全仓零这三种 HTTP 方法 |
| SMTP 凭据入库 | 跟着备份走 + 任何只读库权限的人都能捞走 |
| 流水表软删 / `version` 列 | 只增的审计流水；软删会让「定时任务是不是在偷偷重发」永远查不清 |
| 分角色（不止 Manager） | 端点会对**真实收件人发信**，属配置型 + 有外发副作用的操作，只归管理员 |
| `send` 的幂等键 / 频控 | 见 §8.3「本域设计上的已知取舍」第 3 条 |

## 7. 与 WS 的关系

- **本域不发任何 WS 事件**：6 个端点里唯一的写端点 `POST /config` 改的是本域自有表，
  前端的其它页面不会因它失效。
- **本域不消费 WS 事件**：没有失效通道。配置页的新鲜度靠前端自身的 query 策略
  （挂载 refetch + 保存后显式 invalidate），与全仓其它只读/配置型域一致。
- ⚠️ 若将来要把「定时任务发信了」推到前端，**必须**新增 `WsEvent` 变体并在本节登记 ——
  不要借用现有的 `DashboardEvent` 变体（那会让大屏去 invalidate 一个与它无关的面板）。

## 8. 表依赖与前端配套

### 8.1 读 / 写的表

| 用途 | 表 | 本域关系 |
|---|---|---|
| 配置单例（读 + 写） | `t_mail_digest_config` | **owned**（`T*` 全列投影在 `model.rs`；**2026-10-16 新增 `sender_account_id` 一列**，无物理外键） |
| 发信通道（读一行 + 解密） | `t_mail_account` | **只读**（跨域，`scheduled_task` 域 owned；`service::resolve_sender`） |
| 发送流水（只增） | `t_mail_digest_log` | **owned**（刻意无 `deleted_at` / `version`） |
| 工单与系统交期 | `t_part` | 只读聚合（跨域） |
| 装配件与系统交期 | `t_assembly` | 只读聚合（跨域） |
| 已交量守卫 | `t_part_batch` | 只读聚合（跨域） |
| 客户名 | `t_customer` | `LEFT JOIN`，**刻意不加** `AND c.deleted_at IS NULL`（客户软删时该列退化为空串，但**不该**因此把工单从邮件里剔除） |

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
| **发信邮箱下拉** | ⚠️ **2026-10-16 新增**：选项源取 `GET /api/v2/mail/accounts`（`scheduled_task` 域）；提交时 `sender_account_id` 按 **JSON string** 发（发数字会 422）。⚠️ `POST /config` 是**全量覆写**，不带这个键 = 清成 `null`（= 定时任务不再注册 job） |
| 「立即发送」按钮 | 按 `smtp_configured === false` 灰掉并提示「**未选择发信邮箱**」（⚠️ 不再是「去配 `MAIL_SMTP_*`」—— 那套 env 已删） |
| `GET /meta` 的 `smtp` 段 | ⚠️ **已删除**（2026-10-16），逐条去处见 §2.6 的对照表。**不要**保留对它的引用 |
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
| 「本域设计上的已知取舍」 | 1–8 |
| 「实现与注释不符」 | 1–6（改实现前先改文档或先改测试；第 5 条已修） |
| 「覆盖空洞」 | 1–3（⚠️ 2026-10-16 新增第 3 条：SMTP 层失败造法换过一次） |
| 「与任务书的差异」 | 1–2 |

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
    渲染，关掉的节整段不出现（§4.4）。登记在此是为了防回归
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
   —— 逐条换到了别处，对照表见 §2.6。⚠️ **别把本条读成「`smtp` 段还在、只是字段数与
   任务书不同」**：那是本条登记**之前**的状态，保留在此只为防一件事 —— 有人拿任务书当
   验收标准，跑过去在 `/meta` 上找一个已经删掉的段、然后以为实现「漏做了一部分」。
   判定一律以 §2.6 为准；前端改造时**不要**保留对 `smtp` 段的引用
   （那段的内容里**从来**不含 username / password）。