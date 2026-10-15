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
> ## 域隔离：内容域 → 通道域 + 图纸域，**两条都单向**
>
> 本域 import 两个域，**反向都零代码依赖**（§8.1）：
>
> | 域 | 本域取什么 | 反向 |
> |---|---|---|
> | `wecom`（通道域） | `service::resolve_target` / `service::target_is_available` / `sender::send_markdown` | 零代码依赖；唯一触及是 `wecom::repo` 里一条「查有没有配置在用我」的 COUNT SQL，以及本域调 `wecom::repo::get_webhook` 取 `ConfigOut.webhook_configured`（§2.1） |
> | `files`（图纸域） | 第三节清单的**图纸预览直链**：`preview_service::ensure_ready_for_parts(part_ids, budget)` | 零（`t_file_preview` 的唯一写方是预览状态机，它不知道本域存在） |
>
> ⚠️ 本域**不解密**任何东西：webhook URL 的解密在通道域内完成（明文只在调用栈里活着）。
>
> ## 取数 SQL **零他域符号引用**（图纸那一环除外，理由见 §4.6）
>
> 统计与清单 SQL 完全写在本域（`repo.rs`），**不 import** `statistics` / `mail_digest` /
> `dashboard` 的任何符号（用户 2026-10-16 定的硬要求「在推送通知的域重写 SQL，不要复用
> 其它域的」）。代价：「未交付」状态白名单的人工同步点见 §4.5。
> ⚠️ **图纸链路的 SQL 不在本域** —— `t_part_file` / `t_file_preview` 的读与
> 内容寻址种子全在 `files` 域的批量入口里；本域只交出「第三节清单涉及哪些 `part_id`」
> 这一个入参（§4.6）。

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
| `POST` | `/api/v2/cron/wecom-daily/preview` | 渲染分片。**不发送、不落流水**。空体 `{}` = 全用当前配置。⚠️ **超 `max_chunks` ⇒ `21804`，绝不截断**。⚠️ 图纸直链只取**已就绪**的（零 CI 提交，§4.6） |
| `POST` | `/api/v2/cron/wecom-daily/send` | 真发 + 落流水（**成败都落**）。**顺序**逐片发送。⚠️ 发送前有一段**最长 8 分钟**的图纸就绪等待（§4.6） |
| `GET` | `/api/v2/cron/wecom-daily/logs` | 分页，`sent_at DESC, id DESC`，`limit` clamp `[1,200]` 缺省 20，`offset` clamp `>=0`，`total` 不受 limit 截断 |

⚠️ **5 个端点 / 6 条路由**：`/config` 读与写是两条路由（前端契约里它们是两个语义完全
不同的动作）。`ROUTES` 列 6 条。

**硬切记录**：全新域，无旧路径、无 alias。⚠️ **部署顺序：后端必须先上** —— 旧路径 404
且失败发生在**网络层**（不是契约层），现场只看到一句 axios 错误。

**动词纪律**：只用 GET / POST（全仓零 PUT/PATCH/DELETE）。回归用例
`tests/wecom_daily/config.rs::put_patch_delete_verbs_are_not_routed` 钉住（405）。

## 2. 逐字段

### 2.1 `ConfigOut`（`GET/POST /config`，**23 个字段**）

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
| `order_list_limit` | number | 第三节**每个交期组**最多列出多少工单，缺省 30，**0 = 不限**，范围 0..200。⚠️ 截断在 Rust 侧做，组标题的「N 条」恒是**未截断**的匹配总数（§4.5） |
| `max_chunks` | number | 单次最多发几条，缺省 **6**，范围 **1..20**（20 = 企业微信官方限频） |
| `include_overview` / `include_delivery` / `include_orders` / `include_workers` | bool | 四节开关，缺省全 true。⚠️ **四节全关 ⇒ `21804`** |
| `include_drawings` | bool | 第三节清单是否带图纸预览直链，缺省 `true`。⚠️ **`false` ⇒ 图纸环节整段跳过、一条 SQL 都不发、不触发 CI**（CI 按页计费，§4.6） |
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

形状 = 上面 23 个字段**去掉** `id` / `version` 的新增 / 时间戳 / `webhook_configured`，
**加上** `version`（必填）。

⚠️ `version` **必填、无 `#[serde(default)]`**：缺失时 axum `Json` 提取器返 **HTTP 422
纯文本**，不是业务信封（回归用例 `config::missing_version_is_422_plain_text`）。

⚠️ **与写侧校验的两处刻意不对称**（都照 `mail_digest` 既有口径）：
- `upcoming_days` / `order_list_limit` **clamp**（`1..60` / `0..200` —— 这两个数只影响窗口
  宽窄 / 清单长度，越界时夹到边界比报错更符合「用户想表达的意思」）；
- `worker_top_n` / `max_chunks` / `worker_metric` / 频率配套字段 / 四节全关 **一律拒**
  （`21804`）。

⚠️ 同一批字段在 **`POST /preview` / `POST /send` 的覆盖路径**上改口径：`upcoming_days` 与
`order_list_limit` 走**拒**（`21804`）而不是 clamp —— 覆盖值是管理员在预览框里现填的，
夹到边界会让预览结果与他的输入不符（他填 -5，看到的却是「未来 1 天」）。

⚠️ `webhook_id` 指向不存在**或已软删**的 webhook ⇒ `21806`（写侧校验，不等到推送时才发现）。

### 2.3 `PreviewRequest`（`POST /preview` 与 `POST /send` 共用）

配置字段的**部分覆盖**，全部键可选（空体 `{}` = 完全用当前配置）：

