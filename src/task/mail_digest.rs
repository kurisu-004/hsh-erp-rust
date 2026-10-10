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
//!   ├─ MailConfig::is_enabled() == false → info! + 直接 return（不注册任何 job）
//!   ├─ JobScheduler::new()（0.15 已改成 async）+ spawn(scheduler.start())
//!   ├─ reschedule_from_db()：读配置 → schedule::to_cron() → 注册 / 替换 job
//!   └─ spawn(watcher)：await token.cancelled() → scheduler.shutdown()
//! ```
//!
//! `POST /api/v2/mail-digest/config` 成功后由 handler 调 [`reschedule`]，
//! **改完立刻生效、不必重启进程**。
//!
//! ## 时区：cron 走进程本地时区，所以「不显式指定」是真实的线上事故
//!
//! `docker-compose.yml` 设了 `TZ=Asia/Shanghai`，但 `cargo run` 的开发机未必；
//! 生产镜像与开发机不一致时，「每天 8:30 发信」会在开发机验证通过、
//! 生产却变成凌晨 00:30 —— 而且**不报错**。
//!
//! ### `Job::new_async()` 的默认值是 **UTC**，不是本地时区
//!
//! ⚠️ 实读 tokio-cron-scheduler 0.15.1 源码（`src/job/mod.rs`）逐字核实：
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
//! - `Job::new_async_tz(cron, chrono_tz::Asia::Shanghai, run)` 在 0.15.1 中
//!   确实存在（`src/job/mod.rs:208`），形参 `TZ: chrono::TimeZone`，
//!   `chrono_tz::Asia::Shanghai` 直接满足。
//! - 因此 `Cargo.toml` 里 `chrono-tz = "0.10"` 是**必需的直接依赖**
//!   （它虽是 `tokio-cron-scheduler` 自带的传递依赖，但不显式声明就拿不到
//!   `chrono_tz::Asia::Shanghai` 这个类型）。
//! - 连带效果：**不再依赖 `TZ` 环境变量**，容器 / 开发机 / CI 三处行为一致。
//!
//! ### 已知偏差登记
//!
//! ⚠️ **本节目前只存在于代码注释里**：`docs/api/` 目录下尚无 `mail_digest.md`
//! （本域 2026-10-13 新建，整域契约文档由紧随其后的文档任务补齐）。按
//! CLAUDE.md「`docs/api/` 目录约定」第 3 条，此处**不写成文档链接**（指向不存在
//! 的文件会让人无法判断是漏提交还是信息本就在注释里），只在下面点名该文档的**待建
//! 路径**。文档补齐后，`docs/api/mail_digest.md` §8「已知偏差登记」应照抄以下两条：
//!
//! 1. `JobScheduler::new()` 在 0.15 是 **`async fn`**（0.14 及以前是同步 `new()`）
//!    ⇒ 本文件用 `tokio::sync::OnceCell::get_or_try_init` 承担异步初始化。
//! 2. `Scheduler::new(CoordinationMode)` 与 `job.run_at(ScheduleType::Cron(..))`
//!    在 0.15 **不存在**（那是更早版本的签名）—— 0.15 用
//!    `new_async` / `new_async_tz` / `new_once` 三个构造器。照旧签名写会编译不过。

use std::sync::{Arc, Mutex, OnceLock};

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
/// - `AppState` 已有 27 个字段，且**测试侧有 4 处 `AppState::new` 字面量**
///   （`test-support/src/state.rs` 的 3 个 `test_state*`，加 `tests/dashboard_ws_api.rs`
///   与 `tests/_e2e_api.rs` 各 1 处）。加字段要么每处补一行，要么给 `AppState`
///   加 `Default` + builder —— 都是**与本需求无关的改动面**。
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
struct MailDigestScheduler {
    scheduler: JobScheduler,
    /// 当前已注册的 job id（`None` = 未注册：`enabled=false` 或 `MANUAL_ONLY`）。
    ///
    /// 用 `Mutex<Option<Uuid>>` 而不是「每次 reschedule 都重建一个 scheduler」：
    /// 重建会让旧 scheduler 的 `start()` 循环永远挂着，且 job 会**叠加**
    /// （改 5 次配置 = 5 个 job 同时在跑，一天发 5 封信）。
    job_id: Mutex<Option<uuid::Uuid>>,
}

/// 进程内单例（配合 [`scheduler_cell`] 承担异步初始化）。
static SCHEDULER: OnceLock<MailDigestScheduler> = OnceLock::new();

