//! 统一响应信封 R<T> + 通用分页结构
//!
// 对应 Python myERP/core/response.py 的 `R(BaseModel, Generic[T])`。
// 业务 handler 统一返回 `Result<Json<R<T>>, AppError>`。

use serde::Serialize;

use crate::shared::error::code;

#[derive(Serialize)]
pub struct R<T> {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<T>,
}

impl<T> R<T> {
    pub fn ok(data: T) -> Self {
        Self {
            code: code::SUCCESS,
            message: "ok".into(),
            data: Some(data),
        }
    }

    pub fn err(code: i32, message: impl Into<String>) -> R<()> {
        R {
            code,
            message: message.into(),
            data: None,
        }
    }
}

impl R<()> {
    pub fn ok_empty() -> Self {
        Self {
            code: code::SUCCESS,
            message: "ok".into(),
            data: None,
        }
    }
}

#[derive(Serialize)]
pub struct Page<T> {
    pub total: i64,
    pub items: Vec<T>,
}

impl<T> Page<T> {
    pub fn new(total: i64, items: Vec<T>) -> Self {
        Self { total, items }
    }
}