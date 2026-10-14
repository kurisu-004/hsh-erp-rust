-- ============================================================================
-- 20261018000000_001_wecom —— 「定时任务」域的企业微信推送（通道 + 每日总结）
-- ============================================================================
-- 需求（用户原话）：「api挂载到 /cron/wecom_daily 下，邮件的API也移动到 /cron 下」。
--
-- 归属域（本迁移只建表，不建模块）：
--   * `src/modules/wecom/`      —— 企业微信**通道**（群机器人 webhook 实体）
--   * `src/modules/wecom_daily/` —— 企业微信**内容**（每日总结推送配置 + 流水）
--   URL 挂 `/api/v2/cron/wecom` 与 `/api/v2/cron/wecom-daily`（见
--   `src/modules/mod.rs::v2_router()` 的 `/cron` 前缀族登记）。
--   ⚠️ 与邮件两域同款切分：**通道**（有哪些可发的 webhook）与**内容**（每日总结
--   推什么、什么时候推）是两个独立演进的职责。
--
-- 与本仓既有约定的关系（全部照 CLAUDE.md「DB 约定」）：
--   * 无物理外键 —— 本组表不引用任何他域表；被 `wecom_daily` 引用的那条边
--     （`t_wecom_daily_config.webhook_id`）也是裸 bigint + service 校验。
--   * 无 DB ENUM —— `frequency` / `trigger_source` / `status` / `worker_metric`
--     都是 varchar，合法值由 Rust 侧枚举校验。
--   * 乐观锁 —— 带 `version` 列，写路径走
--     `UPDATE … WHERE id=$1 AND version=$2`，0 行 → 40901 VERSION_CONFLICT。
--   * 软删 —— 带 `deleted_at`；部分唯一索引都以 `deleted_at IS NULL` 为前提。
--   * 雪花主键 —— `id` 由 App 侧 `SnowflakeIdGenerator` 生成，不在 DB 里取号。
--
-- 🔒 **webhook URL 加密入库（本文件的头号硬约束）**：`encrypted_url` 存
--   AES-256-GCM 密文（`base64(nonce12 ‖ ct ‖ tag16)`，AAD = `t_wecom_webhook.url.v1`），
--   主密钥**复用** env `MAIL_ACCOUNT_SECRET`（`src/infra/config.rs::MailAccountSecret`
--   与 `src/infra/crypto.rs::seal` / `open`），**不新增 env**。
--
--   ⚠️ **为什么绝不明文落库**：企业微信群机器人 webhook URL 形如
--   `https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=<uuid>`，那个 `key=` **就是
--   bearer 凭证** —— 任何人拿到它就能以本企业的名义往那个群发消息（官方文档明确
--   要求保护、不得泄漏）。它一旦明文入库，备份 / 库只读权限 / 备份转储都会把它
--   带出去；本仓对 SMTP 授权码已用同一套办法（`t_mail_account.encrypted_secret`），
--   企业微信 URL 按**同等强度**处理。
--
--   ⚠️ **`MAIL_ACCOUNT_SECRET` 一换，`t_wecom_webhook` 里所有 URL 永久解不开**
--   （GCM 无轮换机制，「逐行解密 → 换密钥重新加密」的迁移流程未实现）。该缺口与
--   `t_mail_account` **完全同款**、同样未实现，登记在 `docs/api/mail.md` §8.3；
--   两张表共用一把钥匙也意味着「换密钥」这一缺口是同一个，不分开记。
--
-- ⚠️ `webhook_id` **无物理外键**（合仓约定）：被引用 webhook 软删 / 不存在时，
--   由 service 返 `21806 BIZ_WECOM_WEBHOOK_NOT_FOUND`，而不是外键报 50001；
--   删除侧的拦截在 `wecom::service`（查 `t_wecom_daily_config` 引用 → `21808`）。
--   ⚠️ 与 `t_mail_digest_config.sender_account_id` 同款：存在 TOCTOU 窗口
--   （校验与写入不在同一把锁下），**有意不修** —— 为一个管理员配置动作引入
--   分布式锁不划算，且真撞上时 `21806` 也会如实报出来。
--
-- ⚠️ **为什么 `uk_t_wecom_daily_config_alive` 是表达式唯一索引**
--   （`ON … ((true)) WHERE deleted_at IS NULL`）：配置是**单例**。表达式索引在
--   PG 里是「所有行的 `true` 必须全表唯一」⇒ 只允许存在**至多一行**未软删配置，
--   且不写死 id。它兜的是「并发首次初始化」：两个请求都看到 `version=0` 的空表，
--   都去 INSERT，后到者撞 **23505** unique_violation ⇒ service 把它转成 OCC 冲突
--   `40901`（与 `t_mail_digest_config` 同一套处理）。没有它，首次初始化靠
--   「SELECT 后 INSERT」的读改写窗口会静默产生两行配置。
-- ============================================================================

