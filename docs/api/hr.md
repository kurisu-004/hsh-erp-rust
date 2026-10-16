# hr 域 API（人事排班：全局日历 + 班次 + 班次日历覆盖）

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

`CalendarDayUpsertItem`：`cal_date`（date，必填）/ `day_type`（string，必填，6 档之一）/ `name`（string?，`varchar(50)`，`CUSTOM_LEAVE` **必填**，缺失或全空白 → `21907`；超长 → `21909`）/ `remark`（string?，`varchar(200)`，超长 → `21909`；不进出参，仅管理端留痕）。

⚠️ **同一批里出现同一个 `cal_date` 不会被拒，而是「后写覆盖先写」**（2026-10-23 登记）：那一条只剩一行，终值 = 该日期**最后一次**出现的值。这不是宽容而是**必需** —— 批量 SQL 是 `INSERT … SELECT … FROM UNNEST(…) ON CONFLICT (cal_date) WHERE deleted_at IS NULL DO UPDATE`，同批命中同一行两次时 PG 抛 `ON CONFLICT DO UPDATE command cannot affect row a second time`（SQLSTATE `21000`），既不是 `23505` 也不在任何显式档 ⇒ **HTTP 500**。而「一次范围拖选 + 一次补写」就能造出这种请求，门槛是零。口径与读侧 `into_day_map` 的「脏数据后写覆盖先写」同源：同一天只能有一个最终口径，后出现的那条就是调用方的最新意图。端点 9 同款。

⚠️ 条数上限 366（`21904`）：见 §5.2。

### 3.4 `CalendarDaysDeleteRequest` / `ShiftDaysDeleteRequest`（端点 3 / 10）

| 字段 | 类型 | 语义 |
|---|---|---|
| `dates` | date[]? | 形态 A |
| `from` / `to` | date? | 形态 B |

⚠️ **必须恰好命中一种形态**：都省、只给一半、都给、`dates` 为空数组 —— 四种非法形态一律 `21904`（而不是「删 0 行」的假成功）。判据写成一个 catch-all 而非逐个列举：`Option<Vec<_>>` 的组合有 3×2×2 = 12 种，逐个列必然漏一两个。

### 3.5 `CreateShiftRequest`（端点 5）

| 字段 | 类型 | 语义 |
|---|---|---|
| `code` | string | 必填，`varchar(30)`，未软删行内唯一；重复 → `21902`（HTTP 409，**不是** `40901` —— 后者只表达 OCC）。⚠️ **trim 后为空 / 全空白 → `21909`**，超 30 字符 → `21909` |
| `name` | string | 必填，`varchar(50)`，落库前 `trim()`；⚠️ trim 后为空 → `21909`，超 50 字符 → `21909` |
| `segments` | segment[] | **必填且无 `#[serde(default)]`**（缺 → axum `Json` 提取器返 **HTTP 422 纯文本**）——「没有时段的班次没有意义」。形态见 §2.4 |
| `color` | string? | `varchar(20)`，前端色板值；可 NULL。超 20 字符 → `21909` |
| `sort_order` | int? | 缺省 `0` |
| `remark` | string? | `varchar(200)`；超 200 字符 → `21909`（人事随手粘贴的自由文本，超长是**日常操作**） |

⚠️ 建班次的 `version = 0`、`is_active = true` —— **入参里没有** `version` / `is_active`：新建永远从启用态起（建了就是能用的），下架是独立的端点 7。

⚠️ **列宽守卫是必需的、不是锦上添花**：PG 对 `varchar(n)` 超长抛 `22001`（`string_data_right_truncation`），既不是 `23505`（`is_unique_violation` 判不到）也不在任何显式档 ⇒ 会落 `50001` / **HTTP 500**、message 被替换成「数据库错误」。service 侧统一用 `check_len` 判**字符数**（不是字节数 —— `varchar(30)` 放得下 30 个汉字，前端可以放心传中文 `code` / `name` / `remark`）。

### 3.6 `UpdateShiftRequest`（端点 6）—— 部分覆盖

