//! mail_digest 域：频率 → cron 表达式（纯函数，无 IO、无状态、易单测）
//!
//! ## cron 是 6 段（**带秒**），不是 5 段
//!
//! tokio-cron-scheduler 0.15 内部用 `croner`，解析器固定
//! `Seconds::Required` + `dom_and_dow(true)`（见其 `job/mod.rs::new_async_tz`）。
//! 也就是说本函数返回的串**必须**是 `秒 分 时 日 月 周` 六段。写成常见的五段
//! `分 时 日 月 周` 会在 `Job::new_async_tz` 里直接 `ParseSchedule` 失败 ——
//! 好在那是同步返回的 `Err`，不会等到「到点了没触发」才发现。
//!
//! ## 周序号：cron 与 ISO 差一天
//!
//! | 语义 | 值域 | 出现在哪个字段 |
//! |---|---|---|
//! | cron（croneg / Vixie） | `0`=周日 .. `6`=周六 | 第 6 段 |
//! | 本域 `weekday`（ISO） | `1`=周一 .. `7`=周日 | DB `t_mail_digest_config.weekday` |
//!
//! ⇒ **WEEKLY 必须减 1**。漏减是最容易犯且最难发现的错：cron `1` 表示**周二**，
//! 而管理员以为自己选了「周一」，邮件会安静地每周二早上到，一周都不差，
//! 看日志也看不出问题。单测 [`tests::weekly_cron_subtracts_one`] 钉死这一点。

use crate::modules::mail_digest::dto::Frequency;
use crate::shared::error::{AppError, code};

/// 把频率 + 配套时间字段翻译成 6 段 cron 表达式。
///
/// 返回 `Err(21704)` 的两种情形（都是**配置组合**问题，不是内部错误）：
/// - `MANUAL_ONLY`：**永不**产生 cron（定时任务不注册 job）。
/// - `DAILY` / `WEEKLY` / `MONTHLY` 缺 `send_hour` / `send_minute`（缺省期兜底
///   见 [`crate::modules::mail_digest::service`]，那里已把缺省填成 8:00）。
///
/// 本函数**不**校验 `weekday` / `day_of_month` 的范围（那是 `service` 的职责，
/// 校验错误要带业务语义的错误码与提示文案）；它只对 `WEEKLY` / `MONTHLY`
/// 缺字段报错。
pub fn to_cron(cfg: &NormalizedScheduleInput) -> Result<String, AppError> {
    let (Some(hour), Some(minute)) = (cfg.send_hour, cfg.send_minute) else {
        return Err(AppError::biz(
            code::BIZ_MAIL_CONFIG_INVALID,
            format!(
                "频率 {} 需要填写发送时刻（小时 / 分钟），当前缺省",
                cfg.frequency.as_str()
            ),
        ));
    };

    Ok(match cfg.frequency {
        // DAILY / WEEKLY / MONTHLY 分支都需要上面的 hour+minute，故这里只处理
        // MANUAL_ONLY 的早退与三种 cron 的拼装。
        Frequency::ManualOnly => {
            return Err(AppError::biz(
                code::BIZ_MAIL_CONFIG_INVALID,
                "MANUAL_ONLY（仅手动）不产生 cron 表达式：定时任务不应为它注册 job",
            ));
        }
        Frequency::Daily => format!("0 {minute} {hour} * * *"),
        Frequency::Weekly => {
            let Some(weekday) = cfg.weekday else {
                return Err(AppError::biz(
                    code::BIZ_MAIL_CONFIG_INVALID,
                    "频率 WEEKLY（每周）需要填写星期几（weekday，ISO 1=周一 .. 7=周日）",
                ));
            };
            // cron 第 6 段 0=周日，本域 weekday 1=周一 ⇒ 减 1（理由见模块 doc）。
            format!("0 {minute} {hour} * * {}", weekday - 1)
        }
        Frequency::Monthly => {
            let Some(dom) = cfg.day_of_month else {
                return Err(AppError::biz(
                    code::BIZ_MAIL_CONFIG_INVALID,
                    "频率 MONTHLY（每月）需要填写几号（day_of_month，1..28）",
                ));
            };
            format!("0 {minute} {hour} {dom} * *")
        }
    })
}

/// [`to_cron`] 的入参（刻意只带它真正读的 5 个字段，避免 service 构造一个大结构）。
#[derive(Debug, Clone, Copy)]
pub struct NormalizedScheduleInput {
    pub frequency: Frequency,
    pub send_hour: Option<i16>,
    pub send_minute: Option<i16>,
    pub weekday: Option<i16>,
    pub day_of_month: Option<i16>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(
        frequency: Frequency,
        send_hour: Option<i16>,
        send_minute: Option<i16>,
        weekday: Option<i16>,
        day_of_month: Option<i16>,
    ) -> NormalizedScheduleInput {
        NormalizedScheduleInput {
            frequency,
            send_hour,
            send_minute,
            weekday,
            day_of_month,
        }
    }

