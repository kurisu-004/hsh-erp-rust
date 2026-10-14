//! 应用错误类型 + 错误码常量段
//! 对应 Python myERP/core/error_code.py + exception*.py
//!
//! ## 错误码分段契约（与 Python 前端保持兼容）
//! - `0`          成功
//! - `4xxxx`      HTTP 语义（40000 BAD_REQUEST、40001 VALIDATION、40100 UNAUTHORIZED、
//!   40101 BIZ_AUTH_INVALID、40102 TOKEN_EXPIRED、40103 REFRESH_INVALID、40104 OLD_PASSWORD_MISMATCH、
//!   40105 SESSION_REVOKED、40106 BIZ_WX_LOGIN_FAILED、40107 BIZ_WX_NOT_BOUND、
//!   40108 BIZ_WX_BINDING_DUPLICATE、40109 BIZ_WX_NOT_CONFIGURED、
//!   40110 BIZ_WX_USER_ALREADY_BOUND、
//!   40300 FORBIDDEN、40301 SHELF_MISMATCH、40400 NOT_FOUND、40800 REQUEST_TIMEOUT、
//!   40901 VERSION_CONFLICT、41301 REQUEST_TOO_LARGE）
//!   Auth 业务码与通用 UNAUTHORIZED 的区别：业务码携带细分原因。
//! - `5xxxx`      系统错误（50000 INTERNAL、50001 DATABASE）
//! - `2xxxx`      业务域错误：200xx 用户/订单、201xx 零件/客户、202xx 工人、203xx 装配体、
//!   204xx 图纸文件、205xx 货架、206xx 账号、208xx 工序、209xx 工种、210xx 申请人、
//!   211xx 零件文件、212xx 外协公司、213xx 外协报价、214xx 送货单、215xx 外协发货、
//!   217xx 交期汇总邮件（2026-10-13 新增；216xx 上传会话域 2026-09-28 下线后**未复用**，
//!   理由见 `code` 模块内 217xx 段的注释）。
//!
//!   错误码段位归属（2026-09-19 prod 聚合后）：
//!   - `prod::worker` / `prod::queue` → `202xx` 工人段（共享；queue 的 `WORKER_POOL_EMPTY`/`NO_WORK_TYPE` 等复用 `20205`/`20206`）
//!   - `prod::process_chain` → `207xx` 工艺链段
//!   - `prod::process` → `208xx` 工序段
//!   - `prod::work_type` → `209xx` 工种段
//!
//! ### 与 Python 的差异（冲突解决记录）
//! - `20109` 在 Python 中被 `BIZ_PART_BATCH_NOT_FOUND` 与 `BIZ_CUSTOMER_IN_USE` 双重占用。
//!   Rust 中 `20109` 保留给 `BIZ_PART_BATCH_NOT_FOUND`，`BIZ_CUSTOMER_IN_USE` 移到新槽位 `20113`。
//! - `21110 BIZ_DELIVERY_PARTS_MULTIPLE_CUSTOMERS` 与 `21407 BIZ_DELIVERY_NOTE_PARTS_MULTIPLE_CUSTOMERS` 同义。
//!   Rust 以 `21407` 为正典，`21110` 保留为 `#[deprecated]` 别名（编译期 = 21407），便于未迁移的调用点平滑收敛。
//!
//! 业务实现阶段直接 `AppError::biz(code::BIZ_..., "...")`；常量在 `code` 模块内集中维护。

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use thiserror::Error;

/// 错误码常量（沿用 Python 数字契约）
pub mod code {
    pub const SUCCESS: i32 = 0;

    // HTTP 语义
    pub const BAD_REQUEST: i32 = 40000;
    pub const VALIDATION_ERROR: i32 = 40001;
    pub const UNAUTHORIZED: i32 = 40100;

    // Auth 域业务码（401xx，HTTP 语义层里的业务码，区别于通用 40100 UNAUTHORIZED）
    pub const BIZ_AUTH_INVALID: i32 = 40101; // 登录：用户不存在/已删/已停用/密码错统一
    pub const TOKEN_EXPIRED: i32 = 40102;
    pub const REFRESH_INVALID: i32 = 40103; // refresh token 失效/版本不匹配/用户停用
    pub const OLD_PASSWORD_MISMATCH: i32 = 40104; // 修改密码时旧密码错误
    pub const SESSION_REVOKED: i32 = 40105; // 服务端 Redis session 不存在（已 logout/改密/吊销）
    // 2026-09-29 新增：企业微信小程序登录（2026-10-11 起 URL 为
    // `/api/v2/wx/login/wecom`）4 个码。
    //
    // 槽位选择：40105 之后顺延（401xx = 鉴权语义层，与 iam 登录失败同族）。
    // 语义边界：40106/40109 是「登录请求本身失败」；40107/40108 是「绑定关系
    // 状态异常」，其中 40108 仅在 admin 绑定端点出现。
    //
    // 40106 = 企微侧 `jscode2session` 失败（code 失效 / 过期 / 换取 access_token
    // 失败重试后仍失败 / 企微返回未识别 errcode / 网络层失败）。**不重试**
    //   （code 一次性，重试无意义且会打爆企微频控）。
    // 40107 = 该 userid 未在 `t_wx_identity` 预绑定（仅预绑定，不自动开户），
    //   或企微返回的 corpid 与本地 `WECOM_CORPID` 不符（防跨企业串号）。
    // 40108 = 同一 `(corp_id, wx_user_id)` 已绑到**另一个** `t_user.id`
    //   （admin 绑定端点；绑到同一个 user_id 是幂等成功，不返此码）。
    // 40109 = 后端未配置企业微信凭据（`corpid` / `corpsecret` 留空 → enabled=false，
    //   `NoopWeComClient` 占位），HTTP 503 表示「服务未就绪」而非「调用方无权」。
    // 40110 = system → wx 方向的一对一冲突：某 `t_user.id` 已绑了**另一个**
    //   企业微信 userid（2026-10-10 新增，admin 绑定端点；同 userid 重复绑是幂等成功，
    //   不返此码）。与 40108 的方向相反：40108 是「这个 userid 已经名花有主」。
    pub const BIZ_WX_LOGIN_FAILED: i32 = 40106;
    pub const BIZ_WX_NOT_BOUND: i32 = 40107;
    pub const BIZ_WX_BINDING_DUPLICATE: i32 = 40108;
    pub const BIZ_WX_NOT_CONFIGURED: i32 = 40109;
    pub const BIZ_WX_USER_ALREADY_BOUND: i32 = 40110;

    pub const FORBIDDEN: i32 = 40300;

    // 货架越权
    pub const SHELF_MISMATCH: i32 = 40301;

    pub const NOT_FOUND: i32 = 40400;
    pub const VERSION_CONFLICT: i32 = 40901;
    // 2026-10-03 新增：HTTP 408 REQUEST_TIMEOUT 的错误码（Rust 侧独有，Python 端无对应值）。
    // 触发场景：`middleware::timeout` 的请求级超时到点。**必须带标准 `{code,message,data}`
    // 信封而不是空 body**——前端打印请求是 `responseType: 'blob'`，blob 错误分支会把
    // body 当文本读出来 JSON.parse；空 body 直接 parse 失败，用户只能看到一句泛化
    // 网络错误，丢失「请求超时」这个可读原因。
    // 槽位选择：4xxxx 段 HTTP 语义层，前缀 408 对齐 HTTP 状态码（既有码同样按状态码
    // 取前缀：40000/40100/40300/40400/40901/41301），未被占用。
    pub const REQUEST_TIMEOUT: i32 = 40800;
    pub const REQUEST_TOO_LARGE: i32 = 41301;

    // ===== 2xxxx 业务域错误（沿用 Python core/error_code.py 数值契约） =====

    // 200xx 用户 / 订单
    pub const BIZ_USER_NOT_FOUND: i32 = 20001;
    pub const BIZ_USER_DUPLICATE: i32 = 20002;
    pub const BIZ_ORDER_NOT_FOUND: i32 = 20003;

    // 201xx 零件 / 客户 / 序列号
    pub const BIZ_PART_NOT_FOUND: i32 = 20101;
    pub const BIZ_CUSTOMER_NOT_FOUND: i32 = 20102;
    pub const BIZ_INVALID_TRANSITION: i32 = 20103;
    pub const BIZ_INVALID_VALUE: i32 = 20104;
    pub const BIZ_PART_SERIAL_EXHAUSTED: i32 = 20105; // 序列号池耗尽（>5000 活跃/PREFIX）
    pub const BIZ_SERIAL_PREFIX_UNKNOWN: i32 = 20108; // t_serial_counter 找不到对应 prefix
    pub const BIZ_PART_BATCH_NOT_FOUND: i32 = 20109; // 批次不存在 / 不属于该工单（Python 中此值曾被双重占用，详见模块 docstring）
    pub const BIZ_PART_PRICE_LOCKED_BY_ASSEMBLY: i32 = 20110; // 父装配体已设总价，子件不能再单独改价
    pub const BIZ_PART_BATCH_INVALID_QUANTITY: i32 = 20111; // 拆分/部分流转数量非法（≤0 或超过批次量）
    pub const BIZ_PART_QUANTITY_LOCKED: i32 = 20112; // 已拆分或已流转的工单禁止改总量
    pub const BIZ_CUSTOMER_IN_USE: i32 = 20113; // 客户仍被 part/assembly 引用 → 拒软删（Python 原 20109，新槽位独占）
    pub const BIZ_PART_BATCH_NOT_HELD_BY_WORKER: i32 = 20114; // worker-scan 找不到当前 worker 持有的批次
    pub const BIZ_PART_ALREADY_CANCELLED: i32 = 20115; // 零件已 CANCELLED，cancel/deliver/complete/start-repair 一律返回此码（status guard）
    pub const BIZ_PART_NOT_DELIVERED: i32 = 20116; // complete 操作要求零件当前为 DELIVERED
    pub const BIZ_PART_NOT_READY_TO_SHIP: i32 = 20117; // deliver 操作要求零件当前为 READY_TO_SHIP
    pub const BIZ_PART_REPAIR_NOT_TRIGGERED: i32 = 20118; // start-repair 操作要求零件当前为 IN_PROCESS
    pub const BIZ_PART_NOT_DELETABLE: i32 = 20119; // 零件处于终态 (DELIVERED/COMPLETED) 或已挂送货单，禁 soft-delete
    // 2026-09-29 新增：prod 域批次操作的状态 / 存在性错误码。批次下发
    // （`prod::batch` dispatch）与工人池 move（`POST /api/v2/prod/pool/move`）
    // 两个入口共用这一对码位。
    // - 20120 BIZ_BATCH_INVALID_STATUS：批次当前 status 不允许该操作；白名单由各
    //   调用点自定 —— dispatch 为待下发白名单（2026-10-06：含已废弃的
    //   `PROGRAMMING`，与待下发列表同链路），`prod/pool/move` 为 `IN_PROCESS`
    // - 20121 BIZ_BATCH_NOT_FOUND：按 batch_id 查不到该批次（不存在或已软删；
    //   与 20109 BIZ_PART_BATCH_NOT_FOUND 语义近似但属 prod 域独立槽位，方便前端
    //   按 code 区分场景）
    pub const BIZ_BATCH_INVALID_STATUS: i32 = 20120;
    pub const BIZ_BATCH_NOT_FOUND: i32 = 20121;
    // 2026-09-30 新增：prod/pool/move 路径专用错误码（prod 域工人池 9 端点重构）。
    // 触发场景：caller 在 `POST /api/v2/prod/pool/move` 给的 `from.{kind, holder_id}`
    // 与 batch 当前 `(location, current_holder_id)` 不一致（如 from=POOL 但 batch
    // 实际在 WORKER 持有）。槽位选择：20122 顺延 201xx 段（parts/batch 段），HTTP 409
    // 由 status_from_code 表驱动映射。
    pub const BIZ_BATCH_LOCATION_MISMATCH: i32 = 20122;
    // 2026-09-30 新增：part 域 MANAGER 单角色强推工单为 COMPLETED 端点（force-complete）。
    // 触发场景：`POST /api/v2/prod/parts/{part_id}/force-complete`（端点已硬切到
    // `/prod` 前缀，代码仍在 part 域）撞 part 已是
    // COMPLETED（不可重复强推）。槽位选择：20123 顺延 201xx 段（parts 段），
    // HTTP 409 由 status_from_code 表驱动映射。语义与 `BIZ_PART_ALREADY_CANCELLED=20115`
    // 对称（CAN/COMPLETED 互为幂等拒绝码）。
    pub const BIZ_PART_ALREADY_COMPLETED: i32 = 20123;
    // 2026-10-17 新增：客户层级从固定两层放开为**任意深度**后出现的软删闸门码。
    // 触发场景：`POST /api/v2/com/customers/{id}/soft-delete` 删一个有未软删子节点的
    // 客户。旧两层实现**不**查子节点（「L1 仍有 L2」不报错，子节点由前端显式 cascade），
    // 任意深度下那套隐式约定已经不成立（子节点可能挂在第 5 层，且单据挂叶子，
    // 级联删除会连带软删已被单据引用的节点）⇒ 改为服务端显式拒绝。
    // 与 `BIZ_CUSTOMER_IN_USE`(20113) 的分工：那一条判「**业务单据**引用」（part /
    // assembly），本条判「**客户树**引用」（子节点），两者独立、任一命中即拒软删。
    // 槽位选择：20124 顺延 201xx 段最后一个（20123 是 parts 段的 force-complete）。
    // HTTP 400（与 `BIZ_INVALID_VALUE` 一致）：请求本身合法，是「按这个客户现在的
    // 状态不能删」——调用方改请求也改不动，只能改数据，故不是 409。
    pub const BIZ_CUSTOMER_HAS_CHILDREN: i32 = 20124;

    // 202xx 工人
    pub const BIZ_WORKER_NOT_FOUND: i32 = 20201;
    pub const BIZ_WORKER_INACTIVE: i32 = 20202;
    pub const BIZ_WORKER_IN_USE: i32 = 20203; // 还有 part.current_holder_id 指向 → 拒停用
    pub const BIZ_WORKER_HOLD_LIMIT_EXCEEDED: i32 = 20204; // 工种 max_held_batches 上限触顶 → 拒领取
    pub const BIZ_WORKER_POOL_EMPTY: i32 = 20205; // refill 时工序池无候选
    pub const BIZ_WORKER_NO_WORK_TYPE: i32 = 20206; // worker.work_type_id IS NULL

    // 203xx 装配体
    pub const BIZ_ASSEMBLY_NOT_FOUND: i32 = 20301;
    // ⚠️ 2026-10-17 起**零调用方**，按仓内先例（`20118`）保留常量登记备查：
    // 客户树放开任意深度后，「装配件只能挂在叶子客户」改由
    // `shared::customer::require_leaf_customer` 统一判定，错误码改为
    // `20104 BIZ_INVALID_VALUE`（旧的 20302 语义「一级集团 / 不存在」在深树上已不成立
    // —— 建根不再是错误，只有「还有子节点」才是）。
    pub const BIZ_ASSEMBLY_BAD_CUSTOMER: i32 = 20302;
    pub const BIZ_ASSEMBLY_TOO_MANY_CHILDREN: i32 = 20303; // 子件 > 99，序列号 {serial}-{i:02d} 派生失败
    pub const BIZ_ASSEMBLY_PDF_INVALID: i32 = 20305; // multipart PDF 页数与 children.len()+1 不匹配 / lopdf 解析失败
    pub const BIZ_ASSEMBLY_CHILD_PRICE_LOCKED: i32 = 20306; // 父装配体已设总价时禁止子件改价（预留）
    pub const BIZ_ASSEMBLY_HAS_SHIPMENT: i32 = 20307; // 装配体已挂送货单，禁止 soft_delete（预留）
    pub const BIZ_CUSTOMER_NO_SERIAL_PREFIX: i32 = 20308; // L1 客户的 serial_prefix 为空（无法派发序列号）
    // 2026-10-11 新增：装配件级 MANAGER 单角色强制完成端点
    // （`POST /api/v2/prod/assemblies/{assembly_id}/force-complete`）的两个幂等拒绝码。
    // 触发场景：装配件已是 COMPLETED / CANCELLED 时再调该端点。两个码位**顺延 20309 /
    // 20310** 而非接着 `BIZ_ASSEMBLY_HAS_SHIPMENT` 排 —— 20308 已被
    // `BIZ_CUSTOMER_NO_SERIAL_PREFIX` 占用，`constants_have_unique_values` 会因重复值
    // 直接 panic。HTTP 409 由 `status_from_code` 表驱动映射，语义与
    // `BIZ_PART_ALREADY_COMPLETED=20123` / `BIZ_PART_ALREADY_CANCELLED=20115` 对称。
    pub const BIZ_ASSEMBLY_ALREADY_COMPLETED: i32 = 20309;
    pub const BIZ_ASSEMBLY_ALREADY_CANCELLED: i32 = 20310;

