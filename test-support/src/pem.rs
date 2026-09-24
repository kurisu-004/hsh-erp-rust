//! 测试用 RSA 密钥对（PEM 格式）+ kid 字典
//!
//! 2026-09-23 重构：JWT 切 RS256 后，集成测试需在每进程内生一对 RSA 私钥 + 公钥：
//! - `test_private_pem()` 给 `tests/common/mod.rs`（现 `crate::state`）的
//!   JwtConfig::private_key 字段与 auth_middleware.rs 的 `mint_*_token` helper
//!   签 RS256 token
//! - `test_public_pem(kid)` 给 JwtConfig::public_keys 字典 + 测试用例验签
//! - `test_public_kids()` 公开至少 `("current", pub_pem)` 一对，可扩展
//!   `("next", ...)` 用于密钥轮换场景
//!
//! 2048-bit RSA keypair 用 `rsa` crate (`RsaPrivateKey::new(&mut OsRng, 2048)`)
//! 生成后转 PKCS#8 PEM（私钥）+ SPKI PEM（公钥），与 jsonwebtoken 的
//! `EncodingKey::from_rsa_pem` / `DecodingKey::from_rsa_pem` 配套。
//!
//! 2026-09-24 E1 改造：
//! - **current / next 分两个独立 OnceLock**：绝大多数测试只用 current，next 独立
//!   lazy（只在 `test_public_kids()` 或显式查 `("next", ...)` 时才生成）；旧实现
//!   每次首调用同时生成 current + next 两对 ~2048-bit RSA。
//! - **env 短路**：`JWT_TEST_PRIVATE_PEM_PATH`（私钥 PKCS#8 PEM 文件路径） +
//!   `JWT_TEST_PUBLIC_PEMS_DIR`（公钥 SPKI PEM 目录，kid = 文件名）都设了的话
//!   直接读盘，不再走 OsRng。env 缺失 → 静默回退 OsRng（兼容原 unit test）；
//!   env 设了但文件读不到 → **panic**（不静默回退，否则 session 注入失效却仍
//!   能跑，调试灾难）。
//!
//! 缓存策略：进程级 `OnceLock<KeyMaterial>` —— 私钥 PEM + 单 (kid, 公钥 PEM) 对。
//! 每进程首调用时生成一次，后续调用零成本。**不**写入磁盘（避免密钥泄露到
//! git 跟踪的 fixture 文件）。
//!
//! 纯 CPU 操作：不引入 tokio / sqlx。

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use rsa::pkcs8::{EncodePrivateKey, EncodePublicKey, LineEnding};
use rsa::rand_core::OsRng;
use rsa::traits::PublicKeyParts;
use rsa::{RsaPrivateKey, RsaPublicKey};

/// 2048-bit RSA —— 与生产对齐（OpenSSL 默认 key size）。
const KEY_BITS: usize = 2048;

/// 默认 kid 名（与 `.env` 中 `JWT_SIGNING_KID=current` 对齐）。
pub const DEFAULT_KID: &str = "current";

/// env 短路用的私钥 PEM 文件路径；wrapper (`test_nextest.sh`) 会 export。
const ENV_PRIVATE_PEM_PATH: &str = "JWT_TEST_PRIVATE_PEM_PATH";
/// env 短路用的公钥 PEM 目录；wrapper 会 export。
const ENV_PUBLIC_PEMS_DIR: &str = "JWT_TEST_PUBLIC_PEMS_DIR";

/// 进程级缓存：current 私钥 PEM + current 公钥 PEM 一对。
#[derive(Clone)]
struct KeyMaterial {
    private_pem: String,
    /// (kid, public_pem) 列表；通常 1 项。current KeyMaterial 只装 current；
    /// next KeyMaterial 只装 next —— 由两个独立 OnceLock 拆分管理。
    pairs: Vec<(String, String)>,
}

/// current 私钥 + current 公钥（绝大多数测试只触发这个）。
static KEYS_CURRENT: OnceLock<KeyMaterial> = OnceLock::new();
/// next 公钥（独立 lazy，只在 `test_public_kids()` / `test_public_pem("next")`
/// 首次调用时生成）。
static KEYS_NEXT: OnceLock<KeyMaterial> = OnceLock::new();

fn keys_current() -> &'static KeyMaterial {
    KEYS_CURRENT.get_or_init(load_or_generate_current)
}

