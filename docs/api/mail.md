# scheduled_task 域 API（邮件通道：邮箱 + SMTP 预设）

> **2026-10-16 新建域**。需求（用户原话）：「定时任务菜单的名称结构重新设计。一级菜单：
> 定时任务，二级菜单：邮件。页面上方使用 tab 区分不同的邮件类型，当前只有一种：交期汇总。
> 邮箱账号和授权码可以在页面中配置」。
>
> ## ⚠️ 2026-10-18 URL 硬切：前缀进了 `/cron` 域组
>
> | 旧路径 | 新路径 |
> |---|---|
> | `/api/v2/mail/*` | **`/api/v2/cron/mail/*`** |
>
> **旧路径 404，无 alias**（合仓内 `scan` / `files` / `print_template` / `wx` 一贯做法）。
> 需求出处（用户原话）：「api挂载到 /cron/wecom_daily 下，邮件的API也移动到 /cron 下」。
>
> `/cron` 是**定时任务域组**的统一 API 前缀，其下四个子域：
>
> | 子域 | 职责 | 模块 | URL |
> |---|---|---|---|
> | `mail` | **通道**（本文件） | `src/modules/scheduled_task/` | `/api/v2/cron/mail/*` |
> | `mail-digest` | **内容** | `src/modules/mail_digest/` | `/api/v2/cron/mail-digest/*` |
> | `wecom` | **通道**（企业微信群机器人） | `src/modules/wecom/` | `/api/v2/cron/wecom/*` |
> | `wecom-daily` | **内容**（每日总结推送） | `src/modules/wecom_daily/` | `/api/v2/cron/wecom-daily/*` |
>
> ⚠️ **`wecom` / `wecom-daily` 两行在本轮只是登记**（2026-10-18 建表 + 开错误码段，
> 模块代码随下一批落地）；本文件描述的 6 条端点表逐字有效。
>
> ⚠️ **菜单 path 不跟着 API 前缀变**：菜单 seed 的 `path='/mail'` 与前端路由
> `path: 'mail'` **保持不变**，变的只是后端 URL。
>
> 守卫：`tests/scheduled_task/accounts.rs::old_mail_prefix_returns_404`
> （实跑旧前缀断言 404；同用例先钉新前缀 200 作正向对照）。
>
> 代码位置：`src/modules/scheduled_task/`（六件套）、加密基建 `src/infra/crypto.rs`、
> 主密钥解析 `src/infra/config.rs::MailAccountSecret`、
> 迁移 `migrations/20261016000000_001_scheduled_task.sql`、
> 菜单 `seeds/menu.sql`（`scheduled_task` 定时任务 → `mail` 邮件 → `wecom` 企业微信，
> 均仅 MANAGER，路径 `/mail` 与 `/wecom`）。
>
> ## 域范围：只管**通道**，不管内容与调度
>
> | 归本域 | 归 `mail_digest` 域 |
> |---|---|
> | `t_mail_account`（邮箱实体） | `t_mail_digest_config`（收件人 / 内容口径 / 调度 / **选哪个邮箱**） |
> | SMTP 预设元数据（host / port / tls_mode） | `t_mail_digest_log`（发送流水） |
> | 授权码的加密与出入库 | 交期汇总的渲染与发信编排（`mail_digest::service::resolve_sender`） |
>
> 切分线的理由：**「有哪些发信通道」与「用通道发什么」是两个独立演进的职责**。加第二个
> 邮件类型（订单变更通知、月度报表）时前者不动、后者各自加自己的内容配置表；而邮箱、
> 预设、授权码加密这一整套只需要建一次。
>
> ## 凭据怎么保护（三条硬约束，逐条都有反例）
>
> 1. **加密入库，不是明文、也不是散列**：`encrypted_secret` 存 AES-256-GCM 密文
>    （`base64(nonce12 ‖ ct ‖ tag16)`，AAD = `t_mail_account.secret.v1`），主密钥走 env
>    `MAIL_ACCOUNT_SECRET`。散列（bcrypt）取不回明文，而 SMTP 发信必须把授权码交给
>    lettre ⇒ 只能是**可逆**的对称加密。
> 2. **掩码不回显，出参里连掩码都没有**：本域刻意**不**设 `secret_configured` 布尔、
>    **不**设 `secret_fingerprint` 列、**不**回显 `abc***def` 式的掩码。建邮箱时授权码
>    **必填** ⇒ 「是否配了授权码」恒为 true，是死字段；死字段会让人误以为「授权码可以
>    以后再补」，进而写出「先把邮箱建好、授权码后补」的流程。掩码同样是一条**不需要存在
>    的**泄漏面（前端没有任何算法要用它）。
> 3. **被 `t_mail_digest_config.sender_account_id` 引用 ⇒ 禁止删除**（`21708` + 409），
>    否则配置会指向一条死行，页面看着正常、直到定时任务真发信才以 `21701` 暴露。

## 1. 端点表

**全部 6 条都是 Manager 独占**（角色守卫写在各 handler 第一行，逐字
`current.require_role(Role::Manager)?`）。前缀 `/api/v2/cron/mail`。

