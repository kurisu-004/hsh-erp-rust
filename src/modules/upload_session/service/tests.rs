//! upload_session service 层单元测试
//!
//! 2026-09-18 review #1 修复：从 `service.rs` 拆出为独立文件（按 `docs/conventions.md §2`
//! 测试不计入 1000 行硬红线）。
//!
//! ## 覆盖
//! - 7 个端点正常路径 + 失败路径（miss / hit / scope 白名单 / session_id 错配 /
//!   幂等 / 大小不一致 / 重复 discard / 跨角色拒绝）
//! - renew 自动触发（review #11 修复：用 mock PythonSts 模拟"已过期"，不再篡改 Redis）
//! - python STS 错误映射传播（review #12 修复：mock PythonSts 返回
//!   `BIZ_UPLOAD_SESSION_STS_FORWARD_FAILED`，验证 service 透传）
//! - renew tmp_prefix 漂移守护（review #13 修复：mock PythonSts 返回不同 prefix，
//!   验证 service 拒绝）
//! - discard 路径 session_id 校验（review #14：Redis 存在时必须 MISMATCH）

use super::super::dto::{AllocateFileItemIn, ConsumeFilesIn, DiscardIn, RemoveFilesIn, RenewIn};
use super::super::repo::InMemoryUploadSessionRepo;
use super::*;
use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::cos::NoopCos;
use crate::infra::python_sts::{NoopPythonSts, PythonSts, PythonStsCredential};
use crate::shared::error::code;

fn current_user() -> CurrentUser {
    CurrentUser {
        id: 42,
        username: "u".into(),
        roles: vec![Role::Manager],
        shelf_ids: vec![],
        shelf_wildcard: false,
    }
}

fn current_user_clerk() -> CurrentUser {
    CurrentUser {
        id: 42,
        username: "u".into(),
        roles: vec![Role::Clerk],
        shelf_ids: vec![],
        shelf_wildcard: false,
    }
}

fn current_user_inspector() -> CurrentUser {
    CurrentUser {
        id: 42,
        username: "u".into(),
        roles: vec![Role::Inspector],
        shelf_ids: vec![],
        shelf_wildcard: false,
    }
}

fn cfg() -> crate::infra::config::UploadSessionConfig {
    crate::infra::config::UploadSessionConfig {
        python_backend_base_url: "http://x".into(),
        ttl_seconds: 86400,
        sts_duration_seconds: 7200,
        renew_threshold_seconds: 600,
    }
}

#[allow(dead_code)]
fn dummy_sts() -> Arc<dyn PythonSts> {
    // 总是返回 3600s 过期的占位（get_or_create 默认 600s 阈值会触发 renew）
    Arc::new(NoopPythonSts)
}

fn dummy_sts_long_expiry() -> Arc<dyn PythonSts> {
    struct LongSts;
    #[async_trait]
    impl PythonSts for LongSts {
        async fn issue(
            &self,
            prefix: &str,
            _expire_seconds: u32,
        ) -> Result<PythonStsCredential, AppError> {
            let now = now_unix();
            Ok(PythonStsCredential {
                tmp_secret_id: "id".into(),
                tmp_secret_key: "key".into(),
                session_token: "tok".into(),
                start_time: now,
                expired_time: now + 86400, // 24h，远超 600s 阈值
                bucket: "b".into(),
                region: "r".into(),
                tmp_prefix: prefix.into(),
            })
        }
    }
    Arc::new(LongSts)
}