fn keys_next() -> &'static KeyMaterial {
    KEYS_NEXT.get_or_init(load_or_generate_next)
}

fn load_or_generate_current() -> KeyMaterial {
    match (
        std::env::var_os(ENV_PRIVATE_PEM_PATH),
        std::env::var_os(ENV_PUBLIC_PEMS_DIR),
    ) {
        (Some(p), Some(d)) => load_current_from_env(PathBuf::from(p), PathBuf::from(d)),
        _ => generate_current_osrng(),
    }
}

fn load_or_generate_next() -> KeyMaterial {
    // next 仅需公钥；只要 PUBLIC_PEMS_DIR 设了就走 env（要求 PRIV 也设以保持
    // 对称），否则 OsRng —— 与 current 同样的"双 env 都设才走 env"规则。
    match (
        std::env::var_os(ENV_PRIVATE_PEM_PATH),
        std::env::var_os(ENV_PUBLIC_PEMS_DIR),
    ) {
        (Some(_), Some(d)) => load_next_from_env(PathBuf::from(d)),
        _ => generate_next_osrng(),
    }
}

fn load_current_from_env(priv_path: PathBuf, pub_dir: PathBuf) -> KeyMaterial {
    let private_pem = std::fs::read_to_string(&priv_path).unwrap_or_else(|e| {
        panic!(
            "{}={:?} unreadable: {} (env 已设但读盘失败 → fast-fail，不静默回退 OsRng)",
            ENV_PRIVATE_PEM_PATH, priv_path, e
        )
    });
    let current_pub_pem = read_pub_from_dir(&pub_dir, DEFAULT_KID).unwrap_or_else(|| {
        panic!(
            "{}={:?} missing '{}.pem' (env 已设但文件缺失 → fast-fail)",
            ENV_PUBLIC_PEMS_DIR, pub_dir, DEFAULT_KID
        )
    });
    KeyMaterial {
        private_pem,
        pairs: vec![(DEFAULT_KID.to_string(), current_pub_pem)],
    }
}

fn load_next_from_env(pub_dir: PathBuf) -> KeyMaterial {
    let next_pub_pem = read_pub_from_dir(&pub_dir, "next").unwrap_or_else(|| {
        panic!(
            "{}={:?} missing 'next.pem' (env 已设但文件缺失 → fast-fail)",
            ENV_PUBLIC_PEMS_DIR, pub_dir
        )
    });
    KeyMaterial {
        private_pem: String::new(),
        pairs: vec![("next".to_string(), next_pub_pem)],
    }
}

/// 从目录读 `kid.pem` 或 `kid`（任一存在即可）。kid 不在目录里 → None，caller
/// 决定 fast-fail 还是 fallback。
fn read_pub_from_dir(dir: &Path, kid: &str) -> Option<String> {
    let with_ext = dir.join(format!("{}.pem", kid));
    if let Ok(s) = std::fs::read_to_string(&with_ext) {
        return Some(s);
    }
    let plain = dir.join(kid);
    if let Ok(s) = std::fs::read_to_string(&plain) {
        return Some(s);
    }
    None
}

fn generate_current_osrng() -> KeyMaterial {
    let mut rng = OsRng;
    // 2048-bit RSA 私钥
    let private_key =
        RsaPrivateKey::new(&mut rng, KEY_BITS).expect("generate 2048-bit RSA private key");
    let public_key = RsaPublicKey::from(&private_key);

    // 私钥：PKCS#8 PEM（PKCS#1 也可，但 PKCS#8 是 jsonwebtoken 的 from_rsa_pem 通用输入）
    let private_pem = private_key
        .to_pkcs8_pem(LineEnding::LF)
        .expect("encode private key to PKCS#8 PEM")
        .to_string();

    // 公钥 SPKI PEM（仅 current；next 走独立 OnceLock）
    let current_pub_pem = public_key
        .to_public_key_pem(LineEnding::LF)
        .expect("encode current public key to SPKI PEM");

    // sanity：key size 必须是 2048-bit（防某天有人误把 KEY_BITS 改小）
    debug_assert_eq!(
        public_key.size() * 8,
        KEY_BITS,
        "RSA public key size 应与 KEY_BITS 一致"
    );

    KeyMaterial {
        private_pem,
        pairs: vec![(DEFAULT_KID.to_string(), current_pub_pem.to_string())],
    }
}

