//! iam 域（认证 + 账号合并 + 货架实体管理）
//!
//! ## 为什么货架归 iam（2026-10-10）
//! 用户视角里，账号与货架是同一类东西 —— 「**谁能碰什么**」的权限资源：
//! - `t_user_role` 里 `SHELF_ACCOUNT` 角色的 `scope_id` **指向某个货架**：货架实体
//!   是这套权限体系的落点，不是独立业务对象。归属 iam 后「这个 scope 指向的货架还在
//!   不在」由本域 SQL 直接读 `t_shelf` 回答，不必跨域问；
//! - 5 条货架写端点全部 MANAGER 独占，与账号 / 角色管理是同一批授权动作。
//!
//! 货架实体是 iam 域的**嵌套子模块** `iam::shelf`（`src/modules/iam/shelf/`，5 端点，
//! URL `/api/v2/iam/shelves/*`），不是摊平进本域的扁平目录 —— 扁平目录里已经全是
//! 账号 / 会话文件，货架 CRUD 混进去会失焦。归属缘由与迁移记录见
//! [`shelf::mod`] 的模块 doc。
//!
//! **不归 iam 的两样东西**：工序映射 `prod::shelf_process`（`t_shelf_process` 的
//! `process_id` 指向 prod 域实体 `t_process`）与选架设施 `shared::shelf`（跨域设施层、
//! 无域归属）。
//!
//! 目录形态：`dto/` `handler/` `repo/` `service/` `vo/` 是账号与会话的扁平结构，
//! `service/account/` 是子目录先例；本域端点全挂在 `/api/v2/iam` 下。
pub mod dto;
pub mod handler;
pub mod repo;
pub mod service;
pub mod shelf;
pub mod vo;

use std::sync::Arc;

use axum::Router;

use crate::state::AppState;

pub fn router() -> Router<Arc<AppState>> {
    handler::router()
}

#[cfg(test)]
mod tests {
    //! 域隔离护栏：把「iam 域不依赖其它域」从口头约定变成 CI 强制。
    //!
    //! 探测器实现（剥注释、根段 + 域路径前缀匹配、元测试）见
    //! [`crate::shared::domain_guard`]，本域只负责传参 + 域专属指引。

    use std::path::Path;

    use crate::shared::domain_guard::assert_no_foreign_domain;

    /// iam 域只允许 `crate::` 下的 `auth` / `infra` / `shared` / `state` 与本域自身
    /// （含嵌套子模块 `iam::shelf`）；代码区里出现任何其它域的路径即失败。
    ///
    /// 这条护栏对本域是**结构性成立**的，不是「暂时没依赖」：iam 管的表是
    /// `t_user` / `t_user_role` / `t_menu` / `t_wx_identity` / `t_shelf`，全部自有；
    /// 唯一的跨域诉求是校验 `scope_id` 指向的货架，而货架已于 2026-10-10 收进本域
    /// （`IamRepoTrait::get_shelf_by_id` 委托 `iam::shelf::repo::ShelfRepo::get_by_id`）。
    /// 换句话说 iam 是本仓少有的「**纯自有表域**」—— 不像 `dashboard` / `statistics` /
    /// `admin` 那样要在本域 SQL 里跨域只读聚合，所以不需要为护栏写「正确做法」的
    /// 取舍说明，只要守住「不许 import 别人的 service / repo」这一条。
    ///
    /// ⚠️ 若哪天本域真的需要别的域的数据，**先改这里**而不是删掉这条护栏：要么按
    /// `statistics` / `admin` 的 pattern 在本域 SQL 里只读聚合（护栏仍然成立），
    /// 要么把本域降级为与 `prod::queue` 同形的「经本域 trait 转发他域单表查询」的
    /// 域并显式登记例外。
    ///
    /// ### 2026-10-11 补充：`t_user.worker_id` 跨域指向 `t_worker`，护栏仍成立
    /// 上文「全部自有」指的是**表**归属，不是列的指向。`t_user` 新增的
    /// `worker_id` 列指向 `prod::worker` 域的 `t_worker.id`，但不构成护栏违规：
    /// - 本域只把 `worker_id` 当 `User` 行的**自有属性**存读，**不在本域 SQL 里
    ///   JOIN `t_worker`**（`repo/sql/user.rs` 三条 `query_as!` 已复核，无跨域 JOIN）；
    /// - 护栏探测的是**代码区里的跨域 Rust 路径**（`crate::modules::<他域>::…`），
    ///   SQL 文本里的表名不在探测口径内，故新增一个列名不会触发它。
    ///
    /// ⚠️ 若将来要在 iam 侧**校验该工人是否存在 / 未软删**，那才是真跨域读，正确
    /// 做法有两条（都要按本节开头的老规矩先改这里的 doc）：
    ///   1. 按 `statistics` / `admin` 的 pattern，在本域 SQL 里只读聚合 `t_worker`
    ///      做存在性判定（护栏仍然成立）；或
    ///   2. 走 `wx` BFF 层（`modules/wx/`，B2 起的重构目标）在本域之外解析绑定，
    ///      iam 只负责存 `t_user.worker_id` 这个值本身。
    #[test]
    fn iam_domain_depends_on_no_other_domain() {
        assert_no_foreign_domain(
            "iam",
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("src/modules/iam"),
            "iam 域管的表（t_user / t_user_role / t_menu / t_wx_identity / t_shelf）全部自有，\
             不需要读别的域；真的要跨域只读时，正确做法是像 statistics / admin 那样在本域 \
             SQL 里只读聚合，而不是 import 别人的 service / repo。",
        );
    }
}