-- ---------------------------------------------------------------------------
-- 1. 群机器人 webhook 通道实体
-- ---------------------------------------------------------------------------
-- ⚠️ **普通 CRUD 列表，不是单例**：与 `t_wecom_daily_config` 的单例形态刻意不同 ——
-- 管理员可能要同时配多个群（管理层群 / 车间群），故走标准
-- 「雪花主键 + 业务键唯一索引 + OCC」形态。
--
-- ⚠️ **两个索引都以 `deleted_at IS NULL` 为前提**（partial unique index）：
--   软删后索引项消失 ⇒ 同名 webhook 可以再建（审计行仍留存）。
--   `uk_t_wecom_webhook_one_default` 只对 `is_default = true` 的未软删行生效 ⇒
--   「至多一个默认 webhook」由 DB 保证，写侧**必须先降级旧默认再升新默认**
--   （同一事务内，否则第二个 UPDATE 撞 23505）。
CREATE TABLE public.t_wecom_webhook (
    id             bigint NOT NULL,
    -- 乐观锁版本号；每次 UPDATE 自增；0 行命中 → 40901 VERSION_CONFLICT
    version        integer NOT NULL DEFAULT 0,
    -- 自定义名称（管理端展示用，如「管理层群机器人」）。未软删行内唯一。
    name           character varying(50) NOT NULL,
    -- AES-256-GCM 密文（base64(nonce12 ‖ ct ‖ tag16)）。**绝不出现在任何出参里**。
    -- ⚠️ 明文里是 `…/webhook/send?key=<bearer 凭证>`，见文件头「凭据加密」段。
    encrypted_url  text NOT NULL,
    -- 是否默认 webhook。至多一个未软删行为 true
    -- （uk_t_wecom_webhook_one_default 保证）。被每日总结配置选为 webhook 时按
    -- 行 id 解析，不依赖本列。
    is_default     boolean NOT NULL DEFAULT false,

    created_at     timestamp without time zone DEFAULT now() NOT NULL,
    created_by     bigint,
    updated_at     timestamp without time zone DEFAULT now() NOT NULL,
    updated_by     bigint,
    deleted_at     timestamp without time zone
);

COMMENT ON TABLE  public.t_wecom_webhook IS '定时任务·企业微信通道：群机器人 webhook 实体（wecom 域）；URL AES-256-GCM 加密入库';
COMMENT ON COLUMN public.t_wecom_webhook.id IS '雪花主键，App 侧生成；出参按 JSON string 序列化';
COMMENT ON COLUMN public.t_wecom_webhook.version IS '乐观锁版本号；写路径带 WHERE version=$2，0 行 → 40901';
COMMENT ON COLUMN public.t_wecom_webhook.name IS '自定义名称（varchar(50)）；未软删行内唯一，重复 → 21807';
COMMENT ON COLUMN public.t_wecom_webhook.encrypted_url IS 'webhook URL 密文（AES-256-GCM，base64(nonce‖ct‖tag)，AAD=t_wecom_webhook.url.v1）；主密钥复用 env MAIL_ACCOUNT_SECRET；明文里的 key= 是 bearer 凭证，出参绝不回显';
COMMENT ON COLUMN public.t_wecom_webhook.is_default IS '是否默认 webhook；至多一个未软删行为 true；被每日总结配置选为 webhook 时按行 id 解析，不依赖本列';
COMMENT ON COLUMN public.t_wecom_webhook.deleted_at IS '软删标记；被 t_wecom_daily_config.webhook_id 引用时拒删（21808）';

