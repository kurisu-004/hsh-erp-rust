# wecom_daily 域 API（企业微信每日总结推送：内容域）

> **2026-10-18 新建域**。需求（用户原话）：
> 「我需要一个每日总结定时推送，内容为今日生产情况：1.全厂报工总览 2.今日出货单数
> 3.逾期未交工单，3日内交期工单 4.所有工人今日报工统计（用条形统计图展示）」
> 「消息不要截断，如果发现字符超上限则分成两条消息发送」
> 「按照 1 的设计，参数应该在配置页可调」。
>
> **全新域，无任何旧路径、无 alias**（历史上从未存在过 `/api/v2/wecom-daily/*` 之类的
> 公开路径）。URL 挂 `/api/v2/cron/wecom-daily`，归入「定时任务」域组的 `/cron` 前缀族。
>
> | 子域 | 职责 | 模块 | URL |
> |---|---|---|---|
> | `mail` | 邮件**通道** | `src/modules/scheduled_task/` | `/api/v2/cron/mail/*` |
> | `mail-digest` | 邮件**内容** | `src/modules/mail_digest/` | `/api/v2/cron/mail-digest/*` |
> | `wecom` | 企业微信**通道** | `src/modules/wecom/` | `/api/v2/cron/wecom/*` |
> | `wecom-daily` | 企业微信**内容**（本文件） | `src/modules/wecom_daily/` | `/api/v2/cron/wecom-daily/*` |
>
> 代码位置：`src/modules/wecom_daily/`（八件套）、调度 `src/task/wecom_daily.rs`、
> 迁移 `migrations/20261018000000_001_wecom.sql`、错误码段 `shared/error.rs` 的 `218xx`。
> 通道侧（webhook 实体 / 加密 / 掩码 / 出网）见 [`wecom.md`](wecom.md)。
>
> ## 域隔离：内容域 → 通道域，**单向**
>
> 本域 import `wecom` 的 `service::resolve_target` / `service::target_is_available` /
> `sender::send_markdown`；`wecom` 域**对本域零代码依赖**（§8.1）。
> ⚠️ 唯一的例外是一处**只读**引用：`service::webhook_row_exists` 调
> `wecom::repo::get_webhook` 取 `ConfigOut.webhook_configured`。理由见 §2.1。
>
> ⚠️ 本域**不解密**任何东西：webhook URL 的解密在通道域内完成（明文只在调用栈里活着）。
>
> ## 取数 SQL **零他域符号引用**
>
> 本域的统计 SQL 完全写在本域（`repo.rs`），**不 import** `statistics` / `mail_digest` /
> `dashboard` 的任何符号（用户 2026-10-16 定的硬要求「在推送通知的域重写 SQL，不要复用
> 其它域的」）。代价：「未交付」状态白名单变成**四处人工同步**，见 §5。

## 1. 端点表（5 个端点 / 6 条路由，**全部 Manager 独占**）

挂载点：`src/modules/mod.rs::v2_router()` 的
`.nest("/cron/wecom-daily", wecom_daily::router())`（**独立 `.nest()`**，不合并成
`/cron` 的一个 router；理由见 `modules/mod.rs` 里那段「`/cron` 前缀族的 matchit 注册
顺序核对」）。权威路由清单是 `wecom_daily::handler::ROUTES`（lib 单测逐条比对
`router()` 源码）。

| 方法 | 路径 | 说明 |
|---|---|---|
| `GET` | `/api/v2/cron/wecom-daily/config` | 读配置**单例**。表空时返**全默认 + `version=0` + `id="0"`**（**不是 404**） |
| `POST` | `/api/v2/cron/wecom-daily/config` | **全量覆写**，body **必含 `version`**。成功后 `task::wecom_daily::reschedule` 热重载 cron（**失败不回滚配置**，只 warn） |
| `GET` | `/api/v2/cron/wecom-daily/meta` | 表单选项源：**6 个键**（频率 / 工人榜指标 / 缺省值 / 三段取值范围） |
| `POST` | `/api/v2/cron/wecom-daily/preview` | 渲染分片。**不发送、不落流水**。空体 `{}` = 全用当前配置。⚠️ **超 `max_chunks` ⇒ `21804`，绝不截断** |
| `POST` | `/api/v2/cron/wecom-daily/send` | 真发 + 落流水（**成败都落**）。**顺序**逐片发送 |
| `GET` | `/api/v2/cron/wecom-daily/logs` | 分页，`sent_at DESC, id DESC`，`limit` clamp `[1,200]` 缺省 20，`offset` clamp `>=0`，`total` 不受 limit 截断 |

⚠️ **5 个端点 / 6 条路由**：`/config` 读与写是两条路由（前端契约里它们是两个语义完全
不同的动作）。`ROUTES` 列 6 条。

**硬切记录**：全新域，无旧路径、无 alias。⚠️ **部署顺序：后端必须先上** —— 旧路径 404
且失败发生在**网络层**（不是契约层），现场只看到一句 axios 错误。

**动词纪律**：只用 GET / POST（全仓零 PUT/PATCH/DELETE）。回归用例
`tests/wecom_daily/config.rs::put_patch_delete_verbs_are_not_routed` 钉住（405）。

