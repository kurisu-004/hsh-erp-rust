//! 端点 1/2：`GET /config` + `POST /config`（422 / 40901 / 21804 校验矩阵 / 21806）

use axum::http::StatusCode;
use serde_json::{Value, json};

use super::*;

/// 表空 ⇒ 全默认 + `version=0` + `id="0"`（**不是 404**）。
#[tokio::test]
async fn empty_table_returns_full_defaults_with_zero_version_and_id() {
    let ctx = bootstrap().await;
    let (status, env) = get_as_manager(&ctx, "/cron/wecom-daily/config").await;
    assert_eq!(status, StatusCode::OK, "{env}");

    let d = data(&env);
    assert_eq!(d["version"].as_i64(), Some(0));
    assert_eq!(d["id"].as_str(), Some("0"), "表空时 id 恒为字符串 \"0\"");
    assert_eq!(d["enabled"].as_bool(), Some(false));
    assert_eq!(d["frequency"].as_str(), Some("DAILY"));
    assert_eq!(d["send_hour"].as_i64(), Some(8));
    assert_eq!(d["send_minute"].as_i64(), Some(0));
    assert!(d["weekday"].is_null());
    assert!(d["webhook_id"].is_null());
    assert_eq!(d["upcoming_days"].as_i64(), Some(3));
    assert_eq!(d["worker_metric"].as_str(), Some("QUANTITY"));
    assert_eq!(d["worker_top_n"].as_i64(), Some(15));
    assert_eq!(d["max_chunks"].as_i64(), Some(3));
    assert_eq!(d["webhook_configured"].as_bool(), Some(false));
    assert!(d["subject_template"].is_null());
    assert_eq!(d["created_at"].as_str(), Some(""), "未创建时时间戳是空串");

    // **21 个字段**（前端 `.strict()` + 键数断言钉死；少一个键前端当场抛）。
    let n = d.as_object().expect("data 是对象").len();
    assert_eq!(n, 21, "ConfigOut 必须逐字 21 个字段：{d}");
}

