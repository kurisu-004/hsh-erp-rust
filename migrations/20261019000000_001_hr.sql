-- ============================================================================
-- 20261019000000_001_hr —— 人事排班域：全局日历 + 班次 + 班次日历覆盖
-- ============================================================================
-- 需求：给考勤排班打基础。本期是**纯配置域**，**不做班次与实际工人的绑定**。
--
-- 归属域（本迁移只建表，不建模块）：
--   * `src/modules/hr/` —— 人事排班域
--   URL 挂 `/api/v2/hr`（见 `src/modules/mod.rs::v2_router()` 的登记）。
--
-- 与本仓既有约定的关系（全部照 CLAUDE.md「DB 约定」）：
--   * 无物理外键 —— `t_hr_shift_calendar.shift_id` 是裸 bigint，存在性由 service 校验
--     （查不到 / 已软删 → 21901 BIZ_HR_SHIFT_NOT_FOUND）。
--   * 无 DB ENUM —— `day_type` 是 varchar(24)，合法值由 Rust 侧 `DayType` 枚举校验
--     （未知值 → 21905 BIZ_HR_DAY_TYPE_INVALID）。
--   * 乐观锁 —— 带 `version` 列，写路径走 `UPDATE … WHERE id=$1 AND version=$2`，
--     0 行 → 40901 VERSION_CONFLICT。
--   * 软删 —— 带 `deleted_at`；部分唯一索引都以 `deleted_at IS NULL` 为前提。
--   * 雪花主键 —— `id` 由 App 侧 `SnowflakeIdGenerator` 生成，不在 DB 里取号。
--
-- ---------------------------------------------------------------------------
-- ⚠️ 日历模型：**全局日历 + 班次单日覆盖**，没有独立的「班次日历」实体
-- ---------------------------------------------------------------------------
-- 班次的「自己的工作日历」= 全局日历 ∪ 本班次覆盖行。**不存在 calendar_id 列**，
-- 也没有 `t_hr_shift_calendar` 之外的第三张表来承载「一份可被多班次共享的日历」。
--
-- 推导规则（**唯一真源在后端 service**，前端不重复实现）：
--
--     effective_day_type(shift, date) =
--         班次覆盖行(day_type)                            若存在
--       | 全局日历行(day_type)                            若存在
--       | 按星期推导（周六周日 = REST_DAY，否则 WORKDAY）    兜底
--
--     is_workday = day_type ∈ { WORKDAY, MAKEUP_WORKDAY }
--
-- 日型 6 档：WORKDAY（工作日）/ MAKEUP_WORKDAY（调休上班）/ REST_DAY（正常周末）/
-- STATUTORY_HOLIDAY（法定节假日）/ MAKEUP_HOLIDAY（调休放假）/ CUSTOM_LEAVE（自定义休息日）。
-- ⚠️ **日历表只存例外**：常态工作日 / 常态周末**不落行**，读端点在无显式行时按星期推导
-- 并把该行标 `derived = true`。365 天全落行会让「明年元旦调休」这类操作从「改一行」
-- 变成「改 200 行」。
--
-- ---------------------------------------------------------------------------
-- ⚠️ 为什么 `t_hr_shift.segments` 用 **jsonb**（本仓 42 张表里的第一个 jsonb 列）
-- ---------------------------------------------------------------------------
-- 取舍论证（被否决的方案与代价都记在这里，免得日后有人照「别的域都是关联表」的
-- 直觉把它改回去）：
--
-- 1. 时段是班次的**内嵌值对象**，不是独立实体：
--    - 无单独生命周期（时段离开班次没有意义，也不会被单独引用 / 单独软删）；
--    - 不被其它域引用（考勤那侧读的是「某个班次的时段」，不是「某条时段」）；
--    - 无单独查询口径（没有人会问「全厂哪些班次有 22:00 起的段」—— 真要问，
--      对着一列 jsonb 用 `jsonb_path_exists` 也能做，且口径本就属于班次）。
-- 2. 内联之后：
--    - 班次详情**一次 SELECT 完**，免 join、免 N+1；
--    - 改时段是**单列 UPDATE**（`SET segments = $n`），不必动子表、不必级联；
--    - 而考勤侧「某天某人的班次时段」是高频读路径 —— 每次读工时都要拿到时段定义，
--      内联让它落进同一次行读取里。
-- 3. 被否决的方案：
--    - **关联表**（`t_hr_shift_segment`）：多一张表、多一个生命周期、多一遍 join，
--      换不到上面任何一条收益（时段没有独立查询口径）；
--    - **数组列**（`varchar[]` / 自定义 `text` 编码）：省不掉结构化查询的诉求，
--      反而让「段内重叠 / 跨天 span」这类校验只能在 Rust 里逐条解字符串。
--
-- 形状：`[{"seq":1,"start":"22:00","end":"26:00"}]` —— **绝对时钟轴**：
-- 段值域是 `cal_date 00:00` 起算的绝对偏移，字符串 `"HH:MM"`，`HH ∈ 0..48`
-- （夜班 22:00→次日 02:00 写 `22:00` / `26:00`）。**「跨天班次的日期归属 = 工作开始
-- 时间所在日」由数据结构本身保证**，不需要任何推断逻辑。
-- ⚠️ 写侧校验（`service::validate_segments`，V1–V6）保证
-- `span = max(end) − min(start) ≤ "24:00"` ⇒ 相邻两日 / 隔日的班次实例在时间轴上
-- **结构性地不可能重叠**，日历写端点因此**零重叠检查**。
--
-- ⚠️ `ck_t_hr_shift_segments` 的 `CASE WHEN` **不能省**：CHECK 约束是逐行求值的，
-- 而 `jsonb_array_length()` 遇到非数组（标量 / 对象 / 字符串）时**直接抛错**而不是返回
-- NULL ⇒ 裸写 `jsonb_array_length(segments) BETWEEN 1 AND 6` 会让一条
-- `segments = 'null'` 的 INSERT 报 `23514 CHECK 违规` 之外的 SQL 内部错误
-- （`cannot get array length of a scalar`），把「入参不合法」变成 500。
--
-- ---------------------------------------------------------------------------
-- ⚠️ 为什么覆盖行**不能**也 jsonb 化
-- ---------------------------------------------------------------------------
-- `t_hr_shift_calendar` 与 `t_hr_shift.segments` 的形态看似同类（都是「一个班次的内嵌
-- 数据」），但它承担的是**按日期分片的行集合**，不是值对象：
--   * 区间 SELECT：`GET /shifts/{id}/calendar?from&to` 要按 `cal_date` 取一段；
--   * 批量 upsert / 批量删除：写端点是 `{items:[…]}` 或 `{from,to}` 两种形态；
--   * 独立唯一索引：`(shift_id, cal_date)` 的 partial unique 是防重复写同一覆盖行的
--     **唯一手段**（jsonb 里存一个日期数组没法建这种索引）；
--   * 将来可能被**个人排班**引用（某天某人的班次落在哪一天的口径以覆盖行为准），
--     那时它就是一个要被 JOIN 的行实体。
-- jsonb 一旦要同时满足上面五条，就会退化成「一堆手写 SQL 里的 `jsonb_array_elements`
-- + 表达式唯一索引」，可读性与可查性都比一张真表差。故这里是真表。
--
-- ⚠️ **覆盖行不做「级联软删」**：`t_hr_shift` 停用（`POST /shifts/{id}/deactivate`
-- 走 `is_active = false`）时，`t_hr_shift_calendar` 的行**原样保留**。理由：将来查历史
-- 考勤要复现「某天这个班次按什么口径算的」，若级联清掉，历史就成了空白，只能靠
-- 「全局日历 + 星期推导」重新猜 —— 而覆盖行恰恰是当年与全局不同的那些天。
-- ============================================================================