| 覆盖项 | 取值域 | 覆盖路径的校验 |
|---|---|---|
| `order_list_limit` | `0..200`（`0` = 不限） | 越界 ⇒ `21804`（**不 clamp**，见 §2.2 的不对称说明） |
| `include_drawings` | bool | 无 |
| 其余各配置项（见 §2.1） | 同 §2.1 | 同 §2.2 |

⚠️ **不含 `version`**（这两个端点不写配置）；⚠️ **不含 `webhook_id`**：「推到哪个群」是
一条**配置**决策，不该被「预览一次」顺带改掉（否则 `/send` 的覆盖就成了绕过配置页直接
换通道的后门）。serde 默认忽略未知键 ⇒ 前端带上它也无妨。
覆盖值**重新跑一遍完整校验**。

⚠️ `order_list_limit` 与 `include_drawings` 之所以值得覆盖：清单每行带直链约多 95 字节，
而这两个开关直接决定分片数会不会撞 `max_chunks` ⇒ 管理员必须能在**不动落库配置**的
前提下对比两种形态。

### 2.4 `MetaOut`（`GET /meta`，**6 个键**）

| 键 | 内容 |
|---|---|
| `frequencies[]` | `{value, label, needs_time, needs_weekday, needs_day_of_month}` × 4 |
| `worker_metrics[]` | `{value, label}` × 2（`QUANTITY`「送件数」/ `BATCH`「报工批次数」） |
| `defaults` | 与 `ConfigOut` 同构（含 `order_list_limit` 缺省 30 / `include_drawings` 缺省 true），去掉 `id`/`version`/两个时间戳/`webhook_configured`；`webhook_id` 恒 `null` |
| `upcoming_days_range` | `{min: 1, max: 60}` |
| `worker_top_n_range` | `{min: 0, max: 200}`（0 = 不限） |
| `max_chunks_range` | `{min: 1, max: 20}` |

⚠️ **三个 range 与四个 `needs_*` 全部由本端下发，前端不许硬编码**。
⚠️ **`order_list_limit` 的取值域没有单独的 range 键**（本端仍是 **6 个键**）：它的区间
`0..200` 写在 §2.1 与 `POST /config` 的 clamp 行为里，表单校验按那一份实现即可。要把它
也下发成第 7 个键会改 `GET /meta` 的 wire 契约（前端 `.strict()` schema 同步），当前
不构成阻塞 —— 登记见 §8.3。

### 2.5 `PreviewOut`（`POST /preview`，**13 个字段**）

| 字段 | 说明 |
|---|---|
| `subject` | 渲染后的标题（**不含**分页后缀；后缀只进各片的 Title 块） |
| `chunks[]` | 将按顺序发送的 N 条，逐条 ≤ 4096 **字节** |
| `chunk_count` | 冗余字段，恒等于 `chunks.len()` |
| `overview_batch_count` / `overview_quantity` | 第一节：报工批次数 / 送检放回件数 |
| `completed_count` / `in_process_count` | 第一节：本期完工 / 当前在制。⚠️ `completed_count` **不进正文**（保留在出参里是为了让管理员在预览弹窗核对口径） |
| `note_count` | 第二节**唯一**的数字：本期出货的**去重工单数**（§4.4）。⚠️ **2026-10-23 起语义变更**：此前本键恒 0、真正的数字在 `note_part_count`（那时本键指的是「送货单张数」）；`note_part_count` 那一键已随 DB 列一并下线 |
| `overdue_count` / `upcoming_count` | 第三节：**未截断**的逾期工单数 / 未来 N 日内交期工单数（与组标题「N 条」同源，§4.5） |
| `worker_rows[]` | `{worker_name, badge_code, batch_count, quantity}` —— **两条指标都返回**，不受 `worker_metric` 影响 |
| `worker_row_total` | **全量**工人行数（可能 > `chunks` 里出现的条数） |
| `sections[]` | 本次实际生效的节：`overview` / `delivery` / `orders` / `workers` |

⚠️ **契约里没有 `markdown` 单字段** —— 只维护一个真相源。同时维护 `markdown` 与
`chunks` 会让「要发几条」有两个答案，而那恰恰是管理员唯一需要预判的东西。

### 2.6 流水行（`SendResultOut` = `LogListOut.items[]` 的元素，**18 个字段**）

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
| `overview_batch_count` / `overview_quantity` / `note_count` / `overdue_count` / `upcoming_count` / `worker_row_total` | 各节计数快照。⚠️ `note_count` 与 `PreviewOut.note_count` **同源同义**（2026-10-23 起承载第二节那个数；**更早的历史行里它记的是「送货单张数」**，见 §8.3） |
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

### 4.1 统计窗口：**跨天**的 `[前一天 @ 推送时刻, 当天 @ 推送时刻)`

第一节 / 第二节 / 第四节走**时间戳**窗口，第三节走**日历日**窗口 —— 两套窗口由同一份
配置的同一个值派生，**不要互串**。

| 项 | 内容 |
|---|---|
| 两端 | `(from_ts, to_ts) = (前一天 @ 锚点, 当天 @ 锚点)`，**半开** `[from_ts, to_ts)` |
| 锚点 | `send_hour` / `send_minute`（**推送时刻本身**），缺省 **20:05** |
| 兜底 | 任一分量为 `NULL` 或越界 ⇒ **各自独立**回退到 `20:05`（不 clamp —— 库里可能躺着 `99`，夹成 `23:59` 会得到一个看起来合法、实际统计了完全不同一段时间的窗口） |
| 第三节的 `today` | `to_ts.date()`，与窗口上界**同一个值** ⇒ 一次推送里不会出现两个「发送日」 |

