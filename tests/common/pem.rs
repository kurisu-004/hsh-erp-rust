//! 2026-09-23 重构：测试用 RSA 密钥对（PEM 格式）+ kid 字典
//!
//! JWT 切 RS256 后，集成测试需在每进程内生一对 RSA 私钥 + 公钥：
//! - `test_private_pem()` 给 `tests/common/mod.rs` 的 JwtConfig::private_key 字段
//!   与 auth_middleware.rs 的 `mint_*_token` helper 签 RS256 token
//! - `test_public_pem(kid)` 给 JwtConfig::public_keys 字典 + 测试用例验签
//! - `test_public_kids()` 公开至少 `("current", pub_pem)` 一对，可扩展
//!   `("next", ...)` 用于密钥轮换场景
//!
//! 2048-bit RSA keypair 用 `rsa` crate (`RsaPrivateKey::new(&mut OsRng, 2048)`)
//! 生成后转 PKCS#8 PEM（私钥）+ SPKI PEM（公钥），与 jsonwebtoken 的
//! `EncodingKey::from_rsa_pem` / `DecodingKey::from_rsa_pem` 配套。
//!
//! 缓存策略：进程级 `OnceLock<[String; 3]>` —— 私钥 PEM、公钥 PEM、kid 列表。
//! 每进程首调用时生成一次，后续调用零成本。**不**写入磁盘（避免密钥泄露到
//! git 跟踪的 fixture 文件）。
//!
//! 纯 CPU 操作：不引入 tokio / sqlx。

use std::sync::OnceLock;

use rsa::pkcs8::{EncodePrivateKey, EncodePublicKey, LineEnding};
use rsa::rand_core::OsRng;
use rsa::traits::PublicKeyParts;
use rsa::{RsaPrivateKey, RsaPublicKey};

/// 2048-bit RSA —— 与生产对齐（OpenSSL 默认 key size）。
const KEY_BITS: usize = 2048;

/// 默认 kid 名（与 `.env` 中 `JWT_SIGNING_KID=current` 对齐）。
pub const DEFAULT_KID: &str = "current";

/// 进程级缓存：`[private_pem, current_pub_pem, next_pub_pem]`
///
/// 每次签发走同一私钥（保证 `signing_kid` 总是 `current`），公钥有两份
/// 模拟「轮换中」场景——`current` 是签发端 kid，`next` 是已部署但尚未签发的
/// 公钥，供未来密钥轮换集成测试扩展。
#[derive(Clone)]
struct KeyMaterial {
    private_pem: String,
    /// (kid, public_pem) 列表；按 kid 排序便于稳定断言。
    pairs: Vec<(String, String)>,
}

static KEYS: OnceLock<KeyMaterial> = OnceLock::new();

fn keys() -> &'static KeyMaterial {
    KEYS.get_or_init(generate_key_material)
}

fn generate_key_material() -> KeyMaterial {
    let mut rng = OsRng;
    // 2048-bit RSA 私钥
    let private_key = RsaPrivateKey::new(&mut rng, KEY_BITS).expect("generate 2048-bit RSA private key");
    let public_key = RsaPublicKey::from(&private_key);

    // 私钥：PKCS#8 PEM（PKCS#1 也可，但 PKCS#8 是 jsonwebtoken 的 from_rsa_pem 通用输入）
    let private_pem = private_key
        .to_pkcs8_pem(LineEnding::LF)
        .expect("encode private key to PKCS#8 PEM")
        .to_string();

    // 公钥 SPKI PEM（两枚：current + next 模拟轮换场景）
    let current_pub_pem = public_key
        .to_public_key_pem(LineEnding::LF)
        .expect("encode current public key to SPKI PEM");
    // `next` 单独生成 —— 模拟"新一对密钥"；公钥入字典但私钥不使用。
    let next_priv = RsaPrivateKey::new(&mut rng, KEY_BITS).expect("generate next RSA private key");
    let next_pub = RsaPublicKey::from(&next_priv);
    let next_pub_pem = next_pub
        .to_public_key_pem(LineEnding::LF)
        .expect("encode next public key to SPKI PEM");

    let pairs = vec![
        (DEFAULT_KID.to_string(), current_pub_pem.to_string()),
        ("next".to_string(), next_pub_pem.to_string()),
    ];

    // sanity：key size 必须是 2048-bit（防某天有人误把 KEY_BITS 改小）
    debug_assert_eq!(
        public_key.size() * 8,
        KEY_BITS,
        "RSA public key size 应与 KEY_BITS 一致"
    );

    KeyMaterial {
        private_pem,
        pairs,
    }
}

/// 测试用 RSA 私钥 PEM（PKCS#8）。
///
/// 每进程首次调用时生成一次，后续零成本返回同一份字符串。
/// 用于 `JwtConfig::private_key` 与 `mint_*_token` helper 签 RS256。
pub fn test_private_pem() -> &'static str {
    &keys().private_pem
}

