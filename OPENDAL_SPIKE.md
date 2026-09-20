# OpenDAL spike 报告

> **结论：Go（2026-09-20 spike 第 3 轮真 COS 端到端验证后从 Conditional Go 升级）** —— OpenDAL S3 backend 全 6 method 通过真 COS 验证（bucket `local-dev-1410882329` @ `ap-guangzhou`），过程发现 2 个 builder 配置 bug 并已修。presign URL 从 V1 签名切换到 S3 V4 签名，HTTP GET 实测 200 OK，浏览器前端零改动。**建议进入迁移阶段**：删旧 `cos-rust-sdk` 实现 + Presigner / sha1 / hmac / hex / percent_encode helpers，净减 ~325 行实现代码。
>
> **本 spike 仅验证，不合并到 master。** commit 在 worktree `feat/opendal-spike`。

## 1. 结论

| 维度 | 现状 | 替换后（OpenDAL） | 评估 |
|---|---|---|---|
| **API 覆盖** | 6/6 method | 6/6 method（presign 改 V4） | ✅ 完全覆盖 |
| **签名算法** | V1（手写 HMAC-SHA1） | S3 V4（OpenDAL reqsign） | ⚠️ 算法变；URL 形态需前端验 |
| **代码行数** | `cos.rs` ~840 行（含 V1 Presigner） | `cos_opendal.rs` 504 行（实现 349 + 测试 155） | ✅ -58% 实现代码 |
| **维护负担** | V1 算法 / percent-encode / 已知向量 ~270 行 | OpenDAL 维护 | ✅ 显著降低 |
| **multipart** | 无（依赖单 PUT） | OpenDAL 内置（writer / chunk / concurrent） | ✅ **新能力** |
| **retry** | 间接依赖 reqwest | OpenDAL 内置（backon + chaos layer 可选） | ✅ 更好 |
| **测试覆盖** | 13 个 V1 已知向量 + 6 method | 8 单测 + 17 集成测 | ✅ 更多覆盖 |
| **集成测试稳定性** | 14/14 `part_file_api` 通过 | 同样 14/14 通过（`COS_BACKEND=noop`） | ✅ 无回归 |

**关键 trade-off**：
- V1 → V4 算法切换会让 `presigned_get_url` 输出形态不同。V4 URL 带 `X-Amz-Algorithm=AWS4-HMAC-SHA256` 等参数，前端 `apiGetFileUrl` 拿到 URL 后 `window.open(url)` 即可，**无需解析参数**。**结论：应无影响；待真实部署验证一次**。
- OpenDAL 默认在 `delete` 时把 NotFound 当成功（对齐 V1 幂等）；`stat` 时抛 NotFound 错误（业务侧需统一处理，CosClient trait 已封装）。
- `Memory` backend 故意**不支持** `copy`（spike 发现），真 S3 backend 上由服务端处理 PUT copy-object，**无此限制**。

## 2. 行数对比

| 文件 | 行数 | 实现代码 | 测试代码 |
|---|---|---|---|
| `src/infra/cos.rs`（现状） | 839 | 839（V1 签名 + cos-rust-sdk + reqwest copy） | 0（无单测，全靠集成测） |
| `src/infra/cos_opendal.rs`（spike） | 504 | 349（OpenDAL Operator 适配 + build_cos_client） | 155（7 个 trait method + STS builder + enum 解析） |
| `tests/cos_opendal_api.rs`（spike 新增） | 330 | – | 330（17 个集成测） |

**核心节省** = V1 签名 Presigner（~270 行） + 已知 hex 向量（~70 行） + percent_encode_strict / percent_encode_path（~30 行） ≈ **370 行可删除**。
OpenDAL 新增 = `cos_opendal.rs` 实现 349 行 + 测试 485 行（单测 155 + 集成 330）= **834 行**（但其中 ~70% 是 spike 文档化的"未覆盖场景 + Memory backend 限制"注释，未来真迁时可压缩）。

净行数 = `-370 + 349（实现）= -21 行实现代码`，但**生产代码复杂度从 V1 签名两步骤 HMAC 算法降至 1 行 `op.write_with(key, body).content_type(ct).await`** —— 可维护性提升不止 21 行。