**为什么窗口跨天**：日报回答的是「**这一班**干了多少」，而一班横跨两天（下午下的料、
第二天上午才做完）。按日历日切 ⇒ 昨天推送出去的那条永远缺最后一班的量，而读者恰恰是
当天晚上看昨天那一班结论的人。

⚠️ **SQL 里绝不写 `CURRENT_DATE`**：DB 会话时区（测试容器 UTC）与
`infra::clock::now_naive()` 的 Asia/Shanghai 是**两个时钟**，Shanghai 00:00–08:00 期间
`CURRENT_DATE == today - 1`，窗口整体偏移一天且**不报任何错**。

### 4.2 第一节：全厂报工总览（**一条语句**五个聚合）

事件集 `event_type IN ('RETURNED_TO_SHELF', 'SENT_TO_INSPECTION')`，窗口 §4.1。
`JOIN t_worker w ON w.id = e.worker_id` 是 **INNER**（照 `statistics` 的「有效工时」维度
同款）：`worker_id` 指向一个已消失的工人时，那条事件不该进任何工人维度的统计。

| 口径 | 表达式 | 进正文 | 理由 |
|---|---|---|---|
| `overview_batch_count` | `COUNT(DISTINCT CASE WHEN … THEN e.batch_id END)` | ✅ | `COUNT(DISTINCT)` **忽略 NULL** ⇒ 件级事件不计入；同一批次分两次放回折叠成 **1** 批（日报要「动了多少批」，不是「扫码按了多少次」） |
| `overview_quantity` | `COALESCE(SUM(CASE WHEN … THEN e.quantity END), 0)` | ✅ | ⚠️ **全仓从未算过这个口径**：`statistics` 没有（它算工时 / 领取数），`wx` 域有个近似但事件集是 `PICKED_UP + RETURNED_TO_SHELF` 且**排除** `SENT_TO_INSPECTION` ⇒ 口径不同、不能用 |
| `sent_batch_count` | `COUNT(DISTINCT CASE WHEN event_type='SENT_TO_INSPECTION' THEN batch_id END)` | ✅ | 工人**主动送检**（「这一批做完了、交给品检了」）≠ 放回货架。同样按 `batch_id` 去重，否则「送检 3 批」在只送过 2 批的现场是追责用的假数字 |
| `completed_count` | `COUNT(DISTINCT part_id) WHERE event_type='COMPLETED' AND batch_id IS NULL` | ❌ | 工单级事件。保留在出参里是为了让管理员在预览弹窗核对口径 |
| `in_process_count` | `COUNT(*) FROM t_part WHERE deleted_at IS NULL AND status='IN_PROCESS'` | ✅ | ⚠️ **只看派生列**、当前快照、**不受窗口约束**，见 §4.3 |

**为什么不含 `PICKED_UP`**：那是「从货架**领取**」—— 工人把件拿走的那一刻工序**还没开始**。
日报问的是「这一班干了多少活」，答领取数等于把「拿起来」当成「做完了」。同款判据在
`statistics` 域的「有效工时」维度上**逐字同源**。

⚠️ **`SUM(quantity)` 必须 `COALESCE(…, 0)`**：全 NULL 时 PG 返回 NULL，而「全 NULL」就是
「本期没有任何一条报工事件」—— 日报最常见的形态。

### 4.3 当前在制（第一节第三个数）

`COUNT(*) FROM t_part WHERE deleted_at IS NULL AND status = 'IN_PROCESS'`，**当前快照、
不受窗口约束**。

⚠️ **只判派生列、不带那道「件级 COMPLETED / CANCELLED 事件」守卫**，这与统计页
`statistics::repo::count_in_process_at` 的口径**刻意不同**。理由：日报要的是「**此刻**手上
还有多少活」，那是当前快照而不是本期统计量；判定就该与大屏等其它页面用同一个列 ——
同一个列才不会在三个页面上给出三个数。代价是 `t_part.status` 是 min-progress **派生缓存**、
存在滞后窗口（刚完工的件在派生列翻过来之前仍算在制）⇒ 两个页面可能对不上，差的就是那些
「派生列忘了翻」的脏行。登记见 §8.3。

### 4.4 第二节：本期出货（**一个数字**）

`COUNT(DISTINCT b.part_id)`，`t_delivery_note` JOIN `t_part_batch ON b.delivery_note_id = n.id`
且 `b.deleted_at IS NULL`，`n.picked_up_at` 落在 §4.1 的窗口内。

- ⚠️ **不写 status 谓词**：流转是 `DRAFT → SUBMITTED → PICKED_UP → ARCHIVED`，
  `picked_up_at` 只在 `PICKED_UP` 迁移时被设置且此后**不清空** ⇒ 写死
  `status='PICKED_UP'` 会**漏掉已归档的单**，而那恰恰是「真正出了货」的那些。
  回归：`preview::delivery_section_counts_archived_notes_by_picked_up_at`。
- ⚠️ **按 `part_id` 去重**：一张送货单里同一工单挂多个批次（一次送货装走同一工单的两个
  批次）时，按批次计会把一张单报成两单，管理员按这个数字去核单会对不上。
- ⚠️ **工单不是送货单的列**，要经 `t_part_batch` 才拿得到 ⇒ 必须 JOIN（逐单循环查是功能
  完全正确但语句数线性涨的写法，源码护栏守它）。

⚠️ **本节只有一个数字**（`note_count`）：出参、流水列、标题占位符 `{note_count}` 四处
同源。**送货单张数**这个口径已下线，DB 列 `note_part_count` 一并删除（迁移
`20261023010000_001`）—— 详见 §8.3。

### 4.5 第三节：按交期分组的工单清单（**7 条语句**）