    // 204xx 图纸文件（对象存储 + COS）
    //
    // ⚠️ 2026-10-13 注：`t_drawing_file` 表随
    // `migrations/20261013000000_001_drop_legacy_file_tables.sql` 一并 DROP，但下面
    // **这 4 个 DRAWING_FILE 码位（20401–20404）保留不删**（理由紧跟 20404 之后）；
    // ⚠️ 同段的 20406 / 20407（STS / 打印转发失败）是**活跃码**，不在此列。
    pub const BIZ_DRAWING_FILE_NOT_FOUND: i32 = 20401;
    pub const BIZ_DRAWING_FILE_BAD_TYPE: i32 = 20402; // 扩展名不在 COS_ALLOWED_TYPES 白名单
    pub const BIZ_DRAWING_FILE_TOO_LARGE: i32 = 20403; // 文件大小 ≤0 或 > cos_max_file_size_bytes
    pub const BIZ_DRAWING_UPLOAD_FAILED: i32 = 20404; // COS SDK 抛错
    // ⚠️ 保留码位的理由（2026-10-13）：上面 4 个 DRAWING_FILE 码位随 `t_drawing_file`
    // 表一起失去调用方，但仍保留 —— 它们是 COS 上传/读取错误的历史分段位，Python 端旧
    // 前端仍可能读到；code 值是对外契约，删掉会让 code 值跳变。
    // 下方 20406 / 20407 是活跃码，不属于这一列。
    // 2026-09-28 新增：rust → python 薄壳鉴权转发 STS 失败。
    // 触发场景：`POST /api/v2/files/sts-tmp-keys` 鉴权通过后调
    // `HttpPyBackend::forward_sts_tmp_keys` 失败（网络层超时 / 连接拒 / 读取 body 失败）。
    // HTTP 502 BAD_GATEWAY；语义与 nginx upstream 失败同形（"上传相关资源失败"）。
    // 槽位选择：204xx 图纸文件/上传段，与 `BIZ_DRAWING_UPLOAD_FAILED=20404` 同形；20406 与上1下20405 同段连续。
    pub const BIZ_STS_FORWARD_FAILED: i32 = 20406;
    // 2026-10-03 新增：rust → python 薄壳鉴权转发**打印**失败。
    // 触发场景：打印链路 4 个端点鉴权通过后转发到 Python 端执行，失败（网络层超时 /
    // 连接拒 / 读取 body 失败）。HTTP 502 BAD_GATEWAY，语义与 nginx upstream 失败
    // 同形（"上游 python 后端不可达 / 读 body 失败"）。
    // 与 `BIZ_STS_FORWARD_FAILED` 同形但**独立命名**：打印覆盖 4 个端点（送货单
    // print / print-labels、零件 print-drawing / print-drawing-batch），不只 STS 一条；
    // 独立码让前端与日志能区分是哪条转发链路挂了。
    // 槽位选择：204xx 图纸文件 / 上传段，20406 之后顺延（20405 已刻意跳过）。
    pub const BIZ_PRINT_FORWARD_FAILED: i32 = 20407;

    // 205xx 货架（t_shelf）
    pub const BIZ_SHELF_NOT_FOUND: i32 = 20501;
    pub const BIZ_SHELF_DUPLICATE_CODE: i32 = 20502;
    pub const BIZ_SHELF_IN_USE: i32 = 20503; // 还有 IN_PROCESS/INSPECTION 零件 → 拒软删
    pub const BIZ_SHELF_PROCESS_SHELF_NOT_FOUND: i32 = 20504; // 货架不存在
    pub const BIZ_SHELF_PROCESS_PROCESS_NOT_FOUND: i32 = 20505; // 工序不存在
    pub const BIZ_SHELF_NO_MATCH_FOR_PROCESS: i32 = 20506; // 没有 active 货架映射指定 process
    pub const BIZ_SHELF_PROCESS_NOT_MAPPED: i32 = 20507; // 货架未映射该工序
    // 2026-09-29 新增：prod/batch 下发（dispatch）阶段——按 target_process_id 在
    // t_shelf_process 找不到任何 active 货架映射。dispatch 路径不要求 picker
    // 风格（用户传 bookId 自己挑 shelf），但服务端兜底要给出明确错误码。
    pub const BIZ_SHELF_PROCESS_NOT_FOUND: i32 = 20508;
    // to-inspection 一键送检新增
    pub const BIZ_SHELF_NOT_INSPECTION_ZONE: i32 = 20511; // target_inspection_shelf.zone ≠ 'INSPECTION'
    pub const BIZ_SHELF_INACTIVE: i32 = 20512; // target_inspection_shelf.is_active = false
    // ⚠️ 2026-10-10：`BIZ_SHELF_PROCESS_NOT_MAPPED`(20507) 已**全仓零触发点** —— 最后一个
    // 服务对象（queue move WORKER→POOL 的映射校验）随目标货架自动选架退场，「架必须映射
    // 该工序」改由选架 SQL 的候选集承担。常量与码名暂留（历史行与已发布契约），勿新增使用。

    // 206xx 账号（t_user / t_user_role）—— Python 命名 BIZ_* 长名为正典
    pub const BIZ_USER_ACCOUNT_NOT_FOUND: i32 = 20601;
    pub const BIZ_USER_DUPLICATE_USERNAME: i32 = 20602;
    pub const BIZ_USER_INACTIVE: i32 = 20603;
    pub const BIZ_USER_ROLE_DUPLICATE: i32 = 20604;
    pub const BIZ_USER_ROLE_NOT_FOUND: i32 = 20605;
    pub const BIZ_USER_NO_ROLE: i32 = 20606;
    // 既有代码用的短别名（保留兼容，值与上面长名相等）
    pub const USER_NOT_FOUND: i32 = BIZ_USER_ACCOUNT_NOT_FOUND;
    pub const DUPLICATE_USERNAME: i32 = BIZ_USER_DUPLICATE_USERNAME;
    pub const ROLE_DUPLICATE: i32 = BIZ_USER_ROLE_DUPLICATE;
    pub const ROLE_NOT_FOUND: i32 = BIZ_USER_ROLE_NOT_FOUND;
    pub const NO_ROLE: i32 = BIZ_USER_NO_ROLE;

    // 208xx 工序（t_process）
    pub const BIZ_PROCESS_NOT_FOUND: i32 = 20801;
    pub const BIZ_PROCESS_DUPLICATE_CODE: i32 = 20802;
    pub const BIZ_PROCESS_IN_USE: i32 = 20803; // 仍有 part.next_process_id 或 mapping 引用时拒软删

    // 209xx 工种（t_work_type）
    pub const BIZ_WORK_TYPE_NOT_FOUND: i32 = 20901;
    pub const BIZ_WORK_TYPE_DUPLICATE_CODE: i32 = 20902;
    pub const BIZ_WORK_TYPE_IN_USE: i32 = 20903; // 仍有 worker.work_type_id 或 mapping 引用时拒软删
    pub const BIZ_WORK_TYPE_MAX_HELD_NOT_SET: i32 = 20904; // work_type.max_held_batches IS NULL
    // worker-pool refill_for_worker 新增（Task 7）
    pub const BIZ_WORK_TYPE_NO_PROCESS_MAPPING: i32 = 20905; // work_type_id 在 t_work_type_process 没工序映射

    // 210xx 申请人（t_applicant）
    pub const BIZ_APPLICANT_NOT_FOUND: i32 = 21001;
    pub const BIZ_APPLICANT_DUPLICATE_NAME: i32 = 21002; // 同一一级客户下重名（DB partial unique 兜底）
    pub const BIZ_APPLICANT_BAD_CUSTOMER: i32 = 21003; // customer 不存在或不是一级
    pub const BIZ_APPLICANT_IN_USE: i32 = 21004; // 被 part.applicant_name 引用 → 拒软删

