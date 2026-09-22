//! `scan_add` 单元测试（2026-09-22 review 第 1 轮抽出）
//!
//! 5 组测试（classify / resolve_scan_kind / classify_5groups / c_group_distribution /
//! attachable_batches）从 `scan/mod.rs` 末尾移到本文件，让 `scan/mod.rs` 行数
//! 从 1294 降到 < 1000 行上限（conventions §2）。
//!
//! rust 2018+ 规定 `#[cfg(test)] mod` 之后不能再放任何生产代码——所以本文件
//! 整体是 `#[cfg(test)]` 包裹，只有测试编译时才被纳入。
//!
//! `super::` 仍然指向 `scan/mod.rs`（本文件的父 module），`super::classify::*`
//! 等路径不变。

#[cfg(test)]
mod classify_tests {
    use super::super::GroupWithMemberIds;
    use crate::modules::delivery_note::model::NoteScope;

    fn g(id: i64, members: &[i64]) -> GroupWithMemberIds {
        GroupWithMemberIds {
            group_id: id,
            member_ids: members.to_vec(),
        }
    }

    #[test]
    fn classify_no_groups_returns_l1wide() {
        assert_eq!(NoteScope::classify(101, &[]), NoteScope::L1Wide);
    }

    #[test]
    fn classify_member_returns_group() {
        let groups = vec![g(10, &[101, 102, 103])];
        assert_eq!(NoteScope::classify(102, &groups), NoteScope::Group(10));
    }

    #[test]
    fn classify_non_member_returns_leaf() {
        let groups = vec![g(10, &[101, 102, 103])];
        assert_eq!(NoteScope::classify(104, &groups), NoteScope::Leaf(104));
    }

    #[test]
    fn classify_with_l1_self_returns_leaf_l1_id() {
        let groups = vec![g(10, &[101, 102, 103])];
        assert_eq!(NoteScope::classify(100, &groups), NoteScope::Leaf(100));
    }
}

#[cfg(test)]
mod scan_resolve_tests {
    use super::super::resolve_scan_kind::{resolve_scan_kind, ScanKind};
    use crate::modules::assembly::model::TAssembly;
    use crate::modules::part::model::TPart;

    /// 构造一个最小化的 TPart 用作 fixture。
    fn make_part(id: i64, assembly_id: Option<i64>) -> TPart {
        TPart {
            id,
            serial_no: Some(format!("F{id:04}")),
            name: format!("Part {id}"),
            drawing_no: format!("D-{id:03}"),
            applicant_name: format!("Applicant {id}"),
            quantity: 1,
            request_date: chrono::NaiveDate::from_ymd_opt(2026, 8, 22).unwrap(),
            planned_delivery_date: chrono::NaiveDate::from_ymd_opt(2026, 8, 22).unwrap(),
            customer_id: 100 + id,
            assembly_id,
            status: "INSPECTION".to_string(),
            is_urgent: false,
            next_process_id: None,
            order_no: None,
            system_delivery_date: None,
            note: None,
            version: 0,
            created_at: chrono::NaiveDate::from_ymd_opt(2026, 8, 22)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap(),
            created_by: None,
            updated_at: chrono::NaiveDate::from_ymd_opt(2026, 8, 22)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap(),
            updated_by: None,
            deleted_at: None,
            process_chain_id: None,
        }
    }

    fn make_assembly(id: i64) -> TAssembly {
        TAssembly {
            id,
            drawing_no: format!("A-{id:03}"),
            name: format!("Asm {id}"),
            applicant_name: None,
            customer_id: 900 + id,
            request_date: chrono::NaiveDate::from_ymd_opt(2026, 8, 22).unwrap(),
            planned_delivery_date: chrono::NaiveDate::from_ymd_opt(2026, 9, 22).unwrap(),
            is_urgent: false,
            status: "ACTIVE".to_string(),
            version: 0,
            created_at: chrono::NaiveDate::from_ymd_opt(2026, 8, 22)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap(),
            created_by: None,
            updated_at: chrono::NaiveDate::from_ymd_opt(2026, 8, 22)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap(),
            updated_by: None,
            deleted_at: None,
            serial_no: Some(format!("ASMR{id:04}")),
            quantity: 1,
            unit_price: None,
            total_price: None,
            order_no: None,
            system_delivery_date: None,
            note: None,
        }
    }