| 字段 | 类型 | 语义 |
|---|---|---|
| `version` | int | **必填 OCC 锚**（CLAUDE.md §8）。陈旧 → `40901`；**缺失 → axum 的 422 纯文本**（不是业务信封） |
| `code` / `name` | string? | 给了才改（`name` 会 `trim()`）。⚠️ trim 后为空 → `21909`（能建出来不等于能改成空）；超列宽（30 / 50）→ `21909` |
| `segments` | segment[]? | 给了就**整列替换**（不做段级 merge），且重跑一遍 V1–V6；不传则沿用行上那份 |
| `color` / `remark` | string? | ⚠️ 传 `null` 等于「**这次不改**」—— 本端点**没有**「显式清空某个可空列」的语义（要清空就传空串）。超列宽（20 / 200）→ `21909` |
| `sort_order` | int? | 给了才改 |

⚠️ 本 DTO **没有** `is_active`：启用 / 停用是两个意图，停用有独立端点。

### 3.7 `DeactivateShiftRequest`（端点 7）

| 字段 | 类型 | 语义 |
|---|---|---|
| `version` | int | 必填 OCC 锚（同端点 6） |

### 3.8 `ShiftDaysUpsertRequest` / `ShiftDayUpsertItem`（端点 9）

| 字段 | 类型 | 语义 |
|---|---|---|
| `items` | item[] | 必填；空数组 → `21904`，超 366 条 → `21904`（同端点 2 的口径） |
| `items[].cal_date` | date | 必填；同批重复 → **后写覆盖先写**（见 §3.3 的警示） |
| `items[].day_type` | string | 必填，6 档之一 |
| `items[].remark` | string? | 要写说明走这里；`varchar(200)`，超长 → `21909` |

⚠️ **刻意没有 `name`**（`t_hr_shift_calendar` 表也没这列）：「这天为什么特殊」是**全局日历**的语义，班次覆盖只改「这天算不算上班」。⇒ 端点 9 写 `CUSTOM_LEAVE` **不**要求 `name`、也**不**落 `21907`（那条闸门只属全局日历）。

## 4. 出参逐字段

### 4.1 `CalendarDayOut`（端点 1，区间内**逐日一行，一天都不省**）

| 字段 | 类型 | 口径 |
|---|---|---|
| `cal_date` | date | 按升序 |
| `day_type` | string | 已含按星期推导的兜底（前端**不需要**再自己判周六周日），字面量见 §2.2 |
| `name` | string \| null | 节假日名 / 休息日说明；`derived = true` 的行**恒为 `null`**（库里就没这行） |
| `derived` | bool | `true` = 该日**无显式行**，值是按星期推导出来的 |
| `shift_bands` | `ShiftBandOut[]` | **2026-10-23 新增**：该日**逐班次**色带（见 §4.1.1），供全局日历直接渲染「所有班次的颜色带」 |

#### 4.1.1 `ShiftBandOut`（端点 1 的 `shift_bands[]` 元素，**2026-10-23 新增**）

| 字段 | 类型 | 口径 |
|---|---|---|
| `shift_id` | string | 雪花 id 序列化成 **JSON string**（与 §4.3 的 `ShiftCalendarOut.shift_id` 逐字同款） |
| `effective_day_type` | string | 该班次这一天**真正生效**的日型（三级推导，见 §2.3）。**无覆盖行时逐字等于同一行的 `day_type`**；有覆盖行时可以与全局**不同** |
| `is_workday` | bool | `effective_day_type` 是否算上班日（考勤取数的**唯一**判据，判据表见 §2.2） |
| `is_override` | bool | 是否来自本班次的覆盖行（判据是**存在性**，见 §2.3） |

三条口径，**前端渲染色带全靠它们**：