清单的**行单位是工单级**：散件工单一条、装配件一条；装配件的子件以**子行**挂在父行下面
（渲染前缀 `└`），**不**重复计入行数 —— 否则一个 30 子件的装配件会把「逾期 12 单」撑成
41 单。

| 谓词 | 内容 |
|---|---|
| 逾期组 | `system_delivery_date < today`，**无上界** |
| 日期组 | `today <= system_delivery_date < today + upcoming_days`（**半开，含今天** —— 今天到期最该被看见，`upcoming_days=3` 即共 3 天） |
| 状态白名单 | **品检前 4 态**：`PENDING` / `PROGRAMMING` / `IN_PROCESS` / `OUTSOURCE`（`shared::delivery_status::RED_TIER_STATUSES`） |
| 已交量守卫 | `NOT EXISTS（任一批次 DELIVERED / COMPLETED）` —— **含 `COMPLETED`**（force-complete 端点强推它，漏掉会让已完工的工单继续被算逾期） |
| 装配件那一支 | `t_assembly` 同款谓词，已交量守卫经 `c.assembly_id = a.id` 扫它**全部**子件的批次 ⇒ **装配件能进清单，就意味着它的子件一件都没被交过** |

⚠️ **为什么取 4 态而不是大屏那份 6 态**：本节答的是「**品检还没开始的活**里哪些要催」——
`INSPECTION`（已进品检）与 `READY_TO_SHIP`（品检完待发货）各有自己的下一动作，混进来
会让「该催谁」这个唯一的问题得到一个答非所问的清单。**同一套谓词、不同的状态集**是有意
分叉，不是漂移。

⚠️ **状态白名单必须共用一份**：三域各存一份副本时，「加一个状态」漏改一处不会编译失败，
只会让日报 / 邮件 / 大屏三处的「未交付」判定互相矛盾且无单测变红。真源在
`shared::delivery_status::RED_TIER_STATUSES`；**人工同步点只有 2 处**（§8.3 第 1 条）。

**清单链路的 7 条语句**（与工单数 / 子件数 / 装配件数 / 货架数**无关**，入参为空集时跳过）：

| # | 语句 | 入参形状 |
|---|---|---|
| 1 | 逾期主查询 | `today`（照发，它自己出 `total = 0`） |
| 2 | 未来主查询 | `today` + `upcoming_days`（照发） |
| 3 | 装配件子件展开 | 清单里装配件的 id |
| 4 | 活跃批次 | 散件工单 id + 全部子件 id（**不含装配件 id**，见下） |
| 5 | `t_shelf.code` | 目标批次里 `PRODUCTION_SHELF` / `INSPECTION_SHELF` 的 holder id |
| 6 | `t_worker.name` | 目标批次里 `WORKER` 的 holder id |
| 7 | `t_outsource_company.name` | 目标批次里 `OUTSOURCE_COMPANY` 的 holder id |

⚠️ **位置只对 `t_part` 侧的 id 解析，装配件行恒不显示位置**：`t_part_batch.part_id` 指向
的是 **`t_part`**（批次挂在零件上），而装配件在 `t_assembly` 里、`t_part.assembly_id` 指的
是它的子件 ⇒ **装配件自己没有批次**。「这套装配件现在在哪儿」由**子件行逐条**回答（这正是
清单要展开子件的原因）。

**位置怎么派生**（判据逐字照抄 `part::service::list_enrichment`，不许本域自己发明）：
① 排除 `CANCELLED`；一条不剩则退回全部批次；② 再排除 `COMPLETED`；仍不剩则退回第 ① 步的
集合；③ 取 `shared::batch::status_machine::part_status_progress` **最小**的那批，并列取
`batch_no` 最小。`None` = 无活跃批次 ⇒ 渲染层**整项不显示**位置（不留悬空分隔符）。

⚠️ **`order_list_limit` 的截断在 Rust 侧做，SQL 不加 `LIMIT`**：组标题的「N 条」必须是
**未截断的匹配总数**（`COUNT(*) OVER ()`）；SQL 侧加 `LIMIT` 会让某个日期组的条数与
`total` 一起失守、组标题开始说假话。代价是「逾期几千单」的现场要拉回几千行 —— 渲染仍然
只出 `order_list_limit` 行，被挡掉的部分由组末脚注「另有 N 条未列出」交代。

⚠️ **空清单只出小标题**，不出组标题、不出「无」行：「今天没有要催的工单」本身就是好消息，
每天在日报里加一行「无」是往好消息上刷噪音。管理员仍能从 `/preview` 的 `sections[]` 看到
这一节是开着的。

### 4.6 图纸预览直链（第三节，`include_drawings` 打开时）

每个工单行的名称渲成 `[名称](直链)`，直链是 `files` 域 COS CI 预览的**第 1 页**：

```text
{preview_base_url}/{preview_prefix}/{preview_seed}/p1.jpg?eo-img.resize=l/2560
```

| 环节 | 内容 |
|---|---|
| 入口 | `files::part_file::preview::service::PreviewService::ensure_ready_for_parts(part_ids, budget)`。本域只交出「清单涉及哪些 `part_id`」，**不碰** `t_part_file` / `t_file_preview` / COS / python 任何一样 |
| 入参 | **本次会被列出的**那些行的 `part_id`（**截断之后**的集合，见本节末尾） |
| 每份取哪张 | 该 `part_id` 的**最新一张** `kind='DRAWING' AND file_type='PDF'` 的文件（`(created_at, id)` 最大，`id` 是同秒 tiebreaker） |
| 状态机 | 与 `GET /objects/{id}/preview` **完全同一份**（两道并发闸 / 自愈重置 / 四种终局失败 / `Success` 但 0 页仍当进行中） |
| 只取第 1 页 | 形态决定：日报每行只有一个链接位，企业微信 markdown 渲染不了多图。翻页走 `GET /objects/{id}/preview` |
| 缺键 | 没有 PDF 图纸 / 预算耗尽 / CI 终局失败 / 打 python 失败 ⇒ **该 key 不返回**，渲染成**纯文字名称**。⚠️ **绝不抛错**：一份日报因为一张图没转出来就不该发不出去 |