## 2. 逐字段

### 2.1 `ConfigOut`（`GET/POST /config`，**21 个字段**）

`id` / `webhook_id` 是 JSON **string**（雪花防 JS 精度截断）。

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string | 配置行 id；**表空时恒为 `"0"`** |
| `version` | number | OCC 锚，`POST /config` 必填；**表空时为 0**（首次初始化的合法锚） |
| `enabled` | bool | 定时总开关。`false` 时不注册调度，但**手动发送不看本列** |
| `frequency` | string | `DAILY` / `WEEKLY` / `MONTHLY` / `MANUAL_ONLY` |
| `send_hour` | number \| null | 0..23 |
| `send_minute` | number \| null | 0..59 |
| `weekday` | number \| null | WEEKLY 用，ISO 1=周一..7=周日 |
| `day_of_month` | number \| null | MONTHLY 用，1..28 |
| `webhook_id` | string \| null | `t_wecom_webhook.id`（**无物理外键**）。`null` = 未选通道：定时任务不注册 job、手动发送返 `21801` |
| `upcoming_days` | number | 第三节窗口天数，缺省 3，**clamp 1..60** |
| `worker_metric` | string | `QUANTITY`（送件数）/ `BATCH`（报工批次数）。⚠️ 迁移列注释写的 `HOURS` 是过时的，见 §8.3 第 1 条 |
| `worker_top_n` | number | 第四节人数上限，缺省 15，**0 = 不限**，范围 0..200 |
| `max_chunks` | number | 单次最多发几条，缺省 3，范围 **1..20**（20 = 企业微信官方限频） |
| `include_overview` / `include_delivery` / `include_orders` / `include_workers` | bool | 四节开关，缺省全 true。⚠️ **四节全关 ⇒ `21804`** |
| `subject_template` | string \| null | 标题模板，≤200 **字**，拒 CR/LF。`null` ⇒ 域内默认串 |
| `webhook_configured` | bool | **纯 DB 存在性检查**（`webhook_id` 指向的行还在不在），**不解密** |
| `created_at` / `updated_at` | string | naive timestamp；表空时是**空串**（不是会跳动的假时间） |

⚠️ **`webhook_configured` 刻意不解密**，因此它用的是
`wecom::repo::get_webhook`（纯存在性）而不是 `wecom::service::target_is_available`
（后者会真解一次密）。理由：把「运维换了 `MAIL_ACCOUNT_SECRET` 导致存量密文全解不开」
显示成「通道没配置」，会让管理员去建一条新 webhook 而不是找运维换回密钥 —— 那是
**方向性错误**的提示。解密失败那一类只在真推送时以 `21801` 暴露。
（调度注册的判据仍用 `target_is_available`，见 §7 —— 那里解密失败会让 job 每次触发都
白跑一遍，不如启动阶段就不注册。）

### 2.2 `SaveConfigRequest`（`POST /config` 入参，全量覆写）

形状 = 上面 21 个字段**去掉** `id` / `version` 的新增 / 时间戳 / `webhook_configured`，
**加上** `version`（必填）。

⚠️ `version` **必填、无 `#[serde(default)]`**：缺失时 axum `Json` 提取器返 **HTTP 422
纯文本**，不是业务信封（回归用例 `config::missing_version_is_422_plain_text`）。

⚠️ **与写侧校验的两处刻意不对称**（都照 `mail_digest` 既有口径）：
- `upcoming_days` **clamp** 1..60（越界时夹到边界 —— 这个数只影响窗口宽窄）；
- `worker_top_n` / `max_chunks` / `worker_metric` / 频率配套字段 / 四节全关 **一律拒**
  （`21804`）。

⚠️ `webhook_id` 指向不存在**或已软删**的 webhook ⇒ `21806`（写侧校验，不等到推送时才发现）。

### 2.3 `PreviewRequest`（`POST /preview` 与 `POST /send` 共用）

配置字段的**部分覆盖**，全部键可选（空体 `{}` = 完全用当前配置）。
⚠️ **不含 `version`**（这两个端点不写配置）；⚠️ **不含 `webhook_id`**：「推到哪个群」是
一条**配置**决策，不该被「预览一次」顺带改掉（否则 `/send` 的覆盖就成了绕过配置页直接
换通道的后门）。serde 默认忽略未知键 ⇒ 前端带上它也无妨。
覆盖值**重新跑一遍完整校验**（它在预览框里是用户现填的，夹到边界会让预览与输入不符）。

### 2.4 `MetaOut`（`GET /meta`，**6 个键**）

| 键 | 内容 |
|---|---|
| `frequencies[]` | `{value, label, needs_time, needs_weekday, needs_day_of_month}` × 4 |
| `worker_metrics[]` | `{value, label}` × 2（`QUANTITY`「送件数」/ `BATCH`「报工批次数」） |
| `defaults` | 与 `ConfigOut` 同构，去掉 `id`/`version`/两个时间戳/`webhook_configured`；`webhook_id` 恒 `null` |
| `upcoming_days_range` | `{min: 1, max: 60}` |
| `worker_top_n_range` | `{min: 0, max: 200}`（0 = 不限） |
| `max_chunks_range` | `{min: 1, max: 20}` |

