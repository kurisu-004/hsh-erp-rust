//! 后台定时任务：交期汇总邮件（2026-10-13 新增）
//!
//! 对照 [`super::auto_complete`]（裸 `tokio::time::interval` 循环）—— 本任务
//! 换成 `tokio-cron-scheduler`，因为需求要「按**配置的**时间与频率发信」
//! （每天 / 每周几 / 每月几号），间隔循环表达不了「每周一早上 8:30」这件事。
//!
//! ## 生命周期
//!
//! ```text
//! run(state, token)
//!   ├─ JobScheduler::new()（0.15 已改成 async）+ spawn(scheduler.start())
//!   ├─ reschedule_inner()：读配置 → **解析发信通道** → schedule::to_cron() → 注册 job
//!   └─ spawn(watcher)：await token.cancelled() → scheduler.shutdown()
//! ```
//!
//! `POST /api/v2/mail-digest/config` 成功后由 handler 调 [`reschedule`]，
//! **改完立刻生效、不必重启进程**。
//!
//! ## ⚠️ 判据从 env 换成 DB（2026-10-16）
//!
//! 原先 `run()` 的第一道闸是 `MailConfig::is_enabled()`，取值来自 env
//! （`MAIL_DIGEST_ENABLED` + `MAIL_SMTP_USERNAME` / `_PASSWORD` / 发件人地址）。
//! 那套 env 已随「邮箱改为页面内配置」整轮删除（凭据改由 `scheduled_task` 域的
//! `t_mail_account` 提供），判据因此改成 [`service::sender_is_available`]：
//! `t_mail_digest_config.enabled` 为 true **且** `sender_account_id` 指向一个
//! 未软删邮箱。
//!
//! 它**不再**是 `run()` 的前置 return，而是搬进了 [`reschedule_inner`] 的
//! `make_new` 闭包里（与 `enabled=false` / `MANUAL_ONLY` 同款）。两者的差别：
//! `run()` 早退会跳过 `ensure_scheduler()` + watcher 注册，而走 `make_new` 的
//! `None` 分支走的是 [`swap_job`] 的「**先摘旧 job**、再决定要不要挂新的」——
//! 后者才是正确的形态：管理员配好邮箱之后**不必重启进程**就能让定时任务生效。
//!
//! ⚠️ 判据只到「邮箱行在不在」为止，**不**解密授权码：解密失败要等到真发信时才
//! 暴露（`21701` + 流水里一行 `FAILED`）。理由与 `ConfigOut.smtp_configured`
//! 同款 —— 解密失败的原因对「要不要注册 job」这个决定没有帮助。
//!
//! ## 时区：cron 走进程本地时区，所以「不显式指定」是真实的线上事故
//!
//! `docker-compose.yml` 设了 `TZ=Asia/Shanghai`，但 `cargo run` 的开发机未必；
//! 生产镜像与开发机不一致时，「每天 8:30 发信」会在开发机验证通过、
//! 生产却变成凌晨 00:30 —— 而且**不报错**。
//!
//! ### `Job::new_async()` 的默认值是 **UTC**，不是本地时区
//!
//! ⚠️ 实读 tokio-cron-scheduler 0.15.1 源码逐字核实（**按函数名引用、不写源码行号**
//! ——行号随 patch 升级即漂，2026-10-13 review 第 1 轮 m11）：
//! ```text
//! pub fn new_async<S, T>(schedule: S, run: T) -> Result<Self, JobSchedulerError> {
//!     Self::new_async_tz(schedule, Utc, run)      // ← 默认 UTC，不是 localtime
//! }
//! pub fn new_async_tz<S, T, TZ>(schedule: S, timezone: TZ, run: T) -> ...
//!     where TZ: TimeZone
//! ```
//! 所以「不传时区」不是「按本地时间跑」而是「**按 UTC 跑**」⇒ 上海 8:30 的邮件
//! 会在 UTC 8:30（= 上海下午 16:30）发出。
//!
//! ### 结论：显式传 `Asia/Shanghai`
//!
//! - `Job::new_async_tz(cron, chrono_tz::Asia::Shanghai, run)` 在 0.15.1 中确实存在
//!   （定义在 crate 的 `job` 模块），形参 `TZ: chrono::TimeZone`，
//!   `chrono_tz::Asia::Shanghai` 直接满足。
//! - 因此 `Cargo.toml` 里 `chrono-tz = "0.10"` 是**必需的直接依赖**
//!   （它虽是 `tokio-cron-scheduler` 自带的传递依赖，但不显式声明就拿不到
//!   `chrono_tz::Asia::Shanghai` 这个类型）。
//! - 连带效果：**不再依赖 `TZ` 环境变量**，容器 / 开发机 / CI 三处行为一致。
//!
//! ### 已知偏差登记
//!
//! ⚠️ **整域契约见 [`docs/api/mail_digest.md`](../../docs/api/mail_digest.md)**
//! ——本文件原先在这里写了一段「`docs/api/` 目录下尚无 `mail_digest.md`，故按
//! CLAUDE.md「`docs/api/` 目录约定」第 3 条此处不写成文档链接」的说明（2026-10-13
//! review 第 1 轮 M2 删除），但同一分支的 `6211d0f` 已经提交了那份文档——**注释里
//! 「文档不存在」比文档不存在更糟**，它会让后人以为链接是笔误而去删掉已提交的文档。
//! 本节只留本文件的读者真正需要的指针：
//!
//! - `docs/api/mail_digest.md` §8.3「已知偏差登记」→「依赖库版本相关的实测结论」
//!   第 1、2 条即下面这两条（全文还含本域设计取舍与「实现与注释不符」若干条，本文件
//!   不复述，避免两份副本漂移）：
//!   1. `JobScheduler::new()` 在 0.15 是 **`async fn`**（0.14 及以前是同步 `new()`）
//!      ⇒ 本文件用 `tokio::sync::OnceCell::get_or_try_init` 承担异步初始化。
//!   2. `Job::new_async()` 的默认值是 **UTC 而不是本地时区** ⇒ 必须显式传
//!      `chrono_tz::Asia::Shanghai`（`new_async_tz`），不依赖 `TZ` 环境变量。
//! - 另：`SCHEDULED` 触发时的**准备阶段失败也会落流水**（`status='FAILED'`），
//!   见 `service::send_scheduled` 的 doc。