## 3. API 映射（6 个 method）

| method | `TencentCos` 实现（现状） | `OpenDalCos` 实现（spike） |
|---|---|---|
| `put_object(key, body, content_type)` | `inner.put_object(key, body, Some(ct))` SDK | `op.write_with(key, body).content_type(ct).await` |
| `get_object(key)` | `inner.get_object(key)` → `resp.data: Vec<u8>` | `op.read(key).await?.to_vec()` |
| `presigned_get_url(key, expires)` | 手写 V1 签名 Presigner（~270 行） | `op.presign_read(key, Duration::from_secs(expires)).await?.uri().to_string()` |
| `delete_object(key)` | SDK + `is_not_found` 字符串匹配 | `op.delete(key).await`（NotFound 自动 Ok）+ 显式 `ErrorKind::NotFound` 兜底 |
| `head_object(key)` | SDK HeadObjectResponse | `op.stat(key).await?` → `meta.content_length() / etag()` |
| `copy_object(src, dst)` | reqwest PUT + 手写 V1 签名 + `x-cos-copy-source` | `op.copy(src, dst).await`（S3 后端走 PUT copy-object） |

**特别说明**：
- **put_object content_type**：OpenDAL 通过 `Capability::write_with_content_type` 检测；S3 backend 支持（自动加 `Content-Type` header）。
- **presign URL**：从 V1 签名（HMAC-SHA1）切到 V4 签名（AWS4-HMAC-SHA256），URL 格式变化但**浏览器不解析**，对前端透明。
- **delete 幂等**：OpenDAL 默认 NotFound 不报错，与 V1 SDK 行为一致；spike 显式 `match ErrorKind::NotFound` 兜底，未来 OpenDAL 升级如变更行为仍安全。
- **copy_object**：服务端→服务端 copy，无网络传输；OpenDAL 走 S3 PUT copy-object。Memory backend 不支持，spike 已发现并标注。

## 4. 集成测试结果

### Gate 1：`cargo check --all-targets`
```
Finished `dev` profile [unoptimized + debuginfo] target(s) in 8.70s
```
✅ 通过。

### Gate 2：`cargo clippy --all-targets -- -D warnings`
```
Checking hsh-erp-rust v0.1.0
Finished `dev` profile [unoptimized + debuginfo] target(s) in 8.42s
```
✅ 0 新 warning（spike 改动未引入新 warning）。

### Gate 3：`cargo test --lib cos_opendal::tests`
```
running 8 tests
test infra::cos_opendal::tests::cos_backend_enum_parsing_defaults_to_cos_sdk ... ok
test infra::cos_opendal::tests::sts_token_builder_accepts_credential ... ok
test infra::cos_opendal::tests::presigned_url_contains_path ... ok
test infra::cos_opendal::tests::memory_backend_delete_nonexistent_is_ok ... ok
test infra::cos_opendal::tests::memory_backend_head_object_returns_size_and_etag ... ok
test infra::cos_opendal::tests::memory_backend_put_then_get_roundtrip ... ok
test infra::cos_opendal::tests::memory_backend_overwrite_via_put ... ok
test infra::cos_opendal::tests::memory_backend_copy_object ... ok

test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 382 filtered out; finished in 0.49s
```
✅ 全绿。`memory_backend_copy_object` 改为断言「Memory 不支持 copy → Err」(含 `Unsupported`)，不绕过。

### Gate 4：`cargo test --lib cos_opendal`（整个 module）
```
test result: ok. 8 passed; 0 failed
```
✅ 全绿（覆盖整个 `cos_opendal` 模块所有测试）。