    #[test]
    fn scan_resolve_part_no_assembly_returns_part_kind() {
        let p = make_part(1, None);
        assert_eq!(resolve_scan_kind(Some(&p), None), ScanKind::StandalonePart);
    }

    #[test]
    fn scan_resolve_part_with_assembly_returns_assembly_kind() {
        let p = make_part(2, Some(42));
        assert_eq!(
            resolve_scan_kind(Some(&p), None),
            ScanKind::PartOfAssembly(42)
        );
    }

    #[test]
    fn scan_resolve_assembly_serial_returns_assembly_kind() {
        let a = make_assembly(7);
        assert_eq!(resolve_scan_kind(None, Some(&a)), ScanKind::Assembly);
    }

    #[test]
    fn scan_resolve_both_hits_prefers_part() {
        // 两边都中：同 serial 不可能真发生（数据前提），但当输入同时给出时，
        // part 路径优先（设计 §5：t_part.serial_no == code 命中）。
        let p = make_part(3, Some(99));
        let a = make_assembly(99);
        assert_eq!(
            resolve_scan_kind(Some(&p), Some(&a)),
            ScanKind::PartOfAssembly(99)
        );
    }

    #[test]
    fn scan_resolve_unknown_returns_unknown() {
        assert_eq!(resolve_scan_kind(None, None), ScanKind::Unknown);
    }
}

#[cfg(test)]
mod classify_5groups_tests {
    use super::super::classify::{
        classify_invalid_state, is_attachable_state, is_inspectable_state,
    };
    use crate::modules::part::batch::model::TPartBatch;

    fn b(status: &str, holder: Option<i64>, location: Option<&str>) -> TPartBatch {
        TPartBatch {
            id: 0,
            part_id: 1,
            batch_no: 1,
            quantity: 1,
            status: status.to_string(),
            location: location.map(str::to_string),
            current_holder_id: holder,
            current_process_step_id: None,
            delivery_note_id: None,
            parent_batch_id: None,
            version: 0,
            created_at: chrono::DateTime::from_timestamp(0, 0).unwrap().naive_utc(),
            created_by: None,
            updated_at: chrono::DateTime::from_timestamp(0, 0).unwrap().naive_utc(),
            updated_by: None,
            deleted_at: None,
        }
    }

    #[test]
    fn c_group_delivered_short_circuits() {
        assert_eq!(
            classify_invalid_state(&b("DELIVERED", None, None)),
            Some("DELIVERED")
        );
        assert_eq!(
            classify_invalid_state(&b("OUTSOURCE", None, None)),
            Some("OUTSOURCE")
        );
        assert_eq!(
            classify_invalid_state(&b("COMPLETED", None, None)),
            Some("COMPLETED")
        );
        assert_eq!(
            classify_invalid_state(&b("CANCELLED", None, None)),
            Some("CANCELLED")
        );
    }

    #[test]
    fn c_group_in_process_held_is_invalid() {
        // 工人持有（location='WORKER'）→ C 组
        assert_eq!(
            classify_invalid_state(&b("IN_PROCESS", Some(42), Some("WORKER"))),
            Some("IN_PROCESS_HELD_BY_WORKER")
        );
        // 货架持有（holder = shelf id，location='PRODUCTION_SHELF'）→ 非 C 组（回归：多态 holder 误判）
        assert_eq!(
            classify_invalid_state(&b("IN_PROCESS", Some(42), Some("PRODUCTION_SHELF"))),
            None
        );
        assert_eq!(classify_invalid_state(&b("IN_PROCESS", None, None)), None);
    }

