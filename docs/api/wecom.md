# wecom 域 API（企业微信推送通道：群机器人 webhook）

> **2026-10-18 新建域**。需求（用户原话）：「在定时任务一级菜单下新增 企业微信 子菜单，
> 并新增企业微信子模块。与邮箱类似，**第一个 tab 页面为 webhook 配置**」。
>
> **全新域，无任何旧路径、无 alias**（历史上从未存在过 `/api/v2/wecom/*` 之类的公开路径）。
> URL 挂 `/api/v2/cron/wecom`，归入「定时任务」域组的 `/cron` 前缀族，与邮件两域同构。
>
> | 子域 | 职责 | 模块 | URL |
> |---|---|---|---|
> | `mail` | 邮件**通道** | `src/modules/scheduled_task/` | `/api/v2/cron/mail/*` |
> | `mail-digest` | 邮件**内容** | `src/modules/mail_digest/` | `/api/v2/cron/mail-digest/*` |
> | `wecom` | **通道**（本文件） | `src/modules/wecom/` | `/api/v2/cron/wecom/*` |
> | `wecom-daily` | **内容**（每日总结） | `src/modules/wecom_daily/` | `/api/v2/cron/wecom-daily/*` |
>
> ⚠️ **菜单 path 不跟着 API 前缀变**：菜单 seed 的 `path='/wecom'` 与前端路由保持不变，
> 变的只是后端 URL。
>
> 代码位置：`src/modules/wecom/`（六件套 + `sender.rs`）、加密基建 `src/infra/crypto.rs`
> （AAD 显式版 `seal_with_aad` / `open_with_aad`）、主密钥解析
> `src/infra/config.rs::MailAccountSecret`、
> 迁移 `migrations/20261018000000_001_wecom.sql`、错误码段 `shared/error.rs` 的 `218xx`。
>
> ## ⚠️ 与 `src/modules/wx/wecom_client.rs` **同名不同物**
>
> 排障时先分清这两个（它们唯一的共同点是「都打企业微信」）：
>
> | | 本域 `modules::wecom` | `modules::wx::wecom_client` |
> |---|---|---|
> | 干什么 | 把消息推进企业微信**群**（群机器人 webhook） | **小程序登录**（`wx.login` 的 code 换 userid） |
> | 端点 | `/api/v2/cron/wecom/*` | `/api/v2/wx/login/wecom` |
> | 需凭据 | **不需要** —— 地址里的 `?key=<uuid>` 就是全部鉴权 | 需 `WECOM_CORPID` / `WECOM_CORPSECRET` |
> | 有状态吗 | 无（每次一条裸 HTTP POST） | 有（`access_token` 写 Redis 缓存，TTL `expires_in-300`） |
> | 鉴权 | Manager（6 端点全部） | 公开端点（`auth/middleware.rs::is_public_path` 白名单） |
>
> **零交集**：不要复用那边的 `reqwest::Client` 配置，也不要把本域的 webhook URL 当成
> 那边的 `access_token` 看待。`tests/wecom_login.rs` 是**小程序登录**那条链路的集成测试，
> 与本域的 `tests/wecom/` 无关。
>
> ## 域范围：只管**通道**，不管内容与调度
>
> | 归本域 | 归 `wecom_daily` 域 |
> |---|---|
> | `t_wecom_webhook`（群机器人实体） | `t_wecom_daily_config`（四节开关 / 统计口径 / 频率 / **选哪个 webhook**） |
> | 地址的加密与出入库、掩码 | `t_wecom_daily_log`（推送流水） |
> | 「测试连通」探针（`sender::send_text`） | 每日总结的 Markdown 渲染与分片（`sender::send_markdown` 的调用方） |
> | 出网纪律（超时 / errcode 解析 / 脱敏） | 定时任务的注册与触发 |
>
> 切分线的理由：**「有哪些推送通道」与「用通道推什么」是两个独立演进的职责**（与邮件两域
> 同款）。加第二种推送类型（周报、异常告警）时前者不动、后者各自加自己的内容配置表；
> 而 webhook 实体、地址加密、掩码、`reqwest` 出网这一整套只需要建一次。
>
> ⚠️ **代码依赖方向是单向的**：内容域 import 本域的
> `service::resolve_target` / `service::target_is_available` / `service::send_probe` /
> `sender::send_markdown`；本域**对 `wecom_daily` 零代码依赖**（§8.1）。

## 🔒 凭据怎么保护（三条硬约束，逐条都有反例）

1. **加密入库，不是明文、也不是散列**：`encrypted_url` 存 AES-256-GCM 密文
   （`base64(nonce12 ‖ ct ‖ tag16)`，AAD = `t_wecom_webhook.url.v1`），主密钥走 env
   `MAIL_ACCOUNT_SECRET`。散列（bcrypt）取不回明文，而群机器人推送必须把地址原样交给
   `reqwest` ⇒ 只能是**可逆**的对称加密。
   **反例**：明文入库 ⇒ 一次 `pg_dump` 或一条只读副本就把「以本企业名义往那个群发消息」
   的能力整份送出去 —— 企业微信官方文档明确要求保护 webhook 地址、不得泄漏。
