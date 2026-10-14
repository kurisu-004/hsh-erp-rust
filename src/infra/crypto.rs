//! 对称加解密基建（2026-10-16 新增，AES-256-GCM）
//!
//! 唯一生产用途：`scheduled_task` 域的**邮箱授权码**加密入库 / 解密发信。
//!
//! ## 为什么必须是「可逆」加密
//!
//! 本仓另有 `auth::password`（bcrypt）那条路，那是**单向散列** —— 取不回明文。
//! SMTP 发信必须把授权码原样交给 `lettre`，故这里只能是对称加密。
//! 两者**不可混用**：把授权码当密码去 `hash()` 只会得到一个永远发不出信的配置。
//!
//! ## 密文信封格式
//!
//! ```text
//! base64( nonce(12) ‖ ciphertext(len=N) ‖ tag(16) )
//! ```
//!
//! - **nonce 12 字节**：GCM 的标准 nonce 长度（96 bit），每 seal **必须新生成**
//!   （`SystemRandom` + `generate()`）。复用 nonce 会让 GCM 的机密性与完整性同时失守，
//!   且**不会报错** —— 这是本仓唯一一处「静默出错即安全性归零」的地方。
//! - **tag 16 字节**：GCM 默认 tag 长度，由 `seal_in_place_append_tag` 自动追加。
//! - 整个信封进 `encrypted_secret text` 列，**base64 之外没有任何结构**（无版本前缀、
//!   无字段分隔），换取「列值可以直接搬到别处而仍能解出」的失败面最小：搬走以后
//!   AAD 不匹配 ⇒ 解不开（见下）。
//!
//! ## AAD 必须绑定用途
//!
//! [`AAD`] 是 `b"t_mail_account.secret.v1"`，seal / open **两侧逐字同一个常量**。
//! AEAD 会把 AAD 一并纳入认证，于是密文无法被搬到别的表 / 别的字段上再用：
//! 换 AAD 解即失败。这挡住的是「把某处密文贴到另一列当别的凭据用」这类搬砖式攻击。
//! 改 AAD 常量 = 存量密文**全部**解不开（同换主密钥，见下），故它带 `v1` 后缀。
//!
//! ## 主密钥 `MAIL_ACCOUNT_SECRET`
//!
//! - **格式**：64 位十六进制（`openssl rand -hex 32`）或 44 位 base64（32 字节的
//!   标准 base64 编码）。两者都接受，解析见
//!   [`crate::infra::config::MailAccountSecret::parse`]。
//! - **缺失 / 长度不对 ⇒ 启动直接 bail**（`config.rs` 的 fail-fast，与
//!   `MAIL_SMTP_TLS_MODE` 同款范式）：这是配置错误，不是可降级项 ——
//!   带着一把猜出来的密钥启动，只会让「保存邮箱成功但永远发不出信」变成现场事故。
//! - ⚠️ **换主密钥会让存量授权码全部解不开**（GCM 无密钥轮换机制）。要轮换得
//!   「解密 → 换密钥重新加密」逐行迁移，`docs/api/mail.md` §8.3 已登记该缺口。
//!
//! ## 为什么每次调用都重读 env，而不是把密钥挂在 `AppState` 上
//!
//! [`seal`] / [`open`] 的签名里没有 state 入参（它们是纯工具函数）。重读一次
//! `std::env::var` 的代价约几十纳秒，而调用频率是「管理员配一次邮箱」与「一天发几封
//! 信」，**量级上完全可忽略**。换来的是：本模块不持有任何进程级可变全局态，
//! 单测可以用 [`seal_with`] / [`open_with`] 直接注入密钥，**不需要改 env**
//! （Rust 2024 的 `std::env::set_var` 已是 unsafe，多测试并行改 env 是本仓明确
//! 回避的做法，见 `tests/cos_opendal_api.rs` 的 `ENV_LOCK`）。
//!
//! 启动期的校验（fail-fast）与这里的读取用的是**同一个解析函数**
//! （[`crate::infra::config::MailAccountSecret::parse`]），故「启动通过」与
//! 「运行期可解密」不可能分叉。

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::error::Unspecified;
use ring::rand::{SecureRandom, SystemRandom};

use crate::infra::config::MailAccountSecret;
use crate::shared::error::{AppError, code};