ALTER TABLE ONLY public.t_wecom_webhook ADD CONSTRAINT t_wecom_webhook_pkey PRIMARY KEY (id);

CREATE UNIQUE INDEX uk_t_wecom_webhook_name_alive
    ON public.t_wecom_webhook (name) WHERE deleted_at IS NULL;
CREATE UNIQUE INDEX uk_t_wecom_webhook_one_default
    ON public.t_wecom_webhook ((true)) WHERE deleted_at IS NULL AND is_default;

-- ---------------------------------------------------------------------------
-- 2. 每日总结推送配置（单例）
-- ---------------------------------------------------------------------------
-- ⚠️ **NULL = 合法状态**：新装的库在管理员第一次打开配置页之前就是空表。
-- `GET /config` 因此返**全部默认值 + version=0 + id="0"**（不是 404），
-- 与 `t_mail_digest_config` 逐条同款 —— 配置页要在「还没配过」时也能打开。
-- 定时任务的注册判据是「`enabled` 且 `webhook_id` 指向一个未软删 webhook」。
CREATE TABLE public.t_wecom_daily_config (
    id                  bigint NOT NULL,
    version             integer NOT NULL DEFAULT 0,
    -- 是否启用定时推送。手动触发（预览 / 立即发送）**不看**本列。
    enabled             boolean NOT NULL DEFAULT false,
    -- 频率：DAILY / WEEKLY / MONTHLY / MANUAL_ONLY（无 DB ENUM，Rust 侧枚举）。
    -- ⚠️ MANUAL_ONLY 时 send_hour / send_minute / weekday / day_of_month 恒为 NULL；
    --   WEEKLY 必填 weekday、MONTHLY 必填 day_of_month（违者 21804）。
    frequency           character varying(20) NOT NULL DEFAULT 'DAILY',
    send_hour           smallint,
    send_minute         smallint,
    weekday             smallint,
    day_of_month        smallint,
    -- 推送 webhook（t_wecom_webhook.id，**无物理外键**）。NULL = 未选通道。
    webhook_id          bigint,
    -- 「未来 N 天交期」节的天数窗口。
    upcoming_days       integer NOT NULL DEFAULT 3,
    -- 工人贡献度的度量口径：QUANTITY（件数）/ HOURS（工时）。
    -- ⚠️ 无 DB ENUM：未知值在写侧就被拒（21804），不在 DB 里枚举。
    worker_metric       character varying(10) NOT NULL DEFAULT 'QUANTITY',
    -- 工人榜取前 N 名。
    worker_top_n        integer NOT NULL DEFAULT 15,
    -- Markdown 分片上限：企业微信单条消息有长度上限，超了就切片分条发。
    max_chunks          smallint NOT NULL DEFAULT 3,
    -- 四节开关（概览 / 送货 / 工单 / 工人）。⚠️ **四节全关 ⇒ 21804** ——
    -- 那是一条空消息，渲染出来没有信息量。
    include_overview    boolean NOT NULL DEFAULT true,
    include_delivery    boolean NOT NULL DEFAULT true,
    include_orders      boolean NOT NULL DEFAULT true,
    include_workers     boolean NOT NULL DEFAULT true,
    -- 消息标题模板；NULL ⇒ 用域内默认串。
    subject_template    character varying(200),

    created_at          timestamp without time zone DEFAULT now() NOT NULL,
    created_by          bigint,
    updated_at          timestamp without time zone DEFAULT now() NOT NULL,
    updated_by          bigint,
    deleted_at          timestamp without time zone
);

