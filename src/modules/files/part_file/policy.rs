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
/// 2026-10-16 **新增 `DELIVERY_TEMPLATE`**：送货单打印模板复用 `t_part_file`
/// 承载（不新建模板表），扩展名**只收 `xlsx`**。
/// ⚠️ 「只收 xlsx」是**刻意不给 `.xls`**：`.xls` 是 BIFF 二进制格式，前端
/// hucre / SheetJS 侧读它要走另一条兼容路径，而需求原文就是「模板支持 xlsx
/// 格式」。旧 `.xls` 模板请先另存为 `.xlsx` 再上传。
/// 连带效果：`dto::validate::check_kind` 复用同一张白名单，所以
/// `POST /files/objects/confirm`（STS 两段式）也顺带接受该 kind —— 但
/// `print_template` 域**只用一次性 multipart 上传**，没有走 confirm 的调用方，
/// 见 `docs/api/print_template.md` §8.3 第 3 条。
///
/// 2026-09-11 新增
pub fn allowed_exts(kind: &str) -> &'static [&'static str] {
    match kind {
        "DRAWING" => &["pdf"],
        "3D_MODEL" => &["step", "stp", "iges", "igs", "stl", "obj", "3mf"],
        "ASSEMBLY_MASTER" => &["pdf"],
        "DELIVERY_TEMPLATE" => &["xlsx"],
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
/// 2026-10-16 **新增第三种 owner `CUSTOMER`**（只收 `DELIVERY_TEMPLATE`）：
/// 送货单打印模板挂在**一级客户**上（`print_template` 域）。
/// ⚠️ `CUSTOMER` 是**单向**的：`PART` / `ASSEMBLY` 两臂刻意**不加**
/// `DELIVERY_TEMPLATE` —— 否则 `POST /files/objects`（owner_kind 与 kind 都由
/// client 在 multipart `data` 里给）会造出「零件挂送货单打印模板」这种
/// 语义上不存在的行，且这类行**无人读**（`print_template` 域只按
/// `(customer_id, DELIVERY_TEMPLATE)` 查），是纯垃圾数据。
/// 未列出的 owner_kind 一律返 `false`（fail-safe），该行为**不变**。
///
/// 2026-10-13 新增
pub fn owner_kind_allows_kind(owner_kind: &str, kind: &str) -> bool {
    match owner_kind {
        "PART" => matches!(kind, "DRAWING" | "3D_MODEL"),
        "ASSEMBLY" => matches!(kind, "ASSEMBLY_MASTER"),
        // 2026-10-16 新增：一级客户只挂送货单打印模板（xlsx）。
        "CUSTOMER" => matches!(kind, "DELIVERY_TEMPLATE"),
        // 未知 owner_kind 由 `PartFileService::assert_owner_exists` 报 20104，
        // 这里直接 false（不区分错误码，两道守卫都拒即可）。
        _ => false,
    }
}

/// 扩展名（大写） → `t_part_file.file_type` 字符串（如 `"PDF"` / `"STEP"`）。
///
/// **大小写不敏感**：传入 `"STL"` / `"stl"` 等价。
/// 返回 `Option`：扩展名无对应 file_type（不在白名单）时 `None`，
/// service 层用其判定是否接受。
///
/// 2026-10-16 新增 `"xlsx" => "XLSX"`（`DELIVERY_TEMPLATE` 的唯一格式）。
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
        "xlsx" => Some("XLSX"),
        _ => None,
    }
}

