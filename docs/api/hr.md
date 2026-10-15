# hr::schedule 域 API（人事排班日历）

> 域：`hr`（源码 `src/modules/hr/`，即人事排班域的全局日历 + 班次 + 班次日历覆盖）。本文件是该域的**唯一**契约来源，任何字段 / 端点变更必须同步本文件。
> 路由前缀 `/api/v2/hr`（`handler::router()`，注册链：`modules/mod.rs` 的 `/api/v2` nest → `.nest("/hr", hr::router())`）。
> 权限点两个：`hr.view`（只读 3 条）/ `hr.schedule.manage`（写 7 条），守卫判在 **service 每个用例的第一行**（`current.require_permission(..)`），handler 不重复校验。
> ⚠️ 本文件**不依赖任何行号**（别的域的注释曾引用本文的行号，随改动全部失效）；引用本文时只引小节号。

## 0. 一句话

一个班次的「自己的工作日历」= **全局日历 ∪ 本班次的单日覆盖行**，没有独立的日历实体、没有 `calendar_id` 列；读端点把**三级推导**（覆盖 > 全局 > 按星期兜底）算好回传，前端只渲染、不重复实现。

## 1. 端点表

| # | 方法 | 路径 | 权限（service 层守卫） | 入参 | 响应 |
|---|---|---|---|---|---|
| 1 | GET | `/api/v2/hr/calendar/days?from&to` | `hr.view` | `CalendarRangeQuery`（§3.1） | `CalendarDaysOut` |
| 2 | POST | `/api/v2/hr/calendar/days` | `hr.schedule.manage` | `CalendarDaysUpsertRequest` | `CalendarDaysWriteOut` |
| 3 | POST | `/api/v2/hr/calendar/days/delete` | `hr.schedule.manage` | `CalendarDaysDeleteRequest` | `CalendarDaysWriteOut` |
| 4 | GET | `/api/v2/hr/shifts?include_inactive` | `hr.view` | `ShiftListQuery` | `ShiftListOut` |
| 5 | POST | `/api/v2/hr/shifts` | `hr.schedule.manage` | `CreateShiftRequest` | `ShiftOut` |
| 6 | POST | `/api/v2/hr/shifts/{id}` | `hr.schedule.manage` | `UpdateShiftRequest` | `ShiftOut` |
| 7 | POST | `/api/v2/hr/shifts/{id}/deactivate` | `hr.schedule.manage` | `DeactivateShiftRequest` | `ShiftOut` |
| 8 | GET | `/api/v2/hr/shifts/{id}/calendar?from&to` | `hr.view` | `ShiftCalendarRangeQuery` | `ShiftCalendarOut` |
| 9 | POST | `/api/v2/hr/shifts/{id}/calendar/days` | `hr.schedule.manage` | `ShiftDaysUpsertRequest` | `ShiftDaysWriteOut` |
| 10 | POST | `/api/v2/hr/shifts/{id}/calendar/days/delete` | `hr.schedule.manage` | `ShiftDaysDeleteRequest` | `ShiftDaysWriteOut` |

- 全部走统一信封 `R { code, message, data }`。
- **零 PUT / PATCH / DELETE**（与 `wecom` / `serial` / `print_template` 同款）：改走 `POST /shifts/{id}`、停用走 `POST /shifts/{id}/deactivate`、删走 `POST …/delete`（动词是 POST，`/delete` 在**路径**里）。
- 写端点（2/3/5/6/7/9/10）的事务边界在 handler：`pool.begin()` → service → `tx.commit()`，**整批全成功或全回滚**；读端点（1/4/8）`pool.acquire()` 不开事务。
- `id` 一律序列化成 **JSON 字符串**（雪花 ID > 2^53，`JS Number` 会丢精度）。
- 权威路由清单是 `handler::ROUTES`（10 条），lib 单测 `handler::tests::routes_declared_in_router` 逐条比对 `router()` 源码。
- 本域**零 WS 广播**（见 §8.3）。

## 2. 领域语义

### 2.1 三张表各自的职责