COMMENT ON TABLE  public.t_wecom_daily_config IS '定时任务·企业微信内容：每日总结推送配置单例（wecom_daily 域）';
COMMENT ON COLUMN public.t_wecom_daily_config.id IS '雪花主键，App 侧生成；出参按 JSON string 序列化；空表时读端点返 id="0" + version=0';
COMMENT ON COLUMN public.t_wecom_daily_config.version IS '乐观锁版本号；全量覆写时带 WHERE version=$2，0 行 → 40901；首次并发初始化撞 23505 → 转 40901';
COMMENT ON COLUMN public.t_wecom_daily_config.frequency IS '推送频率：DAILY / WEEKLY / MONTHLY / MANUAL_ONLY（无 DB ENUM，Rust 侧枚举）；与时刻字段不匹配 → 21804';
COMMENT ON COLUMN public.t_wecom_daily_config.webhook_id IS '推送 webhook（t_wecom_webhook.id，无物理外键）；NULL = 未选通道，此时定时任务不注册 job、手动发送返 21801。被引用的 webhook 禁止删除（21808）';
COMMENT ON COLUMN public.t_wecom_daily_config.worker_metric IS '工人贡献度口径：QUANTITY / HOURS（无 DB ENUM）；未知值 → 21804';
COMMENT ON COLUMN public.t_wecom_daily_config.max_chunks IS 'Markdown 分片上限（smallint）；渲染后实际片数超它 → 21804，不是静默截断';
COMMENT ON COLUMN public.t_wecom_daily_config.subject_template IS '消息标题模板（varchar(200)）；NULL ⇒ 用域内默认串';

ALTER TABLE ONLY public.t_wecom_daily_config ADD CONSTRAINT t_wecom_daily_config_pkey PRIMARY KEY (id);

-- ⚠️ 表达式唯一索引：保证「至多一行未软删配置」，见文件头「为什么是表达式唯一索引」。
CREATE UNIQUE INDEX uk_t_wecom_daily_config_alive
    ON public.t_wecom_daily_config ((true)) WHERE deleted_at IS NULL;

CREATE INDEX ix_t_wecom_daily_config_webhook_id
    ON public.t_wecom_daily_config (webhook_id);