2. **掩码不回显，且只回显掩码**：`WebhookOut.url_masked` 是
   `…?key=<前 4>***<后 4>`，明文 URL 一个字都不出现在任何出参、`error_message`、
   `tracing` 日志里。
   **刻意不设 `url_configured` 布尔**：建 webhook 时地址**必填** ⇒ 该布尔恒 true，是死
   字段；死字段会让人误以为「地址可以是空的」，进而写出「先建档、地址以后再补」的流程。
   **反例**：把 `key` 的前后段一起回显（「`693a…faaa` 配错了」这类排障需求）看起来无害，
   但那正是掩码要防的那条泄漏面 —— 管理员要看的是「是不是同一个群」，前 4 + 后 4 已经
   够分辨（真实 key 恒 36 字符 uuid，少 28 位）。
   ⚠️ 唯一**保留**掩码的地方是 `t_wecom_daily_log.webhook_masked`：那是**审计**
   场景，需要肉眼确认「这两次发的是不是同一个群」，与出参的性质不同。
3. **被 `t_wecom_daily_config.webhook_id` 引用 ⇒ 禁止删除**（`21808` + 409），
   否则配置会指向一条死行，页面看着正常、直到定时任务真推送才以 `21801` 暴露。

## 1. 端点表

**全部 6 条都是 Manager 独占**（角色守卫写在各 handler 第一行，逐字
`current.require_role(Role::Manager)?`）。前缀 `/api/v2/cron/wecom`。

| 方法 | 路径 | 权限 | 一句话 |
|---|---|---|---|
| `GET` | `/api/v2/cron/wecom/webhooks` | Manager | webhook 列表（`is_default` 排最前）；**绝不返回** `encrypted_url` |
| `POST` | `/api/v2/cron/wecom/webhooks` | Manager | 新建（`name` / `webhook_url`）；**不设默认** |
| `POST` | `/api/v2/cron/wecom/webhooks/{id}` | Manager | **全量覆写**；body **必含 `version`**（OCC）；`webhook_url` 空串或缺省 ⇒ **沿用旧密文** |
| `POST` | `/api/v2/cron/wecom/webhooks/{id}/delete` | Manager | 软删；被每日总结配置引用 ⇒ `21808`；body **必含 `version`** |
| `POST` | `/api/v2/cron/wecom/webhooks/{id}/set-default` | Manager | 置默认；body **必含 `version`**（目标行的） |
| `POST` | `/api/v2/cron/wecom/webhooks/{id}/test` | Manager | 测试连通；body 为**空对象 `{}`**，⚠️ **不含 `version`**；**不写任何日志表** |

- **只用 GET / POST**：本仓 HTTP 方法**零** PUT/PATCH/DELETE（`mail_digest` 域
  2026-10-13 实测核实：全仓 `.put(` / `.patch(` / `.delete(` 的命中只有 opendal /
  redis 客户端调用）。故「改」走 `POST /webhooks/{id}`、「删」走
  `POST /webhooks/{id}/delete`。回归用例 `accounts::put_patch_delete_verbs_are_not_routed`
  钉住「将来有人加 `DELETE /webhooks/{id}` 时会当场红」。
- **权威路由清单**：`src/modules/wecom/handler.rs::ROUTES`（lib 单测逐条比对 `router()`
  源码，加了 `.route()` 忘登记就在单测里当场红）。
- 挂载点：`src/modules/mod.rs::v2_router()` 的 `.nest("/cron/wecom", …)`。
  ⚠️ 与同族的 `/cron/mail`、`/cron/mail-digest`、将来的 `/cron/wecom-daily`
  **同段数但无冲突**：matchit 在**第 2 段**先静态判别（四个第 2 段互不为对方的前缀），
  `/cron/mail/accounts`、`/cron/mail-digest/config`、`/cron/wecom/webhooks` 在第 2 段就分道。
- ⚠️ **matchit 注册顺序无约束**（域内）：本域最长段数 3（`/webhooks/{id}/delete`），
  最短 1（`/webhooks`）；**不存在 2 段的静态路径**（`Path<i64>` 只出现在
  `/webhooks/{id}`）⇒ 不存在「静态段被 catch-all 吞成 400」的风险。

## 2. `WebhookOut` 逐字段

### 2.1 四个端点共用的出参（`GET /webhooks` / `POST /webhooks` / `POST /webhooks/{id}` / `POST /webhooks/{id}/set-default`）

`id` 是 **JSON string**（雪花 ID 防 JS 精度截断，`shared::types::serialize_i64`）。

| 字段 | 类型 | 取值 / 来源 | 可空 |
|---|---|---|---|
| `id` | string | `t_wecom_webhook.id`（雪花） | 否 |
| `version` | number | OCC 版本；新建为 `0`，每次覆写 / 删除 / 置默认自增 1 | 否 |
| `name` | string | 自定义名称（`varchar(50)`，未软删行内唯一） | 否 |
| `url_masked` | string | 地址掩码 `…?key=<前4>***<后4>`。⚠️ **不是明文** | 否（形态不可识别时为 `"***"`） |
| `is_default` | bool | 至多一个未软删行为 `true`（唯一索引保证）。⚠️ **不参与发送**，见 §4.3 | 否 |
| `created_at` / `updated_at` | string | `YYYY-MM-DD HH:MM:SS`（Asia/Shanghai naive） | 否 |

⚠️ **出参里没有 `encrypted_url`、没有裸 `url`、没有 `url_configured`**。回归用例
`accounts::create_encrypts_url_and_never_echoes_it` 与
`accounts::list_webhooks_hides_plaintext_and_orders_default_first` 把出参整体序列化成文本后
断言「不含地址明文 / key 片段 / `encrypted_url` 字样」。

⚠️ **`url_masked` 需要先解密一行再算**（表里没有掩码列，掩码是从明文派生的）⇒
**任一行解不开 ⇒ 整条列表返 `21801`（503）**，见 §8.3 第 2 条。

