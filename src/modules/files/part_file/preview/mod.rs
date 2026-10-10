//! `files::part_file::preview` 子模块 —— COS CI 图纸预览（惰性轮询，零后台任务）
//!
//! 2026-10-13 PR2 新建。挂在 `part_file::handler::router()` 的
//! `/{file_id}/preview`，完整 URL `GET /api/v2/files/objects/{file_id}/preview`。
//!
//! ## 端点表（本子模块 1 条）
//!
//! | 方法 | URL | 角色 | 入参 | 出参 |
//! |---|---|---|---|---|
//! | `GET` | `/api/v2/files/objects/{file_id}/preview` | M / C / I / CNC / **SHELF** | 无（仅 path 段 `file_id`） | `R<PreviewOut>` |
//!
//! ## 这条端点解决什么
//!
//! 报工台（`/scan/*`）的工控机要显示 PDF 图纸。在浏览器里直接嵌 `<iframe src=PDF>`
//! 在车间浏览器上表现不稳定（缩放 / 滚动 / 移动端都不可控），而把整个 PDF 传给
//! 前端再自己转码则要引入 pdf.js。本端点走第三条路：**服务端把 PDF 逐页光栅化成
//! JPG**，前端只拿一串 `<img>` 直链，天然分页、天然缩放、天然移动端可用。
//!
//! 渲染跑在**腾讯云 CI**（python 侧持有凭据与渲染镜像），**按页计费**且是**异步
//! 任务**。rust 侧因此只做三件事：算种子、提交任务、轮询状态。
//!
//! ## ⚠️ 惰性状态机：**零后台任务、零新增 cron**
//!
//! 状态推进**完全由前端下一次调用本端点驱动**。没有后台定时器意味着：
//!
//! - 不存在「进程重启 / 部署后 job 卡在 PENDING 永不收敛」这类定时器问题；
//! - 不新增一个要随域下线而清理的常驻任务（本仓已有 `task::auto_complete` 一条，
//!   不想再加一条只为跑 CI 轮询的）。
//!
//! 代价是**没人调就没人推进**：一份没人再打开的预览会永远停在 `PENDING`。
//! 这是可接受的 —— 没人看的预览不值得花钱继续渲染，且下次有人打开时会自愈。
//!
//! | 分支 | 判定条件 | 动作 | 外部依赖 |
//! |---|---|---|---|
//! | 不可预览 | `kind ∉ {DRAWING, ASSEMBLY_MASTER}` 或 `file_type ≠ PDF` | 21116 / HTTP 400 | 无 |
//! | seed 未命中 · 登记成功 | `find_by_seed` = None 且 `insert_pending` 返 1 行 | 读回该行（`next_poll_at = now()`，**立即到期**）→ 3c 提交 CI → `mark_submitted` → 返 PENDING | 提交 ×1 |
//! | seed 未命中 · 登记冲突 | `insert_pending` 返 0 行（撞 `uk_t_file_preview_seed`） | **直接返 PENDING，绝不自己提交** | **无** |
//! | PENDING · 未到期 | `now() < next_poll_at` | 返 PENDING（节流） | **无** |
//! | PENDING · 到期 · 无 job id | `ci_job_id IS NULL` | 补提交 → `mark_submitted` → 返 PENDING | 提交 ≤1 |
//! | PENDING · 到期 · 有 job id | `ci_job_id IS NOT NULL` | describe：Success → READY；**四种终局失败**（`Failed` / `Cancel` / `Pause` / `NotExist`）→ FAILED + 21117；其余 → `touch_poll` 退避 → PENDING | 查询 ×1 |
//! | READY | `state = 'READY'` | 直接返 `pages` | **无**（不打 python、不打 COS） |
//! | FAILED | `state = 'FAILED'` | `reset_to_pending` 后按 PENDING 处理（**自愈**） | 见上 |
//!
//! ⚠️ **`reset_to_pending` 的谓词是 `state = 'FAILED'`，不是 `state <> 'PENDING'`** ——
//! 后者会把 `READY` 一并重置，而 READY 是「已收敛」的终态，重置它等于把一个本该
//! **零 IO** 返回的请求变成一次重复的 CI 提交（CI 按页计费 = 重复花钱）。这条
//! 谓词是「READY 不打任何外部依赖」那条契约的**唯一**实现点，改它前先想清楚。
//!
//! ### ⚠️ 并发下不重复提交 CI job：**两道闸**，各管一段
//!
//! ⚠️ **2026-10-13 review 第 1 轮 MAJOR-1 订正**：本节此前的头条声称「唯一索引是
//! **唯一**机制」，但 `insert_pending` 只在「首次登记」那一条路径被调用 ——
//! 「行已存在」的两条路径完全裸奔：
//!
//! | 闸 | 管的路径 | 机制 | 漏了会怎样 |
//! |---|---|---|---|
//! | `uk_t_file_preview_seed` 唯一索引 | **首次登记** | `insert_pending` 的 `ON CONFLICT DO NOTHING`，`rows == 1` 的才是登记者、才允许提交 | —— |
//! | `PreviewRepo::claim_submit` **条件 UPDATE 认领** | **重试** 与 **自愈** | 谓词 `state='PENDING' AND (ci_job_id IS NULL OR ci_job_id LIKE '__claiming__:%')`，只有 `rows == 1` 才允许打 python | 两个 CI job，按页计费 ⇒ **双份钱**，先写的那个变孤儿 |
//!
//! 两条裸奔路径（**这才是唯一索引帮不上忙的地方**）：
//!
//! - **路径 A（提交失败后的重试）**：行 = `PENDING / ci_job_id IS NULL /
//!   next_poll_at = T`。T 到点后两个请求同时读到「due + NULL」，**都**进 3d、
//!   **都** POST `/doc_jobs`。
//! - **路径 B（FAILED 自愈）**：`reset_to_pending` 的返回值此前**被丢弃**。T1 重置
//!   成功并提交了新 job；T2 重置得 **0 行**（状态已被 T1 改掉）却不看行数、
//!   无条件把内存行改成 `PENDING / ci_job_id = None`，继续走 3d **再提交一个**。
//!
//! 认领的标准写法就是「**条件 UPDATE 抢占**」而不是「先查再打 python」—— 后者
//! 查与打之间没有任何闸，是教科书式的 TOCTOU。认领成功后 `ci_job_id` 被写成
//! `__claiming__:<snowflake>` 占位 token（`service::CLAIM_TOKEN_PREFIX`），进程若
//! 在此与 `mark_submitted` 之间挂掉，下一次调用靠 3d 的 `needs_submit` 判据
//! （认 token 为「尚未提交」）恢复。
//!
//! 建表注释里说「独立成表」的第二个理由正是这个内容寻址 + 唯一索引的组合。
//!
//! ### 退避常数
//!
//! [`service::POLL_BACKOFF_SECONDS`] = **2s**、上限
//! [`service::POLL_BACKOFF_MAX_SECONDS`] = **5s**。当前是**固定 2s**、无递增
//! （本表没有「已轮询次数」列，加递增要改 schema，而 CI 通常几秒内收敛）；
//! 上限常量是给「将来加递增退避」预留的接缝。
//!
//! ⚠️ `retry_after_ms` 在**所有**返 PENDING 的分支上都必须传「距**现在**还差多久」
//! （即 `next_poll_at - now`），不能传 `next_poll_at - next_poll_at` —— 后者恒为
//! **0**，前端会立刻重调，退避形同虚设（忙轮询，每个请求都真打一次 python）。
//! `service.rs` 的 3d / 3e 两个分支各自都有这条纪律的注释。
//!
//! ### `FAILED` 是**可恢复**的
//!
//! `mark_failed` 打的是终态，但它**不终止这份预览**：下次调用走 `reset_to_pending`
//! 重新排队。所以「CI 失败一次」不会让用户永远看到失败 —— 重新点一次预览就重来。
//! 反之「提交失败」（打 python 失败）**不**写 `FAILED`（那是 CI 的终态语义，
//! 这里只是这次没送出去），行保持 `PENDING` / `ci_job_id IS NULL`，下轮自动重试。
//!
//! ⚠️ **「重新点一次」的次数由用户控制，不由轮询次数控制** —— 这是
//! `Failed` / `Cancel` / `Pause` / `NotExist` 四种终局失败统一判 `FAILED`
//! 的关键（详见下一节「CI 终局失败的落点」）。
//!
//! ## ⚠️⚠️ 安全条款：预览路径里**绝不能带文件名**
//!
//! `t_part_file.object_key` 的形状是 `{sha16}_{原文件名}`，而文件名是**图号**
//! （如 `A00123_rev2.pdf`）—— **可枚举**。预览目录是 public-read 的（下一节），
//! 把图号拼进路径等于**公开整张厂的图号清单**。
//!
//! 故路径成分只有三段，全是配置或哈希：
//!
//! ```text
//! {preview_base_url}/{preview_prefix}/{preview_seed}/p{n}.jpg
//! ```
//!
//! `preview_seed` 由 [`service::PreviewService::preview_seed`] 推导：
//!
//! - `content_sha256` 有值 ⇒ 取**前 32 位 hex**（sha256 前 16 字节 = 128 bit）；
//! - 为 NULL（历史行）⇒ 退回 `sha256(object_key)` 的**前 32 位 hex**。
//!
//! 退回分支取的是**摘要**，原始 `object_key`（含文件名）本身不进路径。
//! ⚠️ 退回分支因此**不是内容寻址**（同一份内容挂不同文件名会算出不同种子 ⇒
//! 重复跑 CI）—— 这只对 `content_sha256 IS NULL` 的历史行成立，新上传路径必写
//! sha256。根治要回填历史行的哈希（重读 COS 对象），属独立立项。
//!
//! ## ⚠️ 安全取舍反转：预览端点**放开** `ShelfAccount`（用户拍板的选项 A）
//!
//! ### 原本的取舍
//!
//! 2026-10-10 给报工台工控机放开图纸只读时，`GET /objects/{id}/url`（COS 预签直链）
//! **刻意排除** `SHELF_ACCOUNT`，理由是「链接到手就能脱离本后端直接 GET 并外发」：
//! 那是个 **1 小时有效**的预签 URL，工控机用 `GET /objects/{id}/content`
//! （后端代理、每次过 RBAC）就够，故不放开。
//!
//! ### 反转
//!
//! 本预览端点**放开** `SHELF_ACCOUNT`（5 角色：M / C / I / CNC / SHELF），
//! 因为它返回的是 `previews/` 前缀下的**永久不过期公开读直链** —— 比 1 小时预签
//! 窗口**更宽**：预签到期即失效，公开读永不过期。**这是对上述取舍的有意反转**，
//! 目的是让工控机账号能看图纸预览（报工台的核心刚需）。
//!
//! ### 风险与补偿
//!
//! | | |
//! |---|---|
//! | **风险 1** | 预览图永久公开可读，无过期时间、无撤销手段（除改 COS ACL） |
//! | **风险 2** | `t_part_file` 的 owner 是多态的（`PART` / `ASSEMBLY`）+ 客户端可任意指定 `file_id` ⇒ 工控机账号能取**任意**零件的预览，没有货架级 / 工单级收窄（本域权限模型止步于角色） |
//! | **风险 3** | 预览图等价于图纸的完整视觉副本，**绕过 COS 源 PDF 的任何权限** |
//! | **风险 4** | ⚠️ **`POST /objects/{id}/delete` 不回收预览**：该端点只软删
//! | `t_part_file` + 异步删 COS 上的**源对象**；`previews/<seed>/` 下的 JPG 与
//! | `t_file_preview` 行**都不动** ⇒ 图纸「删除」后，一份完整的视觉副本仍以
//! | public-read **永久可读**（攻击者只要事先存过 URL 就一直有效）。本表**只增
//! | 不减**，**无 TTL / 无 GC / 无级联** |
//! | **补偿 1** | 图纸本来就是 5 角色（含 `ShelfAccount` 的报工台账号）都能看的；预览不扩大「谁能看图纸」这个集合，只放大「拿到手的东西能不能外发」 |
//! | **补偿 2** | 路径里绝不带文件名（上一节）⇒ 不可枚举 |
//! | **补偿 3** | `preview_seed` 是内容哈希，不可反推 `object_key` / 图号 |
//! | **补偿 4** | 可回收：`previews/` 前缀的 ACL 是**单独**一条，不动它就只影响预览图；关掉它即断链（源 PDF 走 `obj/` 前缀的预签，**不受影响**） |
//! | **补偿 5（对应风险 4）** | 缓解只在**可发现性**层面：路径不含文件名 ⇒ 删图后没人能重新算出那个 URL。⚠️ **已持有 URL 的人不受影响**（这正是 public-read 无撤销的固有代价，与风险 1 同源） |
//!
//! ⚠️ **反向的操作风险**：本端点**没有** `POST` / `DELETE` 侧的配套端点，
//! 即无法「只为某个零件关闭预览」。要按零件收窄只能改代码或整条关 ACL ——
//! 这一点在验收时已知并接受（用户拍板选项 A 时的前提）。真要做，按 owner 侧加
//! 索引 + 在 service 里判 `CurrentUser.shelf_ids` 是独立立项。
//!
//! ## CDN / 直链契约（固定 URL，无签名）
//!
//! `pages[]` 里是**固定 URL**（无 query、无签名、无过期参数）。含义与后果：
//!
//! - 前端可以把它直接塞进 `<img src>`、`<link rel="preload">`、Service Worker
//!   缓存 —— 因为它不变，缓存命中率天然高；
//! - ⚠️ 也因此，**内容一旦变更 URL 不变**。缓解：seed 是内容哈希，内容变了
//!   就是另一个 seed ⇒ 另一个目录。唯一残留风险是「同内容重渲」（CI 版本升级导致
//!   同一 PDF 渲出不同图）URL 不变 —— 记在下方「已知偏差」第 1 条。
//!
//! ## 跨仓 CI 终态词表（2026-10-13 review 第 1 轮建立）
//!
//! python 的 `describe`（`GET /api/v1/files/ci-preview-jobs/{job_id}`）返的
//! 状态词，本模块**逐个识别**。判据的**唯一真源是**
//! [`service::PreviewService::parse_ci_status`]（纯函数，表驱动单测钉死在
//! `tests/files/preview.rs::parse_ci_status_word_table_is_exhaustive`）。
//!
//! | 原始词（大小写不敏感） | 本仓归一 | 词从哪来 | 谁负责识别 |
//! |---|---|---|---|
//! | `success` | `Success`（**仅当 `total_page_count > 0`**） | CI 官方 | rust |
//! | `failed` / `failure` | `Failed` | CI 官方 | rust |
//! | `cancel` / `canceled` / `cancelled` | `Canceled` | CI 官方 | rust |
//! | `pause` / `paused` | `Paused` | CI 官方 | rust |
//! | `notexist` / `not_exist` / `not_exists` / `nonexist` | `NotExist` | ⚠️ **python 自己合成**，CI 官方六态里**没有**这个值 | rust |
//! | `submitted` / `running` / `processing` / 空串 / 任何未知词 | `Running` | CI 官方进行中态 + 本仓兜底 | rust |
//!
//! ### ⚠️ `NotExist` 是 python **合成**的，不是 CI 的状态（跨仓契约，两边都必须知道）
//!
//! CI 官方六态只有 `Submitted` / `Running` / `Success` / `Failed` / `Pause` /
//! `Cancel`。python 侧把三种「查不到这个 job」的情形**合成为** `NotExist`：
//!
//! 1. 响应根节点带 `NonExistJobIds`（CI 对不存在 job 的固定返回）；
//! 2. `JobsDetail/Code ∈ {NotExistJobId, NoSuchJob, NotFound, …}`；
//! 3. CI 的任务记录**只保留一个月**，过期后查询即「查不到」。
//!
//! python **刻意不抛 404** 而是合成这个值（理由：「记录过期是确定性终局，rust
//! 轮询撞上它应按『查不到』处理而非无限重试」），且响应 schema 严格保持 5 字段
//! （**没加** `exists: bool`）⇒ 这个语义**只能由 `state` 承载**，前端与 rust
//! 都必须自己认这个词。
//!
//! ⚠️ **由此派生的仓间义务**：python 侧**只负责产出**这个词（不抛错、不改字段名、
//! 不加布尔位）；**识别与落点是 rust 的责任**。python 未来若新增合成词，必须
//! 同步本表与 [`service::PreviewService::parse_ci_status`] —— 本仓的兜底是
//! 「未知词当进行中」，代价是**不会自动识别新词**（落进无限轮询的老坑）。
//!
//! ## CI 终局失败的落点：`NotExist` / `Cancel` / `Pause` 一律 **FAILED + 21117**
//!
//! 三者都打 `FAILED`、返 21117 / HTTP 502，**不静默重提 job**（2026-10-13
//! review 第 1 轮的设计决策）。论证：
//!
//! **为什么不是「静默重提一个新 job」**（题面给的另一个选项）：
//!
//! - ⚠️ **钱**：CI **按页计费**。「`NotExist` 就重提」在「CI 系统性返回
//!   `NotExist`」（桶没开通 CI / python 侧映射写错 / 凭据失效）时会变成
//!   **无限提交 job** —— 每轮轮询提交一次，每次都真金白银，而用户一次图也
//!   看不到。本模块现有的三处省钱护栏（唯一索引并发闸门、`READY` 绝不重置、
//!   登记冲突方绝不自己提交）全部指向同一个取舍：**宁可不提交，也不要重复提交**。
//! - ✅ **成本上限明确**：判成 `FAILED` 后，本次调用**一次 submit 都不会发生**。
//!   「重新提交」只在**下一次调用**经 `reset_to_pending` 自愈路径发生 ⇒ 上限是
//!   **「用户点一次 = 最多一个 job」**，是用户驱动的，不是轮询驱动的。
//!   前端拿到的是 502（错误）而不是 `PENDING`，**轮询循环被打断** ⇒ 不存在
//!   「后端自己把自己打成无限提交」。
//! - ✅ **可观测**：静默重提会把「job 莫名消失」这类上游异常变成只留一行会被
//!   轮转掉的 `tracing::warn!`；判成 `FAILED` 则每次都 `mark_failed` 落
//!   `error_message`（含原因短语）+ 返 21117，排障时能直接查库。
//!
//! **`NotExist` 的主因判断**（决定了「该重试还是该报错」）：题面给的触发条件里
//! 「记录过期（> 1 个月）」在本项目**基本不可能是主因** —— `t_file_preview`
//! 的行生命周期远短于一个月（预览通常几分钟内收敛），而 job 是**登记当时**就
//! 提交的。故撞上 `NotExist` 更可能是 **CI 侧 job 记录异常丢失**，那是一件
//! **需要人看**的事，报出来比静默重试更合适。即便真是过期，判 `FAILED` 也
//! 不亏：下一次调用会重提，新 job 是新鲜的、必然查得到。
//!
//! **`Cancel` / `Pause` 不与 `NotExist` 混为一谈**：
//!
//! - `Cancel`（被取消）是 CI 的确定性终态，**不会自行变化** ⇒ 判 FAILED 显然正确。
//! - `Pause`（队列暂停）CI 侧**理论上可被 Resume**，但本系统**两侧都没有任何
//!   一侧会去做 Resume**（零后台任务、纯惰性轮询、没有运维控制台接进本模块）
//!   ⇒ 对本状态机而言它就是终点。判 `Running` 的后果是「永久转圈」，判
//!   `FAILED` 的后果是「一次 21117 + 用户再点一次自动重来」，后者有自愈路径兜底。
//!   ⚠️ **残留风险已登记**（见下方已知偏差 #5）：若 CI 存在「无需人工、某次
//!   `Pause` 会自动恢复」的场景，本判法会**过早判死**一份本可成功的预览；代价是
//!   「多跑一个 job」而不是「永远失败」。
//!
//! ## ⚠️ `Success` 但 0 页仍当进行中（**护栏，勿改**）
//!
//! CI 的 Success 快照**可能先于对象落盘**：报成功的那一刻 `total_page_count`
//! 还是 0。此时若直接判 `Success`，`mark_ready(0)` 会返回**空 `pages` 的 READY**
//! —— 前端拿到「渲染成功」却一张图都没有，且该行已是终态、**永远不会自愈**。
//!
//! ## ⚠️ 已知偏差登记
//!
//! | # | 偏差 | 状态 |
//! |---|---|---|
//! | 1 | 预览图 URL **不带内容版本**：同 seed 重渲（CI 镜像升级等）URL 不变，浏览器缓存会让用户看到旧图，除非手动清缓存 | **未修**（要修得给 URL 加一档「渲染版本号」，需要 python CI 侧一并产出，属跨仓改动） |
//! | 2 | **python 侧端点尚未实现**：本模块打的是 `POST/GET /api/v1/files/ci-preview-jobs`，截至 2026-10-13 `backend-python/` 无该实现。未部署 python 端时走 `NoopPyBackend` 返 21118（HTTP 502），**不会静默成功** | **跨仓待办**（python 侧实现前，本端点在生产恒 502） |
//! | 3 | `preview_seed` 在 `content_sha256 IS NULL` 的历史行上**退回按 `object_key` 哈希**，非内容寻址 ⇒ 同一份内容挂不同文件名会重复跑 CI | **未修**（要回填历史行哈希得重读 COS 对象，属独立立项） |
//! | 4 | `next_poll_at` 的退避**固定 2s、无递增**，长文档（大 PDF 几百页）渲染超过 ~10s 时前端会持续轮询到收敛 | **未修**（本表无「轮询次数」列；上限常数 `POLL_BACKOFF_MAX_SECONDS=5s` 已预留接缝） |
//! | 5 | 预览图 public-read **无过期、无撤销**（除改 COS ACL）；且无法按零件关闭 | **有意保留 + 已登记**（用户拍板选项 A；见「安全取舍反转」一节） |
//! | 6 | **CI 终局失败的落点是 `FAILED` + 21117，不静默重提 job**（`NotExist` / `Cancel` / `Pause` 三者同判）。代价：`Pause` 若在 CI 侧存在「自动恢复」的场景会被**过早判死**（多跑一个 job 而非永远失败） | **有意 + 已论证**（见「CI 终局失败的落点」一节，含「为什么不会变成无限提交 job」的论证） |
//! | 7 | **跨仓词表缺口（2026-10-13 review 第 1 轮已修 rust 侧）**：`NotExist` 是 python **合成**的词（CI 官方六态里没有），python 侧**只负责产出、不抛 404、不加 `exists` 位**；**识别与落点全部由 rust 负责**（`parse_ci_status`）。此前 rust 只认 `Success`/`Failed`，其余全落「进行中」⇒ `NotExist` / `Cancel` / `Pause` 会**无限轮询** | **已修（rust 侧）**；**python 侧需同步 doc**（把 `NotExist` 从「CI 官方状态」改述为「本仓合成」，位置清单见本节末） |
//! | 8 | **状态字段名跨仓两种写法**：`forward_ci_describe` 的响应模型字段是 **`state`**（python `CiPreviewJobStatus.state`，模块 docstring 的线上样例也是它），`status` 键在 python 侧**根本不存在**。本仓**两个键都读、`state` 优先**（读错 ⇒ 空串 ⇒ 落 `Running` 兜底 ⇒ 无限轮询）。⚠️ 2026-10-13 review 第 1 轮 MINOR-1 订正：此前实现是 `status` 优先、**顺序反了** | **已在本仓兜住**；两侧收敛到同一键名后可删掉 `status` 那一支 |
//! | 9 | **提交请求体的跨仓字段名错配（2026-10-13 review 第 1 轮 BLOCKER，已修）**：本仓发 `{src_object_key, preview_prefix, preview_seed}`，python 的 `CiPreviewJobRequest` 只有 `{src_object_key, dst_prefix}` 且 `dst_prefix` **必填** ⇒ pydantic 静默丢掉两个多余键 + FastAPI **422** ⇒ **每一次 submit 都失败**，行永远停在 `PENDING / ci_job_id IS NULL` | **已修**（body 改为 `dst_prefix` + 抽出与 `preview_urls` 同源的 `preview_object_prefix` 真源；跨仓契约由 `tests/files/preview.rs::submit_request_body_matches_python_schema` 钉死） |
//! | 10 | **`Success` 但 `total_page_count == 0` ⇒ 永远 PENDING 且每 2s 真打一次 python**（护栏的代价）：CI 若**永久**不返回页数（如 python 侧 `DocProcessResult` 结构变更导致该字段恒缺），行会**永不停**地以固定 2s 打 python（无递增退避，见 #4）⇒ **一次打不开的预览 = 一个永不停歇的 python 探针**。⚠️ **跨仓义务（前端）**：收到 **21117 / 21118** 时**必须停止自动重试**并回落 `GET /objects/{id}/content`（原始 PDF），**不得**按 `retry_after_ms` 无限重调 —— 每一次重试在「提交失败后自愈」这条路径上都是**一次新的 CI job**（按页计费） | **未修（有意）**：改成「N 次后判 FAILED」要改 schema（加轮询计数字段）。**该义务落在前端**，本仓已在 `PreviewOut` 的 `retry_after_ms` 语义与本表登记 |
//!
//! ### 需要 `backend-python` 同步的位置（本 PR 无法触及，python 在另一 worktree）
//!
//! | 文件 | 要改什么 |
//! |---|---|
//! | `core/ci.py` 的模块 / 函数 doc | 凡是把 `NotExist` 称作「CI 官方状态 / CI 六态之一」的地方，改述为「**本仓合成值**」并写明 3 个触发条件（`NonExistJobIds` 节点 / `JobsDetail.Code` 码 / 记录超期） |
//! | `api/v1/ci_preview.py` 的端点 doc | 同上；并建议在响应字段说明里注明「`state` 的取值集合 = CI 官方六态 ∪ {`NotExist`}，识别方（rust）以该集合为准」 |
//! | 两处 doc 的字段名 | 确认 `describe` 响应里状态字段到底叫 `state` 还是 `status`，与 rust 侧收敛成一个（rust 现读 **`state` 优先**、`status` 兜底，见偏差 #8） |
//! | 两处 doc 的终态语义 | 注明「`Pause` 在 rust 侧被判为终局失败（21117）且不自动重提」，避免 python 侧以为「发 `Pause` 会有人来 Resume」 |
//! | `core/ci.py` 的 `DocProcessResult` | ⚠️ 偏差 #10 的对照面：**`total_page_count` 必须恒有值**（哪怕是 0）。若 python 侧改结构导致该字段缺失，rust 的 0 页护栏会把预览永久挂在 PENDING 上反复打 python |
//!
//! ## 表依赖
//!
//! - 读：`t_part_file`（经 `PartFileRepoTrait::get_by_id` 取一行）
//! - 写：`t_file_preview`（**本域独占**，见建表注释「为什么独立成表」）
//! - 外部：`infra::py_backend::PyBackendClient` 的两个 CI 转发方法
//!
//! ## 事务边界
//!
//! 本端点**不开显式事务**（与 `POST /objects/confirm` 同款「service 自管」的
//! 例外）：它要**跨多次 HTTP 往返**（提交 / 查询 python），把整个状态机包在一个
//! PG 事务里会让连接被外部 IO 长时间占住。故整条链走**一个 `pool.acquire()`
//! 借来的连接**，每步写完立刻读回最新状态（见 `service::get_preview` 的注释）。
//!
//! ## 模块内文件
//!
//! - `repo.rs` —— `t_file_preview` 的全部读写（**8 个静态方法**，**用 `query!`
//!   宏**，理由见该文件模块 doc）。2026-10-13 review 第 1 轮补 `claim_submit`
//!   （条件 UPDATE 认领）与 `release_claim`（提交失败归还认领）⇒ 6 → 8
//! - `model.rs` —— `TFilePreview` 行结构
//! - `service.rs` —— 状态机 + seed 推导 + URL 拼接 + python 转发
//! - `dto.rs` —— `PreviewOut` 出参

pub mod dto;
pub mod model;
pub mod repo;
pub mod service;
