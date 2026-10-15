//! `hr` 域班次端点的集成测试（**2026-10-19 新建**）：端点 4/5/6/7 —— 班次 CRUD +
//! OCC + 时段校验 V1–V6 的失败矩阵。
//!
//! ## 为什么要在这里重跑一遍 V1–V6
//!
//! `service::validate_segments` 的表驱动单测（lib，18 条单测里占一大半）已经逐例钉到
//! 错误码；本文件钉的是**过 HTTP 之后**的那一层：错误码真的进了 `R` 信封、HTTP status
//! 真的映射成 400、且 **`21903` 与 `21908` 没被合并**（前端要能只对「跨度超一天」给一条
//! 专门文案）。

use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::{
    assert_biz_code, bootstrap, create_body, create_shift, date, deactivate_shift, ghost_id, id_of,
    post_as_manager, post_raw, segs, unique_code, version_of,
};

// ===========================================================================
//  1. CRUD 契约
// ===========================================================================

/// 建 → 列出 → 改 → 停用 一条龙；顺带钉三个**派生列**与入参一致。
///
/// ⚠️ `overflow_minutes` 用**分钟**而不是小时：段值允许 `:30`（`26:30` = 次日 02:30），
/// 截断成整小时会把 90 分钟的溢出报成 1 小时 —— 所以这里用 `22:00 → 26:30` 这种带分钟的
/// 形态，让「被截成整小时」的回归当场可见（240 而**不是** 2）。
#[tokio::test]
async fn shift_crud_with_derived_columns() {
    let ctx = bootstrap().await;
    let code = unique_code("CRUD");

    // 建：夜班 22:00 → 次日 02:30（跨天 150 分钟）
    let (status, env) = post_as_manager(
        &ctx,
        "/hr/shifts",
        json!({
            "code": code,
            "name": "夜班",
            "color": "#2c6cb8",
            "segments": segs(&[("22:00", "26:30")]),
            "sort_order": 5,
            "remark": "CRUD 回归",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{env}");
    let created = env["data"].clone();
    let id = id_of(&created);

    // 雪花 id 出参必须是 JSON string（不是数字）
    assert!(
        created["id"].is_string(),
        "雪花 id 出参必须是 JSON string：{created}"
    );
    assert_eq!(created["version"], Value::from(0), "新建班次 version=0");
    assert_eq!(created["is_active"], Value::Bool(true));
    assert_eq!(created["sort_order"], Value::from(5));

    // 三个派生列
    assert_eq!(
        created["cross_day"],
        Value::Bool(true),
        "26:30 > 24:00 ⇒ 跨天"
    );
    assert_eq!(
        created["overflow_minutes"],
        Value::from(150),
        "溢出用**分钟**（26:30 - 24:00 = 2.5h = 150min，截成整小时就错）：{created}"
    );
    assert_eq!(
        created["daily_minutes"],
        Value::from(270),
        "各段时长之和、无折算（26:30 - 22:00 = 4.5h）：{created}"
    );
    // 段值已被规范化补零（输入就是两位，这例看不出补零，但出参形状要钉住）
    assert_eq!(
        created["segments"][0]["start"],
        Value::String("22:00".into())
    );
    assert_eq!(created["segments"][0]["end"], Value::String("26:30".into()));
    assert_eq!(created["segments"][0]["seq"], Value::from(1));

    // 列出：默认**不含**停用项，但新建的必须在
    let (status, env) = crate::get_as_manager(&ctx, "/hr/shifts").await;
    assert_eq!(status, StatusCode::OK, "{env}");
    let listed = env["data"]["items"].as_array().expect("items").clone();
    let row = listed
        .iter()
        .find(|r| r["id"] == json!(id.to_string()))
        .unwrap_or_else(|| panic!("新建的班次必须在列表里：{listed:?}"));
    for key in ["cross_day", "overflow_minutes", "daily_minutes"] {
        assert_eq!(row[key], created[key], "列表出参的 {key} 与建时一致");
    }
    assert_eq!(row["segments"], created["segments"]);

    // 改：只带 name（部分覆盖），version 对得上
    let (status, env) = post_as_manager(
        &ctx,
        &format!("/hr/shifts/{id}"),
        json!({ "version": version_of(&created), "name": "夜班（改）" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{env}");
    let updated = env["data"].clone();
    assert_eq!(updated["name"], json!("夜班（改）"));
    assert_eq!(
        updated["color"],
        json!("#2c6cb8"),
        "没传的列必须保持原值（部分覆盖，不是全量替换）：{updated}"
    );
    assert_eq!(
        updated["segments"], created["segments"],
        "没传 segments ⇒ 时段不变"
    );
    assert_eq!(
        updated["version"],
        Value::from(1),
        "成功更新后 version 自增"
    );

    // 停用
    deactivate_shift(&ctx, id, version_of(&updated)).await;
    let (status, env) = crate::get_as_manager(&ctx, "/hr/shifts?include_inactive=true").await;
    assert_eq!(status, StatusCode::OK, "{env}");
    let row = env["data"]["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|r| r["id"] == json!(id.to_string()))
        .expect("include_inactive=true 时停用项也在")
        .clone();
    assert_eq!(row["is_active"], Value::Bool(false));

    // 默认列表**不含**停用项
    let (_, env) = crate::get_as_manager(&ctx, "/hr/shifts").await;
    assert!(
        !env["data"]["items"]
            .as_array()
            .expect("items")
            .iter()
            .any(|r| r["id"] == json!(id.to_string())),
        "默认 include_inactive=false，停用项不应出现"
    );
}

/// 停用**不软删**班次行（`deleted_at` 保持 NULL），停用项仍可按 id 读到。
#[tokio::test]
async fn deactivating_does_not_soft_delete_the_shift_row() {
    let ctx = bootstrap().await;
    let shift = create_shift(&ctx, &unique_code("SD"), &[("08:00", "17:00")]).await;
    let id = id_of(&shift);
    deactivate_shift(&ctx, id, version_of(&shift)).await;

    let deleted_at: Option<chrono::NaiveDateTime> =
        sqlx::query_scalar("SELECT deleted_at FROM t_hr_shift WHERE id = $1")
            .bind(id)
            .fetch_one(&ctx.pool)
            .await
            .expect("读 t_hr_shift.deleted_at");
    assert!(deleted_at.is_none(), "停用走 is_active，不是软删");

    // 重复停用 → 21906（幂等拒绝）
    let version_after = version_of(&shift) + 1;
    let (status, env) = post_as_manager(
        &ctx,
        &format!("/hr/shifts/{id}/deactivate"),
        json!({ "version": version_after }),
    )
    .await;
    assert_biz_code(status, &env, 21906, StatusCode::CONFLICT);
}

// ===========================================================================
//  2. OCC
// ===========================================================================

/// OCC：`version` 不对 → `40901`（HTTP 409）；缺 `version` → **axum 的 422 纯文本**
/// （不是业务信封）。
///
/// ⚠️ 两条形态都必须是 HTTP 409 / 422 的**纯文本对照**：缺字段走不到 service，是 axum
/// `Json` 提取器直接拒的。
#[tokio::test]
async fn occ_stale_version_is_40901_and_missing_version_is_plain_text_422() {
    let ctx = bootstrap().await;
    let shift = create_shift(&ctx, &unique_code("OCC"), &[("08:00", "17:00")]).await;
    let id = id_of(&shift);

    // 陈旧 version → 40901
    let (status, env) = post_as_manager(
        &ctx,
        &format!("/hr/shifts/{id}"),
        json!({ "version": version_of(&shift) + 99, "name": "撞了" }),
    )
    .await;
    assert_biz_code(status, &env, 40901, StatusCode::CONFLICT);

    // 缺 version → 422 纯文本（用 send_raw，send 会在 JSON 解析处 panic）
    let (status, raw) = post_raw(
        &ctx,
        "POST",
        &format!("/hr/shifts/{id}"),
        json!({ "name": "没有 version" }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "缺 version 应是 axum 提取器的 422：{raw}"
    );
    assert!(
        raw.contains("version"),
        "422 的纯文本 body 应点名缺失字段：{raw}"
    );

    // 停用端点同样收 version
    let (status, raw) = post_raw(
        &ctx,
        "POST",
        &format!("/hr/shifts/{id}/deactivate"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{raw}");
}

// ===========================================================================
//  3. 唯一性与在停用上的写操作
// ===========================================================================

/// 班次 `code` 重复 → `21902` + HTTP **409**（**不是** 40901，后者只表达 OCC）。
#[tokio::test]
async fn duplicate_code_is_21902_not_a_version_conflict() {
    let ctx = bootstrap().await;
    let code = unique_code("DUP");
    create_shift(&ctx, &code, &[("08:00", "17:00")]).await;

    // 建第二个同 code
    let (status, env) = post_as_manager(
        &ctx,
        "/hr/shifts",
        create_body(&code, &[("08:00", "17:00")]),
    )
    .await;
    assert_biz_code(status, &env, 21902, StatusCode::CONFLICT);
    assert_ne!(
        env["code"].as_i64(),
        Some(40901),
        "业务唯一键冲突必须与 OCC 冲突分码"
    );

    // 改成别人已占用的 code 也是 21902（不是 40901）
    let other = create_shift(&ctx, &unique_code("DUP2"), &[("08:00", "17:00")]).await;
    let other_id = id_of(&other);
    let (status, env) = post_as_manager(
        &ctx,
        &format!("/hr/shifts/{other_id}"),
        json!({ "version": version_of(&other), "code": code }),
    )
    .await;
    assert_biz_code(status, &env, 21902, StatusCode::CONFLICT);

    // 改成**自己的** code 合法（自冲突被 exclude_id 排除）
    let (status, env) = post_as_manager(
        &ctx,
        &format!("/hr/shifts/{other_id}"),
        json!({ "version": version_of(&other), "code": other["code"] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "改成自己的 code 应放行：{env}");

    // 幂等：同名再建一次仍 21902
    let (status, env) = post_as_manager(
        &ctx,
        "/hr/shifts",
        create_body(&code, &[("08:00", "17:00")]),
    )
    .await;
    assert_biz_code(status, &env, 21902, StatusCode::CONFLICT);
}

/// 改 / 停用一个不存在的班次 → `21901`（HTTP 404）。
#[tokio::test]
async fn writing_a_ghost_shift_is_404() {
    let ctx = bootstrap().await;
    let ghost = ghost_id();
    let (status, env) = post_as_manager(
        &ctx,
        &format!("/hr/shifts/{ghost}"),
        json!({ "version": 0, "name": "幽灵" }),
    )
    .await;
    assert_biz_code(status, &env, 21901, StatusCode::NOT_FOUND);

    let (status, env) = post_as_manager(
        &ctx,
        &format!("/hr/shifts/{ghost}/deactivate"),
        json!({ "version": 0 }),
    )
    .await;
    assert_biz_code(status, &env, 21901, StatusCode::NOT_FOUND);
}

/// 对**已停用**班次写 → `21906`（HTTP 409）：改班次 / 写覆盖行 / 清覆盖 三条都试。
#[tokio::test]
async fn writing_an_inactive_shift_is_21906() {
    let ctx = bootstrap().await;
    let shift = create_shift(&ctx, &unique_code("INA"), &[("08:00", "17:00")]).await;
    let id = id_of(&shift);
    let version_after = version_of(&shift) + 1;
    deactivate_shift(&ctx, id, version_of(&shift)).await;

    let (status, env) = post_as_manager(
        &ctx,
        &format!("/hr/shifts/{id}"),
        json!({ "version": version_after, "name": "还想改" }),
    )
    .await;
    assert_biz_code(status, &env, 21906, StatusCode::CONFLICT);

    let d = date(0);
    let (status, env) = post_as_manager(
        &ctx,
        &format!("/hr/shifts/{id}/calendar/days"),
        json!({ "items": [{ "cal_date": d, "day_type": "WORKDAY" }] }),
    )
    .await;
    assert_biz_code(status, &env, 21906, StatusCode::CONFLICT);

    let (status, env) = post_as_manager(
        &ctx,
        &format!("/hr/shifts/{id}/calendar/days/delete"),
        json!({ "dates": [d] }),
    )
    .await;
    assert_biz_code(status, &env, 21906, StatusCode::CONFLICT);
}

/// 建班次**缺 `segments`** → axum `Json` 提取器的 422 纯文本（没有时段的班次没有意义）。
#[tokio::test]
async fn missing_segments_is_plain_text_422() {
    let ctx = bootstrap().await;
    let (status, raw) = post_raw(
        &ctx,
        "POST",
        "/hr/shifts",
        json!({ "code": unique_code("NOSE"), "name": "没时段" }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{raw}");
    assert!(raw.contains("segments"), "422 应点名缺失字段：{raw}");
}

// ===========================================================================
//  4. 时段校验 V1–V6 的失败矩阵（过 HTTP 那一层）
// ===========================================================================

/// **7 例段形态问题（V1–V5）全部落 `21903`** + HTTP 400。
#[tokio::test]
async fn segment_shape_violations_all_reach_21903() {
    let ctx = bootstrap().await;
    let cases: Vec<(&str, Value)> = vec![
        ("零长段（V2 严格小于）", segs(&[("08:00", "08:00")])),
        (
            "乱序（V4a）",
            segs(&[("13:00", "17:00"), ("08:00", "12:00")]),
        ),
        (
            "段内重叠（V4b）",
            segs(&[("08:00", "13:00"), ("12:00", "17:00")]),
        ),
        ("首段起点 >= 24:00（V3）", segs(&[("24:00", "28:00")])),
        ("超 48 小时（V1 上界）", segs(&[("08:00", "49:00")])),
        ("空数组（V5 下界）", segs(&[])),
        (
            "超 6 段（V5 上界）",
            segs(&[
                ("01:00", "02:00"),
                ("03:00", "04:00"),
                ("05:00", "06:00"),
                ("07:00", "08:00"),
                ("09:00", "10:00"),
                ("11:00", "12:00"),
                ("13:00", "14:00"),
            ]),
        ),
    ];

    for (label, segments) in cases {
        let (status, env) = post_as_manager(
            &ctx,
            "/hr/shifts",
            json!({ "code": unique_code("SEG"), "name": label, "segments": segments }),
        )
        .await;
        assert_biz_code(status, &env, 21903, StatusCode::BAD_REQUEST);
        assert!(
            env["message"].as_str().is_some_and(|m| !m.is_empty()),
            "{label} 应带可读消息：{env}"
        );
    }
}

/// **`span > 24:00` 落 `21908` 而不是并进 `21903`** —— 前端要能只对「跨度超一天」给一条
/// 专门文案（它同时是「这个班次排法有问题」而不是「参数格式错了」）。
#[tokio::test]
async fn span_over_a_day_is_21908_not_21903() {
    let ctx = bootstrap().await;
    // V1–V5 全过，只卡 V6。
    let (status, env) = post_as_manager(
        &ctx,
        "/hr/shifts",
        json!({
            "code": unique_code("SPAN"),
            "name": "跨了两天",
            // 08:00 → 20:00、26:00 → 33:00：min(start)=08:00、max(end)=33:00 ⇒ span 25h
            "segments": segs(&[("08:00", "20:00"), ("26:00", "33:00")]),
        }),
    )
    .await;
    assert_biz_code(status, &env, 21908, StatusCode::BAD_REQUEST);
    assert_ne!(env["code"].as_i64(), Some(21903), "21908 与 21903 是两个码");

    // 改班次路径上**同样**是 21908（不能只在建班次时判）
    let shift = create_shift(&ctx, &unique_code("SPAN2"), &[("08:00", "17:00")]).await;
    let (status, env) = post_as_manager(
        &ctx,
        &format!("/hr/shifts/{}", id_of(&shift)),
        json!({
            "version": version_of(&shift),
            "segments": segs(&[("08:00", "20:00"), ("26:00", "33:00")]),
        }),
    )
    .await;
    assert_biz_code(status, &env, 21908, StatusCode::BAD_REQUEST);
}

/// V6 的**边界**：span 恰好 `24:00` 放行、`24:01` 拒。
#[tokio::test]
async fn span_boundary_is_inclusive_at_exactly_24h() {
    let ctx = bootstrap().await;
    // 08:00 → 20:00、20:00 → 32:00：span 恰好 24:00（首尾相接不算重叠）
    let ok = create_shift(
        &ctx,
        &unique_code("BND"),
        &[("08:00", "20:00"), ("20:00", "32:00")],
    )
    .await;
    assert_eq!(ok["cross_day"], Value::Bool(true));
    assert_eq!(
        ok["overflow_minutes"],
        Value::from(480),
        "32:00 - 24:00 = 480min"
    );

    // 24:01 → 21908
    let (status, env) = post_as_manager(
        &ctx,
        "/hr/shifts",
        json!({
            "code": unique_code("BND2"),
            "name": "24:01",
            "segments": segs(&[("08:00", "20:01"), ("20:01", "32:01")]),
        }),
    )
    .await;
    assert_biz_code(status, &env, 21908, StatusCode::BAD_REQUEST);
}

/// **规范化**：入参的 `seq` 乱写 / 段值不补零，落库与出参都被规范化成 `1..n` + `"HH:MM"`。
#[tokio::test]
async fn segments_are_normalized_on_the_way_in() {
    let ctx = bootstrap().await;
    let (status, env) = post_as_manager(
        &ctx,
        "/hr/shifts",
        json!({
            "code": unique_code("NORM"),
            "name": "规范化",
            "segments": [
                { "seq": 9, "start": "8:00", "end": "12:00" },
                { "seq": 3, "start": "13:00", "end": "17:00" },
            ],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{env}");
    let s = &env["data"]["segments"];
    assert_eq!(s[0]["seq"], Value::from(1), "seq 重编为 1..n");
    assert_eq!(s[1]["seq"], Value::from(2));
    assert_eq!(s[0]["start"], Value::String("08:00".into()), "段值补零");
    assert_eq!(s[0]["end"], Value::String("12:00".into()));
    assert_eq!(
        env["data"]["daily_minutes"],
        Value::from(480),
        "两段 4h + 4h"
    );
}

/// 段值畸形（不是 `HH:MM`）与越界（`48:01` / `49:00`）都落 `21903`。
///
/// ⚠️ **HTTP 层只钉「码与非空消息」**：service 对「格式不对」与「越界」给的是两条不同的
/// 文案（前者说 `HH:MM`、后者说上限 `48:00`），而那属于实现细节 —— 前端只按码分文案。
/// 逐条文案由 lib 单测 `clock_parser_accepts_reasonable_and_rejects_out_of_range` 守。
#[tokio::test]
async fn malformed_clock_strings_reach_21903() {
    let ctx = bootstrap().await;
    for bad in [
        "", "0800", "08:0", " 08:00", "08:60", "48:01", "49:00", "-1:00",
    ] {
        let (status, env) = post_as_manager(
            &ctx,
            "/hr/shifts",
            json!({
                "code": unique_code("CLK"),
                "name": "畸形",
                "segments": [{ "seq": 1, "start": bad, "end": "17:00" }],
            }),
        )
        .await;
        assert_biz_code(status, &env, 21903, StatusCode::BAD_REQUEST);
        assert!(
            env["message"].as_str().is_some_and(|m| !m.is_empty()),
            "`{bad}` 的 21903 应带可读消息：{env}"
        );
    }

    // `end` 侧同样校验（不是只查 start）
    let (status, env) = post_as_manager(
        &ctx,
        "/hr/shifts",
        json!({
            "code": unique_code("CLK2"),
            "name": "end 畸形",
            "segments": [{ "seq": 1, "start": "08:00", "end": "17:xx" }],
        }),
    )
    .await;
    assert_biz_code(status, &env, 21903, StatusCode::BAD_REQUEST);
}