use std::future::Future;
use std::sync::{Arc, OnceLock};

use tokio_cron_scheduler::{Job, JobScheduler};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::modules::mail_digest::dto::Frequency;
use crate::modules::mail_digest::schedule::{self, NormalizedScheduleInput};
use crate::modules::mail_digest::service;
use crate::state::AppState;

/// 进程内调度器单例。
///
/// ## 为什么走 `OnceLock` 而**不**给 `AppState` 加字段
///
/// - `AppState` 已有 27 个字段，且**全仓有多处 `AppConfig { … }` 全量字面量**要跟着改
///   ——绝大多数在 `test-support/src/state.rs` 的几个 `test_state*` 构造函数里，另在
///   `src/modules/iam/service/tests` 与 `tests/idempotency_api.rs` 里。加字段要么每处补
///   一行，要么给 `AppState` 加 `Default` + builder —— 都是**与本需求无关的改动面**。
///   （2026-10-13 review 第 1 轮 m4：本条原先写的是「测试侧有 4 处 `AppState::new`
///   字面量（test-support 的 3 个 + dashboard_ws_api 与 _e2e_api 各 1 处）」——① 算式
///   自相矛盾（3+1+1=5≠4）；② 那些是 `AppState::new(…)` **调用**不是 struct 字面量，
///   加字段根本不用改它们；③ 真正要改的是 `AppConfig {` 字面量。改成不写死条数，
///   是因为条数会随别人的改动漂 —— 这正是 CLAUDE.md「测试数据两条硬约定」禁的那类
///   立刻过期的数字。）
/// - 调度器本身是**纯进程级单例**：它不依赖请求上下文，一个进程只该有一个。
///   `OnceLock` 正好表达这件事，且懒创建语义天然覆盖「先收到一次 `POST /config`
///   才启动任务」这种顺序。
/// - 代价（明确登记）：单例**不随 `AppState` 析构**。但它本来就不持有
///   `AppState`（job 闭包在触发时从克隆的 `Arc` 取），且进程退出时 tokio runtime
///   整体回收，不构成泄漏。
///
/// ## 多副本部署不防重（有意）
/// 与 `auto_complete` 同一现状：部署 N 个副本 ⇒ 同一时刻可能发 N 封信。
/// 要防重需要分布式锁 / 领导者选举，登记为独立重构。
///
/// ### ⚠️ 与上一条是**两层**不同的问题：进程内互斥只保证「单副本内不漏 job」
///
/// ⚠️ 2026-10-13（review 第 2 轮 M1）：本文件靠 [`swap_job`] 持有的
/// `tokio::sync::Mutex` 保证**同一时刻只有一次 remove→add 在跑**，解决的是
/// **单进程内**两个并发 `POST /config`（axum 的 handler 是并发执行的）互相插队，
/// 导致「旧 job 永久泄漏、此后每天多发一封」的问题。
///
/// - **本条（进程内互斥）管的是**：一个进程内 N 个并发请求 ⇒ 调度器里恒 ≤ 1 个 job。
/// - **上一条（多副本不防重）管的是**：N 个进程各发各的 ⇒ 需要分布式锁 / 领导者选举。
///
/// ⚠️ 两层都不可省，也**不要**互相替代：用「进程内互斥」冒充「多副本防重」，或反过来
/// 用「多副本不防重」当作「进程内可以不互斥」的理由，都会漏掉真实故障。
struct MailDigestScheduler {
    scheduler: JobScheduler,
    /// 当前已注册的 job id（`None` = 未注册：`enabled=false` 或 `MANUAL_ONLY`）。
    ///
    /// 用「槽 + 锁」而不是「每次 reschedule 都重建一个 scheduler」：重建会让旧
    /// scheduler 的 `start()` 循环永远挂着，且 job 会**叠加**（改 5 次配置 = 5 个
    /// job 同时在跑，一天发 5 封信）。
    ///
    /// ⚠️ **这个锁同时就是串行闸**（2026-10-13 review 第 2 轮 M1）：拿住 guard
    /// 即代表「本进程正在做 remove→add」，别的 [`swap_job`] / [`remove_job`] 进不来。
    /// 原先它是 `std::sync::Mutex<Option<Uuid>>` 且**只锁「取 id」这一个动作**，
    /// 中间的 DB 往返 + `add` 全裸奔 ⇒ 两个 Manager 同时保存时，A 取走旧 id、B 拿到
    /// `None` 直接返回、两份配置各自 `add` 出一个 job，而槽里只记后一个 ⇒ 先注册
    /// 那个**永远摘不掉**，直到进程重启。改用 `tokio::sync` 是必须的：临界区跨
    /// `.await`（`pool.acquire()` / `add`），`std::sync::Mutex` 的 guard 跨 `.await`
    /// 会直接 panic。
    job_id: tokio::sync::Mutex<Option<uuid::Uuid>>,
}

