//! part 状态机占位
//!
// 对应 Python myERP/statemachines/part_state_machine.py。
// 实施约定：手写 enum + match 迁移表（`can_transition_to`），状态机不写 DB；
// 事件日志由 service 在事务内统一插入。
