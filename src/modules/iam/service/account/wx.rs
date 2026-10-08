//! `AccountService` 的 `t_wx_identity` 子块：管理端的绑定 / 解绑 / 查询
//!
//! 三个方法的权限守卫都是 `require_role(Role::Manager)`（service 层强制，handler
//! 不重复校验）。表是「**仅预绑定，不自动开户**」：登录侧（wx 域）只按表里的映射
//! 反查系统账号，命中不了就 40107。
//!
//! ## `corp_id` 唯一真相源 = 后端配置
//! `default_corp_id` 由 handler 从 `state.config.wecom.corpid` 传入（`AccountService`
//! 只持 `snowflake`，不注入 config）。请求体不再收 `corp_id` 字段 —— 它以前是
//! 「保留字段、一律忽略」，留着只会让调用方误以为能指定企业。配置为空仍返
//! `40109 BIZ_WX_NOT_CONFIGURED`。
//!
//! ## 绑定基数：双向一对一（业务层，无 DB 约束）
//! - `wx → system`：`uk_wx_identity_corp_user` partial unique 索引 + 应用层预检，
//!   冲突码 `40108 BIZ_WX_BINDING_DUPLICATE`。
//! - `system → wx`：本文件 `bind_wx_identity` 在插入前查活跃绑定数，非 0 且
//!   userid 不同即 `40110 BIZ_WX_USER_ALREADY_BOUND`。**没有**对应索引，
//!   count 与 INSERT 之间存在 TOCTOU 窗口（靠管理端低并发兜住），登记在
//!   `docs/api/iam.md` 的「已知偏差登记」。

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::clock::now_naive;
use crate::modules::iam::repo::WxIdentityInsert;
use crate::shared::error::{AppError, code};

use super::super::super::dto::WxBindRequest;
use super::super::super::repo::IamRepoTrait;
use super::super::super::vo::WxIdentityOut;
use super::AccountService;
use super::{
    MAX_CORP_ID_LEN, MAX_WX_USER_ID_LEN, map_duplicate_wx_identity, to_wx_identity_out,
    user_not_found, version_conflict,
};

impl AccountService {
    /// `POST /api/v2/iam/users/{id}/wx-bind`：把企业微信 userid 预绑定到系统账号。
    ///
    /// ## 幂等语义
    /// - 绑到**同一个** `user_id` + **同一个** userid → 幂等成功（返回已有绑定）
    /// - 同一 userid 已绑到**别的** `user_id` → `40108`（wx → system 方向冲突）
    /// - 该 `user_id` 已绑了**别的** userid → `40110`（system → wx 方向冲突）
    ///
    /// ## 校验顺序（前端按首个失败的分支提示，改动时勿调换）
    /// 归一化 userid → 配置 corp_id（空 → 40109）→ 目标账号存在性（404）→
    /// wx→system 冲突（40108）→ system→wx 冲突（40110）→ 插入。
    pub async fn bind_wx_identity<R: IamRepoTrait>(
        &self,
        mut repo: R,
        user_id: i64,
        req: &WxBindRequest,
        default_corp_id: &str,
        current: &CurrentUser,
    ) -> Result<WxIdentityOut, AppError> {
        current.require_role(Role::Manager)?;

        // 归一化：userid trim + 转小写。依据是企业微信「发送应用消息」文档
        // （/document/path/90236）对返回 userid 的说明：userid 不区分大小写，
        // 统一转小写。不归一会让 "ZhangSan" 与 "zhangsan" 变成两条绑定。
        let wx_user_id = req.wx_user_id.trim().to_lowercase();
        if wx_user_id.is_empty() {
            return Err(AppError::validation("wx_user_id 不能为空"));
        }
        if wx_user_id.chars().count() > MAX_WX_USER_ID_LEN {
            return Err(AppError::validation(format!(
                "wx_user_id 长度超限（> {MAX_WX_USER_ID_LEN} 字符）"
            )));
        }

        // corp_id：**只认后端配置**（登录侧同样只认配置值，两侧必须对称，否则会写出
        // 永远登不进来的死行、还会在别的企业命名空间占住唯一坑位）。
        let corp_id = default_corp_id.trim();
        if corp_id.is_empty() {
            return Err(AppError::biz(
                code::BIZ_WX_NOT_CONFIGURED,
                "企业微信登录未配置（WECOM_CORPID 为空），无法绑定",
            ));
        }
        if corp_id.chars().count() > MAX_CORP_ID_LEN {
            return Err(AppError::validation(format!(
                "corp_id 长度超限（> {MAX_CORP_ID_LEN} 字符）"
            )));
        }

        // 目标账号必须存在（防绑到不存在的 user_id）
        repo.get_user_by_id(user_id)
            .await?
            .ok_or_else(|| user_not_found(user_id))?;

        // wx → system 方向：同一个 (corp_id, wx_user_id) 只能对应一个系统账号
        if let Some(existing) = repo
            .get_wx_identity_by_corp_and_user(corp_id, &wx_user_id)
            .await?
        {
            if existing.user_id == user_id {
                return Ok(to_wx_identity_out(existing)); // 幂等
            }
            return Err(AppError::biz(
                code::BIZ_WX_BINDING_DUPLICATE,
                "该企业微信账号已绑定到其他系统账号",
            ));
        }

        // system → wx 方向：一个系统账号最多一个活跃绑定
        let active = repo.count_active_wx_identities_by_user_id(user_id).await?;
        if active > 0 {
            let rows = repo.get_wx_identity_by_user_id(user_id).await?;
            if let Some(same) = rows.into_iter().find(|r| r.wx_user_id == wx_user_id) {
                return Ok(to_wx_identity_out(same)); // 幂等（corp 与 userid 都相同）
            }
            return Err(AppError::biz(
                code::BIZ_WX_USER_ALREADY_BOUND,
                "该系统账号已绑定其它企业微信 userid，请先解绑",
            ));
        }

        let insert = WxIdentityInsert {
            id: self.snowflake.next_id(),
            corp_id: corp_id.to_string(),
            wx_user_id,
            user_id,
            created_at: now_naive(),
            created_by: Some(current.id),
        };
        // 唯一索引兜底：并发插入撞 `uk_wx_identity_corp_user` → 40108 而非 500
        repo.create_wx_identity(&insert)
            .await
            .map_err(map_duplicate_wx_identity)?;

        let row = repo
            .get_wx_identity_by_corp_and_user(&insert.corp_id, &insert.wx_user_id)
            .await?
            .ok_or_else(|| AppError::internal("创建后回读企业微信绑定失败"))?;
        Ok(to_wx_identity_out(row))
    }