| 表 | 职责 | 唯一的软删/停用口径 |
|---|---|---|
| `t_hr_calendar_day` | **全局**日历的**显式行**，**只存例外**。常态工作日 / 常态周末不落行 | 删显式行（端点 3）⇒ 该日**回退按星期推导** |
| `t_hr_shift` | 班次（`code` / `name` / `color` / 时段 jsonb / `sort_order`） | 停用走 `is_active = false`（端点 7），**不软删**、**不级联**下面那张表 |
| `t_hr_shift_calendar` | 某班次「与全局不同的那天」的覆盖行 | 清覆盖（端点 10）⇒ 该日**回退全局日历**。⚠️ 停用班次**不级联软删**本表（历史考勤还要复现当年那天的口径） |

三张表**无物理外键**（`t_hr_shift_calendar.shift_id` 是裸 `bigint`，存在性由 service 校验 → `21901`）；日型是 `varchar(24)`、**无 DB ENUM**（合法值只在本仓 Rust 枚举里，未知值 → `21905`）。

### 2.2 日型枚举 6 档 + `is_workday` 判据表

| wire / DB 字面量 | 中文标签 | `is_workday` | 典型来源 |
|---|:--:|:--:|---|
| `WORKDAY` | 工作日 | ✅ `true` | 星期兜底 / 显式行 |
| `MAKEUP_WORKDAY` | 调休上班 | ✅ `true` | 显式行（法定节假日调成上班日） |
| `REST_DAY` | 正常周末 | ❌ `false` | 星期兜底 / 显式行 |
| `STATUTORY_HOLIDAY` | 法定节假日 | ❌ `false` | 显式行 |
| `MAKEUP_HOLIDAY` | 调休放假 | ❌ `false` | 显式行（周末调成放假） |
| `CUSTOM_LEAVE` | 自定义休息日 | ❌ `false` | 显式行，**写侧必须带 `name`**（缺失 → `21907`） |

- 出参里的日型**逐字**就是上表的字面量（`REST_DAY`，**不是**变体名 `RestDay`）。它是手写 `Serialize` 委托给唯一的 `as_str()`，刻意不用 `#[derive(Serialize)]` 的机械转写 —— 后者要让 wire 字面量有两份定义，加新档时只改一处会让前端看到的日型**静默变形**。
- 解析**不做 trim、不做大写折叠**：唯一的合法输入就是上表那 6 个字面量。折叠会把「本该在写侧就拒的错值」混进来（库里出现 `workday` 这种行时，折叠会把它静默当成合法）。

### 2.3 推导规则表（覆盖 > 全局 > 星期兜底）

```
effective_day_type(shift, date) =
    班次覆盖行的 day_type                          若存在
  | 全局日历行的 day_type                          若存在
  | 按星期推导（周六周日 = REST_DAY，否则 WORKDAY）  兜底

is_workday = effective_day_type ∈ { WORKDAY, MAKEUP_WORKDAY }
```

| 情形 | `global_day_type` | `effective_day_type` | `is_override` | 端点 1 的 `derived` |
|---|---|---|:--:|:--:|
| 有覆盖行 | 全局那一档（无显式行时已按星期推导过） | **覆盖行** | `true` | 与覆盖行是否有关（端点 1 只看全局） |
| 无覆盖行、有全局显式行 | 全局显式行 | = `global_day_type` | `false` | `false` |
| 两级都没有（常态日） | 按星期推导 | = `global_day_type` | `false` | **`true`** |

- ⚠️ **`is_override` 表达的是「有没有覆盖行」，不是「值是否与全局相同」**：显式写一条与全局同值的覆盖行是**允许**的（用来锁住「将来有人改全局日历不影响这个班次」）。判据只能是覆盖行的**存在性**。
- ⚠️ **`derived` 只在端点 1（全局日历）上有**，端点 8 用 `global_day_type` 与 `effective_day_type` 两根轴表达同一件事（见 §4.3）。前端据此区分「这天的类型是有人在管」与「这天的类型是系统兜底」—— 后者在排班表上**不应该**显示成一条「被配置过」的标记。
- 推导**唯一真源在后端**，service 一次算好回传；前端重复实现就等于有两套口径。