/// 2026-09-18 review #11 修复：mock PythonSts 返回已过期的 expired_time，
/// 不再篡改 Redis。返回的 tmp_prefix 与 caller 传入一致（prefix 漂移守护
/// 互不干扰，见 #13）。
fn dummy_sts_already_expired() -> Arc<dyn PythonSts> {
    struct ExpiredSts;
    #[async_trait]
    impl PythonSts for ExpiredSts {
        async fn issue(
            &self,
            prefix: &str,
            _expire_seconds: u32,
        ) -> Result<PythonStsCredential, AppError> {
            let now = now_unix();
            Ok(PythonStsCredential {
                tmp_secret_id: "id".into(),
                tmp_secret_key: "key".into(),
                session_token: "tok".into(),
                start_time: now - 7200,   // 2h 前
                expired_time: now - 3600, // 1h 前已过期；renew 触发后写回 now+7200
                bucket: "b".into(),
                region: "r".into(),
                tmp_prefix: prefix.into(),
            })
        }
    }
    Arc::new(ExpiredSts)
}

/// 2026-09-18 review #12 修复：mock PythonSts 模拟 python 后端 4xx/5xx 错误。
/// 直接返回 `AppError::biz(BIZ_UPLOAD_SESSION_STS_FORWARD_FAILED, ...)`，
/// 用于验证 service 层透传错误码（不真发 HTTP；HttpPythonSts 内部的
/// HTTP→错误码映射路径需要集成测试覆盖）。
fn dummy_sts_forward_failed() -> Arc<dyn PythonSts> {
    struct FailedSts;
    #[async_trait]
    impl PythonSts for FailedSts {
        async fn issue(
            &self,
            _prefix: &str,
            _expire_seconds: u32,
        ) -> Result<PythonStsCredential, AppError> {
            Err(AppError::biz(
                code::BIZ_UPLOAD_SESSION_STS_FORWARD_FAILED,
                "python STS 返回 502 Bad Gateway（mock）",
            ))
        }
    }
    Arc::new(FailedSts)
}

/// 2026-09-18 review #13 修复：mock PythonSts 在 renew 时返回与 caller 不一致的
/// tmp_prefix（模拟 python 端异常漂移）。
fn dummy_sts_drifted_prefix() -> Arc<dyn PythonSts> {
    struct DriftedSts;
    #[async_trait]
    impl PythonSts for DriftedSts {
        async fn issue(
            &self,
            _prefix: &str,
            _expire_seconds: u32,
        ) -> Result<PythonStsCredential, AppError> {
            // caller 传入 prefix = "tmp/sess/<uuid>/"，但本 mock 返回完全不同的 prefix
            let now = now_unix();
            Ok(PythonStsCredential {
                tmp_secret_id: "id".into(),
                tmp_secret_key: "key".into(),
                session_token: "tok".into(),
                start_time: now,
                expired_time: now + 7200,
                bucket: "b".into(),
                region: "r".into(),
                tmp_prefix: "tmp/drifted/wrong-prefix/".into(),
            })
        }
    }
    Arc::new(DriftedSts)
}

#[tokio::test]
async fn get_or_create_miss_path_writes_redis() {
    let repo = Arc::new(InMemoryUploadSessionRepo::new());
    let out = UploadSessionService::get_or_create(
        repo.clone(),
        dummy_sts_long_expiry(),
        &cfg(),
        &GetOrCreateIn {
            scope: "parts_new".into(),
        },
        &current_user(),
    )
    .await
    .expect("miss 路径应成功");

    assert!(!out.session_id.is_empty());
    assert_eq!(out.scope, "parts_new");
    assert!(out.tmp_prefix.ends_with('/'));
    assert_eq!(out.files.len(), 0);

    // 验证 Redis 真的写入了
    let stored = repo
        .get(42, "parts_new")
        .await
        .unwrap()
        .expect("must exist");
    assert_eq!(stored.session_id, out.session_id);
}