    #[test]
    fn a_group_attachable_states() {
        assert!(is_attachable_state("INSPECTION"));
        assert!(is_attachable_state("READY_TO_SHIP"));
        assert!(!is_attachable_state("PENDING"));
    }

    #[test]
    fn b_group_inspectable_includes_idle_in_process() {
        assert!(is_inspectable_state(&b("PENDING", None, None)));
        assert!(is_inspectable_state(&b("PROGRAMMING", None, None)));
        assert!(is_inspectable_state(&b("REPAIRING", None, None)));
        assert!(is_inspectable_state(&b("IN_PROCESS", None, None)));
        // 货架持有的 IN_PROCESS 也可送检（回归：多态 holder 误判）
        assert!(is_inspectable_state(&b(
            "IN_PROCESS",
            Some(7),
            Some("PRODUCTION_SHELF")
        )));
        // 仅工人持有（location='WORKER'）不可
        assert!(!is_inspectable_state(&b(
            "IN_PROCESS",
            Some(7),
            Some("WORKER")
        )));
    }
}

#[cfg(test)]
mod c_group_distribution_tests {
    use super::super::classify::{classify_invalid_state, has_fully_invalid_target};
    use crate::modules::part::batch::model::TPartBatch;

    /// 紧凑 mock：仅暴露本测试关注的字段，其余用 None / 0 / false 占位。
    fn b(id: i64, part_id: i64, status: &str, location: Option<&str>) -> TPartBatch {
        TPartBatch {
            id,
            part_id,
            batch_no: 1,
            quantity: 1,
            status: status.to_string(),
            location: location.map(str::to_string),
            current_holder_id: None,
            current_process_step_id: None,
            delivery_note_id: None,
            parent_batch_id: None,
            version: 0,
            created_at: chrono::DateTime::from_timestamp(0, 0).unwrap().naive_utc(),
            created_by: None,
            updated_at: chrono::DateTime::from_timestamp(0, 0).unwrap().naive_utc(),
            updated_by: None,
            deleted_at: None,
        }
    }

    #[test]
    fn filter_invalid_state_keeps_attachable_and_inspectable() {
        // 1 个 part：READY_TO_SHIP（A 组）+ PENDING（B 组）+ IN_PROCESS@WORKER（C 组）
        // 过滤 C 组后剩 2 个（A/B）。
        let all = vec![
            b(1, 100, "READY_TO_SHIP", None),
            b(2, 100, "PENDING", None),
            b(3, 100, "IN_PROCESS", Some("WORKER")),
        ];
        let kept: Vec<TPartBatch> = all
            .into_iter()
            .filter(|x| classify_invalid_state(x).is_none())
            .collect();
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].id, 1);
        assert_eq!(kept[1].id, 2);
    }

    #[test]
    fn fully_invalid_target_detection_assembly_case() {
        // 装配件：2 个 targets
        //   A (part 100): 4 个 batch，2B + 2C → 部分 C 不是全 C → 不触发
        //   B (part 200): 3 个 batch，全 C → 全 C → 触发
        let all = vec![
            b(1, 100, "PENDING", None),
            b(2, 100, "PENDING", None),
            b(3, 100, "DELIVERED", None),
            b(4, 100, "CANCELLED", None),
            b(5, 200, "DELIVERED", None),
            b(6, 200, "OUTSOURCE", None),
            b(7, 200, "COMPLETED", None),
        ];
        assert!(has_fully_invalid_target(&all));
    }

    #[test]
    fn fully_invalid_target_detection_standalone_case() {
        // 散件：1 个 target（part 100），3 个 batch 全 C → 触发。
        let all = vec![
            b(1, 100, "DELIVERED", None),
            b(2, 100, "CANCELLED", None),
            b(3, 100, "IN_PROCESS", Some("WORKER")),
        ];
        assert!(has_fully_invalid_target(&all));
    }

    #[test]
    fn partial_invalid_not_trigger_21421() {
        // 1 个 target（part 100），4 个 batch，B+B+C+C → 部分 C 不是全 C → 不触发。
        let all = vec![
            b(1, 100, "PENDING", None),
            b(2, 100, "PENDING", None),
            b(3, 100, "DELIVERED", None),
            b(4, 100, "CANCELLED", None),
        ];
        assert!(!has_fully_invalid_target(&all));
    }

    #[test]
    fn no_batches_does_not_trigger_21421() {
        // 未生产 → 不应触发 21421（设计：避免空数据误报硬错误）。
        let all: Vec<TPartBatch> = Vec::new();
        assert!(!has_fully_invalid_target(&all));
    }
}