/// [`swap_job`] 真正用到的两个调度器操作（生产 = [`JobScheduler`]，单测 = 记录调用的
/// 假实现）。
///
/// ⚠️ 为什么要这一层（2026-10-13 review 第 2 轮 M1）：tokio-cron-scheduler 0.15
/// **没有**「列举当前所有 job」的公开 API（`MetaDataStorage` 只有 `get` /
/// `add_or_update` / `delete` 三件套），所以在真实 `JobScheduler` 上**数不出**
/// 「还剩几个 job」⇒ M1 的核心不变量「remove→add 之间不能插队」无法被直接观测。
/// 把边界收窄成两个可替换方法后，单测就能记录调用序列并断言「调度器里恰好 1 个 job」。
trait JobRegistry {
    fn remove<'a>(&'a self, id: &'a uuid::Uuid) -> impl Future<Output = Result<(), String>> + 'a;
    fn add(&self, job: Job) -> impl Future<Output = Result<uuid::Uuid, String>> + '_;
}

impl JobRegistry for JobScheduler {
    async fn remove(&self, id: &uuid::Uuid) -> Result<(), String> {
        JobScheduler::remove(self, id)
            .await
            .map_err(|e| e.to_string())
    }
    async fn add(&self, job: Job) -> Result<uuid::Uuid, String> {
        JobScheduler::add(self, job)
            .await
            .map_err(|e| e.to_string())
    }
}