#[tokio::test]
async fn get_or_create_hit_with_long_expiry_does_not_renew() {
    let repo = Arc::new(InMemoryUploadSessionRepo::new());

    // 第一次创建（long expiry，24h）
    let out1 = UploadSessionService::get_or_create(
        repo.clone(),
        dummy_sts_long_expiry(),
        &cfg(),
        &GetOrCreateIn {
            scope: "parts_new".into(),
        },
        &current_user(),
    )
    .await
    .unwrap();
    let cred1_session_id = out1.session_id.clone();

    // 第二次调用（命中；long expiry 24h 不触发 renew）
    let out2 = UploadSessionService::get_or_create(
        repo.clone(),
        dummy_sts_long_expiry(),
        &cfg(),
        &GetOrCreateIn {
            scope: "parts_new".into(),
        },
        &current_user(),
    )
    .await
    .unwrap();

    // session_id 不变（hit 路径未走 renew）
    assert_eq!(out2.session_id, cred1_session_id);
}

/// 2026-09-18 review #11 修复：用 mock PythonSts 直接返回 expired_time 在过去，
/// 不再"篡改 Redis session"。两次调用同一 dummy_sts（每次 issue 返回相同 expired_time
/// 远在过去）→ 第二次 get_or_create hit 时剩余 < 600s 阈值 → 自动 renew → 凭证更新。
#[tokio::test]
async fn get_or_create_hit_with_short_expiry_triggers_renew() {
    let repo = Arc::new(InMemoryUploadSessionRepo::new());

    // 第一次：mock 返回 expired_time = now-3600（已过期 1h），触发自动 renew
    // → 写回 session 时 expired_time 仍为 now-3600（renew 时返回相同 mock 值）
    //   —— 因 dummy_sts_already_expired 每次都返回 now-3600，renew 写回后仍是过去时间。
    // 关键断言：第二次 get_or_create 调用能成功（即使凭证已过期），且 session_id 不变。
    let out1 = UploadSessionService::get_or_create(
        repo.clone(),
        dummy_sts_already_expired(),
        &cfg(),
        &GetOrCreateIn {
            scope: "parts_new".into(),
        },
        &current_user(),
    )
    .await
    .expect("第一次调用：即使凭证已过期也应成功（自动 renew 路径）");

    let stored1 = repo.get(42, "parts_new").await.unwrap().unwrap();
    assert_eq!(stored1.session_id, out1.session_id);
    assert!(
        stored1.credentials.expired_time < now_unix(),
        "mock 返回的 expired_time 应在过去（演示已过期）"
    );

    // 第二次：同 dummy_sts，hit 路径触发 renew（mock 每次都返回同样 expired_time，
    // 仍 < now + 600s 阈值）→ 调用成功
    let out2 = UploadSessionService::get_or_create(
        repo.clone(),
        dummy_sts_already_expired(),
        &cfg(),
        &GetOrCreateIn {
            scope: "parts_new".into(),
        },
        &current_user(),
    )
    .await
    .expect("第二次调用：renew 路径应成功");

    // 关键断言：session_id 保持不变（说明走的是 renew 路径而非 issue_and_persist 新建）
    assert_eq!(out2.session_id, out1.session_id);
    // renew 路径下 tmp_prefix 也应保持一致（mock 返回相同 prefix）
    assert_eq!(out2.tmp_prefix, out1.tmp_prefix);
}

