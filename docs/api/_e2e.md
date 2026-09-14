# _e2e 域 API（seed hook）

> 本文件须与 `src/modules/_e2e/{handler.rs,dto.rs,mod.rs}` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`../index.md`](../index.md)
>
> 域覆盖：e2e 测试 seed hook（11 端点），供 Playwright spec 匿名灌入 seed 数据。
> 2026-09-14 落地；dev/test profile 默认启用，release profile 必须显式 `E2E_HOOKS_ENABLED=false`。
> 配套迁移：`migrations/20260914100000_022_create_e2e_seeded_table.sql`

---

## 安全约束（必读）

| 维度 | 实现 |
|---|---|
| **不走 `CurrentUser` extractor** | handler 签名不取 `current: CurrentUser`，从而 axum 不会触发 JWT 校验；spec 端 `request.newContext({ baseURL })` 即可调 |
| **二次 guard** | 每个 handler 开头调用 `e2e_guard(&state)?`：仅当 `state.config.enable_e2e_hooks == true` 放行；否则 `404 NOT_FOUND`（不泄漏端点存在性） |
| **生产硬关** | 单一控制点为 env `E2E_HOOKS_ENABLED`。docker compose / dev `cargo run` 走默认（`true`）；prod / staging 必须显式 `false`（ops 责任，不靠编译期二分） |
| **不引入 DELETE/POST 攻击面** | 仅 POST 写 + GET `probe`；reset 仅清 `t_e2e_seeded` 标记行，不删 alembic seed 数据 |

---

## 端点列表（11 个，全部挂在 `/api/v2/_e2e`）

| Method | Path | 权限 | 说明 |
|---|---|---|---|
| POST | `/api/v2/_e2e/probe` | **公开**（受 guard） | 探测端点存在（返回 `{status:"ok",enabled:true}`） |
| POST | `/api/v2/_e2e/reset` | **公开**（受 guard） | 清空 `t_e2e_seeded` 标记行（不动 alembic seed） |
| POST | `/api/v2/_e2e/seed/customer` | **公开**（受 guard） | 灌入 customer（L1 / L2） |
| POST | `/api/v2/_e2e/seed/applicant` | **公开**（受 guard） | 灌入 applicant |
| POST | `/api/v2/_e2e/seed/worker` | **公开**（受 guard） | 灌入 worker（按 work_type_code 绑定工种） |
| POST | `/api/v2/_e2e/seed/part` | **公开**（受 guard） | 灌入 part（PENDING 状态，绑定 customer_id） |
| POST | `/api/v2/_e2e/seed/outsource_company` | **公开**（受 guard） | 灌入外协公司 |
| POST | `/api/v2/_e2e/seed/outsource_quote` | **公开**（受 guard） | 灌入外协报价（DRAFT） |
| POST | `/api/v2/_e2e/seed/delivery_note` | **公开**（受 guard） | 灌入送货单（status 可指定） |
| POST | `/api/v2/_e2e/seed/user` | **公开**（受 guard） | 灌入用户（带 roles，缺省密码 `changeme`） |
| POST | `/api/v2/_e2e/revoke-session` | **公开**（受 guard） | 删除指定 user 全部 Redis session |

---

## `t_e2e_seeded` 元数据表

由 migration 022 创建（2026-09-14）：

```sql
CREATE TABLE t_e2e_seeded (
    entity    VARCHAR(32) NOT NULL,
    entity_id BIGINT      NOT NULL,
    created_at TIMESTAMP  NOT NULL DEFAULT NOW(),
    PRIMARY KEY (entity, entity_id)
);
```

`reset` 端点 `DELETE FROM t_e2e_seeded RETURNING 1`；返回 `cleared` = `rows.len()`。

---

## 共享 DTO

### SeedCreatedResp 字段（所有 seed 端点通用）

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | 新建实体的雪花 ID（JSON string 防 JS 精度截断） |

### ProbeResp 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `status` | string | 固定 `"ok"` |
| `enabled` | bool | 当前 profile 是否启用（与 `E2E_HOOKS_ENABLED` 对齐） |

### ResetResp 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `cleared` | i64 | 本次 reset 清掉的标记行数 |

### SeedCustomerReq 字段

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `name` | string | ✓ | customer 名 |
| `parent_id` | string (i64)? | — | L2 时必填 |
| `serial_prefix` | string? | — | L1 时必填（单大写字母） |

### SeedApplicantReq 字段

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `name` | string | ✓ | applicant 名 |
| `customer_id` | string (i64) | ✓ | 关联 L1 customer |