### 2.4 班次时段：绝对时钟轴 + V1–V6 校验规则表 + 为什么跨度卡 24:00

段值是自 `cal_date 00:00` 起算的**绝对偏移**，字符串 `"HH:MM"`，`HH ∈ 0..48`。夜班 22:00 → 次日 02:00 写作 `start="22:00"` / `end="26:00"`。**「跨天班次的日期归属 = 工作开始时间所在日」由数据结构本身保证**，不需要任何「跨天标志位 + 推断」逻辑。

| # | 规则 | 违规码 |
|---|---|---|
| V1 | `0 ≤ start`，`end ≤ "48:00"`（`hh > 48` 拒；`"48:30"` 因总分钟 `> 2880` 也拒） | `21903` |
| V2 | `start < end`（**严格**，不承认零长段） | `21903` |
| V3 | `segments[0].start < "24:00"`（首段必须落在当日） | `21903` |
| V4 | 按 `start` **严格升序**（乱序是错，不是「顺手排一下」）且不重叠：`start_{i+1} >= end_i` | `21903` |
| V5 | `1 ≤ n ≤ 6`（上限 6 是**排班表可读性**的约束，不是数据库约束） | `21903` |
| **V6** | **`span = max(end) − min(start) ≤ "24:00"`**（两端闭） | **`21908`** |

- ⚠️ **`21903`（V1–V5 段形态问题）与 `21908`（V6 跨度超一天）是两个码**：前端要能只对「跨度超一天」给一条专门文案（它同时是「这个班次排法有问题」而不是「参数格式错了」）。
- **V3 为什么必需**：它是「日期归属」的锚。首段起点就在 `24:00` 之后时，这一班的**开始时间**落在次日 ⇒ `cal_date` 不再是它的归属日，出参里的 `cal_date` / `is_override` 与两个日历写端点全部失去意义。
- ⚠️ **V6 为什么是本设计最关键的一刀**：**跨日重叠 ⟺ `span > 24h`**。卡死 V6 后，相邻两日 / 隔日的班次实例在时间轴上**结构性地不可能重叠** ⇒ 两个日历写端点**零重叠检查**（它们只需要「同一班次同一天只有一条覆盖行」这条同日语义，那由 `uk_t_hr_shift_calendar` 的 partial unique 守着）。span ≤ 24h 同时也是「一个班次实例最长 24 小时」的劳动法底线。
- 放宽 V6 的代价不是「多一种排班形态」，而是日历写端点必须新增一条跨 `days × shifts` 的区间求交检查（一次请求 O(days × shifts)），且与并发插入之间存在 TOCTOU 窗口。**本设计有意拿「排班形态的表达力」换「日历写端点的可证明正确」。**
- ⚠️ **V6 排除了 `span > 24h` 的班次形态**（见 §8.3 取舍登记）。仍在合法域内的是「夜班 + 次日早班」两段（`22:00→26:00` + `28:00→34:00`，span 12:00）与「24 小时连做」（`08:00→32:00`，span 恰好 24:00）。
- 入参的 `seq` **不参与校验**，写入前由 service 按 `start` 升序重排、重编为 `1..n` 并把段值补零成 `"HH:MM"` —— 这样「改了顺序 / 少写个 0」不会让整列 jsonb 的字节形态无谓漂移。校验是表驱动的（lib 单测 `service::tests::invalid_segments_rejected_with_expected_code`），集成测试只钉「码真的进了 `R` 信封」这一层。

## 3. 入参逐字段

### 3.1 区间 query（端点 1 / 8 共用）

| 字段 | 类型 | 语义 |
|---|---|---|
| `from` | date | 必填，ISO 8601（如 `2026-10-01`） |
| `to` | date | 必填，两端闭 |

⚠️ 区间约束见 §5。⚠️ **端点 8 除了区间还判班次存在性**（不存在 / 已软删 → `21901`，HTTP 404）—— 读端点也要判，否则会对着一个不存在的班次返回一整段「全部走推导」的空日历，看起来像「这个班次不排班」。**停用**（`is_active = false`）的班次照常可读，停用只挡写。

### 3.2 `ShiftListQuery`（端点 4）