| 方法 | 路径 | 权限 | 一句话 |
|---|---|---|---|
| `GET` | `/api/v2/cron/mail/meta` | Manager | SMTP 预设表（**纯函数、不查库**）：`{presets:[{value,label,host,port,tls_mode}]}` |
| `GET` | `/api/v2/cron/mail/accounts` | Manager | 邮箱列表（默认邮箱排最前）；**绝不返回** `encrypted_secret` |
| `POST` | `/api/v2/cron/mail/accounts` | Manager | 新建（`name` / `preset` / `account` / `secret`）；**不设默认** |
| `POST` | `/api/v2/cron/mail/accounts/{id}` | Manager | **全量覆写**；body **必含 `version`**（OCC）；`secret` 空串或缺省 ⇒ **沿用旧密文** |
| `POST` | `/api/v2/cron/mail/accounts/{id}/delete` | Manager | 软删；被交期汇总配置引用 ⇒ `21708`；body **必含 `version`** |
| `POST` | `/api/v2/cron/mail/accounts/{id}/set-default` | Manager | 置默认；body **必含 `version`**（目标行的） |

- **只用 GET / POST**：本仓 HTTP 方法**零** PUT/PATCH/DELETE（`mail_digest` 域
  2026-10-13 实测核实：全仓 `.put(` / `.patch(` / `.delete(` 的命中只有 opendal /
  redis 客户端调用）。故「改」走 `POST /accounts/{id}`、「删」走
  `POST /accounts/{id}/delete` —— 与既有 `POST /prod/batches/{id}/receive-from-outsource`
  同款约定。回归用例 `accounts::put_and_delete_verbs_are_not_routed` 钉住「将来有人加
  `DELETE /accounts/{id}` 时会当场红」。
- **权威路由清单**：`src/modules/scheduled_task/handler.rs::ROUTES`（lib 单测逐条比对
  `router()` 源码，加了 `.route()` 忘登记就在单测里当场红）。
- 挂载点：`src/modules/mod.rs::v2_router()` 的 `.nest("/cron/mail", …)`
  （**`/cron` 父前缀下的两段**，2026-10-18 硬切；第 2 段 `/mail` **刻意不是
  kebab-case** —— 与菜单 seed 的 `path='/mail'`、前端路由同一个词）。
  ⚠️ 与同族的 `/cron/mail-digest` **同段数但无冲突**：matchit 在**第 2 段**先静态
  判别（`mail` vs `mail-digest` 互不为对方的前缀），`/cron/mail/accounts` 与
  `/cron/mail-digest/config` 在第 2 段就分道。
  ⚠️ **matchit 注册顺序核对**（`/cron` 前缀族的四条理由，逐条见 `src/modules/mod.rs`
  `v2_router()` 内 `/cron` 段的注释）：① 每个子域**各自一个 `.nest()`**、没有合并
  router ⇒ 两张 matchit 表互不参与；② 第 2 段互不为前缀；③ 段数相同也在第 2 段分道；
  ④ 本域与 `mail-digest` 的路由表内部本就无 catch-all 竞争（`mail-digest` 6 条
  **全是静态段**）。
- ⚠️ **matchit 注册顺序无约束**（域内）：本域最长段数 3（`/accounts/{id}/delete`），
  最短 1（`/meta` / `/accounts`）；`Path<i64>` 只出现在第 2 段，而**没有任何 2 段的
  静态路径** ⇒ 不存在「静态段被 catch-all 吞成 400」的风险。

## 2. `MailAccount` 逐字段

### 2.1 `GET /accounts` / `POST /accounts` / `POST /accounts/{id}` / `POST /accounts/{id}/set-default` 出参（`MailAccountOut`）

`id` 是 **JSON string**（雪花 ID 防 JS 精度截断，`shared::types::serialize_i64`）。

| 字段 | 类型 | 取值 / 来源 | 可空 |
|---|---|---|---|
| `id` | string | `t_mail_account.id`（雪花） | 否 |
| `version` | number | OCC 版本；新建为 `0`，每次覆写 / 删除 / 置默认自增 1 | 否 |
| `name` | string | 自定义名称（`varchar(50)`，未软删行内唯一）。⚠️ **同时是发件人显示名**（见 §4.2） | 否 |
| `preset` | string | `NET_EASE_163` / `TENCENT_QQ`（无 DB ENUM，Rust 侧校验；未知值 ⇒ `21704`） | 否 |
| `preset_label` | string | 预设中文标签（「网易 163」/「QQ 邮箱」）。前端**不必**另存映射 | 否（库里有未知预设时为**空串**，见 §8.3） |
| `account` | string | SMTP 登录账号（163/QQ 都是完整邮箱地址）。**同时是发件人地址** | 否 |
| `host` | string | **由预设推导**（不存 DB）；空串 = 库里有未知预设 | 否（可为空串） |
| `port` | number | **由预设推导**（两家都 465）；未知预设时为 `0` | 否 |
| `tls_mode` | string | **由预设推导**：`WRAPPED`（隐式 TLS）/ `STARTTLS` / `NONE` | 否（可为空串） |
| `is_default` | bool | 至多一个未软删行为 `true`（唯一索引保证） | 否 |
| `created_at` / `updated_at` | string | `YYYY-MM-DD HH:MM:SS`（Asia/Shanghai naive） | 否 |

⚠️ **出参里没有任何 `secret` / `mask` 字段**，掩码也没有。回归用例
`accounts::create_encrypts_secret_and_never_echoes_it` 与
`accounts::list_accounts_hides_secret_and_orders_default_first` 把出参整体序列化成文本后
断言「不含 `secret` / `mask` 子串、不含授权码明文」。

### 2.2 `GET /accounts` 出参（`MailAccountListOut`）

`{ items: MailAccountOut[] }`。**排序**：`is_default DESC, id ASC`（默认邮箱排最前）。

⚠️ **不返回密文、也没有 `secret_configured`**（理由见顶部「凭据怎么保护」第 2 条）。

### 2.3 `POST /accounts` 入参（`CreateAccountRequest`）