-- ---------------------------------------------------------------------------
-- 1. 全局日历（显式行，只存例外）
-- ---------------------------------------------------------------------------
-- ⚠️ **只存例外**：常态工作日 / 常态周末不落行。读端点无显式行时按星期推导并返回
-- `derived = true`（见文件头「日历模型」段）。
CREATE TABLE public.t_hr_calendar_day (
    id             bigint NOT NULL,
    cal_date       date NOT NULL,
    -- 日型：WORKDAY / MAKEUP_WORKDAY / REST_DAY / STATUTORY_HOLIDAY /
    -- MAKEUP_HOLIDAY / CUSTOM_LEAVE（无 DB ENUM，Rust 侧枚举；未知值 → 21905）。
    day_type       character varying(24) NOT NULL,
    -- 节假日名 / 休息日说明（如「国庆节」「春节」）。
    -- ⚠️ `CUSTOM_LEAVE` 时**必填**（service 校验，缺失 → 21907 BIZ_HR_CALENDAR_LEAVE_NAME_REQUIRED）。
    name           character varying(50),
    remark         character varying(200),
    -- 乐观锁版本号；每次 UPDATE 自增；0 行命中 → 40901 VERSION_CONFLICT
    version        integer NOT NULL DEFAULT 0,

    created_at     timestamp without time zone DEFAULT now() NOT NULL,
    created_by     bigint,
    updated_at     timestamp without time zone DEFAULT now() NOT NULL,
    updated_by     bigint,
    deleted_at     timestamp without time zone
);

