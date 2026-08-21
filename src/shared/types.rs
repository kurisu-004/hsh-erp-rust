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

/// `Option<i64>` 序列化为 JSON 字符串；None 序列化为 `null`（除非 caller
/// 加 `skip_serializing_if = "Option::is_none"`）。
pub fn serialize_i64_opt<S: Serializer>(
    v: &Option<i64>,
    s: S,
) -> Result<S::Ok, S::Error> {
    match v {
        Some(n) => s.serialize_str(&n.to_string()),
        None => s.serialize_none(),
    }
}

pub fn deserialize_i64<'de, D: Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    let s = String::deserialize(d)?;
    s.parse::<i64>().map_err(serde::de::Error::custom)
}

/// `Option<i64>` 反序列化：None 表示字段缺省；Some(str) → parse 为 i64。
///
/// 用于 query string / body 里可选的雪花 id 字段（`customer_id=L1` 时 str，
/// 没传则 None）。常配 `#[serde(default)]` 兜底。
pub fn deserialize_i64_opt<'de, D: Deserializer<'de>>(
    d: D,
) -> Result<Option<i64>, D::Error> {
    let opt: Option<String> = Option::deserialize(d)?;
    match opt {
        None => Ok(None),
        Some(s) => s.parse::<i64>().map(Some).map_err(serde::de::Error::custom),
    }
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

/// `Option<Vec<i64>>` 版本：None 表示字段缺省（`#[serde(default)]` 也会兜底）；
/// Some(vec) 走 `deserialize_i64_vec` 同样的逐元素字符串→i64 解析。
///
/// 用于 update 请求里可选的全量替换字段（`UpdateDeliveryGroupRequest`）。
pub fn deserialize_i64_vec_opt<'de, D: Deserializer<'de>>(
    d: D,
) -> Result<Option<Vec<i64>>, D::Error> {
    // 直接反序列化 Option<Vec<String>>，再 map parse
    let opt: Option<Vec<String>> = Option::deserialize(d)?;
    match opt {
        None => Ok(None),
        Some(v) => v
            .into_iter()
            .map(|s| s.parse::<i64>().map_err(serde::de::Error::custom))
            .collect::<Result<Vec<i64>, _>>()
            .map(Some),
    }
}