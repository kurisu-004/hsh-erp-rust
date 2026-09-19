//! process_chain 域业务编排：get / upsert（header + steps 整组替换）
//!
//! 约定：
//! - 事务边界在 handler；service 收 `&mut PgConnection`
//! - upsert 单事务内做：bump chain version（OCC）→ 软删旧 steps → INSERT 新 steps
//! - 部分 UPDATE/INSERT 异常时由 `Transaction::Drop` 自动回滚；service 不显式 rollback
//!
//! 2026-09-16 FK 翻转（migration 026）：
//! - upsert 新增 PENDING 守卫（非 PENDING → 20705）
//! - 无链路径改为 `insert_chain` + `link_chain_to_part`（同事务）

use sqlx::PgConnection;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::part::repo::PartRepo;
use crate::shared::error::{AppError, code};

use crate::modules::process_chain::dto::{
    ProcessChainOut, ProcessChainStepOut, UpsertChainRequest,
};
use crate::modules::process_chain::model::{NewProcessChainStep, TPartProcessChain};
use crate::modules::process_chain::repo::ProcessChainRepo;

pub struct ProcessChainService;

impl ProcessChainService {
    /// 读 part 绑定的工艺链（header + steps）。
    /// 无链 → 20701 `BIZ_PROCESS_CHAIN_NOT_FOUND`（HTTP 404）。
    /// 权限：任意已登录用户可读（车间排产视角：Manager/Clerk/Inspector/CncProgrammer）。
    pub async fn get_by_part(
        conn: &mut PgConnection,
        part_id: i64,
        current: &CurrentUser,
    ) -> Result<ProcessChainOut, AppError> {
        require_any_read(current)?;

        let chain = ProcessChainRepo::get_chain_by_part(&mut *conn, part_id)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_PROCESS_CHAIN_NOT_FOUND,
                    format!("part {part_id} 尚未绑定工艺链"),
                )
            })?;
        let steps = ProcessChainRepo::list_steps_by_chain(&mut *conn, chain.id).await?;
        Ok(chain_to_out(chain, steps))
    }

    /// 按链 id 读工艺链（header + steps）。2026-09-16 FK 翻转新增：
    /// 前端在「工序制定」页点击零件后，按 `part.process_chain_id` 调本端点。
    /// 无链 / 已软删 → 20701 `BIZ_PROCESS_CHAIN_NOT_FOUND`（HTTP 404）。
    /// 权限：与 `get_by_part` 相同。
    pub async fn get_chain_by_id(
        conn: &mut PgConnection,
        chain_id: i64,
        current: &CurrentUser,
    ) -> Result<ProcessChainOut, AppError> {
        require_any_read(current)?;

        let chain = ProcessChainRepo::get_chain_by_id(&mut *conn, chain_id)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_PROCESS_CHAIN_NOT_FOUND,
                    format!("工艺链 {chain_id} 不存在或已删除"),
                )
            })?;
        let steps = ProcessChainRepo::list_steps_by_chain(&mut *conn, chain.id).await?;
        Ok(chain_to_out(chain, steps))
    }

    /// 整组 upsert：
    /// - 无链 → INSERT header + link 到 part + INSERT all steps（单事务）
    /// - 有链 → bump chain version（OCC）→ 软删旧 steps → INSERT 新 steps
    /// - steps 空数组 ⇒ 保留 header 但清空所有 steps
    /// - 守卫（2026-09-16 新增）：part 不存在 → 20101；part.status 非 PENDING → 20705
    ///
    /// 权限：Manager only（写入域统一约定）。
    pub async fn upsert_chain(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        part_id: i64,
        req: &UpsertChainRequest,
        current: &CurrentUser,
    ) -> Result<ProcessChainOut, AppError> {
        current.require_role(Role::Manager)?;

        // 1. 校验入参：steps 内 sort_order 不可重复（DB 唯一索引兜底）；
        //    process_id 都解析为 i64；estimated_minutes ≥ 0。
        let parsed_steps: Vec<NewProcessChainStep> = {
            let mut v = Vec::with_capacity(req.steps.len());
            for s in &req.steps {
                let pid = s.process_id.parse::<i64>().map_err(|_| {
                    AppError::biz(code::BIZ_INVALID_VALUE, "process_id 必须为雪花 ID 字符串")
                })?;
                if s.estimated_minutes < 0 {
                    return Err(AppError::biz(
                        code::BIZ_INVALID_VALUE,
                        "estimated_minutes 必须 ≥ 0",
                    ));
                }
                v.push(NewProcessChainStep {
                    sort_order: s.sort_order,
                    process_id: pid,
                    estimated_minutes: s.estimated_minutes,
                    note: s
                        .note
                        .as_deref()
                        .map(|t| t.trim())
                        .filter(|t| !t.is_empty())
                        .map(str::to_string),
                });
            }
            v
        };
        // sort_order 重复检查（DB 部分索引也会兜底，但前端校验更友好）
        {
            let mut seen = std::collections::HashSet::new();
            for s in &parsed_steps {
                if !seen.insert(s.sort_order) {
                    return Err(AppError::biz(
                        code::BIZ_INVALID_VALUE,
                        format!("sort_order 重复: {}", s.sort_order),
                    ));
                }
            }
        }

        // 2. 加载 part（FK 翻转后 part 是归属关系的载体，必须先校验存在性）
        let part = PartRepo::get_by_id(&mut *conn, part_id, false)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_PART_NOT_FOUND,
                    format!("part {part_id} 不存在或已删除"),
                )
            })?;

        // 3. PENDING 守卫（2026-09-16 新增）：零件一旦下发（离开 PENDING），
        //    工艺链冻结，禁止制定 / 修改。
        if part.status != "PENDING" {
            return Err(AppError::biz(
                code::BIZ_PROCESS_CHAIN_PART_NOT_PENDING,
                format!(
                    "part {part_id} 已下发（status={}），禁止制定/修改工艺链",
                    part.status
                ),
            ));
        }

        // 4. 查现有链
        let existing = ProcessChainRepo::get_chain_by_part(&mut *conn, part_id).await?;

        // 5. 整组事务
        let chain_id: i64 = if let Some(c) = existing {
            // 5a. OCC 自增 version + 更新 name / note
            let new_name: Option<&str> = if req.name.trim().is_empty() {
                None
            } else {
                Some(req.name.trim())
            };
            let note_update: Option<Option<&str>> = match &req.note {
                None => None,
                Some(s) => {
                    let trimmed = s.trim();
                    if trimmed.is_empty() {
                        Some(None)
                    } else {
                        Some(Some(trimmed))
                    }
                }
            };
            let affected = ProcessChainRepo::bump_chain_version(
                &mut *conn,
                c.id,
                c.version,
                new_name,
                note_update,
                current.id,
            )
            .await?;
            if affected == 0 {
                return Err(AppError::biz(
                    code::VERSION_CONFLICT,
                    format!("工艺链 version={} 已被他人修改", c.version),
                ));
            }
            c.id
        } else {
            // 5b. 新建 header + 绑定 part（同事务；2026-09-16 FK 翻转）
            let id = snowflake.next_id();
            let name = if req.name.trim().is_empty() {
                "默认工艺"
            } else {
                req.name.trim()
            };
            ProcessChainRepo::insert_chain(&mut *conn, id, name, req.note.as_deref(), current.id)
                .await?;
            let linked = ProcessChainRepo::link_chain_to_part(&mut *conn, part_id, id, current.id)
                .await
                .map_err(
                    |e| match e.as_database_error().and_then(|d| d.code()).as_deref() {
                        // uq_t_part_process_chain：同一 chain 被并发绑到两个 part
                        Some("23505") => AppError::biz(
                            code::BIZ_INVALID_VALUE,
                            format!("part {part_id} 已存在工艺链（并发）"),
                        ),
                        _ => AppError::from(e),
                    },
                )?;
            if linked == 0 {
                // process_chain_id IS NULL 守卫未命中：并发请求已抢先绑定
                return Err(AppError::biz(
                    code::BIZ_INVALID_VALUE,
                    format!("part {part_id} 已存在工艺链（并发）"),
                ));
            }
            id
        };

        // 6. 软删旧 steps（替换语义）
        ProcessChainRepo::soft_delete_all_steps_for_chain(&mut *conn, chain_id).await?;

        // 7. INSERT 新 steps
        ProcessChainRepo::bulk_insert_steps(
            &mut *conn,
            chain_id,
            &parsed_steps,
            snowflake,
            current.id,
        )
        .await?;

        // 8. 回读 header（version 已 +1）+ steps
        let refreshed = ProcessChainRepo::get_chain_by_part(&mut *conn, part_id)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_PROCESS_CHAIN_NOT_FOUND,
                    "工艺链 upsert 后回读失败",
                )
            })?;
        let steps = ProcessChainRepo::list_steps_by_chain(&mut *conn, chain_id).await?;
        Ok(chain_to_out(refreshed, steps))
    }
}

/// 把 row + steps 装成 DTO（service 单一出口，避免重复）。
fn chain_to_out(
    chain: TPartProcessChain,
    steps: Vec<crate::modules::process_chain::model::TProcessChainStep>,
) -> ProcessChainOut {
    ProcessChainOut {
        id: chain.id,
        name: chain.name,
        note: chain.note,
        version: chain.version,
        created_at: chain.created_at,
        updated_at: chain.updated_at,
        steps: steps
            .into_iter()
            .map(|s| ProcessChainStepOut {
                id: s.id,
                sort_order: s.sort_order,
                process_id: s.process_id,
                estimated_minutes: s.estimated_minutes,
                note: s.note,
                version: s.version,
            })
            .collect(),
    }
}

fn require_any_read(current: &CurrentUser) -> Result<(), AppError> {
    current.require_any_role(&[
        Role::Manager,
        Role::Clerk,
        Role::Inspector,
        Role::CncProgrammer,
    ])
}