    /// `POST /api/v2/iam/users/{id}/wx-bind/unbind`：软删该账号的全部活跃绑定。
    ///
    /// ## 幂等语义
    /// 该 user 当前**没有**活跃绑定时重复调用 → 成功（`Ok(())`）。
    ///
    /// ## 存量多行绑定
    /// 本仓没有 `uk_wx_identity_user_id` 索引，早期写入可能留下同一 `user_id` 的
    /// 多行活跃绑定。解绑**全部**软删（解绑账号 = 该账号的所有企业微信身份一并失效），
    /// 逐行带同一个 `expected_version` 写，任一行 affected==0 → 409（handler 未
    /// commit，整笔回滚）。
    pub async fn unbind_wx_identity<R: IamRepoTrait>(
        &self,
        mut repo: R,
        user_id: i64,
        expected_version: i32,
        current: &CurrentUser,
    ) -> Result<(), AppError> {
        current.require_role(Role::Manager)?;

        let rows = repo.get_wx_identity_by_user_id(user_id).await?;
        if rows.is_empty() {
            // 幂等：本来就没绑
            return Ok(());
        }

        let when = now_naive();
        for r in rows {
            let affected = repo
                .soft_delete_wx_identity(r.id, expected_version, when, Some(current.id))
                .await?;
            if affected == 0 {
                // 乐观锁冲突：并发已被别人解绑 / 改过。整体回滚（handler 未 commit）
                return Err(version_conflict());
            }
        }
        Ok(())
    }

    /// `GET /api/v2/iam/users/{id}/wx-bind`：查当前绑定，无绑定 → `None`。
    ///
    /// 响应是**单条**而非数组：业务上双向一对一。若存量数据存在多行（无
    /// `uk_wx_identity_user_id` 索引，见 `unbind_wx_identity` 的说明），按
    /// `created_at ASC, id ASC` 取第一行。
    pub async fn get_wx_identity<R: IamRepoTrait>(
        &self,
        mut repo: R,
        user_id: i64,
        current: &CurrentUser,
    ) -> Result<Option<WxIdentityOut>, AppError> {
        current.require_role(Role::Manager)?;

        let rows = repo.get_wx_identity_by_user_id(user_id).await?;
        Ok(rows.into_iter().next().map(to_wx_identity_out))
    }
}