/// 一次待注册的 job（连同写 `info!` 需要的上下文）。
///
/// 单独抽成结构体是为了让 [`swap_job`] 与「读库产出 job」那段逻辑解耦：临界区只管
/// 闸与挂载，产出 job 的部分由调用方用闭包表达，单测则直接喂一个假 job。
struct NewJob {
    job: Job,
    cron: String,
    frequency: &'static str,
}

/// 进程内单例（配合 [`scheduler_cell`] 承担异步初始化）。
static SCHEDULER: OnceLock<MailDigestScheduler> = OnceLock::new();

/// 后台任务入口（`src/main.rs` 里 `tokio::spawn`）。
///
/// ⚠️ **2026-10-16：这里不再有「未配置 SMTP 就早退」的前置闸**（env 那套已删除）。
/// 调度器**总是**建起来，是否注册 job 由 [`reschedule_inner`] 读库后的判据决定
/// （模块 doc 的「判据从 env 换成 DB」一节）。这样管理员配好邮箱 / 打开
/// `enabled` 之后**不必重启进程**就能让定时任务生效 —— 而早退形态做不到这一点
/// （它连 watcher 都不注册）。
pub async fn run(state: Arc<AppState>, token: CancellationToken) {
    info!("mail_digest: 交期汇总邮件定时任务已启动（是否注册 job 由配置决定）");

    let s = match ensure_scheduler().await {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "mail_digest: 调度器启动失败，本进程不再注册定时任务");
            return;
        }
    };

    // start() 是**永不返回**的循环，必须 spawn，否则下面的 reschedule 永远跑不到。
    let start_handle = s.scheduler.clone();
    tokio::spawn(async move {
        if let Err(e) = start_handle.start().await {
            warn!(error = %e, "mail_digest: 调度器主循环退出");
        }
    });

    // 首次装载配置（读库失败不致命：任务照常在跑，只是这一次没注册 job）
    reschedule_inner(&state, s).await;

    // watcher：收到取消令牌 → 关掉调度器，让 start() 循环退出、进程能干净地停。
    let shutdown_handle = s.scheduler.clone();
    tokio::spawn(async move {
        token.cancelled().await;
        let mut s = shutdown_handle;
        if let Err(e) = s.shutdown().await {
            warn!(error = %e, "mail_digest: 调度器关闭失败");
        }
        info!("mail_digest: 定时任务收到取消信号，退出");
    });
}

/// 懒创建 / 取出进程内调度器单例。
async fn ensure_scheduler() -> Result<&'static MailDigestScheduler, String> {
    // `OnceLock::get_or_init` 的闭包是**同步**的，而 `JobScheduler::new()` 在 0.15
    // 是 `async fn` ⇒ 不能直接 `get_or_init(|| JobScheduler::new())`。折中：用
    // `tokio::sync::OnceCell`（异步语义的单例）承担真正的初始化，再把结果塞进
    // 这个 `OnceLock`（同步、快速、可 `&'static` 借用）。
    //
    // 两个单例各自的职责：
    // - [`scheduler_cell`]：保证 `JobScheduler` 只被 `new()` 一次（async）；
    // - [`SCHEDULER`]：保证 `Mutex<Option<Uuid>>`（job 槽位）只被创建一次，
    //   并让 `remove_job` / 注册处能拿到 `&'static` 借用而不必层层传参。
    let cell = scheduler_cell();
    let scheduler = cell
        .get_or_try_init(JobScheduler::new)
        .await
        .map_err(|e| format!("JobScheduler::new 失败：{e}"))?;
    SCHEDULER.get_or_init(|| MailDigestScheduler {
        scheduler: scheduler.clone(),
        job_id: tokio::sync::Mutex::new(None),
    });
    Ok(SCHEDULER.get().expect("get_or_init 刚写入，必然可读"))
}

/// 异步初始化用的 `OnceCell`（与 [`SCHEDULER`] 配对，生命周期同进程）。
fn scheduler_cell() -> &'static tokio::sync::OnceCell<JobScheduler> {
    static CELL: tokio::sync::OnceCell<JobScheduler> = tokio::sync::OnceCell::const_new();
    &CELL
}

