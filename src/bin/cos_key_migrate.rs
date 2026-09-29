//! COS object_key 五段→两段迁移 bin（2026-09-29 CAS key 扁平化）
//!
//! ## 用途
//! 把 `t_part_file.object_key` 历史值
//! `{prefix}{owner_kind}/{owner_id}/{KIND}/{sha16}_{safe_filename}`
//! 重写为扁平化模板 `{prefix}{sha16}_{safe_filename}`。
//!
//! 同一文件内容 sha16 一致 → 新旧 key 指向同一 COS 对象；改宽为 DB schema
//! 的 object_key 列重写 + 服务端读端点 `resolve_effective_key` fallback
//! 双层保险。
//!
//! ## 运行模式
//! - `--dry-run`（默认）：SELECT 全表，对每行打印迁移前后 object_key，
//!   **不 UPDATE**。用于人工核对。
//! - `--apply`：`UPDATE t_part_file SET object_key = $new WHERE id = $id AND
//!   object_key = $old`（review A7 乐观锁），每个 UPDATE 一个独立 transaction，
//!   失败不影响其它行。
//! - 速率：sleep 仅为批间散热，实际速率受 COS RTT 限制（review A6 修）。
//!
//! ## 退出码
//! - 0：成功（dry-run 完毕 / apply 全部 UPDATE 成功）
//! - 1：参数错误 / DB 错误 / 任意 UPDATE 失败
//!
//! ## 使用示例
//! ```bash
//! # 1. 先 dry-run 看一眼
//! DATABASE_URL=postgres://hsh:6065161@localhost:5430/hsh \
//!     cargo run --bin cos-key-migrate -- --dry-run
//!
//! # 2. 确认无误后再 apply
//! DATABASE_URL=postgres://hsh:6065161@localhost:5430/hsh \
//!     cargo run --bin cos-key-migrate -- --apply
//! ```
//!
//! 2026-09-29 新增（review 第 1 轮 A5-A8 + A10 修）

use std::time::Duration;

use anyhow::{Context as _, Result};
use regex::Regex;
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use hsh_erp_rust::util::cos_key;

/// sha16 必须 16 hex chars（小写）。policy::ext_of 同款白名单（kind→ext 不在
/// 本 bin 关心范围，本 bin 仅做 key 重写；kind 白名单仅用于跳过非法历史 key）。
const KIND_WHITELIST: &[&str] = &["DRAWING", "3D_MODEL", "CAD_2D", "G_CODE", "SETUP_SHEET"];

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    // tracing 初始化（仅 RUST_LOG 控制）
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();
    let dry_run = args.iter().any(|a| a == "--dry-run") || !args.iter().any(|a| a == "--apply");
    let apply = args.iter().any(|a| a == "--apply");

    info!(
        mode = if dry_run { "dry-run" } else { "apply" },
        "cos_key_migrate 启动"
    );

    let database_url = std::env::var("DATABASE_URL").context("DATABASE_URL 未设置")?;
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect(&database_url)
        .await
        .context("连接 Postgres 失败（检查 DATABASE_URL）")?;

    let rows = fetch_all_part_file_keys(&pool).await?;
    info!(total = rows.len(), "扫描 t_part_file 完成");

    let sha16_re = Regex::new(r"^[0-9a-f]{16}$").expect("hard-coded regex 必合法");
    let mut stats = Stats::default();
    let mut updates: Vec<(i64, String, String)> = Vec::new(); // (id, old_key, new_key)

    for (id, old_key) in &rows {
        stats.scanned += 1;
        match rewrite(old_key, &sha16_re) {
            Ok(new_key) => {
                stats.rewritable += 1;
                if new_key != *old_key {
                    updates.push((*id, old_key.clone(), new_key));
                } else {
                    stats.already_flat += 1;
                }
            }
            Err(reason) => {
                stats.skipped += 1;
                if stats.skipped <= 5 {
                    warn!(id, old_key, reason, "跳过该行（不属历史五段模板）");
                }
            }
        }
    }

    info!(
        scanned = stats.scanned,
        rewritable = stats.rewritable,
        already_flat = stats.already_flat,
        skipped = stats.skipped,
        pending_updates = updates.len(),
        "扫描统计"
    );

    if !apply {
        // dry-run：前 10 条示例
        info!("dry-run 模式：前 10 条候选 UPDATE 预览：");
        for (i, (id, old, new)) in updates.iter().take(10).enumerate() {
            info!(index = i, id, old = %old, new = %new, "  候选");
        }
        info!("dry-run 完毕。确认无误后加 --apply 实际执行。");
        return Ok(());
    }

    // apply：每行一个 transaction（review A7 乐观锁）
    let mut success = 0u64;
    let mut failed = 0u64;
    for (id, old, new) in &updates {
        match update_one(&pool, *id, old, new).await {
            Ok(()) => success += 1,
            Err(e) => {
                failed += 1;
                warn!(id, old = %old, new = %new, error = %e, "UPDATE 失败（继续处理下一行）");
            }
        }
        // sleep 仅为批间散热，实际速率受 COS RTT 限制（review A6 修）。
        tokio::time::sleep(Duration::from_micros(200)).await;
    }

    info!(success, failed, "apply 完毕");
    if failed > 0 {
        anyhow::bail!("{} 行 UPDATE 失败", failed);
    }
    Ok(())
}