COMMENT ON TABLE  public.t_hr_calendar_day IS '人事排班：全局日历显式行（只存例外，常态按星期推导）；读端点 derived=true 表示该日无显式行';
COMMENT ON COLUMN public.t_hr_calendar_day.id IS '雪花主键，App 侧生成；出参按 JSON string 序列化';
COMMENT ON COLUMN public.t_hr_calendar_day.cal_date IS '日历日；未软删行内唯一（uk_t_hr_calendar_day_date）';
COMMENT ON COLUMN public.t_hr_calendar_day.day_type IS '日型：WORKDAY / MAKEUP_WORKDAY / REST_DAY / STATUTORY_HOLIDAY / MAKEUP_HOLIDAY / CUSTOM_LEAVE（无 DB ENUM）；is_workday = WORKDAY ∪ MAKEUP_WORKDAY；未知值 → 21905';
COMMENT ON COLUMN public.t_hr_calendar_day.name IS '节假日名 / 休息日说明（varchar(50)）；day_type=CUSTOM_LEAVE 时必填（21907）';
COMMENT ON COLUMN public.t_hr_calendar_day.remark IS '备注（varchar(200)）；不进出参，仅管理端留痕';
COMMENT ON COLUMN public.t_hr_calendar_day.version IS '乐观锁版本号；批量 upsert 的 DO UPDATE 自增；删行（软删）同样自增';
COMMENT ON COLUMN public.t_hr_calendar_day.deleted_at IS '软删标记；删行后该日回退到「按星期推导」（POST /calendar/days/delete）';
COMMENT ON COLUMN public.t_hr_calendar_day.created_at IS '行写入时刻（timestamp，无时区；全仓统一按 Asia/Shanghai 解读）';
COMMENT ON COLUMN public.t_hr_calendar_day.created_by IS '建行人（t_user.id，裸 bigint 无外键；审计字段，不参与任何业务判定）';
COMMENT ON COLUMN public.t_hr_calendar_day.updated_at IS '最近一次写入（含 upsert 的 DO UPDATE 与软删）的时刻';
COMMENT ON COLUMN public.t_hr_calendar_day.updated_by IS '最近一次写入人（t_user.id）；与 created_by 相等说明这一行建完没被改过';

ALTER TABLE ONLY public.t_hr_calendar_day ADD CONSTRAINT t_hr_calendar_day_pkey PRIMARY KEY (id);

-- ⚠️ partial unique：软删后索引项消失 ⇒ 同一天可以重新显式登记一次。
-- ⚠️ **刻意不再另建 `(cal_date)` 全列索引**：区间读端点的谓词恒为
--   `deleted_at IS NULL AND cal_date BETWEEN $1 AND $2`，被蕴含于本 partial unique 的
--   谓词里 ⇒ PG 直接拿它走索引。同列再挂一条 `ix_` 只会多一份双份的写放大
--   （每次 upsert / 软删维护两棵同样的 B 树），读端点一行都用不上。
CREATE UNIQUE INDEX uk_t_hr_calendar_day_date
    ON public.t_hr_calendar_day (cal_date) WHERE deleted_at IS NULL;

-- ---------------------------------------------------------------------------
-- 2. 班次（时段内联 jsonb）
-- ---------------------------------------------------------------------------
-- ⚠️ **软删与停用是两种东西**：本表有 `deleted_at`（整行软删，保留给将来的历史考勤）与
-- `is_active`（停用，本期唯一的「下架」动作）。停用**不**软删，也不级联软删
-- `t_hr_shift_calendar` 的覆盖行（见文件头「覆盖行不做级联软删」）。
CREATE TABLE public.t_hr_shift (
    id             bigint NOT NULL,
    version        integer NOT NULL DEFAULT 0,
    -- 班次编码（如「DAY」「NIGHT」）；未软删行内唯一，重复 → 21902（409，**不是** 40901）。
    code           character varying(30) NOT NULL,
    name           character varying(50) NOT NULL,
    -- 排班表 / 甘特图上的显示色（前端色板值，varchar(20)）；可 NULL。
    color          character varying(20),
    -- 时段数组（jsonb）：[{"seq":1,"start":"22:00","end":"26:00"}]。
    -- ⚠️ 绝对时钟轴，`HH ∈ 0..48`；`span ≤ "24:00"` 由 service 校验（V6 → 21908）。
    -- 取舍论证见文件头「为什么用 jsonb」段。
    segments       jsonb NOT NULL,
    is_active      boolean NOT NULL DEFAULT true,
    sort_order     integer NOT NULL DEFAULT 0,
    remark         character varying(200),

    created_at     timestamp without time zone DEFAULT now() NOT NULL,
    created_by     bigint,
    updated_at     timestamp without time zone DEFAULT now() NOT NULL,
    updated_by     bigint,
    deleted_at     timestamp without time zone
);