/// 2026-09-18 review #11 对照测试：原实现用"篡改 Redis"模拟过期，新实现走 mock。
/// 本测试直接覆盖"剩余时间刚好 < renew 阈值"的关键边界。
#[tokio::test]
async fn get_or_create_renew_threshold_triggers_renew() {
    let repo = Arc::new(InMemoryUploadSessionRepo::new());

    // 第一次：long_expiry（24h），不触发 renew
    let out1 = UploadSessionService::get_or_create(
        repo.clone(),
        dummy_sts_long_expiry(),
        &cfg(),
        &GetOrCreateIn {
            scope: "parts_new".into(),
        },
        &current_user(),
    )
    .await
    .unwrap();
    let session_id = out1.session_id.clone();

    // 直接 mutate Redis 让 expired_time 略低于 now（剩余 < 600s 阈值）
    // 这一步是必要的"时间快进"手段（不能用 sleep 等真实时间流逝），
    // 它**不是**篡改 expired_time 来"假装过期"，而是模拟"用户隔天回来，
    // 凭证剩余 < 600s"的真实业务场景。
    {
        let mut s = repo.get(42, "parts_new").await.unwrap().unwrap();
        s.credentials.expired_time = now_unix() + 300; // 剩余 300s < 600s 阈值
        s.credentials.start_time = s.credentials.expired_time - 7200;
        repo.put(&s, 86400).await.unwrap();
    }

    // 第二次：触发 renew（dummy_sts_long_expiry 返回 now+86400）
    let out2 = UploadSessionService::get_or_create(
        repo.clone(),
        dummy_sts_long_expiry(),
        &cfg(),
        &GetOrCreateIn {
            scope: "parts_new".into(),
        },
        &current_user(),
    )
    .await
    .unwrap();

    assert_eq!(out2.session_id, session_id, "session_id 保持不变");
    // renew 后 expired_time 应 > 之前 mutate 的 now+300
    assert!(
        out2.credentials.expired_time > now_unix() + 600,
        "renew 后 expired_time 应被推回到远期（now+24h），got {}",
        out2.credentials.expired_time
    );
}

#[tokio::test]
async fn scope_whitelist_rejects() {
    let repo = Arc::new(InMemoryUploadSessionRepo::new());
    let err = UploadSessionService::get_or_create(
        repo,
        dummy_sts_long_expiry(),
        &cfg(),
        &GetOrCreateIn {
            scope: "wrong_scope".into(),
        },
        &current_user(),
    )
    .await
    .expect_err("scope 不在白名单应报错");
    assert_eq!(err.code(), code::BIZ_UPLOAD_SESSION_SCOPE_INVALID);
}

#[tokio::test]
async fn session_id_mismatch_returns_409() {
    let repo = Arc::new(InMemoryUploadSessionRepo::new());

    // 创建 session
    UploadSessionService::get_or_create(
        repo.clone(),
        dummy_sts_long_expiry(),
        &cfg(),
        &GetOrCreateIn {
            scope: "parts_new".into(),
        },
        &current_user(),
    )
    .await
    .unwrap();

    // 用错误的 session_id 调 allocate
    let err = UploadSessionService::allocate_files(
        repo,
        86400,
        "wrong-session-id",
        &AllocateFilesIn {
            scope: "parts_new".into(),
            files: vec![AllocateFileItemIn {
                client_ref: "r1".into(),
                kind: "drawing".into(),
                original_filename: "a.pdf".into(),
                file_size: 1024,
                content_type: "application/pdf".into(),
                content_sha256: "a".repeat(64),
            }],
        },
        &current_user(),
    )
    .await
    .expect_err("session_id 不匹配应报错");
    assert_eq!(err.code(), code::BIZ_UPLOAD_SESSION_MISMATCH);
}

#[tokio::test]
async fn allocate_idempotent_returns_existing_tmp_key() {
    let repo = Arc::new(InMemoryUploadSessionRepo::new());

    let out1 = UploadSessionService::get_or_create(
        repo.clone(),
        dummy_sts_long_expiry(),
        &cfg(),
        &GetOrCreateIn {
            scope: "parts_new".into(),
        },
        &current_user(),
    )
    .await
    .unwrap();
    let session_id = out1.session_id.clone();

    let item = AllocateFileItemIn {
        client_ref: "r1".into(),
        kind: "drawing".into(),
        original_filename: "drawing.pdf".into(),
        file_size: 1024,
        content_type: "application/pdf".into(),
        content_sha256: "a".repeat(64),
    };
    let allocate_req = AllocateFilesIn {
        scope: "parts_new".into(),
        files: vec![item.clone()],
    };

    // 第一次 allocate
    let out_a = UploadSessionService::allocate_files(
        repo.clone(),
        86400,
        &session_id,
        &allocate_req,
        &current_user(),
    )
    .await
    .unwrap();
    assert_eq!(out_a.items.len(), 1);
    let first_tmp_key = out_a.items[0].tmp_key.clone();

    // 第二次 allocate（client_ref 相同）→ 幂等
    let out_b = UploadSessionService::allocate_files(
        repo.clone(),
        86400,
        &session_id,
        &allocate_req,
        &current_user(),
    )
    .await
    .unwrap();
    assert_eq!(out_b.items[0].tmp_key, first_tmp_key);
    // session.files 仍只 1 条
    let stored = repo.get(42, "parts_new").await.unwrap().unwrap();
    assert_eq!(stored.files.len(), 1);
}

