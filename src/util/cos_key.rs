//! COS 对象 key 派生工具（2026-09-29 CAS key 模板扁平化）
//!
//! ## 关键变更（2026-09-29 扁平化）
//! - `build_cas_key` 模板从五段 `{prefix}{owner_kind}/{owner_id}/{KIND}/{sha16}_{safe_filename}`
//!   简化为两段 `{prefix}{sha16}_{safe_filename}`：owner/kind 信息已在 `t_part_file`
//!   DB 行（外键索引），key 本身只需 prefix + 内容指纹 + 安全 filename。
//! - `legacy_key` 反向解析 + `LegacyKey` 结构体：读端点（`get_part_file_url` /
//!   `get_file_content`）在 DB 持有的 object_key（仍是历史五段）找不对象时，
//!   先尝试按 sha16 推新 key，找不到再试 DB 原值；新数据写入则一律走新模板。
//! - 上传路径（`upload_file_for_owner` / `upload_part_file`）一律走新模板，
//!   `upload_prefix` 配置字段降级为 dead_code（保留兼容 .env 旧值不报错）。
//!
//! 对齐 Python `backend-python/core/file_hash.py:32-78`：
//! - `safe_filename(name)`：把任意 filename 折叠为 `[A-Za-z0-9._-]` 范围；
//!   长度上限 80 字符，扩展名优先保留。
//!
//! 2026-09-11 新增；2026-09-29 扁平化

/// 把文件名折叠为 `[A-Za-z0-9._-]` 安全字符串（最长 80 字符，保留扩展名）。
///
/// 规则（对齐 Python `safe_filename`）：
/// 1. 取最后一段（去目录前缀）
/// 2. 非 `[A-Za-z0-9._-]` 字符替换为 `_`
/// 3. 识别扩展名（最后一段 `.xxx`，ext 长度 1..=7）：
///    - 仅 strip stem 段两端的 `._-`，保留 `.xxx`
///    - 无扩展名或扩展名过长：整段 strip
/// 4. 超过 80 时优先保留扩展名截断
///
/// 示例：
/// - `"图纸.pdf"`            → `"file.pdf"`（stem 全 `._-` 时回退 `"file"`，与 Python 一致）
/// - `"主轴 (v2).STEP"`      → `"v2.STEP"`
/// - `"/path/to/foo bar.PDF"` → `"foo_bar.PDF"`
/// - `""`                    → `"file"`
/// - `"...foo"`              → `"file.foo"`（有 ext 时 stem 回退 "file"，保留 ext）
/// - `"a".repeat(200) + ".pdf"` → 截断到 80 字符保留 `.pdf`
pub fn safe_filename(name: &str) -> String {
    const MAX_LEN: usize = 80;

    // 1. 去目录前缀
    let base = name.rsplit_once('/').map(|(_, tail)| tail).unwrap_or(name);
    // 2. 折叠非 ASCII 安全字符
    let folded: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();

    // 3. 分离 stem + ext（ext 长度 1..=7 视为合法扩展名）
    let (stem, ext) = match folded.rsplit_once('.') {
        Some((s, e)) if (1..=7).contains(&e.len()) => (s, Some(e)),
        _ => (folded.as_str(), None),
    };

    let mut result = match ext {
        Some(e) => {
            let cleaned_stem = stem.trim_matches(|c| c == '.' || c == '_' || c == '-');
            let stem = if cleaned_stem.is_empty() {
                "file"
            } else {
                cleaned_stem
            };
            format!("{stem}.{e}")
        }
        None => {
            let cleaned = folded.trim_matches(|c| c == '.' || c == '_' || c == '-');
            if cleaned.is_empty() {
                "file".to_string()
            } else {
                cleaned.to_string()
            }
        }
    };

    // 4. 超长截断（优先保留扩展名）
    if result.len() > MAX_LEN {
        result = match result.rsplit_once('.') {
            Some((s, e)) if (1..=7).contains(&e.len()) => {
                let keep = MAX_LEN.saturating_sub(e.len() + 1); // +1 for '.'
                let s = &s[..keep.min(s.len())];
                format!("{s}.{e}")
            }
            _ => result[..MAX_LEN].to_string(),
        };
    }

    result
}