COMMENT ON TABLE  public.t_hr_shift IS '人事排班：班次（时段内联 jsonb，绝对时钟轴）；停用走 is_active=false，不级联软删班次覆盖行';
COMMENT ON COLUMN public.t_hr_shift.id IS '雪花主键，App 侧生成；出参按 JSON string 序列化';
COMMENT ON COLUMN public.t_hr_shift.version IS '乐观锁版本号；写路径带 WHERE version=$2，0 行 → 40901';
COMMENT ON COLUMN public.t_hr_shift.code IS '班次编码（varchar(30)）；未软删行内唯一，重复 → 21902（HTTP 409，不是 40901 —— 后者只表达 OCC）；空白 → 21909，超长 → 21909';
COMMENT ON COLUMN public.t_hr_shift.name IS '班次名（varchar(50)，排班表 / 甘特图上显示）；空白 → 21909，超长 → 21909（不是 500 —— PG 的 varchar 超长是 22001，落不进业务码）';
COMMENT ON COLUMN public.t_hr_shift.color IS '显示色（varchar(20)，前端色板值）；可 NULL，前端走自己的默认色；超长 → 21909';
COMMENT ON COLUMN public.t_hr_shift.remark IS '备注（varchar(200)）；可 NULL，不进出参仅管理端留痕；超长 → 21909（人事随手粘贴的自由文本，超长是日常操作）';
COMMENT ON COLUMN public.t_hr_shift.segments IS '时段数组 jsonb：[{"seq":1,"start":"22:00","end":"26:00"}]；绝对时钟轴 HH∈0..48，夜班 22:00→次日 02:00 写 26:00；1..6 段、start<end、首段 <24:00、升序不重叠、span≤24:00（违规见 ck 约束与 21903/21908）';
COMMENT ON COLUMN public.t_hr_shift.is_active IS '是否启用；停用后一切写操作（改 / 写覆盖行 / 清覆盖）→ 21906（HTTP 409）';
COMMENT ON COLUMN public.t_hr_shift.sort_order IS '列表 / 排班表展示序（升序）；同序时按 id ASC 兜底';
COMMENT ON COLUMN public.t_hr_shift.deleted_at IS '软删标记；被 t_hr_shift_calendar.shift_id 引用时**不**级联（保留历史考勤口径）';
COMMENT ON COLUMN public.t_hr_shift.created_at IS '行写入时刻（timestamp，无时区；全仓统一按 Asia/Shanghai 解读）';
COMMENT ON COLUMN public.t_hr_shift.created_by IS '建行人（t_user.id，裸 bigint 无外键；审计字段，不参与任何业务判定）';
COMMENT ON COLUMN public.t_hr_shift.updated_at IS '最近一次写入（改班次 / 停用）的时刻';
COMMENT ON COLUMN public.t_hr_shift.updated_by IS '最近一次写入人（t_user.id）；与 created_by 相等说明这一行建完没被改过';

ALTER TABLE ONLY public.t_hr_shift ADD CONSTRAINT t_hr_shift_pkey PRIMARY KEY (id);

-- ⚠️ partial unique：软删后可复用同一 code（审计行仍留存）。
CREATE UNIQUE INDEX uk_t_hr_shift_code
    ON public.t_hr_shift (code) WHERE deleted_at IS NULL;
-- 列表默认 `is_active ASC / sort_order ASC / id ASC`，停用行排在最后。
CREATE INDEX ix_t_hr_shift_listing
    ON public.t_hr_shift (is_active, sort_order, id);

-- ⚠️ **CASE WHEN 不能省**：jsonb_array_length() 对标量直接抛错（而不是返回 NULL），
--    裸写会让 `segments = 'null'` 的 INSERT 变成 SQL 内部错误 ⇒ 500 而非入参错误。
ALTER TABLE ONLY public.t_hr_shift
    ADD CONSTRAINT ck_t_hr_shift_segments CHECK (
        CASE WHEN jsonb_typeof(segments) = 'array'
             THEN jsonb_array_length(segments) BETWEEN 1 AND 6
             ELSE false
        END
    );