⚠️ **无 `version`**：这是新建，没有「别人先改过」这回事。

| 字段 | 类型 | 必填 | 校验（失败 ⇒ `21704` + 400） |
|---|---|---|---|
| `name` | string | **必填** | trim 后非空；≤50 **字**（`.chars().count()`，不是字节数） |
| `preset` | string | **必填** | `MailAccountPreset::parse`；错误信息列出全部合法值 |
| `account` | string | **必填** | trim 后非空；拒空白 / CR / LF；必须含 `@`；≤120 字 |
| `secret` | string | **必填** | trim 后非空；拒控制字符（CR/LF 等）；≤200 字 |

- **`name` 重复** ⇒ `21707` + 409（**不是** `40901`：CLAUDE.md 明令 40901 只表达 OCC，
  业务唯一键冲突须有专用码）。⚠️ 报错文案里回显的是**校验并 trim 后**的名称（新建与
  改档两条端点同款，`service::create_account` / `update_account`），不是请求里的原文 ⇒
  前端拿它做「与输入框当前值比对」时要先 trim，否则会看到「明明不一样」而找不到焦点。
- ⚠️ **新建永远不设默认**（`is_default` 恒 `false`）。置默认是
  `POST /accounts/{id}/set-default` 那条独立端点的事。理由：建邮箱与「让它成为默认发信
  通道」是两种意图，混在一个请求里会出现「新建第三个邮箱顺手把默认抢走」这种完全没被
  要求的副作用。**代价**：新装的库第一个邮箱不是默认，前端在「还没有默认邮箱」时应显示提示。
- ⚠️ **不存在 CUSTOM 自填主机/端口**：用户口径明确「不做 CUSTOM」，host/port/tls_mode
  全部由预设推导（§3）。

### 2.4 `POST /accounts/{id}` 入参（`UpdateAccountRequest`，全量覆写）

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `version` | number | **必填** | OCC 锚点。**缺失 → HTTP 422 纯文本**，见下「⚠️ 422 不是信封」 |
| `name` / `preset` / `account` | string | **必填** | 同 §2.3 |
| `secret` | string | 否 | **缺省或 trim 后为空 ⇒ 沿用旧密文**（不覆写、不报错；「空」指 trim 后零长，故 `"   "` 也算沿用）；**非空 ⇒ 全量走 §2.3 的 `secret` 校验矩阵**（控制字符 / ≤200 字，失败 `21704`） |

⚠️ **`secret` 不是必填是有意的**：管理员改个名称时前端拿不到授权码明文（服务端从不
回显），强制必填等于逼前端每次把「不知道的东西」原样传回来。空串是前端唯一能表达
「我没改它」的方式。

⚠️ **刻意的不对称：纯空白 / 纯控制字符的授权码在两条路径上判据不同**。「空 ⇒ 沿用」
的判定发生在 `trim()` **之后**（`service::update_account`），所以 `"   "`、`"\r\n"`
这类串在**改档**路径被当作「管理员没填」而静默沿用旧密文；同样的串在**新建**路径是
`21704`（§2.3 的校验矩阵把空 / 纯空白 / 控制字符一律拒）。⚠️ 这是刻意的：改档路径上
「没填」在前端能表达的形态就是空串与带空格的串（输入框清空后提交常带空白），把它们判成
「填了一个空白授权码」会让管理员撞上一个只报错、看不出是自己没填的坑；而新建路径**没有**
「沿用」这个选项，一个空白授权码无论如何都是配置错误，必须当场拒。⚠️ 代价是「改档页面上
一个只含空格的授权码会被静默忽略、不报错」—— 排障时若发现改不动授权码，先看提交的
`secret` 是不是全空白。

⚠️ **非空 `secret` 与新建走同款校验**（`service::validate_secret`）：控制字符
（CR/LF 等，会进 SMTP `AUTH` 命令行）与 `SECRET_MAX_CHARS = 200` 两道闸在**改档**路径上
同样生效。回归：`accounts::update_secret_with_control_chars_returns_21704_and_keeps_ciphertext` /
`accounts::update_secret_over_200_chars_returns_21704`。

⚠️ **换 `account` 时必须同时换 `secret`**：两个字段是**独立**填的，服务端不校验二者
匹配。改了登录账号却沿用旧授权码 ⇒ SMTP 以 535 拒绝认证，而**这个失败要等到真发信那一刻
才暴露**（配置页一切正常、`smtp_configured` 仍是 true，只在流水里留下一行 `FAILED`）。
这是「`secret` 可沿用」这条便利的固有代价：**改档页面上必须提示「换账号请一并重填
授权码」**。

⚠️ **`name` 撞同名未软删行 ⇒ `21707` + 409**（同 §2.3）：本端点是**全量覆写**，
`name` 每次都参与写入 ⇒ 撞 `uk_t_mail_account_name_alive` 与新建撞车是同一件事。service
层捕 `is_unique_violation`（SQLSTATE 23505）转本码，**不是** 500、也不是 `40901`。
回归：`accounts::update_name_to_a_live_duplicate_returns_21707_and_keeps_row_intact`。

⚠️ **全量覆写**：`name` / `preset` / `account` 三项都必须给全，**没有**「只改一项」的
三态语义。`is_default` 与 `encrypted_secret`（未给时）**不在**本端点的可写集合里。

**响应回填的是提交后的新 `version`**（读回最新行），前端据此做下一次提交。