1. **顺序稳定**：按班次 `sort_order` 升序、再 `code` 升序（`code` 在未软删行内唯一 ⇒ 全序）。**同一次响应内**数组顺序稳定 ⇒ 前端**只把它当渲染顺序**用（**不需要自己再排一次**）。⚠️ 但这个顺序**不保证与端点 4 `GET /shifts` 一致** —— 那边 `sort_order` 打平时按 `id` 兜底（`ORDER BY is_active ASC, sort_order ASC, id ASC`），两个键打平且 `code` 与 `id` 序不一致时两端点**会给出不同的数组顺序**。⇒ **配对一律按 `shift_id`**（拿 `shift_id` 建 Map 查表配对，**不是**按数组下标 zip），**不得按下标配对**，否则色带会串到别的班次上且**全程无任何报错**（色带本身合法，只是顺序对不上）。
2. **只含 `is_active = true` 的班次**：停用班次不排人，不该在排班表上占一条色带。（停用班次的覆盖行**仍在库里**且端点 8 照常可读 —— 停用只挡写，见 §5.8。）
3. ⚠️ **`is_workday = false` 的班次照样出现**，只过滤停用班次、**不**在这里按 `is_workday` 过滤：前端要的就是「这天哪些班次在休」，滤掉就没有色带了。**要不要渲染由前端自己决定。**

⚠️ 形状恒定：**恰好 4 个键**，`shift_bands` 恒为数组（无活跃班次时是 `[]`，不是 `null`）。刻意**不**把 `code` / `color` / `name` 塞进 band —— 那些是端点 4 的真相源，复制一份就会与班次列表不同步。

⚠️ 推导与端点 8 走**同一个**后端函数（`service::derive_effective`），`is_workday` 恒取 `effective_day_type.is_workday()` ⇒ **两个端点不可能给出不同的 `is_workday`**。前端也不得自己重算（见 §7）。

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

### 5.2 区间 366 天上限 + 单批 366 条上限

**跨度**：`from <= to` 且 `(to − from) ≤ 366` 天（判据是**差值**；两端闭 ⇒ 最多 **367** 个日历日），违反一律 `21904`（HTTP 400）。理由：读端点要**逐日推导并回传**，一个请求灌 10 年就是 3650 个出参项 —— 前端用不上，却已经把 DB 与序列化 CPU 花掉了。

**条数**：一条 `items` / `dates` 的元素数 ≤ **366**，违反同样 `21904`。

⚠️ 两条判据**缺一不可**：跨度只看「最小 ~ 最大日期」，所以 367 个**全同日期**的项跨度恒为 0、绕得过跨度，却仍会灌进一次巨大的 `UNNEST`（请求体解析完、`Vec` 已分配之后才被拒，那点内存与 CPU 已经花掉了）。条数上限卡在**去重之前**，让这种请求在入口就被拒。

⚠️ **批量 upsert 的 `{items:[…]}` 走同一条判据**：`items` 的最小 / 最大 `cal_date` 构成一个**隐式区间**。不卡它的话，攒一个大 `items` 就能把「一个请求最多 366 天」的承诺从读端点绕过去。

### 5.3 同批重复日期 = 后写覆盖先写

端点 2 / 9 的 `items` 里出现同一个 `cal_date` 时，那一天**只落一行**，终值取该日期**最后一次**出现的值。

⚠️ **这不是宽容，是必需**：批量 SQL 是 `INSERT … SELECT … FROM UNNEST(…) ON CONFLICT … DO UPDATE`，同一条命令影响同一行两次时 PG 抛 `ON CONFLICT DO UPDATE command cannot affect row a second time`（SQLSTATE `21000`）—— 既不是 `23505`（`is_unique_violation` 判不到）也不在任何显式档 ⇒ `50001` / **HTTP 500**、message 被替换成「数据库错误」。而「前端一次范围拖选 + 一次补写」就能造出这种请求。

去重后保留**第一次出现的位置**（只把值换成后写那条），这样 `affected` 与入参顺序的关系不会变得不可预期。

### 5.4 空 `items` 落 `21904`（不是静默 `affected: 0`）

端点 2 与端点 9 的 `items` 为空数组 ⇒ `21904`。理由：「没说要写什么」不是合法请求；返回 `affected: 0` 会让调用方以为「写了但没生效」。这与 §4.4 的「delete 的 `affected = 0` 是合法成功」是**两件不同的事**，别混。

### 5.5 `null` 与缺省的语义

