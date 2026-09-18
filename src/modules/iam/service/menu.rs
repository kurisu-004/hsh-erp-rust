//! iam 域菜单树组装（纯函数）
//!
//! 对应 Python myERP/service/menu.py::_to_tree。
//!
//! 2026-09-19 IAM 域合并：从 `modules::user::service::build_menu_tree` 迁移过来，
//! 单独成文件（rust 惯例：conventions §4.2 纯函数 inline 测试）。
//!
//! 2026-09-19 review 落地：`build_menu_tree` 移到本文件后，inline `#[cfg(test)] mod tests`
//! 保留 ~10 行原算法的环检测兜底测试（plan §4.2 列在原 service.rs，本任务一并迁过来；
//! 树组装的细节分支测试在 `tests/user_repo.rs` 与 service_tests 的 menus_for_roles 用例覆盖）。

use std::collections::{HashMap, HashSet};

use crate::modules::iam::dto::MenuNodeOut;
use crate::modules::iam::model::Menu;

/// 把拍平的菜单行组装成树。
///
/// 规则（与 Python `_to_tree` 逐条对齐）：
/// - 根与每层 children 均按 `(sort_order, code)` 升序
/// - `parent_id` 为 NULL，或指向**不在可见集合内**的父节点（父节点被停用/软删/
///   不属于当前角色）→ 该节点提升为根（孤儿兜底）
/// - 不做深循环检测：DB CHECK `ck_t_menu_no_self_loop` 已挡单行自环，seed 数据可信。
///   但本实现用「从 map 取走节点」的方式递归，即便出现环也只会丢弃成环节点，
///   不会无限递归（比 Python 多一层安全兜底）。
pub fn build_menu_tree(menus: Vec<Menu>) -> Vec<MenuNodeOut> {
    let visible: HashSet<i64> = menus.iter().map(|m| m.id).collect();
    let sort_keys: HashMap<i64, (i32, String)> = menus
        .iter()
        .map(|m| (m.id, (m.sort_order, m.code.clone())))
        .collect();

    let mut nodes: HashMap<i64, MenuNodeOut> = HashMap::with_capacity(menus.len());
    let mut children_of: HashMap<i64, Vec<i64>> = HashMap::new();
    let mut root_ids: Vec<i64> = Vec::new();

    for m in menus {
        let id = m.id;
        let parent_id = m.parent_id;
        nodes.insert(
            id,
            MenuNodeOut {
                id,
                version: m.version,
                parent_id: parent_id.map(|p| p.to_string()),
                code: m.code,
                title: m.title,
                path: m.path,
                icon: m.icon,
                sort_order: m.sort_order,
                children: Vec::new(),
            },
        );
        match parent_id {
            // 父节点可见且非自环 → 挂为子节点
            Some(p) if p != id && visible.contains(&p) => {
                children_of.entry(p).or_default().push(id);
            }
            // parent_id 为 NULL，或父节点不可见 → 升为根（孤儿兜底）
            _ => root_ids.push(id),
        }
    }

    sort_ids(&mut root_ids, &sort_keys);
    for kids in children_of.values_mut() {
        sort_ids(kids, &sort_keys);
    }

    root_ids
        .into_iter()
        .filter_map(|id| assemble_node(id, &mut nodes, &children_of))
        .collect()
}

fn sort_ids(ids: &mut [i64], sort_keys: &HashMap<i64, (i32, String)>) {
    ids.sort_by(|a, b| sort_keys.get(a).cmp(&sort_keys.get(b)));
}

/// 递归组装：从 `nodes` 中「取走」节点，天然防止环导致的无限递归。
fn assemble_node(
    id: i64,
    nodes: &mut HashMap<i64, MenuNodeOut>,
    children_of: &HashMap<i64, Vec<i64>>,
) -> Option<MenuNodeOut> {
    let mut node = nodes.remove(&id)?;
    if let Some(kids) = children_of.get(&id) {
        node.children = kids
            .iter()
            .filter_map(|k| assemble_node(*k, nodes, children_of))
            .collect();
    }
    Some(node)
}

// ---------------------------------------------------------------------------
// 纯函数单测（conventions §4.2：纯函数 inline 测试，100% 行覆盖）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDateTime;

    fn menu(id: i64, parent_id: Option<i64>, code: &str, sort: i32) -> Menu {
        let now =
            NaiveDateTime::parse_from_str("2026-01-01T00:00:00", "%Y-%m-%dT%H:%M:%S").unwrap();
        Menu {
            id,
            parent_id,
            code: code.into(),
            title: format!("Menu {id}"),
            path: None,
            icon: None,
            sort_order: sort,
            is_active: true,
            version: 0,
            created_at: now,
            created_by: None,
            updated_at: now,
            updated_by: None,
            deleted_at: None,
        }
    }

    #[test]
    fn build_tree_with_three_levels_orders_by_sort_then_code() {
        // ROOT (sort=1) > B (sort=0) > A (sort=0)
        let menus = vec![
            menu(1, None, "ROOT", 1),
            menu(2, Some(1), "A", 0),
            menu(3, Some(1), "B", 0),
            menu(4, Some(2), "A-A", 0),
        ];
        let tree = build_menu_tree(menus);
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].code, "ROOT");
        assert_eq!(tree[0].children.len(), 2);
        // children 按 (sort, code) 排：A 在前（sort=0 相同，code 'A' < 'B'）
        assert_eq!(tree[0].children[0].code, "A");
        assert_eq!(tree[0].children[1].code, "B");
        assert_eq!(tree[0].children[0].children[0].code, "A-A");
    }

    #[test]
    fn orphan_node_with_missing_parent_promotes_to_root() {
        // child 指向 999（不在集合内）→ 升为根
        let menus = vec![menu(1, Some(999), "orphan", 0), menu(2, None, "root", 0)];
        let tree = build_menu_tree(menus);
        assert_eq!(tree.len(), 2);
        let codes: Vec<&str> = tree.iter().map(|n| n.code.as_str()).collect();
        assert!(codes.contains(&"orphan"));
        assert!(codes.contains(&"root"));
    }

    #[test]
    fn empty_input_returns_empty_tree() {
        let tree = build_menu_tree(vec![]);
        assert!(tree.is_empty());
    }

    #[test]
    fn self_referencing_node_promotes_to_root() {
        // 自环（DB CHECK 应已挡，但兜底算法也要安全）
        let menus = vec![menu(1, Some(1), "loop", 0)];
        let tree = build_menu_tree(menus);
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].code, "loop");
        assert!(tree[0].children.is_empty());
    }
}