| 字段 | 类型 | 语义 |
|---|---|---|
| `include_inactive` | bool | **缺省 `false`**（只列启用中的）。停用是「下架」，默认混进列表会让排班表出现一排编了但没人排的班次；管理页需要看到停用项才能「重新启用」时才传 `true` |

### 3.3 `CalendarDaysUpsertRequest`（端点 2）

| 字段 | 类型 | 语义 |
|---|---|---|
| `items` | item[] | **必填**（缺 → axum `Json` 提取器的 422 纯文本）。**空数组也落 `21904`** —— 「没说要写什么」不是合法请求，见 §5 |

`CalendarDayUpsertItem`：`cal_date`（date，必填）/ `day_type`（string，必填，6 档之一）/ `name`（string?，`CUSTOM_LEAVE` **必填**，缺失或全空白 → `21907`）/ `remark`（string?，不进出参，仅管理端留痕）。

### 3.4 `CalendarDaysDeleteRequest` / `ShiftDaysDeleteRequest`（端点 3 / 10）

| 字段 | 类型 | 语义 |
|---|---|---|
| `dates` | date[]? | 形态 A |
| `from` / `to` | date? | 形态 B |

⚠️ **必须恰好命中一种形态**：都省、只给一半、都给、`dates` 为空数组 —— 四种非法形态一律 `21904`（而不是「删 0 行」的假成功）。判据写成一个 catch-all 而非逐个列举：`Option<Vec<_>>` 的组合有 3×2×2 = 12 种，逐个列必然漏一两个。

### 3.5 `CreateShiftRequest`（端点 5）

| 字段 | 类型 | 语义 |
|---|---|---|
| `code` | string | 必填，`varchar(30)`，未软删行内唯一；重复 → `21902`（HTTP 409，**不是** `40901` —— 后者只表达 OCC）。落库前 `trim()` |
| `name` | string | 必填，`varchar(50)`，落库前 `trim()` |
| `segments` | segment[] | **必填且无 `#[serde(default)]`**（缺 → axum `Json` 提取器返 **HTTP 422 纯文本**）——「没有时段的班次」没有意义。形态见 §2.4 |
| `color` | string? | `varchar(20)`，前端色板值；可 NULL |
| `sort_order` | int? | 缺省 `0` |
| `remark` | string? | `varchar(200)` |

⚠️ 建班次的 `version = 0`、`is_active = true` —— **入参里没有** `version` / `is_active`：新建永远从启用态起（建了就是能用的），下架是独立的端点 7。

### 3.6 `UpdateShiftRequest`（端点 6）—— 部分覆盖

| 字段 | 类型 | 语义 |
|---|---|---|
| `version` | int | **必填 OCC 锚**（CLAUDE.md §8）。陈旧 → `40901`；**缺失 → axum 的 422 纯文本**（不是业务信封） |
| `code` / `name` | string? | 给了才改（`name` 会 `trim()`） |
| `segments` | segment[]? | 给了就**整列替换**（不做段级 merge），且重跑一遍 V1–V6；不传则沿用行上那份 |
| `color` / `remark` | string? | ⚠️ 传 `null` 等于「**这次不改**」—— 本端点**没有**「显式清空某个可空列」的语义（要清空就传空串） |
| `sort_order` | int? | 给了才改 |

⚠️ 本 DTO **没有** `is_active`：启用 / 停用是两个意图，停用有独立端点。

### 3.7 `DeactivateShiftRequest`（端点 7）

| 字段 | 类型 | 语义 |
|---|---|---|
| `version` | int | 必填 OCC 锚（同端点 6） |

### 3.8 `ShiftDaysUpsertRequest` / `ShiftDayUpsertItem`（端点 9）

| 字段 | 类型 | 语义 |
|---|---|---|
| `items` | item[] | 必填；空数组 → `21904`（同端点 2 的口径） |
| `items[].cal_date` | date | 必填 |
| `items[].day_type` | string | 必填，6 档之一 |
| `items[].remark` | string? | 要写说明走这里 |