- **入参 `Option<T>` 的 `None` 一律是「这次不改」**（端点 6），**没有**「显式清空某个可空列」的语义 —— 要清空就传空串（限 `color` / `remark` 两个可空列；⚠️ **`name` 不适用**，它是必填项，传空串 / 纯空白是 `21909` 而不是「清空」，见 §5.7）。
- **`segments` 是单列覆盖**：传了就整列替换（不做段级 merge），不传就保持原值。
- **出参的可空列是真的可空**：`ShiftOut` 的 `color` / `remark` 是 `string | null`，前端要能处理 `null`（不要 `x || ''` 后再判空）。⚠️ **别把 `CalendarDayOut.name`（全局日历，可空）的形态套到 `ShiftOut.name` 上** —— 后者是必填的 `string`，恒非空。
- ⚠️ **「要清空就传空串」落库得到的是空串 `''` 而不是 `NULL`**：service 直接把入参值绑进 `UPDATE`，没有任何「空串 ⇒ 转 NULL」的转换。所以回显（GET / 列表 / 改完的响应）里 `color` / `remark` **恒为空串**，不是 `null`。⇒ **出参 `color` / `remark` 两种形态都要放行**：`null` = 从没设过 / 库里是 NULL，`''` = 被显式清空过（也是前端唯一能表达「清空」的入参）。把 `''` 当成非法值拒掉，表单里就再也清不掉这两个字段了。

### 5.6 `version` 缺失是 422 纯文本，不是业务信封

端点 6 / 7 的 `version` 必填且无 `#[serde(default)]`，缺失时请求**到不了 service**，由 axum `Json` 提取器直接拒 ⇒ **HTTP 422 + 纯文本 body**（不是 `R { code, … }` 信封）。前端错误处理要把「解析信封」与「解析纯文本」两条路都留着。端点 5 的 `segments` 缺失同理。

### 5.7 字段值守卫（`21909`）：空白必填项 + `varchar` 列宽

service 在落库前对每个 `varchar` 入参做两道检查，违规一律 `21909` / HTTP 400：

| 检查 | 覆盖列 | 上限 |
|---|---|---|
| 必填项 trim 后非空 | `code` / `name`（端点 5、6） | — |
| 列宽（**字符数**，不是字节数） | `code` / `name` / `color` / `remark`（端点 5、6）、日历 `name` / `remark`（端点 2）、覆盖行 `remark`（端点 9） | 30 / 50 / 20 / 200 |

⚠️ **为什么必须显式判**：PG 对 `varchar(n)` 超长抛 `22001`（`string_data_right_truncation`），落不进任何显式档 ⇒ `50001` / **HTTP 500**、message 变成「数据库错误」。而 `remark` 是人事随手粘贴的自由文本，超长是**日常操作** —— 不判就是把一次正常粘贴变成 500。

⚠️ **为什么按字符数**：`varchar(30)` 计的是**字符**，放得下 30 个汉字（90 字节）。按字节判会把「10 个汉字的 code」误拒成 400 —— 挡住合法输入比 500 更坏（它让用户以为自己输错了）。

⚠️ **为什么空白 `code` 要拒**：它会**占住** `uk_t_hr_shift_code` 的 partial unique —— 建出一条 `code = ''` 的班次后，任何人想用空 code 都撞 `21902`，而那条空班次本身没有任何业务含义。`name` 空串则让排班表上出现一行没名字的班次。两者都是「入参非法」（`21909`）而不是「状态冲突」（`21902`）。

⚠️ **检查顺序在 `code_exists` 重复预检之前**：`require_text` / `check_len` 先判 ⇒ **空白 code 一律 `21909`，不会落到 `21902`**。反过来先查重，空 code 会先撞上「库里已存在的空 code」而误报成重复。端点 6 同理：新 `code` 的两道守卫都在 `code_exists(conn, new_code, Some(id))` 之前。

### 5.8 「班次可写」不做读-再-写（判据落点逐端点不同）

service 侧那次 `get_shift` + `ensure_writable` 只是**友好报错的前置检查**，真正的判据在写语句自己的谓词里 —— 但**四条写端点的判据形态各不相同**：