/// AEAD 的附加认证数据（AAD）。**seal / open 两侧必须是同一个常量**。
///
/// ⚠️ 改这一个字面量 = 存量密文全部解不开（同换主密钥）。带 `v1` 后缀就是为了让
/// 「将来要换」这件事在版本号上是可见的、而不是看起来像一次无害的重构。
///
/// ⚠️ **本常量只服务 `t_mail_account.encrypted_secret`**。别的列（`t_wecom_webhook
/// .encrypted_url`）走 [`seal_with_aad`] / [`open_with_aad`] 显式传自己的 AAD，
/// **不要**为了「少一个参数」把它改成读某个全局 —— 一次 AAD 串线会让那一整类密文
/// 永久解不开，且没有任何报错能指出是哪边串的。
const AAD: &[u8] = b"t_mail_account.secret.v1";

/// 报错文案里的「凭据名」前缀（`t_mail_account` 那条路用它）。
const SUBJECT_MAIL_SECRET: &str = "邮箱授权码";

/// GCM nonce 字节数（GCM 标准 nonce 长度 96 bit）。
const NONCE_LEN: usize = 12;

/// GCM tag 字节数（`AES_256_GCM` 的默认 tag 长度）。
const TAG_LEN: usize = 16;

/// 把明文加密成 `base64(nonce ‖ ct ‖ tag)` 文本。
///
/// 主密钥取自 env [`MAIL_ACCOUNT_SECRET_ENV`]；缺失 / 格式不对 ⇒
/// `21701 BIZ_MAIL_NOT_CONFIGURED`（HTTP 503，见 `shared/error.rs` 的映射）。
///
/// 授权码创建是唯一生产调用点（`scheduled_task::service`），一次一封密文，代价可忽略。
pub fn seal(plaintext: &str) -> Result<String, AppError> {
    seal_with(&master_key()?, plaintext)
}

/// 解 `seal` 产出的信封；失败一律返 `21701 BIZ_MAIL_NOT_CONFIGURED`。
///
/// ⚠️ **失败原因有四类，本函数不向调用方区分**（消息文本里区分）：主密钥缺失/格式错、
/// base64 损坏、密文或 tag 被篡改、**AAD 或主密钥不匹配**。
///
/// 第四类是最需要留意的：它同时覆盖「密文被搬到别的表/字段」与「运维把
/// `MAIL_ACCOUNT_SECRET` 换掉了」两种成因。调用方（`mail_digest::service` 的
/// resolver）会在拿到本错误时补一行 `tracing::error!` 提示后者 —— 见该函数 doc。
pub fn open(sealed: &str) -> Result<String, AppError> {
    open_with(&master_key()?, sealed)
}

/// [`seal`] 的 **AAD 显式版**（2026-10-18 新增，供 `wecom` 通道域用）。
///
/// ## 为什么要这一对，而不是给本模块加第二个全局 AAD 常量
///
/// AEAD 把 AAD 纳入认证 ⇒ **不同 AAD 的两类密文互不串扰**：拿 `t_wecom_webhook` 的密文
/// 去冒充 SMTP 授权码（或反过来）会直接 `open` 失败，而不是「悄悄解出别的东西」。
/// 主密钥仍是同一把 env `MAIL_ACCOUNT_SECRET`（**不新增 env**），变的只是这一层用途标签。
///
/// 生产调用点只有一个：`modules::wecom::service`（AAD 取
/// `modules::wecom::dto::WEBHOOK_URL_AAD` = `t_wecom_webhook.url.v1`）。
///
/// ⚠️ 失败时返回的错误码仍是 `21701 BIZ_MAIL_NOT_CONFIGURED`（文案主体为「webhook 地址」）：
/// 成因分类完全相同（主密钥缺失/格式错、base64 损坏、tag 失配、AAD 或主密钥不匹配），
/// **刻意不开新码** —— 调用方 `wecom::service::resolve_target` 会把它转成 `21801`
/// （那是推送域自己的「通道不可用」语义，见该函数 doc）。
pub fn seal_with_aad(plaintext: &str, aad: &[u8]) -> Result<String, AppError> {
    seal_impl(&master_key()?, plaintext, aad)
}

/// [`open`] 的 **AAD 显式版**（见 [`seal_with_aad`] 的存在理由）。
pub fn open_with_aad(sealed: &str, aad: &[u8]) -> Result<String, AppError> {
    open_with_key_aad(&master_key()?, sealed, aad, SUBJECT_WEBHOOK_URL)
}

/// 报错文案里的「凭据名」前缀 —— `t_wecom_webhook.encrypted_url` 那条路用它。
const SUBJECT_WEBHOOK_URL: &str = "webhook 地址";

