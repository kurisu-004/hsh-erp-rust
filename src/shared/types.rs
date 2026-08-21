//! 跨域通用 serde helper
//!
//! - `serialize_i64` / `deserialize_i64`：雪花 ID 在 JSON 中序列化为字符串，
//!   避免 JS `Number.MAX_SAFE_INTEGER` 精度截断。DB 层仍保持 `bigint`。
//!
//! DTO 用法：
//! ```ignore
//! #[derive(Serialize)]
//! pub struct PartOut {
//!     #[serde(serialize_with = "crate::shared::types::serialize_i64")]
//!     pub id: i64,
//!     ...
//! }
//! ```

use serde::{Deserialize, Deserializer, Serializer};

pub fn serialize_i64<S: Serializer>(v: &i64, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&v.to_string())
}

pub fn deserialize_i64<'de, D: Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    let s = String::deserialize(d)?;
    s.parse::<i64>().map_err(serde::de::Error::custom)
}

/// 把 JSON 字符串数组反序列化为 `Vec<i64>`（与 `serialize_i64` 对称）
///
/// 用于 DTO 字段如 `member_customer_ids: Vec<i64>`：
/// ```ignore
/// #[derive(Deserialize)]
/// pub struct CreateGroupReq {
///     #[serde(deserialize_with = "crate::shared::types::deserialize_i64_vec")]
///     pub member_customer_ids: Vec<i64>,
/// }
/// ```
///
/// 空数组 → 空 Vec；含非法字符串 → `serde::de::Error::custom`。
pub fn deserialize_i64_vec<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<i64>, D::Error> {
    let v: Vec<String> = Vec::deserialize(d)?;
    v.into_iter()
        .map(|s| s.parse::<i64>().map_err(serde::de::Error::custom))
        .collect()
}