⚠️ **预算是上限不是承诺**（`DRAWING_CI_BUDGET_SECS = 480`）：

```text
480s(CI 就绪) + max_chunks(6) × 5s(逐片顺序发送) = 510s < 660s（长档超时）
```

余量 150s；`max_chunks` 调到上限 20 时最坏仍是 480 + 100 = 580s ⇒ **与分片数无关地安全**。
超时的几行不挂链接，其余正常挂。

⚠️ **`/preview` 传 `Duration::ZERO` 预算**：只取**已经 READY** 的那些、**零 python 往返**
—— 管理端点不该因为点了「预览日报」就提交 CI job（按页计费），也不该让请求挂在 python 上
等几分钟。

⚠️ **`include_drawings = false` ⇒ 整段跳过、一条 SQL 都不发**：这是那个配置项存在的意义
（关掉就不触发 CI、不花钱）。判断必须落在**调用 `files` 域入口之前** —— 放在它之后就是
「先花完钱再决定不给链接」。

⚠️ **只为「本次真的会被列出来的行」提交 CI**：入参是**截断之后**的行集合，不是未截断的
匹配集。`order_list_limit` 挡掉的那些行一个字符都不会出现在消息里，为它们跑 CI 是纯浪费
（逾期几千单时那是几千次按页计费的提交）。这条与上面那条是同一条纪律的两个刻度。

⚠️ **这一环不持 DB 连接**：它可能跑满 8 分钟，握着 `PoolConnection` 等 CI 会把连接池占满、
读端点跟着排队。故它在 `fetch_rows` 的连接**归还之后**执行，并由 `files` 域的入口自己短借
短还。

⚠️⚠️ **路径里绝不带文件名**（跨域安全条款，`files::part_file::preview` 的模块 doc 有整段
说明）：`t_part_file.object_key` 是 `{sha16}_{原文件名}` 而文件名就是**图号**、可枚举；
预览前缀是 public-read，把图号拼进路径等于**公开整张厂的图号清单**。路径成分只有
`<prefix>/<seed>/p{n}.jpg`，`seed` 是纯哈希（内容哈希 + 渲染配方版本，配方换代即换代）。
回归：`tests/files/preview.rs::ensure_ready_for_parts_zero_budget_is_read_only`。

### 4.7 第四节：工人榜与条形图

`GROUP BY e.worker_id`，输出 `{worker_name, badge_code, batch_count, quantity}`。

- 事件集与 §4.2 逐字同源；`JOIN t_worker w ON w.id = e.worker_id AND w.deleted_at IS NULL`
  （⚠️ **带**软删闸门，而 §4.2 的那条 JOIN **不带** ⇒ 差值登记在 §8.3）。
- ⚠️ **排除 `batch_count = 0 AND quantity = 0` 的工人**（`HAVING`）：统计页会为零报工工人
  **补零行**（那一页要的是「谁今天没干活」），日报只画条形图 —— 让零值行进来等于把条形图
  灌满空条，而日报**只**服务于「本期干了多少」这个问题。
- 排序在**内存**里做（SQL 不认识「按配置选指标」）：指标降序 → 姓名升序 → 工号升序
  （保证两次推送的行序不随机跳）。
- `worker_metric` 切换**条长依据**与**行内数值 / 标题口径**：`QUANTITY` ⇒ 按 `quantity`、
  显示件数、标题「工人报工（送件数）」；`BATCH` ⇒ 按 `batch_count`、显示批次数、标题
  「工人报工（报工批次数）」。

⚠️ **`worker_id` 在 2026-10-12 之前从未被写入过**（`shared/part_event/mod.rs` + 迁移
`20261011120000`）⇒ **该日期之前的日报「工人报工」恒为 0**。这不是 bug，是数据事实；现场
看到大面积 0 时不要当故障排查，先看事件表的写入时间。

### 4.8 SQL 硬约束（改 SQL 前逐条读）

1. **`today` / 窗口两端是形参，SQL 里绝不写 `CURRENT_DATE`**（理由见 §4.1）。
2. **两套窗口不要互串**：第一 / 二 / 四节走**时间戳**窗口，第三节走**日历日**窗口
   （交期列本身是 `date`）。
3. **出货查询不写 status 谓词**（理由见 §4.4）。
4. **`SUM(quantity)` 必须 `COALESCE(…, 0)`**（理由见 §4.2）。

### 4.9 取数恒 **O(1)**（语句数与行数无关）

第一节 1 条、第二节 1 条、第四节 1 个 `GROUP BY`、第三节 7 条（§4.5 的表）；
**关掉的节一条 SQL 都不发**。图纸那一环（§4.6）是**固定 3 条语句 + 每份待推进者每格至多
一次 python 往返**，语句数同样与工单数无关。

⚠️ **刻意不钉死具体条数**：这类计数必然过期。护栏守的是「循环体内不许出现取数调用」那条
东西本身 —— `tests/wecom_daily/n_plus_one_guard.rs` 扫 `service.rs` 全部生产代码的循环体，
命中 `repo::` / `sqlx::query*` 即失败（该文件头登记了它的已知盲区：迭代器链、裸调用、
并发扇出等）。