    // 211xx 零件文件（t_part_file，统一 5 类；另含 4 个送货单打印模板码 21109 / 21111 /
    // 21112 / 21113 —— ⚠️ 21109 自 2026-10-16 起改由 **Rust 侧自产**，其余三个仍是
    // python 端产出、rust 原样透传，rust 侧注册仅为识别 / 回显）
    pub const BIZ_PART_FILE_NOT_FOUND: i32 = 21101;
    pub const BIZ_PART_FILE_BAD_TYPE: i32 = 21102; // 扩展名与 kind 不匹配
    pub const BIZ_PART_FILE_TOO_LARGE: i32 = 21103; // 文件大小 ≤0 或 > cos_max_file_size_bytes
    pub const BIZ_PART_FILE_UPLOAD_FAILED: i32 = 21104; // COS SDK 抛错
    pub const BIZ_PART_FILE_OWNER_NOT_FOUND: i32 = 21105; // polymorphic owner (part/assembly/customer) 不存在
    pub const BIZ_PART_FILE_DUPLICATE: i32 = 21108; // 同 part+kind+content_sha256 撞唯一索引（并发兜底）
    // 2026-10-16：**改由 Rust 侧自产** —— `GET /api/v2/print-templates/resolve`
    // 查不到该一级客户名下未软删的 `kind='DELIVERY_TEMPLATE'` 行时抛出（HTTP **422**，
    // 见 `status_from_code`）。
    // ⚠️ 该码在 wire 上**未变**（仍是 21109，仍在 python 时代的错误码表里），变的只是
    // 「谁产出它」：从前端转发透传（2026-10-03 打印链路改纯转发后 Rust 侧零生产点）
    // 变成 `print_template` 域的正常业务分支。**HTTP 状态码是本次新选的 422**
    // （python 时代无 Rust 侧先例可循，取「请求本身合法、但这个客户的配置不满足
    // 前提」这层语义，与 `VALIDATION_ERROR` 同档），登记在
    // `docs/api/print_template.md` §8.3。
    pub const BIZ_DELIVERY_TEMPLATE_NOT_CONFIGURED: i32 = 21109; // 该一级客户未绑定送货单打印模板
    // 2026-10-03：打印端点改纯转发后本码无 rust 生产点 —— 由 python 端判定并产出、rust 原样透传，
    // rust 侧无零件状态校验。保留常量供 rust 识别 / 回显 python 产出的码。
    pub const BIZ_DELIVERY_PART_STATUS_INVALID: i32 = 21111; // python 侧：所选零件状态非 READY_TO_SHIP
    // 2026-10-03：同上，纯 python 端产出、rust 原样透传，rust 侧无模板容量概念。
    pub const BIZ_DELIVERY_TEMPLATE_TOO_MANY_PARTS: i32 = 21112; // python 侧：所选零件超过模板容量（法 14 / 路 25）
    pub const BIZ_DELIVERY_PRINT_BAD_ORDER: i32 = 21113; // custom_order 含非法 batch id 或漏行（422）
    // 2026-09-16 M2-A 新增：前端直传 COS 链路错误码。
    // 槽位选择说明：
    // - 21114/21115 复用 211xx「零件文件」语义（tmp_key = `t_part_file.tmp_key`）。
    // 槽位不与 21109/21110/21111/21112/21113 冲突（这些已被送货模板占用）。
    // 2026-09-28 删除：原 BIZ_STS_ISSUE_FAILED=21116（无业务调用方，相关 STS 会话域下线后
    // 同步释放码段；与 Python 错误码表的"STS 失败"对齐责任由 STS 签发链路实端承担）。
    pub const BIZ_PART_FILE_TMP_OBJECT_MISSING: i32 = 21114; // tmp_key 在 COS 不存在 / size=0（前端直传未成功落地）
    pub const BIZ_PART_FILE_SIZE_MISMATCH: i32 = 21115; // tmp_key size 与 client 声明的 size_bytes 不一致
    // 2026-10-13 PR2：`files::part_file::preview`（GET /files/objects/{id}/preview）三条。
    // 槽位 21116 是 2026-09-28 随 STS 会话域下线释放的（原 BIZ_STS_ISSUE_FAILED），本轮复用。
    pub const BIZ_PART_FILE_PREVIEW_UNSUPPORTED: i32 = 21116; // 该文件不做预览（kind 非图纸类 / file_type ≠ PDF）→ HTTP 400
    pub const BIZ_PART_FILE_PREVIEW_FAILED: i32 = 21117; // CI job 终态 Failed → HTTP 502
    pub const BIZ_PART_FILE_PREVIEW_FORWARD_FAILED: i32 = 21118; // 打 python / CI 转发失败（网络层 + NoopPyBackend 占位）→ HTTP 502
    // 21110 → 21407 的 deprecated 别名：旧调用点收敛后移除（详见模块 docstring）
    #[deprecated(
        since = "0.1.0",
        note = "改用 BIZ_DELIVERY_NOTE_PARTS_MULTIPLE_CUSTOMERS (21407)；21110 是旧别名，已统一到 21407"
    )]
    pub const BIZ_DELIVERY_PARTS_MULTIPLE_CUSTOMERS: i32 =
        BIZ_DELIVERY_NOTE_PARTS_MULTIPLE_CUSTOMERS;

    // 212xx 外协公司（t_outsource_company + t_outsource_company_process）
    pub const BIZ_OUTSOURCE_COMPANY_NOT_FOUND: i32 = 21201;
    pub const BIZ_OUTSOURCE_COMPANY_DUPLICATE: i32 = 21202; // uk_t_outsource_company_name 部分唯一兜底
    pub const BIZ_OUTSOURCE_COMPANY_BAD_PROCESS: i32 = 21203; // 工序不存在 / 不是 OUTSOURCE/INHOUSE 类别
    pub const BIZ_OUTSOURCE_PROCESS_NOT_MAPPED: i32 = 21204; // 公司未映射该 OUTSOURCE 工序
    pub const BIZ_OUTSOURCE_COMPANY_IN_USE: i32 = 21205; // 被 part OUTSOURCE 引用 / 仍映射工序
    pub const BIZ_PART_NOT_OUTSOURCEABLE: i32 = 21206; // 当前状态不允许发送外协（兜底，正常流不该撞）
    pub const BIZ_OUTSOURCE_DIRECT_REQUIRES_C2_SHELF: i32 = 21207; // 直接发送外协要求零件位于绑定了外协工序的货架
    pub const BIZ_OUTSOURCE_NO_SHELF: i32 = 21208; // 系统无任何绑定了外协工序的货架
    pub const BIZ_OUTSOURCE_COMPANY_DUPLICATE_NAME: i32 = 21214; // DB uk_t_outsource_company_name 兜底（pre-check 漏网 → INSERT 撞唯一索引）；与 21202 同义 409，区分 DB 兜底 vs 应用层预检

    // 213xx 外协报价（t_outsource_quote）
    pub const BIZ_OUTSOURCE_QUOTE_NOT_FOUND: i32 = 21301;
    pub const BIZ_OUTSOURCE_QUOTE_INVALID_TRANSITION: i32 = 21302; // 当前状态不允许此操作
    pub const BIZ_OUTSOURCE_QUOTE_DUPLICATE: i32 = 21303; // 同 (part,company,process) 已存在活跃报价
    // 2026-10-03 review 第 3 轮放宽语义：不是「找不到该 tuple 的 APPROVED 报价」，
    // 而是「这不是可用的审批价来源」——报价非 APPROVED / 是 DIRECT 占位价
    // （is_direct=true）/ DIRECT 占位价并发建行后回查失败。三个成因同码。
    // HTTP 未登记进 `status_from_code` 的任何特例，落 2xxxx 兜底 = 400。
    pub const BIZ_OUTSOURCE_QUOTE_NOT_APPROVED: i32 = 21307; // 报价不可用作审批价来源（非 APPROVED / DIRECT 占位价 / 并发回查失败）

    // 214xx 送货单（t_delivery_note）
    pub const BIZ_DELIVERY_NOTE_NOT_FOUND: i32 = 21401; // 找不到指定的送货单
    pub const BIZ_DELIVERY_NOTE_INVALID_TRANSITION: i32 = 21402; // 当前状态不允许此操作
    /// 非 DRAFT 状态：不能 soft_delete / attach-batches。
    /// 注意：update 路径用 BIZ_INVALID_TRANSITION（21402）而非此码。
    pub const BIZ_DELIVERY_NOTE_NOT_DRAFT: i32 = 21403;
    pub const BIZ_DELIVERY_NOTE_NOT_SUBMITTED: i32 = 21404; // 非 SUBMITTED 状态不能 recall / pickup
    pub const BIZ_DELIVERY_NOTE_PART_NOT_READY: i32 = 21405; // 零件状态非 READY_TO_SHIP
    pub const BIZ_DELIVERY_NOTE_PART_ALREADY_ASSIGNED: i32 = 21406; // 零件已在另一张送货单上（提升为 409：状态冲突）
    pub const BIZ_DELIVERY_NOTE_PARTS_MULTIPLE_CUSTOMERS: i32 = 21407; // 同一单内混客户（与老 21110 同义）
    pub const BIZ_DELIVERY_NOTE_SCAN_MISMATCH: i32 = 21408; // 扫码的 serial_no 不在本单范围内
    pub const BIZ_DELIVERY_NOTE_DRIVER_INVALID: i32 = 21409; // 司机非送货司机 / 不活跃
    pub const BIZ_DELIVERY_NOTE_SCAN_INCOMPLETE: i32 = 21410; // pickup 时还没扫齐
    pub const BIZ_DELIVERY_NOTE_INVALID_VALUE: i32 = 21411; // 空单 / 等其他非法入参
    pub const BIZ_DELIVERY_NOTE_PARTS_LOCKED: i32 = 21412; // SUBMITTED/PICKED_UP 后禁止 add_parts / remove_batches
    pub const BIZ_DELIVERY_GROUP_NOT_FOUND: i32 = 21413; // 找不到指定的送货分组 / 已软删
    pub const BIZ_DELIVERY_GROUP_DUPLICATE_NAME: i32 = 21414; // 同 L1 下分组重名
    pub const BIZ_DELIVERY_GROUP_MEMBER_CONFLICT: i32 = 21415; // 该客户已属于其他活跃分组（任意深度的后代）
    pub const BIZ_DELIVERY_NOTE_SCOPE_MISMATCH: i32 = 21416; // 零件分类与送货单范围不符（add_parts 校验）
    pub const BIZ_DELIVERY_SCAN_UNKNOWN_CODE: i32 = 21417; // 扫码的 serial_no 无法识别
    pub const BIZ_DELIVERY_ASSEMBLY_PARTS_NOT_READY: i32 = 21418; // 装配件整套拒绝：含不可入单子件（message 附明细）
    pub const BIZ_DELIVERY_NOTE_DRAFT_SCOPE_CONFLICT: i32 = 21419; // recall 时同范围已存在 DRAFT
    pub const BIZ_DELIVERY_NOTE_LOCKED_PART: i32 = 21420; // 零件已锁定在另一张非 DRAFT 的送货单上 → 拒 cancel
    /// batch 当前状态不允许本次操作，两个调用点：
    /// - scan（`delivery_note::service::scan`）：C 组状态短路
    ///   （DELIVERED / OUTSOURCE / IN_PROCESS 工人持有 / COMPLETED / CANCELLED）
    /// - submit（`delivery_note::service::lifecycle`）：已挂单批次状态不在
    ///   `{INSPECTION, READY_TO_SHIP}`（挂单后被旁路改状态的数据非法场景）
    pub const BIZ_DELIVERY_BATCH_STATE_INVALID: i32 = 21421;

    // 215xx 外协发货（t_outsource_shipment，2026-09-13 Phase 2 扩展）
    pub const BIZ_OUTSOURCE_SHIPMENT_NOT_FOUND: i32 = 21501;
    pub const BIZ_OUTSOURCE_SHIPMENT_INVALID_TRANSITION: i32 = 21502; // shipment.status 不允许本次操作
    pub const BIZ_OUTSOURCE_SHIPMENT_NO_OPEN: i32 = 21503; // 找不到开口 OUTSOURCING shipment
    pub const BIZ_OUTSOURCE_SHIPMENT_QUANTITY_EXCEEDS: i32 = 21504; // 接收数量超过开口 shipment.quantity

    // 207xx 工艺链（t_part_process_chain + t_process_chain_step）
    pub const BIZ_PROCESS_CHAIN_NOT_FOUND: i32 = 20701;
    pub const BIZ_PROCESS_CHAIN_STEP_NOT_FOUND: i32 = 20702;
    // 20703 / 20704 随 TIME / fill_ratio 移除释放（2026-10-15）：
    // 两码此前分别为「工种工时上限未配」与「自动分配比例非法」，已随
    // auto-allocate 的 TIME 模式 / 比例入参一并删除；当前无生产方、**未复用**
    // （保留空位防撞码）。
    // 2026-09-16 新增（FK 翻转 PR-1）：part.status 非 PENDING 时禁止制定/修改工艺链
    pub const BIZ_PROCESS_CHAIN_PART_NOT_PENDING: i32 = 20705;
    // 2026-09-16 新增（PR-3 批次 step 化）：批次进入生产流（place-on-shelf /
    // release-from-programming / 外协收回时 part 还未制定工艺链 → 拒
    pub const BIZ_PROCESS_CHAIN_REQUIRED: i32 = 20706;

    // 2026-09-28 删除：216xx 相关会话域错误码（域整体下线，码段释放）。

    // 217xx 交期汇总邮件（mail_digest 域，2026-10-13 新增）
    //
    // ⚠️ **为什么不是 215xx**：任务书原定 215xx，但该段在 2026-09-13 已被
    // `outsource::shipment` 占用（`BIZ_OUTSOURCE_SHIPMENT_*` = 21501..21504，
    // 至今在用）。`216xx`（上传会话）已于 2026-09-28 随域下线**释放**，但复用
    // 一个「已用过又被回收」的段位有风险：老客户端 / 旧日志里可能还残留 216xx 的
    // 映射，撞码比撞段更隐蔽。故取**全新段 217xx**，与 CLAUDE.md §3
    // 「新增域错误码先入对应段、每域一段」的约定一致。
    //
    // 21701 = 未配置 SMTP。⚠️ **2026-10-16 语义已改**：`MAIL_SMTP_*` env 整轮删除，
    //   判据从「`MailConfig::is_enabled()`」换成「`t_mail_digest_config.sender_account_id`
    //   为 NULL / 指向已软删邮箱 / 授权码解密失败」。语义仍是「服务未就绪」→ 503
    //   （与 `BIZ_WX_NOT_CONFIGURED` 40109 同款），不是「调用方无权」。
    // 21702 = lettre SMTP 层报错（连接 / 认证 / 拒收 / 握手）。⚠️ 触达本码时
    //   `t_mail_digest_log` **已经落了一行** `status='FAILED'` + error_message，
    //   信息不会因为返错而丢失。
    // 21703 = 邮箱地址格式非法，或含 CR / LF（SMTP header 注入面，见
    //   `mail_digest::service::validate_email`）。⚠️ 只用于**收件人**；
    //   发信登录账号的同类校验走 21704（语义不同，混用会让前端文案串台）。
    // 21704 = 配置组合非法：WEEKLY 缺 weekday、MONTHLY 缺 day_of_month、
    //   未知 visible_columns 列名、两节都关闭、`upcoming_days` 越界、
    //   以及 scheduled_task 域的邮箱名称 / 预设 / 登录账号 / 授权码校验失败。
    // 21705 = 配置行不存在**且不可自动初始化**的语义保留位（当前
    //   `GET /config` 在表空时返回默认值 + `version=0`，不返本码）。
    //
    // 2026-10-16 新增 scheduled_task 域（邮箱实体）三条：
    // ⚠️ **复用 217xx 段而不是新开一段**：它们与 mail_digest 共用同一段前缀，
    //   两域是同一条业务链（通道 → 内容），前端按 `code >= 21701 && code <= 21708`
    //   归类也没必要 —— 前端本来就是按**具体码**分支的。段位已在 `docs/api/mail.md`
    //   与 `docs/api/mail_digest.md` 两处登记。
    // 21706 = 邮箱不存在**或已软删**（HTTP 404）。⚠️ 指向已软删邮箱与指向不存在的
    //   id 得到同一个码：调用方（发信路径）无法也不需要区分，两者都是「发不出信」。
    // 21707 = 邮箱名称重复（HTTP 409，**不是** 40901 —— 40901 只表达 OCC，
    //   业务唯一键冲突须有专用码，见 CLAUDE.md §8）。
    // 21708 = 邮箱被 `t_mail_digest_config.sender_account_id` 引用，禁止删除
    //   （HTTP 409）。同上一条：这是「状态冲突」而非「入参非法」。
    pub const BIZ_MAIL_NOT_CONFIGURED: i32 = 21701;
    pub const BIZ_MAIL_SEND_FAILED: i32 = 21702;
    pub const BIZ_MAIL_RECIPIENT_INVALID: i32 = 21703;
    pub const BIZ_MAIL_CONFIG_INVALID: i32 = 21704;
    pub const BIZ_MAIL_CONFIG_NOT_FOUND: i32 = 21705;
    pub const BIZ_MAIL_ACCOUNT_NOT_FOUND: i32 = 21706;
    pub const BIZ_MAIL_ACCOUNT_NAME_DUPLICATE: i32 = 21707;
    pub const BIZ_MAIL_ACCOUNT_IN_USE: i32 = 21708;

    // 系统错误
    pub const INTERNAL: i32 = 50000;
    pub const DATABASE: i32 = 50001;
}

#[derive(Debug, Error)]
pub enum AppError {
    /// 业务错误：自定义错误码 + 自定义 HTTP 状态码
    #[error("[{code}] {message}")]
    Biz {
        code: i32,
        message: String,
        http: StatusCode,
    },

    /// 业务错误（带失败明细列表，用于装配件整套拒绝 21418 等）。
    ///
    /// `IntoResponse` 会把 `failures` 序列化进 `R::err.data` 字段，结构：
    /// ```jsonc
    /// { "failures": [ {"serial_no": "...", "name": "...", "reason": "..."}, ... ] }
    /// ```
    /// `message` 仍按既有约定继续携带人读文案（设计 §5：「message 含全部失败子件」）。
    ///
    /// `failures` 用 `serde_json::Value` 而非具体 DTO 是为了**避免** `shared::error`
    /// 反向依赖具体业务域的 DTO 类型。
    #[error("[{code}] {message}")]
    BizWithFailures {
        code: i32,
        message: String,
        http: StatusCode,
        failures: Vec<serde_json::Value>,
    },

    /// 校验失败（40001，HTTP 422）
    #[error("校验失败: {0}")]
    Validation(String),

    /// 未授权（40100，HTTP 401）
    #[error("未授权: {0}")]
    Unauthorized(String),

    /// 禁止访问（40300，HTTP 403）
    #[error("禁止访问")]
    Forbidden,

    /// JWT 错误（40100，HTTP 401）
    #[error("JWT: {0}")]
    Jwt(String),

    /// 数据库错误（50001，HTTP 500）
    #[error("数据库错误")]
    Database(#[from] sqlx::Error),

    /// 内部错误（50000，HTTP 500）
    #[error("内部错误: {0}")]
    Internal(String),
}

impl AppError {
    /// 构造业务错误（HTTP 状态码由 code 自动推导）
    pub fn biz(code: i32, message: impl Into<String>) -> Self {
        Self::Biz {
            code,
            message: message.into(),
            http: status_from_code(code),
        }
    }

    /// 构造业务错误并显式指定 HTTP 状态码（绕过 `status_from_code` 自动推导）
    ///
    /// 何时使用：
    /// - 错误码不在 `status_from_code` 表里，需要指定非常规 HTTP 状态
    /// - 想覆盖表里的默认值（如 `BIZ_DELIVERY_PRINT_BAD_ORDER` 强制 422）
    ///
    /// 表驱动场景优先用 [`AppError::biz`]。
    pub fn biz_with_status(code: i32, message: impl Into<String>, http_status: StatusCode) -> Self {
        Self::Biz {
            code,
            message: message.into(),
            http: http_status,
        }
    }

    pub fn validation(msg: impl Into<String>) -> Self {
        Self::Validation(msg.into())
    }

    pub fn unauthorized(msg: impl Into<String>) -> Self {
        Self::Unauthorized(msg.into())
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        Self::Internal(msg.into())
    }

    pub fn code(&self) -> i32 {
        match self {
            Self::Biz { code, .. } | Self::BizWithFailures { code, .. } => *code,
            Self::Validation(_) => code::VALIDATION_ERROR,
            Self::Unauthorized(_) => code::UNAUTHORIZED,
            Self::Forbidden => code::FORBIDDEN,
            Self::Jwt(_) => code::UNAUTHORIZED,
            Self::Database(_) => code::DATABASE,
            Self::Internal(_) => code::INTERNAL,
        }
    }

