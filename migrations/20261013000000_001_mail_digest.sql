-- ============================================================================
-- 20261013000000_001_mail_digest —— 交期到期汇总邮件（配置单例 + 发送流水）
-- ============================================================================
-- 需求：每天（或每周 / 每月 / 仅手动）把「已逾期 + 未来 N 天系统交期到期」的工单
--       汇总成一封 HTML 表格邮件发给指定收件人。
--
-- 归属域：`mail_digest`（`src/modules/mail_digest/`），URL 挂 `/api/v2/mail-digest`。
--
-- 与本仓既有约定的关系（全部照 CLAUDE.md「DB 约定」）：
--   * 无物理外键 —— `t_mail_digest_config.id` 是雪花主键，`created_by` / `updated_by`
--     指向 `t_user.id` 但**不建** FK（存在性由 service 校验，同 `t_part.customer_id`）。
--   * 无 DB ENUM —— `frequency` / `trigger_source` / `status` 一律 `varchar`，
--     合法值由 Rust 侧 `dto::Frequency` 等枚举校验。
--   * 乐观锁 —— 配置表带 `version` 列，`POST /mail-digest/config` 走
--     `UPDATE … WHERE id=$1 AND version=$2`，0 行 → 40901 VERSION_CONFLICT。
--   * 软删 —— 配置表带 `deleted_at`；发送流水表**不软删**（审计流水，见下方说明）。
--   * 雪花主键 —— `id` 由 App 侧 `SnowflakeIdGenerator` 生成，不在 DB 里取号。
-- ============================================================================

-- ---------------------------------------------------------------------------
-- 配置单例表
-- ---------------------------------------------------------------------------
-- ⚠️ **单例**：本表按业务只允许存在一行「未软删」记录，由唯一索引兜底
--   （`uk_t_mail_digest_config_alive ON t_mail_digest_config ((true)) WHERE deleted_at IS NULL`）。
--   表达式索引 `((true))` 的效果等价于「对所有未软删行，只允许一行」，
--   且软删后索引项消失、老行可留存审计。并发首次初始化（两个请求同时 INSERT，
--   都看到 version=0）由该索引兜底：后到者撞 23505 → service 转 OCC 冲突。
--
-- 调度三字段（send_hour / send_minute / weekday / day_of_month）的生效规则：
--   DAILY      → 用 send_hour + send_minute
--   WEEKLY     → 用 send_hour + send_minute + weekday（ISO，1=周一 … 7=周日）
--   MONTHLY    → 用 send_hour + send_minute + day_of_month（1..28，避开短月）
--   MANUAL_ONLY → 四个字段**全部不参与**，写侧一律清 NULL（不留脏值误导读侧）
CREATE TABLE public.t_mail_digest_config (
    id               bigint NOT NULL,
    -- 乐观锁版本号；每次 UPDATE 自增；0 行命中 → 40901 VERSION_CONFLICT
    version          integer NOT NULL DEFAULT 0,
    -- 定时任务总闸（第二道闸；第一道是 env 的 MAIL_DIGEST_ENABLED，见 MailConfig::is_enabled）。
    -- false 时 task::mail_digest 不注册 cron job，但 POST /send 手动触发仍可用。
    enabled          boolean NOT NULL DEFAULT false,
    -- 频率枚举：DAILY / WEEKLY / MONTHLY / MANUAL_ONLY（无 DB ENUM，Rust 侧校验）
    frequency        character varying(20) NOT NULL DEFAULT 'DAILY',
    -- DAILY/WEEKLY/MONTHLY：0..23；MANUAL_ONLY 恒 NULL
    send_hour        smallint,
    -- DAILY/WEEKLY/MONTHLY：0..59；MANUAL_ONLY 恒 NULL
    send_minute      smallint,
    -- WEEKLY：1..7（ISO，1=周一）；其余频率恒 NULL
    weekday          smallint,
    -- MONTHLY：1..28（**刻意上限 28**，避开 2 月 29/30 日与 30/31 日短月）
    day_of_month     smallint,
    -- 收件人（To）。**必填**：即便 MANUAL_ONLY 也必须至少 1 个，否则手动发送无从下手。
    recipients       text[] NOT NULL DEFAULT '{}',
    -- 抄送（Cc）。可空。
    cc_recipients    text[] NOT NULL DEFAULT '{}',
    -- 内容可选 ①：是否包含「已逾期」节
    include_overdue  boolean NOT NULL DEFAULT true,
    -- 内容可选 ①：是否包含「未来 N 天」节
    include_upcoming boolean NOT NULL DEFAULT true,
    -- 未来节窗口天数，clamp 1..60（与 dashboard 的 DASHBOARD_MIN/MAX_DAYS 同档）
    upcoming_days    integer NOT NULL DEFAULT 14,
    -- 内容可选 ②：表格列白名单（决定邮件正文渲染哪些列）。
    -- 取值域 = mail_digest::dto::VisibleColumn 枚举，Rust 侧逐项校验，未知项返 21704。
    visible_columns  character varying(30)[] NOT NULL DEFAULT
        '{serial_no,drawing_no,name,quantity,customer_name,status,system_delivery_date,remaining_days}',
    -- 邮件主题模板；NULL = 用渲染层的缺省主题「交期汇总 YYYY-MM-DD（未来 N 天）」
    subject_template character varying(200),

    created_at       timestamp without time zone DEFAULT now() NOT NULL,
    created_by       bigint,
    updated_at       timestamp without time zone DEFAULT now() NOT NULL,
    updated_by       bigint,
    deleted_at       timestamp without time zone
);