    /// DAILY：每天 08:30 → `0 30 8 * * *`（首段是秒、固定 0）。
    #[test]
    fn daily_cron_has_six_fields_and_fixed_second_zero() {
        let cron = to_cron(&input(Frequency::Daily, Some(8), Some(30), None, None)).unwrap();
        assert_eq!(cron, "0 30 8 * * *");
        assert_eq!(cron.split_whitespace().count(), 6, "croner 要求 6 段（含秒）");
    }

    /// WEEKLY 的**减 1** 边界：ISO 1（周一）⇒ cron 0（周日）；ISO 7（周日）⇒ cron 6（周六）。
    ///
    /// 这条是本文件存在的核心理由：漏减 1 不会报错、只会把邮件安静地挪到隔一天。
    #[test]
    fn weekly_cron_subtracts_one() {
        // ISO 1 = 周一 ⇒ cron 0 = 周日
        assert_eq!(
            to_cron(&input(Frequency::Weekly, Some(9), Some(0), Some(1), None)).unwrap(),
            "0 0 9 * * 0"
        );
        // ISO 7 = 周日 ⇒ cron 6 = 周六
        assert_eq!(
            to_cron(&input(Frequency::Weekly, Some(9), Some(0), Some(7), None)).unwrap(),
            "0 0 9 * * 6"
        );
        // 中间值：ISO 3（周三）⇒ cron 2
        assert_eq!(
            to_cron(&input(Frequency::Weekly, Some(9), Some(0), Some(3), None)).unwrap(),
            "0 0 9 * * 2"
        );
    }

    /// MONTHLY：`day_of_month` 直接进第 4 段（日），不做任何偏移。
    #[test]
    fn monthly_cron_puts_day_of_month_in_day_field() {
        assert_eq!(
            to_cron(&input(Frequency::Monthly, Some(7), Some(5), None, Some(15))).unwrap(),
            "0 5 7 15 * *"
        );
        // 边界 1 与 28 都要能拼出来（1..28 的下界/上界由 service 校验）
        assert_eq!(
            to_cron(&input(Frequency::Monthly, Some(7), Some(5), None, Some(1))).unwrap(),
            "0 5 7 1 * *"
        );
        assert_eq!(
            to_cron(&input(Frequency::Monthly, Some(7), Some(5), None, Some(28))).unwrap(),
            "0 5 7 28 * *"
        );
    }

    /// MANUAL_ONLY 永不产生 cron（定时任务据此「不注册 job」）。
    #[test]
    fn manual_only_never_produces_cron() {
        let err = to_cron(&input(Frequency::ManualOnly, Some(8), Some(0), None, None)).unwrap_err();
        assert_eq!(err.code(), code::BIZ_MAIL_CONFIG_INVALID);
        // 即便时刻字段齐全也照样拒 —— 语义是「仅手动」，不是「缺字段才拒」
        let err2 = to_cron(&input(Frequency::ManualOnly, None, None, None, None)).unwrap_err();
        assert_eq!(err2.code(), code::BIZ_MAIL_CONFIG_INVALID);
    }

    /// WEEKLY 缺 weekday / MONTHLY 缺 day_of_month → 21704（不是 500）。
    #[test]
    fn missing_frequency_specific_field_is_21704() {
        let e1 = to_cron(&input(Frequency::Weekly, Some(8), Some(0), None, None)).unwrap_err();
        assert_eq!(e1.code(), code::BIZ_MAIL_CONFIG_INVALID);
        let e2 = to_cron(&input(Frequency::Monthly, Some(8), Some(0), None, None)).unwrap_err();
        assert_eq!(e2.code(), code::BIZ_MAIL_CONFIG_INVALID);
        // 缺时刻（三个非 MANUAL_ONLY 频率都受影响）
        for f in [Frequency::Daily, Frequency::Weekly, Frequency::Monthly] {
            let e = to_cron(&input(f, Some(8), None, Some(1), Some(1))).unwrap_err();
            assert_eq!(e.code(), code::BIZ_MAIL_CONFIG_INVALID, "频率 {f:?} 应报错");
        }
    }

    /// 四个枚举都被覆盖（防止将来加了新枚举却忘了给它 cron 分支）。
    #[test]
    fn every_frequency_variant_is_exercised() {
        assert_eq!(Frequency::ALL.len(), 4);
        // MANUAL_ONLY 走 Err 分支，其余三个走 Ok 分支 —— 逐个点名，漏一个就在这里红
        assert!(to_cron(&input(Frequency::Daily, Some(0), Some(0), None, None)).is_ok());
        assert!(to_cron(&input(Frequency::Weekly, Some(0), Some(0), Some(7), None)).is_ok());
        assert!(to_cron(&input(Frequency::Monthly, Some(0), Some(0), None, Some(28))).is_ok());
        assert!(to_cron(&input(Frequency::ManualOnly, None, None, None, None)).is_err());
    }

    /// 边界时刻（00:00 / 23:59）必须能拼出合法 cron。
    #[test]
    fn boundary_hours_and_minutes_are_formattable() {
        assert_eq!(
            to_cron(&input(Frequency::Daily, Some(0), Some(0), None, None)).unwrap(),
            "0 0 0 * * *"
        );
        assert_eq!(
            to_cron(&input(Frequency::Daily, Some(23), Some(59), None, None)).unwrap(),
            "0 59 23 * * *"
        );
    }
}