fn generate_next_osrng() -> KeyMaterial {
    // `next` 单独生成 —— 模拟"新一对密钥"；公钥入字典但私钥不使用。
    let mut rng = OsRng;
    let next_priv = RsaPrivateKey::new(&mut rng, KEY_BITS).expect("generate next RSA private key");
    let next_pub = RsaPublicKey::from(&next_priv);
    let next_pub_pem = next_pub
        .to_public_key_pem(LineEnding::LF)
        .expect("encode next public key to SPKI PEM");

    KeyMaterial {
        private_pem: String::new(),
        pairs: vec![("next".to_string(), next_pub_pem.to_string())],
    }
}

/// 测试用 RSA 私钥 PEM（PKCS#8）。
///
/// 每进程首次调用时生成一次，后续零成本返回同一份字符串。
/// 用于 `JwtConfig::private_key` 与 `mint_*_token` helper 签 RS256。
pub fn test_private_pem() -> &'static str {
    &keys_current().private_pem
}

/// 测试用 RSA 公钥 PEM（SPKI），按 kid 索引。
///
/// `kid` 必须在 [`test_public_kids`] 列表内（当前固定 `current` / `next`）。
/// 未在表内 → 抛 `None`，caller 应 `expect` 让测试失败信号清晰。
///
/// 查找顺序：先 current（绝大多数调用），再 next（lazy init `KEYS_NEXT`）。
pub fn test_public_pem(kid: &str) -> Option<&'static str> {
    if let Some(pem) = keys_current()
        .pairs
        .iter()
        .find(|(k, _)| k == kid)
        .map(|(_, p)| p.as_str())
    {
        return Some(pem);
    }
    keys_next()
        .pairs
        .iter()
        .find(|(k, _)| k == kid)
        .map(|(_, p)| p.as_str())
}

/// 测试用 (kid, public_pem) 对列表。
///
/// 当前固定返回 `("current", ...)` + `("next", ...)` 两对：
/// - `current` 是签发端 kid，与 `.env` 的 `JWT_SIGNING_KID` 对齐
/// - `next` 是模拟「密钥轮换中」的已部署公钥，供未来轮换测试扩展
///
/// 注意：返回的字符串都是 `&'static` 借用（数据存进 `OnceLock`），
/// caller 不应持有跨进程边界的引用（理论上不存在）。
///
/// 调用此函数会触发 `KEYS_NEXT` lazy init —— 仅当测试需要 next kid 时才生成。
pub fn test_public_kids() -> &'static [(&'static str, &'static str)] {
    // OnceLock 内的 String 转 &'static str：靠 unsafe 一次性转；这是 OnceLock
    // 内部存 'static 数据 + 不会变更的标准模式。
    static PAIRS: OnceLock<Vec<(&'static str, &'static str)>> = OnceLock::new();
    PAIRS.get_or_init(|| {
        let cur = keys_current();
        let nxt = keys_next();
        // 收集成 (kid, pub_pem) 列表；pub_pem 是 'static 借用（OnceLock 持有 String → &str）。
        let mut v: Vec<(&'static str, &'static str)> =
            Vec::with_capacity(cur.pairs.len() + nxt.pairs.len());
        for (kid, pem) in cur.pairs.iter().chain(nxt.pairs.iter()) {
            // 借用 OnceLock 里的 String 为 'static str —— 安全因为 OnceLock 不会释放
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
    /// ⚠️ 历史 bug：tests/auth_middleware.rs 一度用 `#[path = "common/pem.rs"]
    /// mod pem;` 引入第二个 pem 实例，与 tests/common/mod.rs 的 `mod pem;`
    /// 各自一套 OnceLock → 签发 / 验签走两套不同 keypair → 40100 InvalidSignature。
    /// 自 PR13 Phase A 起：唯一 pem 实例在 `hsh_erp_test_support::pem`，
    /// `tests/common/mod.rs` 保留 `pub mod pem { pub use hsh_erp_test_support::pem::*; }`
    /// 转发到同一份 OnceLock，签发 / 验签自然共享。
    #[test]
    fn roundtrip_sign_and_verify() {
        use chrono::Utc;
        use jsonwebtoken::{
            Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode,
        };
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