#[cfg(test)]
mod attachable_batches_tests {
    use super::super::classify::{build_unresolved_target, classify_invalid_state, classify_outcome, TargetEvaluation};
    use super::super::helpers::{to_attachable_batch_dto, to_available_batch_dto};
    use crate::modules::delivery_note::vo::{
        AttachableBatchDto, AvailableBatchDto, BatchStatusDto, ScanOutcomeDto, UnresolvedTargetDto,
    };
    use crate::modules::part::model::TPart;
    use crate::modules::part::batch::model::TPartBatch;

    /// 紧凑 mock：仅暴露本测试关注的字段，其余用 None / 0 / false 占位。
    fn b(id: i64, part_id: i64, status: &str, location: Option<&str>, version: i32) -> TPartBatch {
        TPartBatch {
            id,
            part_id,
            batch_no: 1,
            quantity: 10,
            status: status.to_string(),
            location: location.map(str::to_string),
            current_holder_id: None,
            current_process_step_id: None,
            delivery_note_id: None,
            parent_batch_id: None,
            version,
            created_at: chrono::DateTime::from_timestamp(0, 0).unwrap().naive_utc(),
            created_by: None,
            updated_at: chrono::DateTime::from_timestamp(0, 0).unwrap().naive_utc(),
            updated_by: None,
            deleted_at: None,
        }
    }

    fn part(id: i64, serial: &str) -> TPart {
        TPart {
            id,
            serial_no: Some(serial.to_string()),
            name: format!("Part {id}"),
            drawing_no: format!("D-{id:03}"),
            applicant_name: String::new(),
            quantity: 1,
            request_date: chrono::NaiveDate::from_ymd_opt(2026, 8, 22).unwrap(),
            planned_delivery_date: chrono::NaiveDate::from_ymd_opt(2026, 8, 22).unwrap(),
            customer_id: 1,
            assembly_id: None,
            status: "INSPECTION".to_string(),
            is_urgent: false,
            next_process_id: None,
            order_no: None,
            system_delivery_date: None,
            note: None,
            version: 0,
            created_at: chrono::NaiveDate::from_ymd_opt(2026, 8, 22)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap(),
            created_by: None,
            updated_at: chrono::NaiveDate::from_ymd_opt(2026, 8, 22)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap(),
            updated_by: None,
            deleted_at: None,
            process_chain_id: None,
        }
    }

    /// 单 target 的 TargetEvaluation 构造助手。
    fn eval_for(
        part: TPart,
        attachable: Vec<TPartBatch>,
        inspectable: Vec<TPartBatch>,
        conflict: Vec<TPartBatch>,
    ) -> TargetEvaluation {
        TargetEvaluation {
            part,
            attachable,
            inspectable,
            conflict,
            had_invalid: false,
        }
    }