/// `POST /config` 成功后的热重载入口。
///
/// **幂等**：
/// - 调度器不存在 ⇒ 懒创建（覆盖「先改配置、后启动任务」的顺序）；
/// - `enabled = false` / `MANUAL_ONLY` / **无可用发信通道** ⇒ **移除**已注册的
///   job（并把 `job_id` 清空）；
/// - 其它 ⇒ 先移除旧 job 再注册新的（不泄漏）。
///
/// ⚠️ 调用方是 HTTP handler，必须**永不 panic、永不把错误抛给调用方**：
/// `JobScheduler::remove/add` 都是 async 且内部有锁，失败只记 warn —— 配置已经
/// 落库，不该因为「调度没换上」而让保存接口返 500。
///
/// ⚠️ **2026-10-16 删掉了原先的 env-disabled 分支**（`if !state.config.mail
/// .is_enabled() { remove_job(..); return }`）：env 那套已删除，而「配置改成不满足
/// 条件」这个场景现在由 [`reschedule_inner`] 的 `make_new → None` 覆盖，且走的是
/// **同一把闸** [`swap_job`]（原先那个分支若与并发的 `reschedule_inner` 各摘各的，
/// 会出现「删了一个 job、另一个 reschedule 又挂上一个」而槽里记的却是刚被删掉的
/// 那个 ⇒ 同样泄漏，review 第 2 轮 M1 的同源问题）。
pub async fn reschedule(state: &Arc<AppState>) {
    let Ok(s) = ensure_scheduler().await else {
        return;
    };
    reschedule_inner(state, s).await;
}

/// 真正读库 + 注册 / 替换 job（**整个函数体都在 [`swap_job`] 的闸内**）。
async fn reschedule_inner(state: &Arc<AppState>, s: &'static MailDigestScheduler) {
    swap_job(&s.scheduler, &s.job_id, async {
        // 读库 → 规范化 → cron → Job。这段**含 DB 往返**，闸在此期间**不释放**
        // —— 这正是 M1 修复的全部要点。
        let mut conn = match state.pool.acquire().await {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, "mail_digest: 热重载读配置失败（连接池）");
                return None;
            }
        };
        let cfg = match service::current_normalized(&mut conn).await {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, "mail_digest: 热重载读配置失败（解析）");
                return None;
            }
        };
        if !cfg.enabled {
            info!("mail_digest: 配置未启用（enabled=false），不注册定时 job");
            return None;
        }
        if cfg.frequency == Frequency::ManualOnly {
            info!("mail_digest: 频率为 MANUAL_ONLY（仅手动），不注册定时 job");
            return None;
        }
        // 2026-10-16 新增：第二道 DB 闸（取代原先的 env 闸）。
        // ⚠️ 只判「邮箱行在不在」（`sender_is_available`），不真解密 —— 解密失败
        // 要到发信那一刻才暴露，而那一次同样会落一行 FAILED 流水，可观测性不缺。
        //
        // ⚠️ **必须在 `drop(conn)` 之前**：那个 drop 会把 `PoolConnection` 移走，
        // 之后再借 `&mut conn` 是 E0382。
        if !service::sender_is_available(&mut conn, cfg.sender_account_id).await {
            info!("mail_digest: 未选定可用的发信邮箱（sender_account_id 为空或已失效），不注册定时 job");
            return None;
        }
        drop(conn);

        let cron = match schedule::to_cron(&NormalizedScheduleInput {
            frequency: cfg.frequency,
            send_hour: cfg.send_hour,
            send_minute: cfg.send_minute,
            weekday: cfg.weekday,
            day_of_month: cfg.day_of_month,
        }) {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, "mail_digest: cron 生成失败，不注册定时 job");
                return None;
            }
        };

        // 时区**显式传** Asia/Shanghai，不依赖进程本地时区 / `TZ` 环境变量（模块 doc）。
        let job_state = state.clone();
        let job = Job::new_async_tz(
            cron.clone(),
            chrono_tz::Asia::Shanghai,
            move |_uuid, _sched| {
                let st = job_state.clone();
                Box::pin(async move {
                    service::send_scheduled(&st).await;
                })
            },
        );
        match job {
            Ok(job) => Some(NewJob {
                job,
                cron,
                frequency: cfg.frequency.as_str(),
            }),
            Err(e) => {
                warn!(error = %e, cron = %cron, "mail_digest: cron 表达式非法，不注册定时 job");
                None
            }
        }
    })
    .await;
}