**⚠️ 缺 `version` 是「HTTP 422 纯文本」，不是业务信封**：`/accounts/{id}`、
`/accounts/{id}/delete`、`/accounts/{id}/set-default` 三条**都必收 `version`**（本表带
`version` 列 ⇒ 照 CLAUDE.md §8，**软删也不例外**）。缺字段时 axum 在 `Json` 提取器层就拒
了，而**本仓没有 `JsonRejection` 的自定义处理器**（全仓零处定制 `handle_error` / `JsonDataError`）
⇒ 走 axum 默认路径：

| | 值 |
|---|---|
| HTTP | **422 Unprocessable Entity** |
| 响应体 | **纯文本**（含缺失字段名，如 ``Failed to deserialize the JSON body into the target type: missing field `version` ``） |
| `code` 字段 | **没有** —— 不是 `R<T>` 信封，前端 `resp.data.code` 恒 `undefined` |

⇒ **前端必须按 HTTP 状态码处理这一类失败，不能指望读 `code`**（尤其 `delete`：它有
`21708`「被引用禁删」这个业务码，**别把 422 也当成「删不掉」的一种**）。回归：
`accounts::update_without_version_returns_422_plain_text` /
`accounts::set_default_requires_version`（都断言了 `!raw.contains("\"code\"")`）。

### 2.5 `POST /accounts/{id}/delete` 与 `POST /accounts/{id}/set-default` 入参

两者都只有 `{"version": <number>}` 一个字段，**必填**、无 `#[serde(default)]`
⇒ 缺失同样是 **HTTP 422 纯文本**（形态见 §2.4 末的「缺 `version`」小节：不是业务信封、
没有 `code` 字段）。

⚠️ **软删也吃 OCC**：本表带 `version` 列，按 CLAUDE.md §8「所有对带 `version` 列的表的
写端点，body 必须显式收客户端 `version` 且必填」，软删也不例外。这正是「读-再-比式
隐式 OCC」被明令禁止的场景：两人同时打开同一个编辑页，一人改了名、一人删了行，
后者会静默成功。

### 2.6 `GET /meta` 出参（`MailMetaOut`）

`{ presets: [{value, label, host, port, tls_mode}, …] }`（顺序 = 展示顺序）。

⚠️ **纯函数、不开事务、不查库**，且**一个字都不含凭据**（host/port/tls_mode 是让管理员
判断「选这家能不能连上」的公开信息，授权码不在这里）。前端**不要**硬编码这份列表。

## 3. 预设枚举（`MailAccountPreset`，封闭）

⚠️ **只做两家，且两家都要授权码而非登录密码** —— 管理员十有八九会填成密码。每个变体
的 doc 里都点明了这一点，页面上也应给出提示文案。

| value | label | host | port | tls_mode |
|---|---|---|---|---|
| `NET_EASE_163` | 网易 163 | `smtp.163.com` | 465 | `WRAPPED` |
| `TENCENT_QQ` | QQ 邮箱 | `smtp.qq.com` | 465 | `WRAPPED` |

- **465 = 隐式 TLS**（连接建立即 TLS），与 `mail_digest::mailer` 的
  `WRAPPED = 465` 语义一致。TLS 参数的 **SNI 域名**取 `host`。
- ⚠️ **不开放 587 / STARTTLS**：lettre 的 `Tls::Opportunistic` 在服务端不支持时会
  **降级明文继续发**（凭据可能裸传），而预设的语义是「选一个能用的服务商」，不该让
  管理员去挑一个协议档位。`MailTlsModeKind` 的另两个变体（`StartTls` / `None`）仍保留
  在枚举里（lettre 侧的映射是穷举 `match`），但**当前无生产方**。
- 未知字面量 ⇒ `21704` + 400（错误信息列出全部合法值）。lib 单测
  `preset_parses_all_variants_and_lists_legal_values` 钉住这条。
- **host / port / tls_mode 不存 DB**：存了列就有「两个预设同一台主机但端口被改成 25」
  这种组合，而 SMTP 配错的现场表现是一个来自网络层的握手异常 —— 比「预设选错了」难查
  一个数量级。

## 4. 口径表

### 4.1 加密口径（`src/infra/crypto.rs`）

| 项 | 值 |
|---|---|
| 算法 | `ring::aead::AES_256_GCM`（AES-256-GCM） |
| 密文信封 | `base64(nonce(12) ‖ ciphertext(N) ‖ tag(16))` |
| nonce | **每次 seal 新生成**（`SystemRandom` + `generate()`）；复用 nonce = GCM 失效且**不报错** |
| AAD | `b"t_mail_account.secret.v1"`，seal / open 两侧逐字同常量 |
| 主密钥 | env `MAIL_ACCOUNT_SECRET`，32 字节，接受 64 位 hex（`openssl rand -hex 32`）或 44 位标准 base64 |

- **AAD 绑定用途**（防「密文被搬到别的表/别的字段再用」）：AEAD 把 AAD 一并纳入认证，
  换 AAD 解即失败。改这个常量 = 存量密文全部解不开（故带 `v1` 后缀）。
- **解密失败复用 `21701 BIZ_MAIL_NOT_CONFIGURED`**（HTTP 503），**刻意不新开
  `SECRET_DECRYPT_FAILED`**：最常见的成因是「运维把 `MAIL_ACCOUNT_SECRET` 换了」，那是
  **运维侧**问题、不是「某个配置项没填」，前端对它没有可执行动作。`mail_digest::service
  ::resolve_sender` 会额外打一行 `tracing::error!` 点名这个可能。
- ⚠️ **`seal` / `open` 每次调用都重读 env**（签名不带 state，见该模块 doc 的取舍段）。
  启动期的 fail-fast 在 `config.rs::AppConfig::from_env`，用的是**同一个解析函数**
  ⇒ 「启动通过」与「运行期可解密」不可能分叉。