- **端点 7（停用）**：写 SQL 带 `AND is_active`，完整谓词是 `WHERE id = $1 AND version = $2 AND deleted_at IS NULL AND is_active`。这个动作本来就要求「当前启用」，带不带都是同一个结果，带上是显式表达。
- **端点 9（写覆盖行）/ 端点 10（清覆盖行）**：以 `FROM … , t_hr_shift s` / `UPDATE … FROM t_hr_shift s` **关联**的形式带班次侧判据（`WHERE s.id = $N AND s.is_active AND s.deleted_at IS NULL`）。
- **端点 6（改班次）：写 SQL 里没有 `is_active`**，只有 `WHERE id = $N AND version = $N AND deleted_at IS NULL`。它的 `21906` **不是**来自写语句的谓词，而是来自 0 行后的**回查归因**（见下方归因表）：并发停用会 `version = version + 1`，所以 `version` 判据**先**失败落 0 行，service 回查看到 `is_active = false` ⇒ 归因成 `21906`。这不构成 TOCTOU 破绽 —— 判据虽不在写语句里，但「停用一定自增 version」保证了这个写不可能在一个已停用的班次上成功。

⚠️ **为什么这些写端点都不能退回「读-再-写」**：service 侧那次 `get_shift` 与写语句之间有 TOCTOU 窗口，并发的 `POST /shifts/{id}/deactivate` 落在窗口里时，覆盖行会写进一个已经停用的班次，而 `21906` 被整个绕过。

⚠️ **0 行的归因**（前端要按 code 分流，不要一律当 OCC）：

| 库里的现状 | `update` / `deactivate` / `upsert days` | `delete days` |
|---|---|---|
| 查不到 / 已软删 | `21901` | `21901` |
| 在但已停用（并发） | `21906` | `21906` |
| 在、启用中 | `40901`（`version` 陈旧） | `Ok(affected = 0)` —— 合法成功 |

⚠️ 最后一行是两种语义的分野：端点 10 的 `affected = 0` 表示「那几天本来就没有覆盖行」（见 §4.4），**不是**错误；端点 6 / 7 的 0 行则一定是冲突。所以只有端点 10 的 0 行需要「班次仍可写才放行」这一脚回查。

## 6. 错误码

| 码 | 常量 | HTTP | 触发场景 |
|---|---|---|---|
| `21901` | `BIZ_HR_SHIFT_NOT_FOUND` | 404 | 查 / 改 / 停用 / 写覆盖 / 清覆盖一个**不存在或已软删**的班次（含端点 8 的读端点）；也含写 SQL 0 行后的回查归因（§5.8） |
| `21902` | `BIZ_HR_SHIFT_CODE_DUPLICATED` | 409 | 班次 `code` 重复（**不是** `40901`，后者只表达 OCC）。应用层预检与 DB partial unique 的并发兜底**同码** |
| `21903` | `BIZ_HR_SHIFT_SEGMENT_INVALID` | 400 | 段非法（V1–V5 任一）**或**库里那一列 jsonb 解不出来 / 解出来过不了 V1–V6（降级成 21903 而不是 500） |
| `21904` | `BIZ_HR_CALENDAR_RANGE_INVALID` | 400 | `from > to` / 跨度 > 366 天 / upsert 或 delete 的批量**条数** > 366 / upsert 的 `items` 为空 / 删行请求没恰好命中 `{dates}` 或 `{from,to}` 一种形态 |
| `21905` | `BIZ_HR_DAY_TYPE_INVALID` | 400 | 未知 `day_type`（含大小写不符） |
| `21906` | `BIZ_HR_SHIFT_INACTIVE` | 409 | 对**已停用**班次做写操作（改 / 停用 / 写覆盖 / 清覆盖）。重复停用、并发停用后迟到的写都走这条 |
| `21907` | `BIZ_HR_CALENDAR_LEAVE_NAME_REQUIRED` | 400 | `CUSTOM_LEAVE` 未填 `name`（**仅全局日历**，见 §3.8） |
| `21908` | `BIZ_HR_SHIFT_SPAN_EXCEEDS_DAY` | 400 | **V6**：`span > "24:00"` |
| `21909` | `BIZ_HR_FIELD_INVALID` | 400 | **字段值非法**：必填项（`code` / `name`）trim 后为空白，或某字段超出 `varchar` 列宽（判**字符数**）。⚠️ 不判的话 PG 的 `22001` 落不进任何显式档 ⇒ **500**，见 §5.7 |
| `40901` | `VERSION_CONFLICT` | 409 | 端点 6 / 7 的 `version` 与库内当前值不一致（也含写 SQL 0 行后的 OCC 归因，见 §5.8） |
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

