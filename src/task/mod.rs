//! 后台定时任务

pub mod auto_complete;
// 2026-10-13 新增：交期汇总邮件（域 `mail_digest`）。用 `tokio-cron-scheduler`
// 而非裸 interval —— 需求要「按配置的频率 / 时刻发信」（每天 / 每周几 /
// 每月几号），间隔循环表达不了「每周一早上 8:30」。
pub mod mail_digest;
