//! 微信小程序 BFF 模块入参 DTO（2026-09-29 新增）
//!
//! 本模块此前只有响应侧 `vo.rs`（无 `dto.rs`）。企业微信 wx-login 端点引入
//! JSON 请求体，故新建本文件放请求侧类型。
//!
//! ## 边界（与 iam 域同形）
//! - `dto.rs`：**仅** axum extractor 反序列化目标（`Deserialize`）
//! - `vo.rs`：**仅** handler 返回值（`Serialize`）
//!
//! 与 iam `dto/account.rs` 不同的是，本模块的校验**不**放在 `#[serde(deserialize_with)]`
//! 里，而是放在 service / handler 层显式 `AppError::validation`——原因见
//! `WxLoginRequest::validate` 的注释。

use serde::Deserialize;

use crate::shared::error::AppError;

/// 小程序 `wx.login()` 返回的 code 长度上限（官方文档：code 最大 512 字节）。
///
/// 提前拒绝超长入参，避免把一个必然会被企微拒绝的请求打到上游。
const CODE_MAX_LEN: usize = 512;

/// `POST /api/v2/wx/iam/wx-login` 入参
#[derive(Debug, Clone, Deserialize)]
pub struct WxLoginRequest {
    /// 小程序 `wx.login()` 返回的临时凭证：**一次性、5 分钟过期**
    pub code: String,
}

impl WxLoginRequest {
    /// 归一化 + 校验：trim 后非空、长度 ≤ 512 字节。
    ///
    /// ## 校验为何不在 `Deserialize` 里做
    /// iam 域 DTO 的长度约束交给 DB / service，本域 code 是**纯透传给企微**的
    /// 一次性凭证：它不做任何 DB 落库，也没有 schema 约束可依赖。因此在
    /// handler 入口显式校验并给中文错误文案（与 `AppError::validation` 语义对齐，
    /// 走 40001 / HTTP 422），比让企微返一个不可归因的 40029 更有排查价值。
    ///
    /// 返回 trim 后的 code（企微对空白敏感度未知，统一去掉首尾空白更稳）。
    pub fn validate(&self) -> Result<String, AppError> {
        let code = self.code.trim();
        if code.is_empty() {
            return Err(AppError::validation("code 不能为空"));
        }
        if code.len() > CODE_MAX_LEN {
            return Err(AppError::validation(format!(
                "code 长度超限（> {CODE_MAX_LEN} 字节）"
            )));
        }
        Ok(code.to_string())
    }
}