⚠️ **三个 range 与四个 `needs_*` 全部由本端下发，前端不许硬编码**。

### 2.5 `PreviewOut`（`POST /preview`，**15 个字段**）

| 字段 | 说明 |
|---|---|
| `subject` | 渲染后的标题（**不含**分页后缀；后缀只进各片的 Title 块） |
| `chunks[]` | 将按顺序发送的 N 条，逐条 ≤ 4096 **字节** |
| `chunk_count` | 冗余字段，恒等于 `chunks.len()` |
| `overview_batch_count` / `overview_quantity` | 第一节：报工批次数 / 送检放回件数 |
| `completed_count` / `in_process_count` / `repair_part_count` | 第一节：今日完工 / 当前在制 / 今日返修 |
| `note_count` / `note_part_count` | 第二节：今日已取货送货单数 / 合计件数 |
| `overdue_count` / `upcoming_count` | 第三节：逾期未交 / N 日内交期 |
| `worker_rows[]` | `{worker_name, badge_code, batch_count, quantity}` —— **两条指标都返回**，不受 `worker_metric` 影响 |
| `worker_row_total` | **全量**工人行数（可能 > `chunks` 里出现的条数） |
| `sections[]` | 本次实际生效的节：`overview` / `delivery` / `orders` / `workers` |

⚠️ **契约里没有 `markdown` 单字段** —— 只维护一个真相源。同时维护 `markdown` 与
`chunks` 会让「要发几条」有两个答案，而那恰恰是管理员唯一需要预判的东西。

### 2.6 流水行（`SendResultOut` = `LogListOut.items[]` 的元素，**19 个字段**）

| 字段 | 说明 |
|---|---|
| `log_id` | string（雪花） |
| `config_version` | 推送当时的配置版本**快照**（不 join 配置表） |
| `trigger_source` | `SCHEDULED` / `MANUAL` |
| `status` | `SUCCESS` / `FAILED` |
| `webhook_name` | 通道**名称**快照（⚠️ 是名称不是 id：通道行可能已软删，流水仍要说得清发到哪个群） |
| `webhook_masked` | 通道地址**掩码**快照（`…?key=<前4>***<后4>`，不含完整 key） |
| `report_date` | 统计基准日 `YYYY-MM-DD`。⚠️ **恒等于 `sent_at` 的日期**（同一个 `now_naive()` 取值） |
| `markdown_bytes` | **全部片**的字节数**之和** |
| `chunk_count` | 渲染出的片数。⚠️ **`0` 是合法值** = 准备阶段就失败、还没渲染 |
| `chunk_sent` | 实发片数 |
| `overview_batch_count` / `overview_quantity` / `note_count` / `note_part_count` / `overdue_count` / `upcoming_count` / `worker_row_total` | 各节计数快照 |
| `error_message` | string \| null（列是 `varchar(500)`，写侧按**字符**截断到 500） |
| `sent_at` | naive timestamp |

⚠️ `POST /send` 的出参与 `/logs` 的行是**同一个结构体**（照 `mail_digest` 的既有做法，
wire 上不可区分）⇒ 前端只维护一份 schema。

## 3. 频率枚举与 cron 映射

`frequency` 四个预设（**没有**「每隔 N 小时」）：

| 频率 | 配套字段 | cron（**6 段带秒**） |
|---|---|---|
| `DAILY` | `send_hour` + `send_minute` | `0 {minute} {hour} * * *` |
| `WEEKLY` | + `weekday`（ISO 1=周一） | `0 {minute} {hour} * * {weekday - 1}` |
| `MONTHLY` | + `day_of_month`（1..28） | `0 {minute} {hour} {day_of_month} * *` |
| `MANUAL_ONLY` | — | **不产生 cron**（定时任务不注册 job） |

两条**必须知道**的坑：

1. **cron 是 6 段带秒**：`tokio-cron-scheduler` 0.15 内部 `croner` 固定
   `Seconds::Required`。写成常见的 5 段会在 `Job::new_async_tz` 里直接
   `ParseSchedule` 失败（好在那是同步返回的 `Err`）。
2. **`WEEKLY` 必减 1**：cron 第 6 段 `0`=周日，本域 `weekday` 是 ISO `1`=周一。
   漏减**不报错**，只会把推送安静挪到隔一天 —— 这是最难查的 bug。
   守卫：`schedule.rs::tests::weekly_cron_subtracts_one`。

时区**显式传** `chrono_tz::Asia::Shanghai`：`Job::new_async()` 的默认值是 **UTC** 而不是
本地时区（照 `mail_digest` 模块 doc 的实测结论）。`JobScheduler::new()` 在 0.15 是
**`async fn`**（故 `run()` 用 `tokio::sync::OnceCell` 承担异步初始化）。

⚠️ 本域的 `Frequency` 枚举**自有**，**不 import** `mail_digest::dto::Frequency`：两个域
各自演进，字面量相同但配套字段语义由各自的 `GET /meta` 下发，共用一个常量会在后端改
一侧语义时让另一侧静默漂移。