/// [`seal`] 的密钥显式版（[`seal_with`] 的密钥注入版）。
///
/// **生产不直接调**（走 `seal` 从 env 取钥）；存在意义是让单测能注入两把不同的
/// 密钥验证「错密钥必须拒绝」，以及绕开 env 依赖。两条路径共用同一段加解密实现，
/// 不存在「测试走的是另一条分支」的问题。
pub fn seal_with(key: &[u8; 32], plaintext: &str) -> Result<String, AppError> {
    seal_impl(key, plaintext, AAD)
}

/// 加密的唯一实现体（AAD 与密钥都由调用方给）。
fn seal_impl(key: &[u8; 32], plaintext: &str, aad: &[u8]) -> Result<String, AppError> {
    let unbound = UnboundKey::new(&AES_256_GCM, key).map_err(crypto_err)?;
    let sealing = LessSafeKey::new(unbound);

    let mut nonce_bytes = [0u8; NONCE_LEN];
    // `generate` 失败 = 拿不到系统熵 ⇒ 直接失败，绝不退回「固定 nonce」（那会
    // 让同一密钥下的两次加密产生相关联的密文对，GCM 的安全性直接归零）。
    SystemRandom::new()
        .fill(&mut nonce_bytes)
        .map_err(|_| internal_err("系统随机数生成失败，无法产生 AES-GCM nonce"))?;

    let nonce = Nonce::assume_unique_for_key(nonce_bytes);
    let mut buf = plaintext.as_bytes().to_vec();
    sealing
        .seal_in_place_append_tag(nonce, Aad::from(aad), &mut buf)
        .map_err(|_| internal_err("AES-256-GCM 加密失败"))?;

    let mut envelope = Vec::with_capacity(NONCE_LEN + buf.len());
    envelope.extend_from_slice(&nonce_bytes);
    envelope.extend_from_slice(&buf);
    Ok(BASE64.encode(envelope))
}

/// [`open`] 的密钥显式版（见 [`seal_with`] 的存在理由）。
pub fn open_with(key: &[u8; 32], sealed: &str) -> Result<String, AppError> {
    open_with_key_aad(key, sealed, AAD, SUBJECT_MAIL_SECRET)
}

/// 解密的唯一实现体。`subject` 只进报错文案，**不参与**密码学计算。
pub fn open_with_key_aad(
    key: &[u8; 32],
    sealed: &str,
    aad: &[u8],
    subject: &str,
) -> Result<String, AppError> {
    let unbound = UnboundKey::new(&AES_256_GCM, key).map_err(crypto_err)?;
    let opening = LessSafeKey::new(unbound);

    let raw = BASE64.decode(sealed.as_bytes()).map_err(|_| {
        decrypt_err(
            subject,
            "密文不是合法 base64（列值被手工改过或不是本模块写入的）",
        )
    })?;
    // 最短也要 nonce(12) + tag(16)；比这更短的一定是被截断的密文。
    if raw.len() < NONCE_LEN + TAG_LEN {
        return Err(decrypt_err(
            subject,
            &format!(
                "密文长度不足（{} 字节，至少需要 nonce {NONCE_LEN} + tag {TAG_LEN}）",
                raw.len()
            ),
        ));
    }

    let (nonce_bytes, rest) = raw.split_at(NONCE_LEN);
    let mut nonce_arr = [0u8; NONCE_LEN];
    nonce_arr.copy_from_slice(nonce_bytes);
    let nonce = Nonce::assume_unique_for_key(nonce_arr);

    // `open_in_place` 原地解密：失败时**缓冲区内容未定义**，所以失败必须立刻返回，
    // 绝不能接着去读 `rest`。ring 的文档明确要求调用方不区分 Ok/Err 之外的状态。
    let mut buf = rest.to_vec();
    let plain = opening
        .open_in_place(nonce, Aad::from(aad), &mut buf)
        .map_err(|_| {
            decrypt_err(
                subject,
                "AES-256-GCM 认证失败（密文/tag 被篡改、AAD 不匹配，或 MAIL_ACCOUNT_SECRET 已被更换）",
            )
        })?;

    String::from_utf8(plain.to_vec())
        .map_err(|_| internal_err("解密结果是非法 UTF-8（密文来源异常）"))
}

/// 取本次调用的主密钥（每次重读 env，见模块 doc 的取舍段）。
///
/// `config.rs` 的解析走 `anyhow`（那是启动期的主战场，`main.rs` 直接 bail），
/// 这里把它降级成 `21701` 业务错误：运行期的调用方是 HTTP handler 与 cron job，
/// 它们需要的是「可读的 503 + 明确的缺哪个 env」，而不是一个 500。
fn master_key() -> Result<[u8; 32], AppError> {
    MailAccountSecret::from_env_value()
        .map(|k| k.key())
        .map_err(|e| AppError::biz(code::BIZ_MAIL_NOT_CONFIGURED, e.to_string()))
}

