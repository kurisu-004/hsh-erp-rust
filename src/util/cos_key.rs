//! COS 对象 key 派生工具
//!
//! 对齐 Python `backend-python/core/file_hash.py:32-78`：
//! - `safe_filename(name)`：把任意 filename 折叠为 `[A-Za-z0-9._-]` 范围；
//!   长度上限 80 字符，扩展名优先保留。
//! - `build_cas_key(prefix, owner_kind, owner_id, kind, sha256_hex, filename)`：
//!   按 Python `make_object_key` 同款模板拼 CAS key：
//!   `{prefix}{owner_kind}/{owner_id}/{KIND}/{sha16}_{safe_filename}`
//!
//! 跨语言产物可读性：同一 owner + sha + filename 在两个后端派生出同一 key，
//! 便于人工从桶扫描恢复 DB 关联。
//!
//! 2026-09-11 新增

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

/// 按 Python `make_object_key` 同款模板拼 CAS key。
///
/// 模板：`{prefix}{owner_kind}/{owner_id}/{KIND}/{sha16}_{safe_filename}`
///
/// 参数：
/// - `prefix`：COS 桶内上传前缀（通常带尾斜杠，如 `"uploads/"`）；由
///   `cos.upload_prefix` 传入。**函数内部自动保证尾斜杠**：传入 `"uploads"`
///   与 `"uploads/"` 等价；空串视作 `""`（桶根）。
/// - `owner_kind`：字面量 `"part"` / `"assembly"`，用于路径段。
/// - `owner_id`：owner 雪花 ID（正整数）。
/// - `kind`：PartFileKind 枚举的字符串值（如 `"DRAWING"` / `"3D_MODEL"`）。
/// - `sha256_hex`：完整 64 字符 SHA-256 hex（取前 16 字符作为 `sha16`）。
/// - `filename`：原始文件名（中文 / 特殊字符 / 路径都可，内部走 `safe_filename`）。
///
/// 注：扩展名已包含在 `safe_filename` 结果里（`foo.pdf` / `__.pdf`），模板
/// 不再追加 `.ext`。
pub fn build_cas_key(
    prefix: &str,
    owner_kind: &str,
    owner_id: i64,
    kind: &str,
    sha256_hex: &str,
    filename: &str,
) -> String {
    let sha16 = &sha256_hex[..16.min(sha256_hex.len())];
    let safe = safe_filename(filename);
    // 2026-09-11 修改：自动保证 prefix 尾斜杠；兼容 `.env` 里写成
    // `uploads` 或 `uploads/` 的两种风格，以及测试 config 里的裸值。
    let prefix = if prefix.is_empty() || prefix.ends_with('/') {
        prefix.to_string()
    } else {
        format!("{prefix}/")
    };
    format!("{prefix}{owner_kind}/{owner_id}/{kind}/{sha16}_{safe}")
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
            "part",
            12345,
            "DRAWING",
            "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
            "drawing.pdf",
        );
        assert_eq!(
            key,
            "uploads/part/12345/DRAWING/abcdef0123456789_drawing.pdf"
        );
    }

    #[test]
    fn build_cas_key_uses_safe_filename() {
        let key = build_cas_key(
            "uploads/",
            "assembly",
            99,
            "3D_MODEL",
            "00112233445566778899aabbccddeeff",
            "主轴.step",
        );
        // 安全文件名折叠后是 "file.step"（stem 全 _- → 回退 "file"）；
        // sha16 是 sha256_hex 前 16 字符
        assert_eq!(
            key,
            "uploads/assembly/99/3D_MODEL/0011223344556677_file.step"
        );
    }

    #[test]
    fn build_cas_key_auto_appends_trailing_slash() {
        // 2026-09-11 新增：prefix 缺尾斜杠时自动补，等价 `uploads/`
        let k1 = build_cas_key(
            "uploads",
            "part",
            1,
            "DRAWING",
            "00112233445566778899aabbccddeeff",
            "a.pdf",
        );
        let k2 = build_cas_key(
            "uploads/",
            "part",
            1,
            "DRAWING",
            "00112233445566778899aabbccddeeff",
            "a.pdf",
        );
        assert_eq!(k1, k2);
        assert_eq!(k1, "uploads/part/1/DRAWING/0011223344556677_a.pdf");
    }

    #[test]
    fn build_cas_key_empty_prefix_yields_root_key() {
        // 2026-09-11 新增：空 prefix 直接拼 owner_kind，不带多余 `/`
        let k = build_cas_key(
            "",
            "part",
            7,
            "3D_MODEL",
            "00112233445566778899aabbccddeeff",
            "x.step",
        );
        assert_eq!(k, "part/7/3D_MODEL/0011223344556677_x.step");
    }
}