- ⚠️ **`MAIL_ACCOUNT_SECRET` 缺失 / 长度不对 ⇒ 启动直接 bail**，不接受「截断 / 补零 /
  取前 32 字节」：一把被悄悄截断的密钥仍是「能用的密钥」，于是配置看着没问题、发信时全失败。

### 4.2 发信时的字段映射（`mail_digest::mailer::ResolvedSender`）

| lettre 侧 | 来源 | 说明 |
|---|---|---|
| `relay(host)` / TLS SNI | `MailAccountOut.host`（预设推导） | 不存 DB |
| `port` | `MailAccountOut.port` | 同上 |
| `Tls::*` | `MailAccountOut.tls_mode` | 同上 |
| `Credentials` 用户名 | `t_mail_account.account` | — |
| `Credentials` 口令 | `crypto::open(encrypted_secret)` | 明文只活在 `ResolvedSender` 里 |
| `From` 地址 | `t_mail_account.account` | 旧 env `MAIL_FROM_ADDRESS` 已删除 |
| `From` 显示名 | `t_mail_account.name` | ⚠️ 旧 env `MAIL_FROM_NAME` 缺省是硬编码的「汇森 ERP」 |

⚠️ **发件人显示名取「管理员给邮箱起的自定义名」**（2026-10-16 的行为变更）：管理员可以把
发件人显示成「汇森告警邮箱」「财务日报」这类名字 —— 收件人一眼能看出这封信是谁发的，
比一个全局常量有用。⚠️ 若自定义名里含 `<` / `>`，lettre 的 `Mailbox` 解析会失败
（`From` 头非法）⇒ 发信以 `21702` 失败并落一行 FAILED 流水。集成测试正是用这一点构造
**零网络 IO** 的确定性失败（§8.3）。

⚠️ **发件人地址缺省回退到登录账号**：企业邮箱的登录账号本身就是可发信地址，而
「显式发件地址与认证主体不一致」的场景 SMTP 服务器会直接拒收 ⇒ 不再提供这个自由度。

### 4.3 唯一性与并发口径

| 约束 | 保障方式 |
|---|---|
| 至多一个未软删的默认邮箱 | 唯一索引 `uk_t_mail_account_one_default ON t_mail_account ((true)) WHERE deleted_at IS NULL AND is_default` |
| 同名邮箱（未软删）唯一 | `uk_t_mail_account_name_alive ON t_mail_account ((name)) WHERE deleted_at IS NULL` |
| 软删后可同名再建 | 两个索引都以 `deleted_at IS NULL` 为前提（回归用例 `delete::name_can_be_reused_after_soft_delete`） |

⚠️ **`set-default` 的两条 UPDATE 必须在同一事务内「先降后升」**，事务的作用是
**失败时不留半截状态**：先升新的、再降旧的会在第二条 UPDATE 上撞 23505（唯一索引只允许
一个 `is_default = true`），所以顺序必须反过来；而更常见的中途失败是**升格那一步自己失败**
（body 的 `version` 与库里对不上 ⇒ `40901`）。两条语句若各自分别自动提交，「降已生效 +
升没生效」就是库的终态 ⇒ **一个默认邮箱都没有**（§8.3 第 5 条已登记这个状态是合法的）。
放进同一事务后任何一步失败都整笔回滚 ⇒ 要么原默认还在、要么新默认已升上。回归：
`accounts::set_default_demotes_the_previous_default`。

⚠️ **⚠️ 开事务消不掉的是「两个管理员同时对不同邮箱点置默认」**：两个事务各自看不见对方
未提交的结果（各自要降的那行不是对方要升的），于是后到者的 `promote_to_default` 撞
`uk_t_mail_account_one_default`（23505）。这一支在 service 层捕 23505 转 `40901`
（**不是** 500：处置是刷新列表后重试，判据照 CLAUDE.md 对 `mail_digest` 配置单例并发
首次初始化的同款处理），详见 §8.3 第 11 条。⚠️ **不要**为了让这个窗口消失而去动那个
partial 唯一索引 —— 它是「至多一个默认」的**唯一**保障。回归：
`accounts::concurrent_set_default_on_two_accounts_never_returns_500`（守的是**允许的结果
集合**：两次都 200（串行跑完时本该如此）**或** 一个 200 一个 `40901`，另加「至少一个赢家」
与「终态恰好一个默认」两条；**刻意不锁死「恰好一个赢家」** —— 那等于把某个具体交错顺序
钉成断言，调度一变就间歇红）。

⚠️ **删掉默认邮箱不会把默认自动转给别的邮箱**（库里变成「零个默认」）：把默认自动转给
某个「别的邮箱」需要一条选择规则（最近建的？第一个？），而任何一条规则都会让「我以为 A
是默认」在某天悄悄变成 B —— 比「没有默认」危险得多。回归：
`delete::deleting_the_default_leaves_no_default`。

### 4.4 「被引用禁删」的窗口（**已知偏差**）

`delete_account` 是「先查引用、再软删」两条语句，不在同一事务 ⇒ 存在极窄的窗口：
查完之后、删之前，另一份配置刚好改成指向本邮箱，产出的是一个指向已软删行的配置。
后果是**发信时以 `21701` 暴露**（`resolve_sender` 对「不存在」与「已软删」给同一个
结果）。这是一个**有意的取舍**：为一个配置型实体引入跨表约束 / 分布式锁不划算，而故障面
是「发不出信 + 一条明确错误」，不是数据损坏。

