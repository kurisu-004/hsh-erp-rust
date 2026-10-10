//! part_file kind → 扩展名 / file_type / 期望 content_type 映射
//!
//! 对齐 Python 后端 `core/_file_kind_policy.py`（2026-09-11 新增）。
//!
//! ## 用途
//! - service 层 `upload_file_for_owner` 通用函数在落库前做扩展名校验：
//!   1. 取扩展名（`ext_of`），缺扩展名 → 拒绝
//!   2. 在 `allowed_exts(kind)` 白名单内 → 否则拒绝
//!   3. `content_type` 在 `expected_content_types_for_ext(&ext)` 白名单内 → 否则拒绝
//! - file_type 由 `file_type_for_ext(&ext)` 推导（`step`/`stp` → `"STEP"` 等），
//!   避免调用方在 service 里再写一遍映射。

use std::path::Path;

/// kind → 允许的扩展名（小写，不含点）。
///
/// 扩展名作为静态切片返回，service 层用 `contains` 判断。
///
/// 2026-10-13 **新增 `ASSEMBLY_MASTER`**：上传四合一（`POST /files/objects`
/// 同时承接 part 与 assembly 两条 owner）之后，装配件 PDF 也走
/// `upload_file_for_owner`，而本白名单原先**只认 DRAWING / 3D_MODEL** ⇒
/// `owner_kind=ASSEMBLY` 的上传会一律撞 21102。取值 `&["pdf"]` 与被删的
/// `AssemblyService::upload_assembly_files` 的判定（`ext != "pdf"` 即拒）逐字等价。
/// 连带效果：`dto::validate::check_kind` 此前会把 `ASSEMBLY_MASTER` 判成「未知 kind」
/// 一并拒掉，`POST /files/objects/confirm` 现在也接受该 kind。
///
/// ⚠️ **仍未覆盖**：CAD_2D / G_CODE / SETUP_SHEET 三个 kind 不在本白名单里 ——
/// 它们只经 CNC 配对端点（`POST /files/cnc/pairs`，走 `cnc/service.rs` 的专用
/// 分支）写入，不经过本函数，故不受影响。
///
/// 2026-09-11 新增
pub fn allowed_exts(kind: &str) -> &'static [&'static str] {
    match kind {
        "DRAWING" => &["pdf"],
        "3D_MODEL" => &["step", "stp", "iges", "igs", "stl", "obj", "3mf"],
        "ASSEMBLY_MASTER" => &["pdf"],
        _ => &[],
    }
}

/// `owner_kind` × `kind` 允许对（2026-10-13 PR1b review 第 1 轮 MINOR-3）。
///
/// **为什么需要这张表**：四合一之前，`owner_kind` 与 `kind` 的配对是**服务端结构
/// 决定的** —— `upload-drawing` 端点在 path 上钉死「owner 是 part、kind 是
/// DRAWING」，`POST /assemblies/{id}/files` 钉死「owner 是装配件、kind 恒
/// ASSEMBLY_MASTER」。合并成一个端点后两个字段都改由 client 在 multipart `data`
/// 里提供，而 `allowed_exts` 只按 `kind` 判 ⇒ `owner_kind=PART` +
/// `kind=ASSEMBLY_MASTER`、`owner_kind=ASSEMBLY` + `kind=DRAWING` 都会被放行。
/// 后者造出「装配件的图纸」这种无意义行，前者更糟：造出的
/// **part + ASSEMBLY_MASTER** 行在 `soft_delete_file` 的 kind 分派里长期不可删
/// （见 `part_file/mod.rs` 的角色矩阵 / 软删一节），等于「写得了、删不了」的
/// 永久垃圾数据。故配对由服务端**重新强制**。
///
/// **与 `allowed_exts` 的分工**：本表管「谁（owner）能挂哪种文件」，`allowed_exts`
/// 管「这种文件能是什么格式」。两张表都过了才进扩展名 / content_type 校验。
///
/// **fail-safe 方向**：不在表内的组合一律 `false`。将来若给 `allowed_exts` 加了
/// 新的 kind（CAD_2D / G_CODE / SETUP_SHEET 走 CNC 配对端点写入，不经本函数），
/// 本表**不会**自动放行它 —— 那是有意的：新 kind 必须先在这里显式声明它挂哪种
/// owner，避免又出现「能写不能删」的行。
///
/// 2026-10-13 新增
pub fn owner_kind_allows_kind(owner_kind: &str, kind: &str) -> bool {
    match owner_kind {
        "PART" => matches!(kind, "DRAWING" | "3D_MODEL"),
        "ASSEMBLY" => matches!(kind, "ASSEMBLY_MASTER"),
        // 未知 owner_kind 由 `PartFileService::assert_owner_exists` 报 20104，
        // 这里直接 false（不区分错误码，两道守卫都拒即可）。
        _ => false,
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
    fn allowed_exts_assembly_master_only_pdf() {
        // 2026-10-13：上传四合一后装配件 PDF 也走 upload_file_for_owner，
        // 该白名单必须认 ASSEMBLY_MASTER，否则 assembly owner 的上传一律 21102。
        assert_eq!(allowed_exts("ASSEMBLY_MASTER"), &["pdf"]);
        // 与被删的 upload_assembly_files 判定逐字等价：只收 PDF
        assert!(!allowed_exts("ASSEMBLY_MASTER").contains(&"step"));
    }

    #[test]
    fn allowed_exts_unknown_kind_returns_empty() {
        assert_eq!(allowed_exts("UNKNOWN"), &[] as &[&str]);
    }

    #[test]
    fn owner_kind_allows_kind_normal_pairs() {
        // PART owner 只能挂零件自己的两类文件
        assert!(owner_kind_allows_kind("PART", "DRAWING"));
        assert!(owner_kind_allows_kind("PART", "3D_MODEL"));
        // ASSEMBLY owner 只能挂装配件整图
        assert!(owner_kind_allows_kind("ASSEMBLY", "ASSEMBLY_MASTER"));
    }

    #[test]
    fn owner_kind_allows_kind_rejects_cross_pairs() {
        // ★ 这两组正是四合一后被放行的「脏组合」：
        //   前者造出永不可删的 part 行（soft_delete_file 的 kind 分派不含 ASSEMBLY_MASTER）
        //   后者造出语义上不存在的「装配件的零件图纸」
        assert!(!owner_kind_allows_kind("PART", "ASSEMBLY_MASTER"));
        assert!(!owner_kind_allows_kind("ASSEMBLY", "DRAWING"));
        assert!(!owner_kind_allows_kind("ASSEMBLY", "3D_MODEL"));
    }

    #[test]
    fn owner_kind_allows_kind_rejects_cnc_kinds_and_unknowns() {
        // CAD_2D / G_CODE / SETUP_SHEET 只经 `POST /files/cnc/pairs` 写入，
        // 两条写路径都不该收它们
        for k in ["CAD_2D", "G_CODE", "SETUP_SHEET"] {
            assert!(!owner_kind_allows_kind("PART", k), "PART 不该收 {k}");
        }
        assert!(!owner_kind_allows_kind("PART", "UNKNOWN"));
        assert!(!owner_kind_allows_kind("PART_FILE", "DRAWING"));
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