⚠️ **刻意没有 `name`**（`t_hr_shift_calendar` 表也没这列）：「这天为什么特殊」是**全局日历**的语义，班次覆盖只改「这天算不算上班」。⇒ 端点 9 写 `CUSTOM_LEAVE` **不**要求 `name`、也**不**落 `21907`（那条闸门只属全局日历）。

## 4. 出参逐字段

### 4.1 `CalendarDayOut`（端点 1，区间内**逐日一行，一天都不省**）

| 字段 | 类型 | 口径 |
|---|---|---|
| `cal_date` | date | 按升序 |
| `day_type` | string | 已含按星期推导的兜底（前端**不需要**再自己判周六周日），字面量见 §2.2 |
| `name` | string \| null | 节假日名 / 休息日说明；`derived = true` 的行**恒为 `null`**（库里就没这行） |
| `derived` | bool | `true` = 该日**无显式行**，值是按星期推导出来的 |

### 4.2 `ShiftOut`（端点 4 / 5 / 6 / 7 复用同一形状）

| 字段 | 类型 | 口径 |
|---|---|---|
| `id` | string | 雪花 ID |
| `version` | int | OCC 锚，端点 6 / 7 的 body 必须回传它 |
| `code` / `name` / `color` / `sort_order` / `remark` | — | 与 §3.5 同义 |
| `segments` | segment[] | **后端已按 `start` 升序规范化 + 重编 `seq`**，前端直接渲染即可 |
| `is_active` | bool | `false` = 已停用（**未软删**，行仍在库里） |
| `cross_day` | bool | **派生**：`max(end) > "24:00"` |
| `overflow_minutes` | int | **派生**：`max(end) − "24:00"`（分钟），`0` 表示不跨天（与 `cross_day` 同真同假） |
| `daily_minutes` | int | **派生**：`Σ(end − start)`（分钟，**无折算**） |
| `created_at` / `updated_at` | string | `NaiveDateTime` 原生序列化 |

⚠️ **`overflow_minutes` 用分钟而不是小时**：段值允许 `:30`（`26:30` = 次日 02:30），截断成整小时会把 90 分钟的溢出报成 1 小时。`cross_day` 已经能表达「有没有跨天」这个布尔事实，要小时显示由前端自己四舍五入。
⚠️ **`daily_minutes` 是各段时长之和、不按是否跨天折算**：段已经是绝对时钟轴上的偏移，`26:00 − 22:00` 天然就是 4 小时，再折算一次是**双重记账**。

### 4.3 `ShiftCalendarDayOut`（端点 8）—— 两根独立的轴

| 字段 | 类型 | 口径 |
|---|---|---|
| `cal_date` | date | 按升序，逐日一行 |
| `global_day_type` | string | **全局日历**说的这天的日型（无显式行时已按星期推导过） |
| `effective_day_type` | string | 该班次**真正生效**的日型（覆盖 > 全局 > 星期推导，见 §2.3） |
| `is_workday` | bool | **`effective_day_type` 是否算上班日**（考勤取数的**唯一**判据） |
| `is_override` | bool | 是否来自本班次的覆盖行（判据是**存在性**，见 §2.3） |

外层 `ShiftCalendarOut` 另带 `shift_id`（string）—— 让前端不必为了画个标题再回查一次列表。

⇒ 前端可以既显示「这天全局是节假日」又显示「但这个班次那天照常上班」，**不需要自己实现推导**。

### 4.4 `CalendarDaysWriteOut` / `ShiftDaysWriteOut`（写端点共用）

| 字段 | 类型 | 口径 |
|---|---|---|
| `affected` | int | upsert = `INSERT … ON CONFLICT DO UPDATE` 命中行数；delete = 被软删的行数 |

⚠️ **delete 的 `affected = 0` 是合法成功**（那几天本来就没有显式行 / 覆盖行），不是错误。⚠️ 与之相反，**upsert 的空 `items` 落 `21904`**（见 §5）。

## 5. 口径专章

### 5.1 跨天归属 = 工作开始时间所在日

段值是绝对偏移 ⇒ 归属日就是 `cal_date` 本身。**不需要**任何「跨天标志位 + 推断」：出参里没有 `cross_day` 之外还需要前端自己算的日期字段。