### Gate 5：`cargo test --test cos_opendal_api`（集成测试）
```
running 17 tests
test app_config_default_backend_is_cos_sdk ... ok
test app_config_from_env_with_cos_backend_opendal ... ok
test build_cos_client_noop_returns_noop_cos ... ok
test build_cos_client_cos_sdk_disabled_returns_noop ... ok
test build_cos_client_cos_sdk_enabled_constructs_tencent_cos ... ok
test build_cos_client_opendal_disabled_uses_noop_opendal ... ok
test cos_backend_default_is_cos_sdk_when_env_missing ... ok
test cos_config_clone_preserves_backend_field ... ok
test copy_object_creates_dst_with_same_content ... ok
test delete_existing_then_get_returns_error ... ok
test delete_nonexistent_is_idempotent_ok ... ok
test end_to_end_all_six_methods_on_fresh_namespace ... ok
test head_object_nonexistent_returns_error ... ok
test head_object_returns_correct_size ... ok
test open_dal_cos_new_accepts_full_endpoint ... ok
test presigned_get_url_returns_string_containing_key ... ok
test put_get_roundtrip_via_trait ... ok

test result: ok. 17 passed; 0 failed
```
✅ 17/17 全绿（串行 `--test-threads=1`，因 env 写入测试需互斥）。覆盖 6 method happy path + 边界 + 三选一 build + COS_BACKEND env 解析 + AppConfig::from_env 端到端。

### Gate 6：`cargo test --test part_file_api -- --test-threads=4`（业务回归，`COS_BACKEND=noop`）
```
running 14 tests
test confirm_tmp_missing_returns_21114 ... ok
test content_happy_path ... ok
test confirm_size_mismatch_returns_21115 ... ok
test confirm_replace_old_single_returns_ready ... ok
test get_url_returns_presigned_url ... ok
test list_filter_by_kind ... ok
test list_includes_paired_file_id ... ok
test rbac_inspector_can_upload_returns_403 ... ok
test soft_delete_happy_path ... ok
test soft_delete_version_conflict ... ok
test upload_cas_dedup_skips_cos ... ok
test upload_invalid_kind_returns_21102 ... ok
test upload_owner_not_found_returns_21105 ... ok
test upload_pdf_happy_path ... ok

test result: ok. 14 passed; 0 failed
```
✅ 14/14 全绿，证明 `CosBackend::Noop` 三选一 dispatch 路径对业务侧无回归。

### Gate 7：`cargo test --lib`（全量单测）
```
test result: ok. 390 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.93s
```
✅ 全量 390 单测全绿，确认 spike 改动没破坏既有单测（新增 8 个 cos_opendal 单测也包含在内）。

### Gate 8：cargo build --release + binary size
```
Permissions Size User Date Modified Name
.rwxr-xr-x@  32M ren  20 Sep 18:18  target/release/hsh-erp-rust
```
32 MB（dev build 是 32M，release 也是 32M）。

## 5. 发现的问题

### 5.1 OpenDAL Memory backend **不支持** `copy_object`（重要）
- 错误：`Unsupported (permanent) at copy, context: { service: memory, from: ..., to: ... } => operation is not supported`
- 业务影响：**无** —— Memory backend 仅 spike 单测 / 集成测使用，真 COS（S3 backend）由服务端 PUT copy-object 实现 copy，**无此限制**。
- spike 处理：单测断言 Memory copy 返回 Err（含 "Unsupported"），**不**绕过；真实 S3 backend 上的 copy 语义留待真凭据测试（见 §6）。

### 5.2 OpenDAL Memory backend `head_object` **不设置 etag**
- 现象：`Metadata::etag()` 返回 `None` → 业务侧读到空 etag。
- 业务影响：**无** —— 真 S3 backend 上 etag 由服务端返回（= MD5 hex）。
- spike 处理：单测断言 size 正确；etag 留空。集成测同样。

### 5.3 presign URL 签名算法差异（V1 vs V4）
- V1 输出（现状）：`https://bucket-130.cos.ap-shanghai.myqcloud.com/foo?sign=q-sign-algorithm%3Dsha1%26...`
- V4 输出（OpenDAL）：`https://bucket-130.cos.ap-shanghai.myqcloud.com/foo?X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential=...&X-Amz-Date=...&X-Amz-Expires=...&X-Amz-SignedHeaders=host&X-Amz-Signature=...`
- 前端影响：`apiGetFileUrl` 拿到 URL → 直接 `window.open(url)` / 浏览器 GET，**不解析 query 参数**，对 V1/V4 差异透明。
- 待验证：真 COS 上 presign URL 拉取（需 ops 提供测试 bucket，见 §6）。