/// 按 2026-09-29 扁平化模板拼 CAS key（仅新数据写入路径使用）。
///
/// 模板（两段）：`{prefix}{sha16}_{safe_filename}`
///
/// 注：owner / kind 信息已在 `t_part_file` DB 行（外键索引），key 本身只需
/// prefix + 内容指纹 + 安全 filename，不再带 owner_kind/owner_id/KIND 三段。
///
/// 参数：
/// - `prefix`：COS 桶内上传前缀（通常带尾斜杠，如 `"uploads/"`）；由
///   `cos.upload_prefix` 传入。**函数内部自动保证尾斜杠**：传入 `"uploads"`
///   与 `"uploads/"` 等价；空串视作 `""`（桶根）。
/// - `sha256_hex`：完整 64 字符 SHA-256 hex（取前 16 字符作为 `sha16`）。
/// - `filename`：原始文件名（中文 / 特殊字符 / 路径都可，内部走 `safe_filename`）。
///
/// 注：扩展名已包含在 `safe_filename` 结果里（`foo.pdf` / `__.pdf`），模板
/// 不再追加 `.ext`。
pub fn build_cas_key(prefix: &str, sha256_hex: &str, filename: &str) -> String {
    let sha16 = &sha256_hex[..16.min(sha256_hex.len())];
    let safe = safe_filename(filename);
    // 2026-09-11 修改：自动保证 prefix 尾斜杠；兼容 `.env` 里写成
    // `uploads` 或 `uploads/` 的两种风格，以及测试 config 里的裸值。
    // 2026-09-29 扁平化：前缀只保留 prefix + sha16_<filename> 两段。
    let prefix = if prefix.is_empty() || prefix.ends_with('/') {
        prefix.to_string()
    } else {
        format!("{prefix}/")
    };
    format!("{prefix}{sha16}_{safe}")
}

/// 历史五段 CAS key 解析结果（仅用于读端点 fallback）。
///
/// 2026-09-29 扁平化：DB 历史行 `object_key` 形如
/// `{prefix}{owner_kind}/{owner_id}/{KIND}/{sha16}_{safe_filename}`；
/// 读端点拿到 object_key 后，若按此 key 在 COS 找不到对象（NOSUCHKEY），可
/// 调 `parse_legacy_key` 解析五段，再重新按 sha16 + filename 拼**新模板**的
/// key 重试一次（同一文件内容相同 → sha16 一致 → 新模板 key 一致）。
///
/// 新数据写入走 `build_cas_key`（两段），DB 行 `object_key` 列也是两段形式。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyKey {
    pub owner_kind: String,
    pub owner_id: i64,
    pub kind: String,
    pub sha16: String,
    pub safe_filename: String,
}

/// 反向解析历史五段 key；无法解析（非历史模板 / 段数错）→ `None`。
///
/// 模板：`[<prefix>/]{owner_kind}/{owner_id}/{KIND}/{sha16}_{safe_filename}`
/// - prefix 是任意 0+ 段前缀（最常见 `uploads` 或 `uploads/`）
/// - 末尾必须是 `{sha16}_{safe_filename}`（sha16 16 hex + 下划线 + 剩余文件名）
///
/// 边界：
/// - 段数 < 4（含 prefix 后总段数；最少 4 = owner_kind/owner_id/KIND/sha16_file）→ None
/// - `sha16_{filename}` 段无 `_` → None（无法定位 sha16）
/// - `owner_id` 解析失败 → None
/// - `kind` 不在白名单 → None（仅识别 DRAWING / 3D_MODEL 等，避免误识别业务乱写 key）
pub fn parse_legacy_key(key: &str) -> Option<LegacyKey> {
    // 拆分出末尾 `{sha16}_{safe_filename}` 与前面的剩余段
    let (before_tail, tail) = key.rsplit_once('/')?;
    let (sha16, safe_filename) = tail.split_once('_')?;
    if sha16.len() != 16 || !sha16.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    // before_tail 形如 `[<prefix>/]{owner_kind}/{owner_id}/{KIND}` —— 至少 3 段
    let parts: Vec<&str> = before_tail.split('/').collect();
    if parts.len() < 3 {
        return None;
    }
    // 取最后 3 段作为 owner_kind/owner_id/KIND；前面所有段视作 prefix
    let n = parts.len();
    let owner_kind = parts[n - 3];
    let owner_id: i64 = parts[n - 2].parse().ok()?;
    let kind = parts[n - 1];
    if !matches!(
        kind,
        "DRAWING" | "3D_MODEL" | "CAD_2D" | "G_CODE" | "SETUP_SHEET"
    ) {
        return None;
    }
    if !matches!(owner_kind, "part" | "assembly") {
        return None;
    }
    Some(LegacyKey {
        owner_kind: owner_kind.to_string(),
        owner_id,
        kind: kind.to_string(),
        sha16: sha16.to_string(),
        safe_filename: safe_filename.to_string(),
    })
}