### 2.2 `GET /webhooks` 出参（`WebhookListOut`）

`{ items: WebhookOut[] }`。**排序**：`is_default DESC, id ASC`（默认 webhook 排最前）。

### 2.3 `POST /webhooks` 入参（`CreateWebhookRequest`）

⚠️ **无 `version`**：这是新建，没有「别人先改过」这回事。

| 字段 | 类型 | 必填 | 校验（失败 ⇒ `21804` / `21803` + 400） |
|---|---|---|---|
| `name` | string | **必填** | trim 后非空；拒控制字符；≤50 **字**（`.chars().count()`，不是字节数） |
| `webhook_url` | string | **必填** | trim；非空；拒控制字符；≤500 字；必须匹配 `^https://qyapi\.weixin\.qq\.com/cgi-bin/webhook/send\?key=\S+$` |

- **`name` 重复 ⇒ `21807` + 409**（**不是** `40901`：CLAUDE.md 明令 40901 只表达 OCC，
  业务唯一键冲突须有专用码）。⚠️ 报错文案里回显的是**校验并 trim 后**的名称（新建与
  改档两条端点同款），不是请求里的原文。
- ⚠️ **地址校验失败时**报文**不回显收到的地址**（它可能是一个真地址，管理员从别处粘来的
   ⇒ 回显等于把凭证写进日志与 HTTP 响应体）。回归：
  `guards::invalid_url_returns_21803_without_echoing_it`。
- ⚠️ **地址正则必须与前端 `weComWebhookSchema.ts::WECOM_WEBHOOK_URL_RE` 逐字一致**
  （后端转写见 `src/modules/wecom/dto.rs::WEBHOOK_URL_REGEX`）。两边各写一份、没有编译期
  约束，放宽或收紧任何一侧都会造成「后端放行 / 前端拒收」。**收紧的理由是 SSRF**：
  本域会拿这个地址真发 HTTP 请求（探针与内容域推送都走），放开 host 就等于让一条数据库
  记录变成任意地址的请求发起器。
- ⚠️ **刻意放行** `?key=A&x=1` 这类形态（`\S+` 不含任何空白约束，`&` 落在里面）⇒ 前端
  同样接受。后果是企微把 key 解析成 `A`、推送失败（`21802`），不会静默错发。
- ⚠️ **新建永远不设默认**（`is_default` 恒 `false`）。置默认是
  `POST /webhooks/{id}/set-default` 那条独立端点的事。**代价**：新装的库第一个 webhook
  不是默认，前端在「还没有默认 webhook」时应显示提示。

### 2.4 `POST /webhooks/{id}` 入参（`UpdateWebhookRequest`，全量覆写）

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `version` | number | **必填** | OCC 锚点。**缺失 → HTTP 422 纯文本**，见下 |
| `name` | string | **必填** | 同 §2.3 |
| `webhook_url` | string \| null | 否 | **缺省或 trim 后为空 ⇒ 沿用旧密文**（不覆写、不报错；`"   "` 也算沿用）；**非空 ⇒ 全量走 §2.3 的地址校验矩阵** |

⚠️ **`webhook_url` 的类型是 `Option<String>` 而不是裸 `String`**：用裸 `String` 的话
**省略键会 422**，而「省略 ⇒ 沿用」正是前端编辑弹窗的既定交互（管理员改个名称时前端拿不到
地址明文 —— 服务端从不回显 —— 强制必填等于逼前端每次把「不知道的东西」原样传回来）。

⚠️ **全量覆写**：`name` 每次都参与写入，**没有**「只改一项」的三态语义。
`is_default` 与 `encrypted_url`（未给时）**不在**本端点的可写集合里。

⚠️ **改名撞同名未软删行 ⇒ `21807` + 409**（同 §2.3）。service 层捕
`is_unique_violation`（SQLSTATE 23505）转本码，**不是** 500、也不是 `40901`。回归：
`guards::rename_to_a_live_duplicate_returns_21807_and_keeps_row_intact`。

⚠️ **地址校验失败时旧密文一个字节都不动**（回归：
`accounts::update_with_invalid_url_returns_21803_and_keeps_row_intact`）。

**响应回填的是提交后的新 `version`**（读回最新行），前端据此做下一次提交。

**⚠️ 缺 `version` 是「HTTP 422 纯文本」，不是业务信封**：`/webhooks/{id}`、
`/webhooks/{id}/delete`、`/webhooks/{id}/set-default` 三条**都必收 `version`**（本表带
`version` 列 ⇒ 照 CLAUDE.md §8，**软删也不例外**）。缺字段时 axum 在 `Json` 提取器层就拒
了，而本仓**没有** `JsonRejection` 的自定义处理器 ⇒ 走 axum 默认路径：

| | 值 |
|---|---|
| HTTP | **422 Unprocessable Entity** |
| 响应体 | **纯文本**（含缺失字段名，如 ``Failed to deserialize the JSON body into the target type: missing field `version` ``） |
| `code` 字段 | **没有** —— 不是 `R<T>` 信封，前端 `resp.data.code` 恒 `undefined` |

⇒ **前端必须按 HTTP 状态码处理这一类失败，不能指望读 `code`**（尤其 `delete`：它有 `21808`
「被引用禁删」这个业务码，**别把 422 也当成「删不掉」的一种**）。回归：
`guards::update_without_version_returns_422_plain_text` / `guards::delete_and_set_default_require_version`。