## 5. 渲染与分片

### 5.1 为什么选 `markdown` **v1** 而不是 `markdown_v2`

| | `markdown`（v1） | `markdown_v2` |
|---|---|---|
| 表格 / 列表 | ❌ | ✅ |
| **字体颜色** | ✅ | ❌ |
| 客户端门槛 | 无 | **4.1.36+**（安卓 4.1.38+），**低版本退化为纯文本** |

选 v1 的两条理由：① **颜色是日报的核心信号**（统计区间的灰、清单脚注的灰、节内数字的
蓝是这份日报最该被一眼区分的东西），低版本退化成纯文本 ⇒ 整份日报在老客户端上变成一坨
没格式的文本，**风险不可接受**；② v1 牺牲的表格能力本域**本来也不需要**（日报是
「几个数字 + 一份清单 + 一张条形图」，明细在邮件域与统计页）。

⚠️ **链接是 v1 的必要能力而不是加分项**：第三节清单行的图纸直链就是 `[名称](URL)`，
而 `markdown_v2` 虽然也支持链接，但它不支持颜色 ⇒ 两者只能取其一，取了 v2 就失去颜色。

### 5.2 消息形态

```markdown
# 生产日报 · 2026-10-18

> 统计区间 <font color="comment">10-17 20:05 ~ 10-18 20:05</font>

## 一、全厂报工总览
> 报工：<font color="info">128</font> 批  共 <font color="info">456</font> 件
> 送检 <font color="info">86</font> 批  在制 <font color="info">92</font> 单

## 二、本期出货
> 出货：<font color="info">214</font> 单

## 三、逾期未交 / 3 日内交期
> **逾期未交 · 12 条**
> F1001 [名称甲](https://cos.hsh-erp.cloud/previews/ba6ad9c78280e31bc23ef746e78e27d8/p1.jpg?eo-img.resize=l/2560) ×2  工人:张三
>   └ F1003 [子件丙](https://cos.hsh-erp.cloud/previews/1f0c…/p1.jpg?eo-img.resize=l/2560) ×4  品检架:B-11
> **2026-10-19 · 7 条**
> F1005 名称乙 ×1  生产架:A-03
> <font color="comment">另有 18 条未列出</font>

## 四、工人报工（送件数）
> 42 张三 ██████████
> 25 李四 ██████
> 3 王五 ██
> <font color="comment">另有 8 人在此省略（配置「最多列出工人」= 15）</font>
```

- ⚠️ **关掉的节完全不出现**（不是「出现标题 + 0」）。第三节一条工单都没有时**只出小标题**。
- ⚠️ 统计区间那行是**跨天**窗口（§4.1），写成「今日 00:00 ~ 23:59」必然是假的；读者唯一
  能核对的就是这一行，它错了整份日报的可信度就没了。
- 第三节行内四段的缺省规则：**序列号**为 `NULL` 时字段连同分隔空格一起去掉；**图纸直链**
  缺键时名称渲成**纯文字**（不留空 `[]()`）；**位置**取不到时整项不显示（不留悬空分隔符）。
- ⚠️ 标题模板占位符 `{note_count}` 喂的是第二节那个数（本期出货去重工单数）；**未知占位符
  原样保留**（照 `mail_digest` 既有口径：管理员能一眼看出自己写错了什么）。空 ⇒ 后端默认串
  `# 生产日报 · {date}`。
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
`Title → Overview → Delivery → OrdersHead → OrderLine×k → OrderFooter… → WorkersHead →
WorkerRow×k → Footer`。

⚠️ **两套粒度，刻意相反**：

| | 粒度 | 为什么 |
|---|---|---|
| **工人行** | 一个 `WorkerRow` = 一个块，**不可拆** | 劈开得到两条半个名字的工人，看着像数据坏了 |
| **第三节清单行** | **一行一个块，逐行可拆** | 一行装配件带 30 个子件就是 31 个渲染行；若「一组一个块」不可拆，那 31 行会变成 ≈4.2 KB 的单块 —— **超过单条消息预算**，于是「一个装配件就能让整份日报渲染失败」（`21804`），而现场看到的现象只是「今天装配件特别多」 |

可拆与不可拆的**边界**（逐条钉住）：

| 不可拆（同一个块内） | 可拆（块边界上断开） | 后果 |
|---|---|---|
| 第三节小标题 + **第一组**的组标题 + 该组首行 | 该组其余行 | 每片至多以「组标题 + 首行」开头，不会出现只有标题没有内容的片 |
| 后续每组：组标题 + 该组首行 | 该组其余行 | 组内某片可能以若干**子件行**开头，而它们的装配件行在上一片 |
| 组末脚注「另有 N 条未列出」 | — | 脚注可能落在下一片 |

「子件行与父行被拆到两片」是**可接受**的：子件行自带序列号 / 名称 / 数量 / 位置，单看一行
就完整；反过来「把一行劈成两半」不可接受 —— 那会得到半个链接 URL。

**两趟贪心装填**：

- **pass 1**：逐块装入当前片，超则开新片。每片预算 = `4096 − RESERVE`，`RESERVE = 48`。
  ⚠️ **RESERVE 必须存在**：「（i/N）」要等装填完才知道 N（自指），不预留则「第 1 片刚好
  4090 字节、pass 2 追加后缀后 4095」会炸。⚠️ `String::len()` 就是 UTF-8 **字节数**；
  `<font color="info">…</font>` 每个约 22 字节，计入预算。