## 4. 口径表

### 4.1 报工 = **送检 + 放回**（不是「领取」）

事件集：`event_type IN ('RETURNED_TO_SHELF', 'SENT_TO_INSPECTION')`，窗口半开
`[today, today+1)`。

| 口径 | 表达式 | 理由 |
|---|---|---|
| `overview_batch_count` | `COUNT(DISTINCT batch_id)` | ① `COUNT(DISTINCT)` **忽略 NULL** ⇒ 件级事件不计入 —— 这正是「报工批次数」的正解；② 同一批次分两次放回折叠成 **1** 批（日报要「动了多少批」，不是「扫码按了多少次」）。同款判据在 `statistics` 域的「有效工时」维度上**逐字同源** |
| `overview_quantity` | `COALESCE(SUM(quantity), 0)` | ⚠️ **全仓从未算过这个口径**：`statistics` 域没有（它算工时 / 领取数），`wx` 域有一个近似但事件集是 `PICKED_UP + RETURNED_TO_SHELF` **且排除 `SENT_TO_INSPECTION`**（那边是为了算「经手件数」去重）⇒ 口径不同、不能用 |
| `completed_count` | `COUNT(DISTINCT part_id) WHERE event_type='COMPLETED' AND batch_id IS NULL` | 工单级事件（与 `statistics::count_completed` 同款判据） |
| `repair_part_count` | `COUNT(DISTINCT part_id) WHERE event_type='REPAIR_STARTED'` | 同上 |

**为什么不含 `PICKED_UP`**：那是「从货架**领取**」—— 工人把件拿走的那一刻，工序**还没
开始**。日报问的是「今天干了多少活」，答领取数等于把「拿起来」当成「做完了」。

⚠️ **⚠️ `worker_id` 在 2026-10-12 之前从未被写入过**（`shared/part_event/mod.rs` +
迁移 `20261011120000`）⇒ **该日期之前的日报「工人报工」恒为 0、第一节四个数字全为 0**。
**这不是 bug**，是数据事实。现场看到大面积 0 时不要当故障排查，先看事件表的写入时间。

⚠️ `JOIN t_worker w ON w.id = e.worker_id` 是 **INNER**（照 `statistics` 同款）：
`worker_id` 指向一个已消失的工人时，那条事件不该进任何工人维度的统计。连带后果见 §8.3
第 3 条（`completed` / `repair` 与统计域存在刻意差异）。

### 4.2 当前在制（第一节第三个数）

`t_part` where `deleted_at IS NULL AND status NOT IN ('COMPLETED','CANCELLED')`
**AND** `NOT EXISTS（件级 COMPLETED / CANCELLED 事件）`，**当前快照、非日窗**。

⚠️ **二选一已定：照抄 `statistics::repo::count_in_process_at`（含那条 `NOT EXISTS`）**。
理由：日报的首要属性是**可与统计页对账**，而那一次子查询的代价是 `t_part_event` 上的
一个索引扫描，在日频场景下可忽略。不抄能省一次全表扫，但遇到历史脏数据会与统计页不一致。

### 4.3 出货（第二节）

`t_delivery_note` where `deleted_at IS NULL AND picked_up_at` 落在 `[today, today+1)`；
件数 = `LEFT JOIN LATERAL (SELECT COUNT(*) FROM t_part_batch WHERE delivery_note_id = n.id
AND deleted_at IS NULL)` 的和。

- ⚠️ **不写 status 谓词**：流转是 `DRAFT → SUBMITTED → PICKED_UP → ARCHIVED`，
  `picked_up_at` 只在 `PICKED_UP` 迁移时被设置且此后**不清空** ⇒ 写死
  `status='PICKED_UP'` 会**漏掉已归档的单**，而那恰恰是「今天真正出了货」的那些。
  回归：`preview::delivery_section_counts_archived_notes_by_picked_up_at`。
- ⚠️ **`part_count` 不是 `t_delivery_note` 的列**：它是「该单名下**未软删批次**的条数」
  （照 `com/delivery_note/service/inner.rs` 的 `PartBatchRepo::list_by_delivery_note`）
  ⇒ 必须 `LATERAL` 一次数完（逐单查是功能正确但语句数线性涨的写法，源码护栏守它）。

### 4.4 交期窗口（第三节）

逾期 = `system_delivery_date < today`；未来 = `>= today AND < today + N`（`N =
upcoming_days`，clamp 1..60）。**窗口是半开的，「N 日内交期」含今天**（今天到期最该被
看见）。第三条**照抄** `mail_digest::repo::list_overdue` / `list_upcoming` 的全部谓词：

- 工单级行单位（`t_part` 侧带 `assembly_id IS NULL`，装配件单独一支 `UNION ALL`，其批次
  经子件两跳）；
- 6 态白名单 `PENDING / PROGRAMMING / IN_PROCESS / OUTSOURCE / INSPECTION /
  READY_TO_SHIP`；