COMMENT ON TABLE  public.t_mail_digest_config IS '交期汇总邮件配置单例（mail_digest 域）；按业务只允许一行未软删记录';
COMMENT ON COLUMN public.t_mail_digest_config.id IS '雪花主键，App 侧生成';
COMMENT ON COLUMN public.t_mail_digest_config.version IS '乐观锁版本号；POST /config 全量覆写时带 WHERE version=$2，0 行 → 40901';
COMMENT ON COLUMN public.t_mail_digest_config.enabled IS '定时任务闸；false 时不注册 cron job（但 POST /send 手动触发仍可用）';
COMMENT ON COLUMN public.t_mail_digest_config.frequency IS '频率：DAILY/WEEKLY/MONTHLY/MANUAL_ONLY；无 DB ENUM，Rust 枚举校验';
COMMENT ON COLUMN public.t_mail_digest_config.send_hour IS '发送小时 0..23；MANUAL_ONLY 恒 NULL';
COMMENT ON COLUMN public.t_mail_digest_config.send_minute IS '发送分钟 0..59；MANUAL_ONLY 恒 NULL';
COMMENT ON COLUMN public.t_mail_digest_config.weekday IS 'WEEKLY 专用：1..7（ISO，1=周一）；其余频率恒 NULL';
COMMENT ON COLUMN public.t_mail_digest_config.day_of_month IS 'MONTHLY 专用：1..28（避开短月）；其余频率恒 NULL';
COMMENT ON COLUMN public.t_mail_digest_config.recipients IS '收件人 To（text[]）；非空，写侧校验邮箱格式并拒绝 CR/LF';
COMMENT ON COLUMN public.t_mail_digest_config.cc_recipients IS '抄送 Cc（text[]）；可空';
COMMENT ON COLUMN public.t_mail_digest_config.include_overdue IS '是否包含「已逾期」节；两节都 false 会被 service 拒绝';
COMMENT ON COLUMN public.t_mail_digest_config.include_upcoming IS '是否包含「未来 N 天」节；两节都 false 会被 service 拒绝';
COMMENT ON COLUMN public.t_mail_digest_config.upcoming_days IS '未来节窗口天数，clamp 1..60，缺省 14';
COMMENT ON COLUMN public.t_mail_digest_config.visible_columns IS '正文表格列白名单（varchar(30)[]）；未知列返 21704';
COMMENT ON COLUMN public.t_mail_digest_config.subject_template IS '邮件主题模板；NULL = 用缺省主题';
COMMENT ON COLUMN public.t_mail_digest_config.deleted_at IS '软删标记；未软删行由 uk_t_mail_digest_config_alive 约束只有一行';

