//! files 域 HTTP handler（路由表）
//!
//! ## 路由表（2026-10-15 起本文件是 files 域的**唯一**路由声明处）
//!
//! ```text
//! /objects/*      → part_file::router()       （嵌套子模块 part_file）
//! ```
//!
//! 完整路径（叠加 `modules::v2_router()` 的 `.nest("/files", …)`）：
//! `/api/v2/files/objects/*`。
//!
//! ⚠️ **为什么 nest 段写 `/objects` 而不是模块名 `part_file`**：URL 对外语义是
//! 「文件对象」而非「零件文件」—— `t_part_file` 的 owner 本来就是多态的
//! （`owner_kind ∈ {PART, ASSEMBLY, CUSTOMER}`），装配件文件与客户送货单打印模板
//! 也走这里。模块归 `part_file`（它 own 这张表）、URL 说对象，两者有意不同。
//!
//! ⚠️ **matchit 注册顺序**：本文件只剩一个 `.nest`，无静态段 / 捕获段之争。

use std::sync::Arc;

use axum::Router;

use crate::modules::files::part_file;
use crate::state::AppState;

/// files 域路由工厂。
///
/// 本文件**不再持有任何端点**：`POST /sts-tmp-keys`（2026-09-28 加的 python 薄壳
/// 鉴权转发）随上传链路统一一并下线 —— 预签 PUT 由
/// `POST /objects/upload-intents` 在 files 域内部直接签发，不再向 python 取凭证。
/// 本文件保留为**路由表所在**（CLAUDE.md 规约：路由表在 `handler.rs`，`mod.rs`
/// 只放 `pub fn` 转发），权威路由清单归 [`part_file::handler::ROUTES`]。
///
/// ⚠️ python 侧同名端点 `POST /api/v1/files/sts-tmp-keys` 随之失去唯一调用方，
/// 待后续在 backend-python 仓清理（本次不跨仓改动）。
pub fn router() -> Router<Arc<AppState>> {
    Router::new().nest("/objects", part_file::router())
}

/// 本文件 `router()` 注册的全部 `METHOD /path`（相对 `/api/v2/files`）。
///
/// ⚠️ **恒为空**：`.nest(...)` 不计入本表（子模块的路由清单归子模块自己的
/// `ROUTES`，在这里再记一遍就是重复计数）。保留本常量 + 同文件的守卫单测，是为了
/// 「本文件将来有人加了 `.route(...)` 却没登记」当场红；今天它断言的是「本域容器
/// 层没有自有端点」。
///
/// ⚠️ 只被同文件的 `mod tests` 消费 —— `ROUTES` 是**测试期登记表**，不是运行期读的
/// 配置。而 `mod handler` 本身私有 ⇒ 非测试构建里本常量不可达，`dead_code` 会误报。
/// 故只在 `not(test)` 下放行（同域另两张表挂在 `pub mod` 下，天然可达，无需此豁免）。
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const ROUTES: &[&str] = &[];

#[cfg(test)]
mod tests {
    use super::ROUTES;
    use crate::modules::files::route_guard::routes_in_router_source;

    /// `ROUTES` 必须与 `router()` 源码**逐条**一致（解析规则见 `route_guard`）。
    ///
    /// ⚠️ 本表恒空**不是**「漏登记」：解析器不数 `.nest(...)`，而本文件 2026-10-15
    /// 起确实没有自有 `.route(...)`（`POST /sts-tmp-keys` 下线）。
    #[test]
    fn routes_declared_in_router() {
        let actual = routes_in_router_source(include_str!("handler.rs"), "router");
        assert_eq!(
            actual.len(),
            ROUTES.len(),
            "`router()` 注册了 {} 条自有路由，ROUTES 只列了 {} 条（新增端点忘了登记？）\n\
             router(): {:?}\nROUTES: {:?}",
            actual.len(),
            ROUTES.len(),
            actual,
            ROUTES,
        );
        for (got, want) in actual.iter().zip(ROUTES.iter()) {
            assert_eq!(
                got,
                want,
                "`router()` 与 ROUTES 的第 {} 条不一致",
                ROUTES.len()
            );
        }
    }
}
