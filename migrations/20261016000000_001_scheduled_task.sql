-- ============================================================================
-- 20261016000000_001_scheduled_task —— 「定时任务」域的邮件通道（邮箱实体）
-- ============================================================================
-- 需求（用户原话）：「定时任务菜单的名称结构重新设计。一级菜单：定时任务，二级菜单：
--       邮件。页面上方使用 tab 区分不同的邮件类型……邮箱账号和授权码可以在页面中配置」。
--
-- 归属域：`scheduled_task`（`src/modules/scheduled_task/`），URL 挂 `/api/v2/mail`。
--         本域只承载**邮件通道**（邮箱实体 + SMTP 预设元数据）；调度与内容渲染
--         仍属 `mail_digest` 域（`t_mail_digest_config` / `t_mail_digest_log`）。
--
-- 与本仓既有约定的关系（全部照 CLAUDE.md「DB 约定」）：
--   * 无物理外键 —— 本表不引用任何他域表；被 mail_digest 引用的那条边
--     （`t_mail_digest_config.sender_account_id`）也是裸 bigint + service 校验。
--   * 无 DB ENUM —— `preset` 是 varchar(30)，合法值由 Rust 侧
--     `dto::MailAccountPreset` 校验（`NET_EASE_163` / `TENCENT_QQ`）。
--   * 乐观锁 —— 带 `version` 列，`POST /mail/accounts/{id}` 走
--     `UPDATE … WHERE id=$1 AND version=$2`，0 行 → 40901 VERSION_CONFLICT。
--   * 软删 —— 带 `deleted_at`；两个部分唯一索引都以 `deleted_at IS NULL` 为前提。
--   * 雪花主键 —— `id` 由 App 侧 `SnowflakeIdGenerator` 生成，不在 DB 里取号。
--
-- ⚠️ **授权码加密入库**：`encrypted_secret` 存 AES-256-GCM 密文
--   （`base64(nonce12 ‖ ct ‖ tag16)`，AAD = `t_mail_account.secret.v1`），
--   主密钥来自 env `MAIL_ACCOUNT_SECRET`（见 `src/infra/config.rs::MailAccountSecret`
--   与 `src/infra/crypto.rs`）。**没有掩码列、没有 `secret_configured` 布尔**：
--   建邮箱时授权码必填 ⇒ 该布尔恒 true，是死字段，而死字段会让人误以为
--   「授权码可以是空的」。（加密而非散列的理由：SMTP 发信必须取回明文，
--   bcrypt 那条路是单向的。）
--
-- ⚠️ **换 `MAIL_ACCOUNT_SECRET` 会让本表所有密文永久解不开**（GCM 无密钥轮换）。
--   该缺口与「逐行重新加密」的迁移流程登记在 `docs/api/mail.md` §8.3。
-- ============================================================================

-- ---------------------------------------------------------------------------
-- 邮箱表
-- ---------------------------------------------------------------------------
-- ⚠️ **普通 CRUD 列表，不是单例**：与 `t_mail_digest_config` 的「一行未软删记录 +
-- 表达式唯一索引」形态刻意不同 —— 那一套是为了「配置页在还没配过时也能打开」，
-- 而这里管理员要能同时配多个邮箱（163 一个、QQ 一个），故走标准
-- 「雪花主键 + 业务键唯一索引 + OCC」形态。
--
-- ⚠️ **两个索引都以 `deleted_at IS NULL` 为前提**（partial unique index）：
--   软删后索引项消失 ⇒ 同名邮箱可以再建（审计行仍留存）。
--   `uk_t_mail_account_one_default` 只对 `is_default = true` 的未软删行生效 ⇒
--   「至多一个默认邮箱」由 DB 保证，写侧**必须先降级旧默认再升新默认**（同一事务内，
--   否则第二个 UPDATE 撞 23505）。
CREATE TABLE public.t_mail_account (
    id               bigint NOT NULL,
    -- 乐观锁版本号；每次 UPDATE 自增；0 行命中 → 40901 VERSION_CONFLICT
    version          integer NOT NULL DEFAULT 0,
    -- 自定义名称（管理端展示用，如「汇森告警邮箱」）。未软删行内唯一。
    name             character varying(50) NOT NULL,
    -- SMTP 预设：NET_EASE_163 / TENCENT_QQ（无 DB ENUM，Rust 侧校验）。
    -- **预设是 host/port/tls_mode 的唯一来源** —— 本表不存主机名/端口，
    -- 写错一个端口的代价是「握手失败」这种最难自查的错，见 docs/api/mail.md §4。
    preset           character varying(30) NOT NULL,
    -- SMTP 登录账号（163/QQ 都是完整邮箱地址）。同时是**发件人地址**的来源。
    account          character varying(120) NOT NULL,
    -- AES-256-GCM 密文（base64(nonce12 ‖ ct ‖ tag16)）。**绝不出现在任何出参里**。
    encrypted_secret text NOT NULL,
    -- 是否默认邮箱。至多一个未软删行为 true（uk_t_mail_account_one_default 保证）。
    is_default       boolean NOT NULL DEFAULT false,

    created_at       timestamp without time zone DEFAULT now() NOT NULL,
    created_by       bigint,
    updated_at       timestamp without time zone DEFAULT now() NOT NULL,
    updated_by       bigint,
    deleted_at       timestamp without time zone
);