### 4.5 定时任务的注册判据（跨域，但写在这里）

`task::mail_digest::reschedule_inner` 注册 cron job 的条件是：

```
t_mail_digest_config.enabled == true
  && frequency != MANUAL_ONLY
  && sender_account_id 指向一个未软删的 t_mail_account 行
```

⚠️ **判据只到「邮箱行在不在」为止，不解密授权码**：解密失败要等到真发信那一刻才暴露
（`21701` + 流水里一行 `FAILED`），而那一次同样留痕，可观测性不缺。理由与
`ConfigOut.smtp_configured` 同款 —— 解密失败的原因对「要不要注册 job」这个决定没有帮助。

⚠️ **不满足条件 ⇒ 摘掉已注册的 job**（走 `swap_job` 的「先摘旧、再决定挂不挂」），
所以管理员配好邮箱之后**不必重启进程**就能让定时任务生效。

## 5. 状态域约定（无编译期保障）

- 后端常量：`scheduled_task::dto::MailAccountPreset::ALL`（2 项）+ 每个变体的
  `as_str()` / `host()` / `port()` / `tls_mode()`。
- 前端对应：邮箱编辑表单的「服务商」下拉，选项源**取 `GET /api/v2/cron/mail/meta` 的
  `presets[]`**（`label` 是后端给的中文文案，`host` 用于展示「选这家会连到哪台服务器」）。
- 两者是**人工同步**关系，Rust 字面量与 TS 侧之间没有编译期约束，漂了不会编译失败。
  ⚠️ 抄一份到前端是最容易犯的错：加一家服务商时 host/port/tls_mode 三处都会变，
  而 TS 那份不会被任何检查发现。**正确做法是取 `/meta`。**
- ⚠️ **`preset` 在库里可能是未知字面量**（手工改库 / 将来加了预设但没同步 / 从旧版本
  回滚）。`MailAccountOut::from_row` 对它**降级而不丢行**：`preset` 原样回显，
  `preset_label` / `host` / `tls_mode` 退化为空串、`port` 为 `0`。理由：列表页整体 500
  会让人以为「邮箱全没了」，而实际上只是某个值不认识。回归：
  `account_out_never_exposes_secret_material` 同文件的
  `unknown_preset_degrades_instead_of_panicking`（lib 单测）。

## 6. 移除记录

**本域 2026-10-16 新立，没有任何移除项**（无端点下线、无字段删除、无 VO 裁剪）。

但本轮**移除**的东西跨了两个域，逐条登记在此以防后人来「补齐」：

| 移除项 | 位置 | 原因 / 去处 |
|---|---|---|
| `MAIL_SMTP_HOST` / `_PORT` / `_TLS_MODE` / `_USERNAME` / `_PASSWORD` | `infra::config`（**已删**） | 凭据改由页面配置。**无 env 兜底路径**，运行时不读 |
| `MAIL_FROM_ADDRESS` / `MAIL_FROM_NAME` | `infra::config`（**已删**） | 发件地址 = 登录账号、显示名 = 自定义名称（§4.2） |
| `MAIL_DIGEST_ENABLED` | `infra::config`（**已删**） | 「是否注册 cron」的判据从 env 换成 DB（§4.5） |
| `MailConfig` 结构体（含 `is_enabled()`） | `infra::config`（**已删**） | 判据换成「存在可用发信通道」 |
| `MailTlsModeKind::parse` + `LEGAL_VALUES` | `infra::config`（**已删**） | env 字符串解析随 env 一起删；**枚举类型本身保留**（预设表与 lettre 映射要用） |
| `MetaOut.smtp` 段 | `mail_digest`（**已删**） | 逐条换了归属，见 `docs/api/mail_digest.md` §2.6 的对照表 |
| 菜单 code `mail_group` / `mail_digest` | `seeds/menu.sql`（**已软删**） | 换成 `scheduled_task` / `mail`；旧行的 `t_role_menu` 授权残留由 `seeds/rbac.sql` §5 清理 |

刻意**没有**做的事：

| 没做的事 | 原因 |
|---|---|
| `PUT` / `PATCH` / `DELETE` 端点 | 全仓零这三种 HTTP 方法 |
| CUSTOM 预设（自填主机 / 端口） | 用户口径明确「不做」；猜端口会让「587 + WRAPPED」这类组合被静默改写 |
| `secret_configured` 出参 / `secret_fingerprint` 列 | 建邮箱时授权码必填 ⇒ 恒 true 的死字段；死字段会误导出「以后再补授权码」的流程 |
| 授权码掩码回显 | 没有消费方，且是一条不需要存在的泄漏面 |
| 「删除时级联清空引用」 | 反向耦合（通道域 → 内容域），且用户口径是「禁删 + 提示」；回归 `delete::delete_referenced_account_returns_21708` |
| 启动自检 / 一次性引导导入（把历史 env 凭据搬进表） | 本轮口径是「彻底废弃」，不留任何 env 兜底 |

## 7. 与 WS 的关系

- **本域不发任何 WS 事件**：6 个端点里全部只写本域自有表，前端的其它页面不会因它失效。
- **本域不消费 WS 事件**：没有失效通道。邮箱列表的新鲜度靠前端自身的 query 策略
  （挂载 refetch + 保存后显式 invalidate），与全仓其它配置型域一致。
- ⚠️ 与 `mail_digest` 域的关系**也不经过 WS**：后者在读配置时**实时查本域的表**
  （`service::resolve_sender`），不靠事件 ⇒ 「管理员刚在另一个页面改了邮箱」这条路径
  **不需要**任何通知机制，下一次读配置自然看到新值。