### 2.5 `POST /webhooks/{id}/delete` 与 `POST /webhooks/{id}/set-default` 入参与响应

两者入参都只有 `{"version": <number>}` 一个字段，**必填**、无 `#[serde(default)]`
⇒ 缺失同样是 **HTTP 422 纯文本**（形态见 §2.4 末）。

⚠️ **两者的响应都是 `R<()>`（`data: null`）**：
- `delete` ⇒ `{ "code": 0, "message": "ok", "data": null }`
- `set-default` ⇒ `{ "code": 0, "message": "ok", "data": <WebhookOut> }`

⚠️ **`delete` 刻意不返回 `WebhookOut`**（与 `POST /mail/accounts/{id}/delete` 同款形状）：
前端按「HTTP 状态 + `code`」处理、不 parse 这个响应体，返回一个出参只会白丢。
回归：`guards::delete_referenced_webhook_returns_21808`（断言 `data: null`）。

⚠️ **软删也吃 OCC**（CLAUDE.md §8）。这正是「读-再-比式隐式 OCC」被明令禁止的场景：
两人同时打开同一个编辑页，一人改了名、一人删了行，后者会静默成功。

### 2.6 `POST /webhooks/{id}/test` 入参与响应

- **入参是空对象 `{}`**，⚠️ **不含 `version`** —— 这条端点**不改那一行**，所以没有
  「别人先改过」这回事，也就不该吃 OCC 锚点。
- **响应是 `R<()>`（`data: null`）**：前端不 parse 响应体，只判 HTTP 状态与 `code`。
- ⚠️ **不写任何日志表**（`t_wecom_daily_log` 一行都不动）：这是「管理员点一下看通不通的诊断
  动作」，不是一次推送；混进流水会让「今天推了几次」这个统计口径失真。回归：
  `probe::probe_writes_no_log_rows`（走**失败**路径来钉这条 —— 真实推送失败是要记账的）。
- ⚠️ **不改那一行**（不涨 `version`、不动 `updated_at`）。回归：`probe::probe_does_not_mutate_the_row`。
- 失败码：`21806`（id 不存在或已软删，404）、`21801`（密文解不开，503）、`21802`（推送失败，502）。

## 3. `sender` —— 本域唯一的出网点

### 3.1 `SendOutcome` 与官方契约

```text
POST https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=<uuid>
Content-Type: application/json

{"msgtype":"text","text":{"content":"…"}}
{"msgtype":"markdown","markdown":{"content":"…"}}

→ { "errcode": 0,  "errmsg": "ok" }
→ { "errcode": 93000, "errmsg": "invalid webhook url, hint: [...], from ip: …" }
```

| 输入 | 结果 |
|---|---|
| 2xx + `errcode: 0` | `Ok` |
| 2xx + `errcode != 0` | `Failed`（带 errcode + errmsg，errmsg 截断到 200 字） |
| 2xx + 非合法 JSON / 缺 `errcode` | `Failed`（「响应体无法解析」） |
| 非 2xx | `Failed`（**只带 status code**，`body` 参数被忽略） |

### 3.2 ★ 必须解析响应体的 `errcode`

企业微信的失败**绝大多数是 HTTP 200 + `errcode != 0`**（地址失效、群里机器人被移除、
消息被限频…）。不解析它就是「HTTP 成功 = 推送成功」的**静默失败** —— 流水里会记一条
SUCCESS、页面上「每日总结照常到达」，而群里什么都没有。

守卫（三层，缺一层就有零信号风险）：
- lib 单测 `sender::classify` 的四条分支（**不发真实网络请求**）；
- 集成测试 `tests/wecom/probe.rs::probe_with_invalid_key_returns_21802_and_hides_the_key`
  **真打** `qyapi.weixin.qq.com`（实测返回 `HTTP 200 {"errcode":93000,…}`）⇒ 端点必须 `21802`；
- 实测：把 `classify` 的 `errcode` 解析整段摘掉后，上述 2 条 lib 单测与该集成用例**同时变红**
  （端点会 200 成功，正是那个静默失败形态）。

### 3.3 安全纪律（三条，逐条有反例）

1. **URL 永远不进任何日志或错误文案**：`reqwest::Error` 的 `Display` 在 `inner.url`
   存在时会追加 ` for url (<完整 URL，含 query>)`，而 query 里就是 `key=`。故本文件打
   日志前一律先 `.without_url()`（与 `wx::wecom_client.rs` 的 R1 订正同款纪律）。
2. **HTTP 非 2xx 时只记 status code，不记 body 全文**（上游在某些错误路径下会回显请求内容）。
3. **响应体解析失败只记「无法解析」，不记原文**（同上，且响应体没有诊断价值）。

⚠️ `errmsg` 是**服务端生成**的（不含我们的 key），且做了控制字符过滤 + 200 字截断
（它会原样进 `t_wecom_daily_log.error_message`，那是 `varchar(500)`）。

### 3.4 ⚠️ 限频与长度上限（调用方的责任）

| 约束 | 值 | 归属 |
|---|---|---|
| 每个消息推送 ≤ 20 条/分钟 | 官方限频 | 内容域：`t_wecom_daily_config.max_chunks`（范围 `1..=20`）**就是这条限频的上界** |
| `markdown` 内容 ≤ 4096 字节（UTF-8） | 官方上限 | **内容域负责分片**（`sender` 不截断、不分片） |
| `text` 内容 ≤ 2048 字节 | 官方上限 | 同上（探针文案固定一句话，远低于上限） |
| 单次请求超时 5s | `sender::HTTP_TIMEOUT_MS` | ⚠️ **刻意是模块常量、不走 `infra::config` 的 env**：已有的 `WECOM_HTTP_TIMEOUT_MS`（`infra::config::WeComConfig::http_timeout_ms`）是**小程序登录**那条链路的超时，同名不同物、互不共享 —— 挂到本域会让人以为改一个 env 能同时调两条链路 |