/// reschedule 的**临界区**：摘掉旧 job → 造新 job → 注册新 job → 写回槽。
///
/// ⚠️ **2026-10-13 review 第 2 轮 M1 的修复本体**。`job_id` 的 guard 从
/// 「摘旧」一直持有到「写回新」，中间隔着 `make_new` 里的 DB 往返 —— 整段串行。
///
/// 修复前的时序（两个 Manager 同时点保存即命中）：
/// ```text
/// A: remove_job → take() 取走 id_old          B: remove_job → 拿到 None，直接 return
/// A: 读库…（await）                           B: 读库…（await）
/// A: add → id_a；槽 = id_a                    B: add → id_b；槽 = id_b
/// ⇒ 调度器里同时挂着 id_a 与 id_b，槽里只记 id_b；此后任何一次 reschedule
///    只会摘掉 id_b，id_a 永远留在调度器里 ⇒ 每天在旧时刻多发一封，且直到进程重启都不消失。
/// ```
///
/// **死锁检查（2026-10-13 review 第 2 轮）**：本函数只持有 `job_id` 这一把锁，
/// `JobRegistry` 的两个方法内部不再取本模块的任何锁；`make_new` 闭包走的是
/// `service::current_normalized`，它**不反向依赖 `task::mail_digest`**（域内唯一
/// 的调用方 `handler::save_config` 是在 service 返回**之后**才调 [`reschedule`]）
/// ⇒ 锁序是一条直线，无环。
///
/// ⚠️ 闸用 `tokio::sync::Mutex` 而非 `std::sync::Mutex`：临界区跨 `.await`，
/// std 版 guard 跨 `.await` 会 panic。
async fn swap_job<R: JobRegistry>(
    reg: &R,
    job_id: &tokio::sync::Mutex<Option<uuid::Uuid>>,
    make_new: impl Future<Output = Option<NewJob>>,
) {
    let mut slot = job_id.lock().await;

    // 无论本次成败，**先摘掉旧 job**：否则「改了一次配置但没换上」会变成两个 job
    // 同时跑，一天发两封 —— 比不发严重得多。
    if let Some(old) = slot.take() {
        match reg.remove(&old).await {
            Ok(()) => info!("mail_digest: 旧定时 job 已摘除"),
            Err(e) => warn!(error = %e, "mail_digest: 摘除旧定时 job 失败"),
        }
    }

    let Some(new) = make_new.await else {
        return;
    };
    match reg.add(new.job).await {
        Ok(id) => {
            *slot = Some(id);
            info!(
                cron = %new.cron,
                frequency = new.frequency,
                timezone = "Asia/Shanghai",
                "mail_digest: 定时 job 已注册"
            );
        }
        Err(e) => warn!(error = %e, cron = %new.cron, "mail_digest: 注册定时 job 失败"),
    }
}

/// 摘掉已注册的 job（幂等：没注册过就是空操作）。
///
/// 与 [`swap_job`] 取**同一把闸** ⇒ 不会与并发的 `reschedule_inner` 交错。
///
/// ⚠️ **2026-10-16 起是测试专用**（`#[cfg(test)]`）：原先生产路径有两处调它 ——
/// `reschedule` 的 env-disabled 分支（env 已整轮删除，该分支随之删除）与
/// `run()` 的早退。现在「配置不满足条件」一律由 [`swap_job`] 的
/// 「**先摘旧 → 再决定挂不挂**」覆盖，而那条路径**内联**了同一段摘除逻辑，
/// 于是本函数在生产侧**零调用方**。
///
/// **不删它的理由**：回归用例 `remove_job_and_swap_share_the_same_gate`
/// （review 第 2 轮 M1 配套）守的是「槽与调度器恒一致」这个**并发不变式**，
/// 它需要一条「只摘不挂」的独立入口才能构造出 interleaving。留着一个
/// `#[cfg(test)]` 的辅助函数比删掉一条并发回归便宜。
#[cfg(test)]
async fn remove_job<R: JobRegistry>(reg: &R, job_id: &tokio::sync::Mutex<Option<uuid::Uuid>>) {
    let mut slot = job_id.lock().await;
    let Some(id) = slot.take() else {
        return;
    };
    match reg.remove(&id).await {
        Ok(()) => info!("mail_digest: 旧定时 job 已摘除"),
        Err(e) => warn!(error = %e, "mail_digest: 摘除旧定时 job 失败"),
    }
}