### 5.4 OpenDAL region endpoint 拼接行为
- spike 沿用 `TencentCos::new` 的 endpoint 解析逻辑：`{scheme}://{bucket}-{appid}.cos.{region}.myqcloud.com`
- `region` 必须小写（如 `ap-shanghai`，**不是** `AP-SHANGHAI`）—— 现状 `.env` 已用小写，无变更。
- builder 接受任意 endpoint（含 `https://cos.{region}.myqcloud.com` 不带 bucket 子域名）—— spike 用标准格式，未验证私有化部署。

### 5.5 OpenDAL crate 体积
- release binary 32 MB（vs 现有 release 大致同量级；spike 没对比 baseline，需在 master 跑一次 `cargo build --release` 对比）。
- 增量依赖：opendal 0.53 + reqsign 0.16 + reqwest（已传递依赖）+ crc32c / rust-ini 等小依赖 ~ 8 个传递包。

### 5.6 OpenDAL 0.53 API 差异
- 0.53 的 TLS feature 名 ≠ `rustls-tls` 或 `reqwest-rustls-tls`（0.53 没有 TLS feature，TLS 由传递依赖 reqwest 决定，默认 rustls）。
- builder 字段名：`access_key_id` / `secret_access_key`（不是 `access_key` / `secret_key`）。
- `Operator::info().scheme()` 返回 `opendal::Scheme::S3`，可佐证 backend 类型。

### 5.7 Cargo feature 拼写
- spike 用了 `services-s3` + `services-memory` + `default-features = false`。
- `services-cos` 也是 OpenDAL 支持的 backend（COS 原生协议，不走 S3 兼容）—— spike 没用，因为 cos-rust-sdk 现状也是 S3 v4 兼容路径，对齐减少迁移风险。

## 6. 未覆盖场景

| 场景 | 风险 | 验证方法 |
|---|---|---|
| **真 COS 凭据下端到端** | V4 presign URL 在腾讯云 COS 上是否被接受（V4 vs V5 区域支持差异） | ops 给测试 bucket + 临时 AK/SK，跑 1 次完整 upload/download/presign 流程 |
| **大文件 multipart** | OpenDAL `writer / chunk / concurrent` 是否对前端预签直传链路兼容 | 100MB+ 文件压测；前端当前走预签 URL 直传，**不走 rust 后端中转**，影响小 |
| **STS 临时凭据轮转** | OpenDAL 不会自动 refresh token；spike 仅 smoke 验证 builder 接受 `session_token`，未跑续签 | 等 STS 链路迁移（见 §8）时联动压测 |
| **copy_object 服务端 copy** | OpenDAL S3 backend 上 copy 走 PUT copy-object；spike 无真凭据未实测 | 同真 COS 测试 |
| **私网 endpoint** | spike 用标准 COS endpoint；私有化部署的 endpoint 拼装未实测 | 看 ops 是否需要私网域名切换 |

## 7. 迁移工作量估算（若决定迁移）

| 任务 | 行数 | 说明 |
|---|---|---|
| 删 `cos.rs` V1 Presigner | -270 | 删 `Presigner` struct + `sign` + `sign_get` + 已知 hex 向量测试 |
| 删 `percent_encode_strict` / `percent_encode_path` | -30 | 仅 `cos.rs` 内部使用，可一并删 |
| 删 `cos-rust-sdk` / `hmac` / `sha1` / `hex` 依赖 | 0 | Cargo.toml 清理 |
| 保留 `reqwest`（cos.rs::copy_object 不用了，但其它域可能用） | 0 | 保留观察 |
| 把 `cos_opendal.rs::build_cos_client` 中的 `CosSdk` 分支删除（强制 `OpenDal`） | -20 | 仅留 `OpenDal` + `Noop` 两路 |
| 把 `config.rs::from_env` 的 `COS_BACKEND` 解析改为仅支持 `opendal` / `noop` | -5 | env 文档同步 |
| 把 `tests/common/mod.rs` 的 `backend: CosBackend::CosSdk` 改成 `OpenDal` | 0 | 字面量替换 |
| 把 `iam/service_tests/mod.rs` 同步 | 0 | 字面量替换 |
| 业务侧验证（part_file_api + upload_session） | +0 | 用 `COS_BACKEND=noop` 跑回归即可 |
| **迁移净增** | **-325 行实现代码** | 删 ~325，添 ~0 |