### 5.2 区间 366 天上限

`from <= to` 且 `(to − from) ≤ 366` 天（**两端闭**：恰好 366 放行、367 拒），违反一律 `21904`（HTTP 400）。理由：读端点要**逐日推导并回传**，一个请求灌 10 年就是 3650 个出参项 —— 前端用不上，却已经把 DB 与序列化 CPU 花掉了。

⚠️ **批量 upsert 的 `{items:[…]}` 走同一条判据**：`items` 的最小 / 最大 `cal_date` 构成一个**隐式区间**。不卡它的话，攒一个大 `items` 就能把「一个请求最多 366 天」的承诺从读端点绕过去。

### 5.3 空 `items` 落 `21904`（不是静默 `affected: 0`）

端点 2 与端点 9 的 `items` 为空数组 ⇒ `21904`。理由：「没说要写什么」不是合法请求；返回 `affected: 0` 会让调用方以为「写了但没生效」。这与 §4.4 的「delete 的 `affected = 0` 是合法成功」是**两件不同的事**，别混。

### 5.4 `null` 与缺省的语义

- **入参 `Option<T>` 的 `None` 一律是「这次不改」**（端点 6），**没有**「显式清空某个可空列」的语义 —— 要清空就传空串（`color` / `remark` / `name` 都是 varchar 而非有约束的可空列）。
- **`segments` 是单列覆盖**：传了就整列替换（不做段级 merge），不传就保持原值。
- **出参的可空列是真的可空**：`name` / `color` / `remark` 是 `string | null`，前端要能处理 `null`（不要 `x || ''` 后再判空）。

### 5.5 `version` 缺失是 422 纯文本，不是业务信封

端点 6 / 7 的 `version` 必填且无 `#[serde(default)]`，缺失时请求**到不了 service**，由 axum `Json` 提取器直接拒 ⇒ **HTTP 422 + 纯文本 body**（不是 `R { code, … }` 信封）。前端错误处理要把「解析信封」与「解析纯文本」两条路都留着。端点 5 的 `segments` 缺失同理。

## 6. 错误码

| 码 | 常量 | HTTP | 触发场景 |
|---|---|---|---|
| `21901` | `BIZ_HR_SHIFT_NOT_FOUND` | 404 | 查 / 改 / 停用 / 写覆盖 / 清覆盖一个**不存在或已软删**的班次（含端点 8 的读端点） |
| `21902` | `BIZ_HR_SHIFT_CODE_DUPLICATED` | 409 | 班次 `code` 重复（**不是** `40901`，后者只表达 OCC）。应用层预检与 DB partial unique 的并发兜底**同码** |
| `21903` | `BIZ_HR_SHIFT_SEGMENT_INVALID` | 400 | 段非法（V1–V5 任一）**或**库里那一列 jsonb 解不出来 / 解出来过不了 V1–V6（降级成 21903 而不是 500） |
| `21904` | `BIZ_HR_CALENDAR_RANGE_INVALID` | 400 | `from > to` / 跨度 > 366 天 / upsert 的 `items` 为空 / 删行请求没恰好命中 `{dates}` 或 `{from,to}` 一种形态 |
| `21905` | `BIZ_HR_DAY_TYPE_INVALID` | 400 | 未知 `day_type`（含大小写不符） |
| `21906` | `BIZ_HR_SHIFT_INACTIVE` | 409 | 对**已停用**班次做写操作（改 / 停用 / 写覆盖 / 清覆盖）。重复停用也走这条（幂等拒绝） |
| `21907` | `BIZ_HR_CALENDAR_LEAVE_NAME_REQUIRED` | 400 | `CUSTOM_LEAVE` 未填 `name`（**仅全局日历**，见 §3.8） |
| `21908` | `BIZ_HR_SHIFT_SPAN_EXCEEDS_DAY` | 400 | **V6**：`span > "24:00"` |
| `40901` | `VERSION_CONFLICT` | 409 | 端点 6 / 7 的 `version` 与库内当前值不一致 |
| `40300` | `FORBIDDEN` | 403 | 缺对应权限点（守卫判在 service 第一行 ⇒ **在区间/段/存在性校验之前**） |