- `NOT EXISTS（任一批次 `DELIVERED` / `COMPLETED`）` 已交量守卫，**含 `COMPLETED`**
  （force-complete 端点强推它；漏掉会让已完工的工单继续被算逾期）。

⚠️ **四处人工同步**（本域的 `repo::DELIVERY_STATUSES` 是**自有常量**，改一处必须同步
另三处）：本域常量 / `mail_digest::repo::DELIVERY_STATUSES` /
`dashboard::repo::delivery::DELIVERY_STATUSES` / 前端 `UpcomingDeliveryChart.vue` 的
`LAYERS[].statuses`。

### 4.5 工人榜与条形图（第四节）

`GROUP BY e.worker_id`，输出 `{worker_name, badge_code, batch_count, quantity}`。

- 事件集与 §4.1 逐字同源；`JOIN t_worker w ON w.id = e.worker_id AND w.deleted_at IS NULL`。
- ⚠️ **排除 `batch_count = 0 AND quantity = 0` 的工人**（`HAVING`）：统计页会为零报工工人
  **补零行**（那一页要的是「谁今天没干活」），日报只画条形图 —— 让零值行进来等于把条形
  图灌满空条，而日报**只**服务于「今天干了多少」这个问题。
- 排序在**内存**里做（SQL 不认识「按配置选指标」）：指标降序 → 姓名升序 → 工号升序
  （保证两次推送的行序不随机跳）。
- `worker_metric` 切换**条长依据**与**行内数值 / 标题口径**：`QUANTITY` ⇒ 按
  `quantity`、显示件数、标题「工人报工（送件数）」；`BATCH` ⇒ 按 `batch_count`、显示
  批次数、标题「工人报工（报工批次数）」。

### 4.6 四条 SQL 硬约束（改 SQL 前逐条读）

1. **`today` 是形参，SQL 里绝不写 `CURRENT_DATE`**：DB 会话时区（测试容器 UTC）与
   `infra::clock::now_naive()` 的 Asia/Shanghai 是**两个时钟**，Shanghai 00:00–08:00
   期间 `CURRENT_DATE == today - 1`，且**不报任何错**。
2. **窗口是半开 `[today, today+N)`**。
3. **出货查询不写 status 谓词**。
4. **`SUM(quantity)` 必须 `COALESCE(…, 0)`**：全 NULL 时 PG 返 NULL，而「全 NULL」就是
   「今天没有任何一条报工事件」—— 日报最常见的形态。

### 4.7 取数恒 ≤ 6 条语句

上界 = 第一节 2 条（`overview_events` + `in_process_count`）+ 第二节 1 条 +
第三节 2 条 + 第四节 1 条 = **6 条**。**关掉的节一条 SQL 都不发**（`service::fetch_rows`
的四个 `if cfg.include_*`），与工单数 / 送货单数 / 工人数**无关**。

⚠️ **节开关拦的是「四节全关 ⇒ `21804`」，不是「只能开一个」** —— 四节都开是缺省形态。
⚠️ **刻意不写死总数**：这类计数必然过期，错的那个数比没有更坏。上界是**推出来的**，
由 `fetch_rows` 的 doc + 源码护栏守住。

护栏：`tests/wecom_daily/n_plus_one_guard.rs`（两条：循环体内禁 `repo::` / `sqlx::query`
+ 顶层恰好 6 个调用点）。⚠️ **逐工单循环查是功能完全正确、集成测试照样全绿的写法**，
只有语句数会线性涨，所以只能靠源码护栏守（该文件头登记了 6 条已知盲区）。

## 5. 渲染与分片

### 5.1 为什么选 `markdown` **v1** 而不是 `markdown_v2`

| | `markdown`（v1） | `markdown_v2` |
|---|---|---|
| 表格 / 列表 | ❌ | ✅ |
| **字体颜色** | ✅ | ❌ |
| 客户端门槛 | 无 | **4.1.36+**（安卓 4.1.38+），**低版本退化为纯文本** |

选 v1 的两条理由：① **颜色是日报的核心信号**（逾期数 / 返修数橙红是这份日报最该被一眼
看到的东西），低版本退化成纯文本 ⇒ 整份日报在老客户端上变成一坨没格式的文本，**风险
不可接受**；② v1 牺牲的表格能力本域**本来也不需要**（日报是「几个数字 + 一张条形图」的
摘要，明细在邮件域与统计页）。

### 5.2 消息形态

```markdown
# 生产日报 · 2026-10-18

> 统计区间 <font color="comment">2026-10-18 00:00 ~ 23:59</font>

## 一、全厂报工总览
> 报工批次 <font color="info">128</font> 批 · 送检放回 <font color="info">456</font> 件
> 今日完工 <font color="info">37</font> 单 · 当前在制 <font color="info">92</font> 单
> 今日返修 <font color="warning">5</font> 单

## 二、今日出货
> 已取货送货单 <font color="warning">6</font> 单 · 合计 <font color="info">214</font> 件

## 三、逾期未交 / 3 日内交期
> 逾期未交 <font color="warning">12</font> 单 · 3 日内交期 <font color="info">7</font> 单

## 四、工人报工（送件数）
> 42 张三 ██████████
> 25 李四 ██████
> 3 王五 ██
```