CREATE UNIQUE INDEX uk_t_mail_digest_config_alive
    ON t_mail_digest_config ((true)) WHERE deleted_at IS NULL;

-- ---------------------------------------------------------------------------
-- 发送流水表
-- ---------------------------------------------------------------------------
-- ⚠️ **刻意不软删**（无 deleted_at）：它是「这封信到底发没发出去」的审计流水，
--    `GET /mail-digest/logs` 直接按 sent_at DESC 倒序翻页。软删一份审计流水
--    会让「定时任务是不是在偷偷重发」这类问题永远查不清。
--    代价是本表只增不减 —— 交接给运维：按 sent_at 做分区 / 定期归档（当前量级
--    每天 1 行，可忽略）。
--
-- ⚠️ **无论成败都 INSERT**：SMTP 层报错时也要落一行（status='FAILED' + error_message）。
--    只记成功会让「定时任务每天都在失败」这件事完全不可观测。
CREATE TABLE public.t_mail_digest_log (
    id             bigint NOT NULL,
    -- 发送时的配置版本号（快照）。改了配置不影响历史行的可解释性。
    config_version integer NOT NULL,
    -- 触发来源：SCHEDULED（cron）/ MANUAL（POST /send）
    trigger_source character varying(20) NOT NULL,
    -- 收件人快照，逗号分隔（存快照而非 join 配置表：配置改了之后仍要看当时发给谁）
    to_recipients  text NOT NULL,
    -- 抄送快照，逗号分隔；无抄送时为空串（不是 NULL）
    cc_recipients  text NOT NULL DEFAULT '',
    subject        character varying(300) NOT NULL,
    overdue_count  integer NOT NULL DEFAULT 0,
    upcoming_count integer NOT NULL DEFAULT 0,
    -- 正文 HTML 字节数（UTF-8）；正文本身不存表，需要复现用 POST /preview 重渲
    html_bytes     integer NOT NULL DEFAULT 0,
    status         character varying(20) NOT NULL, -- SUCCESS / FAILED
    error_message  character varying(500),
    sent_at        timestamp without time zone DEFAULT now() NOT NULL,
    created_at     timestamp without time zone DEFAULT now() NOT NULL
);

COMMENT ON TABLE  public.t_mail_digest_log IS '交期汇总邮件发送流水（mail_digest 域）；成败都记，刻意不软删';
COMMENT ON COLUMN public.t_mail_digest_log.id IS '雪花主键，App 侧生成';
COMMENT ON COLUMN public.t_mail_digest_log.config_version IS '发送时的 t_mail_digest_config.version 快照';
COMMENT ON COLUMN public.t_mail_digest_log.trigger_source IS 'SCHEDULED=cron 定时 / MANUAL=POST /send 手动';
COMMENT ON COLUMN public.t_mail_digest_log.to_recipients IS '收件人快照（逗号分隔）';
COMMENT ON COLUMN public.t_mail_digest_log.cc_recipients IS '抄送快照（逗号分隔）；无则空串';
COMMENT ON COLUMN public.t_mail_digest_log.subject IS '本次邮件主题快照';
COMMENT ON COLUMN public.t_mail_digest_log.overdue_count IS '逾期节行数（工单级）';
COMMENT ON COLUMN public.t_mail_digest_log.upcoming_count IS '未来节行数（工单级）';
COMMENT ON COLUMN public.t_mail_digest_log.html_bytes IS '正文 HTML 字节数（UTF-8）；正文本体不入库，复现走 POST /preview';
COMMENT ON COLUMN public.t_mail_digest_log.status IS 'SUCCESS / FAILED；SMTP 层报错也落 FAILED 行';
COMMENT ON COLUMN public.t_mail_digest_log.error_message IS '失败原因（截断到 500 字符）；成功时 NULL';
COMMENT ON COLUMN public.t_mail_digest_log.sent_at IS '发送时刻（naive timestamp，Asia/Shanghai）';

CREATE INDEX ix_t_mail_digest_log_sent_at ON t_mail_digest_log (sent_at DESC);