#[derive(Default, Debug)]
struct Stats {
    scanned: u64,
    rewritable: u64,
    already_flat: u64,
    skipped: u64,
}

/// 拉 t_part_file 全表 (id, object_key) 行（含已软删，留待运维手工决策）；
/// 软删行同样重写（object_key 是历史事实，与 deleted_at 正交）。
async fn fetch_all_part_file_keys(pool: &PgPool) -> Result<Vec<(i64, String)>> {
    let rows = sqlx::query("SELECT id, object_key FROM t_part_file")
        .fetch_all(pool)
        .await
        .context("SELECT t_part_file 失败")?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let id: i64 = r.try_get("id").expect("id 列");
            let key: String = r.try_get("object_key").expect("object_key 列");
            (id, key)
        })
        .collect())
}

/// 把历史五段 key 重写为新两段 key（review A5：先验 sha16 hex，再验 kind 白名单）。
///
/// 边界：
/// - 已是新模板（段数 < 4 拆分后）→ Ok(old) 不动（视为 already_flat）
/// - kind 不在白名单 → Err("kind {kind:?} 不在白名单")
/// - sha16 不是 16 hex → Err("sha16 不是 16 hex")
/// - 段数 ≠ 4（去掉 prefix 后）→ Err("段数错")
fn rewrite(old_key: &str, sha16_re: &Regex) -> Result<String, String> {
    let Some(lk) = cos_key::parse_legacy_key(old_key) else {
        // parse_legacy_key 已经做了 kind/sha16 白名单校验；None 已是误识别场景
        return Err("不是历史五段模板 / kind 不在白名单 / sha16 不合法".to_string());
    };
    if !KIND_WHITELIST.contains(&lk.kind.as_str()) {
        return Err(format!("kind {:?} 不在白名单", lk.kind));
    }
    if !sha16_re.is_match(&lk.sha16) {
        return Err(format!("sha16 {:?} 不是 16 hex", lk.sha16));
    }
    // 2026-09-29 扁平化：prefix 取 lk 解析前的"剩余段"中除最后 owner_kind/owner_id/KIND 三段外的所有段。
    // 例：`uploads/part/123/DRAWING/abc...pdf` → prefix = `uploads/`；`uploads/foo/bar/part/123/DRAWING/abc...pdf` → prefix = `uploads/foo/bar/`。
    let before_tail = old_key.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
    let n_slash_total = before_tail.matches('/').count();
    // before_tail 形如 `prefix/owner_kind/owner_id/KIND`，最后 3 段是 owner 信息。
    // 切出 prefix 部分（去掉最后 3 段：两次 rsplit_once 取第 n-3 段之后所有）。
    let prefix_part = if n_slash_total >= 3 {
        // 至少需要 owner_kind/owner_id/KIND 三段；前面部分均为 prefix。
        let mut s = before_tail;
        for _ in 0..3 {
            s = s.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
        }
        s
    } else {
        ""
    };
    let prefix = if prefix_part.is_empty() {
        String::new()
    } else {
        format!("{prefix_part}/")
    };
    // 2026-09-29 扁平化：新模板 `{prefix}{sha16}_{safe_filename}`，直接手拼
    let new_key = format!("{prefix}{}_{}", lk.sha16, lk.safe_filename);
    Ok(new_key)
}

/// UPDATE 单行；带乐观锁 `WHERE id=$1 AND object_key=$2`（review A7）。
async fn update_one(pool: &PgPool, id: i64, old_key: &str, new_key: &str) -> Result<()> {
    let mut tx = pool.begin().await.context("begin tx")?;
    let res =
        sqlx::query("UPDATE t_part_file SET object_key = $1 WHERE id = $2 AND object_key = $3")
            .bind(new_key)
            .bind(id)
            .bind(old_key)
            .execute(&mut *tx)
            .await
            .context("UPDATE 失败")?;
    if res.rows_affected() == 0 {
        // 乐观锁失败：被其它进程改走了，跳过
        warn!(id, old = %old_key, "UPDATE 0 行（object_key 已变更，跳过）");
        tx.rollback().await.ok();
        return Ok(());
    }
    tx.commit().await.context("commit tx")?;
    Ok(())
}
