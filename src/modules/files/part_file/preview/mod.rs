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
//! | PENDING · 到期 · 有 job id | `ci_job_id IS NOT NULL` | describe：Success → READY；Failed → 21117；其余 → `touch_poll` 退避 → PENDING | 查询 ×1 |
//! | READY | `state = 'READY'` | 直接返 `pages` | **无**（不打 python、不打 COS） |
//! | FAILED | `state = 'FAILED'` | `reset_to_pending` 后按 PENDING 处理（**自愈**） | 见上 |
//!
//! ⚠️ **`reset_to_pending` 的谓词是 `state = 'FAILED'`，不是 `state <> 'PENDING'`** ——
//! 后者会把 `READY` 一并重置，而 READY 是「已收敛」的终态，重置它等于把一个本该
//! **零 IO** 返回的请求变成一次重复的 CI 提交（CI 按页计费 = 重复花钱）。这条
//! 谓词是「READY 不打任何外部依赖」那条契约的**唯一**实现点，改它前先想清楚。
//!
//! ### ⚠️ 并发下**不重复提交** CI job 的唯一机制是唯一索引
//!
//! 两个请求同时 cache miss 时，两边都会 `find_by_seed` = None。若靠应用层
//! 「先查再插」防重复，两边都会以为自己该提交 ⇒ **同一个内容提交两个 CI job**，
//! 而 CI 按页计费 ⇒ 真金白银的重复开销。
//!
//! 故 `insert_pending` 用 **`ON CONFLICT (preview_seed) WHERE deleted_at IS NULL
//! DO NOTHING`**，把裁决权交给 `uk_t_file_preview_seed`：只有 `rows_affected == 1`
//! 的那个调用才是「登记者」，才允许提交；`0` 行的那个直接返 PENDING。
//! 这也是建表注释里说「独立成表」的第二个理由。
//!
//! ### 退避常数
//!
//! [`service::POLL_BACKOFF_SECONDS`] = **2s**、上限
//! [`service::POLL_BACKOFF_MAX_SECONDS`] = **5s**。当前是**固定 2s**、无递增
//! （本表没有「已轮询次数」列，加递增要改 schema，而 CI 通常几秒内收敛）；
//! 上限常量是给「将来加递增退避」预留的接缝。
//!
//! ### `FAILED` 是**可恢复**的
//!
//! `mark_failed` 打的是终态，但它**不终止这份预览**：下次调用走 `reset_to_pending`
//! 重新排队。所以「CI 失败一次」不会让用户永远看到失败 —— 重新点一次预览就重来。
//! 反之「提交失败」（打 python 失败）**不**写 `FAILED`（那是 CI 的终态语义，
//! 这里只是这次没送出去），行保持 `PENDING` / `ci_job_id IS NULL`，下轮自动重试。
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
//! | **补偿 1** | 图纸本来就是 5 个角色（含 `ShelfAccount` 的报工台账号）都能看的；预览不扩大「谁能看图纸」这个集合，只放大「拿到手的东西能不能外发」 |
//! | **补偿 2** | 路径里绝不带文件名（上一节）⇒ 不可枚举 |
//! | **补偿 3** | `preview_seed` 是内容哈希，不可反推 `object_key` / 图号 |
//! | **补偿 4** | 可回收：`previews/` 前缀的 ACL 是**单独**一条，不动它就只影响预览图；关掉它即断链（源 PDF 走 `obj/` 前缀的预签，**不受影响**） |
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
//! ## ⚠️ 已知偏差登记
//!
//! | # | 偏差 | 状态 |
//! |---|---|---|
//! | 1 | 预览图 URL **不带内容版本**：同 seed 重渲（CI 镜像升级等）URL 不变，浏览器缓存会让用户看到旧图，除非手动清缓存 | **未修**（要修得给 URL 加一档「渲染版本号」，需要 python CI 侧一并产出，属跨仓改动） |
//! | 2 | **python 侧端点尚未实现**：本模块打的是 `POST/GET /api/v1/files/ci-preview-jobs`，截至 2026-10-13 `backend-python/` 无该实现。未部署 python 端时走 `NoopPyBackend` 返 21118（HTTP 502），**不会静默成功** | **跨仓待办**（python 侧实现前，本端点在生产恒 502） |
//! | 3 | `preview_seed` 在 `content_sha256 IS NULL` 的历史行上**退回按 `object_key` 哈希**，非内容寻址 ⇒ 同一份内容挂不同文件名会重复跑 CI | **未修**（要回填历史行哈希得重读 COS 对象，属独立立项） |
//! | 4 | `next_poll_at` 的退避**固定 2s、无递增**，长文档（大 PDF 几百页）渲染超过 ~10s 时前端会持续轮询到收敛 | **未修**（本表无「轮询次数」列；上限常数 `POLL_BACKOFF_MAX_SECONDS=5s` 已预留接缝） |
//! | 5 | 预览图 public-read **无过期、无撤销**（除改 COS ACL）；且无法按零件关闭 | **有意保留 + 已登记**（用户拍板选项 A；见「安全取舍反转」一节） |
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
//! - `repo.rs` —— `t_file_preview` 的全部读写（6 个静态方法，**用 `query!` 宏**，
//!   理由见该文件模块 doc）
//! - `model.rs` —— `TFilePreview` 行结构
//! - `service.rs` —— 状态机 + seed 推导 + URL 拼接 + python 转发
//! - `dto.rs` —— `PreviewOut` 出参

pub mod dto;
pub mod model;
pub mod repo;
pub mod service;