    /// 单 target 的 TargetEvaluation 构造助手（含 had_invalid 标记）。
    /// 用于测试 C 组过滤后强制走弹窗路径的 outcome 短路。
    fn eval_for_with_invalid(
        part: TPart,
        attachable: Vec<TPartBatch>,
        inspectable: Vec<TPartBatch>,
        conflict: Vec<TPartBatch>,
        had_invalid: bool,
    ) -> TargetEvaluation {
        TargetEvaluation {
            part,
            attachable,
            inspectable,
            conflict,
            had_invalid,
        }
    }

    #[test]
    fn build_unresolved_target_converts_attachable_to_dto() {
        // 直测 build_unresolved_target 的字段映射：
        // - part 元数据透传
        // - available_batches 来源于 inspectable
        // - attachable_batches 来源于 attachable
        let p = part(100, "SN100");
        let attachable = vec![
            b(1, 100, "INSPECTION", None, 5),
            b(2, 100, "READY_TO_SHIP", None, 7),
        ];
        let inspectable = vec![
            b(3, 100, "PENDING", None, 0),
            b(4, 100, "IN_PROCESS", None, 1),
        ];
        let eval = eval_for(p, attachable, inspectable, Vec::new());
        let out: UnresolvedTargetDto = build_unresolved_target(eval);

        assert_eq!(out.part_id, 100);
        assert_eq!(out.serial_no, "SN100");
        assert_eq!(out.drawing_no, "D-100");
        assert_eq!(out.name, "Part 100");

        // B 组：2 个 inspectable → 2 个 AvailableBatchDto
        assert_eq!(out.available_batches.len(), 2);
        let avail_ids: Vec<i64> = out.available_batches.iter().map(|x| x.batch_id).collect();
        assert_eq!(avail_ids, vec![3, 4]);
        // 状态正确：PENDING → Pending；IN_PROCESS 无 location（不是 WORKER）→ Inspect 状态；
        // 此处 from_db 校验 PENDING/IN_PROCESS 都能映射成对应 DTO
        assert!(matches!(
            out.available_batches[0].status,
            BatchStatusDto::Pending
        ));

        // A 组：2 个 attachable → 2 个 AttachableBatchDto
        assert_eq!(out.attachable_batches.len(), 2);
        let attach_ids: Vec<i64> = out.attachable_batches.iter().map(|x| x.batch_id).collect();
        assert_eq!(attach_ids, vec![1, 2]);
        // version 透传（用于前端 add-parts 转发）
        assert_eq!(out.attachable_batches[0].version, 5);
        assert_eq!(out.attachable_batches[1].version, 7);
        // quantity 透传
        assert_eq!(out.attachable_batches[0].quantity, 10);
        // status 透传
        assert!(matches!(
            out.attachable_batches[0].status,
            BatchStatusDto::Inspection
        ));
        assert!(matches!(
            out.attachable_batches[1].status,
            BatchStatusDto::ReadyToShip
        ));
    }

