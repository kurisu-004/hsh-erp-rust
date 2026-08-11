//! bcrypt 密码散列/校验封装
//!
// 对应 Python myERP/core/security.py 的 bcrypt 直接 hash/verify（不用 passlib）。

use crate::shared::error::{code, AppError};

pub fn hash(password: &str) -> Result<String, AppError> {
    bcrypt::hash(password, bcrypt::DEFAULT_COST)
        .map_err(|e| AppError::biz(code::INTERNAL, format!("bcrypt hash: {e}")))
}

pub fn verify(password: &str, hashed: &str) -> Result<bool, AppError> {
    bcrypt::verify(password, hashed)
        .map_err(|e| AppError::biz(code::INTERNAL, format!("bcrypt verify: {e}")))
}