⚠️ **超限的表现是企业微信返 `errcode != 0`（`Failed`），不会静默截断** ⇒ 内容域必须在
流水里看到它，而不是发出一条被腰斩的消息。

### 3.5 markdown 语法子集（v1）

标题 1-6 级（`#` 与文字间要有空格）/ `**加粗**` / 链接 / 行内代码（不支持跨行）/
`>` 引用 / `<font color="info|comment|warning">`。
**不支持**列表、表格、分割线、图片、代码块、斜体。
（`markdown_v2` 支持表格与列表但**不支持字体颜色**且客户端需 4.1.36+，本仓选 v1 ——
选型理由归内容域，写在 `wecom_daily` 的 template doc 里。）

## 4. 口径表

### 4.1 加密口径（`src/infra/crypto.rs`）

| 项 | 值 |
|---|---|
| 算法 | `ring::aead::AES_256_GCM`（AES-256-GCM） |
| 密文信封 | `base64(nonce(12) ‖ ciphertext(N) ‖ tag(16))` |
| nonce | **每次 seal 新生成**（`SystemRandom` + `generate()`）；复用 nonce = GCM 失效且**不报错** |
| AAD | `b"t_wecom_webhook.url.v1"`（`dto::WEBHOOK_URL_AAD`），seal / open 两侧逐字同常量 |
| 主密钥 | env `MAIL_ACCOUNT_SECRET`，32 字节，接受 64 位 hex 或 44 位标准 base64 |
| 调用 | `crypto::seal_with_aad` / `crypto::open_with_aad`（2026-10-18 新增的 AAD 显式版） |

- ⚠️ **AAD 与 mail 的 `t_mail_account.secret.v1` 刻意不同**：两域**共用同一把主密钥**、
  **不同 AAD** ⇒ 两类密文**互不串扰**（AEAD 把 AAD 纳入认证：拿 webhook 的密文去冒充
  SMTP 授权码会直接解不开）。守卫：`infra::crypto::tests::different_aad_cannot_open_the_other_tables_ciphertext`。
- **不新增 env**：`MAIL_ACCOUNT_SECRET` 是复用（与 `t_mail_account` 同钥匙）。这意味着
  「换主密钥」这一个操作会同时打掉两张表的存量密文（§8.3 第 1 条）。
- ⚠️ **`seal_with_aad` / `open_with_aad` 每次调用都重读 env**（签名不带 state）。集成测试
  进程**不走** `AppConfig::from_env` ⇒ 必须在 `tests/wecom/main.rs::ensure_mail_secret_env`
  用 `OnceLock` + `unsafe { set_var }` 设一次（§8.3 第 6 条）。

### 4.2 掩码口径

| 项 | 值 |
|---|---|
| 输出 | `…?key=` + `key` **前 4 个字符** + `***` + **后 4 个字符** |
| 例 | `…send?key=693a91f6-7xxx-4bc4-97a0-0ec2sifa5aaa` ⇒ `…send?key=693a***5aaa` |
| 短 key（< 9 个字符） | 降级成固定 `********`（**不按真实长度**，避免顺手泄露长度） |
| 无 `?key=` 段（库里被手工改坏） | 整段打码 `***` |
| 有 `?key=` 但后面还挂 query 段 | 只取第一段（尾部那一段不进掩码） |
| 切分单位 | **char**（不是字节）：多字节字符按字节切会 panic 或切出半个字符 |

守卫（lib 单测）：`mask_keeps_first_and_last_four_of_key` / `mask_does_not_panic_on_short_key` /
`mask_of_unparsable_shape_is_fully_masked` / `mask_drops_extra_query_segments`。

### 4.3 唯一性与并发口径

| 约束 | 保障方式 |
|---|---|
| 至多一个未软删的默认 webhook | 唯一索引 `uk_t_wecom_webhook_one_default ON t_wecom_webhook ((true)) WHERE deleted_at IS NULL AND is_default` |
| 同名 webhook（未软删）唯一 | `uk_t_wecom_webhook_name_alive ON t_wecom_webhook ((name)) WHERE deleted_at IS NULL` |
| 软删后可同名再建 | 两个索引都以 `deleted_at IS NULL` 为前提（回归 `guards::name_can_be_reused_after_soft_delete`） |

⚠️ **`set-default` 的两条 UPDATE 必须在同一事务内「先降后升」**，事务的作用是
**失败时不留半截状态**：先升新的、再降旧的会在第二条 UPDATE 上撞 23505（唯一索引只允许
一个 `is_default = true`）；而更常见的中途失败是**升格那一步自己失败**（body 的 `version`
与库里对不上 ⇒ `40901`）。两条语句若各自分别自动提交，「降已生效 + 升没生效」就是库的终态
⇒ **一个默认 webhook 都没有**。放进同一事务后任何一步失败都整笔回滚。回归：
`accounts::set_default_demotes_the_previous_default`。

