//! `hr` 域日历端点的集成测试（**2026-10-19 新建**）：端点 1/2/3/8/9/10 +
//! **推导规则 6 条**。
//!
//! ## 为什么推导规则必须落在集成测试
//!
//! 「覆盖 > 全局 > 星期兜底」这条链**跨三张表**（`t_hr_shift_calendar` /
//! `t_hr_calendar_day` / 星期函数），service 的单元测试碰不到它；`day_type` 出参的字面量
//! 形态（`REST_DAY` 而不是变体名 `RestDay`）也只有过一遍 HTTP 序列化才算钉住。
//! **这 6 条是本域的业务心脏**，少一条就等于把「节假日该谁上班」判错。

use axum::http::StatusCode;
use chrono::Weekday;
use serde_json::{Value, json};

use crate::{
    Ctx, assert_biz_code, bootstrap, create_shift, date, deactivate_shift, ghost_id,
    global_calendar_map, id_of, live_override_count, post_as_manager, segs, shift_calendar_items,
    version_of, weekday_of, write_global_day, write_override_day,
};

// ===========================================================================
//  1. 推导规则 —— 六条
// ===========================================================================

/// **规则 1：覆盖优先。** 全局日历某日 `STATUTORY_HOLIDAY` + 班次覆盖行
/// `MAKEUP_WORKDAY` ⇒ `effective_day_type = MAKEUP_WORKDAY`、`is_workday = true`、
/// `is_override = true`，而 `global_day_type` 仍是 `STATUTORY_HOLIDAY`。
///
/// ⚠️ **两个日型必须一起断言**：只钉 `effective` 的话，「读端点偷偷把 `global` 也覆盖成
/// 同一个值」这种回归照样全绿 —— 而前端正是靠 `global ≠ effective` 才能在日历上
/// 画出「这天全局放假、但这个班次照常上班」。
#[tokio::test]
async fn override_beats_global_calendar() {
    let ctx = bootstrap().await;
    let holiday = date(0); // 周一，工作日 —— 覆盖成「上班」才有观察价值
    write_global_day(&ctx, &holiday, "STATUTORY_HOLIDAY", Some("测试法定节假日")).await;

    let shift = create_shift(&ctx, &crate::unique_code("OVR"), &[("08:00", "17:00")]).await;
    let id = id_of(&shift);
    write_override_day(&ctx, id, &holiday, "MAKEUP_WORKDAY").await;

    let items = shift_calendar_items(&ctx, id, &holiday, &holiday).await;
    assert_eq!(items.len(), 1, "单日区间就一行：{items:?}");
    assert_eq!(items[0]["cal_date"], json!(holiday));
    assert_eq!(
        items[0]["global_day_type"],
        json!("STATUTORY_HOLIDAY"),
        "全局日历说的那行必须原样回传：{items:?}"
    );
    assert_eq!(
        items[0]["effective_day_type"],
        json!("MAKEUP_WORKDAY"),
        "覆盖行必须压过全局行：{items:?}"
    );
    assert_eq!(items[0]["is_workday"], json!(true), "调休上班是上班日");
    assert_eq!(
        items[0]["is_override"],
        json!(true),
        "有覆盖行 ⇒ is_override"
    );
}

/// **规则 2：继承全局。** 班次在该日**无覆盖行** ⇒ `effective_day_type` 逐字等于
/// `global_day_type`、`is_override = false`。
#[tokio::test]
async fn inherits_global_calendar_when_no_override_row() {
    let ctx = bootstrap().await;
    let rest = date(0);
    let makeup_holiday = date(1);
    write_global_day(&ctx, &rest, "CUSTOM_LEAVE", Some("设备检修")).await;
    write_global_day(&ctx, &makeup_holiday, "MAKEUP_HOLIDAY", None).await;

    let shift = create_shift(&ctx, &crate::unique_code("INH"), &[("08:00", "17:00")]).await;
    let id = id_of(&shift);

    let items = shift_calendar_items(&ctx, id, &rest, &makeup_holiday).await;
    assert_eq!(items.len(), 2, "{items:?}");
    for it in &items {
        assert_eq!(
            it["effective_day_type"], it["global_day_type"],
            "无覆盖行时 effective 必须逐字继承 global：{it}"
        );
        assert_eq!(
            it["is_override"],
            json!(false),
            "无覆盖行 ⇒ is_override=false"
        );
    }
    assert_eq!(items[0]["effective_day_type"], json!("CUSTOM_LEAVE"));
    assert_eq!(items[1]["effective_day_type"], json!("MAKEUP_HOLIDAY"));
}

/// **规则 3：星期兜底。** 全局日历该日**无显式行** + 班次无覆盖行 ⇒ 周六 / 周日给
/// `REST_DAY`、工作日给 `WORKDAY`，且**全局端点 1** 把那些日标 `derived = true`。
///
/// ⚠️ `derived` 是前端区分「有人在管这天」与「系统兜底」的唯一依据（排班表不该把
/// 常态周末显示成一条「被配置过」的标记）⇒ 这条断言不能省。
#[tokio::test]
async fn weekday_fallback_when_no_rows_at_all() {
    let ctx = bootstrap().await;
    // 基准日 2030-03-04 是周一；+5 = 周六、+6 = 周日、+7 = 下周一。
    assert_eq!(weekday_of(0), Weekday::Mon, "基准日必须是周一");
    assert_eq!(weekday_of(5), Weekday::Sat);
    assert_eq!(weekday_of(6), Weekday::Sun);

    let sat = date(5);
    let mon = date(7);

    let shift = create_shift(&ctx, &crate::unique_code("FBK"), &[("08:00", "17:00")]).await;
    let id = id_of(&shift);

    // 端点 8：两端都走星期兜底
    let items = shift_calendar_items(&ctx, id, &sat, &mon).await;
    assert_eq!(items.len(), 3, "{items:?}");
    assert_eq!(
        items[0]["global_day_type"],
        json!("REST_DAY"),
        "周六兜底 REST_DAY"
    );
    assert_eq!(
        items[1]["global_day_type"],
        json!("REST_DAY"),
        "周日兜底 REST_DAY"
    );
    assert_eq!(
        items[2]["global_day_type"],
        json!("WORKDAY"),
        "周一兜底 WORKDAY"
    );
    assert_eq!(items[0]["effective_day_type"], json!("REST_DAY"));
    assert_eq!(items[2]["effective_day_type"], json!("WORKDAY"));
    assert_eq!(items[0]["is_workday"], json!(false));
    assert_eq!(items[2]["is_workday"], json!(true));

    // 端点 1：同样三行，且**全部 derived = true**（库里压根没这三行）
    let days = global_calendar_map(&ctx, &sat, &mon).await;
    assert_eq!(days.len(), 3, "{days:?}");
    for d in &days {
        assert_eq!(
            d["derived"],
            json!(true),
            "无显式行 ⇒ derived=true（前端据此不画「已配置」标记）：{d}"
        );
        assert_eq!(d["name"], Value::Null, "推导日的 name 恒为 null：{d}");
    }
    assert_eq!(days[0]["day_type"], json!("REST_DAY"));
    assert_eq!(days[1]["day_type"], json!("REST_DAY"));
    assert_eq!(days[2]["day_type"], json!("WORKDAY"));
}