-- ---------------------------------------------------------------------------
-- 3. 推送流水（只增的审计流水）
-- ---------------------------------------------------------------------------
-- ⚠️ **刻意无 `version`、无 `deleted_at`**（与前两张表不同）：
--   流水是**只增**的审计记录 —— 没有人会改一行历史，也没有人会软删它
--   （软删流水等于让审计可被抹掉）。OCC 对它没有意义，唯一键只有 id。
--
-- ⚠️ **`chunk_sent < chunk_count ⟺ status='FAILED'`**：
--   正常成功两条相等；部分分片发出去后失败是 `chunk_sent < chunk_count`。
--   ⚠️ **`chunk_count = 0` 是合法值** —— 表示「准备阶段就失败、还没渲染」
--   （拿不到 webhook、解不开密文、统计 SQL 报错…）。这与
--   `mail_digest::service::log_prepare_failure` 的既有行为对齐：邮件侧同样有
--   「渲染前失败」这一档，那一档在流水里也是空内容 + 一条 FAILED。
--   ⚠️ 这条不变量意味着**不能**用「chunk_count > 0」当「进了渲染阶段」的判据。
CREATE TABLE public.t_wecom_daily_log (
    id                      bigint NOT NULL,
    -- 推送当时的配置版本（快照，不 join 配置表）—— 配置后来改了，历史仍是当时的。
    config_version          integer NOT NULL,
    -- 触发源：SCHEDULED（定时任务）/ MANUAL（手动触发）。
    trigger_source          character varying(20) NOT NULL,
    -- 推送当时的 webhook 名称（快照）。⚠️ 是**名称**不是 id：webhook 行可能
    --   已被软删，流水仍要能说清「发到哪个群去了」。
    webhook_name            character varying(50) NOT NULL,
    -- webhook URL 的**掩码**（只留可辨识片段，不含 key 全值）。
    -- ⚠️ 这一列存在的理由与 `t_mail_account`「不设掩码列」相反：那里掩码是
    --   不必要的泄漏面（前端没算法用它），而流水是**审计**场景，排障时需要
    --   肉眼确认「这两次发的是不是同一个群」。掩码不得还原出 key。
    webhook_masked          character varying(120) NOT NULL DEFAULT '',
    -- 统计基准日（推送「哪一天」的数据；跨零点的定时任务取业务日而非自然日）。
    report_date             date NOT NULL,
    -- 渲染出的 Markdown 字节数 / 分片数 / 实发片数。⚠️ chunk_count=0 合法，见文件头。
    markdown_bytes          integer NOT NULL DEFAULT 0,
    chunk_count             smallint NOT NULL DEFAULT 0,
    chunk_sent              smallint NOT NULL DEFAULT 0,
    -- 四节各自的计数（概览批次数/件数、送货单数/零件数、逾期数、临期数）。
    overview_batch_count    integer NOT NULL DEFAULT 0,
    overview_quantity       integer NOT NULL DEFAULT 0,
    note_count              integer NOT NULL DEFAULT 0,
    note_part_count         integer NOT NULL DEFAULT 0,
    overdue_count           integer NOT NULL DEFAULT 0,
    upcoming_count          integer NOT NULL DEFAULT 0,
    -- 工人榜行数（分片前的总数，不是每片各多少）。
    worker_row_total        integer NOT NULL DEFAULT 0,
    -- SUCCESS / FAILED（无 DB ENUM，Rust 侧枚举）。
    status                  character varying(20) NOT NULL,
    -- 失败原因（varchar(500)；SUCCESS 时为 NULL）。
    error_message           character varying(500),
    -- 推送时刻（默认 now()）。⚠️ 与 created_at 冗余但**刻意都留**：sent_at 是
    -- 业务语义（这条消息什么时候发出去的），created_at 是行语义（这一行什么时候
    -- 写进来的）—— 两者在「补写历史流水」的场景下会分叉。
    sent_at                 timestamp without time zone DEFAULT now() NOT NULL,
    created_at              timestamp without time zone DEFAULT now() NOT NULL
);

COMMENT ON TABLE  public.t_wecom_daily_log IS '定时任务·企业微信内容：每日总结推送流水（wecom_daily 域）；只增审计行，刻意无 version / deleted_at';
COMMENT ON COLUMN public.t_wecom_daily_log.config_version IS '推送当时的配置版本快照（不 join 配置表）';
COMMENT ON COLUMN public.t_wecom_daily_log.trigger_source IS '触发源：SCHEDULED / MANUAL（无 DB ENUM）';
COMMENT ON COLUMN public.t_wecom_daily_log.webhook_name IS '推送当时的 webhook 名称快照（行可能已软删，流水仍要说得清发到哪个群）';
COMMENT ON COLUMN public.t_wecom_daily_log.webhook_masked IS 'webhook URL 掩码（不含 key 全值）；审计场景的肉眼确认用，取舍见建表注释';
COMMENT ON COLUMN public.t_wecom_daily_log.chunk_count IS '渲染后的分片数；= 0 表示「准备阶段就失败、还没渲染」，是合法值';
COMMENT ON COLUMN public.t_wecom_daily_log.chunk_sent IS '实发分片数；chunk_sent < chunk_count ⟺ status=FAILED';
COMMENT ON COLUMN public.t_wecom_daily_log.report_date IS '统计基准日（业务日）；与 sent_at 可能跨天';
COMMENT ON COLUMN public.t_wecom_daily_log.sent_at IS '推送时刻；与 created_at 语义不同（业务时刻 vs 行写入时刻）';

ALTER TABLE ONLY public.t_wecom_daily_log ADD CONSTRAINT t_wecom_daily_log_pkey PRIMARY KEY (id);

-- ⚠️ 列表默认按时间倒序，索引跟着这个方向建。
CREATE INDEX ix_t_wecom_daily_log_sent_at ON public.t_wecom_daily_log (sent_at DESC);