/// 测试用 RSA 公钥 PEM（SPKI），按 kid 索引。
///
/// `kid` 必须在 [`test_public_kids`] 列表内（当前固定 `current` / `next`）。
/// 未在表内 → 抛 `None`，caller 应 `expect` 让测试失败信号清晰。
pub fn test_public_pem(kid: &str) -> Option<&'static str> {
    keys()
        .pairs
        .iter()
        .find(|(k, _)| k == kid)
        .map(|(_, pem)| pem.as_str())
}

/// 测试用 (kid, public_pem) 对列表。
///
/// 当前固定返回 `("current", ...)` + `("next", ...)` 两对：
/// - `current` 是签发端 kid，与 `.env` 的 `JWT_SIGNING_KID` 对齐
/// - `next` 是模拟「密钥轮换中」的已部署公钥，供未来轮换测试扩展
///
/// 注意：返回的字符串都是 `&'static` 借用（数据存进 `OnceLock`），
/// caller 不应持有跨进程边界的引用（理论上不存在）。
pub fn test_public_kids() -> &'static [(&'static str, &'static str)] {
    // OnceLock 内的 String 转 &'static str：靠 unsafe 一次性转；这是 OnceLock
    // 内部存 'static 数据 + 不会变更的标准模式。
    static PAIRS: OnceLock<Vec<(&'static str, &'static str)>> = OnceLock::new();
    PAIRS.get_or_init(|| {
        let k = keys();
        // 收集成 (kid, pub_pem) 列表；pub_pem 是 'static 借用（OnceLock 持有 String → &str）。
        let mut v: Vec<(&'static str, &'static str)> = Vec::with_capacity(k.pairs.len());
        for (kid, pem) in &k.pairs {
            // 借用 k.pairs 里的 String 为 'static str —— 安全因为 OnceLock 不会释放
            let kid_static: &'static str = leak_static(kid.clone());
            let pem_static: &'static str = leak_static(pem.clone());
            v.push((kid_static, pem_static));
        }
        v
    })
}

/// 把 `String` 转 `&'static str`：仅在 `OnceLock::get_or_init` 闭包里用，
/// 内存永久驻留（与进程同寿命），适合测试 fixture 的「永远不释放」语义。
fn leak_static(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_material_is_stable_across_calls() {
        let p1 = test_private_pem();
        let p2 = test_private_pem();
        // 同一进程内多次调用必须返回同一引用（OnceLock 缓存命中）。
        assert_eq!(p1.as_ptr(), p2.as_ptr(), "private pem 引用应一致");
    }

    #[test]
    fn kids_list_is_not_empty() {
        let kids = test_public_kids();
        assert!(!kids.is_empty(), "kid 列表至少 1 项");
        // current 必须存在（与 JwtConfig::signing_kid 对齐）。
        assert!(
            kids.iter().any(|(k, _)| *k == DEFAULT_KID),
            "current kid 必须存在"
        );
    }

    #[test]
    fn public_pem_lookup_works() {
        let pem = test_public_pem(DEFAULT_KID).expect("current kid 应在表内");
        assert!(pem.contains("BEGIN PUBLIC KEY"), "应为 SPKI PEM");
    }

    /// 关键 roundtrip：拿 test_private_pem 签一个 RS256 token，再用 current kid
    /// 的公钥解码；签名验证必须通过。这是测试 helper 的最小不变量。
    ///
    /// ⚠️ 早期 bug：tests/auth_middleware.rs 一度用 `#[path = "common/pem.rs"]
    /// mod pem;` 引入第二个 pem 实例，与 tests/common/mod.rs 的 `mod pem;`
    /// 各自一套 OnceLock → 签发 / 验签走两套不同 keypair → 40100 InvalidSignature。
    /// 本 roundtrip 用 top-level `pem::*` + 同模块 PEM，与 test_state 走的
    /// `common::pem::*` 必须共享同一 OnceLock（`tests/common/mod.rs` 必须 `pub
    /// mod pem` 让所有 test binary 顶层能 `use common::pem;`）。
    #[test]
    fn roundtrip_sign_and_verify() {
        use chrono::Utc;
        use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode};
        let now = Utc::now().timestamp();
        #[derive(serde::Serialize, serde::Deserialize)]
        struct Claims {
            sub: i64,
            exp: i64,
            iat: i64,
        }
        let claims = Claims {
            sub: 42,
            exp: now + 3600,
            iat: now,
        };
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(DEFAULT_KID.to_string());
        let enc = EncodingKey::from_rsa_pem(test_private_pem().as_bytes()).expect("enc");
        let token = encode(&header, &claims, &enc).expect("encode");
        let dec = DecodingKey::from_rsa_pem(test_public_pem(DEFAULT_KID).unwrap().as_bytes())
            .expect("dec");
        let mut v = Validation::new(Algorithm::RS256);
        v.algorithms = vec![Algorithm::RS256];
        v.set_required_spec_claims(&["exp"]);
        let parsed = decode::<Claims>(&token, &dec, &v).expect("decode must succeed");
        assert_eq!(parsed.claims.sub, 42, "sub must match");
    }
}