/// **规则 4：清除覆盖回到继承。** 写覆盖行（`MAKEUP_HOLIDAY`）→ 调端点 10 清掉 ⇒
/// 退回全局值（`STATUTORY_HOLIDAY`）。
///
/// ⚠️ 同时钉「清覆盖**不需要**补一条与全局同值的行」—— 覆盖行是例外表，删掉就是
/// 「这天按全局来」。
#[tokio::test]
async fn clearing_override_falls_back_to_global() {
    let ctx = bootstrap().await;
    let day = date(0);
    write_global_day(&ctx, &day, "STATUTORY_HOLIDAY", Some("春节")).await;

    let shift = create_shift(&ctx, &crate::unique_code("CLR"), &[("08:00", "17:00")]).await;
    let id = id_of(&shift);

    write_override_day(&ctx, id, &day, "MAKEUP_HOLIDAY").await;
    let before = shift_calendar_items(&ctx, id, &day, &day).await;
    assert_eq!(before[0]["effective_day_type"], json!("MAKEUP_HOLIDAY"));
    assert_eq!(before[0]["is_override"], json!(true));

    let (status, env) = post_as_manager(
        &ctx,
        &format!("/hr/shifts/{id}/calendar/days/delete"),
        json!({ "dates": [day] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "清覆盖应成功：{env}");
    assert_eq!(env["data"]["affected"], json!(1), "恰好删掉一行：{env}");

    let after = shift_calendar_items(&ctx, id, &day, &day).await;
    assert_eq!(
        after[0]["effective_day_type"],
        json!("STATUTORY_HOLIDAY"),
        "{after:?}"
    );
    assert_eq!(after[0]["global_day_type"], json!("STATUTORY_HOLIDAY"));
    assert_eq!(after[0]["is_override"], json!(false), "清掉后无覆盖行");
    assert_eq!(
        live_override_count(&ctx.pool, id).await,
        0,
        "库里也不该留活行"
    );
}

/// **规则 5：停用班次不级联软删覆盖行。** 停用后 `t_hr_shift_calendar` 的活行数与
/// 停用前一致，且端点 8 照常回传那些天。
///
/// ⚠️ 这是**历史考勤**的地基：级联清掉覆盖行后，「某天这个班次按什么口径算的」就只剩
/// 「全局日历 + 星期推导」能重猜，而覆盖行恰恰是当年与全局不同的那些天。
#[tokio::test]
async fn deactivating_shift_does_not_cascade_override_rows() {
    let ctx = bootstrap().await;
    let shift = create_shift(&ctx, &crate::unique_code("CAS"), &[("08:00", "17:00")]).await;
    let id = id_of(&shift);

    let d0 = date(0);
    let d1 = date(1);
    write_override_day(&ctx, id, &d0, "MAKEUP_WORKDAY").await;
    write_override_day(&ctx, id, &d1, "MAKEUP_HOLIDAY").await;
    let before = live_override_count(&ctx.pool, id).await;
    assert_eq!(before, 2, "停用前两行活覆盖：{before}");

    deactivate_shift(&ctx, id, version_of(&shift)).await;

    assert_eq!(
        live_override_count(&ctx.pool, id).await,
        before,
        "停用**不得**级联软删覆盖行"
    );
    // 停用只挡写，读照常（handler 文档口径）。
    let items = shift_calendar_items(&ctx, id, &d0, &d1).await;
    assert_eq!(items[0]["effective_day_type"], json!("MAKEUP_WORKDAY"));
    assert_eq!(
        items[0]["is_override"],
        json!(true),
        "覆盖行仍在生效：{items:?}"
    );
    assert_eq!(items[1]["effective_day_type"], json!("MAKEUP_HOLIDAY"));

    // 但写一律 21906（HTTP 409）—— 停用只挡写、不挡读。
    let (status, env) = post_as_manager(
        &ctx,
        &format!("/hr/shifts/{id}/calendar/days"),
        json!({ "items": [{ "cal_date": date(2), "day_type": "WORKDAY" }] }),
    )
    .await;
    assert_biz_code(status, &env, 21906, StatusCode::CONFLICT);
}

/// **规则 6：区间约束。** 端点 1 与端点 8 都按 `from <= to` + 跨度 ≤ 366 天判，
/// 违规一律 `21904`（HTTP 400）。
#[tokio::test]
async fn range_constraints_on_both_read_endpoints() {
    let ctx = bootstrap().await;
    let shift = create_shift(&ctx, &crate::unique_code("RNG"), &[("08:00", "17:00")]).await;
    let id = id_of(&shift);

    let d0 = date(0);
    let d1 = date(1);
    // 跨度判据是 `to - from > 366` ⇒ 恰好 366 放行、367 拒。
    let ok_far = date(366);
    let over_far = date(367);

    // 边界：from == to 与恰好 366 天都放行
    let (status, _) = crate::get_as_manager(&ctx, &crate::global_cal_path(&d0, &d0)).await;
    assert_eq!(status, StatusCode::OK, "from == to 放行");
    let (status, _) = crate::get_as_manager(&ctx, &crate::global_cal_path(&d0, &ok_far)).await;
    assert_eq!(status, StatusCode::OK, "恰好 366 天放行");

    for (path, label) in [
        (crate::global_cal_path(&d1, &d0), "端点 1 倒序"),
        (crate::global_cal_path(&d0, &over_far), "端点 1 超跨度"),
        (crate::cal_path(id, &d1, &d0), "端点 8 倒序"),
        (crate::cal_path(id, &d0, &over_far), "端点 8 超跨度"),
    ] {
        let (status, env) = crate::get_as_manager(&ctx, &path).await;
        assert_biz_code(status, &env, 21904, StatusCode::BAD_REQUEST);
        assert!(
            env["message"].as_str().is_some_and(|m| !m.is_empty()),
            "{label} 应带可读消息：{env}"
        );
    }
}

// ===========================================================================
//  2. 端点 1 / 2 / 3：全局日历
// ===========================================================================

/// 建一个**指定 `sort_order`** 的班次（色带排序口径用例要造同 `sort_order` 的两个班次，
/// `crate::create_shift` 那版固定走缺省 `0`，造不出来）。
async fn create_shift_ordered(ctx: &Ctx, code_prefix: &str, sort_order: i32) -> Value {
    let code = crate::unique_code(code_prefix);
    let (status, env) = post_as_manager(
        ctx,
        "/hr/shifts",
        json!({
            "code": code,
            "name": format!("色带班次 {code_prefix}"),
            "sort_order": sort_order,
            "segments": segs(&[("08:00", "17:00")]),
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "建班次应成功：{env}");
    env["data"].clone()
}

/// 该行的 `shift_bands`（`CalendarDayOut.shift_bands`），顺带断言「恰好 4 个键」。
fn bands_of(day: &Value) -> Vec<Value> {
    let arr = day["shift_bands"]
        .as_array()
        .unwrap_or_else(|| panic!("shift_bands 是数组（无活跃班次时是空数组）：{day}"));
    for b in arr {
        let obj = b.as_object().expect("band 是对象");
        assert_eq!(
            obj.len(),
            4,
            "band 恰好 4 个键（多一个就是「第二个班次真相源」）：{b}"
        );
        assert!(
            b["shift_id"].is_string(),
            "band.shift_id 必须是 JSON string（雪花防精度截断）：{b}"
        );
    }
    arr.clone()
}

/// 端点 1（2026-10-19 新增）：每行带 `shift_bands` —— **每日 × 每活跃班次**一条色带，
/// 全局日历上直接渲染「所有班次的颜色带」不必再发 N 次 `GET /shifts/{id}/calendar`。
///
/// ⚠️ 这条钉的是**三条口径**：只含**活跃**班次、顺序**稳定**（`sort_order` 升序再
/// `code` 升序）、`is_workday = false` 的班次**照样出现**（滤掉就等于前端拿不到
/// 「这天哪些班次在休」）。
///
/// ⚠️ 其中「`code` 升序」这半截是**唯一**能抓住 tie-break 写错的构造：`AA` / `BB` 的
/// `sort_order` 打平，而**建库顺序刻意让 `id` 序与 `code` 序相反**（`BB` 先建）。
#[tokio::test]
async fn global_calendar_carries_one_band_per_active_shift_in_stable_order() {
    let ctx = bootstrap().await;
    let holiday = date(0); // 周一：全局显式行 = 法定节假日
    let tue = date(1); // 周二：无显式行 ⇒ 兜底 WORKDAY
    write_global_day(&ctx, &holiday, "STATUTORY_HOLIDAY", Some("法定节假日")).await;

    // 排序口径：`sort_order` 升序 → `code` 升序。
    // `ZZ` 的 code 字典序**最末**但 `sort_order` 最小 ⇒ 它必须排第一（证明 code 不能盖过
    // sort_order）；`AA` 与 `BB` 同为 `sort_order = 1` ⇒ 两者之间才按 code 排。
    //
    // ⚠️ **`BB` 必须先于 `AA` 建**（2026-10-19）：`unique_code` 尾部是单调雪花 ⇒
    // 「先建先小」，于是 `id(BB) < id(AA)`，而 code 字典序仍是 `AA < BB`。
    // 这样本用例才真的钉死了 tie-break 是 **`code`** —— 若实现把 `code` 换成 `id`（即
    // 照抄端点 4 `list_shifts` 的兜底键），本用例会红。若两者同序（AA 先建），两种
    // tie-break 给出同样的答案，把实现换成 `id` 这条用例照样全绿 ⇒ 覆盖是假的。
    let zz = create_shift_ordered(&ctx, "ZZ", 0).await;
    let zid = id_of(&zz);
    let bb = create_shift_ordered(&ctx, "BB", 1).await;
    let bid = id_of(&bb);
    let aa = create_shift_ordered(&ctx, "AA", 1).await;
    let aid = id_of(&aa);
    assert!(
        bid < aid,
        "雪花单调 ⇒ BB 先建必然 id 更小，这条前提不成立本用例就失去鉴别力：{bid} vs {aid}"
    );

    // A 班次在法定节假日里照常上班（覆盖行）
    write_override_day(&ctx, aid, &holiday, "MAKEUP_WORKDAY").await;
    // B 班次在**全局是工作日**里独自休息（覆盖行）⇒ is_workday = false 但**照样出现**
    write_override_day(&ctx, bid, &tue, "MAKEUP_HOLIDAY").await;

    let days = global_calendar_map(&ctx, &holiday, &tue).await;
    assert_eq!(days.len(), 2, "{days:?}");

    // ── 法定节假日那天 ──
    let hb = bands_of(&days[0]);
    assert_eq!(
        hb.iter()
            .map(|b| b["shift_id"].as_str().unwrap().to_string())
            .collect::<Vec<_>>(),
        vec![zid.to_string(), aid.to_string(), bid.to_string()],
        "顺序必须是 sort_order 升序（ZZ 最前）再 code 升序（AA 在 BB 前，**与 id 序相反**）：{hb:?}"
    );
    assert_eq!(
        hb[1]["effective_day_type"],
        json!("MAKEUP_WORKDAY"),
        "{hb:?}"
    );
    assert_eq!(hb[1]["is_workday"], json!(true), "调休上班是上班日");
    assert_eq!(hb[1]["is_override"], json!(true), "有覆盖行");
    for b in [&hb[0], &hb[2]] {
        assert_eq!(
            b["effective_day_type"],
            json!("STATUTORY_HOLIDAY"),
            "无覆盖行 ⇒ effective 逐字等于该行的全局 day_type：{b}"
        );
        assert_eq!(b["is_workday"], json!(false));
        assert_eq!(
            b["is_override"],
            json!(false),
            "无覆盖行 ⇒ is_override=false"
        );
    }
    assert_eq!(days[0]["day_type"], json!("STATUTORY_HOLIDAY"), "{days:?}");

    // ── 周二：全局兜底 WORKDAY，B 班次独自休息 ──
    assert_eq!(days[1]["day_type"], json!("WORKDAY"));
    assert_eq!(days[1]["derived"], json!(true));
    let tb = bands_of(&days[1]);
    let tb_b = tb
        .iter()
        .find(|b| b["shift_id"] == json!(bid.to_string()))
        .unwrap();
    assert_eq!(
        tb_b["effective_day_type"],
        json!("MAKEUP_HOLIDAY"),
        "覆盖行压过全局：{tb:?}"
    );
    assert_eq!(
        tb_b["is_workday"],
        json!(false),
        "⚠️ 休息的班次**照样出现在色带里**（只过滤停用班次）：{tb:?}"
    );
    assert_eq!(tb_b["is_override"], json!(true));
    let tb_a = tb
        .iter()
        .find(|b| b["shift_id"] == json!(aid.to_string()))
        .unwrap();
    assert_eq!(tb_a["effective_day_type"], json!("WORKDAY"), "{tb:?}");
    assert_eq!(tb_a["is_workday"], json!(true));

    // ── 停用 ZZ 后：色带里不再有它，其余顺序不变 ──
    deactivate_shift(&ctx, zid, version_of(&zz)).await;
    let days2 = global_calendar_map(&ctx, &holiday, &holiday).await;
    let hb2 = bands_of(&days2[0]);
    assert_eq!(
        hb2.iter()
            .map(|b| b["shift_id"].as_str().unwrap().to_string())
            .collect::<Vec<_>>(),
        vec![aid.to_string(), bid.to_string()],
        "⚠️ 停用班次不排人，不该占一条色带：{hb2:?}"
    );
}

/// 端点 1：区间内**逐日一行，一天都不省**；显式行 `derived = false`、带 `name`。
#[tokio::test]
async fn global_calendar_lists_every_day_of_the_range() {
    let ctx = bootstrap().await;
    let d0 = date(0);
    let d4 = date(4);
    write_global_day(&ctx, &date(2), "STATUTORY_HOLIDAY", Some("中秋")).await;

    let days = global_calendar_map(&ctx, &d0, &d4).await;
    assert_eq!(days.len(), 5, "5 天就要 5 行：{days:?}");
    for (i, d) in days.iter().enumerate() {
        assert_eq!(d["cal_date"], json!(date(i as i64)), "逐日一行且按序");
    }
    let hit = &days[2];
    assert_eq!(hit["day_type"], json!("STATUTORY_HOLIDAY"));
    assert_eq!(hit["name"], json!("中秋"));
    assert_eq!(hit["derived"], json!(false), "有显式行 ⇒ derived=false");
}

/// 端点 2：批量 upsert 的**空 `items` 也落 21904**（不是静默 `affected: 0`）。
///
/// ⚠️ 这条与「删行 `affected = 0` 是合法成功」是**两件事**：写端点的空批是「没说要写
/// 什么」，删行端点的 0 行是「那几天本来就没有显式行」。
#[tokio::test]
async fn upsert_with_empty_items_is_a_range_error_not_a_silent_zero() {
    let ctx = bootstrap().await;
    let (status, env) = post_as_manager(&ctx, "/hr/calendar/days", json!({ "items": [] })).await;
    assert_biz_code(status, &env, 21904, StatusCode::BAD_REQUEST);
}

/// 端点 2：批量 upsert 的**日期跨度同样走 366 天判据**（攒大 `items` 不能绕过上限）。
#[tokio::test]
async fn upsert_batch_span_is_capped_by_the_same_366_day_rule() {
    let ctx = bootstrap().await;
    let (status, env) = post_as_manager(
        &ctx,
        "/hr/calendar/days",
        json!({ "items": [
            { "cal_date": date(0), "day_type": "WORKDAY" },
            { "cal_date": date(367), "day_type": "WORKDAY" },
        ] }),
    )
    .await;
    assert_biz_code(status, &env, 21904, StatusCode::BAD_REQUEST);
}

/// ⚠️ **回归**：**同一条命令里出现同一个 `cal_date` 必须不是 500。**
///
/// PG 的 `INSERT … SELECT … FROM UNNEST(…) ON CONFLICT (cal_date) WHERE deleted_at IS
/// NULL DO UPDATE` 在同批命中同一行两次时抛 `ON CONFLICT DO UPDATE command cannot
/// affect row a second time`（SQLSTATE `21000`）—— 它既不是 `23505` 也不在任何显式档
/// ⇒ 落 `AppError::Database` → `50001` / **HTTP 500**。而「前端一次范围拖选 + 一次补写」
/// 就能造出这种请求，门槛是零。
///
/// 口径是**后写覆盖先写**（与读侧 `into_day_map` 的「脏数据后写覆盖先写」同源），
/// 终值必须是**后出现的那条**。
#[tokio::test]
async fn duplicate_dates_in_one_batch_are_not_500_and_last_write_wins() {
    let ctx = bootstrap().await;
    let d = date(0);

    // 端点 2：先 WORKDAY 后 REST_DAY ⇒ 终值 REST_DAY
    let (status, env) = post_as_manager(
        &ctx,
        "/hr/calendar/days",
        json!({ "items": [
            { "cal_date": d, "day_type": "WORKDAY" },
            { "cal_date": d, "day_type": "REST_DAY" },
        ] }),
    )
    .await;
    assert_ne!(
        status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "同批重复日期绝不能是 500：{env}"
    );
    assert_eq!(
        status,
        StatusCode::OK,
        "同批重复日期按后写覆盖先写放行：{env}"
    );
    assert_eq!(
        env["data"]["affected"],
        json!(1),
        "去重后只剩一行受影响：{env}"
    );
    let days = global_calendar_map(&ctx, &d, &d).await;
    assert_eq!(
        days[0]["day_type"],
        json!("REST_DAY"),
        "终值必须是**后写**那条：{days:?}"
    );
    assert_eq!(days[0]["derived"], json!(false));

    // 反向再钉一次：后写 WORKDAY 时终值必须变回来（不是「第一条赢」的另一种实现）
    let (status, env) = post_as_manager(
        &ctx,
        "/hr/calendar/days",
        json!({ "items": [
            { "cal_date": d, "day_type": "REST_DAY" },
            { "cal_date": d, "day_type": "WORKDAY" },
        ] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{env}");
    let days = global_calendar_map(&ctx, &d, &d).await;
    assert_eq!(days[0]["day_type"], json!("WORKDAY"), "{days:?}");

    // 端点 9：同一批里同一天两条覆盖行也是同一刀（冲突目标是 (shift_id, cal_date)）
    let shift = create_shift(&ctx, &crate::unique_code("DUPDAY"), &[("08:00", "17:00")]).await;
    let id = id_of(&shift);
    let (status, env) = post_as_manager(
        &ctx,
        &format!("/hr/shifts/{id}/calendar/days"),
        json!({ "items": [
            { "cal_date": d, "day_type": "MAKEUP_WORKDAY" },
            { "cal_date": d, "day_type": "MAKEUP_HOLIDAY" },
        ] }),
    )
    .await;
    assert_ne!(
        status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "端点 9 同样不能 500：{env}"
    );
    assert_eq!(status, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["affected"], json!(1), "{env}");
    let items = shift_calendar_items(&ctx, id, &d, &d).await;
    assert_eq!(
        items[0]["effective_day_type"],
        json!("MAKEUP_HOLIDAY"),
        "端点 9 也是后写覆盖先写：{items:?}"
    );
}

/// ⚠️ **回归**：一条 `items` 的**条数**也有上限（366）。
///
/// 跨度判据只看「最小 ~ 最大日期」，所以 367 个**全同日期**的项跨度恒为 0、能绕过去，
/// 却仍会灌进一次巨大的 `UNNEST`。
#[tokio::test]
async fn batch_item_count_is_capped_even_when_all_dates_are_identical() {
    let ctx = bootstrap().await;
    let d = date(0);
    let items: Vec<Value> = (0..=366)
        .map(|_| json!({ "cal_date": d, "day_type": "WORKDAY" }))
        .collect();
    assert_eq!(items.len(), 367, "本例要 367 条（上限 +1）");
    let (status, env) = post_as_manager(&ctx, "/hr/calendar/days", json!({ "items": items })).await;
    assert_biz_code(status, &env, 21904, StatusCode::BAD_REQUEST);

    // 恰好 366 条放行（边界不能跟着一起收紧）
    let items: Vec<Value> = (0..366)
        .map(|_| json!({ "cal_date": d, "day_type": "WORKDAY" }))
        .collect();
    let (status, env) = post_as_manager(&ctx, "/hr/calendar/days", json!({ "items": items })).await;
    assert_eq!(status, StatusCode::OK, "恰好 366 条应放行：{env}");
    assert_eq!(env["data"]["affected"], json!(1), "去重后仍是一行：{env}");
}

/// ⚠️ **回归**：`varchar` 列超长必须是 `21909` / 400，**不是 500**。
///
/// PG 对 `varchar(n)` 超长抛 `22001`（`string_data_right_truncation`），落不进任何显式档
/// ⇒ 会变成 `50001` / HTTP 500、message 被替换成「数据库错误」。`remark` 尤其要紧：
/// 它是人事随手粘贴的自由文本，超长是**日常操作**。
#[tokio::test]
async fn oversized_varchar_columns_are_400_not_500() {
    let ctx = bootstrap().await;

    // 班次四列：code 30 / name 50 / color 20 / remark 200
    let cases: Vec<(&str, Value)> = vec![
        (
            "code 31 字符",
            json!({
                "code": "C".repeat(31),
                "name": "超长编码",
                "segments": segs(&[("08:00", "17:00")]),
            }),
        ),
        (
            "name 51 字符",
            json!({
                "code": crate::unique_code("LEN1"),
                "name": "名".repeat(51),
                "segments": segs(&[("08:00", "17:00")]),
            }),
        ),
        (
            "color 21 字符",
            json!({
                "code": crate::unique_code("LEN2"),
                "name": "超长颜色",
                "color": "#".repeat(21),
                "segments": segs(&[("08:00", "17:00")]),
            }),
        ),
        (
            "remark 201 字符",
            json!({
                "code": crate::unique_code("LEN3"),
                "name": "超长备注",
                "remark": "备".repeat(201),
                "segments": segs(&[("08:00", "17:00")]),
            }),
        ),
    ];
    for (label, body) in cases {
        let (status, env) = post_as_manager(&ctx, "/hr/shifts", body).await;
        assert_ne!(
            status,
            StatusCode::INTERNAL_SERVER_ERROR,
            "{label} 绝不能是 500：{env}"
        );
        assert_biz_code(status, &env, 21909, StatusCode::BAD_REQUEST);
    }

    // 边界放行：30 / 50 / 20 / 200 恰好不拒
    let (status, env) = post_as_manager(
        &ctx,
        "/hr/shifts",
        json!({
            "code": format!("E{}", "C".repeat(29)),
            "name": "名".repeat(50),
            "color": "#".repeat(20),
            "remark": "备".repeat(200),
            "segments": segs(&[("08:00", "17:00")]),
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "恰好等于列宽必须放行：{env}");

    // 日历两列同样要卡：name 50 / remark 200（端点 2）
    for (label, item) in [
        (
            "日历 name 51 字符",
            json!({ "cal_date": date(0), "day_type": "CUSTOM_LEAVE", "name": "节".repeat(51) }),
        ),
        (
            "日历 remark 201 字符",
            json!({ "cal_date": date(1), "day_type": "WORKDAY", "remark": "备".repeat(201) }),
        ),
    ] {
        let (status, env) =
            post_as_manager(&ctx, "/hr/calendar/days", json!({ "items": [item] })).await;
        assert_ne!(status, StatusCode::INTERNAL_SERVER_ERROR, "{label}：{env}");
        assert_biz_code(status, &env, 21909, StatusCode::BAD_REQUEST);
    }

    // 端点 9 的 remark 同样要卡
    let shift = create_shift(&ctx, &crate::unique_code("LENOV"), &[("08:00", "17:00")]).await;
    let id = id_of(&shift);
    let (status, env) = post_as_manager(
        &ctx,
        &format!("/hr/shifts/{id}/calendar/days"),
        json!({ "items": [{ "cal_date": date(0), "day_type": "WORKDAY", "remark": "备".repeat(201) }] }),
    )
    .await;
    assert_ne!(status, StatusCode::INTERNAL_SERVER_ERROR, "{env}");
    assert_biz_code(status, &env, 21909, StatusCode::BAD_REQUEST);
}

/// 端点 2：`CUSTOM_LEAVE` 未填 `name` → `21907`；其它日型不填 `name` 正常放行。
#[tokio::test]
async fn custom_leave_requires_a_name_but_other_day_types_do_not() {
    let ctx = bootstrap().await;

    let (status, env) = post_as_manager(
        &ctx,
        "/hr/calendar/days",
        json!({ "items": [{ "cal_date": date(0), "day_type": "CUSTOM_LEAVE" }] }),
    )
    .await;
    assert_biz_code(status, &env, 21907, StatusCode::BAD_REQUEST);

    // 空白串等同于没填
    let (status, env) = post_as_manager(
        &ctx,
        "/hr/calendar/days",
        json!({ "items": [{ "cal_date": date(1), "day_type": "CUSTOM_LEAVE", "name": "   " }] }),
    )
    .await;
    assert_biz_code(status, &env, 21907, StatusCode::BAD_REQUEST);

    // 其它日型不填 name 正常放行
    let (status, env) = post_as_manager(
        &ctx,
        "/hr/calendar/days",
        json!({ "items": [{ "cal_date": date(2), "day_type": "MAKEUP_HOLIDAY" }] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "非 CUSTOM_LEAVE 不必带 name：{env}");
    assert_eq!(env["data"]["affected"], json!(1));
}

/// 端点 2：未知 `day_type` → `21905`（**不做**大小写折叠、不 trim）。
#[tokio::test]
async fn unknown_day_type_is_rejected_case_sensitively() {
    let ctx = bootstrap().await;
    for bad in ["Workday", "workday", "HOLIDAY", "自定义"] {
        let (status, env) = post_as_manager(
            &ctx,
            "/hr/calendar/days",
            json!({ "items": [{ "cal_date": date(0), "day_type": bad }] }),
        )
        .await;
        assert_biz_code(status, &env, 21905, StatusCode::BAD_REQUEST);
        assert!(
            env["message"]
                .as_str()
                .unwrap_or_default()
                .contains("REST_DAY"),
            "21905 的消息应列出合法值（前端据此提示）：{env}"
        );
    }
}

/// 端点 2 / 9：**同一批里有一条非法就整批不进**（handler 的事务边界）。
#[tokio::test]
async fn a_bad_item_rolls_the_whole_batch_back() {
    let ctx = bootstrap().await;
    let good = date(0);
    let (status, env) = post_as_manager(
        &ctx,
        "/hr/calendar/days",
        json!({ "items": [
            { "cal_date": good, "day_type": "WORKDAY" },
            { "cal_date": date(1), "day_type": "NOT_A_DAY_TYPE" },
        ] }),
    )
    .await;
    assert_biz_code(status, &env, 21905, StatusCode::BAD_REQUEST);

    // 前一条**合法**的行也必须没落库（否则就是「半批进去」的静默脏数据）。
    let days = global_calendar_map(&ctx, &good, &good).await;
    assert_eq!(
        days[0]["derived"],
        json!(true),
        "整批回滚 ⇒ 那行不存在：{days:?}"
    );
}

/// 端点 3：删行后该日**回退按星期推导**；`affected = 0` 是**合法成功**。
#[tokio::test]
async fn deleting_global_rows_falls_back_to_weekday_and_zero_is_success() {
    let ctx = bootstrap().await;
    let sat = date(5);
    let mon = date(7);
    assert_eq!(weekday_of(5), Weekday::Sat);

    // 周六显式写成「上班日」→ 端点 1 必须照它回（derived=false）
    write_global_day(&ctx, &sat, "WORKDAY", None).await;
    let days = global_calendar_map(&ctx, &sat, &sat).await;
    assert_eq!(days[0]["day_type"], json!("WORKDAY"));
    assert_eq!(days[0]["derived"], json!(false));

    // 删掉 → 回退周六兜底 REST_DAY + derived=true
    let (status, env) =
        post_as_manager(&ctx, "/hr/calendar/days/delete", json!({ "dates": [sat] })).await;
    assert_eq!(status, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["affected"], json!(1));
    let days = global_calendar_map(&ctx, &sat, &sat).await;
    assert_eq!(
        days[0]["day_type"],
        json!("REST_DAY"),
        "删掉显式行 ⇒ 回退星期推导"
    );
    assert_eq!(days[0]["derived"], json!(true));

    // 再删一次：affected=0 是合法成功（那几天本来就没有显式行）
    let (status, env) =
        post_as_manager(&ctx, "/hr/calendar/days/delete", json!({ "dates": [sat] })).await;
    assert_eq!(status, StatusCode::OK, "affected=0 不是错误：{env}");
    assert_eq!(env["data"]["affected"], json!(0));

    // 区间形态 `{from,to}` 等价
    write_global_day(&ctx, &mon, "MAKEUP_HOLIDAY", None).await;
    let (status, env) = post_as_manager(
        &ctx,
        "/hr/calendar/days/delete",
        json!({ "from": mon, "to": mon }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["affected"], json!(1));
}

/// 端点 3：删行目标必须**恰好命中一种形态**；四种非法形态一律 `21904`。
#[tokio::test]
async fn delete_target_requires_exactly_one_form() {
    let ctx = bootstrap().await;
    let d = date(0);
    // ⚠️ 「都省 / 只给一半 / 都给」共用 catch-all 的消息（「必须二选一」），
    // 「dates 空数组」走的是另一条更具体的消息 —— 这里只钉**形态全被拒**这个事实，
    // 消息文案不进断言（改文案不必改测试）。
    for body in [
        json!({}),
        json!({ "from": d }),
        json!({ "to": d }),
        json!({ "dates": [d], "from": d, "to": d }),
        json!({ "dates": [] }),
    ] {
        let (status, env) = post_as_manager(&ctx, "/hr/calendar/days/delete", body).await;
        assert_biz_code(status, &env, 21904, StatusCode::BAD_REQUEST);
    }
}

// ===========================================================================
//  3. 端点 8 / 9 / 10：班次日历
// ===========================================================================

/// 端点 8：出参带 `shift_id`（雪花 **JSON string**），停用班次照常可读。
#[tokio::test]
async fn shift_calendar_carries_shift_id_as_json_string() {
    let ctx = bootstrap().await;
    let shift = create_shift(&ctx, &crate::unique_code("SID"), &[("08:00", "17:00")]).await;
    let id = id_of(&shift);
    let d = date(0);

    let (status, env) = crate::get_as_manager(&ctx, &crate::cal_path(id, &d, &d)).await;
    assert_eq!(status, StatusCode::OK, "{env}");
    assert_eq!(
        env["data"]["shift_id"],
        json!(id.to_string()),
        "雪花 id 出参必须是 JSON string：{env}"
    );

    // 停用后仍可读
    deactivate_shift(&ctx, id, version_of(&shift)).await;
    let (status, _) = crate::get_as_manager(&ctx, &crate::cal_path(id, &d, &d)).await;
    assert_eq!(status, StatusCode::OK, "停用只挡写、不挡读");
}

/// 端点 8：班次不存在 / 已软删 → `21901`（HTTP 404）。
///
/// ⚠️ 读端点**也**要判：否则会对着一个不存在的班次返回一整段「全部走推导」的空日历，
/// 看起来像「这个班次不排班」。
#[tokio::test]
async fn shift_calendar_of_a_ghost_shift_is_404() {
    let ctx = bootstrap().await;
    let d = date(0);
    let ghost = ghost_id();
    let (status, env) = crate::get_as_manager(&ctx, &crate::cal_path(ghost, &d, &d)).await;
    assert_biz_code(status, &env, 21901, StatusCode::NOT_FOUND);
}

/// 端点 9：覆盖行**刻意没有 `name` 字段**；`CUSTOM_LEAVE` 覆盖**不**要求 name
/// （21907 只属全局日历 —— 表里根本没那列）。
#[tokio::test]
async fn override_rows_have_no_name_and_custom_leave_needs_none() {
    let ctx = bootstrap().await;
    let shift = create_shift(&ctx, &crate::unique_code("NONAME"), &[("08:00", "17:00")]).await;
    let id = id_of(&shift);
    let d = date(0);

    let (status, env) = post_as_manager(
        &ctx,
        &format!("/hr/shifts/{id}/calendar/days"),
        json!({ "items": [{ "cal_date": d, "day_type": "CUSTOM_LEAVE", "remark": "厂区停电" }] }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "覆盖行的 CUSTOM_LEAVE 不必带 name：{env}"
    );

    let items = shift_calendar_items(&ctx, id, &d, &d).await;
    assert_eq!(items[0]["effective_day_type"], json!("CUSTOM_LEAVE"));
    // 出参上也**没有** name 键（它是全局日历的语义）
    assert!(
        items[0].get("name").is_none(),
        "覆盖行出参不该有 name：{items:?}"
    );
}

/// 端点 9：空 `items` → `21904`；未知日型 → `21905`；幽灵班次 → `21901`。
#[tokio::test]
async fn override_write_endpoints_error_matrix() {
    let ctx = bootstrap().await;
    let shift = create_shift(&ctx, &crate::unique_code("EMX"), &[("08:00", "17:00")]).await;
    let id = id_of(&shift);
    let d = date(0);

    let (status, env) = post_as_manager(
        &ctx,
        &format!("/hr/shifts/{id}/calendar/days"),
        json!({ "items": [] }),
    )
    .await;
    assert_biz_code(status, &env, 21904, StatusCode::BAD_REQUEST);

    let (status, env) = post_as_manager(
        &ctx,
        &format!("/hr/shifts/{id}/calendar/days"),
        json!({ "items": [{ "cal_date": d, "day_type": "NOPE" }] }),
    )
    .await;
    assert_biz_code(status, &env, 21905, StatusCode::BAD_REQUEST);

    let ghost = ghost_id();
    let (status, env) = post_as_manager(
        &ctx,
        &format!("/hr/shifts/{ghost}/calendar/days"),
        json!({ "items": [{ "cal_date": d, "day_type": "WORKDAY" }] }),
    )
    .await;
    assert_biz_code(status, &env, 21901, StatusCode::NOT_FOUND);
}

/// 端点 9：**与全局同值的覆盖行也是允许的**（锁住「将来有人改全局日历不影响这个
/// 班次」）；`is_override` 表达的是「有没有覆盖行」，**不是**「值是否与全局相同」。
#[tokio::test]
async fn an_override_row_equal_to_global_is_still_an_override() {
    let ctx = bootstrap().await;
    let d = date(0);
    write_global_day(&ctx, &d, "WORKDAY", None).await;
    let shift = create_shift(&ctx, &crate::unique_code("EQ"), &[("08:00", "17:00")]).await;
    let id = id_of(&shift);
    write_override_day(&ctx, id, &d, "WORKDAY").await;

    let items = shift_calendar_items(&ctx, id, &d, &d).await;
    assert_eq!(items[0]["effective_day_type"], json!("WORKDAY"));
    assert_eq!(items[0]["global_day_type"], json!("WORKDAY"));
    assert_eq!(
        items[0]["is_override"],
        json!(true),
        "同值覆盖行仍是覆盖行（判据是存在性）：{items:?}"
    );
}

/// 端点 10：清覆盖的两种形态 + 幽灵班次 21901 + 停用班次 21906。
#[tokio::test]
async fn clearing_override_forms_and_error_matrix() {
    let ctx = bootstrap().await;
    let shift = create_shift(&ctx, &crate::unique_code("CLR2"), &[("08:00", "17:00")]).await;
    let id = id_of(&shift);
    let d0 = date(0);
    let d2 = date(2);
    write_override_day(&ctx, id, &d0, "MAKEUP_WORKDAY").await;
    write_override_day(&ctx, id, &d2, "MAKEUP_WORKDAY").await;

    // `{from,to}` 形态清掉两天
    let (status, env) = post_as_manager(
        &ctx,
        &format!("/hr/shifts/{id}/calendar/days/delete"),
        json!({ "from": d0, "to": d2 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["affected"], json!(2), "区间内两行都被清：{env}");
    assert_eq!(live_override_count(&ctx.pool, id).await, 0);

    // 形态非法 → 21904
    let (status, env) = post_as_manager(
        &ctx,
        &format!("/hr/shifts/{id}/calendar/days/delete"),
        json!({}),
    )
    .await;
    assert_biz_code(status, &env, 21904, StatusCode::BAD_REQUEST);

    // 幽灵班次 → 21901（守卫在 resolve_delete_target 之前）
    let ghost = ghost_id();
    let (status, env) = post_as_manager(
        &ctx,
        &format!("/hr/shifts/{ghost}/calendar/days/delete"),
        json!({ "dates": [d0] }),
    )
    .await;
    assert_biz_code(status, &env, 21901, StatusCode::NOT_FOUND);

    // 停用班次 → 21906
    deactivate_shift(&ctx, id, version_of(&shift)).await;
    let (status, env) = post_as_manager(
        &ctx,
        &format!("/hr/shifts/{id}/calendar/days/delete"),
        json!({ "dates": [d0] }),
    )
    .await;
    assert_biz_code(status, &env, 21906, StatusCode::CONFLICT);
}

// ===========================================================================
//  4. 日型出参字面量的回归
// ===========================================================================

/// ⚠️ **回归**：出参里的日型必须逐字是 DB 字面量（`REST_DAY`），**不是**变体名
/// （`RestDay`）。
///
/// `#[derive(Serialize)]` 的默认行为恰好产出变体名，而 wire 上契约是 DB 字面量 ——
/// 两者不一致时前端按 `REST_DAY` 匹配全部落空、服务端日志却全绿。
/// 这条用例把 6 档全过一遍 HTTP 序列化，逐档钉住字面量。
#[tokio::test]
async fn day_type_literals_on_the_wire_are_screaming_snake_case() {
    const ALL: [(&str, &str); 6] = [
        ("WORKDAY", "工作日"),
        ("MAKEUP_WORKDAY", "调休上班"),
        ("REST_DAY", "正常周末"),
        ("STATUTORY_HOLIDAY", "法定节假日"),
        ("MAKEUP_HOLIDAY", "调休放假"),
        ("CUSTOM_LEAVE", "自定义休息日"),
    ];

    let ctx = bootstrap().await;
    let shift = create_shift(&ctx, &crate::unique_code("LIT"), &[("08:00", "17:00")]).await;
    let id = id_of(&shift);

    // 6 档各占一天（跨度 6 天，远小于 366 天上限）
    let items_in: Vec<Value> = ALL
        .iter()
        .enumerate()
        .map(|(i, (dt, name))| json!({ "cal_date": date(i as i64), "day_type": dt, "name": name }))
        .collect();
    let (status, env) =
        post_as_manager(&ctx, "/hr/calendar/days", json!({ "items": items_in })).await;
    assert_eq!(status, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["affected"], json!(6));

    // 端点 1 回传逐字字面量
    let days = global_calendar_map(&ctx, &date(0), &date(5)).await;
    for (i, (dt, _)) in ALL.iter().enumerate() {
        assert_eq!(
            days[i]["day_type"],
            json!(dt),
            "第 {i} 档日型字面量不对（期望 {dt}，绝不是变体名）：{days:?}"
        );
    }

    // 端点 8 的 global_day_type / effective_day_type 同样逐字
    let items = shift_calendar_items(&ctx, id, &date(0), &date(5)).await;
    for (i, (dt, _)) in ALL.iter().enumerate() {
        assert_eq!(items[i]["global_day_type"], json!(dt));
        assert_eq!(items[i]["effective_day_type"], json!(dt));
    }
}

/// `is_workday` 只认 `WORKDAY` 与 `MAKEUP_WORKDAY` 两档（考勤取数的唯一判据）。
#[tokio::test]
async fn is_workday_only_for_workday_and_makeup_workday() {
    let ctx = bootstrap().await;
    let shift = create_shift(&ctx, &crate::unique_code("WD"), &[("08:00", "17:00")]).await;
    let id = id_of(&shift);

    let expect: [(&str, bool); 6] = [
        ("WORKDAY", true),
        ("MAKEUP_WORKDAY", true),
        ("REST_DAY", false),
        ("STATUTORY_HOLIDAY", false),
        ("MAKEUP_HOLIDAY", false),
        ("CUSTOM_LEAVE", false),
    ];
    let items_in: Vec<Value> = expect
        .iter()
        .enumerate()
        .map(|(i, (dt, _))| json!({ "cal_date": date(i as i64), "day_type": dt, "name": "n" }))
        .collect();
    let (status, _) =
        post_as_manager(&ctx, "/hr/calendar/days", json!({ "items": items_in })).await;
    assert_eq!(status, StatusCode::OK);

    let items = shift_calendar_items(&ctx, id, &date(0), &date(5)).await;
    for (i, (dt, want)) in expect.iter().enumerate() {
        assert_eq!(
            items[i]["is_workday"],
            json!(*want),
            "{dt} 的 is_workday 应为 {want}：{items:?}"
        );
    }
}

/// 端点 8：显式写一条与全局同日的覆盖时，`global_day_type` 仍回传**全局那一档**
/// （前端要显示「这天全局是 X，但本班次是 Y」）。
#[tokio::test]
async fn global_and_effective_are_independent_axes() {
    let ctx = bootstrap().await;
    let d = date(0);
    write_global_day(&ctx, &d, "MAKEUP_HOLIDAY", Some("调休")).await;
    let shift = create_shift(&ctx, &crate::unique_code("AX"), &[("08:00", "17:00")]).await;
    let id = id_of(&shift);
    write_override_day(&ctx, id, &d, "MAKEUP_WORKDAY").await;

    let items = shift_calendar_items(&ctx, id, &d, &d).await;
    assert_eq!(items[0]["global_day_type"], json!("MAKEUP_HOLIDAY"));
    assert_eq!(items[0]["effective_day_type"], json!("MAKEUP_WORKDAY"));
    assert_eq!(
        items[0]["is_workday"],
        json!(true),
        "只按 effective 判上班日"
    );
    assert_eq!(items[0]["is_override"], json!(true));
}