    #[test]
    fn attachable_batches_populated_when_outcome_partial_added() {
        // 装配件混合：sub-part 1 = [A,A]（attachable=2）、sub-part 2 = [B,B]（inspectable=2）
        // → PartialAdded → unresolved_targets 包含 2 个元素：
        //   sub-part 1 的 attachable_batches 非空，available_batches 空
        //   sub-part 2 的 available_batches 非空，attachable_batches 空
        let p1 = part(100, "SN100");
        let p2 = part(200, "SN200");
        let attachable_p1 = vec![
            b(10, 100, "INSPECTION", None, 1),
            b(11, 100, "READY_TO_SHIP", None, 2),
        ];
        let inspectable_p2 = vec![
            b(20, 200, "PENDING", None, 3),
            b(21, 200, "IN_PROCESS", None, 4),
        ];
        let evals = vec![
            eval_for(p1, attachable_p1, Vec::new(), Vec::new()),
            eval_for(p2, Vec::new(), inspectable_p2, Vec::new()),
        ];

        // Step 5 outcome 判定
        let is_assembly = true;
        let any_inspectable = evals.iter().any(|e| !e.inspectable.is_empty());
        let all_attachable_empty = evals.iter().all(|e| e.attachable.is_empty());
        let any_had_invalid_filtered = evals.iter().any(|e| e.had_invalid);
        let outcome = classify_outcome(
            is_assembly,
            any_inspectable,
            all_attachable_empty,
            any_had_invalid_filtered,
        );
        assert_eq!(outcome, ScanOutcomeDto::PartialAdded);

        // Step 7 unresolved_targets 构造（与生产代码 PartialAdded filter 一致：
        // A 或 B 任一非空的子件都保留，让前端能看到 A 组的 attachable_batches）
        let unresolved: Vec<UnresolvedTargetDto> = evals
            .into_iter()
            .filter(|e| !e.inspectable.is_empty() || !e.attachable.is_empty())
            .map(build_unresolved_target)
            .collect();
        assert_eq!(unresolved.len(), 2);

        // sub-part 100：有 attachable、无 inspectable → 进列表但 attachable_batches 含 2 个 A
        let u0 = &unresolved[0];
        assert_eq!(u0.part_id, 100);
        assert_eq!(u0.attachable_batches.len(), 2);
        assert_eq!(u0.available_batches.len(), 0);
        let a_ids: Vec<i64> = u0.attachable_batches.iter().map(|x| x.batch_id).collect();
        assert_eq!(a_ids, vec![10, 11]);

        // sub-part 200：无 attachable、有 inspectable → 进列表但 available_batches 含 2 个 B
        let u1 = &unresolved[1];
        assert_eq!(u1.part_id, 200);
        assert_eq!(u1.attachable_batches.len(), 0);
        assert_eq!(u1.available_batches.len(), 2);
        let b_ids: Vec<i64> = u1.available_batches.iter().map(|x| x.batch_id).collect();
        assert_eq!(b_ids, vec![20, 21]);
    }

    #[test]
    fn attachable_batches_empty_when_no_attachable() {
        // 散件场景：全 B（inspectable=2，attachable=0）→ CandidatesAvailable →
        // unresolved_targets 单元素，且 attachable_batches 必须为空 Vec
        // （不漏字段、不为 None）。
        let p = part(100, "SN100");
        let inspectable = vec![
            b(1, 100, "PENDING", None, 0),
            b(2, 100, "IN_PROCESS", None, 0),
        ];
        let evals = vec![eval_for(p, Vec::new(), inspectable, Vec::new())];

        let outcome = classify_outcome(false, true, true, false);
        assert_eq!(outcome, ScanOutcomeDto::CandidatesAvailable);

        let unresolved: Vec<UnresolvedTargetDto> = evals
            .into_iter()
            .next()
            .map(|e| vec![build_unresolved_target(e)])
            .unwrap();
        assert_eq!(unresolved.len(), 1);
        assert_eq!(unresolved[0].attachable_batches.len(), 0);
        assert_eq!(unresolved[0].available_batches.len(), 2);
        // 字段存在且为空 Vec（类型断言，编译期保证 Vec<AttachableBatchDto> 不是 Option）
        let _: &Vec<AttachableBatchDto> = &unresolved[0].attachable_batches;
        let _: &Vec<AvailableBatchDto> = &unresolved[0].available_batches;
    }