### SeedWorkerReq 字段

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `name` | string | ✓ | worker 真名 |
| `work_type_code` | string | ✓ | 工种 code（须已存在；否则 20901） |

> `badge_code` 由 server 端生成（`E2E-{6位snowflake末6}`），保证同 test 内多次 seed 不撞唯一索引。

### SeedPartReq 字段

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `serial` | string | ✓ | 工单序列号 |
| `customer_id` | string (i64) | ✓ | 关联 L2 customer |
| `applicant_name` | string | ✓ | 申请人 |
| `name` | string? | — | 缺省 = `applicant_name` |
| `drawing_no` | string? | — | 缺省 = `E2E-DWG-{serial}` |

### SeedOutsourceCompanyReq 字段

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `name` | string | ✓ | 公司名 |

### SeedOutsourceQuoteReq 字段

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `part_id` | string (i64) | ✓ | 关联 part |
| `company_id` | string (i64) | ✓ | 关联外协公司 |
| `process_id` | string (i64) | ✓ | 关联工序 |
| `price` | number | ✓ | 报价单价（> 0） |

### SeedDeliveryNoteReq 字段

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `customer_id` | string (i64) | ✓ | 关联 L2 customer |
| `status` | string? | — | 缺省 `"DRAFT"` |

### SeedUserReq 字段

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `username` | string | ✓ | 用户名（trim + lowercase） |
| `role_codes` | string[] | ✓ | 角色 code（MANAGER / CLERK / INSPECTOR / CNC_PROGRAMMER） |
| `password` | string? | — | 缺省 `"changeme"` |
| `phone` | string? | — | |
| `full_name` | string? | — | 缺省 = username |

### RevokeSessionReq 字段

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `username` | string | ✓ | 目标用户名 |

---

## release profile toggle 说明

### env 配置

```bash
# dev / test profile（默认）
E2E_HOOKS_ENABLED=true

# production / staging profile（必须显式关）
E2E_HOOKS_ENABLED=false
```

### 行为差异

| profile | `_e2e/*` 行为 |
|---|---|
| dev / test | `e2e_guard` 返回 `Ok(())` → handler 正常处理；spec 可匿名调用 |
| release | `e2e_guard` 返回 `AppError::biz(code::NOT_FOUND, "endpoint disabled")` → 404，**不泄漏端点存在性** |

### docker compose 用法

```yaml
services:
  rust-backend:
    environment:
      E2E_HOOKS_ENABLED: ${E2E_HOOKS_ENABLED:-true}  # dev 默认开
```

prod / staging compose 用 `E2E_HOOKS_ENABLED: false` 显式覆盖。

---

## 错误码（受业务端点复用）

| code | 来源 | 触发场景 |
|---|---|---|
| 40000 | `BAD_REQUEST` | 雪花 ID parse 失败 / 缺字段 |
| 20601 | `BIZ_USER_ACCOUNT_NOT_FOUND` | `revoke-session` 的 username 不存在 |
| 20901 | `BIZ_WORK_TYPE_NOT_FOUND` | `seed/worker` 的 `work_type_code` 不存在 |

> 本域不注册独立错误码，全部复用 `shared::error::code` 的通用 / 业务码。

---

## 实现位置

- mod：`src/modules/_e2e/mod.rs::e2e_guard + router()`
- handler：`src/modules/_e2e/handler.rs`（11 个 handler，全走 sqlx 动态 query，不依赖 `.sqlx/` 离线元数据）
- dto：`src/modules/_e2e/dto.rs`
- 路由挂载：`/_e2e`（见 `src/modules/mod.rs::v2_router`）
- 配置：`src/infra/config.rs::E2eConfig::enable_e2e_hooks`（env `E2E_HOOKS_ENABLED`）

---

## e2e 调用模式（Playwright）

```ts
// frontend/e2e tests/helpers/seed.ts（示例）
const SEED_BASE = 'http://localhost:3000/api/v2/_e2e';

export async function seedPart(req: SeedPartReq) {
  const resp = await request.newContext({ baseURL: SEED_BASE });
  return resp.post('/seed/part', { data: req }).then(r => r.json());
}

export async function resetSeed() {
  const resp = await request.newContext({ baseURL: SEED_BASE });
  return resp.post('/reset').then(r => r.json());
}
```

> seed.ts 在 endpoint 404 时降级为 `seed-skip-<name>` 占位 id（兼容 release profile 关闭时 spec 不挂）。

---

## 集成测试

`tests/_e2e_api.rs`（12+ 用例：probe / reset / 各 seed happy / revoke-session / `E2E_HOOKS_ENABLED=false` 时全 404）