- **pass 2**：已知 N，为每片的 Title 块标题追加「（i/N）」，断言 `len() <= 4096`。
  ⚠️ **`N == 1` 时不追加后缀**（单条消息不该挂「（1/1）」）。
  ⚠️ **标题块在每一片都重复**（一份**副本**）—— 否则第 2 片在群里是几行孤零零的工人
  数据、没有日期上下文。

**两处「被挡掉」各有自己的脚注，且都不是字节截断**（渲染器从不静默少发内容）：

| 挡掉什么 | 脚注 | 何时不出现 |
|---|---|---|
| 某组里超出 `order_list_limit` 的工单 | `> <font color="comment">另有 18 条未列出</font>` | 该组没挡掉任何行（`limit = 0` 或行数不足） |
| 超出 `worker_top_n` 的工人 | `> <font color="comment">另有 8 人在此省略（配置「最多列出工人」= 15）</font>` | `worker_top_n = 0`（不限） |

⚠️ 两处脚注的**数量必须与组标题的「N 条」自洽**：组标题用 `COUNT(*) OVER ()`（未截断
总数），脚注 = 总数 − 实际列出行数。脚注文案要能分辨「配置挡掉的」与「装不下」—— 后者
是不存在的：装不下只会 `21804`（§5.5），不会变成一条「已省略」。

### 5.5 三个防御性分支（**一律 `21804`，绝不静默截断**）

| 情形 | 处置 |
|---|---|
| 单个块自身 > `4096 − RESERVE` | `21804`，错误消息**指明是哪一节**。当前最大块是 `WorkersHead`（~120 字节），但 `subject_template` 很长时是标题块 —— 这条是防御性的 |
| 装填结果片数 > `max_chunks` | `21804`，文案：「本次日报需要 4 条，但配置上限为 2；请调高「最多拆几条」或调低「最多列出工人」」。⚠️ 清单形态下调低 `order_list_limit` 同样有效（每行约 135 字节，其中一条直链约 95 字节） |
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

⚠️ **发送之前还有一段等待**：图纸就绪（§4.6，最长 480s）。它是**纯等待**、不发任何
消息，所以「群里第一条消息出现在什么时候」取决于当日工单的预览是否已就绪 —— CI 通常
几秒内收敛，预算只在 CI 异常时才会被吃掉。

## 6. 移除记录

- **端点 / 表：无移除。** 全新域，无任何端点或表被删。迁移
  `migrations/20261018000000_001_wecom.sql` 建的三张表（`t_wecom_webhook` 属通道域 +
  本域的 `t_wecom_daily_config` / `t_wecom_daily_log`）**全部在用**。
- **唯一的列移除**：`t_wecom_daily_log.note_part_count`，迁移
  `20261023010000_001_wecom_daily_note_count_semantics.sql`。第二节改成单值口径后它是第
  二个表达同一个数字的键，随该迁移整列删除；`note_count` 改承载现在这个数（语义变更与
  历史行的读法见 §8.3 第 11 条）。Rust 字段与出参 wire 键同步下线 ⇒ `PreviewOut` 15→13
  字段、流水行 19→18 字段（标题占位符 `{note_count}` **名字保持不变**，避免存量
  `subject_template` 变成裸占位符）。

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
| `t_worker` | ✅（第一节 `JOIN` + 第四节 `GROUP BY` + holder 名批量解析） | ❌ | `prod` |
| `t_shelf` | ✅（位置：生产架 / 品检架的 `code`） | ❌ | `prod` |
| `t_outsource_company` | ✅（位置：外协厂名） | ❌ | `prod::outsource` |
| `t_part_file` | ✅（图纸直链，**在 `files` 域内**） | ❌ | `files` |
| `t_file_preview` | ✅（预览状态，**在 `files` 域内**） | ⚠️ **间接写**（`ensure_ready_for_parts` 内部推进状态机；本域只调那个入口） | `files`（本域不直连） |

⚠️ **别域表的张数刻意不写死**（加表就过期）。**域隔离护栏不适用**：本域读若干张别域表
（只读聚合是既定 pattern，同 `dashboard` / `statistics`），而内容域向通道域取推送通道、
向图纸域取预览直链，都是**「通道 / 内容」「能力提供方 / 消费方」这两个切分的必然结果**
（与 `mail_digest → scheduled_task` 同款），护栏若装上会把**正确**的写法也拦掉。
⚠️ 图纸那张表的读写**不在本域的 SQL 里**：本域连 `t_part_file` 的表结构都不引用
（§4.6），所以「图纸链路变了」不需要改本域一行代码。

### 8.2 前端配套

| 后端 | 前端 | 备注 |
|---|---|---|
| `/cron/wecom-daily/config`（23 键） | `views/wecom/daily/weComDailySchema.ts::weComDailyConfigSchema` | `.strict()` + `Object.keys().length === 23` |
| `/cron/wecom-daily/meta`（6 键） | `weComDailyMetaSchema` | 三个 range + 四个 `needs_*` **不许硬编码** |
| `/cron/wecom-daily/preview`（13 键） | `weComDailyPreviewSchema` | ⚠️ 无 `markdown` 单字段 |
| `/cron/wecom-daily/send` | `weComDailyLogSchema` | 与 `/logs` 的行同一个结构体 |
| `/cron/wecom-daily/logs`（18 键/行） | `weComDailyLogSchema` / `weComDailyLogListSchema` | |

**部署顺序**：后端必须先上（旧路径 404、无 alias，失败发生在**网络层**）。
⚠️ 前端兜底常量（`DEFAULT_WECOM_DAILY_FREQUENCY_LABELS` / `DEFAULT_WECOM_DAILY_WORKER_METRIC_OPTIONS`
/ `DEFAULT_WECOM_DAILY_RANGES`）只在 `meta` 还没到达时用，**meta 到达后一律以 meta 为准**
（两份兜底与后端的 label 文案措辞不同，见 §8.3 第 2 条）。