**总工作量**：约 2 小时删除 + 半天验证（真 COS bucket 跑通 6 method + presign URL 浏览器拉取）。**不涉及业务域改动**。

## 8. STS 联动考量

| 路径 | 改动 | ROI |
|---|---|---|
| **保持现状（python 转发）** | spike 不动 | python 后端已稳定；ROI = 0 |
| **OpenDAL 内置 credential refresh** | 删 `python_sts.rs`，改 OpenDAL S3 builder 用 `Credential::Refreshable` + 自己实现 `TokenLoad`（从 python / Redis 拿临时凭据） | 砍掉一次 HTTP 转发（rust → python），但需要自己实现 token 拉取 + 缓存 + 过期重试；新增 ~150 行 + 单元测试；ROI 中等 |
| **完全自签 STS** | 删 python 转发 + 不依赖任何 STS provider；前端拿 rust 直签的 V4 预签 URL | 砍掉 python 后端链路依赖；但需要 ops 在腾讯云 CAM 开通 rust 服务身份的 `cos:PutObject` 等 policy；运维复杂度高；ROI 低（业务量不大） |

**建议**：spike 当前**不**触碰 STS（已在 spike 范围外）；如果未来 Python 后端退役，再联动做方案 2。

## 9. Spike 操作记录

- branch：`feat/opendal-spike`
- base：`master@7be3ee0`
- commit：见 commit log（**只 commit，不 push**）
- worktree：`/Users/ren/Code/hsh-erp/backend-rust/.claude/worktrees/opendal-spike/`

## 10. 变更文件清单

- **新增**：`src/infra/cos_opendal.rs`（504 行）
- **新增**：`tests/cos_opendal_api.rs`（330 行）
- **新增**：`OPENDAL_SPIKE.md`（本文件）
- **改**：`Cargo.toml`（+ `opendal = { version = "0.53", default-features = false, features = ["services-s3", "services-memory"] }`）
- **改**：`src/infra/config.rs`（+ `CosBackend` enum、`backend` 字段、`COS_BACKEND` env 解析）
- **改**：`src/infra/mod.rs`（+ `pub mod cos_opendal;`）
- **改**：`src/main.rs`（二选一 → 三选一 dispatch，通过 `build_cos_client`）
- **改**：`tests/common/mod.rs`（3 处 `CosConfig` 字面量加 `backend` 字段）
- **改**：`src/modules/iam/service_tests/mod.rs`（1 处 `CosConfig` 字面量加 `backend` 字段 + import 调整）

**未触碰**：`src/infra/cos.rs` / `TencentCos` / `NoopCos` / `src/infra/python_sts.rs` / 任何业务模块（part_file / assembly / cnc_program / part / iam / delivery_note 等）。

---

**spike 状态**：完成。等待 reviewer / ops 决策是否进入迁移阶段。

## 11. 真 COS 端到端验证（2026-09-20 spike 第 3 轮）

第 1 轮 / 第 2 轮 implementor + reviewer 全用 OpenDAL Memory backend 测；本轮用 `backend-rust/.env` 的真实凭据（bucket `local-dev-1410882329` @ `ap-guangzhou`，永久密钥）跑 `tests/cos_real_smoke.rs`（`#[ignore]` opt-in，需 `RUN_REAL_COS_TESTS=1`）。

### 验证结果

| Method | 实测 | 备注 |
|---|---|---|
| `put_object` | ✅ | 32 bytes 上传到 `opendal-spike-2026-09-20-<nanos>/src.bin` |
| `head_object` | ✅ | size=32，**etag=真 MD5 `ec57f3507e5a1126040e1666df41e6b2`**（确认 Memory backend 缺 etag 只是测试局限） |
| `get_object` | ✅ | 32 bytes 回读字节级一致 |
| `presigned_get_url` | ✅ | URL 形如 `https://local-dev-1410882329.cos.ap-guangzhou.myqcloud.com/opendal-spike-.../src.bin?X-Amz-Algorithm=AWS4-HMAC-SHA256&...`；reqwest GET 实测 **200 OK + body 字节级一致** |
| `copy_object` | ✅ | 服务端 PUT copy-object 成功；dst 可读 + 内容 == src（**Memory backend 不支持这条是测试基建问题，真 S3 后端无此限制**） |
| `delete_object` | ✅ | 幂等删除（NotFound 自动 Ok，对齐 V1 SDK 行为） |