⚠️ **开事务消不掉的是「两个管理员同时对不同 webhook 点置默认」**：两个事务各自看不见对方
未提交的结果 ⇒ 后到者的 `promote_to_default` 撞 23505，在 service 层转 `40901`（**不是**
500）。⚠️ **不要**为了消灭这个窗口去动那个 partial 唯一索引 —— 它是「至多一个默认」的
**唯一**保障。

⚠️ **⚠️ `is_default` 不参与发送**：推送只认 `t_wecom_daily_config.webhook_id` 指到哪一行。
本列只用于**列表排序**与唯一索引。改它**不会**改变任何消息的落点。⚠️ 后果是「默认 webhook」
这个词在前端 UI 上要小心措辞：它是「列表里排第一的那个」，不是「没选通道时会用的那个」。

⚠️ **删掉默认 webhook 不会把默认自动转给别的**（库里变成「零个默认」）：任何「自动转给
某个别的 webhook」的规则都会让「我以为 A 是默认」在某天悄悄变成 B —— 比「没有默认」危险
得多。回归：`accounts::deleting_the_default_leaves_no_default`。

### 4.4 通道解析口径（供内容域调用）

`wecom::service::resolve_target(conn, webhook_id: Option<i64>) -> Result<ResolvedTarget, AppError>`：

| 失败 | 判据 | 错误码 |
|---|---|---|
| 未选通道 | `webhook_id IS NULL` | `21801`（503） |
| 指向不存在 / 已软删 | `get_webhook` 返 `None` | `21801`（503） |
| 地址解不开（base64 坏 / tag 失配 / AAD 不匹配 / **主密钥被换过**） | `crypto::open_with_aad` 失败 | `21801`（503） |
| 未知行状态（DB 报错） | `get_webhook` 抛错 | 透传（500） |

⚠️ **四者共用 `21801` 是刻意的**（与 `mail_digest::service::resolve_sender` 同款）：调用方
（HTTP handler / cron job）对这四类的可行动动作**完全相同** —— 都是「去把推送通道配好」。
⚠️ **密文解不开在本域被转成 `21801` 而不是让 crypto 的 `21701` 直接冒出来**：`21701` 的
字面义是「邮箱发信通道不可用」，让推送域吐一个邮件域的码会让前端错误文案串台。

⚠️ **`target_is_available` 会尝试解密**（`mail_digest::sender_is_available` 只做 DB 存在性
检查）。选它的理由与代价见 §8.3 第 4 条。

## 5. 状态域约定（无编译期保障）

- 本域**没有枚举字面量下发给前端**（与 mail 域的 SMTP 预设不同：webhook 地址形态只有一种，
  前端一条正则即可覆盖）。
- ⚠️ **地址正则两边各一份**：后端 `dto::WEBHOOK_URL_REGEX`、前端
  `weComWebhookSchema.ts::WECOM_WEBHOOK_URL_RE`，**人工同步**，漂了不会编译失败。
  另外两条常量（`WECOM_WEBHOOK_NAME_MAX = 50` / `WECOM_WEBHOOK_URL_MAX = 500`）同理。
  ⚠️ 长度口径：后端按 **char** 判，前端按 JS 的 `String.length`（**也是 UTF-16 code unit**）——
  ⚠️ **对含 emoji 的名字两边会不一致**（一个 emoji 在后端算 1 字、在前端算 2）。真实 key 恒
  为 ASCII uuid，不受影响；管理员起的名字里出现 emoji 时前端会**更早**拒绝（更严，不会更松）。

## 6. 移除记录

**本域 2026-10-18 新立，没有任何移除项**（无端点下线、无字段删除、无 VO 裁剪、无旧路径
可下线）。`grep -rn "wecom" src/modules/wecom/` 里没有任何「已删除」的登记，因为没有历史。

⚠️ 但本轮**新增/改动**的东西跨了两个文件，逐条登记在此：

| 改动 | 位置 | 说明 |
|---|---|---|
| 新增 AAD 显式版 `seal_with_aad` / `open_with_aad` / `open_with_key_aad` | `src/infra/crypto.rs` | 纯**增量**：`seal` / `open` / `seal_with` / `open_with` 的签名与行为**一行未变**（内部重构出 `seal_impl` / `open_with_key_aad` 两个实现体，`subject` 只进报错文案） |
| `.nest("/cron/wecom", …)` | `src/modules/mod.rs::v2_router()` | 第 4 条 `/cron` 前缀族注释已同步（`wecom` 不再是「将来的」） |
| `pub mod wecom;` | `src/modules/mod.rs` | 模块声明 |

刻意**没有**做的事：

| 没做的事 | 原因 |
|---|---|
| `PUT` / `PATCH` / `DELETE` 端点 | 全仓零这三种 HTTP 方法 |
| 新增 env（`WECOM_WEBHOOK_*`） | 主密钥复用 `MAIL_ACCOUNT_SECRET`；超时用模块常量 `sender::HTTP_TIMEOUT_MS`，待接线 |
| `url_configured` 出参 | 建档时地址必填 ⇒ 恒 true 的死字段 |
| 掩码列（表里存一份） | 迁移已在上一批定稿（append-only），掩码从明文派生即可；多一列就多一处会与明文不同步的数据 |
| `/test` 写流水 | 它是诊断动作，不是推送；写进去会让「今天推了几次」失真 |
| 密钥轮换迁移流程 | 与 mail 域同一个缺口，**未实现**（§8.3 第 1 条） |

⚠️ 本表只登记**本通道域自身**没做的事。内容域 `wecom_daily`（含 `src/task/wecom_daily.rs`
与 `docs/api/wecom-daily.md`）**已经落地**，它是本域唯一的消费者 —— 见 §7「与 `wecom_daily`
域的关系」。