#[cfg(test)]
mod tests {
    //! M1（review 第 2 轮）的并发回归。
    //!
    //! 为什么用假注册表而不是真 `JobScheduler`：tokio-cron-scheduler 0.15 **没有**
    //! 「列举当前所有 job」的公开 API（`MetaDataStorage` 只有 `get` /
    //! `add_or_update` / `delete`），所以在真实调度器上**数不出**「还剩几个 job」。
    //! [`FakeRegistry`] 维护一个「在架 job 集合」，等价于那个计数，且能在 `remove`
    //! 里人为放大时序窗口，让插队**必然**发生（而不是靠运气撞上）。

    use std::collections::HashSet;
    use std::sync::{Arc as StdArc, Mutex as StdMutex};
    use std::time::Duration;

    use super::*;

    const FAKE_CRON: &str = "0 0 9 * * *";

    /// 假调度器：记录调用序列 + 维护「在架 job 集合」。
    struct FakeRegistry {
        /// 调用序列（`remove:<id>` / `add:<id>`），用来断言「先摘后挂」严格成对。
        events: StdArc<StdMutex<Vec<String>>>,
        /// 模拟「调度器里当前挂着的 job」—— 即本测试要数的那个数。
        live: StdArc<StdMutex<HashSet<uuid::Uuid>>>,
        next_id: StdArc<StdMutex<u128>>,
    }

    impl FakeRegistry {
        fn new() -> Self {
            Self {
                events: StdArc::new(StdMutex::new(Vec::new())),
                live: StdArc::new(StdMutex::new(HashSet::new())),
                next_id: StdArc::new(StdMutex::new(0)),
            }
        }

        fn events(&self) -> Vec<String> {
            self.events.lock().unwrap().clone()
        }

        /// 只取操作名（`remove` / `add`），断言交替顺序时用。
        fn ops(&self) -> Vec<String> {
            self.events()
                .iter()
                .map(|e| e.split(':').next().unwrap().to_string())
                .collect()
        }

        fn live_count(&self) -> usize {
            self.live.lock().unwrap().len()
        }

        fn live_set(&self) -> HashSet<uuid::Uuid> {
            self.live.lock().unwrap().clone()
        }
    }