前端侧的五条同源要求：

1. **不重复实现推导**：渲染一律用后端给的 `day_type` / `effective_day_type` / `is_workday`，不要在前端按星期或按 `is_workday = day_type !== REST_DAY` 之类近似式重算。
2. **⚠️ 不得按班次自己重算推导**（2026-10-23）：全局日历上的「每个班次这天上不上班」一律读端点 1 的 `shift_bands[].is_workday`（§4.1.1），**不要**为了画色带去逐个班次发 `GET /shifts/{id}/calendar`（那是 N+1 次请求），也不要拿 `day_type` 加 `is_active` 之类近似式在本地推。端点 1 与端点 8 的 `is_workday` 由**同一个**后端函数算出，前端复制一遍就等于埋了第二套口径。
3. **`is_workday` 的判据与后端同源**：只有 `WORKDAY` 与 `MAKEUP_WORKDAY` 算上班日。前端若要自己判，用 `effective_day_type` 而**不是** `global_day_type`（端点 1 的 band 上只有 `effective_day_type`，没有 `global_day_type`）。
4. **`overflow_minutes` 按分钟渲染**：要显示「跨到次日 02:30」就用分钟自己换算，别指望后端给小时。
5. **长度上限的前端常量必须逐字对齐 `varchar` 真值**（`code` **30** / `name` **50** / `color` **20** / `remark` **200**，日历 `name` 50 / `remark` 200）：后端超长一律 `21909` / 400，前端多放 1 个字符就会让用户撞一条「看起来像格式错」的报错。同理，`code` / `name` 前端不要用 `trim()` 前的长度做校验 —— **后端判的是 trim 后的值**。

⚠️ **部署顺序**：`hr.view` / `hr.schedule.manage` 两个权限点与两个菜单 code 由 `seeds/*.sql` 下发（启动钩子自动跑）。菜单/权限未上而后端已上时，端点会正常返回 40300（不是 404），症状是「页面能进、数据全空」；先跑一次 `./scripts/seed_apply.sh` 即可。

⚠️ **部署顺序（出参形状变更，2026-10-23）**：端点 1 的**每个日行新增 `shift_bands` 键**（§4.1 / §4.1.1），前端 `hrCalendarDaySchema` 是 **`.strict()`** ⇒ **后端先上、前端没同步加这个键，每一个响应都会抛 `unrecognized_keys`，全局日历整页空**（不是部分错、是无报错的白屏）。这条与上面的「seed 未上」症状长得不一样（那个是 40300 + 数据空），排查时先看 devtools 里有没有 `unrecognized_keys`。⇒ **上线顺序必须前端先，或前后端同批**；只回后端不回前端 = 必坏。

## 8. 跨域影响清单 + 已知取舍登记

### 8.1 跨域影响

**零跨域引用** —— 三张表全是本域自有，排班与工人 / 订单 / 考勤零交集，SQL 真源全部写在 `repo.rs` 里。改本域**不需要**检查其它域的调用点。

⚠️ **但整域「域隔离护栏」刻意不装**：`shared::domain_guard::assert_no_foreign_domain` 是**按路径前缀**判定的，而 `handler.rs` 的 `mod tests` 按仓内统一约定**必须**复用共用比对器 `crate::modules::files::route_guard::routes_in_router_source`（复制一份会让「解析规则」变成两份可漂移的散文）。那一行是本域唯一的跨域路径，且它既不是 service 也不是 repo，只是一条 `#[cfg(test)]` 的路由表比对器。护栏的规则（「不 import 别域的 service / repo」）在这里恰好把**正确**的写法也拦了。