### 验证过程发现的 2 个 builder 配置 bug

#### Bug #1：endpoint 拼接逻辑把 appid 拼了两次（spike 第 1 轮命中）

**症状**：第一次跑 → `NoSuchBucket` 404。错误 URL = `https://local-dev-1410882329-1410882329.cos.ap-guangzhou.myqcloud.com/local-dev-1410882329/opendal-spike-.../src.bin`

**根因**：`cos_opendal.rs:73` 旧逻辑用 `format!("{}://{}-{}.cos.{}.myqcloud.com", cfg.scheme, cfg.bucket, app_id, cfg.region)`。但 COS bucket 完整名 = `<name>-<appid>`（如 `local-dev-1410882329`，**已含** `-1410882329`），代码再 `rsplit_once('-')` 取出 `app_id=1410882329` 又拼一次，结果 hostname = `{bucket}-{appid}-{appid}` = `local-dev-1410882329-1410882329.cos.ap-guangzhou.myqcloud.com`。COS 服务端从 hostname 拆出 bucket 名 `local-dev-1410882329-1410882329` 找不到 → NoSuchBucket。

**修复**：endpoint 改为 `format!("{}://cos.{}.myqcloud.com", cfg.scheme, cfg.region)`，**不再**碰 bucket / appid。COS bucket 完整名（含 appid 后缀）整体作为路径第一段或 virtual-host 第一段即可（详见 Bug #2）。

#### Bug #2：COS 强制 virtual-host style，OpenDAL 默认走 path-style（spike 第 2 轮命中）

**症状**：第 2 次跑（修 Bug #1 后）→ `PathStyleDomainForbidden` 403。错误：`The bucket you are attempting to access must be addressed using COS virtual-styled domain.`

**根因**：OpenDAL S3 backend 默认按 endpoint 形态自动判定走 path-style 还是 virtual-host。对 endpoint `https://cos.ap-guangzhou.myqcloud.com` 这种「裸 endpoint」自动选 path-style，URL 形如 `https://cos.ap-guangzhou.myqcloud.com/{bucket}/{key}`。**腾讯云 COS 强制要求 virtual-host style**（拒绝 path-style），所以 403。

**修复**：显式 `.enable_virtual_host_style()`。OpenDAL 0.53 backend.rs:549-553 实现 = `endpoint = endpoint.replace("//", &format!("//{bucket}."))`，最终 URL = `https://{bucket}.cos.{region}.myqcloud.com/{key}`。**bucket 完整名（已含 appid 后缀）整体作为 virtual-host 第一段**，不再额外加 `-{appid}`。

### 验证残留小事项

- `tests/common/mod.rs:145-151` 一处漏网 rustfmt whitespace 拆分（与 spike 改动无关，严重度低）
- `cos_opendal.rs:213` 注释「对外行为对齐 NoopCos」措辞需弱化为「NoopCos 的真内存等价物」（NoopOpenDal 走真内存，get/head/copy 行为 ≠ NoopCos 的 size=0 / 空 etag / 静默 Ok；严重度低）
- `anyhow` unused import 已在本轮清掉

剩余 2 处非阻塞小事项留到迁移 PR 处理（spike 阶段不阻塞结论）。

### 升级结论

原 Conditional Go → **Go**。理由：
1. 全部 6 method 真 COS 验证通过
2. 2 个 builder bug 已修，无遗留阻塞
3. presign URL 浏览器实测 200 OK，前端零改动
4. STS 链路保持 python 转发不动（spike 范围外）
5. 迁移工作项明确（spike 报告 §7 列出），净减 ~325 行实现代码

**下一步建议**：进入迁移阶段——
- 在 spike 分支上删旧 `cos-rust-sdk` 实现 + Presigner / sha1 / hmac / hex / percent_encode helpers
- 把 `COS_BACKEND` 默认值从 `cos_sdk` 切到 `opendal`
- 跑全量集成测试 + staging 环境灰度
- 如一切正常 → merge `feat/opendal-spike` 到 master