    impl JobRegistry for FakeRegistry {
        fn remove<'a>(
            &'a self,
            id: &'a uuid::Uuid,
        ) -> impl Future<Output = Result<(), String>> + 'a {
            let events = self.events.clone();
            let live = self.live.clone();
            let id = *id;
            async move {
                events.lock().unwrap().push(format!("remove:{id}"));
                // ⚠️ 人为放大时序窗口：没有这道 sleep，两次并发调用很可能**碰巧**
                // 串行，测试就成了「测运气」；有它则无闸版本**必然**插队。
                tokio::time::sleep(Duration::from_millis(5)).await;
                live.lock().unwrap().remove(&id);
                Ok(())
            }
        }

        fn add(&self, _job: Job) -> impl Future<Output = Result<uuid::Uuid, String>> + '_ {
            let events = self.events.clone();
            let live = self.live.clone();
            async move {
                let id = {
                    let mut g = self.next_id.lock().unwrap();
                    *g += 1;
                    uuid::Uuid::from_u128(*g)
                };
                events.lock().unwrap().push(format!("add:{id}"));
                live.lock().unwrap().insert(id);
                Ok(id)
            }
        }
    }

    /// 造一个必然成功的假 `NewJob`（cron 内容不参与断言，只占位）。
    async fn fake_new_job() -> Option<NewJob> {
        let job = Job::new_async_tz(
            String::from(FAKE_CRON),
            chrono_tz::Asia::Shanghai,
            |_uuid, _sched| Box::pin(async {}),
        )
        .expect("构造 job 不应失败");
        Some(NewJob {
            job,
            cron: String::from(FAKE_CRON),
            frequency: "DAILY",
        })
    }

    /// ★ M1 主回归：两个 Manager 同时保存 ⇒ 调度器里**恰好 1 个** job，槽与调度器一致。
    ///
    /// 修复前这个用例会红：两次 reschedule 各自 remove→add，第二次 remove 拿到
    /// `None` 直接返回，于是两个 job 都在架上而槽里只记后一个 ⇒ 先注册那个永远摘不掉。
    #[tokio::test]
    async fn concurrent_swap_leaves_exactly_one_job() {
        let reg = FakeRegistry::new();
        let job_id = tokio::sync::Mutex::new(None);

        // 先挂一个（模拟「进程启动时已注册过一次」）
        swap_job(&reg, &job_id, fake_new_job()).await;
        assert_eq!(reg.live_count(), 1, "预热后就应有 1 个 job");

        // 两个并发保存
        tokio::join!(
            swap_job(&reg, &job_id, fake_new_job()),
            swap_job(&reg, &job_id, fake_new_job())
        );

        assert_eq!(
            reg.live_count(),
            1,
            "两个并发 reschedule 后调度器里必须恰好剩 1 个 job（原先会剩 2 个，\
             先注册那个永远摘不掉）：调用序列 = {:?}",
            reg.events()
        );

        // 槽里记的必须就是「在架的那个」——否则下一次 reschedule 会摘错 job。
        let Some(slot) = *job_id.lock().await else {
            panic!("槽不该为空");
        };
        assert_eq!(
            reg.live_set(),
            HashSet::from([slot]),
            "槽里记的 id 必须与在架 job 一致，否则下次 reschedule 摘的是另一个"
        );

        // 调用序列严格「先摘后挂」成对（无闸版本会变成 remove,remove,add,add）
        assert_eq!(
            reg.ops(),
            vec!["add", "remove", "add", "remove", "add"],
            "remove 与 add 必须严格交替成对"
        );
    }

    /// `remove_job` 与 `swap_job` 取**同一把闸**（覆盖 reviewer 点名的第二条路径：
    /// `reschedule` 的 env-disabled 分支会直接摘 job）。
    ///
    /// 断言的是「槽与调度器永远一致」这个不变量，而不是某一种先后顺序的结果。
    #[tokio::test]
    async fn remove_job_and_swap_share_the_same_gate() {
        let reg = FakeRegistry::new();
        let job_id = tokio::sync::Mutex::new(None);
        swap_job(&reg, &job_id, fake_new_job()).await;
        assert_eq!(reg.live_count(), 1);

        tokio::join!(
            remove_job(&reg, &job_id),
            swap_job(&reg, &job_id, fake_new_job())
        );

        // 终态只可能是「槽有值 ⇔ 调度器里 1 个」；绝不允许「槽空而调度器还挂着 job」。
        let slot = *job_id.lock().await;
        assert_eq!(
            slot.is_some(),
            reg.live_count() == 1,
            "槽与调度器必须一致：slot={slot:?}, live_count={}, 序列={:?}",
            reg.live_count(),
            reg.events()
        );
        if let Some(slot) = slot {
            assert_eq!(reg.live_set(), HashSet::from([slot]), "槽记的必须在架");
        }
    }

    /// `make_new` 返回 `None`（未启用 / MANUAL_ONLY / 读库失败）⇒ 只摘不挂。
    #[tokio::test]
    async fn swap_with_no_new_job_only_removes() {
        let reg = FakeRegistry::new();
        let job_id = tokio::sync::Mutex::new(None);
        swap_job(&reg, &job_id, fake_new_job()).await;
        assert_eq!(reg.live_count(), 1);

        swap_job(&reg, &job_id, async { None }).await;
        assert_eq!(reg.live_count(), 0, "不该挂新 job");
        assert!(job_id.lock().await.is_none(), "槽应被清空");
        assert_eq!(reg.ops(), vec!["add", "remove"]);
    }
}
