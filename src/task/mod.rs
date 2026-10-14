//! 后台定时任务

pub mod auto_complete;
// 2026-10-13 新增：交期汇总邮件（域 `mail_digest`）。用 `tokio-cron-scheduler`
// 而非裸 interval —— 需求要「按配置的频率 / 时刻发信」（每天 / 每周几 /
// 每月几号），间隔循环表达不了「每周一早上 8:30」。
pub mod mail_digest;
// 2026-10-18 新增：企业微信每日总结推送（域 `wecom_daily`）。与 `mail_digest` 同款
// 形态（tokio-cron-scheduler + 单 job 槽 + swap_job 临界区），但**刻意不抽公共
// 注册表**：两个推送域各一个 job 是既有形态，等出现第三个推送类型时再合。
pub mod wecom_daily;
