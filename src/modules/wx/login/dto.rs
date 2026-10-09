//! wx::login 子模块入参 DTO 层（仅 `Deserialize`）
//!
//! 2026-10-11 自旧 `src/modules/wx/dto.rs` 平铺实现迁入（该文件随本次重构整体
//! 删除，其唯一类型 `WxLoginRequest` 只服务于 wx-login 端点）。
//!
//! ## 边界（与 iam 域同形）
//! - `dto.rs`：**仅** axum extractor 反序列化目标（`Deserialize`）
//! - `vo.rs`：**仅** handler 返回值（`Serialize`）
//!
//! ## 与 iam `dto/account.rs` 的不同
//! 本模块的校验**不**放在 `#[serde(deserialize_with)]` 里，而是由
//! [`WxLoginRequest::validate`] 在 service 层显式做 —— 理由见该方法注释。

use serde::Deserialize;

use crate::shared::error::AppError;

/// 小程序 `wx.login()` 返回的 code 长度上限（官方文档：code 最大 512 字节）。
///
/// 提前拒绝超长入参，避免把一个必然会被企微拒绝的请求打到上游。
const CODE_MAX_LEN: usize = 512;

/// `POST /api/v2/wx/login/wecom` 入参
#[derive(Debug, Clone, Deserialize)]
pub struct WxLoginRequest {
    /// 小程序 `wx.login()` 返回的临时凭证：**一次性、5 分钟过期**
    pub code: String,
}

impl WxLoginRequest {
    /// 归一化 + 校验：trim 后非空、长度 ≤ 512 字节。
    ///
    /// ## 校验为何不在 `Deserialize` 里做
    /// iam 域 DTO 的长度约束交给 DB / service，本模块的 code 是**纯透传给企微**的
    /// 一次性凭证：它不做任何 DB 落库，也没有 schema 约束可依赖。因此在
    /// service 入口显式校验并给中文错误文案（与 `AppError::validation` 语义对齐，
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

#[cfg(test)]
mod tests {
    use super::*;

    fn req(code: &str) -> WxLoginRequest {
        WxLoginRequest {
            code: code.to_string(),
        }
    }

    #[test]
    fn validate_trims_and_accepts() {
        assert_eq!(req("  abc  ").validate().unwrap(), "abc");
    }

    #[test]
    fn validate_rejects_blank() {
        assert!(req("   ").validate().is_err());
        assert!(req("").validate().is_err());
    }

    #[test]
    fn validate_rejects_over_512_bytes() {
        let long = "c".repeat(CODE_MAX_LEN + 1);
        assert!(req(&long).validate().is_err());
        let exact = "c".repeat(CODE_MAX_LEN);
        assert_eq!(req(&exact).validate().unwrap().len(), CODE_MAX_LEN);
    }
}
