//! 通用分页 Query 参数与响应结构
//!
//! 与 [`crate::shared::response::Page`] 配合使用：
//! ```ignore
//! async fn list(
//!     Query(q): Query<PageQuery>,
//! ) -> Result<Json<R<Page<ItemOut>>>, AppError> {
//!     ...
//!     Ok(Json(R::ok(Page::new(total, items))))
//! }
//! ```

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct PageQuery {
    #[serde(default = "default_page")]
    pub page: i64,
    #[serde(default = "default_size")]
    pub size: i64,
}

fn default_page() -> i64 {
    1
}
fn default_size() -> i64 {
    20
}

impl PageQuery {
    pub fn offset(&self) -> i64 {
        (self.page.max(1) - 1) * self.limit()
    }

    pub fn limit(&self) -> i64 {
        self.size.clamp(1, 200)
    }
}