- ⚠️ **关掉的节完全不出现**（不是「出现标题 + 0」）。
- `subject_template` 占位符**只认** `{date}` `{batch_count}` `{quantity}` `{note_count}`
  `{overdue_count}`，**未知占位符原样保留**（照 `mail_digest` 既有口径：管理员能一眼看出
  自己写错了什么）。空 ⇒ 后端默认串 `# 生产日报 · {date}`。
- ⚠️ **本域是 markdown 不是 HTML ⇒ 不做任何转义**：工人名里的 `<` / `>` 原样透传，
  `<font color=…>` 自身成对（守卫 `template.rs::tests::markdown_special_chars_are_not_swallowed`）。

### 5.3 字符条形图的三条排版口径

1. **条形放行尾、数值放行首**：`█`(U+2588) / `░`(U+2591) 在不同字体下是全宽还是比例
   宽**不可控**，放行尾则宽度抖动不可见；数值列的行首参差是可接受的排版噪声。
2. **姓名按「显示宽度」补空格**（CJK 计 2、ASCII 计 1）：否则 `张三`（宽 4）与 `王`
   （宽 2）起跑的条不在同一列。
3. ⚠️ **`markdown` v1 的行首连续空格会被当缩进吃掉** ⇒ 数值**不做右对齐补空格**，
   直接从行首写起。（代价：「条形起始列」随数值位数变化，所以守卫
   `bar_rows_are_display_width_aligned` 的 fixture 用**同宽数值**；另有
   `name_field_is_fixed_width_regardless_of_value_width` 单独守「姓名域定宽」这条真正
   的不变量。）

条长：`BAR_CELLS = 12`，满格数 `round(12 * v / max_v)`（`max_v == 0` 时全空条），
**恒占满 12 格**（不足补 `░`）⇒ 每行等宽。

### 5.4 分片：**不截断**

渲染器产出**块**列表，顺序固定
`Title → Overview → Delivery → Orders → WorkersHead → WorkerRow×k → Footer`。
⚠️ **`WorkerRow` 不可再拆**（它是一整行）⇒ **所有分片都发生在块边界上，永远不会把一行
劈成两半**（劈开会得到两条半个名字的工人，看着像数据坏了）。

**两趟贪心装填**：

- **pass 1**：逐块装入当前片，超则开新片。每片预算 = `4096 − RESERVE`，`RESERVE = 48`。
  ⚠️ **RESERVE 必须存在**：「（i/N）」要等装填完才知道 N（自指），不预留则「第 1 片刚好
  4090 字节、pass 2 追加后缀后 4095」会炸。⚠️ `String::len()` 就是 UTF-8 **字节数**；
  `<font color="info">…</font>` 每个约 22 字节，计入预算。
- **pass 2**：已知 N，为每片的 Title 块标题追加「（i/N）」，断言 `len() <= 4096`。
  ⚠️ **`N == 1` 时不追加后缀**（单条消息不该挂「（1/1）」）。
  ⚠️ **标题块在每一片都重复**（一份**副本**）—— 否则第 2 片在群里是几行孤零零的工人
  数据、没有日期上下文。

**没有「另有 N 人省略」的字节截断 footer。** `worker_top_n > 0` 时挡掉的人是**管理员
主动设的展示上限**，footer 文案必须能分辨：

```
> <font color="comment">另有 8 人在此省略（配置「最多列出工人」= 15）</font>
```

`worker_top_n == 0`（不限）时 footer **不出现**。

### 5.5 三个防御性分支（**一律 `21804`，绝不静默截断**）

| 情形 | 处置 |
|---|---|
| 单个块自身 > `4096 − RESERVE` | `21804`，错误消息**指明是哪一节**。当前最大块是 `WorkersHead`（~120 字节），但 `subject_template` 很长时是标题块 —— 这条是防御性的 |
| 装填结果片数 > `max_chunks` | `21804`，文案：「本次日报需要 4 条，但配置上限为 2；请调高「最多拆几条」或调低「最多列出工人」」 |
| `max_chunks > 20` | **保存配置时**即拒 `21804`（企业微信官方限频：每个消息推送 ≤ 20 条/分钟） |

### 5.6 部分投递语义

企业微信**没有批量接口**（一次只能推一条）：

| 规则 | 内容 |
|---|---|
| **顺序发送，绝不并发** | 群里必须先出现 1/N 再出现 2/N |
| **不重发已成功的片** | 重发会在群里出现重复内容，**比缺一半更糟**（运维也没有幂等键可凭）。这是刻意取舍 |
| 流水状态 | 任一片失败 ⇒ 整条 `status='FAILED'`，`error_message` 写「第 i/N 条失败：<errmsg>」 |
| `chunk_sent` | 记录**失败前**已成功的片数 |

⚠️ `deliver` 返回 `Err(21802)` 时**流水已经写了**，错误消息带 `log_id`（照 `mail_digest`
的做法）。⚠️ **`render` 阶段的 `21804`（超 `max_chunks`）不落流水** —— 没发出去，不是
投递失败。前端 `/send` 失败后正是靠「有没有流水行」区分这两类。