    pub fn http_status(&self) -> StatusCode {
        match self {
            Self::Biz { http, .. } | Self::BizWithFailures { http, .. } => *http,
            Self::Validation(_) => StatusCode::UNPROCESSABLE_ENTITY,
            Self::Unauthorized(_) | Self::Jwt(_) => StatusCode::UNAUTHORIZED,
            Self::Forbidden => StatusCode::FORBIDDEN,
            Self::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

/// 由错误码推导 HTTP 状态码（与 Python 错误码分段一致）
fn status_from_code(c: i32) -> StatusCode {
    match c {
        c if c == code::BAD_REQUEST => StatusCode::BAD_REQUEST,
        c if c == code::VALIDATION_ERROR => StatusCode::UNPROCESSABLE_ENTITY,
        c if c == code::UNAUTHORIZED => StatusCode::UNAUTHORIZED,
        c if c == code::TOKEN_EXPIRED => StatusCode::UNAUTHORIZED,
        c if c == code::BIZ_AUTH_INVALID
            || c == code::REFRESH_INVALID
            || c == code::OLD_PASSWORD_MISMATCH
            || c == code::SESSION_REVOKED
            || c == code::BIZ_WX_LOGIN_FAILED =>
        {
            StatusCode::UNAUTHORIZED
        }
        // 2026-09-29 新增（企业微信登录）：40107 未绑定 → 403（身份已认证但无权访问
        // 任何系统账号，是授权决策失败而非认证失败）；40109 未配置 → 503（服务未就绪）。
        // ⚠️ 必须显式登记：下面 `(40000..50000) => BAD_REQUEST` 的兜底会把它们变成 400。
        c if c == code::BIZ_WX_NOT_BOUND => StatusCode::FORBIDDEN,
        c if c == code::BIZ_WX_NOT_CONFIGURED => StatusCode::SERVICE_UNAVAILABLE,
        c if c == code::BIZ_WX_BINDING_DUPLICATE || c == code::BIZ_WX_USER_ALREADY_BOUND => {
            StatusCode::CONFLICT
        }
        c if c == code::SHELF_MISMATCH => StatusCode::FORBIDDEN,
        c if c == code::USER_NOT_FOUND || c == code::ROLE_NOT_FOUND => StatusCode::NOT_FOUND,
        c if c == code::DUPLICATE_USERNAME || c == code::ROLE_DUPLICATE => StatusCode::CONFLICT,
        c if c == code::NO_ROLE => StatusCode::FORBIDDEN,
        c if c == code::FORBIDDEN => StatusCode::FORBIDDEN,
        c if c == code::NOT_FOUND => StatusCode::NOT_FOUND,
        c if c == code::VERSION_CONFLICT => StatusCode::CONFLICT,
        // 2026-10-03 新增：请求级超时（`middleware::timeout`）→ 408。必须显式登记：
        // 下面 `(40000..50000) => BAD_REQUEST` 的兜底会把它变成 400，前端就无法区分
        // 「超时」与「参数错」。
        c if c == code::REQUEST_TIMEOUT => StatusCode::REQUEST_TIMEOUT,
        c if c == code::REQUEST_TOO_LARGE => StatusCode::PAYLOAD_TOO_LARGE,
        c if c == code::INTERNAL || c == code::DATABASE => StatusCode::INTERNAL_SERVER_ERROR,

        // ---- 2xxxx 业务码：404 (资源缺失) ----
        c if c == code::BIZ_USER_NOT_FOUND
            || c == code::BIZ_ORDER_NOT_FOUND
            || c == code::BIZ_PART_NOT_FOUND
            || c == code::BIZ_CUSTOMER_NOT_FOUND
            || c == code::BIZ_PART_BATCH_NOT_FOUND
            || c == code::BIZ_WORKER_NOT_FOUND
            || c == code::BIZ_ASSEMBLY_NOT_FOUND
            || c == code::BIZ_DRAWING_FILE_NOT_FOUND
            || c == code::BIZ_SHELF_NOT_FOUND
            || c == code::BIZ_SHELF_PROCESS_SHELF_NOT_FOUND
            || c == code::BIZ_SHELF_PROCESS_PROCESS_NOT_FOUND
            || c == code::BIZ_SHELF_PROCESS_NOT_FOUND
            || c == code::BIZ_PROCESS_NOT_FOUND
            || c == code::BIZ_WORK_TYPE_NOT_FOUND
            || c == code::BIZ_APPLICANT_NOT_FOUND
            || c == code::BIZ_PART_FILE_NOT_FOUND
            || c == code::BIZ_PART_FILE_OWNER_NOT_FOUND
            || c == code::BIZ_OUTSOURCE_COMPANY_NOT_FOUND
            || c == code::BIZ_OUTSOURCE_QUOTE_NOT_FOUND
            || c == code::BIZ_DELIVERY_NOTE_NOT_FOUND
            || c == code::BIZ_DELIVERY_GROUP_NOT_FOUND
            || c == code::BIZ_DELIVERY_SCAN_UNKNOWN_CODE
            || c == code::BIZ_OUTSOURCE_SHIPMENT_NOT_FOUND
            || c == code::BIZ_PROCESS_CHAIN_NOT_FOUND
            || c == code::BIZ_PROCESS_CHAIN_STEP_NOT_FOUND
            || c == code::BIZ_BATCH_NOT_FOUND
            || c == code::BIZ_MAIL_CONFIG_NOT_FOUND
            // 2026-10-16 新增（scheduled_task 域）：邮箱不存在**或已软删**。
            // 与 `BIZ_MAIL_CONFIG_NOT_FOUND` 同为「引用一个已经不在的实体」。
            || c == code::BIZ_MAIL_ACCOUNT_NOT_FOUND =>
        {
            StatusCode::NOT_FOUND
        }

        // 2026-10-16 新增（`print_template` 域）：`GET /print-templates/resolve`
        // 查不到该一级客户名下未软删的 `kind='DELIVERY_TEMPLATE'` 行。
        // ⚠️ **必须是 422、且必须显式登记**：本码原先在 HTTP_TABLE_CASES 里显式
        // 映射成 400（python 转发时代的形态），下面 `(20000..30000) => BAD_REQUEST`
        // 的兜底也会把它变成 400。取 422 的理由：请求本身完全合法（客户 id 存在、
        // 是一级客户、没有越权），**不满足的是「这个客户的配置」**，与
        // `VALIDATION_ERROR`（40001 = 422）同档 —— 前端据此走「提示去模板管理页
        // 绑定 / 或让用户手动上传兜底」，而不是「你请求参数错了」。
        c if c == code::BIZ_DELIVERY_TEMPLATE_NOT_CONFIGURED => StatusCode::UNPROCESSABLE_ENTITY,

        // 2026-10-13 新增（mail_digest 域）：
        // - 21701 未配置 SMTP → 503「服务未就绪」，与 `BIZ_WX_NOT_CONFIGURED`
        //   (40109) 同款；**不是** 403/400（调用方没做错任何事，是运维没配）。
        // - 21702 发信失败 → 502 Bad Gateway（SMTP 是外部依赖，它挂了；
        //   本服务的入参校验全过了，失败发生在下游）。⚠️ 必须显式登记：
        //   下面 `(20000..30000) => BAD_REQUEST` 的兜底会把它变成 400，
        //   前端就会把「SMTP 挂了」显示成「你填错了」——两者排查方向完全相反。
        // - 21703 邮箱非法 / 21704 配置组合非法 → 400（纯入参问题，含 CR/LF 注入）。
        c if c == code::BIZ_MAIL_NOT_CONFIGURED => StatusCode::SERVICE_UNAVAILABLE,
        c if c == code::BIZ_MAIL_SEND_FAILED => StatusCode::BAD_GATEWAY,
        c if c == code::BIZ_MAIL_RECIPIENT_INVALID || c == code::BIZ_MAIL_CONFIG_INVALID => {
            StatusCode::BAD_REQUEST
        }

        // ---- 2xxxx 业务码：400（入参 / 当前状态不允许）----
        // 2026-10-17 新增：客户有未软删子节点 → 拒软删（400）。刻意**不**并进下面
        // 那条 409 段（`BIZ_CUSTOMER_IN_USE`）：409 的语义是「换个值 / 重试也许能过」，
        // 而这里改请求改不动（客户就是有子节点），只能先把子节点处理掉。
        // 下面 `(20000..30000) => BAD_REQUEST` 的兜底本来就是 400，这里显式登记是
        // 为了让「它落在 400 段」成为有断言的契约，而不是兜底的副产物。
        c if c == code::BIZ_CUSTOMER_HAS_CHILDREN => StatusCode::BAD_REQUEST,

        // ---- 2xxxx 业务码：409 (状态冲突 / 重复 / 占用 / 锁) ----
        // 2026-09-30 新增：prod/pool/move 的 from/holder 不一致也归到 409 段，
        // 与 worker→worker/worker→pool 的版本冲突并列。前端按 code 区分
        // 40904 (LOCATION_MISMATCH) vs 40901 (VERSION_CONFLICT)。
        c if c == code::BIZ_USER_DUPLICATE
            || c == code::BIZ_USER_DUPLICATE_USERNAME
            || c == code::BIZ_USER_ROLE_DUPLICATE
            || c == code::BIZ_PART_PRICE_LOCKED_BY_ASSEMBLY
            || c == code::BIZ_PART_QUANTITY_LOCKED
            || c == code::BIZ_CUSTOMER_IN_USE
            || c == code::BIZ_WORKER_IN_USE
            || c == code::BIZ_WORKER_HOLD_LIMIT_EXCEEDED
            || c == code::BIZ_SHELF_DUPLICATE_CODE
            || c == code::BIZ_SHELF_IN_USE
            || c == code::BIZ_PROCESS_DUPLICATE_CODE
            || c == code::BIZ_PROCESS_IN_USE
            || c == code::BIZ_WORK_TYPE_DUPLICATE_CODE
            || c == code::BIZ_WORK_TYPE_IN_USE
            || c == code::BIZ_APPLICANT_DUPLICATE_NAME
            || c == code::BIZ_APPLICANT_IN_USE
            || c == code::BIZ_PART_FILE_DUPLICATE
            || c == code::BIZ_OUTSOURCE_COMPANY_DUPLICATE
            || c == code::BIZ_OUTSOURCE_COMPANY_DUPLICATE_NAME
            || c == code::BIZ_OUTSOURCE_COMPANY_IN_USE
            || c == code::BIZ_OUTSOURCE_QUOTE_DUPLICATE
            || c == code::BIZ_DELIVERY_NOTE_PART_ALREADY_ASSIGNED
            || c == code::BIZ_DELIVERY_NOTE_PARTS_LOCKED
            || c == code::BIZ_DELIVERY_GROUP_DUPLICATE_NAME
            || c == code::BIZ_DELIVERY_GROUP_MEMBER_CONFLICT
            || c == code::BIZ_DELIVERY_NOTE_DRAFT_SCOPE_CONFLICT
            || c == code::BIZ_PART_ALREADY_CANCELLED
            || c == code::BIZ_DELIVERY_NOTE_LOCKED_PART
            || c == code::BIZ_PART_BATCH_NOT_HELD_BY_WORKER
            || c == code::BIZ_PART_NOT_DELETABLE
            || c == code::BIZ_PROCESS_CHAIN_PART_NOT_PENDING
            || c == code::BIZ_PROCESS_CHAIN_REQUIRED
            || c == code::BIZ_BATCH_INVALID_STATUS
            // 2026-09-30 新增：move 路径 from/holder 不一致 → 409（与 VERSION_CONFLICT
            // 并列；前端按 code 区分 40904 vs 40901）。
            || c == code::BIZ_BATCH_LOCATION_MISMATCH
            // 2026-09-30 新增：force-complete 端点已 COMPLETED 幂等拒绝 → 409
            // （与 BIZ_PART_ALREADY_CANCELLED=20115 语义对称；前端按 code 区分）。
            || c == code::BIZ_PART_ALREADY_COMPLETED
            // 2026-10-11 新增：装配件级 force-complete 端点两条幂等拒绝 → 409
            // （与零件级 20123 / 20115 语义对称）。
            || c == code::BIZ_ASSEMBLY_ALREADY_COMPLETED
            || c == code::BIZ_ASSEMBLY_ALREADY_CANCELLED
            // 2026-10-16 新增（scheduled_task 域）：邮箱重名（21707，**不是**
            // 40901 —— 40901 只表达 OCC）与「邮箱被交期汇总配置引用、禁止删除」
            // （21708）。两者都是状态冲突而非入参非法。
            || c == code::BIZ_MAIL_ACCOUNT_NAME_DUPLICATE
            || c == code::BIZ_MAIL_ACCOUNT_IN_USE =>
        {
            StatusCode::CONFLICT
        }

        // ---- 2xxxx 业务码：422 (校验类，Python 显式声明 21113 → 422) ----
        c if c == code::BIZ_DELIVERY_PRINT_BAD_ORDER || c == code::BIZ_SHELF_PROCESS_NOT_MAPPED => {
            StatusCode::UNPROCESSABLE_ENTITY
        }

        // ---- 2xxxx 业务码：400 (客户端错误，与 2xxxx 兜底同值，显式登记以钉住契约) ----
        // 2026-10-13 PR2：`files::part_file::preview` 对「这份文件不做预览」
        // （kind 非图纸类 DRAWING / ASSEMBLY_MASTER，或 file_type ≠ PDF）返 400。
        // 与兜底分支同值，**显式列出**而非靠 `20000..30000` 兜住：这条是前端要按
        // code 分流「不弹重试、提示换文件」的契约，不该随兜底规则漂移而无声改变。
        c if c == code::BIZ_PART_FILE_PREVIEW_UNSUPPORTED => StatusCode::BAD_REQUEST,

        // ---- 2xxxx 业务码：502 (上游网关失败，nginx upstream 同形) ----
        // 2026-09-28 新增：rust → python 薄壳鉴权转发 STS 失败 = 502 BAD_GATEWAY。
        // 2026-10-03 新增：打印转发失败同为 502（与 nginx upstream fail 同语义）。
        // 2026-10-13 PR2 新增：CI 预览 job 终态 Failed（21117）/ 打 python 的 CI 转发
        // 失败（21118）同为 502 —— 两者都是「上游（python / CI）没能把活干完」，
        // 与 STS / 打印转发同族。BIZ_PART_FILE_PREVIEW_UNSUPPORTED（21116）**不**在
        // 这里：那是「这个文件压根不该做预览」的客户端错误，走下方 2xxxx 兜底 400。
        c if c == code::BIZ_STS_FORWARD_FAILED
            || c == code::BIZ_PRINT_FORWARD_FAILED
            || c == code::BIZ_PART_FILE_PREVIEW_FAILED
            || c == code::BIZ_PART_FILE_PREVIEW_FORWARD_FAILED =>
        {
            StatusCode::BAD_GATEWAY
        }

        // ---- 2xxxx 兜底：Python BizError 默认 400 ----
        c if c == code::BIZ_DELIVERY_NOTE_SCOPE_MISMATCH
            || c == code::BIZ_DELIVERY_ASSEMBLY_PARTS_NOT_READY
            || c == code::BIZ_PART_NOT_DELIVERED
            || c == code::BIZ_PART_NOT_READY_TO_SHIP
            || c == code::BIZ_PART_REPAIR_NOT_TRIGGERED =>
        {
            StatusCode::BAD_REQUEST
        }

        // ---- 2xxxx 兜底：Python BizError 默认 400 ----
        c if (20000..30000).contains(&c) => StatusCode::BAD_REQUEST,

        c if (40000..50000).contains(&c) => StatusCode::BAD_REQUEST,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = self.http_status();
        // 不向客户端泄露 sqlx 内部信息
        let message = match &self {
            AppError::Database(e) => {
                tracing::error!(error = %e, "数据库错误");
                "数据库错误".to_string()
            }
            other => other.to_string(),
        };
        // 直接拼装 JSON body（不走泛型 `R<T>`，因为 BizWithFailures 需要序列化
        // 非 `()` 的 `data`；这里手动构造，与 `R<T>` 序列化后的字段约定一致）。
        let body: serde_json::Value = match &self {
            AppError::BizWithFailures { code, failures, .. } => serde_json::json!({
                "code": code,
                "message": &message,
                "data": serde_json::json!({ "failures": failures }),
            }),
            other => serde_json::json!({
                "code": other.code(),
                "message": &message,
                "data": serde_json::Value::Null,
            }),
        };
        (status, Json(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    /// 所有"正典"常量（用于唯一性 + HTTP 表覆盖）。
    /// 故意排除：
    /// - `BIZ_DELIVERY_PARTS_MULTIPLE_CUSTOMERS` (21110)：deprecated 别名，与 21407 值相等
    /// - `USER_NOT_FOUND` / `DUPLICATE_USERNAME` / `ROLE_DUPLICATE` / `ROLE_NOT_FOUND` / `NO_ROLE`：
    ///   206xx 段的短别名，与各自 `BIZ_*` 长名值相等
    const NON_DEPRECATED_CODES: &[(i32, &str)] = &[
        // 0
        (code::SUCCESS, "SUCCESS"),
        // 4xxxx
        (code::BAD_REQUEST, "BAD_REQUEST"),
        (code::VALIDATION_ERROR, "VALIDATION_ERROR"),
        (code::UNAUTHORIZED, "UNAUTHORIZED"),
        (code::BIZ_AUTH_INVALID, "BIZ_AUTH_INVALID"),
        (code::TOKEN_EXPIRED, "TOKEN_EXPIRED"),
        (code::REFRESH_INVALID, "REFRESH_INVALID"),
        (code::OLD_PASSWORD_MISMATCH, "OLD_PASSWORD_MISMATCH"),
        (code::SESSION_REVOKED, "SESSION_REVOKED"),
        // 2026-09-29 新增：企业微信小程序登录 4 码
        (code::BIZ_WX_LOGIN_FAILED, "BIZ_WX_LOGIN_FAILED"),
        (code::BIZ_WX_NOT_BOUND, "BIZ_WX_NOT_BOUND"),
        (code::BIZ_WX_BINDING_DUPLICATE, "BIZ_WX_BINDING_DUPLICATE"),
        (code::BIZ_WX_NOT_CONFIGURED, "BIZ_WX_NOT_CONFIGURED"),
        (code::FORBIDDEN, "FORBIDDEN"),
        (code::SHELF_MISMATCH, "SHELF_MISMATCH"),
        (code::NOT_FOUND, "NOT_FOUND"),
        (code::VERSION_CONFLICT, "VERSION_CONFLICT"),
        // 2026-10-03 新增：请求级超时（middleware::timeout）
        (code::REQUEST_TIMEOUT, "REQUEST_TIMEOUT"),
        (code::REQUEST_TOO_LARGE, "REQUEST_TOO_LARGE"),
        // 5xxxx
        (code::INTERNAL, "INTERNAL"),
        (code::DATABASE, "DATABASE"),
        // 206xx 长名（短别名见 constants_match_python_values）
        (
            code::BIZ_USER_ACCOUNT_NOT_FOUND,
            "BIZ_USER_ACCOUNT_NOT_FOUND",
        ),
        (
            code::BIZ_USER_DUPLICATE_USERNAME,
            "BIZ_USER_DUPLICATE_USERNAME",
        ),
        (code::BIZ_USER_INACTIVE, "BIZ_USER_INACTIVE"),
        (code::BIZ_USER_ROLE_DUPLICATE, "BIZ_USER_ROLE_DUPLICATE"),
        (code::BIZ_USER_ROLE_NOT_FOUND, "BIZ_USER_ROLE_NOT_FOUND"),
        (code::BIZ_USER_NO_ROLE, "BIZ_USER_NO_ROLE"),
        // 200xx
        (code::BIZ_USER_NOT_FOUND, "BIZ_USER_NOT_FOUND"),
        (code::BIZ_USER_DUPLICATE, "BIZ_USER_DUPLICATE"),
        (code::BIZ_ORDER_NOT_FOUND, "BIZ_ORDER_NOT_FOUND"),
        // 201xx
        (code::BIZ_PART_NOT_FOUND, "BIZ_PART_NOT_FOUND"),
        (code::BIZ_CUSTOMER_NOT_FOUND, "BIZ_CUSTOMER_NOT_FOUND"),
        (code::BIZ_INVALID_TRANSITION, "BIZ_INVALID_TRANSITION"),
        (code::BIZ_INVALID_VALUE, "BIZ_INVALID_VALUE"),
        (code::BIZ_PART_SERIAL_EXHAUSTED, "BIZ_PART_SERIAL_EXHAUSTED"),
        (code::BIZ_SERIAL_PREFIX_UNKNOWN, "BIZ_SERIAL_PREFIX_UNKNOWN"),
        (code::BIZ_PART_BATCH_NOT_FOUND, "BIZ_PART_BATCH_NOT_FOUND"),
        (
            code::BIZ_PART_PRICE_LOCKED_BY_ASSEMBLY,
            "BIZ_PART_PRICE_LOCKED_BY_ASSEMBLY",
        ),
        (
            code::BIZ_PART_BATCH_INVALID_QUANTITY,
            "BIZ_PART_BATCH_INVALID_QUANTITY",
        ),
        (code::BIZ_PART_QUANTITY_LOCKED, "BIZ_PART_QUANTITY_LOCKED"),
        (code::BIZ_CUSTOMER_IN_USE, "BIZ_CUSTOMER_IN_USE"),
        (
            code::BIZ_PART_BATCH_NOT_HELD_BY_WORKER,
            "BIZ_PART_BATCH_NOT_HELD_BY_WORKER",
        ),
        (
            code::BIZ_PART_ALREADY_CANCELLED,
            "BIZ_PART_ALREADY_CANCELLED",
        ),
        (code::BIZ_PART_NOT_DELIVERED, "BIZ_PART_NOT_DELIVERED"),
        (
            code::BIZ_PART_NOT_READY_TO_SHIP,
            "BIZ_PART_NOT_READY_TO_SHIP",
        ),
        (
            code::BIZ_PART_REPAIR_NOT_TRIGGERED,
            "BIZ_PART_REPAIR_NOT_TRIGGERED",
        ),
        (code::BIZ_PART_NOT_DELETABLE, "BIZ_PART_NOT_DELETABLE"),
        // 2026-09-29 新增：prod/batch dispatch 专用错误码
        (code::BIZ_BATCH_INVALID_STATUS, "BIZ_BATCH_INVALID_STATUS"),
        (code::BIZ_BATCH_NOT_FOUND, "BIZ_BATCH_NOT_FOUND"),
        // 2026-09-30 新增：prod/pool/move from/holder 不一致（409）
        (
            code::BIZ_BATCH_LOCATION_MISMATCH,
            "BIZ_BATCH_LOCATION_MISMATCH",
        ),
        // 2026-09-30 新增：force-complete 端点已 COMPLETED 幂等拒绝（409）
        (
            code::BIZ_PART_ALREADY_COMPLETED,
            "BIZ_PART_ALREADY_COMPLETED",
        ),
        // 2026-10-17 新增：客户有未软删子节点 → 拒软删（400）
        (code::BIZ_CUSTOMER_HAS_CHILDREN, "BIZ_CUSTOMER_HAS_CHILDREN"),
        // 202xx
        (code::BIZ_WORKER_NOT_FOUND, "BIZ_WORKER_NOT_FOUND"),
        (code::BIZ_WORKER_INACTIVE, "BIZ_WORKER_INACTIVE"),
        (code::BIZ_WORKER_IN_USE, "BIZ_WORKER_IN_USE"),
        (
            code::BIZ_WORKER_HOLD_LIMIT_EXCEEDED,
            "BIZ_WORKER_HOLD_LIMIT_EXCEEDED",
        ),
        // 203xx
        (code::BIZ_ASSEMBLY_NOT_FOUND, "BIZ_ASSEMBLY_NOT_FOUND"),
        (code::BIZ_ASSEMBLY_BAD_CUSTOMER, "BIZ_ASSEMBLY_BAD_CUSTOMER"),
        (
            code::BIZ_ASSEMBLY_TOO_MANY_CHILDREN,
            "BIZ_ASSEMBLY_TOO_MANY_CHILDREN",
        ),
        (code::BIZ_ASSEMBLY_PDF_INVALID, "BIZ_ASSEMBLY_PDF_INVALID"),
        (
            code::BIZ_ASSEMBLY_CHILD_PRICE_LOCKED,
            "BIZ_ASSEMBLY_CHILD_PRICE_LOCKED",
        ),
        (code::BIZ_ASSEMBLY_HAS_SHIPMENT, "BIZ_ASSEMBLY_HAS_SHIPMENT"),
        (
            code::BIZ_CUSTOMER_NO_SERIAL_PREFIX,
            "BIZ_CUSTOMER_NO_SERIAL_PREFIX",
        ),
        // 2026-10-11 新增：装配件级 force-complete 幂等拒绝（409）
        (
            code::BIZ_ASSEMBLY_ALREADY_COMPLETED,
            "BIZ_ASSEMBLY_ALREADY_COMPLETED",
        ),
        (
            code::BIZ_ASSEMBLY_ALREADY_CANCELLED,
            "BIZ_ASSEMBLY_ALREADY_CANCELLED",
        ),
        // 204xx
        (
            code::BIZ_DRAWING_FILE_NOT_FOUND,
            "BIZ_DRAWING_FILE_NOT_FOUND",
        ),
        (code::BIZ_DRAWING_FILE_BAD_TYPE, "BIZ_DRAWING_FILE_BAD_TYPE"),
        (
            code::BIZ_DRAWING_FILE_TOO_LARGE,
            "BIZ_DRAWING_FILE_TOO_LARGE",
        ),
        (code::BIZ_DRAWING_UPLOAD_FAILED, "BIZ_DRAWING_UPLOAD_FAILED"),
        // 2026-09-28 新增：rust → python STS 转发失败
        (code::BIZ_STS_FORWARD_FAILED, "BIZ_STS_FORWARD_FAILED"),
        // 2026-10-03 新增：rust → python 打印转发失败
        (code::BIZ_PRINT_FORWARD_FAILED, "BIZ_PRINT_FORWARD_FAILED"),
        // 205xx
        (code::BIZ_SHELF_NOT_FOUND, "BIZ_SHELF_NOT_FOUND"),
        (code::BIZ_SHELF_DUPLICATE_CODE, "BIZ_SHELF_DUPLICATE_CODE"),
        (code::BIZ_SHELF_IN_USE, "BIZ_SHELF_IN_USE"),
        (
            code::BIZ_SHELF_PROCESS_SHELF_NOT_FOUND,
            "BIZ_SHELF_PROCESS_SHELF_NOT_FOUND",
        ),
        (
            code::BIZ_SHELF_PROCESS_PROCESS_NOT_FOUND,
            "BIZ_SHELF_PROCESS_PROCESS_NOT_FOUND",
        ),
        (
            code::BIZ_SHELF_NO_MATCH_FOR_PROCESS,
            "BIZ_SHELF_NO_MATCH_FOR_PROCESS",
        ),
        (
            code::BIZ_SHELF_PROCESS_NOT_MAPPED,
            "BIZ_SHELF_PROCESS_NOT_MAPPED",
        ),
        // 2026-09-29 新增：prod/batch dispatch —— 按 target_process_id 在 t_shelf_process
        // 找不到任何 active 货架映射
        (
            code::BIZ_SHELF_PROCESS_NOT_FOUND,
            "BIZ_SHELF_PROCESS_NOT_FOUND",
        ),
        (
            code::BIZ_SHELF_NOT_INSPECTION_ZONE,
            "BIZ_SHELF_NOT_INSPECTION_ZONE",
        ),
        (code::BIZ_SHELF_INACTIVE, "BIZ_SHELF_INACTIVE"),
        // 208xx
        (code::BIZ_PROCESS_NOT_FOUND, "BIZ_PROCESS_NOT_FOUND"),
        (
            code::BIZ_PROCESS_DUPLICATE_CODE,
            "BIZ_PROCESS_DUPLICATE_CODE",
        ),
        (code::BIZ_PROCESS_IN_USE, "BIZ_PROCESS_IN_USE"),
        // 209xx
        (code::BIZ_WORK_TYPE_NOT_FOUND, "BIZ_WORK_TYPE_NOT_FOUND"),
        (
            code::BIZ_WORK_TYPE_DUPLICATE_CODE,
            "BIZ_WORK_TYPE_DUPLICATE_CODE",
        ),
        (code::BIZ_WORK_TYPE_IN_USE, "BIZ_WORK_TYPE_IN_USE"),
        // 210xx
        (code::BIZ_APPLICANT_NOT_FOUND, "BIZ_APPLICANT_NOT_FOUND"),
        (
            code::BIZ_APPLICANT_DUPLICATE_NAME,
            "BIZ_APPLICANT_DUPLICATE_NAME",
        ),
        (
            code::BIZ_APPLICANT_BAD_CUSTOMER,
            "BIZ_APPLICANT_BAD_CUSTOMER",
        ),
        (code::BIZ_APPLICANT_IN_USE, "BIZ_APPLICANT_IN_USE"),
        // 211xx（21110 deprecated 别名除外）
        (code::BIZ_PART_FILE_NOT_FOUND, "BIZ_PART_FILE_NOT_FOUND"),
        (code::BIZ_PART_FILE_BAD_TYPE, "BIZ_PART_FILE_BAD_TYPE"),
        (code::BIZ_PART_FILE_TOO_LARGE, "BIZ_PART_FILE_TOO_LARGE"),
        (
            code::BIZ_PART_FILE_UPLOAD_FAILED,
            "BIZ_PART_FILE_UPLOAD_FAILED",
        ),
        (
            code::BIZ_PART_FILE_OWNER_NOT_FOUND,
            "BIZ_PART_FILE_OWNER_NOT_FOUND",
        ),
        (code::BIZ_PART_FILE_DUPLICATE, "BIZ_PART_FILE_DUPLICATE"),
        (
            code::BIZ_DELIVERY_TEMPLATE_NOT_CONFIGURED,
            "BIZ_DELIVERY_TEMPLATE_NOT_CONFIGURED",
        ),
        (
            code::BIZ_DELIVERY_PART_STATUS_INVALID,
            "BIZ_DELIVERY_PART_STATUS_INVALID",
        ),
        (
            code::BIZ_DELIVERY_TEMPLATE_TOO_MANY_PARTS,
            "BIZ_DELIVERY_TEMPLATE_TOO_MANY_PARTS",
        ),
        (
            code::BIZ_DELIVERY_PRINT_BAD_ORDER,
            "BIZ_DELIVERY_PRINT_BAD_ORDER",
        ),
        // 2026-09-16 M2-A：前端直传 COS 链路 + STS 凭证签发错误码
        (
            code::BIZ_PART_FILE_TMP_OBJECT_MISSING,
            "BIZ_PART_FILE_TMP_OBJECT_MISSING",
        ),
        (
            code::BIZ_PART_FILE_SIZE_MISMATCH,
            "BIZ_PART_FILE_SIZE_MISMATCH",
        ),
        // 2026-09-28 删除：BIZ_STS_ISSUE_FAILED 常量项（21116 码段释放）。
        // 2026-10-13 PR2：21116 由 `files::part_file::preview` 复用（预览不支持的文件类型）。
        (
            code::BIZ_PART_FILE_PREVIEW_UNSUPPORTED,
            "BIZ_PART_FILE_PREVIEW_UNSUPPORTED",
        ),
        (
            code::BIZ_PART_FILE_PREVIEW_FAILED,
            "BIZ_PART_FILE_PREVIEW_FAILED",
        ),
        (
            code::BIZ_PART_FILE_PREVIEW_FORWARD_FAILED,
            "BIZ_PART_FILE_PREVIEW_FORWARD_FAILED",
        ),
        // 212xx
        (
            code::BIZ_OUTSOURCE_COMPANY_NOT_FOUND,
            "BIZ_OUTSOURCE_COMPANY_NOT_FOUND",
        ),
        (
            code::BIZ_OUTSOURCE_COMPANY_DUPLICATE,
            "BIZ_OUTSOURCE_COMPANY_DUPLICATE",
        ),
        (
            code::BIZ_OUTSOURCE_COMPANY_BAD_PROCESS,
            "BIZ_OUTSOURCE_COMPANY_BAD_PROCESS",
        ),
        (
            code::BIZ_OUTSOURCE_PROCESS_NOT_MAPPED,
            "BIZ_OUTSOURCE_PROCESS_NOT_MAPPED",
        ),
        (
            code::BIZ_OUTSOURCE_COMPANY_IN_USE,
            "BIZ_OUTSOURCE_COMPANY_IN_USE",
        ),
        (
            code::BIZ_PART_NOT_OUTSOURCEABLE,
            "BIZ_PART_NOT_OUTSOURCEABLE",
        ),
        (
            code::BIZ_OUTSOURCE_DIRECT_REQUIRES_C2_SHELF,
            "BIZ_OUTSOURCE_DIRECT_REQUIRES_C2_SHELF",
        ),
        (code::BIZ_OUTSOURCE_NO_SHELF, "BIZ_OUTSOURCE_NO_SHELF"),
        (
            code::BIZ_OUTSOURCE_COMPANY_DUPLICATE_NAME,
            "BIZ_OUTSOURCE_COMPANY_DUPLICATE_NAME",
        ),
        // 213xx
        (
            code::BIZ_OUTSOURCE_QUOTE_NOT_FOUND,
            "BIZ_OUTSOURCE_QUOTE_NOT_FOUND",
        ),
        (
            code::BIZ_OUTSOURCE_QUOTE_INVALID_TRANSITION,
            "BIZ_OUTSOURCE_QUOTE_INVALID_TRANSITION",
        ),
        (
            code::BIZ_OUTSOURCE_QUOTE_DUPLICATE,
            "BIZ_OUTSOURCE_QUOTE_DUPLICATE",
        ),
        (
            code::BIZ_OUTSOURCE_QUOTE_NOT_APPROVED,
            "BIZ_OUTSOURCE_QUOTE_NOT_APPROVED",
        ),
        // 214xx
        (
            code::BIZ_DELIVERY_NOTE_NOT_FOUND,
            "BIZ_DELIVERY_NOTE_NOT_FOUND",
        ),
        (
            code::BIZ_DELIVERY_NOTE_INVALID_TRANSITION,
            "BIZ_DELIVERY_NOTE_INVALID_TRANSITION",
        ),
        (
            code::BIZ_DELIVERY_NOTE_NOT_DRAFT,
            "BIZ_DELIVERY_NOTE_NOT_DRAFT",
        ),
        (
            code::BIZ_DELIVERY_NOTE_NOT_SUBMITTED,
            "BIZ_DELIVERY_NOTE_NOT_SUBMITTED",
        ),
        (
            code::BIZ_DELIVERY_NOTE_PART_NOT_READY,
            "BIZ_DELIVERY_NOTE_PART_NOT_READY",
        ),
        (
            code::BIZ_DELIVERY_NOTE_PART_ALREADY_ASSIGNED,
            "BIZ_DELIVERY_NOTE_PART_ALREADY_ASSIGNED",
        ),
        (
            code::BIZ_DELIVERY_NOTE_PARTS_MULTIPLE_CUSTOMERS,
            "BIZ_DELIVERY_NOTE_PARTS_MULTIPLE_CUSTOMERS",
        ),
        (
            code::BIZ_DELIVERY_NOTE_SCAN_MISMATCH,
            "BIZ_DELIVERY_NOTE_SCAN_MISMATCH",
        ),
        (
            code::BIZ_DELIVERY_NOTE_DRIVER_INVALID,
            "BIZ_DELIVERY_NOTE_DRIVER_INVALID",
        ),
        (
            code::BIZ_DELIVERY_NOTE_SCAN_INCOMPLETE,
            "BIZ_DELIVERY_NOTE_SCAN_INCOMPLETE",
        ),
        (
            code::BIZ_DELIVERY_NOTE_INVALID_VALUE,
            "BIZ_DELIVERY_NOTE_INVALID_VALUE",
        ),
        (
            code::BIZ_DELIVERY_NOTE_PARTS_LOCKED,
            "BIZ_DELIVERY_NOTE_PARTS_LOCKED",
        ),
        (
            code::BIZ_DELIVERY_GROUP_NOT_FOUND,
            "BIZ_DELIVERY_GROUP_NOT_FOUND",
        ),
        (
            code::BIZ_DELIVERY_GROUP_DUPLICATE_NAME,
            "BIZ_DELIVERY_GROUP_DUPLICATE_NAME",
        ),
        (
            code::BIZ_DELIVERY_GROUP_MEMBER_CONFLICT,
            "BIZ_DELIVERY_GROUP_MEMBER_CONFLICT",
        ),
        (
            code::BIZ_DELIVERY_NOTE_SCOPE_MISMATCH,
            "BIZ_DELIVERY_NOTE_SCOPE_MISMATCH",
        ),
        (
            code::BIZ_DELIVERY_SCAN_UNKNOWN_CODE,
            "BIZ_DELIVERY_SCAN_UNKNOWN_CODE",
        ),
        (
            code::BIZ_DELIVERY_ASSEMBLY_PARTS_NOT_READY,
            "BIZ_DELIVERY_ASSEMBLY_PARTS_NOT_READY",
        ),
        (
            code::BIZ_DELIVERY_NOTE_DRAFT_SCOPE_CONFLICT,
            "BIZ_DELIVERY_NOTE_DRAFT_SCOPE_CONFLICT",
        ),
        (
            code::BIZ_DELIVERY_NOTE_LOCKED_PART,
            "BIZ_DELIVERY_NOTE_LOCKED_PART",
        ),
        // 215xx
        (
            code::BIZ_OUTSOURCE_SHIPMENT_NOT_FOUND,
            "BIZ_OUTSOURCE_SHIPMENT_NOT_FOUND",
        ),
        (
            code::BIZ_OUTSOURCE_SHIPMENT_INVALID_TRANSITION,
            "BIZ_OUTSOURCE_SHIPMENT_INVALID_TRANSITION",
        ),
        (
            code::BIZ_OUTSOURCE_SHIPMENT_NO_OPEN,
            "BIZ_OUTSOURCE_SHIPMENT_NO_OPEN",
        ),
        (
            code::BIZ_OUTSOURCE_SHIPMENT_QUANTITY_EXCEEDS,
            "BIZ_OUTSOURCE_SHIPMENT_QUANTITY_EXCEEDS",
        ),
        // 217xx 交期汇总邮件（2026-10-13 新增）
        (code::BIZ_MAIL_NOT_CONFIGURED, "BIZ_MAIL_NOT_CONFIGURED"),
        (code::BIZ_MAIL_SEND_FAILED, "BIZ_MAIL_SEND_FAILED"),
        (
            code::BIZ_MAIL_RECIPIENT_INVALID,
            "BIZ_MAIL_RECIPIENT_INVALID",
        ),
        (code::BIZ_MAIL_CONFIG_INVALID, "BIZ_MAIL_CONFIG_INVALID"),
        (code::BIZ_MAIL_CONFIG_NOT_FOUND, "BIZ_MAIL_CONFIG_NOT_FOUND"),
        // 21706..21708 scheduled_task 域（2026-10-16 新增：邮箱实体）
        (
            code::BIZ_MAIL_ACCOUNT_NOT_FOUND,
            "BIZ_MAIL_ACCOUNT_NOT_FOUND",
        ),
        (
            code::BIZ_MAIL_ACCOUNT_NAME_DUPLICATE,
            "BIZ_MAIL_ACCOUNT_NAME_DUPLICATE",
        ),
        (code::BIZ_MAIL_ACCOUNT_IN_USE, "BIZ_MAIL_ACCOUNT_IN_USE"),
        // 207xx 工艺链（worker-pool auto-allocate 阶段新增）
        (
            code::BIZ_PROCESS_CHAIN_NOT_FOUND,
            "BIZ_PROCESS_CHAIN_NOT_FOUND",
        ),
        (
            code::BIZ_PROCESS_CHAIN_STEP_NOT_FOUND,
            "BIZ_PROCESS_CHAIN_STEP_NOT_FOUND",
        ),
        // 20703 / 20704 已随 auto-allocate TIME / fill_ratio 移除释放（2026-10-15），
        // 见常量声明处的注释。
        (
            code::BIZ_PROCESS_CHAIN_PART_NOT_PENDING,
            "BIZ_PROCESS_CHAIN_PART_NOT_PENDING",
        ),
        // 2026-09-16 PR-3 批次 step 化：part 无工艺链时禁上生产流
        (
            code::BIZ_PROCESS_CHAIN_REQUIRED,
            "BIZ_PROCESS_CHAIN_REQUIRED",
        ),
        // 2026-09-28 删除：相关会话域错误码（域整体下线）。
    ];

    #[test]
    fn constants_have_unique_values() {
        let mut seen = std::collections::HashMap::new();
        for &(c, name) in NON_DEPRECATED_CODES {
            if let Some(prev) = seen.insert(c, name) {
                panic!("duplicate value {c}: {prev} and {name}");
            }
        }
    }

    #[test]
    #[allow(deprecated)]
    fn constants_match_python_values() {
        // 0 / 4xxxx / 5xxxx
        assert_eq!(code::SUCCESS, 0);
        assert_eq!(code::BAD_REQUEST, 40000);
        assert_eq!(code::VALIDATION_ERROR, 40001);
        assert_eq!(code::UNAUTHORIZED, 40100);
        assert_eq!(code::BIZ_AUTH_INVALID, 40101);
        assert_eq!(code::TOKEN_EXPIRED, 40102);
        assert_eq!(code::REFRESH_INVALID, 40103);
        assert_eq!(code::OLD_PASSWORD_MISMATCH, 40104);
        assert_eq!(code::SESSION_REVOKED, 40105);
        // 2026-09-29 新增：企业微信小程序登录（Rust 侧新增，Python 端无对应值）
        assert_eq!(code::BIZ_WX_LOGIN_FAILED, 40106);
        assert_eq!(code::BIZ_WX_NOT_BOUND, 40107);
        assert_eq!(code::BIZ_WX_BINDING_DUPLICATE, 40108);
        assert_eq!(code::BIZ_WX_NOT_CONFIGURED, 40109);
        assert_eq!(code::BIZ_WX_USER_ALREADY_BOUND, 40110);
        assert_eq!(code::FORBIDDEN, 40300);
        assert_eq!(code::SHELF_MISMATCH, 40301);
        assert_eq!(code::NOT_FOUND, 40400);
        assert_eq!(code::VERSION_CONFLICT, 40901);
        // 2026-10-03 新增：HTTP 408 语义（Rust 侧独有，Python 端无对应值）
        assert_eq!(code::REQUEST_TIMEOUT, 40800);
        assert_eq!(code::REQUEST_TOO_LARGE, 41301);
        assert_eq!(code::INTERNAL, 50000);
        assert_eq!(code::DATABASE, 50001);

        // 200xx
        assert_eq!(code::BIZ_USER_NOT_FOUND, 20001);
        assert_eq!(code::BIZ_USER_DUPLICATE, 20002);
        assert_eq!(code::BIZ_ORDER_NOT_FOUND, 20003);

        // 201xx
        assert_eq!(code::BIZ_PART_NOT_FOUND, 20101);
        assert_eq!(code::BIZ_CUSTOMER_NOT_FOUND, 20102);
        assert_eq!(code::BIZ_INVALID_TRANSITION, 20103);
        assert_eq!(code::BIZ_INVALID_VALUE, 20104);
        assert_eq!(code::BIZ_PART_SERIAL_EXHAUSTED, 20105);
        assert_eq!(code::BIZ_SERIAL_PREFIX_UNKNOWN, 20108);
        assert_eq!(code::BIZ_PART_BATCH_NOT_FOUND, 20109);
        assert_eq!(code::BIZ_PART_PRICE_LOCKED_BY_ASSEMBLY, 20110);
        assert_eq!(code::BIZ_PART_BATCH_INVALID_QUANTITY, 20111);
        assert_eq!(code::BIZ_PART_QUANTITY_LOCKED, 20112);
        assert_eq!(code::BIZ_CUSTOMER_IN_USE, 20113);
        assert_eq!(code::BIZ_PART_BATCH_NOT_HELD_BY_WORKER, 20114);
        assert_eq!(code::BIZ_PART_ALREADY_CANCELLED, 20115);
        assert_eq!(code::BIZ_PART_NOT_DELIVERED, 20116);
        assert_eq!(code::BIZ_PART_NOT_READY_TO_SHIP, 20117);
        assert_eq!(code::BIZ_PART_REPAIR_NOT_TRIGGERED, 20118);
        assert_eq!(code::BIZ_PART_NOT_DELETABLE, 20119);
        // 2026-09-29 新增：prod/batch dispatch 专用错误码
        assert_eq!(code::BIZ_BATCH_INVALID_STATUS, 20120);
        assert_eq!(code::BIZ_BATCH_NOT_FOUND, 20121);
        // 2026-09-30 新增：move 路径 from/holder 不一致 → 409
        assert_eq!(code::BIZ_BATCH_LOCATION_MISMATCH, 20122);
        // 2026-09-30 新增：force-complete 端点已 COMPLETED 幂等拒绝 → 409
        assert_eq!(code::BIZ_PART_ALREADY_COMPLETED, 20123);
        // 2026-10-17 新增：客户层级任意深度 → 子节点闸门码
        assert_eq!(code::BIZ_CUSTOMER_HAS_CHILDREN, 20124);

        // 202xx
        assert_eq!(code::BIZ_WORKER_NOT_FOUND, 20201);
        assert_eq!(code::BIZ_WORKER_INACTIVE, 20202);
        assert_eq!(code::BIZ_WORKER_IN_USE, 20203);
        assert_eq!(code::BIZ_WORKER_HOLD_LIMIT_EXCEEDED, 20204);

        // 203xx
        assert_eq!(code::BIZ_ASSEMBLY_NOT_FOUND, 20301);
        assert_eq!(code::BIZ_ASSEMBLY_BAD_CUSTOMER, 20302);
        assert_eq!(code::BIZ_ASSEMBLY_TOO_MANY_CHILDREN, 20303);
        assert_eq!(code::BIZ_ASSEMBLY_PDF_INVALID, 20305);
        assert_eq!(code::BIZ_ASSEMBLY_CHILD_PRICE_LOCKED, 20306);
        assert_eq!(code::BIZ_ASSEMBLY_HAS_SHIPMENT, 20307);
        assert_eq!(code::BIZ_CUSTOMER_NO_SERIAL_PREFIX, 20308);
        assert_eq!(code::BIZ_ASSEMBLY_ALREADY_COMPLETED, 20309);
        assert_eq!(code::BIZ_ASSEMBLY_ALREADY_CANCELLED, 20310);

        // 204xx
        assert_eq!(code::BIZ_DRAWING_FILE_NOT_FOUND, 20401);
        assert_eq!(code::BIZ_DRAWING_FILE_BAD_TYPE, 20402);
        assert_eq!(code::BIZ_DRAWING_FILE_TOO_LARGE, 20403);
        assert_eq!(code::BIZ_DRAWING_UPLOAD_FAILED, 20404);
        // 2026-09-28 新增：rust → python STS 转发失败
        assert_eq!(code::BIZ_STS_FORWARD_FAILED, 20406);
        // 2026-10-03 新增：rust → python 打印转发失败
        assert_eq!(code::BIZ_PRINT_FORWARD_FAILED, 20407);

        // 205xx
        assert_eq!(code::BIZ_SHELF_NOT_FOUND, 20501);
        assert_eq!(code::BIZ_SHELF_DUPLICATE_CODE, 20502);
        assert_eq!(code::BIZ_SHELF_IN_USE, 20503);
        assert_eq!(code::BIZ_SHELF_PROCESS_SHELF_NOT_FOUND, 20504);
        assert_eq!(code::BIZ_SHELF_PROCESS_PROCESS_NOT_FOUND, 20505);
        assert_eq!(code::BIZ_SHELF_NO_MATCH_FOR_PROCESS, 20506);
        assert_eq!(code::BIZ_SHELF_PROCESS_NOT_MAPPED, 20507);
        // 2026-09-29 新增：prod/batch dispatch —— t_shelf_process 找不到映射
        assert_eq!(code::BIZ_SHELF_PROCESS_NOT_FOUND, 20508);
        assert_eq!(code::BIZ_SHELF_NOT_INSPECTION_ZONE, 20511);
        assert_eq!(code::BIZ_SHELF_INACTIVE, 20512);

        // 206xx + 短别名
        assert_eq!(code::BIZ_USER_ACCOUNT_NOT_FOUND, 20601);
        assert_eq!(code::BIZ_USER_DUPLICATE_USERNAME, 20602);
        assert_eq!(code::BIZ_USER_INACTIVE, 20603);
        assert_eq!(code::BIZ_USER_ROLE_DUPLICATE, 20604);
        assert_eq!(code::BIZ_USER_ROLE_NOT_FOUND, 20605);
        assert_eq!(code::BIZ_USER_NO_ROLE, 20606);
        assert_eq!(code::USER_NOT_FOUND, code::BIZ_USER_ACCOUNT_NOT_FOUND);
        assert_eq!(code::DUPLICATE_USERNAME, code::BIZ_USER_DUPLICATE_USERNAME);
        assert_eq!(code::ROLE_DUPLICATE, code::BIZ_USER_ROLE_DUPLICATE);
        assert_eq!(code::ROLE_NOT_FOUND, code::BIZ_USER_ROLE_NOT_FOUND);
        assert_eq!(code::NO_ROLE, code::BIZ_USER_NO_ROLE);

        // 208xx
        assert_eq!(code::BIZ_PROCESS_NOT_FOUND, 20801);
        assert_eq!(code::BIZ_PROCESS_DUPLICATE_CODE, 20802);
        assert_eq!(code::BIZ_PROCESS_IN_USE, 20803);

        // 209xx
        assert_eq!(code::BIZ_WORK_TYPE_NOT_FOUND, 20901);
        assert_eq!(code::BIZ_WORK_TYPE_DUPLICATE_CODE, 20902);
        assert_eq!(code::BIZ_WORK_TYPE_IN_USE, 20903);

        // 210xx
        assert_eq!(code::BIZ_APPLICANT_NOT_FOUND, 21001);
        assert_eq!(code::BIZ_APPLICANT_DUPLICATE_NAME, 21002);
        assert_eq!(code::BIZ_APPLICANT_BAD_CUSTOMER, 21003);
        assert_eq!(code::BIZ_APPLICANT_IN_USE, 21004);

        // 211xx + 21110 deprecated 别名指向 21407
        assert_eq!(code::BIZ_PART_FILE_NOT_FOUND, 21101);
        assert_eq!(code::BIZ_PART_FILE_BAD_TYPE, 21102);
        assert_eq!(code::BIZ_PART_FILE_TOO_LARGE, 21103);
        assert_eq!(code::BIZ_PART_FILE_UPLOAD_FAILED, 21104);
        assert_eq!(code::BIZ_PART_FILE_OWNER_NOT_FOUND, 21105);
        assert_eq!(code::BIZ_PART_FILE_DUPLICATE, 21108);
        assert_eq!(code::BIZ_DELIVERY_TEMPLATE_NOT_CONFIGURED, 21109);
        assert_eq!(code::BIZ_DELIVERY_PART_STATUS_INVALID, 21111);
        assert_eq!(code::BIZ_DELIVERY_TEMPLATE_TOO_MANY_PARTS, 21112);
        assert_eq!(code::BIZ_DELIVERY_PRINT_BAD_ORDER, 21113);
        // 2026-09-16 M2-A：前端直传 COS + STS 错误码数值契约
        assert_eq!(code::BIZ_PART_FILE_TMP_OBJECT_MISSING, 21114);
        assert_eq!(code::BIZ_PART_FILE_SIZE_MISMATCH, 21115);
        // 2026-10-13 PR2：COS CI 预览三码（21116 是 2026-09-28 释放的 STS 槽位）
        assert_eq!(code::BIZ_PART_FILE_PREVIEW_UNSUPPORTED, 21116);
        assert_eq!(code::BIZ_PART_FILE_PREVIEW_FAILED, 21117);
        assert_eq!(code::BIZ_PART_FILE_PREVIEW_FORWARD_FAILED, 21118);
        assert_eq!(code::BIZ_DELIVERY_PARTS_MULTIPLE_CUSTOMERS, 21407);

        // 212xx
        assert_eq!(code::BIZ_OUTSOURCE_COMPANY_NOT_FOUND, 21201);
        assert_eq!(code::BIZ_OUTSOURCE_COMPANY_DUPLICATE, 21202);
        assert_eq!(code::BIZ_OUTSOURCE_COMPANY_BAD_PROCESS, 21203);
        assert_eq!(code::BIZ_OUTSOURCE_PROCESS_NOT_MAPPED, 21204);
        assert_eq!(code::BIZ_OUTSOURCE_COMPANY_IN_USE, 21205);
        assert_eq!(code::BIZ_PART_NOT_OUTSOURCEABLE, 21206);
        assert_eq!(code::BIZ_OUTSOURCE_DIRECT_REQUIRES_C2_SHELF, 21207);
        assert_eq!(code::BIZ_OUTSOURCE_NO_SHELF, 21208);

        // 213xx
        assert_eq!(code::BIZ_OUTSOURCE_QUOTE_NOT_FOUND, 21301);
        assert_eq!(code::BIZ_OUTSOURCE_QUOTE_INVALID_TRANSITION, 21302);
        assert_eq!(code::BIZ_OUTSOURCE_QUOTE_DUPLICATE, 21303);
        assert_eq!(code::BIZ_OUTSOURCE_QUOTE_NOT_APPROVED, 21307);

        // 214xx
        assert_eq!(code::BIZ_DELIVERY_NOTE_NOT_FOUND, 21401);
        assert_eq!(code::BIZ_DELIVERY_NOTE_INVALID_TRANSITION, 21402);
        assert_eq!(code::BIZ_DELIVERY_NOTE_NOT_DRAFT, 21403);
        assert_eq!(code::BIZ_DELIVERY_NOTE_NOT_SUBMITTED, 21404);
        assert_eq!(code::BIZ_DELIVERY_NOTE_PART_NOT_READY, 21405);
        assert_eq!(code::BIZ_DELIVERY_NOTE_PART_ALREADY_ASSIGNED, 21406);
        assert_eq!(code::BIZ_DELIVERY_NOTE_PARTS_MULTIPLE_CUSTOMERS, 21407);
        assert_eq!(code::BIZ_DELIVERY_NOTE_SCAN_MISMATCH, 21408);
        assert_eq!(code::BIZ_DELIVERY_NOTE_DRIVER_INVALID, 21409);
        assert_eq!(code::BIZ_DELIVERY_NOTE_SCAN_INCOMPLETE, 21410);
        assert_eq!(code::BIZ_DELIVERY_NOTE_INVALID_VALUE, 21411);
        assert_eq!(code::BIZ_DELIVERY_NOTE_PARTS_LOCKED, 21412);
        assert_eq!(code::BIZ_DELIVERY_GROUP_NOT_FOUND, 21413);
        assert_eq!(code::BIZ_DELIVERY_GROUP_DUPLICATE_NAME, 21414);
        assert_eq!(code::BIZ_DELIVERY_GROUP_MEMBER_CONFLICT, 21415);
        assert_eq!(code::BIZ_DELIVERY_NOTE_SCOPE_MISMATCH, 21416);
        assert_eq!(code::BIZ_DELIVERY_SCAN_UNKNOWN_CODE, 21417);
        assert_eq!(code::BIZ_DELIVERY_ASSEMBLY_PARTS_NOT_READY, 21418);
        assert_eq!(code::BIZ_DELIVERY_NOTE_DRAFT_SCOPE_CONFLICT, 21419);
        assert_eq!(code::BIZ_DELIVERY_NOTE_LOCKED_PART, 21420);

        // 215xx
        assert_eq!(code::BIZ_OUTSOURCE_SHIPMENT_NOT_FOUND, 21501);
        assert_eq!(code::BIZ_OUTSOURCE_SHIPMENT_INVALID_TRANSITION, 21502);
        assert_eq!(code::BIZ_OUTSOURCE_SHIPMENT_NO_OPEN, 21503);
        assert_eq!(code::BIZ_OUTSOURCE_SHIPMENT_QUANTITY_EXCEEDS, 21504);

        // 207xx
        assert_eq!(code::BIZ_PROCESS_CHAIN_NOT_FOUND, 20701);
        assert_eq!(code::BIZ_PROCESS_CHAIN_STEP_NOT_FOUND, 20702);
        // 20703 / 20704 已释放（2026-10-15，见常量声明处注释），无断言。
        assert_eq!(code::BIZ_PROCESS_CHAIN_PART_NOT_PENDING, 20705);
        // 2026-09-16 PR-3 批次 step 化：part 无工艺链时禁上生产流
        assert_eq!(code::BIZ_PROCESS_CHAIN_REQUIRED, 20706);

        // 2026-09-28 删除：216xx 相关会话域断言（域下线，码段**未复用**）。

        // 217xx（mail_digest，2026-10-13 新增；后 3 条 scheduled_task，2026-10-16 新增）
        assert_eq!(code::BIZ_MAIL_NOT_CONFIGURED, 21701);
        assert_eq!(code::BIZ_MAIL_SEND_FAILED, 21702);
        assert_eq!(code::BIZ_MAIL_RECIPIENT_INVALID, 21703);
        assert_eq!(code::BIZ_MAIL_CONFIG_INVALID, 21704);
        assert_eq!(code::BIZ_MAIL_CONFIG_NOT_FOUND, 21705);
        assert_eq!(code::BIZ_MAIL_ACCOUNT_NOT_FOUND, 21706);
        assert_eq!(code::BIZ_MAIL_ACCOUNT_NAME_DUPLICATE, 21707);
        assert_eq!(code::BIZ_MAIL_ACCOUNT_IN_USE, 21708);
    }

    /// 数据驱动的 HTTP 表覆盖测试：每个 (code, expected_http, name) 一行。
    /// 覆盖所有显式映射到 404/409/422 的新码，以及 2xxxx 默认 400 兜底。
    const HTTP_TABLE_CASES: &[(i32, StatusCode, &str)] = &[
        // 4xxxx（既有）
        (code::BAD_REQUEST, StatusCode::BAD_REQUEST, "BAD_REQUEST"),
        (
            code::VALIDATION_ERROR,
            StatusCode::UNPROCESSABLE_ENTITY,
            "VALIDATION_ERROR",
        ),
        (code::UNAUTHORIZED, StatusCode::UNAUTHORIZED, "UNAUTHORIZED"),
        (
            code::BIZ_AUTH_INVALID,
            StatusCode::UNAUTHORIZED,
            "BIZ_AUTH_INVALID",
        ),
        (
            code::TOKEN_EXPIRED,
            StatusCode::UNAUTHORIZED,
            "TOKEN_EXPIRED",
        ),
        (
            code::REFRESH_INVALID,
            StatusCode::UNAUTHORIZED,
            "REFRESH_INVALID",
        ),
        (
            code::OLD_PASSWORD_MISMATCH,
            StatusCode::UNAUTHORIZED,
            "OLD_PASSWORD_MISMATCH",
        ),
        (
            code::SESSION_REVOKED,
            StatusCode::UNAUTHORIZED,
            "SESSION_REVOKED",
        ),
        // 2026-09-29 新增：企业微信小程序登录 4 码（显式登记，绕开 40000 段兜底 400）
        (
            code::BIZ_WX_LOGIN_FAILED,
            StatusCode::UNAUTHORIZED,
            "BIZ_WX_LOGIN_FAILED",
        ),
        (
            code::BIZ_WX_NOT_BOUND,
            StatusCode::FORBIDDEN,
            "BIZ_WX_NOT_BOUND",
        ),
        (
            code::BIZ_WX_BINDING_DUPLICATE,
            StatusCode::CONFLICT,
            "BIZ_WX_BINDING_DUPLICATE",
        ),
        (
            code::BIZ_WX_NOT_CONFIGURED,
            StatusCode::SERVICE_UNAVAILABLE,
            "BIZ_WX_NOT_CONFIGURED",
        ),
        // 2026-10-13 新增（mail_digest 域 217xx）：未配置 SMTP → 503（与 40109 同款）；
        // 发信失败 → 502（外部依赖挂了，不是入参问题）；邮箱/配置非法 → 400。
        (
            code::BIZ_MAIL_NOT_CONFIGURED,
            StatusCode::SERVICE_UNAVAILABLE,
            "BIZ_MAIL_NOT_CONFIGURED",
        ),
        (
            code::BIZ_MAIL_SEND_FAILED,
            StatusCode::BAD_GATEWAY,
            "BIZ_MAIL_SEND_FAILED",
        ),
        (
            code::BIZ_MAIL_RECIPIENT_INVALID,
            StatusCode::BAD_REQUEST,
            "BIZ_MAIL_RECIPIENT_INVALID",
        ),
        (
            code::BIZ_MAIL_CONFIG_INVALID,
            StatusCode::BAD_REQUEST,
            "BIZ_MAIL_CONFIG_INVALID",
        ),
        (
            code::BIZ_MAIL_CONFIG_NOT_FOUND,
            StatusCode::NOT_FOUND,
            "BIZ_MAIL_CONFIG_NOT_FOUND",
        ),
        // 2026-10-16 新增（scheduled_task 域）：邮箱不存在 → 404；重名 / 被引用
        // 禁删 → 409（⚠️ 重名**不是** 40901，那一码只表达 OCC）。
        (
            code::BIZ_MAIL_ACCOUNT_NOT_FOUND,
            StatusCode::NOT_FOUND,
            "BIZ_MAIL_ACCOUNT_NOT_FOUND",
        ),
        (
            code::BIZ_MAIL_ACCOUNT_NAME_DUPLICATE,
            StatusCode::CONFLICT,
            "BIZ_MAIL_ACCOUNT_NAME_DUPLICATE",
        ),
        (
            code::BIZ_MAIL_ACCOUNT_IN_USE,
            StatusCode::CONFLICT,
            "BIZ_MAIL_ACCOUNT_IN_USE",
        ),
        (code::FORBIDDEN, StatusCode::FORBIDDEN, "FORBIDDEN"),
        (
            code::SHELF_MISMATCH,
            StatusCode::FORBIDDEN,
            "SHELF_MISMATCH",
        ),
        (code::NO_ROLE, StatusCode::FORBIDDEN, "NO_ROLE"),
        (
            code::USER_NOT_FOUND,
            StatusCode::NOT_FOUND,
            "USER_NOT_FOUND",
        ),
        (
            code::ROLE_NOT_FOUND,
            StatusCode::NOT_FOUND,
            "ROLE_NOT_FOUND",
        ),
        (
            code::DUPLICATE_USERNAME,
            StatusCode::CONFLICT,
            "DUPLICATE_USERNAME",
        ),
        (code::ROLE_DUPLICATE, StatusCode::CONFLICT, "ROLE_DUPLICATE"),
        (code::NOT_FOUND, StatusCode::NOT_FOUND, "NOT_FOUND"),
        (
            code::VERSION_CONFLICT,
            StatusCode::CONFLICT,
            "VERSION_CONFLICT",
        ),
        // 2026-10-03 新增：请求级超时 → 408（显式登记，绕开 40000 段兜底 400）
        (
            code::REQUEST_TIMEOUT,
            StatusCode::REQUEST_TIMEOUT,
            "REQUEST_TIMEOUT",
        ),
        (
            code::REQUEST_TOO_LARGE,
            StatusCode::PAYLOAD_TOO_LARGE,
            "REQUEST_TOO_LARGE",
        ),
        (
            code::INTERNAL,
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL",
        ),
        (
            code::DATABASE,
            StatusCode::INTERNAL_SERVER_ERROR,
            "DATABASE",
        ),
        // 2xxxx 显式 404
        (
            code::BIZ_USER_NOT_FOUND,
            StatusCode::NOT_FOUND,
            "BIZ_USER_NOT_FOUND",
        ),
        (
            code::BIZ_ORDER_NOT_FOUND,
            StatusCode::NOT_FOUND,
            "BIZ_ORDER_NOT_FOUND",
        ),
        (
            code::BIZ_PART_NOT_FOUND,
            StatusCode::NOT_FOUND,
            "BIZ_PART_NOT_FOUND",
        ),
        (
            code::BIZ_CUSTOMER_NOT_FOUND,
            StatusCode::NOT_FOUND,
            "BIZ_CUSTOMER_NOT_FOUND",
        ),
        (
            code::BIZ_PART_BATCH_NOT_FOUND,
            StatusCode::NOT_FOUND,
            "BIZ_PART_BATCH_NOT_FOUND",
        ),
        (
            code::BIZ_WORKER_NOT_FOUND,
            StatusCode::NOT_FOUND,
            "BIZ_WORKER_NOT_FOUND",
        ),
        (
            code::BIZ_ASSEMBLY_NOT_FOUND,
            StatusCode::NOT_FOUND,
            "BIZ_ASSEMBLY_NOT_FOUND",
        ),
        (
            code::BIZ_DRAWING_FILE_NOT_FOUND,
            StatusCode::NOT_FOUND,
            "BIZ_DRAWING_FILE_NOT_FOUND",
        ),
        (
            code::BIZ_SHELF_NOT_FOUND,
            StatusCode::NOT_FOUND,
            "BIZ_SHELF_NOT_FOUND",
        ),
        // 2026-09-29 新增：prod/batch dispatch —— t_shelf_process 找不到映射
        (
            code::BIZ_SHELF_PROCESS_NOT_FOUND,
            StatusCode::NOT_FOUND,
            "BIZ_SHELF_PROCESS_NOT_FOUND",
        ),
        (
            code::BIZ_PROCESS_NOT_FOUND,
            StatusCode::NOT_FOUND,
            "BIZ_PROCESS_NOT_FOUND",
        ),
        (
            code::BIZ_WORK_TYPE_NOT_FOUND,
            StatusCode::NOT_FOUND,
            "BIZ_WORK_TYPE_NOT_FOUND",
        ),
        (
            code::BIZ_APPLICANT_NOT_FOUND,
            StatusCode::NOT_FOUND,
            "BIZ_APPLICANT_NOT_FOUND",
        ),
        (
            code::BIZ_PART_FILE_NOT_FOUND,
            StatusCode::NOT_FOUND,
            "BIZ_PART_FILE_NOT_FOUND",
        ),
        (
            code::BIZ_OUTSOURCE_COMPANY_NOT_FOUND,
            StatusCode::NOT_FOUND,
            "BIZ_OUTSOURCE_COMPANY_NOT_FOUND",
        ),
        (
            code::BIZ_OUTSOURCE_QUOTE_NOT_FOUND,
            StatusCode::NOT_FOUND,
            "BIZ_OUTSOURCE_QUOTE_NOT_FOUND",
        ),
        (
            code::BIZ_DELIVERY_NOTE_NOT_FOUND,
            StatusCode::NOT_FOUND,
            "BIZ_DELIVERY_NOTE_NOT_FOUND",
        ),
        (
            code::BIZ_DELIVERY_GROUP_NOT_FOUND,
            StatusCode::NOT_FOUND,
            "BIZ_DELIVERY_GROUP_NOT_FOUND",
        ),
        (
            code::BIZ_DELIVERY_SCAN_UNKNOWN_CODE,
            StatusCode::NOT_FOUND,
            "BIZ_DELIVERY_SCAN_UNKNOWN_CODE",
        ),
        (
            code::BIZ_OUTSOURCE_SHIPMENT_NOT_FOUND,
            StatusCode::NOT_FOUND,
            "BIZ_OUTSOURCE_SHIPMENT_NOT_FOUND",
        ),
        (
            code::BIZ_PROCESS_CHAIN_NOT_FOUND,
            StatusCode::NOT_FOUND,
            "BIZ_PROCESS_CHAIN_NOT_FOUND",
        ),
        (
            code::BIZ_PROCESS_CHAIN_STEP_NOT_FOUND,
            StatusCode::NOT_FOUND,
            "BIZ_PROCESS_CHAIN_STEP_NOT_FOUND",
        ),
        (
            code::BIZ_PROCESS_CHAIN_PART_NOT_PENDING,
            StatusCode::CONFLICT,
            "BIZ_PROCESS_CHAIN_PART_NOT_PENDING",
        ),
        (
            code::BIZ_PROCESS_CHAIN_REQUIRED,
            StatusCode::CONFLICT,
            "BIZ_PROCESS_CHAIN_REQUIRED",
        ),
        // 2026-09-29 新增：prod/batch dispatch 阶段 status 守卫（409）
        (
            code::BIZ_BATCH_INVALID_STATUS,
            StatusCode::CONFLICT,
            "BIZ_BATCH_INVALID_STATUS",
        ),
        // 2026-09-29 新增：prod/batch dispatch 按 batch_id 查不到（404）
        (
            code::BIZ_BATCH_NOT_FOUND,
            StatusCode::NOT_FOUND,
            "BIZ_BATCH_NOT_FOUND",
        ),
        // 2026-09-30 新增：move 路径 from/holder 不一致 → 409
        (
            code::BIZ_BATCH_LOCATION_MISMATCH,
            StatusCode::CONFLICT,
            "BIZ_BATCH_LOCATION_MISMATCH",
        ),
        // 2026-09-30 新增：force-complete 端点已 COMPLETED 幂等拒绝 → 409
        (
            code::BIZ_PART_ALREADY_COMPLETED,
            StatusCode::CONFLICT,
            "BIZ_PART_ALREADY_COMPLETED",
        ),
        // 2026-10-11 新增：装配件级 force-complete 两条幂等拒绝 → 409
        (
            code::BIZ_ASSEMBLY_ALREADY_COMPLETED,
            StatusCode::CONFLICT,
            "BIZ_ASSEMBLY_ALREADY_COMPLETED",
        ),
        (
            code::BIZ_ASSEMBLY_ALREADY_CANCELLED,
            StatusCode::CONFLICT,
            "BIZ_ASSEMBLY_ALREADY_CANCELLED",
        ),
        // 2026-09-28 删除：相关会话域 HTTP 表覆盖（域下线，码段释放）。
        // 2xxxx 显式 409
        (
            code::BIZ_USER_DUPLICATE,
            StatusCode::CONFLICT,
            "BIZ_USER_DUPLICATE",
        ),
        (
            code::BIZ_USER_DUPLICATE_USERNAME,
            StatusCode::CONFLICT,
            "BIZ_USER_DUPLICATE_USERNAME",
        ),
        (
            code::BIZ_USER_ROLE_DUPLICATE,
            StatusCode::CONFLICT,
            "BIZ_USER_ROLE_DUPLICATE",
        ),
        (
            code::BIZ_PART_PRICE_LOCKED_BY_ASSEMBLY,
            StatusCode::CONFLICT,
            "BIZ_PART_PRICE_LOCKED_BY_ASSEMBLY",
        ),
        (
            code::BIZ_PART_QUANTITY_LOCKED,
            StatusCode::CONFLICT,
            "BIZ_PART_QUANTITY_LOCKED",
        ),
        (
            code::BIZ_CUSTOMER_IN_USE,
            StatusCode::CONFLICT,
            "BIZ_CUSTOMER_IN_USE",
        ),
        (
            code::BIZ_CUSTOMER_HAS_CHILDREN,
            StatusCode::BAD_REQUEST,
            "BIZ_CUSTOMER_HAS_CHILDREN",
        ),
        (
            code::BIZ_WORKER_IN_USE,
            StatusCode::CONFLICT,
            "BIZ_WORKER_IN_USE",
        ),
        (
            code::BIZ_WORKER_HOLD_LIMIT_EXCEEDED,
            StatusCode::CONFLICT,
            "BIZ_WORKER_HOLD_LIMIT_EXCEEDED",
        ),
        (
            code::BIZ_PART_BATCH_NOT_HELD_BY_WORKER,
            StatusCode::CONFLICT,
            "BIZ_PART_BATCH_NOT_HELD_BY_WORKER",
        ),
        (
            code::BIZ_SHELF_DUPLICATE_CODE,
            StatusCode::CONFLICT,
            "BIZ_SHELF_DUPLICATE_CODE",
        ),
        (
            code::BIZ_SHELF_IN_USE,
            StatusCode::CONFLICT,
            "BIZ_SHELF_IN_USE",
        ),
        (
            code::BIZ_PROCESS_DUPLICATE_CODE,
            StatusCode::CONFLICT,
            "BIZ_PROCESS_DUPLICATE_CODE",
        ),
        (
            code::BIZ_PROCESS_IN_USE,
            StatusCode::CONFLICT,
            "BIZ_PROCESS_IN_USE",
        ),
        (
            code::BIZ_WORK_TYPE_DUPLICATE_CODE,
            StatusCode::CONFLICT,
            "BIZ_WORK_TYPE_DUPLICATE_CODE",
        ),
        (
            code::BIZ_WORK_TYPE_IN_USE,
            StatusCode::CONFLICT,
            "BIZ_WORK_TYPE_IN_USE",
        ),
        (
            code::BIZ_APPLICANT_DUPLICATE_NAME,
            StatusCode::CONFLICT,
            "BIZ_APPLICANT_DUPLICATE_NAME",
        ),
        (
            code::BIZ_APPLICANT_IN_USE,
            StatusCode::CONFLICT,
            "BIZ_APPLICANT_IN_USE",
        ),
        (
            code::BIZ_PART_FILE_DUPLICATE,
            StatusCode::CONFLICT,
            "BIZ_PART_FILE_DUPLICATE",
        ),
        (
            code::BIZ_OUTSOURCE_COMPANY_DUPLICATE,
            StatusCode::CONFLICT,
            "BIZ_OUTSOURCE_COMPANY_DUPLICATE",
        ),
        (
            code::BIZ_OUTSOURCE_COMPANY_DUPLICATE_NAME,
            StatusCode::CONFLICT,
            "BIZ_OUTSOURCE_COMPANY_DUPLICATE_NAME",
        ),
        (
            code::BIZ_OUTSOURCE_COMPANY_IN_USE,
            StatusCode::CONFLICT,
            "BIZ_OUTSOURCE_COMPANY_IN_USE",
        ),
        (
            code::BIZ_OUTSOURCE_QUOTE_DUPLICATE,
            StatusCode::CONFLICT,
            "BIZ_OUTSOURCE_QUOTE_DUPLICATE",
        ),
        (
            code::BIZ_DELIVERY_NOTE_PART_ALREADY_ASSIGNED,
            StatusCode::CONFLICT,
            "BIZ_DELIVERY_NOTE_PART_ALREADY_ASSIGNED",
        ),
        (
            code::BIZ_DELIVERY_NOTE_PARTS_LOCKED,
            StatusCode::CONFLICT,
            "BIZ_DELIVERY_NOTE_PARTS_LOCKED",
        ),
        (
            code::BIZ_DELIVERY_GROUP_DUPLICATE_NAME,
            StatusCode::CONFLICT,
            "BIZ_DELIVERY_GROUP_DUPLICATE_NAME",
        ),
        (
            code::BIZ_DELIVERY_GROUP_MEMBER_CONFLICT,
            StatusCode::CONFLICT,
            "BIZ_DELIVERY_GROUP_MEMBER_CONFLICT",
        ),
        (
            code::BIZ_DELIVERY_NOTE_DRAFT_SCOPE_CONFLICT,
            StatusCode::CONFLICT,
            "BIZ_DELIVERY_NOTE_DRAFT_SCOPE_CONFLICT",
        ),
        (
            code::BIZ_PART_ALREADY_CANCELLED,
            StatusCode::CONFLICT,
            "BIZ_PART_ALREADY_CANCELLED",
        ),
        (
            code::BIZ_DELIVERY_NOTE_LOCKED_PART,
            StatusCode::CONFLICT,
            "BIZ_DELIVERY_NOTE_LOCKED_PART",
        ),
        // 2xxxx 显式 422
        (
            code::BIZ_DELIVERY_PRINT_BAD_ORDER,
            StatusCode::UNPROCESSABLE_ENTITY,
            "BIZ_DELIVERY_PRINT_BAD_ORDER",
        ),
        (
            code::BIZ_SHELF_PROCESS_NOT_MAPPED,
            StatusCode::UNPROCESSABLE_ENTITY,
            "BIZ_SHELF_PROCESS_NOT_MAPPED",
        ),
        // 2xxxx 默认 400 兜底
        (
            code::BIZ_INVALID_TRANSITION,
            StatusCode::BAD_REQUEST,
            "BIZ_INVALID_TRANSITION",
        ),
        (
            code::BIZ_INVALID_VALUE,
            StatusCode::BAD_REQUEST,
            "BIZ_INVALID_VALUE",
        ),
        // 2026-10-16：`print_template` 域自产 21109 ⇒ 显式登记 422
        // （原先是 400，python 转发时代的形态；显式登记是为了压住下面
        //  `(20000..30000) => BAD_REQUEST` 的兜底，见 `status_from_code` 的注记）。
        (
            code::BIZ_DELIVERY_TEMPLATE_NOT_CONFIGURED,
            StatusCode::UNPROCESSABLE_ENTITY,
            "BIZ_DELIVERY_TEMPLATE_NOT_CONFIGURED",
        ),
        (
            code::BIZ_DELIVERY_NOTE_SCOPE_MISMATCH,
            StatusCode::BAD_REQUEST,
            "BIZ_DELIVERY_NOTE_SCOPE_MISMATCH",
        ),
        (
            code::BIZ_DELIVERY_ASSEMBLY_PARTS_NOT_READY,
            StatusCode::BAD_REQUEST,
            "BIZ_DELIVERY_ASSEMBLY_PARTS_NOT_READY",
        ),
        (
            code::BIZ_PART_NOT_DELIVERED,
            StatusCode::BAD_REQUEST,
            "BIZ_PART_NOT_DELIVERED",
        ),
        (
            code::BIZ_PART_NOT_READY_TO_SHIP,
            StatusCode::BAD_REQUEST,
            "BIZ_PART_NOT_READY_TO_SHIP",
        ),
        (
            code::BIZ_PART_REPAIR_NOT_TRIGGERED,
            StatusCode::BAD_REQUEST,
            "BIZ_PART_REPAIR_NOT_TRIGGERED",
        ),
        (
            code::BIZ_PART_NOT_DELETABLE,
            StatusCode::CONFLICT,
            "BIZ_PART_NOT_DELETABLE",
        ),
        // 2026-09-28 新增：rust → python STS 转发失败 → 502 BAD_GATEWAY
        (
            code::BIZ_STS_FORWARD_FAILED,
            StatusCode::BAD_GATEWAY,
            "BIZ_STS_FORWARD_FAILED",
        ),
        // 2026-10-03 新增：rust → python 打印转发失败 → 502 BAD_GATEWAY
        (
            code::BIZ_PRINT_FORWARD_FAILED,
            StatusCode::BAD_GATEWAY,
            "BIZ_PRINT_FORWARD_FAILED",
        ),
        // 2026-10-13 PR2：CI 预览不支持的文件类型 → 400（显式登记，不靠 2xxxx 兜底）
        (
            code::BIZ_PART_FILE_PREVIEW_UNSUPPORTED,
            StatusCode::BAD_REQUEST,
            "BIZ_PART_FILE_PREVIEW_UNSUPPORTED",
        ),
        // 2026-10-13 PR2：CI job 终态 Failed → 502（与 STS / 打印转发同族）
        (
            code::BIZ_PART_FILE_PREVIEW_FAILED,
            StatusCode::BAD_GATEWAY,
            "BIZ_PART_FILE_PREVIEW_FAILED",
        ),
        // 2026-10-13 PR2：打 python 的 CI 转发失败 → 502
        (
            code::BIZ_PART_FILE_PREVIEW_FORWARD_FAILED,
            StatusCode::BAD_GATEWAY,
            "BIZ_PART_FILE_PREVIEW_FORWARD_FAILED",
        ),
    ];

    #[test]
    fn status_from_code_is_correct_for_every_known_code() {
        for &(c, expected, name) in HTTP_TABLE_CASES {
            let err = AppError::biz(c, "x");
            let actual = err.http_status();
            assert_eq!(
                actual, expected,
                "status_from_code({c} / {name}): got {actual:?}, want {expected:?}"
            );
            assert_eq!(err.code(), c, "{name} code() 必须是 {c}");
        }
    }

    #[test]
    fn status_from_code_fallback_for_out_of_range() {
        // 任何不在表里的码 → 500（catch-all 不变）
        assert_eq!(
            AppError::biz(99_999, "x").http_status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        // 任何 2xxxx 但不在显式映射里 → 400（Python BizError 默认）
        assert_eq!(
            AppError::biz(20_999, "x").http_status(),
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    #[allow(deprecated)]
    fn deprecated_alias_resolves_to_canonical_code() {
        // 21110 → 21407（编译期）
        assert_eq!(
            code::BIZ_DELIVERY_PARTS_MULTIPLE_CUSTOMERS,
            code::BIZ_DELIVERY_NOTE_PARTS_MULTIPLE_CUSTOMERS
        );
        assert_eq!(code::BIZ_DELIVERY_PARTS_MULTIPLE_CUSTOMERS, 21407);
    }

    #[test]
    fn biz_with_status_overrides_table() {
        // 40100 表里 → 401；显式覆盖为 418
        let err = AppError::biz_with_status(code::UNAUTHORIZED, "teapot", StatusCode::IM_A_TEAPOT);
        assert_eq!(err.code(), code::UNAUTHORIZED);
        assert_eq!(err.http_status(), StatusCode::IM_A_TEAPOT);
        assert_eq!(err.to_string(), "[40100] teapot");
    }

    #[test]
    fn biz_with_status_preserves_code_in_envelope() {
        // 20606 NO_ROLE 表里 → 403；biz_with_status 也走 403（一致）
        let err = AppError::biz_with_status(code::NO_ROLE, "no role", StatusCode::FORBIDDEN);
        assert_eq!(err.http_status(), StatusCode::FORBIDDEN);
        // 与表驱动形式结果一致
        let err2 = AppError::biz(code::NO_ROLE, "no role");
        assert_eq!(err.http_status(), err2.http_status());
    }

    #[test]
    fn biz_with_status_renders_in_response() {
        let err = AppError::biz_with_status(20109, "missing batch", StatusCode::NOT_FOUND);
        let s = err.to_string();
        assert!(s.contains("20109"), "Display 必须包含 code: {s}");
        assert!(s.contains("missing batch"), "Display 必须包含 message: {s}");
    }

    #[test]
    fn biz_with_failures_carries_failures_into_envelope() {
        let err = AppError::BizWithFailures {
            code: code::BIZ_DELIVERY_ASSEMBLY_PARTS_NOT_READY,
            message: "整套拒绝".to_string(),
            http: StatusCode::BAD_REQUEST,
            failures: vec![
                serde_json::json!({
                    "part_id": "1001",
                    "batch_id": "2001",
                    "drawing_no": "DWG-001",
                    "status": "IN_PROCESS",
                    "serial_no": "B01",
                    "name": "fala-A",
                    "reason": "status=IN_PROCESS",
                }),
                serde_json::json!({
                    "part_id": "1002",
                    "batch_id": "2002",
                    "drawing_no": "DWG-002",
                    "status": "INSPECTION",
                    "serial_no": "B02",
                    "name": "fala-B",
                    "reason": "on note DN-20260821-0001",
                }),
            ],
        };
        assert_eq!(err.code(), code::BIZ_DELIVERY_ASSEMBLY_PARTS_NOT_READY);
        assert_eq!(err.http_status(), StatusCode::BAD_REQUEST);
        let s = err.to_string();
        assert!(s.contains("21418"));
        assert!(s.contains("整套拒绝"));
        if let AppError::BizWithFailures { failures, .. } = &err {
            assert_eq!(failures.len(), 2);
            assert_eq!(failures[0]["part_id"], "1001");
            assert_eq!(failures[0]["batch_id"], "2001");
            assert_eq!(failures[0]["drawing_no"], "DWG-001");
            assert_eq!(failures[0]["status"], "IN_PROCESS");
            assert_eq!(failures[1]["part_id"], "1002");
        } else {
            panic!("expected BizWithFailures variant");
        }
    }
}