/// `webhook_id` 指向**已软删**的 webhook ⇒ `GET /config` 的 `webhook_configured=false`
/// （而**不**是 404 / 500）。
#[tokio::test]
async fn soft_deleted_webhook_flips_webhook_configured() {
    let ctx = bootstrap().await;
    let (status, env) = post_as_manager(
        &ctx,
        "/cron/wecom-daily/config",
        config_body(0, ctx.webhook_id),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{env}");
    assert_eq!(data(&env)["webhook_configured"].as_bool(), Some(true));

    sqlx::query("UPDATE t_wecom_webhook SET deleted_at = now() WHERE id = $1")
        .bind(ctx.webhook_id)
        .execute(&ctx.pool)
        .await
        .expect("软删 webhook");

    let (status, env) = get_as_manager(&ctx, "/cron/wecom-daily/config").await;
    assert_eq!(status, StatusCode::OK, "{env}");
    assert_eq!(data(&env)["webhook_configured"].as_bool(), Some(false));
}

/// 保存后 version 自增、读回的是**存完之后**那一行。
#[tokio::test]
async fn save_bumps_version_and_returns_fresh_row() {
    let ctx = bootstrap().await;
    let (status, env) = post_as_manager(
        &ctx,
        "/cron/wecom-daily/config",
        config_body(0, ctx.webhook_id),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{env}");
    let d = data(&env);
    assert_eq!(
        d["version"].as_i64(),
        Some(0),
        "首次创建后 version 仍是 0（那行本身是第 0 版）"
    );
    assert_ne!(d["id"].as_str(), Some("0"), "建行之后 id 不再是 \"0\"");
    assert_eq!(
        id_str(&d["webhook_id"]),
        ctx.webhook_id,
        "webhook_id 是 JSON string（雪花防精度截断）"
    );

    let mut body = config_body(d["version"].as_i64().unwrap() as i32, ctx.webhook_id);
    body["upcoming_days"] = json!(7);
    let (status, env2) = post_as_manager(&ctx, "/cron/wecom-daily/config", body).await;
    assert_eq!(status, StatusCode::OK, "{env2}");
    assert_eq!(data(&env2)["upcoming_days"].as_i64(), Some(7));
    assert_eq!(
        data(&env2)["version"].as_i64(),
        Some(1),
        "覆写后 version 自增"
    );
}

/// `version` 缺失 ⇒ **HTTP 422 纯文本**（不是业务信封）。
#[tokio::test]
async fn missing_version_is_422_plain_text() {
    let ctx = bootstrap().await;
    let mut body = config_body(0, ctx.webhook_id);
    body.as_object_mut().unwrap().remove("version");

    let (status, text) = post_raw(&ctx, "POST", "/cron/wecom-daily/config", body).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{text}");
    assert!(
        !text.contains("\"code\""),
        "422 是纯文本、不是 R<T> 信封：{text}"
    );
}

/// 陈旧 `version` ⇒ `40901`（且**不**改动那一行）。
#[tokio::test]
async fn stale_version_is_40901_and_does_not_touch_the_row() {
    let ctx = bootstrap().await;
    // 建行（首次初始化走 INSERT，那一行本身是第 0 版）
    let (status, env) = post_as_manager(
        &ctx,
        "/cron/wecom-daily/config",
        config_body(0, ctx.webhook_id),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{env}");
    assert_eq!(data(&env)["version"].as_i64(), Some(0));

    // 用一个明显不存在的 version 再提交
    let mut body = config_body(77, ctx.webhook_id);
    body["upcoming_days"] = json!(99);
    let (status, env2) = post_as_manager(&ctx, "/cron/wecom-daily/config", body).await;
    assert_biz_code(status, &env2, 40901, StatusCode::CONFLICT);

    // 那一行没被动过
    let (status, env3) = get_as_manager(&ctx, "/cron/wecom-daily/config").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(data(&env3)["version"].as_i64(), Some(0));
    assert_eq!(data(&env3)["upcoming_days"].as_i64(), Some(3));
}

/// 行已存在（version 已自增到 1）时再带 `version=0` 提交 ⇒ `40901`（0 行命中）。
///
/// ⚠️ 注意「首次创建那一行的 `version` 就是 0」，所以「建行后立刻再带 0 存一次」
/// 是**成功**的（它就是正常的下一次覆写）；要让 40901 发生，必须先让它涨过 0。
#[tokio::test]
async fn second_save_with_stale_zero_version_is_40901() {
    let ctx = bootstrap().await;
    let (status, first) = post_as_manager(
        &ctx,
        "/cron/wecom-daily/config",
        config_body(0, ctx.webhook_id),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let v0 = data(&first)["version"].as_i64().unwrap();

    // 带**读回来的那个** version 再存 ⇒ 成功并自增
    let (status, second) = post_as_manager(
        &ctx,
        "/cron/wecom-daily/config",
        config_body(v0 as i32, ctx.webhook_id),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{second}");
    assert_eq!(data(&second)["version"].as_i64(), Some(v0 + 1));

    // 再带那个已经过期的 version ⇒ 40901
    let (status, env) = post_as_manager(
        &ctx,
        "/cron/wecom-daily/config",
        config_body(v0 as i32, ctx.webhook_id),
    )
    .await;
    assert_biz_code(status, &env, 40901, StatusCode::CONFLICT);
}

/// `webhook_id` 指向已软删 / 不存在的行 ⇒ `21806`。
#[tokio::test]
async fn deleted_or_missing_webhook_is_21806() {
    let ctx = bootstrap().await;
    sqlx::query("UPDATE t_wecom_webhook SET deleted_at = now() WHERE id = $1")
        .bind(ctx.webhook_id)
        .execute(&ctx.pool)
        .await
        .expect("软删 webhook");

    let (status, env) = post_as_manager(
        &ctx,
        "/cron/wecom-daily/config",
        config_body(0, ctx.webhook_id),
    )
    .await;
    assert_biz_code(status, &env, 21806, StatusCode::NOT_FOUND);

    // 库里完全没有的 id 同样
    let ghost = shared_test_snowflake().next_id();
    let (status, env) =
        post_as_manager(&ctx, "/cron/wecom-daily/config", config_body(0, ghost)).await;
    assert_biz_code(status, &env, 21806, StatusCode::NOT_FOUND);

    // 且**一个配置行都没被建出来**（校验失败不留痕）
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM t_wecom_daily_config")
        .fetch_one(&ctx.pool)
        .await
        .expect("count config");
    assert_eq!(n, 0, "校验失败不该建行");
}

/// `21804` 校验矩阵：频率与时刻不匹配。
#[tokio::test]
async fn config_invalid_matrix_frequency_and_time() {
    let ctx = bootstrap().await;
    let cases: Vec<(&str, Value)> = vec![
        ("未知频率", json!({"frequency": "HOURLY"})),
        (
            "DAILY 缺小时",
            json!({"send_hour": Value::Null, "send_minute": Value::Null}),
        ),
        ("小时越界", json!({"send_hour": 24})),
        ("分钟越界", json!({"send_minute": 60})),
    ];
    for (label, patch) in cases {
        let mut body = config_body(0, ctx.webhook_id);
        for (k, v) in patch.as_object().unwrap() {
            body[k] = v.clone();
        }
        let (status, env) = post_as_manager(&ctx, "/cron/wecom-daily/config", body).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{label} 应 400；信封={env}"
        );
        assert_eq!(
            env.get("code").and_then(|c| c.as_i64()),
            Some(21804),
            "{label} 应 21804：{env}"
        );
    }
}

/// `21804` 校验矩阵：频率配套字段（WEEKLY 缺 weekday / MONTHLY 缺 day_of_month）。
#[tokio::test]
async fn config_invalid_matrix_frequency_specific_fields() {
    let ctx = bootstrap().await;
    let mut weekly = config_body(0, ctx.webhook_id);
    weekly["frequency"] = json!("WEEKLY");
    weekly["weekday"] = Value::Null;
    let (status, env) = post_as_manager(&ctx, "/cron/wecom-daily/config", weekly).await;
    assert_biz_code(status, &env, 21804, StatusCode::BAD_REQUEST);

    let mut monthly = config_body(0, ctx.webhook_id);
    monthly["frequency"] = json!("MONTHLY");
    monthly["day_of_month"] = Value::Null;
    let (status, env) = post_as_manager(&ctx, "/cron/wecom-daily/config", monthly).await;
    assert_biz_code(status, &env, 21804, StatusCode::BAD_REQUEST);

    // WEEKLY + weekday=1 合法（且 cron 会减 1，那是 schedule 的职责）
    let mut weekly_ok = config_body(0, ctx.webhook_id);
    weekly_ok["frequency"] = json!("WEEKLY");
    weekly_ok["weekday"] = json!(1);
    let (status, env) = post_as_manager(&ctx, "/cron/wecom-daily/config", weekly_ok).await;
    assert_eq!(status, StatusCode::OK, "WEEKLY + weekday=1 应合法：{env}");
    assert_eq!(data(&env)["weekday"].as_i64(), Some(1));
}

/// `21804` 校验矩阵：四节全关。
#[tokio::test]
async fn config_invalid_four_sections_all_off() {
    let ctx = bootstrap().await;
    let mut body = config_body(0, ctx.webhook_id);
    for k in [
        "include_overview",
        "include_delivery",
        "include_orders",
        "include_workers",
    ] {
        body[k] = json!(false);
    }
    let (status, env) = post_as_manager(&ctx, "/cron/wecom-daily/config", body).await;
    assert_biz_code(status, &env, 21804, StatusCode::BAD_REQUEST);
    assert!(env["message"].as_str().unwrap().contains("至少要开启一节"));

    // 关掉一节（三个开）**合法**
    let mut three = config_body(0, ctx.webhook_id);
    three["include_delivery"] = json!(false);
    let (status, env) = post_as_manager(&ctx, "/cron/wecom-daily/config", three).await;
    assert_eq!(status, StatusCode::OK, "关一节不该报错：{env}");
}

/// `21804` 校验矩阵：`worker_top_n` / `max_chunks` 越界与未知 `worker_metric`。
#[tokio::test]
async fn config_invalid_matrix_numbers_and_metric() {
    let ctx = bootstrap().await;
    let cases: Vec<(&str, Value)> = vec![
        ("worker_top_n 下界", json!({"worker_top_n": -1})),
        ("worker_top_n 上界", json!({"worker_top_n": 201})),
        ("max_chunks 0", json!({"max_chunks": 0})),
        ("max_chunks 负", json!({"max_chunks": -1})),
        // ⚠️ 21 > 20：企业微信官方限频（每个消息推送 ≤ 20 条/分钟），写侧就拒
        ("max_chunks 超官方限频", json!({"max_chunks": 21})),
        ("未知 worker_metric", json!({"worker_metric": "HOURS"})),
    ];
    for (label, patch) in cases {
        let mut body = config_body(0, ctx.webhook_id);
        for (k, v) in patch.as_object().unwrap() {
            body[k] = v.clone();
        }
        let (status, env) = post_as_manager(&ctx, "/cron/wecom-daily/config", body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{label} 应 400：{env}");
        assert_eq!(
            env.get("code").and_then(|c| c.as_i64()),
            Some(21804),
            "{label} 应 21804：{env}"
        );
    }

    // 边界值都合法
    for (top, chunks) in [(0, 1), (200, 20)] {
        let mut body = config_body(0, ctx.webhook_id);
        body["worker_top_n"] = json!(top);
        body["max_chunks"] = json!(chunks);
        let (status, env) = post_as_manager(&ctx, "/cron/wecom-daily/config", body).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "边界 worker_top_n={top} max_chunks={chunks}：{env}"
        );
    }
}

/// `upcoming_days` 在保存路径是 **clamp**（不是拒）。
#[tokio::test]
async fn upcoming_days_is_clamped_on_save() {
    let ctx = bootstrap().await;
    let mut body = config_body(0, ctx.webhook_id);
    body["upcoming_days"] = json!(9999);
    let (status, env) = post_as_manager(&ctx, "/cron/wecom-daily/config", body).await;
    assert_eq!(status, StatusCode::OK, "{env}");
    assert_eq!(data(&env)["upcoming_days"].as_i64(), Some(60));
}

/// `subject_template` 超长 / 含 CR-LF ⇒ `21804`。
#[tokio::test]
async fn subject_template_overlong_or_crlf_is_21804() {
    let ctx = bootstrap().await;
    for (label, tpl) in [
        ("超 200 字", "标".repeat(201)),
        ("含 CR/LF", "标题\r\n注入".to_string()),
    ] {
        let mut body = config_body(0, ctx.webhook_id);
        body["subject_template"] = json!(tpl);
        let (status, env) = post_as_manager(&ctx, "/cron/wecom-daily/config", body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{label}：{env}");
        assert_eq!(
            env.get("code").and_then(|c| c.as_i64()),
            Some(21804),
            "{label}"
        );
    }
}

/// 事务边界：`POST /config` 不**级联**写流水表（配置保存不产生任何推送记录）。
#[tokio::test]
async fn saving_config_never_writes_a_log_row() {
    let ctx = bootstrap().await;
    let (status, _) = post_as_manager(
        &ctx,
        "/cron/wecom-daily/config",
        config_body(0, ctx.webhook_id),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(count_logs(&ctx.pool).await, 0, "保存配置不落流水");
}

/// 保存成功后 cron **热重载存活**（照 `mail_digest` 同名用例的形状）。
///
/// 断言的是「后端没崩 + `/config` 仍可读」：真调度器在测试进程里不启动（`main.rs`
/// 才 spawn 它），热重载是**懒创建**一个进程内单例，不会注册到真 cron 上。
#[tokio::test]
async fn save_config_then_reschedule_survives() {
    let ctx = bootstrap().await;
    let (status, env) = post_as_manager(
        &ctx,
        "/cron/wecom-daily/config",
        config_body(0, ctx.webhook_id),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{env}");
    // 热重载之后配置仍读得到，且再存一次（带新 version）正常
    let (status, env2) = get_as_manager(&ctx, "/cron/wecom-daily/config").await;
    assert_eq!(status, StatusCode::OK);
    let v = data(&env2)["version"].as_i64().unwrap() as i32;
    let (status, env3) = post_as_manager(
        &ctx,
        "/cron/wecom-daily/config",
        config_body(v, ctx.webhook_id),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "热重载后仍可保存：{env3}");
}

/// CLERK（越权）在本域 5 个端点上**全部 403**。
#[tokio::test]
async fn clerk_is_forbidden_on_every_endpoint() {
    let ctx = bootstrap().await;
    for path in ["/config", "/meta", "/logs"] {
        let (status, env) = get_as_clerk(&ctx, &format!("/cron/wecom-daily{path}")).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "GET {path} 应 403：{env}");
    }
    // ⚠️ `/config` 必须送**合法** body：axum 的 `Json<SaveConfigRequest>` 提取器在
    // handler 之前执行，body 缺 `version` 会先被 422 拒掉、根本走不到角色守卫（这与
    // 「守卫写在第一行」不矛盾：守卫守的是业务逻辑，提取器守的是报文形状）。
    let (status, env) = post_as_clerk(
        &ctx,
        "/cron/wecom-daily/config",
        config_body(0, ctx.webhook_id),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "POST /config 应 403：{env}");
    for path in ["/preview", "/send"] {
        let (status, env) =
            post_as_clerk(&ctx, &format!("/cron/wecom-daily{path}"), json!({})).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "POST {path} 应 403：{env}");
    }
}

/// 动词纪律：PUT / PATCH / DELETE **不路由**（405）。
#[tokio::test]
async fn put_patch_delete_verbs_are_not_routed() {
    let ctx = bootstrap().await;
    for method in ["PUT", "PATCH", "DELETE"] {
        let (status, _) = post_raw(&ctx, method, "/cron/wecom-daily/config", json!({})).await;
        assert_eq!(
            status,
            StatusCode::METHOD_NOT_ALLOWED,
            "{method} 不该被路由"
        );
    }
}

/// `GET /meta`：6 个键 + 三个 range + 四个 `needs_*` 全部由后端下发。
#[tokio::test]
async fn meta_ships_six_keys_with_ranges_and_flags() {
    let ctx = bootstrap().await;
    let (status, env) = get_as_manager(&ctx, "/cron/wecom-daily/meta").await;
    assert_eq!(status, StatusCode::OK, "{env}");
    let d = data(&env);

    let obj = d.as_object().expect("data 是对象");
    assert_eq!(obj.len(), 6, "MetaOut 必须逐字 6 个键：{d}");

    assert_eq!(d["upcoming_days_range"]["min"].as_i64(), Some(1));
    assert_eq!(d["upcoming_days_range"]["max"].as_i64(), Some(60));
    assert_eq!(d["worker_top_n_range"]["min"].as_i64(), Some(0));
    assert_eq!(d["worker_top_n_range"]["max"].as_i64(), Some(200));
    assert_eq!(d["max_chunks_range"]["min"].as_i64(), Some(1));
    assert_eq!(d["max_chunks_range"]["max"].as_i64(), Some(20));

    let freqs = d["frequencies"].as_array().expect("frequencies 是数组");
    assert_eq!(freqs.len(), 4);
    let by_value = |v: &str| {
        freqs
            .iter()
            .find(|f| f["value"].as_str() == Some(v))
            .unwrap_or_else(|| panic!("缺频率 {v}"))
    };
    let weekly = by_value("WEEKLY");
    assert_eq!(weekly["needs_time"].as_bool(), Some(true));
    assert_eq!(weekly["needs_weekday"].as_bool(), Some(true));
    assert_eq!(weekly["needs_day_of_month"].as_bool(), Some(false));
    let daily = by_value("DAILY");
    assert_eq!(daily["needs_weekday"].as_bool(), Some(false));
    let monthly = by_value("MONTHLY");
    assert_eq!(monthly["needs_day_of_month"].as_bool(), Some(true));
    let manual = by_value("MANUAL_ONLY");
    assert_eq!(manual["needs_time"].as_bool(), Some(false));

    let metrics = d["worker_metrics"]
        .as_array()
        .expect("worker_metrics 是数组");
    assert_eq!(metrics.len(), 2);
    assert_eq!(metrics[0]["value"].as_str(), Some("QUANTITY"));
    assert_eq!(metrics[1]["value"].as_str(), Some("BATCH"));
    for m in metrics {
        assert!(!m["label"].as_str().unwrap().is_empty());
    }

    // defaults 段与 `GET /config` 的表空形态逐字同构（webhook_id 恒 null）
    assert_eq!(d["defaults"]["frequency"].as_str(), Some("DAILY"));
    assert!(d["defaults"]["webhook_id"].is_null());
    assert_eq!(d["defaults"]["worker_metric"].as_str(), Some("QUANTITY"));
    assert_eq!(d["defaults"]["max_chunks"].as_i64(), Some(3));
}