⚠️ **与 `serial` / `print_template` 的差别不要说成「同款」**：那两域不装护栏，是因为它们的**生产代码**里就 import 了别域（`assembly::repo` / `files::part_file`），护栏即使放过 `#[cfg(test)]` 也会照样报警；本域唯一的跨域路径**只在 `#[cfg(test)]` 里**，生产代码零跨域。三处都「不装护栏」，成因并不同款。日后把 `route_guard` 上提到 `shared` 之后，本域应改成「装护栏」（它那时真的干净了），另两域仍不装。

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
| 4 | **未装域隔离护栏** | 本域唯一的跨域路径只在 `#[cfg(test)]` 里（理由见 §8.1），与 `serial` / `print_template`（**生产代码**就 import 别域）成因不同款 |
| 5 | **`color` 传 `null` = 不改** | 端点 6 没有「显式清空可空列」的语义（三态的第三态缺席）。要清空传空串 —— 见 §5.5 |
| 6 | **停用不级联软删覆盖行** | 历史考勤要复现「某天这个班次按什么口径算的」，级联清掉后只剩「全局日历 + 星期推导」能重猜，而覆盖行恰恰是当年与全局不同的那些天 |
| 7 | **`overflow_minutes` 用分钟** | 见 §4.2；用小时会把 90 分钟的溢出报成 1 小时 |
| 8 | **权限守卫在入参校验之前** | 见 §6 末；代价是写端点上无权账号会先开一次事务（零写入、不落锁） |
| 9 | **两张批量写表上的 `version` 不做客户端锚**（偏离 CLAUDE.md §8「带 `version` 列的表写端点 body 必须收 `version`」） | `t_hr_calendar_day` / `t_hr_shift_calendar` 的写端点是 `{items:[…]}` 的**整批 upsert / 软删**，行级 OCC 在这个形态下不成立：一批里每一行都是「按 `cal_date` 命中即覆盖」，客户端并不持有某一行的 `version`，要塞进 `items[].version` 只会暴露内部实现、并把「一批要么全过要么全不进」拆成一堆行级判据。真正要防的并发（两个人把同一天改成不同口径）由「partial unique + `ON CONFLICT DO UPDATE` 的后写覆盖」与「`t_hr_shift` 上的真 OCC」挡住。⇒ `version` 在这两张表上**只作写计数器**（每次 upsert / 软删自增，供审计与将来排查）。先例：`t_wecom_daily_log`（只增审计流水、刻意无 `version`）。客户端的版本锚**只加在 `t_hr_shift` 上**（端点 6 / 7） |
| 10 | **不另建同列的非 partial 索引** | 两条读查询的谓词恒带 `deleted_at IS NULL`，被同列 partial unique 的谓词**蕴含** ⇒ PG 直接拿 partial unique 走索引。`ix_t_hr_calendar_day_cal_date` / `ix_t_hr_shift_calendar_shift_date` 只会各多贡献一份写放大（每次 upsert / 软删维护两棵同样的 B 树），读端点一行都用不上 |
| 11 | **`shift_bands` 让出参体积 = 区间天数 × 活跃班次数**（2026-10-23） | 端点 1 每行带**逐班次色带**（§4.1.1）⇒ 跨度上限 366 天 ⇒ 两端闭**最多 367 天**；⚠️ **band 条数 = 367 × 活跃班次数，而活跃班次没有上限**（端点 4 不分页、`list_shifts` 全量返回）⇒ 20 个活跃班次是 7340 条（实测体积见 §8.4），30 个就是 11010 条。响应体随班次数**线性**放大，**没有系统级上界**，体积明显变大。**有意不为它下调区间上限（§5.2）**：上限防的是「一个请求灌 10 年」，而色带是前端渲染排班表的必需数据 —— 砍掉它等于让前端退回 N+1 次 `GET /shifts/{id}/calendar`。推导是**纯内存** HashMap 查表，SQL 总共仍是 3 条（全局行 / 活跃班次 / 区间内全部覆盖行），**不是** N+1 查询。实测体积见 §8.4。⚠️ 那第 3 条 SQL（`list_shift_overrides_in_range`）**按 `cal_date` 过滤，而 `t_hr_shift_calendar` 上唯一可用索引的前导列是 `shift_id`**（`uk_t_hr_shift_calendar (shift_id, cal_date) WHERE deleted_at IS NULL`）⇒ 目前（表体量小时）走**顺序扫描**；本域刻意不为此建 `(cal_date)` 索引（与第 10 条同源取舍）。⚠️ **这不是稳态结论**：这条 SQL 的 `ORDER BY shift_id, cal_date` 恰好与该索引的列序一致、谓词 `deleted_at IS NULL` 又与 partial 谓词一致，体量上来后 PG 完全可能改走 `uk_t_hr_shift_calendar` 的 Index Scan 再按 `cal_date` 过滤 —— **两条都不理想**（前者扫全表，后者读全索引再丢弃）。届时需二选一：补 `(cal_date, shift_id)` 索引（列序同时对上 `cal_date` 的区间过滤与 `ORDER BY shift_id, cal_date` ⇒ **Filter 与 Sort 一并免掉**；只补 `(cal_date)` 只能免 Filter、仍要一次显式 Sort），或改成按活跃班次 `shift_id = ANY($1)` 让现有索引可用 |