    #[test]
    fn attachable_batches_filtered_when_c_group_present() {
        // 散件 [A, B, C@WORKER]：
        //   - A 组 (INSPECTION) → 进 attachable
        //   - B 组 (PENDING) → 进 inspectable
        //   - C 组 (IN_PROCESS@WORKER) → 被 C 组短路过滤，不进任何 Vec
        // → CandidatesAvailable，attachable_batches 含 1 个 A，
        // available_batches 含 1 个 B。
        let all: Vec<TPartBatch> = vec![
            b(1, 100, "INSPECTION", None, 0),
            b(2, 100, "PENDING", None, 0),
            b(3, 100, "IN_PROCESS", Some("WORKER"), 0),
        ];

        // C 组过滤（与 scan_add Step 4 一致）
        let filtered: Vec<TPartBatch> = all
            .into_iter()
            .filter(|x| classify_invalid_state(x).is_none()) // 直接调，不依赖上面的 mod path
            .collect();
        assert_eq!(filtered.len(), 2);

        // 按 attachable/inspectable 分桶（与 Step 4 一致）
        use super::super::classify::{is_attachable_state, is_inspectable_state};
        let mut attachable = Vec::new();
        let mut inspectable = Vec::new();
        for b in &filtered {
            if is_attachable_state(&b.status) {
                attachable.push(b.clone());
            } else if is_inspectable_state(b) {
                inspectable.push(b.clone());
            }
        }
        assert_eq!(attachable.len(), 1);
        assert_eq!(attachable[0].id, 1);
        assert_eq!(inspectable.len(), 1);
        assert_eq!(inspectable[0].id, 2);

        // outcome：C 组过滤后只剩 B → CandidatesAvailable
        let outcome = classify_outcome(false, !inspectable.is_empty(), attachable.is_empty(), true);
        assert_eq!(outcome, ScanOutcomeDto::CandidatesAvailable);

        // build_unresolved_target：DTO 字段正确分离
        let p = part(100, "SN100");
        let eval = eval_for(p, attachable, inspectable, Vec::new());
        let out = build_unresolved_target(eval);
        assert_eq!(out.attachable_batches.len(), 1);
        assert_eq!(out.attachable_batches[0].batch_id, 1);
        assert_eq!(out.available_batches.len(), 1);
        assert_eq!(out.available_batches[0].batch_id, 2);
        // C 组 batch id=3 不出现在任何 Vec
        for b in &out.attachable_batches {
            assert_ne!(b.batch_id, 3);
        }
        for b in &out.available_batches {
            assert_ne!(b.batch_id, 3);
        }
    }

    #[test]
    fn helper_dto_under_separate_paths() {
        // 测试 helpers::to_available_batch_dto / to_attachable_batch_dto
        // 拆分后独立可用（覆盖子模块入口）。
        let batch = b(99, 1, "INSPECTION", None, 3);
        let avail: AvailableBatchDto = to_available_batch_dto(batch.clone());
        assert_eq!(avail.batch_id, 99);
        assert_eq!(avail.version, 3);
        assert_eq!(avail.quantity, 10);
        let attach: AttachableBatchDto = to_attachable_batch_dto(batch);
        assert_eq!(attach.batch_id, 99);
        assert_eq!(attach.version, 3);
        assert_eq!(attach.quantity, 10);
    }

    // ---- had_invalid 短路 outcome 测试：覆盖 spec 约定的「原始含 C → 强制弹窗」 ----

    /// 散件 [A, C@WORKER] 混合 → outcome 必须是 CandidatesAvailable（即使只剩 A），
    /// A 不自动 attach，进入 attachable_batches 让前端弹窗确认。
    ///
    /// 这是本次 fix 的核心场景：spec 约定 C 被静默过滤后，剩余的合法批次
    /// 也必须走弹窗路径，不能让 A 静默自动 attach。
    #[test]
    fn had_invalid_standalone_a_plus_c_returns_candidates() {
        // 散件：1 个 target，attachable=[A]，inspectable=[]，had_invalid=true
        let p = part(100, "SN100");
        let attachable = vec![b(1, 100, "INSPECTION", None, 0)];
        let eval = eval_for_with_invalid(p, attachable, Vec::new(), Vec::new(), true);

        // outcome：C 被过滤（had_invalid=true）+ 散件 → CandidatesAvailable
        let outcome = classify_outcome(
            false, false, // any_inspectable
            false, // all_attachable_empty
            true,  // any_had_invalid_filtered
        );
        assert_eq!(outcome, ScanOutcomeDto::CandidatesAvailable);

        // 验证响应形态：unresolved_targets 单元素 + attachable_batches 含 A
        let unresolved: Vec<UnresolvedTargetDto> = vec![build_unresolved_target(eval)];
        assert_eq!(unresolved.len(), 1);
        assert_eq!(unresolved[0].attachable_batches.len(), 1);
        assert_eq!(unresolved[0].available_batches.len(), 0);
        assert_eq!(unresolved[0].attachable_batches[0].batch_id, 1);
    }