## 7. 与 WS 的关系

- **本域不发任何 WS 事件**：6 个端点里 `/test` 会出网、其余只写本域自有表，前端的其它页面
  不会因它失效。
- **本域不消费 WS 事件**：没有失效通道。webhook 列表的新鲜度靠前端自身的 query 策略
  （挂载 refetch + 保存后显式 invalidate），与全仓其它配置型域一致。
- ⚠️ 与 `wecom_daily` 域的关系**也不经过 WS**：后者在推送时**实时查本域的表**
  （`service::resolve_target`），不靠事件 ⇒ 「管理员刚在另一个页面改了通道」这条路径
  **不需要**任何通知机制，下一次推送自然看到新值。
- ⚠️ 若将来要把「推送失败了」推到前端，**必须**新增 `WsEvent` 变体并在本节登记 ——
  不要借用现有的 `DashboardEvent` 变体（那会让大屏去 invalidate 一个与它无关的面板）。

## 8. 表依赖与前端配套

### 8.1 读 / 写的表

| 用途 | 表 | 本域关系 |
|---|---|---|
| webhook 实体（读 + 写） | `t_wecom_webhook` | **owned**（`T*` 全列投影在 `model.rs`） |
| 「是否被引用」判定 | `t_wecom_daily_config` | **只读**（跨域，1 条 COUNT，见下） |
| 加密主密钥 | 无（env `MAIL_ACCOUNT_SECRET`） | — |

- **域外模块依赖 = 0**：本域**不 import** 任何别的域的 service / repo。
  `wecom_domain_depends_on_no_other_domain`（`assert_no_foreign_domain` 扫
  `src/modules/wecom/**`）在 CI 里逐条钉住这一点。
- ⚠️ **「跨域只读」与「跨域 import」是两回事**：那条 COUNT 的 SQL **写在本域**
  （`repo.rs::count_daily_configs_referencing`），只查一个标量 —— 这是本仓对
  `dashboard` / `statistics` / `admin` / `scheduled_task` 都用过的既定 pattern。护栏扫的是
  `crate::modules::<他域>` 的**代码路径**引用，不是 SQL 表名。
- 反向依赖存在且**刻意**：`wecom_daily` 域 import 本域的 `service` / `sender`
  （`resolve_target` / `target_is_available` / `send_probe` / `send_markdown`）。方向是
  **内容域 → 通道域**，通道域对内容域零引用。

### 8.2 前端配套改动清单

| 落点（`frontend` 仓） | 说明 |
|---|---|
| 路由 / 视图 | `/wecom` 页（菜单已在 `seeds/menu.sql` 登记：`scheduled_task` 定时任务 → `wecom` 企业微信）。页面上方用 **tab** 区分推送类型，**当前只有「webhook 配置」一个 tab** |
| webhook 配置 UI | 「自定义名称 + webhook 地址」两个字段；**没有**「默认」勾选框（置默认是独立按钮） |
| api 层 | 6 个函数：`fetchWeComWebhooks` / `createWeComWebhook` / `updateWeComWebhook` / `deleteWeComWebhook` / `setDefaultWeComWebhook` / `testWeComWebhook` |
| 权限 | 菜单仅授予 `MANAGER`；页面路由守卫按同一角色判定（6 个端点全部 `require_role(Role::Manager)`） |
| **地址输入框** | 提交时用**后端同款正则** `^https://qyapi\.weixin\.qq\.com/cgi-bin/webhook/send\?key=\S+$` 自查（§5）。⚠️ 放宽/收紧必须与后端 `dto::WEBHOOK_URL_REGEX` **同一次改动一起改** |
| **地址输入框（改档时）** | 服务端**从不回显地址明文**（只给掩码）⇒ 编辑表单必须显示为「已配置（不修改则留空）」这类占位提示，**不要**尝试回填 |
| **改档提交** | 不改地址时**不要**发 `webhook_url` 或发空串 / 纯空白（两者都表示「沿用旧密文」） |
| **测试连通按钮** | body 发 `{}`（⚠️ **不要**带 `version`）；按钮上要有明确的等待态（真发一次外网请求，最长 5s 超时） |
| **`/test` 的错误展示** | `21802`（502）的 message 里**含**企微的 `errcode` + `errmsg`（如 `93000: invalid webhook url`）⇒ **原样展示**，那是排障的唯一线索；文案里**不会**有地址与 key（可以放心显示） |
| 删除的提示 | 被引用时后端返 `21808` + 409，message 里带引用份数与该去哪儿改 —— 前端**原样展示**即可 |
| 雪花 ID | `WebhookOut.id` 是 **JSON string**，不要按 `number` 声明 |
| **OCC body** | `deleteWeComWebhook` / `setDefaultWeComWebhook` 的 body **必须带 `version`**（从列表行取，与改档同款）。⚠️ 发 `{}` 得到的是 **HTTP 422 纯文本**（axum 提取器拒了，连 `code` 都读不到）⇒ 按 `err.response.data.code` 分类的错误处理会在这里抛 `undefined`，落到「未知错误」分支 |
| **删除 / 测试的响应** | 两者都是 `R<()>`（`data: null`）⇒ **不要**对它们做 Zod 守门，直接忽略 `data` |
| **列表掩码** | `url_masked` 已是掩码 ⇒ 前端**不要**再自己截一次、也不要试图还原 key |
| 错误处理 | `21803`（400，地址非法）/ `21804`（400，名称非法）/ `21806`（404，不存在或已软删）/ `21807`（409，重名）/ `21808`（409，被引用禁删）/ `21801`（503，主密钥被换导致整列表打不开）/ `21802`（502，推送失败）。⚠️ `21807` 在新建与改档两条端点上**共用同一句**提示，不要按端点写两套文案 |
| **部署顺序** | ⚠️ **后端先上（或与前端同批）**：本域 6 条端点是全新端点，旧前端没有调用点、不调即可 ⇒ 没有硬阻断。真正的顺序约束在**菜单 seed**（`seeds/menu.sql` 的 `wecom` 行必须先落，否则管理员看不到入口） |