/// 扩展名 → 浏览器可能上传的 content_type 白名单。
///
/// **大小写不敏感**（与 `file_type_for_ext` 一致）。
///
/// service 层据此校验客户端声明的 `content_type` 是否与扩展名一致；
/// 返回 `&[]` 表示该扩展名不接受任何 content_type（实际不会到这，因为
/// `file_type_for_ext` 已先把不在白名单的扩展名挡掉）。
///
/// ⚠️ **白名单里给不给 `application/octet-stream` 是逐扩展名的取舍，不是默认值**：
/// 本文件对 **pdf 从来不给**它 —— 浏览器对 PDF 一律发 `application/pdf`，
/// 收 octet-stream 等于「不校验 MIME」。`step` / `stl` / `obj` / `3mf` 那几支
/// 则**故意**收了（部分浏览器 / 拖拽上传对未知类型一律发 octet-stream，收了才
/// 是可用性而不是放水）。
/// 2026-10-16 **新增 `xlsx` 分支，只收精确值**：
/// OOXML 的 MIME 是标准化的，主流浏览器 / Element Plus 的 `:accept` 都发
/// `application/vnd.openxmlformats-officedocument.spreadsheetml.sheet`，
/// **刻意不给 octet-stream 兜底** —— 收下 octet-stream 等于让任何文件改个名
/// 成 `.xlsx` 就能过 content_type 这道闸，而模板是要被前端 SheetJS / hucre
/// 直接解析的，伪装进来只会在渲染阶段炸。
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
        // ⚠️ 只收精确值，**不给** `application/octet-stream` 兜底（同 pdf 一节的理由）。
        "xlsx" => &["application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"],
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

    /// 2026-10-16：`DELIVERY_TEMPLATE` **只收 xlsx**（刻意不收 `.xls`）。
    #[test]
    fn allowed_exts_delivery_template_only_xlsx() {
        assert_eq!(allowed_exts("DELIVERY_TEMPLATE"), &["xlsx"]);
        assert!(!allowed_exts("DELIVERY_TEMPLATE").contains(&"pdf"));
        assert!(!allowed_exts("DELIVERY_TEMPLATE").contains(&"xls"));
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

    /// 2026-10-16：`CUSTOMER` 只挂 `DELIVERY_TEMPLATE`，且**反向一律拒**。
    ///
    /// 反向那三条（PART / ASSEMBLY 挂 DELIVERY_TEMPLATE）是本条守卫存在的全部
    /// 理由：`POST /files/objects` 的 owner_kind 与 kind 都由 client 给，
    /// 不挡就会有「零件挂送货单打印模板」这种无人读、删不掉的垃圾行。
    #[test]
    fn owner_kind_allows_kind_customer_is_delivery_template_only() {
        assert!(owner_kind_allows_kind("CUSTOMER", "DELIVERY_TEMPLATE"));
        for k in ["DRAWING", "3D_MODEL", "ASSEMBLY_MASTER"] {
            assert!(
                !owner_kind_allows_kind("CUSTOMER", k),
                "CUSTOMER 不该收 {k}"
            );
        }
        assert!(!owner_kind_allows_kind("PART", "DELIVERY_TEMPLATE"));
        assert!(!owner_kind_allows_kind("ASSEMBLY", "DELIVERY_TEMPLATE"));
        // 未列出的 owner_kind 继续 fail-safe（新增 CUSTOMER 不得放宽默认臂）
        assert!(!owner_kind_allows_kind("CUSTOMERS", "DELIVERY_TEMPLATE"));
        assert!(!owner_kind_allows_kind("SHELF", "DELIVERY_TEMPLATE"));
    }

    #[test]
    fn file_type_for_ext_maps_step_stp_to_step() {
        assert_eq!(file_type_for_ext("step"), Some("STEP"));
        assert_eq!(file_type_for_ext("stp"), Some("STEP"));
        assert_eq!(file_type_for_ext("pdf"), Some("PDF"));
        assert_eq!(file_type_for_ext("STL"), Some("STL"));
        assert_eq!(file_type_for_ext("3mf"), Some("3MF"));
        // 2026-10-16：xlsx → XLSX（送货单打印模板）
        assert_eq!(file_type_for_ext("xlsx"), Some("XLSX"));
        assert_eq!(file_type_for_ext("XLSX"), Some("XLSX"));
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

    /// 2026-10-16：`xlsx` **只收精确 OOXML MIME**，不给 octet-stream 兜底。
    #[test]
    fn expected_content_types_xlsx_is_exact_only() {
        let cts = expected_content_types_for_ext("xlsx");
        assert_eq!(
            cts,
            &["application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"]
        );
        assert!(
            !cts.contains(&"application/octet-stream"),
            "xlsx 刻意不给 octet-stream 兜底：收下它等于放行「改名成 .xlsx 的任意文件」"
        );
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