⚠️ 权限守卫（`40300`）**先于**一切入参校验：无权账号连候选 SELECT 都跑不到。这是有意的（端点 1/8 要逐日推导并回传整个区间，不该让无权者拿它做信息探测）。代价是无权账号在写端点上会**先开一次事务**才被拒 —— 只是短连接占用，零写入、不落锁。

⚠️ `219xx` 段是全新段位：`216xx` 已于 2026-09-28 随域下线释放（复用「用过又被回收」的段有老客户端残留风险），`218xx` 已被 `wecom` 占用。

## 7. 前端配套清单

| 项 | 值 | 备注 |
|---|---|---|
| 菜单 code（一级分组） | `hr_group` | `seeds/menu.sql` §1；`path = NULL` 表示分组节点（渲染成 `el-sub-menu`，**不需要**前端路由） |
| 菜单 code（子菜单） | `hr_schedule_calendar` | `seeds/menu.sql` §2 |
| 菜单 path | `/hr/schedule` | ⚠️ 必须与前端 router 的 `path: 'hr/schedule'` **逐字一致**，否则前端路由守卫会把用户踢走（现象是「点菜单闪一下又回到别的页」，**不是编译错误**） |
| 菜单 icon | 子菜单 `Calendar` / 分组 `Avatar` | 必须与前端 `src/layouts/components/menuIcons.ts` 的 `ICON_MAP` 逐字一致；查不到图标在模板里 `v-if` **静默不显示、不报错** |
| 权限点 | `hr.view` / `hr.schedule.manage` | `seeds/rbac.sql` §2 / §3；矩阵是 **MANAGER 写、CLERK 只读**（ADMIN 是元角色，代码里短路通配，seed 不写它的行） |
| 菜单授权 | MANAGER + CLERK 都授 | `seeds/rbac.sql` §4.1 / §4.2。⚠️ CLERK 持有 `hr.view` 却拿不到菜单 = 权限给了但页面进不去，自相矛盾 |
| ⚠️ **API 前缀 ≠ 菜单 path** | API `/api/v2/hr/*`，菜单 `/hr/schedule` | 两层是不同层的东西；菜单 path 不跟着 API 前缀变 |

前端侧的三条同源要求：

1. **不重复实现推导**：渲染一律用后端给的 `day_type` / `effective_day_type` / `is_workday`，不要在前端按星期或按 `is_workday = day_type !== REST_DAY` 之类近似式重算。
2. **`is_workday` 的判据与后端同源**：只有 `WORKDAY` 与 `MAKEUP_WORKDAY` 算上班日。前端若要自己判，用 `effective_day_type` 而**不是** `global_day_type`。
3. **`overflow_minutes` 按分钟渲染**：要显示「跨到次日 02:30」就用分钟自己换算，别指望后端给小时。

⚠️ **部署顺序**：`hr.view` / `hr.schedule.manage` 两个权限点与两个菜单 code 由 `seeds/*.sql` 下发（启动钩子自动跑）。菜单/权限未上而后端已上时，端点会正常返回 40300（不是 404），症状是「页面能进、数据全空」；先跑一次 `./scripts/seed_apply.sh` 即可。

## 8. 跨域影响清单 + 已知取舍登记

### 8.1 跨域影响

**零跨域引用** —— 三张表全是本域自有，排班与工人 / 订单 / 考勤零交集，SQL 真源全部写在 `repo.rs` 里。改本域**不需要**检查其它域的调用点。

⚠️ **但整域「域隔离护栏」刻意不装**：`shared::domain_guard::assert_no_foreign_domain` 是**按路径前缀**判定的，而 `handler.rs` 的 `mod tests` 按仓内统一约定**必须**复用共用比对器 `crate::modules::files::route_guard::routes_in_router_source`（复制一份会让「解析规则」变成两份可漂移的散文）。那一行是本域唯一的跨域路径，且它既不是 service 也不是 repo，只是一条 `#[cfg(test)]` 的路由表比对器。护栏的规则（「不 import 别域的 service / repo」）在这里恰好把**正确**的写法也拦了 —— 与 `serial`、`print_template` 是**同款豁免**。日后若把 `route_guard` 这类「共用测试工具」上提到 `shared`，三处应一并改成「装护栏」。

