-- ============================================================================
--  企业微信小程序登录：身份映射表 t_wx_identity（2026-09-29 新增）
--
--  ## 用途
--  记录「企业微信 userid → hsh-erp 系统账号 t_user.id」的预绑定关系。
--  企业微信小程序（自建应用）走 `GET /cgi-bin/miniprogram/jscode2session`，
--  返回明文 `userid`（非微信的 openid）。本表是唯一的身份映射真相源：
--  wx-login 端点拿 userid 后按 `(corp_id, wx_user_id)` 反查本表，
--  **未绑定直接拒绝**（不自动开户），由管理员在
--  `POST /api/v2/iam/users/{id}/wx-bind` 端点预绑定。
--
--  ## 表设计对齐 migrations/README.md
--  - 无物理外键（`user_id` 是普通 bigint 列 + 索引，引用完整性由 service 层保证）
--  - 雪花 bigint 主键（App 侧 `SnowflakeIdGenerator::next_id()` 生成）
--  - `version integer NOT NULL DEFAULT 0` 乐观锁（解绑走 soft_delete，带 version 条件）
--  - `deleted_at timestamp NULL` 软删（查询统一 `WHERE deleted_at IS NULL`）
--  - 审计字段 created_at / created_by / updated_at / updated_by，naive timestamp
--    （`DEFAULT now()` 与 t_user 对齐；App 侧仍显式传值，DEFAULT 只是兜底）
--
--  ## 唯一索引为何带 `WHERE deleted_at IS NULL`
--  `uk_wx_identity_corp_user` 是 **partial unique**（与 `uk_t_user_role_scope`、
--  `uk_t_outsource_company_name` 同形）：软删行不参与唯一性判定，因此
--  「解绑 → 重新绑定同一个 userid」不会撞唯一索引冲突。若建成全表唯一索引，
--  解绑一次就永久占坑，管理员再也无法把该 userid 绑到别的系统账号。
--  同时该索引保证同一 `(corp_id, wx_user_id)` 在同一时刻至多绑定一个系统账号
--  （防止一个企微账号同时登录两个 hsh-erp 账号）。
-- ============================================================================

CREATE TABLE public.t_wx_identity (
    id bigint NOT NULL,
    corp_id character varying(64) NOT NULL,
    wx_user_id character varying(64) NOT NULL,
    user_id bigint NOT NULL,
    version integer NOT NULL DEFAULT 0,
    created_at timestamp without time zone DEFAULT now() NOT NULL,
    created_by bigint,
    updated_at timestamp without time zone DEFAULT now() NOT NULL,
    updated_by bigint,
    deleted_at timestamp without time zone
);

-- 主键：与 baseline 全部 34 张表的 `<table>_pkey PRIMARY KEY (id)` 约定对齐。
-- 2026-09-29 补（review 第 1 轮 Y1）：初版只建了 partial unique + user_id 索引，
-- **没有任何索引能定位单行 id** → `WxIdentityRepo::soft_delete` 的
-- `WHERE id = $1 AND version = $2` 只能走 seq scan。雪花 id 本身唯一，不构成
-- 正确性 bug，但与全库约定不一致，且解绑路径在绑定量涨起来后会成为慢点。
ALTER TABLE ONLY public.t_wx_identity
    ADD CONSTRAINT t_wx_identity_pkey PRIMARY KEY (id);

-- 同一企业内 userid 唯一（软删后释放）；跨企业互不影响（corp_id 参与唯一键）
CREATE UNIQUE INDEX uk_wx_identity_corp_user
    ON public.t_wx_identity (corp_id, wx_user_id) WHERE deleted_at IS NULL;

-- 反查「该系统账号绑了哪些企微身份」+ 解绑时定位全部绑定行
CREATE INDEX idx_wx_identity_user ON public.t_wx_identity (user_id);