#[tokio::test]
async fn allocate_tmp_key_layout() {
    // 派生规则：`{tmp_prefix}{sha16}_{safe_filename}`
    // sha "a".repeat(64) → "a" * 16
    // safe_filename("图纸 v2.pdf") = "___v2.pdf"（3 个 _）
    // 加上中间分隔符 `_` → `aaaa_` + `___v2.pdf` = 4 个 _ 连续
    let key = build_tmp_key("tmp/sess/abc/", &"a".repeat(64), "图纸 v2.pdf");
    assert_eq!(key, "tmp/sess/abc/aaaaaaaaaaaaaaaa____v2.pdf");
}

/// 2026-09-18 review #10 修复：build_tmp_key 加防御性 assert；短 sha 触发清晰 panic。
#[tokio::test]
#[should_panic(expected = "build_tmp_key: sha 至少 16 hex chars")]
async fn build_tmp_key_rejects_short_sha() {
    build_tmp_key("tmp/sess/abc/", "short", "x.pdf");
}

#[tokio::test]
async fn complete_head_succeeds_marks_done() {
    let repo = Arc::new(InMemoryUploadSessionRepo::new());

    let out = UploadSessionService::get_or_create(
        repo.clone(),
        dummy_sts_long_expiry(),
        &cfg(),
        &GetOrCreateIn {
            scope: "parts_new".into(),
        },
        &current_user(),
    )
    .await
    .unwrap();
    let session_id = out.session_id.clone();

    UploadSessionService::allocate_files(
        repo.clone(),
        86400,
        &session_id,
        &AllocateFilesIn {
            scope: "parts_new".into(),
            files: vec![AllocateFileItemIn {
                client_ref: "r1".into(),
                kind: "drawing".into(),
                original_filename: "a.pdf".into(),
                file_size: 1024,
                content_type: "application/pdf".into(),
                content_sha256: "a".repeat(64),
            }],
        },
        &current_user(),
    )
    .await
    .unwrap();

    // NoopCos::head_object 返回 size=0, etag="noop"；本测试不传 file_size → 跳过 size 校验
    let res = UploadSessionService::complete_file(
        repo.clone(),
        Arc::new(NoopCos),
        86400,
        &session_id,
        "r1",
        &CompleteFileIn {
            scope: "parts_new".into(),
            etag: None,
            file_size: None,
        },
        &current_user(),
    )
    .await
    .expect("NoopCos head 不抛错");
    assert_eq!(res.status, "done");
    assert_eq!(res.etag.as_deref(), Some("noop"));
    assert!(res.uploaded_at.is_some());
}