## 6. 移除记录

**无。** 全新域，无任何端点 / 表 / 迁移被移除。迁移
`migrations/20261018000000_001_wecom.sql` 建的三张表（`t_wecom_webhook` 属通道域 +
本域的 `t_wecom_daily_config` / `t_wecom_daily_log`）**全部在用**。

## 7. 与 WS 的关系

**零关系**。本域不产生、不消费任何 WS 事件，也不订阅 `AFFECTS_DASHBOARD` 那条白名单。
日报是**定时推送**（cron → HTTP → 企业微信），与大屏的实时链路完全独立。

## 8. 表依赖与前端配套

### 8.1 读 / 写的表

| 表 | 读 | 写 | 归属 |
|---|---|---|---|
| `t_wecom_daily_config` | ✅ | ✅ | 本域 |
| `t_wecom_daily_log` | ✅ | ✅ | 本域 |
| `t_wecom_webhook` | ✅（`webhook_row_exists` 判存在性） | ❌ | **wecom 通道域** |
| `t_part_event` | ✅（第一节 + 第四节） | ❌ | `prod` |
| `t_part` / `t_part_batch` | ✅（在制 / 交期 / 出货件数 / 批量列） | ❌ | `part` |
| `t_assembly` / `t_customer` | ✅（交期两支 `UNION ALL`） | ❌ | `assembly` / `com` |
| `t_delivery_note` | ✅（第二节） | ❌ | `com::delivery_note` |
| `t_worker` | ✅（两处 `JOIN`） | ❌ | `prod` |

⚠️ **别域表的张数刻意不写死**（加表就过期）。**域隔离护栏不适用**：本域读若干张别域表
（只读聚合是既定 pattern，同 `dashboard` / `statistics`），而内容域向通道域取推送通道是
**「通道 / 内容」这个切分的必然结果**（与 `mail_digest → scheduled_task` 同款），
护栏若装上会把**正确**的写法也拦掉。

### 8.2 前端配套

| 后端 | 前端 | 备注 |
|---|---|---|
| `/cron/wecom-daily/config`（21 键） | `views/wecom/daily/weComDailySchema.ts::weComDailyConfigSchema` | `.strict()` + `Object.keys().length === 21` |
| `/cron/wecom-daily/meta`（6 键） | `weComDailyMetaSchema` | 三个 range + 四个 `needs_*` **不许硬编码** |
| `/cron/wecom-daily/preview`（15 键） | `weComDailyPreviewSchema` | ⚠️ 无 `markdown` 单字段 |
| `/cron/wecom-daily/send` | `weComDailyLogSchema` | 与 `/logs` 的行同一个结构体 |
| `/cron/wecom-daily/logs`（19 键/行） | `weComDailyLogSchema` / `weComDailyLogListSchema` | |

**部署顺序**：后端必须先上（旧路径 404、无 alias，失败发生在**网络层**）。
⚠️ 前端兜底常量（`DEFAULT_WECOM_DAILY_FREQUENCY_LABELS` / `DEFAULT_WECOM_DAILY_WORKER_METRIC_OPTIONS`
/ `DEFAULT_WECOM_DAILY_RANGES`）只在 `meta` 还没到达时用，**meta 到达后一律以 meta 为准**
（两份兜底与后端的 label 文案措辞不同，见 §8.3 第 2 条）。

⚠️ **⚠️ 与前端的口径分叉登记**（前端**刻意**如此，不是漏做）：

1. **前端刻意不建「统计日」列** —— `report_date` 恒等于 `sent_at` 的日期（后端同一个
   `now_naive()` 取值）⇒ 一列是另一列的常量前缀，列表里放两列等于给同一事实两行。
2. **前端刻意不做日报 6 个数字的表格列**（`overview_batch_count` / `overview_quantity` /
   `note_count` / `note_part_count` / `overdue_count` / `upcoming_count`）—— 流水表存它们
   只是为了「事后复盘当时那份日报是什么数字」，而企业微信群里还看得到同样的数字；列表里
   再铺 6 列会把真正需要看的（状态 / 片数 / 通道 / 时间）挤成窄列。它们**在库里、在
   `GET /logs` 出参里都在**，只是不做表格列。

### 8.3 已知偏差登记

1. **迁移的 `worker_metric` 列注释写的是 `QUANTITY / HOURS`，本域实现的是
   `QUANTITY / BATCH`**。取值域的真源是 Rust 侧 `dto::WorkerMetric`（无 DB ENUM，未知值
   在**写侧**就被 `21804` 拒）⇒ 代码侧逐字生效，**迁移注释那一行是过时的**。
   为什么不是 `HOURS`：本域的口径里**没有工时**（有效工时要经工序链上配的
   `estimated_minutes`，跨域且跨工序链配置率，而日报要的是一个当场可算、与第一节可对账
   的量）。
   ⚠️ **为什么不去改那一行注释**（不是「规矩不许改」这种形式主义理由）：迁移是
   append-only，**改一个字都会让已 apply 过该迁移的库 checksum 失配** ⇒
   `sqlx migrate run` 直接失败、**进程起不来**。代价（一次不可逆的启动失败）与收益
   （一行过时注释）完全不成比例，所以偏差如实登记在**四处**（本条 / `dto.rs` 文件头 +
   `model.rs` 字段 doc / `CLAUDE.md`），而**真源只有一处**：`dto::WorkerMetric` +
   写侧 `21804` 拒未知值。