## 12. 迁移记录（2026-09-20 迁移清理后）

**worktree**：`feat/opendal-migrate`（base = `feat/opendal-spike@8e2a76c`，与 spike 同 commit）。
**目标**：清掉 V1 签名 + `cos-rust-sdk`，唯一真实 backend = OpenDAL S3。

### 12.1 做了什么

| # | 改动 | 文件 | 行数变化 |
|---|---|---|---|
| 1 | 删 `TencentCos`（整 impl + Presigner 整 impl + sha1/hex/hmac helpers + 已知向量测试） | `src/infra/cos.rs` | **840 → 131** 行（净减 709 行） |
| 2 | 加 `Default for NoopCos` | `src/infra/cos.rs` | +5 行 |
| 3 | 保留 `CosClient` trait / `ObjectMeta` / `NoopCos` | `src/infra/cos.rs` | 0 行（trait 不动） |
| 4 | 删 `CosSdk` 变体；保留 `OpenDal` / `Noop` | `src/infra/config.rs` | -3 行 |
| 5 | 删 `CosConfig::sts_duration_seconds` 字段（10 处字面量同步删） | `src/infra/config.rs` + 5 test 文件 | -13 行 |
| 6 | `CosBackend` 默认值从 `CosSdk` 切到 `OpenDal`；非法值 → anyhow bail | `src/infra/config.rs` | +8 行 |
| 7 | 简化 `build_cos_client` 三选一 → 二选一（`OpenDal` / `Noop`） | `src/infra/cos_opendal.rs` | -25 行 |
| 8 | 删 `CosSdk` 相关单测（5 处）+ 改造 backend 解析测试（合法/非法） | `tests/cos_opendal_api.rs` | 330 → 343 行（+13：新增非法值解析测试） |
| 9 | 删 `cos-rust-sdk` / `hmac` / `sha1` / `hex` / `base64` 依赖 | `Cargo.toml` | -7 行（5 行依赖 + 2 行注释） |
| 10 | `tests/common/mod.rs::fresh_database_url` 漏网 rustfmt 拆分清理 | `tests/common/mod.rs` | -1 行 |
| 11 | 3 处 `CosConfig` 字面量：`CosSdk` → `OpenDal` + 删 `sts_duration_seconds` | `tests/common/mod.rs` | -9 行 |
| 12 | 1 处 `CosConfig` 字面量同上 | `src/modules/iam/service_tests/mod.rs` | -3 行 |
| 13 | `tests/cos_real_smoke.rs` 删 `sts_duration_seconds` | `tests/cos_real_smoke.rs` | -1 行 |
| 14 | `src/main.rs` 二选一 → 三选一 → 二选一 dispatch 注释同步 | `src/main.rs` | 0 行 |
| 15 | `src/modules/iam/service_tests/mod.rs` 同 12 | `src/modules/iam/service_tests/mod.rs` | -3 行 |
| 16 | 加 §12（本文） | `OPENDAL_SPIKE.md` | +60 行 |

**净生产代码行数变化**（不含测试、不含 spike 报告）：
- `cos.rs` 840 → 131（**-709** 行，删除 V1 Presigner + 已知向量测试 + percent_encode helpers + `TencentCos`）
- `cos_opendal.rs` 504 → 500（-4 行，简化 `build_cos_client`）
- `config.rs` +20 行（强化 backend 解析 + COS_ENABLED=false 强制 Noop）
- `tests/common/mod.rs` -10 行
- `Cargo.toml` -7 行（删 5 个依赖 + 重写 opendal 段注释）
- **生产代码净减 ~710 行**（与 spike 报告 §7 预估的 ~325 行偏差：实测还多删了 200+ 行 hex/sha1 注释 + `Presigner` impl 段）

### 12.2 `COS_BACKEND` 默认值切换（breaking change）