/// 后台任务入口（`src/main.rs` 里 `tokio::spawn`）。
pub async fn run(state: Arc<AppState>, token: CancellationToken) {
    // 第一道闸在 env（总闸 + 三项凭据），第二道在 DB（`t_mail_digest_config.enabled`）。
    if !state.config.mail.is_enabled() {
        info!("mail_digest: SMTP 未配置（MAIL_DIGEST_ENABLED / MAIL_SMTP_USERNAME / MAIL_SMTP_PASSWORD / 发件人地址），定时任务跳过");
        return;
    }
    info!(
        host = %state.config.mail.host,
        port = state.config.mail.port,
        tls_mode = %state.config.mail.tls_mode,
        "mail_digest: 交期汇总邮件定时任务已启动"
    );

    let scheduler = match ensure_scheduler().await {
        Ok(s) => s.scheduler.clone(),
        Err(e) => {
            warn!(error = %e, "mail_digest: 调度器启动失败，本进程不再注册定时任务");
            return;
        }
    };

    // start() 是**永不返回**的循环，必须 spawn，否则下面的 reschedule 永远跑不到。
    let start_handle = scheduler.clone();
    tokio::spawn(async move {
        if let Err(e) = start_handle.start().await {
            warn!(error = %e, "mail_digest: 调度器主循环退出");
        }
    });

    // 首次装载配置（读库失败不致命：任务照常在跑，只是这一次没注册 job）
    reschedule_inner(&state, &scheduler).await;

    // watcher：收到取消令牌 → 关掉调度器，让 start() 循环退出、进程能干净地停。
    let shutdown_handle = scheduler.clone();
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
        job_id: Mutex::new(None),
    });
    Ok(SCHEDULER
        .get()
        .expect("get_or_init 刚写入，必然可读"))
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
/// - `MANUAL_ONLY` 或 `enabled = false` ⇒ **移除**已注册的 job（并把 `job_id` 清空）；
/// - 其它 ⇒ 先移除旧 job 再注册新的（不泄漏）。
///
/// ⚠️ 调用方是 HTTP handler，必须**永不 panic、永不把错误抛给调用方**：
/// `JobScheduler::remove/add` 都是 async 且内部有锁，失败只记 warn —— 配置已经
/// 落库，不该因为「调度没换上」而让保存接口返 500。
pub async fn reschedule(state: &Arc<AppState>) {
    if !state.config.mail.is_enabled() {
        // env 侧被关掉 ⇒ 把已注册的 job 也摘掉（可能有旧配置留下的 job）
        if let Ok(s) = ensure_scheduler().await {
            remove_job(&s.scheduler).await;
        }
        return;
    }
    let Ok(s) = ensure_scheduler().await else {
        return;
    };
    reschedule_inner(state, &s.scheduler).await;
}

/// 真正读库 + 注册 / 替换 job。
async fn reschedule_inner(state: &Arc<AppState>, scheduler: &JobScheduler) {
    // 无论本次成败，**先摘掉旧 job**：否则「改了一次配置但没换上」会变成两个 job
    // 同时跑，一天发两封 —— 比不发严重得多。
    remove_job(scheduler).await;

    let mut conn = match state.pool.acquire().await {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "mail_digest: 热重载读配置失败（连接池）");
            return;
        }
    };
    let cfg = match service::current_normalized(&mut conn).await {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "mail_digest: 热重载读配置失败（解析）");
            return;
        }
    };
    drop(conn);

    if !cfg.enabled {
        info!("mail_digest: 配置未启用（enabled=false），不注册定时 job");
        return;
    }
    if cfg.frequency == Frequency::ManualOnly {
        info!("mail_digest: 频率为 MANUAL_ONLY（仅手动），不注册定时 job");
        return;
    }

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
            return;
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
    let job = match job {
        Ok(j) => j,
        Err(e) => {
            warn!(error = %e, cron = %cron, "mail_digest: cron 表达式非法，不注册定时 job");
            return;
        }
    };

    match scheduler.add(job).await {
        Ok(id) => {
            if let Some(s) = SCHEDULER.get()
                && let Ok(mut slot) = s.job_id.lock()
            {
                *slot = Some(id);
            }
            info!(
                cron = %cron,
                frequency = cfg.frequency.as_str(),
                timezone = "Asia/Shanghai",
                "mail_digest: 定时 job 已注册"
            );
        }
        Err(e) => warn!(error = %e, cron = %cron, "mail_digest: 注册定时 job 失败"),
    }
}

/// 摘掉已注册的 job（幂等：没注册过就是空操作）。
async fn remove_job(scheduler: &JobScheduler) {
    let Some(s) = SCHEDULER.get() else {
        return;
    };
    let Some(id) = s.job_id.lock().ok().and_then(|mut slot| slot.take()) else {
        return;
    };
    match scheduler.remove(&id).await {
        Ok(()) => info!("mail_digest: 旧定时 job 已摘除"),
        Err(e) => warn!(error = %e, "mail_digest: 摘除旧定时 job 失败"),
    }
}