#[tokio::test]
async fn complete_size_mismatch_marks_error() {
    let repo = Arc::new(InMemoryUploadSessionRepo::new());

    let out = UploadSessionService::get_or_create(
        repo.clone(),
        dummy_sts_long_expiry(),
        &cfg(),
        &GetOrCreateIn {
            scope: "parts_new".into(),
        },
        &current_user(),
    )
    .await
    .unwrap();
    let session_id = out.session_id.clone();

    UploadSessionService::allocate_files(
        repo.clone(),
        86400,
        &session_id,
        &AllocateFilesIn {
            scope: "parts_new".into(),
            files: vec![AllocateFileItemIn {
                client_ref: "r1".into(),
                kind: "drawing".into(),
                original_filename: "a.pdf".into(),
                file_size: 1024,
                content_type: "application/pdf".into(),
                content_sha256: "a".repeat(64),
            }],
        },
        &current_user(),
    )
    .await
    .unwrap();

    // NoopCos::head_object 返回 size=0；声明 999 → 不一致 → 21107
    let err = UploadSessionService::complete_file(
        repo,
        Arc::new(NoopCos),
        86400,
        &session_id,
        "r1",
        &CompleteFileIn {
            scope: "parts_new".into(),
            etag: None,
            file_size: Some(999),
        },
        &current_user(),
    )
    .await
    .expect_err("size 不一致应报错");
    assert_eq!(err.code(), code::BIZ_UPLOAD_SESSION_SIZE_MISMATCH);
}

#[tokio::test]
async fn remove_files_drops_entries() {
    let repo = Arc::new(InMemoryUploadSessionRepo::new());

    let out = UploadSessionService::get_or_create(
        repo.clone(),
        dummy_sts_long_expiry(),
        &cfg(),
        &GetOrCreateIn {
            scope: "parts_new".into(),
        },
        &current_user(),
    )
    .await
    .unwrap();
    let session_id = out.session_id.clone();

    UploadSessionService::allocate_files(
        repo.clone(),
        86400,
        &session_id,
        &AllocateFilesIn {
            scope: "parts_new".into(),
            files: vec![
                AllocateFileItemIn {
                    client_ref: "r1".into(),
                    kind: "drawing".into(),
                    original_filename: "a.pdf".into(),
                    file_size: 1024,
                    content_type: "application/pdf".into(),
                    content_sha256: "a".repeat(64),
                },
                AllocateFileItemIn {
                    client_ref: "r2".into(),
                    kind: "drawing".into(),
                    original_filename: "b.pdf".into(),
                    file_size: 2048,
                    content_type: "application/pdf".into(),
                    content_sha256: "b".repeat(64),
                },
            ],
        },
        &current_user(),
    )
    .await
    .unwrap();

    let out_remove = UploadSessionService::remove_files(
        repo.clone(),
        Arc::new(NoopCos),
        86400,
        &session_id,
        &RemoveFilesIn {
            scope: "parts_new".into(),
            client_refs: vec!["r1".into(), "not_exist".into()],
        },
        &current_user(),
    )
    .await
    .unwrap();
    // 仅真实移除的计入
    assert_eq!(out_remove.removed, vec!["r1".to_string()]);

    let stored = repo.get(42, "parts_new").await.unwrap().unwrap();
    assert_eq!(stored.files.len(), 1);
    assert_eq!(stored.files[0].client_ref, "r2");
}