COMMENT ON TABLE  public.t_mail_account IS '定时任务·邮件通道：邮箱实体（scheduled_task 域）；授权码 AES-256-GCM 加密入库';
COMMENT ON COLUMN public.t_mail_account.id IS '雪花主键，App 侧生成；出参按 JSON string 序列化';
COMMENT ON COLUMN public.t_mail_account.version IS '乐观锁版本号；POST /accounts/{id} 全量覆写时带 WHERE version=$2，0 行 → 40901';
COMMENT ON COLUMN public.t_mail_account.name IS '自定义名称（varchar(50)）；未软删行内唯一，重复 → 21707';
COMMENT ON COLUMN public.t_mail_account.preset IS 'SMTP 预设：NET_EASE_163 / TENCENT_QQ；host/port/tls_mode 由预设表推导，不存本表';
COMMENT ON COLUMN public.t_mail_account.account IS 'SMTP 登录账号（varchar(120)，163/QQ 均为完整邮箱地址）；同时是发件人地址来源';
COMMENT ON COLUMN public.t_mail_account.encrypted_secret IS '授权码密文（AES-256-GCM，base64(nonce‖ct‖tag)）；主密钥 env MAIL_ACCOUNT_SECRET；出参绝不回显';
COMMENT ON COLUMN public.t_mail_account.is_default IS '是否默认邮箱；至多一个未软删行为 true；被 mail_digest 配置选为 sender 时按账号解析，不依赖本列';
COMMENT ON COLUMN public.t_mail_account.deleted_at IS '软删标记；被 t_mail_digest_config.sender_account_id 引用时拒删（21708）';

CREATE UNIQUE INDEX uk_t_mail_account_name_alive
    ON t_mail_account ((name)) WHERE deleted_at IS NULL;

CREATE UNIQUE INDEX uk_t_mail_account_one_default
    ON t_mail_account ((true)) WHERE deleted_at IS NULL AND is_default;

-- ---------------------------------------------------------------------------
-- mail_digest 侧：交期汇总配置选哪个邮箱
-- ---------------------------------------------------------------------------
-- ⚠️ **无物理外键**（CLAUDE.md「DB 约定」）：被引用邮箱软删/不存在时，
--   解析器返 `21701 BIZ_MAIL_NOT_CONFIGURED`（而非外键报 50001）；
--   删除侧的拦截在 `scheduled_task::service::delete_account`（查引用 → 21708）。
--
-- ⚠️ **NULL = 未选邮箱**（合法状态）：新装的库在管理员第一次打开配置页之前就是
--   NULL。定时任务因此**不注册 cron**（`task::mail_digest` 的判据：
--   `t_mail_digest_config.enabled` 且存在可用发信通道），
--   手动 `POST /send` 同样返 21701 —— 与旧 env 方案「没配 SMTP 就跳过」的
--   降级语义逐条对齐，只是判据从 env 换成了本列。
ALTER TABLE public.t_mail_digest_config
    ADD COLUMN sender_account_id bigint;

COMMENT ON COLUMN public.t_mail_digest_config.sender_account_id IS
    '发信邮箱（t_mail_account.id，无物理外键）；NULL = 未选邮箱，此时定时任务不注册 cron、/send 返 21701。被引用的邮箱禁止删除（21708）';

CREATE INDEX ix_t_mail_digest_config_sender_account_id
    ON t_mail_digest_config (sender_account_id);