- ⚠️ 若将来要把「定时任务发信了」推到前端，**必须**新增 `WsEvent` 变体并在本节登记 ——
  不要借用现有的 `DashboardEvent` 变体（那会让大屏去 invalidate 一个与它无关的面板）。

## 8. 表依赖与前端配套

### 8.1 读 / 写的表

| 用途 | 表 | 本域关系 |
|---|---|---|
| 邮箱实体（读 + 写） | `t_mail_account` | **owned**（`T*` 全列投影在 `model.rs`） |
| 「是否被引用」判定 | `t_mail_digest_config` | **只读**（跨域，1 条 COUNT，见 §8.2） |
| 加密主密钥 | 无（env `MAIL_ACCOUNT_SECRET`） | — |

- **域外模块依赖 = 0**：本域**不 import** 任何别的域的 service / repo。
  `scheduled_task_domain_depends_on_no_other_domain`（`assert_no_foreign_domain` 扫
  `src/modules/scheduled_task/**`）在 CI 里逐条钉住这一点。
- ⚠️ **「跨域只读」与「跨域 import」是两回事**：那条 COUNT 的 SQL **写在本域**
  （`repo.rs::count_digest_configs_referencing`），只查一个标量 —— 这是本仓对
  `dashboard` / `statistics` / `admin` 都用过的既定 pattern。护栏扫的是
  `crate::modules::<他域>` 的**代码路径**引用，不是 SQL 表名。
- 反向依赖存在且**刻意**：mail_digest 域 import 本域的 `dto` / `repo`
  （`service::resolve_sender`）。方向是**内容域 → 通道域**，通道域对内容域零引用。

### 8.2 前端配套改动清单

| 落点（`frontend` 仓） | 说明 |
|---|---|
| 路由 / 视图 | 新增 `/mail` 页（菜单已在 `seeds/menu.sql` 登记：`scheduled_task` 定时任务 → `mail` 邮件，`sort_order=10`）。页面上方用 **tab** 区分邮件类型，**当前只有「交期汇总」一种** |
| 邮箱配置 UI | 「自定义名称 + 服务商预设下拉 + 登录账号 + 授权码」四个字段；**没有**主机 / 端口输入框 |
| api 层 | 6 个函数：`fetchMailMeta` / `fetchMailAccounts` / `createMailAccount` / `updateMailAccount` / `deleteMailAccount` / `setDefaultMailAccount` |
| 权限 | 菜单仅授予 `MANAGER`；页面路由守卫按同一角色判定（6 个端点全部 `require_role(Role::Manager)`） |
| **枚举** | **不要在前端硬编码预设 / host / port** —— 一律取 `GET /api/v2/cron/mail/meta` 的 `presets[]`（§5） |
| **授权码输入框** | 每次提交都是**全量**的：修改时若不重新填授权码，**不要**发 `secret` 或发空串（两者都表示「沿用旧密文」） |
| **`sender_account_id`** | 交期汇总配置页的发信邮箱下拉，选项源是 `GET /api/v2/cron/mail/accounts`；提交时按 **JSON string**（与出参对称，`deserialize_i64_opt` 只收字符串或 `null`） |
| 授权码字段的交互 | **建完之后前端拿不到明文**（出参不回显）⇒ 编辑表单必须显示为「已配置（不修改则留空）」这类占位提示，**不要**试图回填 |
| 删除的提示 | 被引用时后端返 `21708` + 409，message 里带引用份数与该去哪儿改 —— 前端**原样展示**即可 |
| 雪花 ID | `MailAccountOut.id` / `ConfigOut.sender_account_id` 是 **JSON string**，不要按 `number` 声明 |
| 错误处理 | `21701`（503）「发信通道不可用」与 `21702`（502）「SMTP 挂了」**排查方向相反**，UI 文案必须区分；`21706` / `21707` / `21708` 分别是「邮箱不存在」「重名」「被引用禁删」 |
| **改名撞重名** | `21707` 与新建同款（§2.4）⇒ 表单上「名称已存在」的提示**两条端点共用同一句**，不要按端点写两套文案 |
| **改档时的授权码** | 换 `account` 必须一并重填 `secret`（§2.4 的 535 提醒）；不改则**不发 `secret` 或发空串** |
| **OCC body** | `deleteMailAccount` / `setDefaultMailAccount` 的 body **必须带 `version`**（从列表行取，与改档同款）。⚠️ 发 `{}` 得到的是 **HTTP 422 纯文本**（axum 提取器拒了，连 `code` 都读不到）⇒ 按 `err.response.data.code` 分类的错误处理会在这里抛 `undefined`，落到「未知错误」分支 |

⚠️ **部署顺序：后端先上（或与前端同批）**。本轮 `ConfigOut` **新增了 `sender_account_id`
字段**（`mail_digest` 域，见 `docs/api/mail_digest.md` §2.1），而前端
`mailDigestConfigSchema`（`views/mail/digest/mailDigestSchema.ts`）是 **`.strict()`**
⇒ **旧前端拿到新出参会整包 `parse` 抛 ZodError ⇒ 配置页白屏**（不是某个字段
`undefined`、也不是静默少一列 —— strict 是全包拒）。本域 6 个端点是全新端点，旧前端
没有调用点、不调即可，**唯一的硬阻断就是那个新键**。

### 8.3 已知偏差登记

⚠️ 本节是「已知的数字 / 口径不一致及产品决议（处理还是不处理）」的**唯一**登记处。