### 8.2 `t_hr_shift.segments` 是本仓第一个 jsonb 列

取舍论证（时段为什么是内嵌值对象而不是关联表）：

1. **时段没有独立生命周期**：离开班次它没有意义，也不会被单独引用 / 单独软删；考勤侧读的是「某班次的时段」，不是「某条时段」。
2. **没有独立查询口径**：没有人会问「全厂哪些班次有 22:00 起的段」—— 真要问，对着一列 jsonb 用 `jsonb_path_exists` 也能做，且口径本就属于班次。
3. **内联的收益**：班次详情**一次 SELECT 完**（免 join、免 N+1）；改时段是**单列 UPDATE**；而「某天某人的班次时段」是考勤侧的高频读路径，内联让它落进同一次行读取。
4. **被否决的两种形态**：关联表（`t_hr_shift_segment`）多一张表、多一个生命周期、多一遍 join，换不到上面任何一条收益；数组列（`varchar[]` / 自定义 text 编码）省不掉结构化查询的诉求，反而让「段内重叠 / 跨天 span」只能在 Rust 里逐条解字符串。

⚠️ **连带后果：本域走运行时 `sqlx::query_as`，不进 `query!` 宏**（`.sqlx/` 的离线元数据里 0 个 `JSONB` type_info，`query!` + jsonb 在本仓是**零先例组合**，未经验证）。因此本域**不需要**跑 `./scripts/sqlx_prepare.sh`、**不产生** `.sqlx/` 缓存 diff。

⚠️ **`t_hr_shift_calendar` 刻意没有一起 jsonb 化**：它承担的是「按日期分片的行集合」，需要区间 SELECT、批量 upsert / 删、`(shift_id, cal_date)` 的 partial unique 索引、以及将来可能被个人排班 JOIN —— jsonb 一旦要同时满足这四条，就退化成「一堆手写 SQL 里的 `jsonb_array_elements` + 表达式唯一索引」，可读性与可查性都比一张真表差。

### 8.3 已知取舍登记

| # | 取舍 | 内容 |
|---|---|---|
| 1 | **跨度 ≤ 24:00 排除了 `span > 24h` 的班次** | 典型被排除形态：「昨晚 20:00 连做到今天中午 14:00」（span 18h 仍合法，但「22:00 连做 26 小时」不合法）。放宽它需要给两个日历写端点新增跨 `days × shifts` 的重叠检查（O(days × shifts) + 与并发插入之间的 TOCTOU 窗口）。**有意不放宽**，理由见 §2.4 |
| 2 | **日历表只存例外** | 常态工作日 / 常态周末不落行，读端点按星期推导并标 `derived = true`。365 天全落行会让「明年元旦调休」从「改一行」变成「改 200 行」 |
| 3 | **不广播 WS** | 本域是**配置页**，**没有订阅方**：大屏 WS（`/ws/dashboard`）的增量清单里没有排班数据，仓库也没有「排班变更」的消费端。加一条没有订阅者的广播等于给每个大屏客户端白加一次防抖与重取。考勤排班表页面将来落地时走自己的取数路径（HTTP 首取 + 页面级刷新） |
| 4 | **未装域隔离护栏** | 同 `serial` / `print_template` 的同款豁免，理由见 §8.1 |
| 5 | **`color` 传 `null` = 不改** | 端点 6 没有「显式清空可空列」的语义（三态的第三态缺席）。要清空传空串 —— 见 §5.4 |
| 6 | **停用不级联软删覆盖行** | 历史考勤要复现「某天这个班次按什么口径算的」，级联清掉后只剩「全局日历 + 星期推导」能重猜，而覆盖行恰恰是当年与全局不同的那些天 |
| 7 | **`overflow_minutes` 用分钟** | 见 §4.2；用小时会把 90 分钟的溢出报成 1 小时 |
| 8 | **权限守卫在入参校验之前** | 见 §6 末；代价是写端点上无权账号会先开一次事务（零写入、不落锁） |