-- ---------------------------------------------------------------------------
-- 3. 班次日历覆盖行（只存与全局不同的那天）
-- ---------------------------------------------------------------------------
-- ⚠️ **与全局相同的覆盖行也是允许的**（允许「显式声明这天就是工作日」以覆盖将来对全局
--   日历的改动）；是否真的与全局不同由读端点的 `is_override` 表达（它表达的是
--   「有没有覆盖行」，不是「值是否相同」）。
-- ⚠️ **停用班次时这些行不级联软删**（见文件头）。
CREATE TABLE public.t_hr_shift_calendar (
    id             bigint NOT NULL,
    version        integer NOT NULL DEFAULT 0,
    -- 所属班次（t_hr_shift.id，**无物理外键**）；查不到 / 已软删 → 21901。
    shift_id       bigint NOT NULL,
    cal_date       date NOT NULL,
    -- 覆盖后的日型（6 档之一，无 DB ENUM）；未知值 → 21905。
    -- ⚠️ 与全局日历不同：本表**没有 name 列** —— 「这天为什么特殊」是这个班次的口径，
    --   全局日历的名字对它没有意义；要写说明走 remark。
    day_type       character varying(24) NOT NULL,
    remark         character varying(200),

    created_at     timestamp without time zone DEFAULT now() NOT NULL,
    created_by     bigint,
    updated_at     timestamp without time zone DEFAULT now() NOT NULL,
    updated_by     bigint,
    deleted_at     timestamp without time zone
);

COMMENT ON TABLE  public.t_hr_shift_calendar IS '人事排班：班次的单日日历覆盖行（优先于全局日历）；班次停用/软删时**不级联**本表，保留历史考勤口径';
COMMENT ON COLUMN public.t_hr_shift_calendar.shift_id IS '所属班次（t_hr_shift.id，无物理外键）；停用的班次写覆盖行 → 21906，不存在 / 已软删 → 21901';
COMMENT ON COLUMN public.t_hr_shift_calendar.cal_date IS '被覆盖的那一天；与 shift_id 组成未软删行内的唯一键（uk_t_hr_shift_calendar）';
COMMENT ON COLUMN public.t_hr_shift_calendar.day_type IS '覆盖后的日型（6 档之一，无 DB ENUM）；刻意无 name 列（名称属全局日历语义），要说明走 remark';
COMMENT ON COLUMN public.t_hr_shift_calendar.version IS '乐观锁版本号；批量 upsert 的 DO UPDATE 自增；清覆盖（软删）同样自增；批量写端点不做行级 OCC（客户端不锚这一列，见 hr.md §8.3 取舍 9）';
COMMENT ON COLUMN public.t_hr_shift_calendar.id IS '雪花主键，App 侧生成；出参按 JSON string 序列化（本表不直接出参，端点 8 只回 cal_date 与日型）';
COMMENT ON COLUMN public.t_hr_shift_calendar.remark IS '说明（本班次视角，varchar(200)）；可 NULL；超长 → 21909';
COMMENT ON COLUMN public.t_hr_shift_calendar.created_at IS '行写入时刻（timestamp，无时区；全仓统一按 Asia/Shanghai 解读）';
COMMENT ON COLUMN public.t_hr_shift_calendar.created_by IS '建行人（t_user.id，裸 bigint 无外键；审计字段，不参与任何业务判定）';
COMMENT ON COLUMN public.t_hr_shift_calendar.updated_at IS '最近一次写入（upsert 的 DO UPDATE 或清覆盖的软删）的时刻';
COMMENT ON COLUMN public.t_hr_shift_calendar.updated_by IS '最近一次写入人（t_user.id）；与 created_by 相等说明这一行建完没被改过';
COMMENT ON COLUMN public.t_hr_shift_calendar.deleted_at IS '软删标记；清掉后该日回退到全局日历；⚠️ 班次停用**不**级联软删本表（保留历史考勤口径）';

ALTER TABLE ONLY public.t_hr_shift_calendar ADD CONSTRAINT t_hr_shift_calendar_pkey PRIMARY KEY (id);

-- ⚠️ partial unique：一个班次同一天只能有一条覆盖行（防重复写同一天两种口径）。
-- ⚠️ **同样刻意不另建 `(shift_id, cal_date)` 全列索引**：班次区间读端点的谓词恒带
--   `deleted_at IS NULL`，被本 partial unique 的谓词蕴含 ⇒ 同列 `ix_` 只会多一份
--   写放大，读端点一行都用不上。
CREATE UNIQUE INDEX uk_t_hr_shift_calendar
    ON public.t_hr_shift_calendar (shift_id, cal_date) WHERE deleted_at IS NULL;