/// 把 `LegacyKey` 重写成新模板 key（用于读端点 fallback 第二次尝试）。
///
/// 模板：`{prefix}{sha16}_{safe_filename}`（保留 LegacyKey 里的 safe_filename）。
///
/// `prefix` 仍走 caller 传入的 `cos.upload_prefix`（fallback 与 build_cas_key
/// 一致都接受 `"uploads"` 或 `"uploads/"` 两种风格）。
pub fn rewrite_legacy_to_new(prefix: &str, lk: &LegacyKey) -> String {
    build_cas_key(prefix, &lk.sha16, &lk.safe_filename)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_filename_basic_ascii() {
        assert_eq!(safe_filename("drawing.pdf"), "drawing.pdf");
        assert_eq!(safe_filename("foo_bar-v1.PDF"), "foo_bar-v1.PDF");
    }

    #[test]
    fn safe_filename_strips_directory() {
        assert_eq!(safe_filename("/path/to/foo bar.PDF"), "foo_bar.PDF");
        assert_eq!(safe_filename("no_dir.txt"), "no_dir.txt");
    }

    #[test]
    fn safe_filename_collapses_non_ascii() {
        // 每个汉字 / 空格 / 括号折叠成单个 _；stem 全部 _- 字符时回退 "file"
        // （与 Python `safe_filename` 一致：docstring 写 `__.pdf` 是错的）
        assert_eq!(safe_filename("图纸.pdf"), "file.pdf");
        assert_eq!(safe_filename("主轴 (v2).STEP"), "v2.STEP");
    }

    #[test]
    fn safe_filename_strips_edge_punct() {
        // "...foo" 视为有 ext（foo 长度 3，在 1..=7）；stem 全 `._-` → 回退 "file"
        assert_eq!(safe_filename("...foo"), "file.foo");
        assert_eq!(safe_filename(""), "file");
        assert_eq!(safe_filename("___"), "file");
    }

    #[test]
    fn safe_filename_truncates_with_ext_preserved() {
        let long = format!("{}.pdf", "a".repeat(200));
        let s = safe_filename(&long);
        assert!(s.ends_with(".pdf"), "扩展名必须保留: {s}");
        assert!(s.len() <= 80, "长度不得超过 80: {}", s.len());
    }

    #[test]
    fn safe_filename_truncates_without_ext() {
        let long = "a".repeat(200);
        let s = safe_filename(&long);
        assert_eq!(s.len(), 80);
    }

    #[test]
    fn build_cas_key_basic_format() {
        let key = build_cas_key(
            "uploads/",
            "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
            "drawing.pdf",
        );
        assert_eq!(key, "uploads/abcdef0123456789_drawing.pdf");
    }

    #[test]
    fn build_cas_key_uses_safe_filename() {
        let key = build_cas_key("uploads/", "00112233445566778899aabbccddeeff", "主轴.step");
        // 安全文件名折叠后是 "file.step"（stem 全 _- → 回退 "file"）；
        // sha16 是 sha256_hex 前 16 字符；扁平化后只剩 prefix + sha16_<filename>
        assert_eq!(key, "uploads/0011223344556677_file.step");
    }

    #[test]
    fn build_cas_key_auto_appends_trailing_slash() {
        // 2026-09-11 新增：prefix 缺尾斜杠时自动补，等价 `uploads/`
        let k1 = build_cas_key("uploads", "00112233445566778899aabbccddeeff", "a.pdf");
        let k2 = build_cas_key("uploads/", "00112233445566778899aabbccddeeff", "a.pdf");
        assert_eq!(k1, k2);
        assert_eq!(k1, "uploads/0011223344556677_a.pdf");
    }

    #[test]
    fn build_cas_key_empty_prefix_yields_root_key() {
        // 2026-09-11 新增：空 prefix 直接拼 sha16_<filename>，不带多余 `/`
        let k = build_cas_key("", "00112233445566778899aabbccddeeff", "x.step");
        assert_eq!(k, "0011223344556677_x.step");
    }

    #[test]
    fn parse_legacy_key_round_trip() {
        // 历史五段 key 反向解析
        let lk = parse_legacy_key("uploads/part/12345/DRAWING/abcdef0123456789_drawing.pdf")
            .expect("历史五段 key 必须解析成功");
        assert_eq!(lk.owner_kind, "part");
        assert_eq!(lk.owner_id, 12345);
        assert_eq!(lk.kind, "DRAWING");
        assert_eq!(lk.sha16, "abcdef0123456789");
        assert_eq!(lk.safe_filename, "drawing.pdf");
    }

    #[test]
    fn parse_legacy_key_unknown_kind_returns_none() {
        // 非白名单 kind → None
        assert!(parse_legacy_key("uploads/part/1/UNKNOWN/abcdef0123456789_a.pdf").is_none());
    }

    #[test]
    fn parse_legacy_key_wrong_segment_count_returns_none() {
        // 段数 ≠ 4 → None（新两段模板：含 prefix 才 3 段）
        assert!(parse_legacy_key("uploads/part/1").is_none());
        assert!(parse_legacy_key("uploads/abcdef0123456789_a.pdf").is_none());
    }

    #[test]
    fn parse_legacy_key_bad_sha16_returns_none() {
        // sha16 长度不是 16 或含非 hex → None
        assert!(parse_legacy_key("uploads/part/1/DRAWING/short_a.pdf").is_none());
        assert!(parse_legacy_key("uploads/part/1/DRAWING/zzzzzzzzzzzzzzzz_a.pdf").is_none());
    }

    #[test]
    fn rewrite_legacy_to_new_round_trip() {
        // 解析历史五段后重写成新两段，应与 build_cas_key 结果一致
        let lk = parse_legacy_key("uploads/part/12345/DRAWING/abcdef0123456789_drawing.pdf")
            .expect("parse ok");
        let new_key = rewrite_legacy_to_new("uploads/", &lk);
        let expected = build_cas_key("uploads/", "abcdef0123456789...", "drawing.pdf");
        assert_eq!(new_key, "uploads/abcdef0123456789_drawing.pdf");
        assert_eq!(new_key, expected);
    }
}