⚠️ **⚠️ 与前端的口径分叉登记**（前端**刻意**如此，不是漏做）：

1. **前端刻意不建「统计日」列** —— `report_date` 恒等于 `sent_at` 的日期（后端同一个
   `now_naive()` 取值）⇒ 一列是另一列的常量前缀，列表里放两列等于给同一事实两行。
2. **前端刻意不做日报 5 个数字的表格列**（`overview_batch_count` / `overview_quantity` /
   `note_count` / `overdue_count` / `upcoming_count`）—— 流水表存它们只是为了「事后复盘
   当时那份日报是什么数字」，而企业微信群里还看得到同样的数字；列表里再铺 5 列会把真正
   需要看的（状态 / 片数 / 通道 / 时间）挤成窄列。它们**在库里、在 `GET /logs` 出参里
   都在**，只是不做表格列。

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
   （一行过时注释）完全不成比例，所以偏差如实登记在本域几处（本文 / `dto.rs` 文件头 /
   `model.rs` 字段 doc），而**真源只有一处**：`dto::WorkerMetric` + 写侧 `21804` 拒未知值。
2. **前端 `DEFAULT_WECOM_DAILY_*` 兜底常量与后端 `GET /meta` 的 label 措辞不同**
   （「按件数」vs「送件数」、「每周（指定星期几）」vs「每周」）。真源是 meta，前端那份
   只在 meta 在飞时用 ⇒ 不是契约冲突，但**看到两者不一致不要以为哪个错了**。
3. **`completed_count` 与 `statistics` 域存在刻意差异**：`statistics::count_completed`
   **没有 JOIN `t_worker`**，而本域为了「一条语句五个聚合」把
   `JOIN t_worker w ON w.id = e.worker_id`（INNER）加在整条语句上 ⇒ `worker_id IS NULL`
   或指向已删工人的事件在本域**不计入**这个数字。连带后果：2026-10-12 之前的事件
   （`worker_id` 从未被写入）在本域第一节的完工数恒为 0。详见 §4.2。
   ⚠️ **「返修件数」这个口径已下线**（第一节不再有 `repair_part_count`，出参里也没有）——
   本仓的返修是「工件级」而非「工单级」，与本域的行单位不同源，保留一个同名字段只会让
   读的人以为它是工单数。
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
    「落流水」。`POST /cron/wecom-daily/send` 最坏 ≈ **510s**（图纸就绪预算 480s +
    `max_chunks` 上限 20 × 单片 5s；见 §4.6 的核算），为此本域已把 `/send` 放进
    `middleware::timeout::is_long_timeout_path` 的**长档白名单**（660s，与批量打印共用
    一档）⇒ **正常路径下「失败也落流水」成立**。⚠️ 但**已知残留缺口**：整链超出该档时
    （数据库本身不可用、上游网关比 660s 更早切断连接），那一行流水**仍然写不出来**，
    `GET /logs` 里查不到痕迹。这不是「已完全解决」，是「把触发概率压到可忽略」的处置；
    要彻底消除它得改成分片投递与流水落库**解耦**（后台任务补写流水），那是独立重构。
    护栏：`tests/wecom_daily/timeout_guard.rs` + `middleware::timeout` 的 lib 单测。
11. **⚠️ `note_count` 的历史行按**旧语义**读**：该列在 2026-10-23 之前记的是「本期出货的
    **送货单张数**」，现在记的是「本期出货的**去重工单数**」。迁移不改任何存量行的值 ⇒
    两种量级混在同一列里（一张单装多个工单时前者远小于后者）。日报正文与新流水只看得到
    新语义；读历史流水做复盘时必须知道这个切换点。列注释已登记同一件事（迁移是
    append-only，无法给历史行加标记列而不引入新的口径）。
12. **⚠️ 位置用派生列有滞后窗口**：「这个工单现在在哪儿」取的是
    `shared::batch::status_machine` 的 min-progress 判据，而批次状态本身是派生缓存 ⇒
    刚流转完的工单在派生列翻过来之前可能还显示旧位置。与 §4.3 的在制数同源问题，处置
    同款（同一个判据，好过每页各发明一套）。
13. **⚠️ 图纸链路依赖 python 侧的 CI 端点**：本域侧实现完整，但 python 侧未部署
    `/api/v1/files/ci-preview-jobs` 时，打 python 的每一步都失败 ⇒ 全部行不带链接
    （**不报错、日照发出**）。这是「缺链接优于发不出去」那条取舍的直接后果，排障时先看
    链接是否成片消失（是 ⇒ python/CI 侧问题，不是本域取数问题）。
14. **`order_list_limit` 的取值域没有在 `GET /meta` 里下发**（本端仍是 6 个键）：区间
    `0..200` 写在 §2.1 与 `POST /config` 的 clamp 行为里。要下发成第 7 个键会改 wire
    契约（前端 `.strict()` schema 要同步），当前不构成阻塞（管理员填超界值时后端会 clamp）。
15. **⚠️ 状态白名单的人工同步点只有 2 处**（后端 1 处 + 前端 1 处），登记在此是因为
    「漏改一处不会编译失败」这个性质没有变：
    - 后端：`shared::delivery_status::RED_TIER_STATUSES`（日报 / 邮件 / 大屏共用一份）；
    - 前端：`UpcomingDeliveryChart.vue` 的 `LAYERS[].statuses`（大屏的交期面板用 6 态，
      与日报的 4 态**有意不同**，不是漂移）。