    /// 装配件 + 某子件 had_invalid=true → PartialAdded（即便该子件只剩 A）。
    ///
    /// 装配件场景下，C 被过滤后该子件的 A 也必须走弹窗（不能被静默 auto-attach），
    /// 让前端决定 attach 哪些子件。
    #[test]
    fn had_invalid_assembly_returns_partial_added() {
        // 装配件 1 个子件：attachable=[A]，had_invalid=true
        let p = part(100, "SN100");
        let attachable = vec![b(1, 100, "INSPECTION", None, 0)];
        let eval = eval_for_with_invalid(p, attachable, Vec::new(), Vec::new(), true);

        let outcome = classify_outcome(
            true,  // is_assembly
            false, // any_inspectable
            false, // all_attachable_empty
            true,  // any_had_invalid_filtered
        );
        assert_eq!(outcome, ScanOutcomeDto::PartialAdded);

        let unresolved: Vec<UnresolvedTargetDto> = vec![build_unresolved_target(eval)];
        assert_eq!(unresolved.len(), 1);
        assert_eq!(unresolved[0].attachable_batches.len(), 1);
        assert_eq!(unresolved[0].available_batches.len(), 0);
    }

    /// 散件 + 全 A（无 invalid + 无 inspectable）→ Added（既有行为不变）。
    ///
    /// 回归测试：had_invalid=false 时新参数完全不影响既有 outcome 分支。
    #[test]
    fn had_invalid_false_full_a_returns_added() {
        let outcome = classify_outcome(
            false, // is_assembly
            false, // any_inspectable
            false, // all_attachable_empty
            false, // any_had_invalid_filtered
        );
        assert_eq!(outcome, ScanOutcomeDto::Added);
    }

    /// 散件 + invalid + B 同时存在 → CandidatesAvailable（与无 invalid 的
    /// 「全 B 走 CandidatesAvailable」行为一致）。
    ///
    /// 验证 invalid 与 inspectable 共存时短路仍生效（CandidatesAvailable）。
    #[test]
    fn had_invalid_with_inspectable_returns_candidates() {
        let outcome = classify_outcome(
            false, // is_assembly
            true,  // any_inspectable
            false, // all_attachable_empty
            true,  // any_had_invalid_filtered
        );
        assert_eq!(outcome, ScanOutcomeDto::CandidatesAvailable);
    }

    /// 散件 + 仅 B（无 invalid）→ CandidatesAvailable（既有行为不变）。
    ///
    /// 回归测试：had_invalid=false + any_inspectable=true → CandidatesAvailable。
    #[test]
    fn no_invalid_with_inspectable_returns_candidates() {
        let outcome = classify_outcome(
            false, // is_assembly
            true,  // any_inspectable
            false, // all_attachable_empty
            false, // any_had_invalid_filtered
        );
        assert_eq!(outcome, ScanOutcomeDto::CandidatesAvailable);
    }

    /// 装配件 + 仅 B（无 invalid）→ PartialAdded（既有行为不变）。
    ///
    /// 回归测试：had_invalid=false + is_assembly=true + any_inspectable=true → PartialAdded。
    #[test]
    fn no_invalid_assembly_with_inspectable_returns_partial_added() {
        let outcome = classify_outcome(
            true,  // is_assembly
            true,  // any_inspectable
            true,  // all_attachable_empty
            false, // any_had_invalid_filtered
        );
        assert_eq!(outcome, ScanOutcomeDto::PartialAdded);
    }
}