### 8.4 `shift_bands` 的实测序列化体积（2026-10-23）

用**真实出参结构**（`CalendarDaysOut` 逐日最多 367 行 + 每行 N 条 `ShiftBandOut`，雪花
`shift_id` 取 19 位）实测 `serde_json::to_string` 的字节数。⚠️ **行数口径**：跨度上限是
**366 天**（`to − from ≤ 366`，§5.2），但两端闭 ⇒ **最多 367 个日历日**，下表的
「区间天数」一律指**两端闭的实际天数**：

| 区间天数（两端闭） | 活跃班次 | band 条数 | 整份出参字节 | 单条 band 约 |
|---:|---:|---:|---:|---:|
| 31（一个月） | 5 | 155 | 19,020 B（约 18.6 KB） | 约 123 B |
| 366 | 5 | 1,830 | 224,415 B（约 219 KB） | 约 123 B |
| 366 | 10 | 3,660 | 415,775 B（约 406 KB） | 约 114 B |
| **367（跨度 366 天的上限，两端闭）** | **20** | **7,340** | **800,706 B（约 782 KB）** | 约 109 B |

- ⚠️ 末列是「整份出参字节 ÷ band 条数」的**摊薄均值**，**不是**单条 band 自身的尺寸。整份出参
  = band 字节 + **日行骨架**字节（`cal_date` / `day_type` / `name` / `derived`，实测约
  90 B/天），而日行骨架是**按天**摊的 ⇒ 班次数越少，每条 band 分摊到的骨架越厚，均值就越高。
  上界那行（367 天 × 20 班次）骨架被摊得最薄，均值几乎贴着单条 band 自身尺寸。
- 单条 band 的**上界**约 **114 B**：形态恒为 4 个键，日型字面量取最长的一档
  `STATUTORY_HOLIDAY`（17 字符，`is_workday` 随之恒为 `false`）⇒
  `{"shift_id":"7300000000000000000","effective_day_type":"STATUTORY_HOLIDAY","is_workday":false,"is_override":false}`。
  同一形态的基线（`WORKDAY`，7 字符、`is_workday` 为 `true`）实测 **103 B**，加进数组后
  每条再摊 1 B 分隔逗号 ⇒ 约 104 B，正是上表末列小值的来源。`shift_id` 是 19 位雪花
  （字符串 19 字节 + 引号），占其中约 1/4。
- ⚠️ **常态请求远小于上表**：上界要同时「区间拉满」且「班次编到 20 个」；一个月 5 个班次
  只有 ~19 KB。体积是**线性**增长的（O(days × shifts)），没有二次项。
- 结论：**不为色带下调 366 天的跨度上限**（理由见 §8.3 第 11 条）。真要再省，只能让前端
  按月分段取（区间口径不变）—— 那是前端侧的取数策略，后端不为它加参数。