/// `UnboundKey::new` 失败：密钥长度不对。走 decrypt/encrypt 各自的中性文案。
fn crypto_err(_: Unspecified) -> AppError {
    internal_err("AES-256-GCM 密钥长度非法（必须是 32 字节）")
}

fn decrypt_err(subject: &str, msg: &str) -> AppError {
    AppError::biz(
        code::BIZ_MAIL_NOT_CONFIGURED,
        format!("{subject}解密失败：{msg}"),
    )
}

fn internal_err(msg: &str) -> AppError {
    AppError::internal(msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::config::MAIL_ACCOUNT_SECRET_ENV;

    /// 固定测试密钥（生产密钥来自 env，两者在测试里刻意用不同值，
    /// 以便「错密钥拒绝」这条用例有意义）。
    const KEY_A: [u8; 32] = [0x11u8; 32];
    const KEY_B: [u8; 32] = [0x22u8; 32];

    #[test]
    fn round_trip_restores_plaintext() {
        let sealed = seal_with(&KEY_A, "abcdef123456").expect("加密必须成功");
        assert_eq!(
            open_with(&KEY_A, &sealed).expect("解密必须成功"),
            "abcdef123456"
        );
    }

    /// 两次加密同一明文必须产出**不同**密文（nonce 每次新生成）。
    ///
    /// 若 nonce 复用了，这条会红；它比「密文对不对」更能守住 GCM 的根本前提。
    #[test]
    fn two_seals_of_same_plaintext_differ() {
        let a = seal_with(&KEY_A, "same-secret").unwrap();
        let b = seal_with(&KEY_A, "same-secret").unwrap();
        assert_ne!(a, b, "同一明文两次加密必须不同（nonce 复用 = GCM 失效）");
        // 但都还能解回同一明文。
        assert_eq!(open_with(&KEY_A, &a).unwrap(), "same-secret");
        assert_eq!(open_with(&KEY_A, &b).unwrap(), "same-secret");
    }

    #[test]
    fn wrong_key_is_rejected() {
        let sealed = seal_with(&KEY_A, "top-secret").unwrap();
        let err = open_with(&KEY_B, &sealed).unwrap_err();
        assert_eq!(err.code(), code::BIZ_MAIL_NOT_CONFIGURED);
        assert!(err.to_string().contains("解密失败"));
    }

    #[test]
    fn tampered_ciphertext_is_rejected() {
        let sealed = seal_with(&KEY_A, "top-secret").unwrap();
        let mut raw = BASE64.decode(&sealed).unwrap();
        // 动密文正文区的最后一个字节（tag 前一字节）—— 认证必然失败。
        let idx = raw.len() - TAG_LEN - 1;
        raw[idx] ^= 0xFF;
        let err = open_with(&KEY_A, &BASE64.encode(&raw)).unwrap_err();
        assert_eq!(err.code(), code::BIZ_MAIL_NOT_CONFIGURED);
    }

    /// 改 nonce 也必须被拒：nonce 是 GCM 的完整性覆盖范围的一部分。
    #[test]
    fn tampered_nonce_is_rejected() {
        let sealed = seal_with(&KEY_A, "top-secret").unwrap();
        let mut raw = BASE64.decode(&sealed).unwrap();
        raw[0] ^= 0xFF;
        assert!(open_with(&KEY_A, &BASE64.encode(&raw)).is_err());
    }

    /// 空串明文是**合法**输入（授权码空串由 service 层拦，这里只保证加解密本身
    /// 能往返：信封长度 = nonce 12 + tag 16，`open` 的长度下限恰好卡在这条线上）。
    #[test]
    fn empty_plaintext_round_trips() {
        let sealed = seal_with(&KEY_A, "").unwrap();
        let raw = BASE64.decode(&sealed).unwrap();
        assert_eq!(
            raw.len(),
            NONCE_LEN + TAG_LEN,
            "空明文的信封应只剩 nonce + tag"
        );
        assert_eq!(open_with(&KEY_A, &sealed).unwrap(), "");
    }

    #[test]
    fn broken_base64_is_rejected() {
        for bad in ["!!!not-base64!!!", "a", "", "  "] {
            let err = open_with(&KEY_A, bad).unwrap_err();
            assert_eq!(
                err.code(),
                code::BIZ_MAIL_NOT_CONFIGURED,
                "损坏 base64 {bad:?} 必须被拒"
            );
        }
    }

    /// 比「nonce + tag」还短的密文（在正确 base64 的前提下）必须被长度闸拦下，
    /// 而不是进到 `open_in_place` 拿一个语义不明的错误。
    #[test]
    fn truncated_envelope_is_rejected() {
        let too_short = BASE64.encode([0u8; NONCE_LEN + TAG_LEN - 1]);
        assert!(open_with(&KEY_A, &too_short).is_err());
    }

    /// AAD 不匹配必须被拒：用另一段 AAD 复用同一段密文（模拟「密文被搬到
    /// 别的表 / 别的字段」）。本测试直接调 ring，验证 AAD 确实参与认证。
    #[test]
    fn aad_mismatch_is_rejected() {
        let sealed = seal_with(&KEY_A, "moved-secret").unwrap();
        let raw = BASE64.decode(&sealed).unwrap();
        let (nonce_bytes, rest) = raw.split_at(NONCE_LEN);
        let mut nonce_arr = [0u8; NONCE_LEN];
        nonce_arr.copy_from_slice(nonce_bytes);
        let unbound = UnboundKey::new(&AES_256_GCM, &KEY_A).unwrap();
        let opening = LessSafeKey::new(unbound);
        let mut buf = rest.to_vec();
        let bad_aad = Aad::from(b"t_mail_account.other.v1".as_slice());
        assert!(
            opening
                .open_in_place(Nonce::assume_unique_for_key(nonce_arr), bad_aad, &mut buf)
                .is_err(),
            "换掉 AAD 必须解不开（否则密文能被搬到别的字段复用）"
        );
    }

    /// 两类凭据**共用一把主密钥、不同 AAD** ⇒ 互相解不开（2026-10-18）。
    ///
    /// 守的是「`seal_with_aad` / `open_with_aad` 的 AAD 真的进了认证计算」这件事：
    /// 若哪天有人图省事把 AAD 改成忽略参数，webhook 的密文就能被当成 SMTP 授权码取回
    /// （反之亦然），而两边都跑得好好的、零报错。
    #[test]
    fn different_aad_cannot_open_the_other_tables_ciphertext() {
        const WECOM_AAD: &[u8] = b"t_wecom_webhook.url.v1";

        let webhook_sealed = seal_impl(&KEY_A, "693a-uuid-key", WECOM_AAD).unwrap();
        // 用 mail 的 AAD 解不开
        assert!(
            open_with(&KEY_A, &webhook_sealed).is_err(),
            "webhook 的密文不得被当成 SMTP 授权码解出"
        );
        // 用 webhook 自己的 AAD 能解开
        assert_eq!(
            open_with_key_aad(&KEY_A, &webhook_sealed, WECOM_AAD, SUBJECT_WEBHOOK_URL).unwrap(),
            "693a-uuid-key"
        );
        // 反向：mail 的密文也解不开
        let mail_sealed = seal_with(&KEY_A, "smtp-secret").unwrap();
        assert!(
            open_with_key_aad(&KEY_A, &mail_sealed, WECOM_AAD, SUBJECT_WEBHOOK_URL).is_err(),
            "SMTP 授权码的密文不得被当成 webhook 地址解出"
        );
    }

    /// env 里没有 [`MAIL_ACCOUNT_SECRET_ENV`] 时 [`seal`] / [`open`] 必须返
    /// 21701 而不是 panic（lib 单测进程不加载 `.env`，本用例天然满足「未设置」）。
    ///
    /// 覆盖的是**降级形态**这条契约：配置缺失要变成一条可读的 503 业务错误，
    /// 不能变成 500 或 panic。启动期的 fail-fast 由 `config.rs` 单独守。
    #[test]
    fn missing_env_secret_yields_biz_error_not_panic() {
        if std::env::var(MAIL_ACCOUNT_SECRET_ENV).is_ok() {
            // 进程环境里恰好有值（有人在跑测试前 export 过）→ 本用例不成立，跳过
            // 而不是改 env（Rust 2024 的 set_var 是 unsafe，且会污染并行测试）。
            return;
        }
        let err = seal("x").unwrap_err();
        assert_eq!(err.code(), code::BIZ_MAIL_NOT_CONFIGURED);
        let err = open("aGVsbG8=").unwrap_err();
        assert_eq!(err.code(), code::BIZ_MAIL_NOT_CONFIGURED);
    }
}
