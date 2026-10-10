//! part_file kind → 扩展名 / file_type / 期望 content_type 映射
//!
//! 对齐 Python 后端 `core/_file_kind_policy.py`（2026-09-11 新增）。
//!
//! ## 用途
//! - service 层 `upload_part_file` 通用函数在落库前做扩展名校验：
//!   1. 取扩展名（`ext_of`），缺扩展名 → 拒绝
//!   2. 在 `allowed_exts(kind)` 白名单内 → 否则拒绝
//!   3. `content_type` 在 `expected_content_types_for_ext(&ext)` 白名单内 → 否则拒绝
//! - file_type 由 `file_type_for_ext(&ext)` 推导（`step`/`stp` → `"STEP"` 等）
//!   避免 `upload_3d_model` 在 service 里再写一遍映射。

use std::path::Path;

/// kind → 允许的扩展名（小写，不含点）。
///
/// 扩展名作为静态切片返回，service 层用 `contains` 判断。
///
/// 2026-09-11 新增
pub fn allowed_exts(kind: &str) -> &'static [&'static str] {
    match kind {
        "DRAWING" => &["pdf"],
        "3D_MODEL" => &["step", "stp", "iges", "igs", "stl", "obj", "3mf"],
        _ => &[],
    }
}

/// 扩展名（小写，不含点） → file_type 字符串（如 `"PDF"` / `"STEP"`）。
///
/// **大小写不敏感**：传入 `"STL"` / `"stl"` 等价。
/// 返回 `Option`：扩展名无对应 file_type（不在白名单）时 `None`，
/// service 层用其判定是否接受。
///
/// 2026-09-11 新增
pub fn file_type_for_ext(ext: &str) -> Option<&'static str> {
    match ext.to_ascii_lowercase().as_str() {
        "pdf" => Some("PDF"),
        "step" | "stp" => Some("STEP"),
        "iges" | "igs" => Some("IGES"),
        "stl" => Some("STL"),
        "obj" => Some("OBJ"),
        "3mf" => Some("3MF"),
        _ => None,
    }
}

/// 扩展名 → 浏览器可能上传的 content_type 白名单（含 `application/octet-stream` 兜底）。
///
/// **大小写不敏感**（与 `file_type_for_ext` 一致）。
///
/// service 层据此校验客户端声明的 `content_type` 是否与扩展名一致；
/// 返回 `&[]` 表示该扩展名不接受任何 content_type（实际不会到这，因为
/// `file_type_for_ext` 已先把不在白名单的扩展名挡掉）。
///
/// 2026-09-11 新增
pub fn expected_content_types_for_ext(ext: &str) -> &'static [&'static str] {
    match ext.to_ascii_lowercase().as_str() {
        "pdf" => &["application/pdf"],
        "step" | "stp" => &[
            "application/step",
            "application/stp",
            "application/octet-stream",
        ],
        "iges" | "igs" => &[
            "application/iges",
            "application/igs",
            "application/octet-stream",
        ],
        "stl" => &["model/stl", "application/octet-stream"],
        "obj" => &["model/obj", "application/octet-stream"],
        "3mf" => &[
            "application/vnd.ms-3mfdocument",
            "model/3mf",
            "application/octet-stream",
        ],
        _ => &[],
    }
}

/// 从 filename 取扩展名（小写，不含点），若无扩展名返回 `None`。
///
/// 使用 `std::path::Path::extension`：自动处理 `.tar.gz` 这种多段扩展
/// （取最后一段），与 Python `os.path.splitext` 行为一致。
///
/// 边界：`"."` / `".."` / `"..."` / `"foo."` 等无扩展名场景
/// `Path::extension` 返回 `Some("")`，本函数过滤空字符串。
///
/// 2026-09-11 新增
pub fn ext_of(filename: &str) -> Option<String> {
    let ext = Path::new(filename).extension().and_then(|s| s.to_str())?;
    if ext.is_empty() {
        return None;
    }
    Some(ext.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowed_exts_drawing_only_pdf() {
        assert_eq!(allowed_exts("DRAWING"), &["pdf"]);
    }

    #[test]
    fn allowed_exts_3d_model_includes_step_stl() {
        let exts = allowed_exts("3D_MODEL");
        assert!(exts.contains(&"step"));
        assert!(exts.contains(&"stp"));
        assert!(exts.contains(&"stl"));
        assert!(exts.contains(&"obj"));
        assert!(exts.contains(&"3mf"));
    }

    #[test]
    fn allowed_exts_unknown_kind_returns_empty() {
        assert_eq!(allowed_exts("UNKNOWN"), &[] as &[&str]);
    }

    #[test]
    fn file_type_for_ext_maps_step_stp_to_step() {
        assert_eq!(file_type_for_ext("step"), Some("STEP"));
        assert_eq!(file_type_for_ext("stp"), Some("STEP"));
        assert_eq!(file_type_for_ext("pdf"), Some("PDF"));
        assert_eq!(file_type_for_ext("STL"), Some("STL"));
        assert_eq!(file_type_for_ext("3mf"), Some("3MF"));
    }

    #[test]
    fn file_type_for_ext_unknown_returns_none() {
        assert_eq!(file_type_for_ext("exe"), None);
        assert_eq!(file_type_for_ext(""), None);
    }

    #[test]
    fn expected_content_types_pdf_only_application_pdf() {
        assert_eq!(expected_content_types_for_ext("pdf"), &["application/pdf"]);
    }

    #[test]
    fn expected_content_types_step_includes_octet_stream() {
        let cts = expected_content_types_for_ext("step");
        assert!(cts.contains(&"application/step"));
        assert!(cts.contains(&"application/octet-stream"));
    }

    #[test]
    fn ext_of_basic_ascii() {
        assert_eq!(ext_of("foo.pdf"), Some("pdf".to_string()));
        assert_eq!(ext_of("a.b.c.step"), Some("step".to_string()));
        assert_eq!(ext_of("无扩展名"), None);
    }

    #[test]
    fn ext_of_uppercase_lowered() {
        assert_eq!(ext_of("FOO.PDF"), Some("pdf".to_string()));
        assert_eq!(ext_of("Bar.Step"), Some("step".to_string()));
    }

    #[test]
    fn ext_of_no_extension() {
        assert_eq!(ext_of("noext"), None);
        assert_eq!(ext_of(""), None);
        assert_eq!(ext_of("..."), None);
    }
}