**1. ⚠️ `MAIL_ACCOUNT_SECRET` 的密钥轮换未实现（安全缺口）**：GCM 没有密钥轮换机制，
换 `MAIL_ACCOUNT_SECRET` 会让 `t_mail_account` 里**所有**授权码永久解不开（`21701`）。
正确处置是「逐行解密 → 换新密钥重新加密」的迁移，**该流程未实现**。运维换密钥前必须先
把该表 dump 出来。**不要**为了「让旧密文还能用」而加一条 env 兜底解密路径 —— 那正是
本轮要删掉的东西。

**2. ⚠️ 「被引用禁删」有 TOCTOU 窗口**：详见 §4.4。后果是「配置指向已软删行」，
发信时以 `21701` 暴露。

**3. ⚠️ 「查了引用但没在同一事务」不是 bug**：见 §4.4 的取舍理由。

**4. ⚠️ 新建邮箱不自动成为默认**：见 §2.3。前端需在「还没有默认邮箱」时显示提示。

**5. ⚠️ 删掉默认邮箱后库里可能「零个默认」**：见 §4.3。

**6. ⚠️ 未知 `preset` 的降级形态**：见 §5。`preset_label` / `host` / `tls_mode` 退化为空串、
`port` 为 `0`，**不是** 404/500。

**7. ⚠️ `smtp_configured` 不校验解密**：`mail_digest::ConfigOut.smtp_configured`（语义
2026-10-16 已从「env 侧是否具备发信能力」改写为「是否已选定且**存在**的邮箱」）**不**试
解密。主密钥被换过导致存量密文全解不开这一类，在配置页上仍显示 `true`，只在真发信时
以 `21701` 暴露。理由与 §4.5 相同：解密失败的原因对页面上的「能不能发」没有帮助，
且每次读配置都解一次密会把 `GET /config` 变成写密集操作。

**8. ⚠️ 集成测试里「SMTP 层失败」的造法在本轮换过**：host/port 由**编译期预设表**固定，
原先「把 SMTP 指向 `127.0.0.1:1`」这种确定性造法**不再可复现**。现改用「自定义名含
`<` / `>` ⇒ `From` 头 `Mailbox` 解析失败」（§4.2 末）——**零网络 IO**，且 `deliver` 里
「`mailer::send` 返回 `Err` → 记 FAILED 流水 → 返 `21702`」的两段代码逐字未变。
回归：`tests/mail_digest/send.rs::send_to_unreachable_smtp_returns_21702_and_records_failed_log`
（用例名沿用旧名，doc 里已登记这次造法变更）。

**9. ⚠️ 集成测试必须自己把测试主密钥写进进程 env**：`crypto::seal` / `open` 每次重读
env，而集成测试进程**不走** `AppConfig::from_env`（`test-support` 直构 `AppConfig`）。
故 `tests/scheduled_task/main.rs::ensure_mail_secret_env` 与
`tests/mail_digest/main.rs::ensure_mail_secret_env` 各自用
`OnceLock` + `unsafe { std::env::set_var }` 串行写一次（Rust 2024 的 `set_var` 是 unsafe）。

**10. ⚠️ 部署前置：三步，一步都不能跳（升级存量部署时尤其）**：① **先写
`MAIL_ACCOUNT_SECRET`**（`openssl rand -hex 32`，写进**进程 env**，不是只在 `.env` 里
放着）—— 缺失或长度不对 ⇒ `AppConfig::from_env` **启动直接 bail**（§4.1 末条），
**任何没改 env 的存量部署升级后起不来**；② 再建至少一个邮箱（`t_mail_account` 表空 ⇒
交期汇总的「发信邮箱」下拉是空的、`sender_account_id` 恒 `NULL`）；③ 再回交期汇总页
选定发信邮箱、打开 `enabled`、配好收件人与时刻。

⚠️ **②③ 之前定时任务静默不注册**：调度器**总会**建起来
（`task::mail_digest::run` 已经没有「未配置 SMTP 就早退」的前置闸），而
`reschedule_inner` 的判据（§4.5）不满足 ⇒ 只摘不挂，**全程只有两行 `info!`**、不报错、
不落流水。现场看到的是「每天该发的汇总邮件没来，日志里什么都好着」。⚠️ 旧 env
`MAIL_SMTP_*` 那套本轮**整轮删除、不做迁移、不留兜底**（§6）⇒ 升级后必须由管理员
在页面上重填一遍，**「升级前先备份 env 里的 SMTP 密码」这条例行习惯在这里没有用**。

**11. ⚠️ `set-default` 的并发唯一索引冲突转 `40901` 而不是 500**（2026-10-16）：两个
管理员同时对**不同**邮箱点置默认时，后到者的 `promote_to_default`
撞 `uk_t_mail_account_one_default`（23505）。这一支**事务消不掉**（两条语句在各自事务里
看不见对方未提交的结果，§4.3），所以在 service 层捕 23505 并转 `40901` —— 语义是「本次
提交基于一份已过期的『谁是默认』快照，处置是刷新后重试」，判据照 CLAUDE.md 对
`mail_digest` 配置单例并发首次初始化的同款处理。⚠️ **不要**改那个 partial 唯一索引
（它是「至多一个默认」的唯一保障）。⚠️ **两次请求可能串行跑完、两个都是 200**，那是
合法结果而不是「闸门没生效」（早先回归用例断言「恰好一个赢家」，等于把某个交错顺序钉成
断言、调度一变就间歇红；现已改为「允许结果集合 + 至少一个赢家 + 终态恰好一个默认」）。
回归：`accounts::concurrent_set_default_on_two_accounts_never_returns_500`。