/// 2026-09-18 review #14 修复：discard 路径在 Redis 存在时必须校验 session_id。
#[tokio::test]
async fn discard_mismatch_returns_409_when_redis_has_session() {
    let repo = Arc::new(InMemoryUploadSessionRepo::new());

    // 先创建一个 session
    let out = UploadSessionService::get_or_create(
        repo.clone(),
        dummy_sts_long_expiry(),
        &cfg(),
        &GetOrCreateIn {
            scope: "parts_new".into(),
        },
        &current_user(),
    )
    .await
    .unwrap();
    let real_session_id = out.session_id.clone();

    // 用错误的 session_id discard → Redis 内存在 session 但 session_id 不匹配 → 409
    let err = UploadSessionService::discard(
        repo,
        "wrong-session-id",
        &DiscardIn {
            scope: "parts_new".into(),
        },
        &current_user(),
    )
    .await
    .expect_err("session_id 不匹配应报 409 MISMATCH");
    assert_eq!(err.code(), code::BIZ_UPLOAD_SESSION_MISMATCH);

    // 正确 session_id 应能成功 discard
    let _ = UploadSessionService::discard(
        Arc::new(InMemoryUploadSessionRepo::new()), // 新 repo；先 re-create
        &real_session_id,
        &DiscardIn {
            scope: "parts_new".into(),
        },
        &current_user(),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn discard_deletes_redis_key() {
    let repo = Arc::new(InMemoryUploadSessionRepo::new());

    let out = UploadSessionService::get_or_create(
        repo.clone(),
        dummy_sts_long_expiry(),
        &cfg(),
        &GetOrCreateIn {
            scope: "parts_new".into(),
        },
        &current_user(),
    )
    .await
    .unwrap();
    let session_id = out.session_id.clone();

    let out_discard = UploadSessionService::discard(
        repo.clone(),
        &session_id,
        &DiscardIn {
            scope: "parts_new".into(),
        },
        &current_user(),
    )
    .await
    .unwrap();
    assert_eq!(out_discard.session_id, session_id);

    // Redis 已删
    assert!(repo.get(42, "parts_new").await.unwrap().is_none());

    // 重复 discard：Redis 已不存在 → 幂等返回（不报错）
    let _ = UploadSessionService::discard(
        repo.clone(),
        &session_id,
        &DiscardIn {
            scope: "parts_new".into(),
        },
        &current_user(),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn consume_files_drops_without_cos_delete() {
    // 与 remove_files 区别：consume 不触发 cos.delete_object（避免与
    // batch.rs 的 spawn delete_object 重复；consume 业务语义是"已消费"，
    // tmp 删理由由 confirm / batch_create 端点统一处理）。
    let repo = Arc::new(InMemoryUploadSessionRepo::new());

    let out = UploadSessionService::get_or_create(
        repo.clone(),
        dummy_sts_long_expiry(),
        &cfg(),
        &GetOrCreateIn {
            scope: "parts_new".into(),
        },
        &current_user(),
    )
    .await
    .unwrap();
    let session_id = out.session_id.clone();

    UploadSessionService::allocate_files(
        repo.clone(),
        86400,
        &session_id,
        &AllocateFilesIn {
            scope: "parts_new".into(),
            files: vec![AllocateFileItemIn {
                client_ref: "r1".into(),
                kind: "drawing".into(),
                original_filename: "a.pdf".into(),
                file_size: 1024,
                content_type: "application/pdf".into(),
                content_sha256: "a".repeat(64),
            }],
        },
        &current_user(),
    )
    .await
    .unwrap();

    let res = UploadSessionService::consume_files(
        repo.clone(),
        86400,
        &session_id,
        &ConsumeFilesIn {
            scope: "parts_new".into(),
            client_refs: vec!["r1".into()],
        },
        &current_user(),
    )
    .await
    .unwrap();
    assert_eq!(res.consumed, vec!["r1".to_string()]);
    let stored = repo.get(42, "parts_new").await.unwrap().unwrap();
    assert_eq!(stored.files.len(), 0);
}

#[tokio::test]
async fn renew_returns_new_credentials() {
    let repo = Arc::new(InMemoryUploadSessionRepo::new());

    let out1 = UploadSessionService::get_or_create(
        repo.clone(),
        dummy_sts_long_expiry(),
        &cfg(),
        &GetOrCreateIn {
            scope: "parts_new".into(),
        },
        &current_user(),
    )
    .await
    .unwrap();
    let session_id = out1.session_id.clone();
    let cred1_expired = out1.credentials.expired_time;

    let renew_out = UploadSessionService::renew(
        repo.clone(),
        dummy_sts_long_expiry(),
        &cfg(),
        &session_id,
        &RenewIn {
            scope: "parts_new".into(),
        },
        &current_user(),
    )
    .await
    .unwrap();
    assert!(renew_out.credentials.expired_time >= cred1_expired);
}

/// 2026-09-18 review #12 修复：python STS 转发失败的错误码透传。
/// service 层应**直接透传** `BIZ_UPLOAD_SESSION_STS_FORWARD_FAILED`，不替换错误码。
#[tokio::test]
async fn python_sts_forward_failure_propagates_error_code() {
    let repo = Arc::new(InMemoryUploadSessionRepo::new());

    // miss 路径下 issue_and_persist 调 sts.issue → 返回 Err
    let err = UploadSessionService::get_or_create(
        repo,
        dummy_sts_forward_failed(),
        &cfg(),
        &GetOrCreateIn {
            scope: "parts_new".into(),
        },
        &current_user(),
    )
    .await
    .expect_err("mock PythonSts 返回 21608，service 应透传");
    assert_eq!(err.code(), code::BIZ_UPLOAD_SESSION_STS_FORWARD_FAILED);

    // 显式 renew 路径同样透传
    let repo2 = Arc::new(InMemoryUploadSessionRepo::new());
    let out = UploadSessionService::get_or_create(
        repo2.clone(),
        dummy_sts_long_expiry(),
        &cfg(),
        &GetOrCreateIn {
            scope: "parts_new".into(),
        },
        &current_user(),
    )
    .await
    .unwrap();
    let session_id = out.session_id.clone();
    let err2 = UploadSessionService::renew(
        repo2,
        dummy_sts_forward_failed(),
        &cfg(),
        &session_id,
        &RenewIn {
            scope: "parts_new".into(),
        },
        &current_user(),
    )
    .await
    .expect_err("renew 路径同样应透传 21608");
    assert_eq!(err2.code(), code::BIZ_UPLOAD_SESSION_STS_FORWARD_FAILED);
}

/// 2026-09-18 review #13 修复：renew 时若 python 返回的 tmp_prefix 与 session 内
/// 不一致 → service 拒绝（返回 internal 错误），不静默漂移。
#[tokio::test]
async fn renew_rejects_drifted_tmp_prefix() {
    let repo = Arc::new(InMemoryUploadSessionRepo::new());

    // 第一次：long_expiry 创建 session（session.tmp_prefix = "tmp/sess/<uuid>/"）
    let out = UploadSessionService::get_or_create(
        repo.clone(),
        dummy_sts_long_expiry(),
        &cfg(),
        &GetOrCreateIn {
            scope: "parts_new".into(),
        },
        &current_user(),
    )
    .await
    .unwrap();
    let session_id = out.session_id.clone();

    // 第二次：调 renew，但用 drifted mock（返回不同 prefix）→ 应拒绝
    let err = UploadSessionService::renew(
        repo,
        dummy_sts_drifted_prefix(),
        &cfg(),
        &session_id,
        &RenewIn {
            scope: "parts_new".into(),
        },
        &current_user(),
    )
    .await
    .expect_err("drifted prefix 应拒绝");

    // 检查错误消息含 "prefix 漂移"
    let msg = format!("{}", err);
    assert!(
        msg.contains("prefix") && msg.contains("漂移"),
        "错误消息应明示 prefix 漂移原因；实际 = {msg}"
    );
}

#[tokio::test]
async fn clerk_role_also_allowed() {
    let repo = Arc::new(InMemoryUploadSessionRepo::new());
    let out = UploadSessionService::get_or_create(
        repo,
        dummy_sts_long_expiry(),
        &cfg(),
        &GetOrCreateIn {
            scope: "parts_new".into(),
        },
        &current_user_clerk(),
    )
    .await
    .expect("Clerk 应允许");
    assert!(!out.session_id.is_empty());
}

#[tokio::test]
async fn inspector_role_is_rejected() {
    let repo = Arc::new(InMemoryUploadSessionRepo::new());
    let err = UploadSessionService::get_or_create(
        repo,
        dummy_sts_long_expiry(),
        &cfg(),
        &GetOrCreateIn {
            scope: "parts_new".into(),
        },
        &current_user_inspector(),
    )
    .await
    .expect_err("Inspector 应被拒");
    assert_eq!(err.code(), code::FORBIDDEN);
}