### 8.3 已知偏差登记

⚠️ 本节是「已知的数字 / 口径不一致及产品决议（处理还是不处理）」的**唯一**登记处。

**1. ⚠️ `MAIL_ACCOUNT_SECRET` 的密钥轮换未实现（安全缺口）**：GCM 没有密钥轮换机制，
换 `MAIL_ACCOUNT_SECRET` 会让 `t_wecom_webhook` 里**所有**地址**与** `t_mail_account` 里
所有授权码**永久解不开（`21801` / `21701`）。正确处置是「逐行解密 → 换新密钥重新加密」的
迁移，**该流程未实现**。运维换密钥前必须先把两张表 dump 出来。**不要**为了「让旧密文还能
用」而加一条 env 兜底解密路径 —— 那正是要避免的东西。

**2. ⚠️ 列表「任一行解不开 ⇒ 整个列表 503」是刻意的**：掩码是从明文派生的（表里没有掩码列），
而解不开最常见的成因是「运维把主密钥换了」。逐行降级成占位会让列表页看起来「都配好了、
只是显示不出地址」，把问题一直藏到真推送那天 ⇒ 宁可当场 503。lib 单测不覆盖这一条（需
构造一条坏密文），集成测试也未覆盖（需要换掉进程级主密钥）。⚠️ **这是一处已登记的覆盖空洞**。

**3. ⚠️ 「被引用禁删」有 TOCTOU 窗口**：`delete_webhook` 是「先查引用、再软删」两条语句，
不在同一事务 ⇒ 存在极窄的窗口：查完之后、删之前，另一份配置刚好改成指向本 webhook，产出
的是一个指向已软删行的配置。后果是**推送时以 `21801` 暴露**。**有意不修** —— 为一个配置型
实体引入跨表约束 / 分布式锁不划算，而故障面是「发不出推送 + 一条明确错误」，不是数据损坏。

**4. ⚠️ `target_is_available` 会解密，`mail_digest::sender_is_available` 不会**：
选「会解密」的理由是 cron 注册闸问的是「到点能不能真的发出去」—— 密文解不开时注册那个 job
必然每天产出一条 `FAILED` 流水并占掉官方的 20 条/分钟限频配额，而摘掉 job 是零成本的。
⚠️ **代价**：主密钥被换过这一类**运维问题**在配置页上会显示成「通道不可用」（`false`），
管理员据此会去检查「是不是没配通道」而不是去查 env。⇒ **内容域若把这个布尔用在配置出参
（`webhook_configured`）上，必须在页面上把「通道不可用」与「通道未选」分开写文案**。

**5. ⚠️ `is_default` 不参与发送**（§4.3）：改它不改变任何消息的落点。前端若把「默认」
做成一个看起来「不选通道就会用它」的开关，会与实际行为不符。

**6. ⚠️ 集成测试必须自己把测试主密钥写进进程 env**：`crypto::seal_with_aad` /
`open_with_aad` 每次重读 env，而集成测试进程**不走** `AppConfig::from_env`。
`tests/wecom/main.rs::ensure_mail_secret_env` 用 `OnceLock` + `unsafe { set_var }` 串行写一次
（Rust 2024 的 `set_var` 是 unsafe）。⚠️ 与 `tests/scheduled_task` / `tests/mail_digest`
**各设各的**（不同 binary = 不同进程，不共享 env）。

**7. ⚠️ `/test` 的成功路径（企微返 `errcode=0`）无法在 CI 覆盖**：那需要一个真实可用的群
机器人地址。本域的集成测试覆盖的是**失败路径**（`errcode=93000 invalid webhook url`，实测
真打到了 `qyapi.weixin.qq.com` 并拿到 `HTTP 200`）+ `classify` 的四条纯函数分支。
⇒ 「企微真的收到了这条 text」这条链路**未经验证**；首次上线时建议人工点一次 `/test`。

**8. ⚠️ `/test` 需要外网**：出网不通（DNS 失败 / 超时 / 代理拦截）时，`probe_*` 那几条用例
**仍然全绿**（网络层失败与 `errcode != 0` 都归到 `21802`）⇒ 它们不会因为「公司断网」变红，
也就不会在断网时误报。代价是「测试真的打通了」这件事在 CI 里不可区分（见上一条）。

**9. ⚠️ `/test` 的 body 是空对象**：完全不读 body 的任何字段，发 `{"version":0}` 也能进到
handler。前端发 `{}` 即可；**不要**给 `/test` 加 `version`（它不改那一行，加了反而会让
「version 必填」那条模式在前端误导）。

**10. ⚠️ 与 `wx::wecom_client.rs` 的关系是「同名不同物」**：两者除了域名相同外零交集
（§ 顶部对照表）。⚠️ 排障时看错文件会得出完全错误的结论（找不到 `corpid` 配置、或以为要
清理 Redis token 缓存）。