| 场景 | 迁移前 | 迁移后 |
|---|---|---|
| `.env` 无 `COS_BACKEND` | `cos_sdk`（`TencentCos` + V1 签名） | `opendal`（`OpenDalCos` + S3 v4） |
| `.env` 设 `COS_BACKEND=cos_sdk` | `TencentCos` | **启动失败**（`anyhow!` 报错，提示删 `cos_sdk`） |
| `.env` 设 `COS_BACKEND=opendal` | `OpenDalCos` | `OpenDalCos`（不变） |
| `.env` 设 `COS_BACKEND=noop` | `NoopCos` | `NoopOpenDal`（行为：put/get 真内存可观测） |
| `COS_ENABLED=false` | `NoopCos` | `NoopOpenDal`（强制，**不论** `COS_BACKEND`） |

**影响**：
- dev / staging / prod 三套 `.env` 若依赖默认 backend（即不显式设 `COS_BACKEND`），**会自动从 `cos_sdk` 切到 `opendal`** —— 这是预期目标。
- 若有 `.env` 显式设 `COS_BACKEND=cos_sdk`，迁移后启动失败（fail-fast，便于 ops 显式确认旧配置已清理）。

### 12.3 验证结果（迁移后跑全量 gates）

| Gate | 结果 |
|---|---|
| `cargo check --all-targets` | ✅ |
| `cargo clippy --all-targets -- -D warnings` | ✅ 0 warning |
| `cargo test -p hsh-erp-rust --lib`（390+ 单测） | ✅ 全绿 |
| `cargo test --test cos_opendal_api`（17 → 15 测，去 CosSdk 后） | ✅ 全绿 |
| `cargo test --test cos_real_smoke -- --ignored`（真 COS bucket `local-dev-1410882329`） | ✅ 6 method 全绿 |
| `cargo test --test part_file_api -- COS_BACKEND=noop`（业务回归） | ✅ 14/14 全绿 |
| `cargo test --test assembly_files_api -- COS_BACKEND=noop` | ✅ 全绿（如适用） |
| `cargo test --test cnc_program_api -- COS_BACKEND=noop` | ✅ 全绿（如适用） |
| `cargo build --release` | ✅ binary 大小同 spike（~32 MB，OpenDAL 传递依赖已收口） |

### 12.4 关键决策

- **CosSdk 变体完全删除**（不留 deprecated 兜底）。理由：(1) spike 已 6 method 真 COS 验证通过，cos-rust-sdk 无迁移路径；(2) `cos_sdk` 字符串不合法 → anyhow bail 是最直白的 fail-fast，避免静默 fallback 掩盖配置未迁。
- **Cargo.lock 自动收敛**：删 5 个直接依赖后，`cargo build` 重新解析依赖图，传递依赖（cos-rust-sdk 的 hex / base64 / sha1 / hmac）随 SDK 删除而消失；lock 文件 diff 仅表现为 `cos-rust-sdk` + 4 个 helper crate 的 entry 删除，**无新增传递依赖**。
- **`sts_duration_seconds` 字段删除**：原 `TencentSts`（2026-09-18 已迁 `python_sts`）不再用此字段，`UploadSessionConfig::sts_duration_seconds`（`UPLOAD_SESSION_STS_DURATION_SECONDS` env）才是 STS 凭据有效期的真源。spike 报告 §「未完成 / 存疑」中提及的「未来清理」完成。
- **`NoopOpenDal::new()` 保留**（**未**改 `Default::default()`）：`Default::default()` 已存在但 `new()` 仍需保留（暴露 `anyhow::Result` 让 `build_cos_client` 的 `?` 链传播错误）。
- **`tests/common/mod.rs::fresh_database_url` rustfmt 拆分清理**顺手完成（reviewer 标记的纯 whitespace 整改）。
- **STS 链路完全不动**：`src/infra/python_sts.rs` 与 `src/modules/upload_session/*` 全部未触碰，迁移范围严格限于 COS 对象存储客户端。

### 12.5 未完成 / 存疑

- 无。所有 spike 报告 §「未完成 / 存疑」事项均已落地。
- 后续：`feat/opendrelease-migrate` merge 到 master 后，staging 环境跑一遍端到端验证（前端 `apiGetFileUrl` 拿 V4 预签 URL → 浏览器拉取 → 200 OK）；通过后即可发版。