2. **前端 `DEFAULT_WECOM_DAILY_*` 兜底常量与后端 `GET /meta` 的 label 措辞不同**
   （「按件数」vs「送件数」、「每周（指定星期几）」vs「每周」）。真源是 meta，前端那份
   只在 meta 在飞时用 ⇒ 不是契约冲突，但**看到两者不一致不要以为哪个错了**。
3. **`completed_count` / `repair_part_count` 与 `statistics` 域存在刻意差异**：
   `statistics::count_completed` / `count_repair_parts` **没有 JOIN `t_worker`**，
   而本域为了「一条语句四个聚合」把 `JOIN t_worker w ON w.id = e.worker_id`（INNER）加
   在整条语句上 ⇒ `worker_id IS NULL` 或指向已删工人的事件在本域**不计入**这两个数字。
   连带后果：2026-10-12 之前的事件（`worker_id` 从未被写入）在本域第一节四个数字全为 0。
   详见 §4.1。
4. **`overview_quantity` 与「各工人 `quantity` 之和」在软删工人处会不等**：
   `overview_events` 的 `JOIN t_worker` **不带**软删闸门，`worker_rows` **带**
   （`w.deleted_at IS NULL`）⇒ 一个「当天有事件、但工人行已被软删」的行只进前者。
   `service::worker_quantity_total` 的 lib 单测把这个差值显式钉住（`7` 件那条用例）。
   处置：**如实登记、不偷偷对齐**（对齐哪一侧都会让那一侧与它的源域脱节）。
5. **`/send` 无频控**（唯一护栏是 MANAGER 角色 + `enabled` 闸 + `max_chunks ≤ 20`）。
   登记为后续增强。
6. **多副本部署不防重**：部署 N 个副本 ⇒ 同一时刻可能推 N 次（无 leader election）。
   与 `task::mail_digest` / `task::auto_complete` 同一现状，登记为独立重构。
   ⚠️ 与「进程内互斥」是**两层**问题：`task::wecom_daily::swap_job` 的
   `tokio::sync::Mutex` 保证**单进程内**调度器里恒 ≤ 1 个 job（否则两个并发
   `POST /config` 会各注册一个、先注册那个永远摘不掉）；多副本防重**仍需**分布式锁。
7. **两个调度器单例未抽公共注册表**：`task::mail_digest` 与 `task::wecom_daily` 各持一个
   独立的 `JobScheduler` + 单 job 槽。刻意如此（两个推送域各一个 job 是既有形态）；等出现
   **第三个**推送类型时再抽。
8. **🔒 主密钥缺口**：`MAIL_ACCOUNT_SECRET` 一换，`t_wecom_webhook` 里全部 webhook 密文
   **永久解不开**（AES-GCM 无轮换机制，「逐行解密 → 换密钥重新加密」的迁移流程未实现）。
   该缺口与 `t_mail_account` **完全同款**，两张表共用一把钥匙 ⇒ 是同一个缺口。表现是
   `21801`（通道解析失败）+ 一行 FAILED 流水。
9. **⚠️ 覆盖空洞：`/send` 的成功路径无集成测试**（`status='SUCCESS'` 的流水行 /
   真实投递）—— 测试环境没有真实可达的群机器人地址，且本仓**刻意不引入 webhook 的
   mock transport**（那会为了一条 happy-path 给生产依赖树加一层测试专用构造）。
   替代覆盖：`/preview` 覆盖渲染与分片（含多片、字节预算、页码后缀）、「指向不可达 key」
   路径覆盖落流水与部分失败、`wecom::sender::classify` 的 lib 单测覆盖 errcode 解析。
   要补它需要一个**真实群机器人地址**作为测试夹具。
10. **⚠️ 请求超时优先于落流水**：`timeout_middleware` 是 `tokio::time::timeout` ——
    到点把 handler 的 future **整个 drop 掉**，所以「请求级超时」永远**优先于**
    「落流水」。`POST /cron/wecom-daily/send` 最坏 ≈ 100s（`max_chunks` 上界 20 ×
    单片 5s），为此本域已把 `/send` 放进
    `middleware::timeout::is_long_timeout_path` 的**长档白名单**（660s，与批量打印共用
    一档）⇒ **正常路径下「失败也落流水」成立**。⚠️ 但**已知残留缺口**：整链超出该档时
    （数据库本身不可用、上游网关比 660s 更早切断连接），那一行流水**仍然写不出来**，
    `GET /logs` 里查不到痕迹。这不是「已完全解决」，是「把触发概率压到可忽略」的处置；
    要彻底消除它得改成分片投递与流水落库**解耦**（后台任务补写流水），那是独立重构。
    护栏：`tests/wecom_daily/timeout_guard.rs` + `middleware::timeout` 的 lib 单测。
