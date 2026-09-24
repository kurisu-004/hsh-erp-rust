-- ============================================================================
-- Migration 001: BASELINE —— 全量 schema 基线（sqlx 接管点）
-- ============================================================================
--
-- 2026-09-25 生产重建：从 29 个分散 migration 合并为单文件 baseline。
--
-- 来源（旧文件已迁移到 migrations/archive/，仅历史参考，不再演进）：
--   001-014：建表 + 索引 + 改列
--   015：业务数据 backfill（schema 已在文件中创建 seq_t_part_batch_backfill）
--   016-029：列变更 + 新增表 + 索引
--
-- 菜单 DML（来自旧 018/021/023/024/025）已剥离，由 seeds/menu.sql 声明式接管。
--
-- ----------------------------------------------------------------------------
-- 表清单（按域分组，dump 按字母序输出）：
--   1. IAM：        t_user, t_user_role, t_menu, t_role_menu
--   2. Customer：   t_customer, t_applicant, t_serial_counter
--   3. Resource：   t_work_type, t_work_type_process, t_process,
--                   t_shelf, t_shelf_process, t_worker
--   4. Part (核心):  t_assembly, t_part, t_part_batch, t_part_event,
--                   t_part_file, t_part_process_chain, t_process_chain_step,
--                   t_pickup_skip_event
--   5. Delivery：   t_delivery_group, t_delivery_group_member,
--                   t_delivery_note, t_delivery_note_counter,
--                   t_delivery_note_event
--   6. Outsource：  t_outsource_company, t_outsource_company_process,
--                   t_outsource_quote, t_outsource_quote_event,
--                   t_outsource_shipment
--   7. File 遗留：  t_drawing_file, t_cnc_program
--   8. E2E：        t_e2e_seeded
-- ----------------------------------------------------------------------------
--
-- 校验：本文件应在空 DB 上跑出与 dump 时完全一致的 schema。
-- 重建生产：pg_restore prod backup → 跑 schema 部分 → TRUNCATE _sqlx_migrations
--           → INSERT 本文件 checksum → 跑 seeds/menu.sql。
--
-- 修改规范：本文件**不再修改**。所有 schema 变更走追加的新 migration。
-- ============================================================================

--
-- Name: seq_t_part_batch_backfill; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.seq_t_part_batch_backfill
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


SET default_tablespace = '';

SET default_table_access_method = heap;

--
-- Name: t_applicant; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.t_applicant (
    id bigint NOT NULL,
    name character varying(50) NOT NULL,
    customer_id bigint NOT NULL,
    version integer DEFAULT 0 NOT NULL,
    created_at timestamp without time zone DEFAULT now() NOT NULL,
    created_by bigint,
    updated_at timestamp without time zone DEFAULT now() NOT NULL,
    updated_by bigint,
    deleted_at timestamp without time zone
);


--
-- Name: COLUMN t_applicant.name; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_applicant.name IS '申请人姓名';


--
-- Name: COLUMN t_applicant.customer_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_applicant.customer_id IS '逻辑外键 → t_customer.id（一级客户）';


--
-- Name: COLUMN t_applicant.version; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_applicant.version IS '乐观锁版本号；每次 UPDATE 自增；冲突抛 BIZ_VERSION_CONFLICT 409';


--
-- Name: t_applicant_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.t_applicant_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: t_applicant_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.t_applicant_id_seq OWNED BY public.t_applicant.id;


--
-- Name: t_assembly; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.t_assembly (
    id bigint NOT NULL,
    drawing_no character varying(100) NOT NULL,
    name character varying(200) NOT NULL,
    applicant_name character varying(50),
    customer_id bigint NOT NULL,
    request_date date NOT NULL,
    planned_delivery_date date NOT NULL,
    is_urgent boolean DEFAULT false NOT NULL,
    status character varying(20) DEFAULT 'PENDING'::character varying NOT NULL,
    version integer DEFAULT 0 NOT NULL,
    created_at timestamp without time zone DEFAULT now() NOT NULL,
    created_by bigint,
    updated_at timestamp without time zone DEFAULT now() NOT NULL,
    updated_by bigint,
    deleted_at timestamp without time zone,
    serial_no character varying(15),
    quantity integer DEFAULT 1 NOT NULL,
    unit_price numeric(12,2) DEFAULT 0 NOT NULL,
    total_price numeric(14,2) DEFAULT 0 NOT NULL,
    order_no character varying(30),
    system_delivery_date date,
    note character varying(500)
);


--
-- Name: COLUMN t_assembly.drawing_no; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_assembly.drawing_no IS '总图图号（如 E42FX1020107101）';


--
-- Name: COLUMN t_assembly.name; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_assembly.name IS '装配体名称（如 精研挡料座）';


--
-- Name: COLUMN t_assembly.customer_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_assembly.customer_id IS '逻辑外键 → t_customer.id 叶子节点';


--
-- Name: COLUMN t_assembly.status; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_assembly.status IS 'PENDING（默认）/ IN_PROCESS / COMPLETED / CANCELLED';


--
-- Name: COLUMN t_assembly.version; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_assembly.version IS '乐观锁版本号；每次 UPDATE 自增；冲突抛 BIZ_VERSION_CONFLICT 409';


--
-- Name: COLUMN t_assembly.serial_no; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_assembly.serial_no IS '装配体序列号；子件派生为 ''{serial_no}-{i:02d}''';


--
-- Name: t_cnc_program; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.t_cnc_program (
    id bigint NOT NULL,
    part_id bigint NOT NULL,
    object_key character varying(500) NOT NULL,
    original_filename character varying(255) NOT NULL,
    file_size bigint NOT NULL,
    content_type character varying(100) NOT NULL,
    upload_status character varying(20) DEFAULT 'READY'::character varying NOT NULL,
    version integer DEFAULT 0 NOT NULL,
    created_at timestamp without time zone DEFAULT now() NOT NULL,
    created_by bigint,
    updated_at timestamp without time zone DEFAULT now() NOT NULL,
    updated_by bigint,
    deleted_at timestamp without time zone
);


--
-- Name: COLUMN t_cnc_program.version; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_cnc_program.version IS '乐观锁版本号；每次 UPDATE 自增；冲突抛 BIZ_VERSION_CONFLICT 409';


--
-- Name: t_cnc_program_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.t_cnc_program_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: t_cnc_program_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.t_cnc_program_id_seq OWNED BY public.t_cnc_program.id;


--
-- Name: t_customer; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.t_customer (
    id bigint NOT NULL,
    name character varying(100) NOT NULL,
    parent_id bigint,
    version integer DEFAULT 0 NOT NULL,
    created_at timestamp without time zone DEFAULT now() NOT NULL,
    created_by bigint,
    updated_at timestamp without time zone DEFAULT now() NOT NULL,
    updated_by bigint,
    deleted_at timestamp without time zone,
    serial_prefix character varying(1),
    CONSTRAINT ck_t_customer_no_self_parent CHECK (((parent_id IS NULL) OR (parent_id <> id))),
    CONSTRAINT ck_t_customer_serial_prefix_uppercase CHECK (((serial_prefix IS NULL) OR ((serial_prefix)::text ~ '^[A-Z]$'::text)))
);


--
-- Name: COLUMN t_customer.version; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_customer.version IS '乐观锁版本号；每次 UPDATE 自增；冲突抛 BIZ_VERSION_CONFLICT 409';


--
-- Name: COLUMN t_customer.serial_prefix; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_customer.serial_prefix IS '一级客户序列号前缀（A-Z）；叶子客户 NULL';


--
-- Name: t_delivery_group; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.t_delivery_group (
    id bigint NOT NULL,
    customer_id bigint NOT NULL,
    name character varying(100) NOT NULL,
    version integer DEFAULT 0 NOT NULL,
    created_at timestamp without time zone DEFAULT now() NOT NULL,
    created_by bigint,
    updated_at timestamp without time zone DEFAULT now() NOT NULL,
    updated_by bigint,
    deleted_at timestamp without time zone
);


--
-- Name: COLUMN t_delivery_group.customer_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_delivery_group.customer_id IS '逻辑外键 → t_customer.id（一级客户 / parent_id IS NULL）';


--
-- Name: COLUMN t_delivery_group.name; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_delivery_group.name IS '分组名（同 L1 下活跃内唯一）';


--
-- Name: COLUMN t_delivery_group.version; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_delivery_group.version IS '乐观锁版本号；每次 UPDATE 自增；冲突抛 BIZ_VERSION_CONFLICT 409';


--
-- Name: t_delivery_group_member; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.t_delivery_group_member (
    id bigint NOT NULL,
    group_id bigint NOT NULL,
    customer_id bigint NOT NULL,
    created_at timestamp without time zone DEFAULT now() NOT NULL,
    created_by bigint,
    deleted_at timestamp without time zone
);


--
-- Name: COLUMN t_delivery_group_member.group_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_delivery_group_member.group_id IS '逻辑外键 → t_delivery_group.id';


--
-- Name: COLUMN t_delivery_group_member.customer_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_delivery_group_member.customer_id IS '逻辑外键 → t_customer.id（必须是 group.customer_id 的直接 L2 子节点）';


--
-- Name: t_delivery_note; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.t_delivery_note (
    id bigint NOT NULL,
    delivery_note_no character varying(16) NOT NULL,
    customer_id bigint NOT NULL,
    status character varying(16) DEFAULT 'DRAFT'::character varying NOT NULL,
    submitted_at timestamp without time zone,
    picked_up_at timestamp without time zone,
    submitted_by bigint,
    picked_up_by bigint,
    driver_worker_id bigint,
    note character varying(500),
    version integer DEFAULT 0 NOT NULL,
    created_at timestamp without time zone DEFAULT now() NOT NULL,
    created_by bigint,
    updated_at timestamp without time zone DEFAULT now() NOT NULL,
    updated_by bigint,
    deleted_at timestamp without time zone,
    delivery_date date,
    delivery_group_id bigint,
    leaf_customer_id bigint,
    CONSTRAINT ck_t_delivery_note_scope_exclusive CHECK ((NOT ((delivery_group_id IS NOT NULL) AND (leaf_customer_id IS NOT NULL)))),
    CONSTRAINT ck_t_delivery_note_status CHECK (((status)::text = ANY (ARRAY[('DRAFT'::character varying)::text, ('SUBMITTED'::character varying)::text, ('PICKED_UP'::character varying)::text, ('ARCHIVED'::character varying)::text])))
);


--
-- Name: COLUMN t_delivery_note.delivery_note_no; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_delivery_note.delivery_note_no IS '单号，格式 DN-YYYYMMDD-NNNN（4 位数）；唯一约束见 uq_t_delivery_note_no_active';


--
-- Name: COLUMN t_delivery_note.customer_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_delivery_note.customer_id IS '逻辑外键 → t_customer.id；送货单的客户（叶子二级）';


--
-- Name: COLUMN t_delivery_note.status; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_delivery_note.status IS 'DRAFT / SUBMITTED / PICKED_UP / ARCHIVED';


--
-- Name: COLUMN t_delivery_note.submitted_by; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_delivery_note.submitted_by IS '提交人 t_user.id（MANAGER / CLERK）';


--
-- Name: COLUMN t_delivery_note.picked_up_by; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_delivery_note.picked_up_by IS '领取时登入账号 t_user.id（一般是 SHELF_ACCOUNT 等扫码台账号）';


--
-- Name: COLUMN t_delivery_note.driver_worker_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_delivery_note.driver_worker_id IS '司机 t_worker.id；必须是 work_type.code=''送货司机'' 的活跃工人';


--
-- Name: COLUMN t_delivery_note.note; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_delivery_note.note IS '备注';


--
-- Name: COLUMN t_delivery_note.version; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_delivery_note.version IS '乐观锁版本号；每次 UPDATE 自增；冲突抛 BIZ_VERSION_CONFLICT 409';


--
-- Name: COLUMN t_delivery_note.delivery_date; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_delivery_note.delivery_date IS '送货日期；默认 = 创建当天；DRAFT/SUBMITTED 可改';


--
-- Name: COLUMN t_delivery_note.delivery_group_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_delivery_note.delivery_group_id IS '逻辑外键 → t_delivery_group.id；非空 = 分组单（D1）';


--
-- Name: COLUMN t_delivery_note.leaf_customer_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_delivery_note.leaf_customer_id IS '逻辑外键 → t_customer.id（叶子 L2）；非空 = 单厂单（D1）';


--
-- Name: t_delivery_note_counter; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.t_delivery_note_counter (
    date_ymd character varying(8) NOT NULL,
    last_value integer DEFAULT 0 NOT NULL,
    created_at timestamp without time zone DEFAULT now() NOT NULL,
    updated_at timestamp without time zone DEFAULT now() NOT NULL
);


--
-- Name: COLUMN t_delivery_note_counter.date_ymd; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_delivery_note_counter.date_ymd IS '自然日，格式 YYYYMMDD';


--
-- Name: COLUMN t_delivery_note_counter.last_value; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_delivery_note_counter.last_value IS '当日已发放的最大序列号（0 起；NNN 段从 1 开始）';


--
-- Name: t_delivery_note_event; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.t_delivery_note_event (
    id bigint NOT NULL,
    delivery_note_id bigint NOT NULL,
    event_type character varying(32) NOT NULL,
    from_status character varying(16),
    to_status character varying(16),
    note character varying(500),
    created_by bigint,
    created_at timestamp without time zone DEFAULT now() NOT NULL
);


--
-- Name: COLUMN t_delivery_note_event.delivery_note_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_delivery_note_event.delivery_note_id IS '逻辑外键 → t_delivery_note.id';


--
-- Name: COLUMN t_delivery_note_event.event_type; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_delivery_note_event.event_type IS 'CREATED / EDITED / ITEM_ADDED / ITEM_REMOVED / SUBMITTED / RECALLED / PICKUP_SCANNED / PICKED_UP / ARCHIVED';


--
-- Name: COLUMN t_delivery_note_event.note; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_delivery_note_event.note IS '事件备注 / 扩展元数据';


--
-- Name: COLUMN t_delivery_note_event.created_by; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_delivery_note_event.created_by IS '操作用户 t_user.id';


--
-- Name: t_delivery_note_event_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.t_delivery_note_event_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: t_delivery_note_event_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.t_delivery_note_event_id_seq OWNED BY public.t_delivery_note_event.id;


--
-- Name: t_delivery_note_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.t_delivery_note_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: t_delivery_note_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.t_delivery_note_id_seq OWNED BY public.t_delivery_note.id;


--
-- Name: t_drawing_file; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.t_drawing_file (
    id bigint NOT NULL,
    part_id bigint NOT NULL,
    object_key character varying(500) NOT NULL,
    original_filename character varying(255) NOT NULL,
    file_size bigint NOT NULL,
    content_type character varying(100) NOT NULL,
    upload_status character varying(20) DEFAULT 'READY'::character varying NOT NULL,
    version integer DEFAULT 0 NOT NULL,
    created_at timestamp without time zone DEFAULT now() NOT NULL,
    created_by bigint,
    updated_at timestamp without time zone DEFAULT now() NOT NULL,
    updated_by bigint,
    deleted_at timestamp without time zone
);


--
-- Name: COLUMN t_drawing_file.version; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_drawing_file.version IS '乐观锁版本号；每次 UPDATE 自增；冲突抛 BIZ_VERSION_CONFLICT 409';


--
-- Name: t_drawing_file_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.t_drawing_file_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: t_drawing_file_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.t_drawing_file_id_seq OWNED BY public.t_drawing_file.id;


--
-- Name: t_e2e_seeded; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.t_e2e_seeded (
    entity character varying(32) NOT NULL,
    entity_id bigint NOT NULL,
    seeded_at timestamp without time zone DEFAULT now() NOT NULL
);


--
-- Name: TABLE t_e2e_seeded; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON TABLE public.t_e2e_seeded IS '_e2e 模块 seed/reset 元数据：哪些行是 _e2e 灌入的';


--
-- Name: COLUMN t_e2e_seeded.entity; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_e2e_seeded.entity IS '业务实体类型，与路由段同名（customer/applicant/...）';


--
-- Name: COLUMN t_e2e_seeded.entity_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_e2e_seeded.entity_id IS '业务表雪花主键；reset 时按 (entity, entity_id) 反查';


--
-- Name: COLUMN t_e2e_seeded.seeded_at; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_e2e_seeded.seeded_at IS 'seed 时间，仅供调试观察';


--
-- Name: t_menu; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.t_menu (
    id bigint NOT NULL,
    parent_id bigint,
    code character varying(64) NOT NULL,
    title character varying(50) NOT NULL,
    path character varying(200),
    icon character varying(50),
    sort_order integer DEFAULT 0 NOT NULL,
    is_active boolean DEFAULT true NOT NULL,
    version integer DEFAULT 0 NOT NULL,
    created_at timestamp without time zone DEFAULT now() NOT NULL,
    created_by bigint,
    updated_at timestamp without time zone DEFAULT now() NOT NULL,
    updated_by bigint,
    deleted_at timestamp without time zone,
    CONSTRAINT ck_t_menu_no_self_loop CHECK (((parent_id IS NULL) OR (parent_id <> id)))
);


--
-- Name: COLUMN t_menu.version; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_menu.version IS '乐观锁版本号；每次 UPDATE 自增；冲突抛 BIZ_VERSION_CONFLICT 409';


--
-- Name: t_menu_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.t_menu_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: t_menu_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.t_menu_id_seq OWNED BY public.t_menu.id;


--
-- Name: t_outsource_company; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.t_outsource_company (
    id bigint NOT NULL,
    name character varying(100) NOT NULL,
    contact_name character varying(50),
    contact_phone character varying(50),
    address character varying(200),
    is_active boolean DEFAULT true NOT NULL,
    version integer DEFAULT 0 NOT NULL,
    created_at timestamp without time zone DEFAULT now() NOT NULL,
    created_by bigint,
    updated_at timestamp without time zone DEFAULT now() NOT NULL,
    updated_by bigint,
    deleted_at timestamp without time zone
);


--
-- Name: COLUMN t_outsource_company.name; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_outsource_company.name IS '外协公司名';


--
-- Name: COLUMN t_outsource_company.contact_name; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_outsource_company.contact_name IS '联系人';


--
-- Name: COLUMN t_outsource_company.contact_phone; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_outsource_company.contact_phone IS '联系电话';


--
-- Name: COLUMN t_outsource_company.address; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_outsource_company.address IS '地址';


--
-- Name: COLUMN t_outsource_company.is_active; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_outsource_company.is_active IS '是否启用（停用后下拉不再展示）';


--
-- Name: COLUMN t_outsource_company.version; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_outsource_company.version IS '乐观锁版本号；每次 UPDATE 自增；冲突抛 BIZ_VERSION_CONFLICT 409';


--
-- Name: t_outsource_company_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.t_outsource_company_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: t_outsource_company_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.t_outsource_company_id_seq OWNED BY public.t_outsource_company.id;


--
-- Name: t_outsource_company_process; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.t_outsource_company_process (
    id bigint NOT NULL,
    outsource_company_id bigint NOT NULL,
    process_id bigint NOT NULL,
    sort_order integer DEFAULT 0 NOT NULL,
    version integer DEFAULT 0 NOT NULL,
    created_at timestamp without time zone DEFAULT now() NOT NULL,
    created_by bigint,
    updated_at timestamp without time zone DEFAULT now() NOT NULL,
    updated_by bigint,
    deleted_at timestamp without time zone,
    CONSTRAINT ck_t_outsource_company_process_no_self_loop CHECK ((outsource_company_id <> process_id))
);


--
-- Name: COLUMN t_outsource_company_process.outsource_company_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_outsource_company_process.outsource_company_id IS '逻辑外键 → t_outsource_company.id';


--
-- Name: COLUMN t_outsource_company_process.process_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_outsource_company_process.process_id IS '逻辑外键 → t_process.id（通常 category=OUTSOURCE）';


--
-- Name: COLUMN t_outsource_company_process.sort_order; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_outsource_company_process.sort_order IS '工序在该公司能力清单内的显示顺序';


--
-- Name: COLUMN t_outsource_company_process.version; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_outsource_company_process.version IS '乐观锁版本号；每次 UPDATE 自增；冲突抛 BIZ_VERSION_CONFLICT 409';


--
-- Name: t_outsource_company_process_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.t_outsource_company_process_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: t_outsource_company_process_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.t_outsource_company_process_id_seq OWNED BY public.t_outsource_company_process.id;


--
-- Name: t_outsource_quote; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.t_outsource_quote (
    id bigint NOT NULL,
    part_id bigint NOT NULL,
    outsource_company_id bigint NOT NULL,
    process_id bigint NOT NULL,
    price numeric(12,2) NOT NULL,
    note character varying(500),
    status character varying(16) DEFAULT 'DRAFT'::character varying NOT NULL,
    submitted_at timestamp without time zone,
    reviewed_at timestamp without time zone,
    review_note character varying(500),
    version integer DEFAULT 0 NOT NULL,
    created_at timestamp without time zone DEFAULT now() NOT NULL,
    created_by bigint,
    updated_at timestamp without time zone DEFAULT now() NOT NULL,
    updated_by bigint,
    deleted_at timestamp without time zone,
    sent_at timestamp without time zone,
    received_at timestamp without time zone,
    quantity integer,
    is_billed boolean DEFAULT false NOT NULL,
    is_direct boolean DEFAULT false NOT NULL,
    CONSTRAINT ck_t_outsource_quote_price_positive CHECK ((price >= (0)::numeric)),
    CONSTRAINT ck_t_outsource_quote_status CHECK (((status)::text = ANY (ARRAY[('DRAFT'::character varying)::text, ('SUBMITTED'::character varying)::text, ('APPROVED'::character varying)::text, ('REJECTED'::character varying)::text, ('OUTSOURCING'::character varying)::text, ('RECEIVED'::character varying)::text, ('BILLED'::character varying)::text, ('USED'::character varying)::text])))
);


--
-- Name: COLUMN t_outsource_quote.part_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_outsource_quote.part_id IS '逻辑外键 → t_part.id';


--
-- Name: COLUMN t_outsource_quote.outsource_company_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_outsource_quote.outsource_company_id IS '逻辑外键 → t_outsource_company.id';


--
-- Name: COLUMN t_outsource_quote.process_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_outsource_quote.process_id IS '逻辑外键 → t_process.id（必须 OUTSOURCE 类别）';


--
-- Name: COLUMN t_outsource_quote.price; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_outsource_quote.price IS '单件单价（CNY）';


--
-- Name: COLUMN t_outsource_quote.note; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_outsource_quote.note IS '备注';


--
-- Name: COLUMN t_outsource_quote.status; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_outsource_quote.status IS 'DRAFT / SUBMITTED / APPROVED / REJECTED / USED';


--
-- Name: COLUMN t_outsource_quote.review_note; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_outsource_quote.review_note IS '审批意见（reject 必填）';


--
-- Name: COLUMN t_outsource_quote.version; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_outsource_quote.version IS '乐观锁版本号；每次 UPDATE 自增；冲突抛 BIZ_VERSION_CONFLICT 409';


--
-- Name: COLUMN t_outsource_quote.sent_at; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_outsource_quote.sent_at IS '发送时间（send_to_outsource 触发时写入）';


--
-- Name: COLUMN t_outsource_quote.received_at; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_outsource_quote.received_at IS '接收时间（receive_from_outsource 触发时写入）';


--
-- Name: COLUMN t_outsource_quote.quantity; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_outsource_quote.quantity IS '本次发送数量 snapshot；可能与 t_part.quantity 不同';


--
-- Name: COLUMN t_outsource_quote.is_billed; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_outsource_quote.is_billed IS '对账标记（与状态 RECEIVED/BILLED 配套）';


--
-- Name: t_outsource_quote_event; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.t_outsource_quote_event (
    id bigint NOT NULL,
    quote_id bigint NOT NULL,
    event_type character varying(32) NOT NULL,
    from_status character varying(16),
    to_status character varying(16),
    note character varying(500),
    created_by bigint,
    created_at timestamp without time zone DEFAULT now() NOT NULL
);


--
-- Name: COLUMN t_outsource_quote_event.quote_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_outsource_quote_event.quote_id IS '逻辑外键 → t_outsource_quote.id';


--
-- Name: COLUMN t_outsource_quote_event.event_type; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_outsource_quote_event.event_type IS 'CREATED / EDITED / SUBMITTED / APPROVED / REJECTED / USED';


--
-- Name: COLUMN t_outsource_quote_event.from_status; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_outsource_quote_event.from_status IS '状态机前态';


--
-- Name: COLUMN t_outsource_quote_event.to_status; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_outsource_quote_event.to_status IS '状态机后态';


--
-- Name: COLUMN t_outsource_quote_event.note; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_outsource_quote_event.note IS '事件备注';


--
-- Name: COLUMN t_outsource_quote_event.created_by; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_outsource_quote_event.created_by IS '操作人 user id';


--
-- Name: t_outsource_quote_event_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.t_outsource_quote_event_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: t_outsource_quote_event_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.t_outsource_quote_event_id_seq OWNED BY public.t_outsource_quote_event.id;


--
-- Name: t_outsource_quote_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.t_outsource_quote_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: t_outsource_quote_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.t_outsource_quote_id_seq OWNED BY public.t_outsource_quote.id;


--
-- Name: t_outsource_shipment; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.t_outsource_shipment (
    id bigint NOT NULL,
    quote_id bigint NOT NULL,
    part_id bigint NOT NULL,
    batch_id bigint,
    outsource_company_id bigint NOT NULL,
    process_id bigint NOT NULL,
    quantity integer NOT NULL,
    unit_price numeric(12,2) NOT NULL,
    status character varying(16) DEFAULT 'OUTSOURCING'::character varying NOT NULL,
    sent_at timestamp without time zone NOT NULL,
    received_at timestamp without time zone,
    is_billed boolean DEFAULT false NOT NULL,
    version integer DEFAULT 0 NOT NULL,
    created_at timestamp without time zone DEFAULT now() NOT NULL,
    created_by bigint,
    updated_at timestamp without time zone DEFAULT now() NOT NULL,
    updated_by bigint,
    deleted_at timestamp without time zone,
    CONSTRAINT ck_t_outsource_shipment_quantity_positive CHECK ((quantity > 0)),
    CONSTRAINT ck_t_outsource_shipment_status CHECK (((status)::text = ANY (ARRAY[('OUTSOURCING'::character varying)::text, ('RECEIVED'::character varying)::text, ('CANCELLED'::character varying)::text])))
);


--
-- Name: t_outsource_shipment_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.t_outsource_shipment_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: t_outsource_shipment_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.t_outsource_shipment_id_seq OWNED BY public.t_outsource_shipment.id;


--
-- Name: t_part; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.t_part (
    id bigint NOT NULL,
    serial_no character varying(15),
    name character varying(200) NOT NULL,
    drawing_no character varying(100) NOT NULL,
    applicant_name character varying(50) NOT NULL,
    quantity integer DEFAULT 1 NOT NULL,
    unit_price numeric(12,2) DEFAULT 0 NOT NULL,
    total_price numeric(14,2) DEFAULT 0 NOT NULL,
    request_date date NOT NULL,
    planned_delivery_date date NOT NULL,
    status character varying(20) DEFAULT 'PENDING'::character varying NOT NULL,
    is_urgent boolean DEFAULT false NOT NULL,
    next_process_id bigint,
    customer_id bigint NOT NULL,
    assembly_id bigint,
    version integer DEFAULT 0 NOT NULL,
    created_at timestamp without time zone DEFAULT now() NOT NULL,
    created_by bigint,
    updated_at timestamp without time zone DEFAULT now() NOT NULL,
    updated_by bigint,
    deleted_at timestamp without time zone,
    order_no character varying(30),
    system_delivery_date date,
    note character varying(500),
    process_chain_id bigint
);


--
-- Name: COLUMN t_part.is_urgent; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_part.is_urgent IS '是否加急';


--
-- Name: COLUMN t_part.next_process_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_part.next_process_id IS '逻辑外键 → t_process.id；place_on_shelf / RETURNED 时更新';


--
-- Name: COLUMN t_part.assembly_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_part.assembly_id IS '逻辑外键 → t_assembly.id；NULL = 非装配件子件';


--
-- Name: COLUMN t_part.version; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_part.version IS '乐观锁版本号；每次 UPDATE 自增；冲突抛 BIZ_VERSION_CONFLICT 409';


--
-- Name: COLUMN t_part.process_chain_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_part.process_chain_id IS '逻辑 FK → t_part_process_chain.id（无物理 FK）；NULL = 未制定工艺链；活跃 part 间 1:1（uq_t_part_process_chain）';


--
-- Name: t_part_batch; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.t_part_batch (
    id bigint NOT NULL,
    part_id bigint NOT NULL,
    batch_no integer NOT NULL,
    quantity integer NOT NULL,
    status character varying(20) DEFAULT 'PENDING'::character varying NOT NULL,
    location character varying(20),
    current_holder_id bigint,
    delivery_note_id bigint,
    parent_batch_id bigint,
    version integer DEFAULT 0 NOT NULL,
    created_at timestamp without time zone DEFAULT now() NOT NULL,
    created_by bigint,
    updated_at timestamp without time zone DEFAULT now() NOT NULL,
    updated_by bigint,
    deleted_at timestamp without time zone,
    current_process_step_id bigint
);


--
-- Name: COLUMN t_part_batch.part_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_part_batch.part_id IS '逻辑外键 → t_part.id';


--
-- Name: COLUMN t_part_batch.batch_no; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_part_batch.batch_no IS '工单内批次序号（1 起）';


--
-- Name: COLUMN t_part_batch.quantity; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_part_batch.quantity IS '本批次数量';


--
-- Name: COLUMN t_part_batch.location; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_part_batch.location IS 'OFFICE / PRODUCTION_SHELF / WORKER / INSPECTION_SHELF / OUTSOURCE_COMPANY';


--
-- Name: COLUMN t_part_batch.current_holder_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_part_batch.current_holder_id IS '多态 holder：shelf/worker/outsource_company';


--
-- Name: COLUMN t_part_batch.delivery_note_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_part_batch.delivery_note_id IS '逻辑外键 → t_delivery_note.id';


--
-- Name: COLUMN t_part_batch.parent_batch_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_part_batch.parent_batch_id IS '拆分谱系：源批次 id；根批次 NULL';


--
-- Name: COLUMN t_part_batch.version; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_part_batch.version IS '乐观锁版本号；每次 UPDATE 自增；冲突抛 BIZ_VERSION_CONFLICT 409';


--
-- Name: COLUMN t_part_batch.current_process_step_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_part_batch.current_process_step_id IS '逻辑 FK → t_process_chain_step.id；batch 当前所处的工艺链步骤。NULL 表示批次尚未进入生产流（PENDING/PROGRAMMING）或所属 part 无工艺链或 step 已软删。';


--
-- Name: t_part_batch_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.t_part_batch_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: t_part_batch_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.t_part_batch_id_seq OWNED BY public.t_part_batch.id;


--
-- Name: t_part_event; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.t_part_event (
    id bigint NOT NULL,
    part_id bigint NOT NULL,
    worker_id bigint,
    event_type character varying(30) NOT NULL,
    from_status character varying(20),
    to_status character varying(20),
    drawing_code character varying(100),
    badge_code character varying(50),
    note character varying(500),
    created_at timestamp without time zone DEFAULT now() NOT NULL,
    created_by bigint,
    outsource_company_id bigint,
    batch_id bigint,
    quantity integer
);


--
-- Name: COLUMN t_part_event.created_by; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_part_event.created_by IS '操作者 t_user.id（NULL = 系统调度/历史数据）';


--
-- Name: COLUMN t_part_event.outsource_company_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_part_event.outsource_company_id IS 'SENT_TO_OUTSOURCE / RECEIVED_FROM_OUTSOURCE 时填入；外协对账按此列聚合';


--
-- Name: COLUMN t_part_event.batch_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_part_event.batch_id IS '逻辑外键 → t_part_batch.id；NULL = 工单级事件';


--
-- Name: COLUMN t_part_event.quantity; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_part_event.quantity IS '本次事件涉及的数量；NULL = 历史数据 / 不适用';


--
-- Name: t_part_event_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.t_part_event_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: t_part_event_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.t_part_event_id_seq OWNED BY public.t_part_event.id;


--
-- Name: t_part_file; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.t_part_file (
    id bigint NOT NULL,
    part_id bigint NOT NULL,
    kind character varying(20) NOT NULL,
    file_type character varying(20) NOT NULL,
    object_key character varying(500) NOT NULL,
    original_filename character varying(255) NOT NULL,
    file_size bigint NOT NULL,
    content_type character varying(100) NOT NULL,
    upload_status character varying(20) DEFAULT 'READY'::character varying NOT NULL,
    content_sha256 character(64),
    version integer DEFAULT 0 NOT NULL,
    created_at timestamp without time zone DEFAULT now() NOT NULL,
    created_by bigint,
    updated_at timestamp without time zone DEFAULT now() NOT NULL,
    updated_by bigint,
    deleted_at timestamp without time zone,
    paired_file_id bigint,
    CONSTRAINT ck_t_part_file_kind CHECK (((kind)::text = ANY (ARRAY[('DRAWING'::character varying)::text, ('3D_MODEL'::character varying)::text, ('G_CODE'::character varying)::text, ('SETUP_SHEET'::character varying)::text, ('ASSEMBLY_MASTER'::character varying)::text, ('CAD_2D'::character varying)::text])))
);


--
-- Name: COLUMN t_part_file.part_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_part_file.part_id IS 'polymorphic: t_part.id 或 t_assembly.id (kind=ASSEMBLY_MASTER)';


--
-- Name: COLUMN t_part_file.kind; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_part_file.kind IS 'DRAWING / 3D_MODEL / G_CODE / SETUP_SHEET / ASSEMBLY_MASTER / CAD_2D';


--
-- Name: COLUMN t_part_file.file_type; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_part_file.file_type IS '扩展名大写（PDF / STEP / NC / ...），与 kind 配套';


--
-- Name: COLUMN t_part_file.object_key; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_part_file.object_key IS 'COS 对象 key';


--
-- Name: COLUMN t_part_file.upload_status; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_part_file.upload_status IS 'PENDING / READY / FAILED';


--
-- Name: COLUMN t_part_file.content_sha256; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_part_file.content_sha256 IS 'SHA-256 hex of file bytes（去重用）；NULL = 未计算 / 历史记录';


--
-- Name: COLUMN t_part_file.version; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_part_file.version IS '乐观锁版本号；每次 UPDATE 自增；冲突抛 BIZ_VERSION_CONFLICT 409';


--
-- Name: COLUMN t_part_file.paired_file_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_part_file.paired_file_id IS '关联的配对文件ID（G_CODE <-> SETUP_SHEET 双向关联）';


--
-- Name: t_part_file_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.t_part_file_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: t_part_file_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.t_part_file_id_seq OWNED BY public.t_part_file.id;


--
-- Name: t_part_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.t_part_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: t_part_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.t_part_id_seq OWNED BY public.t_part.id;


--
-- Name: t_part_process_chain; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.t_part_process_chain (
    id bigint NOT NULL,
    name character varying(64) DEFAULT '默认工艺'::character varying NOT NULL,
    version integer DEFAULT 0 NOT NULL,
    note text,
    created_at timestamp without time zone DEFAULT now() NOT NULL,
    created_by bigint NOT NULL,
    updated_at timestamp without time zone DEFAULT now() NOT NULL,
    updated_by bigint NOT NULL,
    deleted_at timestamp without time zone
);


--
-- Name: COLUMN t_part_process_chain.version; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_part_process_chain.version IS '乐观锁版本号；每次 UPDATE 自增；冲突抛 40901 VERSION_CONFLICT';


--
-- Name: t_pickup_skip_event; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.t_pickup_skip_event (
    id bigint NOT NULL,
    worker_id bigint NOT NULL,
    part_id bigint NOT NULL,
    batch_id bigint NOT NULL,
    batch_no integer NOT NULL,
    part_serial_no character varying(100),
    shelf_id bigint NOT NULL,
    work_type_id bigint,
    quantity integer NOT NULL,
    part_planned_delivery_date date,
    skipped_earliest_date date,
    created_at timestamp without time zone DEFAULT now() NOT NULL
);


--
-- Name: COLUMN t_pickup_skip_event.worker_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_pickup_skip_event.worker_id IS '逻辑外键 → t_worker.id；触发跳序的工人';


--
-- Name: COLUMN t_pickup_skip_event.part_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_pickup_skip_event.part_id IS '逻辑外键 → t_part.id；本次实际领取的工单';


--
-- Name: COLUMN t_pickup_skip_event.batch_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_pickup_skip_event.batch_id IS '逻辑外键 → t_part_batch.id；记录实际领取批次（拆分后为新批次）';


--
-- Name: COLUMN t_pickup_skip_event.batch_no; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_pickup_skip_event.batch_no IS '快照：领取批次号';


--
-- Name: COLUMN t_pickup_skip_event.part_serial_no; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_pickup_skip_event.part_serial_no IS '快照：工单流水号（流水号会被释放复用，必须快照）';


--
-- Name: COLUMN t_pickup_skip_event.shelf_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_pickup_skip_event.shelf_id IS '取件货架 t_shelf.id';


--
-- Name: COLUMN t_pickup_skip_event.work_type_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_pickup_skip_event.work_type_id IS '工人当时工种 t_work_type.id 快照';


--
-- Name: COLUMN t_pickup_skip_event.quantity; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_pickup_skip_event.quantity IS '本次领取数量';


--
-- Name: COLUMN t_pickup_skip_event.part_planned_delivery_date; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_pickup_skip_event.part_planned_delivery_date IS '所取件计划交期；NULL 表示无交期';


--
-- Name: COLUMN t_pickup_skip_event.skipped_earliest_date; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_pickup_skip_event.skipped_earliest_date IS '被跳过的候选件中最早交期；NULL 表示无可比候选';


--
-- Name: t_pickup_skip_event_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.t_pickup_skip_event_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: t_pickup_skip_event_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.t_pickup_skip_event_id_seq OWNED BY public.t_pickup_skip_event.id;


--
-- Name: t_process; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.t_process (
    id bigint NOT NULL,
    code character varying(32) NOT NULL,
    name character varying(50) NOT NULL,
    category character varying(16) NOT NULL,
    sort_order integer DEFAULT 0 NOT NULL,
    description character varying(200),
    version integer DEFAULT 0 NOT NULL,
    created_at timestamp without time zone DEFAULT now() NOT NULL,
    created_by bigint,
    updated_at timestamp without time zone DEFAULT now() NOT NULL,
    updated_by bigint,
    deleted_at timestamp without time zone,
    requires_approval boolean DEFAULT true NOT NULL,
    color character varying(9),
    CONSTRAINT ck_t_process_category CHECK (((category)::text = ANY (ARRAY[('INHOUSE'::character varying)::text, ('OUTSOURCE'::character varying)::text])))
);


--
-- Name: COLUMN t_process.code; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_process.code IS '工序代码（业务唯一键，不可变）';


--
-- Name: COLUMN t_process.name; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_process.name IS '工序名称（前端显示）';


--
-- Name: COLUMN t_process.category; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_process.category IS 'INHOUSE 自产 / OUTSOURCE 外协';


--
-- Name: COLUMN t_process.sort_order; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_process.sort_order IS '显示顺序';


--
-- Name: COLUMN t_process.version; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_process.version IS '乐观锁版本号；每次 UPDATE 自增；冲突抛 BIZ_VERSION_CONFLICT 409';


--
-- Name: COLUMN t_process.requires_approval; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_process.requires_approval IS '外协工序是否需要报价审批';


--
-- Name: COLUMN t_process.color; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_process.color IS '前端工序卡片颜色（hex 含 alpha）；格式 #RRGGBBAA，9 字符。NULL = 未设置。';


--
-- Name: t_process_chain_step; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.t_process_chain_step (
    id bigint NOT NULL,
    chain_id bigint NOT NULL,
    sort_order integer NOT NULL,
    process_id bigint NOT NULL,
    estimated_minutes integer NOT NULL,
    version integer DEFAULT 0 NOT NULL,
    created_at timestamp without time zone DEFAULT now() NOT NULL,
    created_by bigint NOT NULL,
    updated_at timestamp without time zone DEFAULT now() NOT NULL,
    updated_by bigint NOT NULL,
    deleted_at timestamp without time zone,
    note text,
    CONSTRAINT t_process_chain_step_estimated_minutes_check CHECK ((estimated_minutes >= 0))
);


--
-- Name: COLUMN t_process_chain_step.version; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_process_chain_step.version IS '乐观锁版本号；每次 UPDATE 自增；冲突抛 40901 VERSION_CONFLICT';


--
-- Name: COLUMN t_process_chain_step.note; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_process_chain_step.note IS '单步备注；车间操作员参考（如"必须干燥 24h 后才能上 CNC"）。NULL = 无备注。';


--
-- Name: t_process_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.t_process_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: t_process_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.t_process_id_seq OWNED BY public.t_process.id;


--
-- Name: t_role_menu; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.t_role_menu (
    id bigint NOT NULL,
    role character varying(20) NOT NULL,
    menu_id bigint NOT NULL,
    version integer DEFAULT 0 NOT NULL,
    created_at timestamp without time zone DEFAULT now() NOT NULL,
    created_by bigint,
    updated_at timestamp without time zone DEFAULT now() NOT NULL,
    updated_by bigint,
    deleted_at timestamp without time zone
);


--
-- Name: COLUMN t_role_menu.version; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_role_menu.version IS '乐观锁版本号；每次 UPDATE 自增；冲突抛 BIZ_VERSION_CONFLICT 409';


--
-- Name: t_role_menu_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.t_role_menu_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: t_role_menu_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.t_role_menu_id_seq OWNED BY public.t_role_menu.id;


--
-- Name: t_serial_counter; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.t_serial_counter (
    prefix character varying(1) NOT NULL,
    counter bigint DEFAULT 0 NOT NULL,
    version integer DEFAULT 0 NOT NULL,
    created_at timestamp without time zone DEFAULT now() NOT NULL,
    created_by bigint,
    updated_at timestamp without time zone DEFAULT now() NOT NULL,
    updated_by bigint,
    deleted_at timestamp without time zone
);


--
-- Name: COLUMN t_serial_counter.version; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_serial_counter.version IS '乐观锁版本号；每次 UPDATE 自增；冲突抛 BIZ_VERSION_CONFLICT 409';


--
-- Name: t_shelf; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.t_shelf (
    id bigint NOT NULL,
    code character varying(32) NOT NULL,
    name character varying(100) NOT NULL,
    zone character varying(16) NOT NULL,
    location character varying(200),
    is_active boolean DEFAULT true NOT NULL,
    version integer DEFAULT 0 NOT NULL,
    created_at timestamp without time zone DEFAULT now() NOT NULL,
    created_by bigint,
    updated_at timestamp without time zone DEFAULT now() NOT NULL,
    updated_by bigint,
    deleted_at timestamp without time zone,
    display_order integer DEFAULT 0 NOT NULL
);


--
-- Name: COLUMN t_shelf.version; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_shelf.version IS '乐观锁版本号；每次 UPDATE 自增；冲突抛 BIZ_VERSION_CONFLICT 409';


--
-- Name: COLUMN t_shelf.display_order; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_shelf.display_order IS '物理顺序（0=未设置；manager 在 ShelfList 后台手填）';


--
-- Name: t_shelf_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.t_shelf_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: t_shelf_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.t_shelf_id_seq OWNED BY public.t_shelf.id;


--
-- Name: t_shelf_process; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.t_shelf_process (
    id bigint NOT NULL,
    shelf_id bigint NOT NULL,
    process_id bigint NOT NULL,
    sort_order integer DEFAULT 0 NOT NULL,
    version integer DEFAULT 0 NOT NULL,
    created_at timestamp without time zone DEFAULT now() NOT NULL,
    created_by bigint,
    updated_at timestamp without time zone DEFAULT now() NOT NULL,
    updated_by bigint,
    deleted_at timestamp without time zone,
    CONSTRAINT ck_t_shelf_process_no_self_loop CHECK ((shelf_id <> process_id))
);


--
-- Name: COLUMN t_shelf_process.shelf_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_shelf_process.shelf_id IS '逻辑外键 → t_shelf.id';


--
-- Name: COLUMN t_shelf_process.process_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_shelf_process.process_id IS '逻辑外键 → t_process.id';


--
-- Name: COLUMN t_shelf_process.sort_order; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_shelf_process.sort_order IS '工序在货架映射内的显示顺序';


--
-- Name: COLUMN t_shelf_process.version; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_shelf_process.version IS '乐观锁版本号；每次 UPDATE 自增；冲突抛 BIZ_VERSION_CONFLICT 409';


--
-- Name: t_shelf_process_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.t_shelf_process_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: t_shelf_process_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.t_shelf_process_id_seq OWNED BY public.t_shelf_process.id;


--
-- Name: t_user; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.t_user (
    id bigint NOT NULL,
    username character varying(50) NOT NULL,
    password_hash character varying(255) NOT NULL,
    full_name character varying(50) NOT NULL,
    phone character varying(20),
    is_active boolean DEFAULT true NOT NULL,
    last_login_at timestamp without time zone,
    version integer DEFAULT 0 NOT NULL,
    created_at timestamp without time zone DEFAULT now() NOT NULL,
    created_by bigint,
    updated_at timestamp without time zone DEFAULT now() NOT NULL,
    updated_by bigint,
    deleted_at timestamp without time zone,
    refresh_token_version integer DEFAULT 0 NOT NULL
);


--
-- Name: COLUMN t_user.version; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_user.version IS '乐观锁版本号；每次 UPDATE 自增；冲突抛 BIZ_VERSION_CONFLICT 409';


--
-- Name: COLUMN t_user.refresh_token_version; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_user.refresh_token_version IS 'refresh token 轮转计数器；每次成功 refresh 后 +1';


--
-- Name: t_user_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.t_user_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: t_user_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.t_user_id_seq OWNED BY public.t_user.id;


--
-- Name: t_user_role; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.t_user_role (
    id bigint NOT NULL,
    user_id bigint NOT NULL,
    role character varying(20) NOT NULL,
    scope_type character varying(20),
    scope_id bigint,
    version integer DEFAULT 0 NOT NULL,
    created_at timestamp without time zone DEFAULT now() NOT NULL,
    created_by bigint,
    updated_at timestamp without time zone DEFAULT now() NOT NULL,
    updated_by bigint,
    deleted_at timestamp without time zone
);


--
-- Name: COLUMN t_user_role.version; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_user_role.version IS '乐观锁版本号；每次 UPDATE 自增；冲突抛 BIZ_VERSION_CONFLICT 409';


--
-- Name: t_user_role_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.t_user_role_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: t_user_role_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.t_user_role_id_seq OWNED BY public.t_user_role.id;


--
-- Name: t_work_type; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.t_work_type (
    id bigint NOT NULL,
    code character varying(32) NOT NULL,
    name character varying(50) NOT NULL,
    description character varying(200),
    sort_order integer DEFAULT 0 NOT NULL,
    version integer DEFAULT 0 NOT NULL,
    created_at timestamp without time zone DEFAULT now() NOT NULL,
    created_by bigint,
    updated_at timestamp without time zone DEFAULT now() NOT NULL,
    updated_by bigint,
    deleted_at timestamp without time zone,
    max_held_batches integer,
    max_held_minutes integer
);


--
-- Name: COLUMN t_work_type.code; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_work_type.code IS '工种代码（业务唯一键，不可变）';


--
-- Name: COLUMN t_work_type.name; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_work_type.name IS '工种名称（前端显示）';


--
-- Name: COLUMN t_work_type.sort_order; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_work_type.sort_order IS '显示顺序';


--
-- Name: COLUMN t_work_type.version; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_work_type.version IS '乐观锁版本号；每次 UPDATE 自增；冲突抛 BIZ_VERSION_CONFLICT 409';


--
-- Name: COLUMN t_work_type.max_held_batches; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_work_type.max_held_batches IS '工种工人最多可同时持有批次数；NULL=不限';


--
-- Name: COLUMN t_work_type.max_held_minutes; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_work_type.max_held_minutes IS '工种按预估工时计算的最大持有分钟数；NULL=未设置，auto-allocate TIME 模式调用会触发 20703 BIZ_WORK_TYPE_MAX_HELD_MINUTES_NOT_SET';


--
-- Name: t_work_type_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.t_work_type_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: t_work_type_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.t_work_type_id_seq OWNED BY public.t_work_type.id;


--
-- Name: t_work_type_process; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.t_work_type_process (
    id bigint NOT NULL,
    work_type_id bigint NOT NULL,
    process_id bigint NOT NULL,
    sort_order integer DEFAULT 0 NOT NULL,
    version integer DEFAULT 0 NOT NULL,
    created_at timestamp without time zone DEFAULT now() NOT NULL,
    created_by bigint,
    updated_at timestamp without time zone DEFAULT now() NOT NULL,
    updated_by bigint,
    deleted_at timestamp without time zone,
    CONSTRAINT ck_t_work_type_process_no_self_loop CHECK ((work_type_id <> process_id))
);


--
-- Name: COLUMN t_work_type_process.work_type_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_work_type_process.work_type_id IS '逻辑外键 → t_work_type.id';


--
-- Name: COLUMN t_work_type_process.process_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_work_type_process.process_id IS '逻辑外键 → t_process.id';


--
-- Name: COLUMN t_work_type_process.sort_order; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_work_type_process.sort_order IS '工序在工种映射内的显示顺序';


--
-- Name: COLUMN t_work_type_process.version; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_work_type_process.version IS '乐观锁版本号；每次 UPDATE 自增；冲突抛 BIZ_VERSION_CONFLICT 409';


--
-- Name: t_work_type_process_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.t_work_type_process_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: t_work_type_process_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.t_work_type_process_id_seq OWNED BY public.t_work_type_process.id;


--
-- Name: t_worker; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.t_worker (
    id bigint NOT NULL,
    badge_code character varying(50) NOT NULL,
    name character varying(50) NOT NULL,
    id_card_no character varying(18),
    phone character varying(20),
    is_active boolean DEFAULT true NOT NULL,
    work_type_id bigint,
    version integer DEFAULT 0 NOT NULL,
    created_at timestamp without time zone DEFAULT now() NOT NULL,
    created_by bigint,
    updated_at timestamp without time zone DEFAULT now() NOT NULL,
    updated_by bigint,
    deleted_at timestamp without time zone
);


--
-- Name: COLUMN t_worker.id_card_no; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_worker.id_card_no IS '身份证号；与 id 组成联合唯一索引';


--
-- Name: COLUMN t_worker.phone; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_worker.phone IS '手机号';


--
-- Name: COLUMN t_worker.is_active; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_worker.is_active IS '是否在职';


--
-- Name: COLUMN t_worker.work_type_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_worker.work_type_id IS '逻辑外键 → t_work_type.id；NULL = 未分配工种';


--
-- Name: COLUMN t_worker.version; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.t_worker.version IS '乐观锁版本号；每次 UPDATE 自增；冲突抛 BIZ_VERSION_CONFLICT 409';


--
-- Name: t_worker_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.t_worker_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: t_worker_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.t_worker_id_seq OWNED BY public.t_worker.id;


--
-- Name: t_applicant id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_applicant ALTER COLUMN id SET DEFAULT nextval('public.t_applicant_id_seq'::regclass);


--
-- Name: t_cnc_program id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_cnc_program ALTER COLUMN id SET DEFAULT nextval('public.t_cnc_program_id_seq'::regclass);


--
-- Name: t_delivery_note id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_delivery_note ALTER COLUMN id SET DEFAULT nextval('public.t_delivery_note_id_seq'::regclass);


--
-- Name: t_delivery_note_event id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_delivery_note_event ALTER COLUMN id SET DEFAULT nextval('public.t_delivery_note_event_id_seq'::regclass);


--
-- Name: t_drawing_file id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_drawing_file ALTER COLUMN id SET DEFAULT nextval('public.t_drawing_file_id_seq'::regclass);


--
-- Name: t_menu id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_menu ALTER COLUMN id SET DEFAULT nextval('public.t_menu_id_seq'::regclass);


--
-- Name: t_outsource_company id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_outsource_company ALTER COLUMN id SET DEFAULT nextval('public.t_outsource_company_id_seq'::regclass);


--
-- Name: t_outsource_company_process id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_outsource_company_process ALTER COLUMN id SET DEFAULT nextval('public.t_outsource_company_process_id_seq'::regclass);


--
-- Name: t_outsource_quote id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_outsource_quote ALTER COLUMN id SET DEFAULT nextval('public.t_outsource_quote_id_seq'::regclass);


--
-- Name: t_outsource_quote_event id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_outsource_quote_event ALTER COLUMN id SET DEFAULT nextval('public.t_outsource_quote_event_id_seq'::regclass);


--
-- Name: t_outsource_shipment id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_outsource_shipment ALTER COLUMN id SET DEFAULT nextval('public.t_outsource_shipment_id_seq'::regclass);


--
-- Name: t_part id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_part ALTER COLUMN id SET DEFAULT nextval('public.t_part_id_seq'::regclass);


--
-- Name: t_part_batch id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_part_batch ALTER COLUMN id SET DEFAULT nextval('public.t_part_batch_id_seq'::regclass);


--
-- Name: t_part_event id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_part_event ALTER COLUMN id SET DEFAULT nextval('public.t_part_event_id_seq'::regclass);


--
-- Name: t_part_file id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_part_file ALTER COLUMN id SET DEFAULT nextval('public.t_part_file_id_seq'::regclass);


--
-- Name: t_pickup_skip_event id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_pickup_skip_event ALTER COLUMN id SET DEFAULT nextval('public.t_pickup_skip_event_id_seq'::regclass);


--
-- Name: t_process id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_process ALTER COLUMN id SET DEFAULT nextval('public.t_process_id_seq'::regclass);


--
-- Name: t_role_menu id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_role_menu ALTER COLUMN id SET DEFAULT nextval('public.t_role_menu_id_seq'::regclass);


--
-- Name: t_shelf id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_shelf ALTER COLUMN id SET DEFAULT nextval('public.t_shelf_id_seq'::regclass);


--
-- Name: t_shelf_process id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_shelf_process ALTER COLUMN id SET DEFAULT nextval('public.t_shelf_process_id_seq'::regclass);


--
-- Name: t_user id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_user ALTER COLUMN id SET DEFAULT nextval('public.t_user_id_seq'::regclass);


--
-- Name: t_user_role id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_user_role ALTER COLUMN id SET DEFAULT nextval('public.t_user_role_id_seq'::regclass);


--
-- Name: t_work_type id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_work_type ALTER COLUMN id SET DEFAULT nextval('public.t_work_type_id_seq'::regclass);


--
-- Name: t_work_type_process id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_work_type_process ALTER COLUMN id SET DEFAULT nextval('public.t_work_type_process_id_seq'::regclass);


--
-- Name: t_worker id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_worker ALTER COLUMN id SET DEFAULT nextval('public.t_worker_id_seq'::regclass);


--
-- Name: t_applicant t_applicant_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_applicant
    ADD CONSTRAINT t_applicant_pkey PRIMARY KEY (id);


--
-- Name: t_assembly t_assembly_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_assembly
    ADD CONSTRAINT t_assembly_pkey PRIMARY KEY (id);


--
-- Name: t_cnc_program t_cnc_program_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_cnc_program
    ADD CONSTRAINT t_cnc_program_pkey PRIMARY KEY (id);


--
-- Name: t_customer t_customer_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_customer
    ADD CONSTRAINT t_customer_pkey PRIMARY KEY (id);


--
-- Name: t_delivery_group_member t_delivery_group_member_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_delivery_group_member
    ADD CONSTRAINT t_delivery_group_member_pkey PRIMARY KEY (id);


--
-- Name: t_delivery_group t_delivery_group_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_delivery_group
    ADD CONSTRAINT t_delivery_group_pkey PRIMARY KEY (id);


--
-- Name: t_delivery_note_counter t_delivery_note_counter_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_delivery_note_counter
    ADD CONSTRAINT t_delivery_note_counter_pkey PRIMARY KEY (date_ymd);


--
-- Name: t_delivery_note_event t_delivery_note_event_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_delivery_note_event
    ADD CONSTRAINT t_delivery_note_event_pkey PRIMARY KEY (id);


--
-- Name: t_delivery_note t_delivery_note_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_delivery_note
    ADD CONSTRAINT t_delivery_note_pkey PRIMARY KEY (id);


--
-- Name: t_drawing_file t_drawing_file_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_drawing_file
    ADD CONSTRAINT t_drawing_file_pkey PRIMARY KEY (id);


--
-- Name: t_e2e_seeded t_e2e_seeded_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_e2e_seeded
    ADD CONSTRAINT t_e2e_seeded_pkey PRIMARY KEY (entity, entity_id);


--
-- Name: t_menu t_menu_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_menu
    ADD CONSTRAINT t_menu_pkey PRIMARY KEY (id);


--
-- Name: t_outsource_company t_outsource_company_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_outsource_company
    ADD CONSTRAINT t_outsource_company_pkey PRIMARY KEY (id);


--
-- Name: t_outsource_company_process t_outsource_company_process_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_outsource_company_process
    ADD CONSTRAINT t_outsource_company_process_pkey PRIMARY KEY (id);


--
-- Name: t_outsource_quote_event t_outsource_quote_event_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_outsource_quote_event
    ADD CONSTRAINT t_outsource_quote_event_pkey PRIMARY KEY (id);


--
-- Name: t_outsource_quote t_outsource_quote_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_outsource_quote
    ADD CONSTRAINT t_outsource_quote_pkey PRIMARY KEY (id);


--
-- Name: t_outsource_shipment t_outsource_shipment_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_outsource_shipment
    ADD CONSTRAINT t_outsource_shipment_pkey PRIMARY KEY (id);


--
-- Name: t_part_batch t_part_batch_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_part_batch
    ADD CONSTRAINT t_part_batch_pkey PRIMARY KEY (id);


--
-- Name: t_part_event t_part_event_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_part_event
    ADD CONSTRAINT t_part_event_pkey PRIMARY KEY (id);


--
-- Name: t_part_file t_part_file_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_part_file
    ADD CONSTRAINT t_part_file_pkey PRIMARY KEY (id);


--
-- Name: t_part t_part_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_part
    ADD CONSTRAINT t_part_pkey PRIMARY KEY (id);


--
-- Name: t_part_process_chain t_part_process_chain_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_part_process_chain
    ADD CONSTRAINT t_part_process_chain_pkey PRIMARY KEY (id);


--
-- Name: t_pickup_skip_event t_pickup_skip_event_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_pickup_skip_event
    ADD CONSTRAINT t_pickup_skip_event_pkey PRIMARY KEY (id);


--
-- Name: t_process_chain_step t_process_chain_step_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_process_chain_step
    ADD CONSTRAINT t_process_chain_step_pkey PRIMARY KEY (id);


--
-- Name: t_process t_process_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_process
    ADD CONSTRAINT t_process_pkey PRIMARY KEY (id);


--
-- Name: t_role_menu t_role_menu_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_role_menu
    ADD CONSTRAINT t_role_menu_pkey PRIMARY KEY (id);


--
-- Name: t_serial_counter t_serial_counter_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_serial_counter
    ADD CONSTRAINT t_serial_counter_pkey PRIMARY KEY (prefix);


--
-- Name: t_shelf t_shelf_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_shelf
    ADD CONSTRAINT t_shelf_pkey PRIMARY KEY (id);


--
-- Name: t_shelf_process t_shelf_process_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_shelf_process
    ADD CONSTRAINT t_shelf_process_pkey PRIMARY KEY (id);


--
-- Name: t_user t_user_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_user
    ADD CONSTRAINT t_user_pkey PRIMARY KEY (id);


--
-- Name: t_user_role t_user_role_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_user_role
    ADD CONSTRAINT t_user_role_pkey PRIMARY KEY (id);


--
-- Name: t_work_type t_work_type_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_work_type
    ADD CONSTRAINT t_work_type_pkey PRIMARY KEY (id);


--
-- Name: t_work_type_process t_work_type_process_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_work_type_process
    ADD CONSTRAINT t_work_type_process_pkey PRIMARY KEY (id);


--
-- Name: t_worker t_worker_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_worker
    ADD CONSTRAINT t_worker_pkey PRIMARY KEY (id);


--
-- Name: t_user_role uk_t_user_role_user_role_scope; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_user_role
    ADD CONSTRAINT uk_t_user_role_user_role_scope UNIQUE (user_id, role, scope_type, scope_id);


--
-- Name: t_part_batch uq_t_part_batch_part_no; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.t_part_batch
    ADD CONSTRAINT uq_t_part_batch_part_no UNIQUE (part_id, batch_no);


--
-- Name: ix_chain_step_chain; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_chain_step_chain ON public.t_process_chain_step USING btree (chain_id) WHERE (deleted_at IS NULL);


--
-- Name: ix_part_event_created_at; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_part_event_created_at ON public.t_part_event USING btree (created_at);


--
-- Name: ix_part_event_event_type; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_part_event_event_type ON public.t_part_event USING btree (event_type);


--
-- Name: ix_part_event_part_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_part_event_part_id ON public.t_part_event USING btree (part_id);


--
-- Name: ix_part_event_worker_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_part_event_worker_id ON public.t_part_event USING btree (worker_id);


--
-- Name: ix_part_process_chain_deleted; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_part_process_chain_deleted ON public.t_part_process_chain USING btree (deleted_at);


--
-- Name: ix_t_applicant_customer_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_applicant_customer_id ON public.t_applicant USING btree (customer_id);


--
-- Name: ix_t_applicant_deleted_at; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_applicant_deleted_at ON public.t_applicant USING btree (deleted_at);


--
-- Name: ix_t_applicant_name; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_applicant_name ON public.t_applicant USING btree (name);


--
-- Name: ix_t_assembly_customer_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_assembly_customer_id ON public.t_assembly USING btree (customer_id);


--
-- Name: ix_t_assembly_customer_status; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_assembly_customer_status ON public.t_assembly USING btree (customer_id, status);


--
-- Name: ix_t_assembly_deleted_at; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_assembly_deleted_at ON public.t_assembly USING btree (deleted_at);


--
-- Name: ix_t_assembly_drawing_no; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_assembly_drawing_no ON public.t_assembly USING btree (drawing_no);


--
-- Name: ix_t_assembly_name; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_assembly_name ON public.t_assembly USING btree (name);


--
-- Name: ix_t_assembly_order_no; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_assembly_order_no ON public.t_assembly USING btree (order_no);


--
-- Name: ix_t_assembly_planned_delivery; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_assembly_planned_delivery ON public.t_assembly USING btree (planned_delivery_date);


--
-- Name: ix_t_assembly_serial_no; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_assembly_serial_no ON public.t_assembly USING btree (serial_no);


--
-- Name: ix_t_assembly_status; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_assembly_status ON public.t_assembly USING btree (status);


--
-- Name: ix_t_customer_deleted_at; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_customer_deleted_at ON public.t_customer USING btree (deleted_at);


--
-- Name: ix_t_customer_name; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_customer_name ON public.t_customer USING btree (name);


--
-- Name: ix_t_customer_parent_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_customer_parent_id ON public.t_customer USING btree (parent_id);


--
-- Name: ix_t_delivery_group_customer_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_delivery_group_customer_id ON public.t_delivery_group USING btree (customer_id);


--
-- Name: ix_t_delivery_group_deleted_at; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_delivery_group_deleted_at ON public.t_delivery_group USING btree (deleted_at);


--
-- Name: ix_t_delivery_group_member_customer_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_delivery_group_member_customer_id ON public.t_delivery_group_member USING btree (customer_id);


--
-- Name: ix_t_delivery_group_member_group_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_delivery_group_member_group_id ON public.t_delivery_group_member USING btree (group_id);


--
-- Name: ix_t_delivery_note_customer_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_delivery_note_customer_id ON public.t_delivery_note USING btree (customer_id);


--
-- Name: ix_t_delivery_note_deleted_at; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_delivery_note_deleted_at ON public.t_delivery_note USING btree (deleted_at);


--
-- Name: ix_t_delivery_note_delivery_group_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_delivery_note_delivery_group_id ON public.t_delivery_note USING btree (delivery_group_id) WHERE (delivery_group_id IS NOT NULL);


--
-- Name: ix_t_delivery_note_event_created_at; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_delivery_note_event_created_at ON public.t_delivery_note_event USING btree (created_at);


--
-- Name: ix_t_delivery_note_event_note_created; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_delivery_note_event_note_created ON public.t_delivery_note_event USING btree (delivery_note_id, created_at);


--
-- Name: ix_t_delivery_note_event_note_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_delivery_note_event_note_id ON public.t_delivery_note_event USING btree (delivery_note_id);


--
-- Name: ix_t_delivery_note_leaf_customer_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_delivery_note_leaf_customer_id ON public.t_delivery_note USING btree (leaf_customer_id) WHERE (leaf_customer_id IS NOT NULL);


--
-- Name: ix_t_delivery_note_status; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_delivery_note_status ON public.t_delivery_note USING btree (status);


--
-- Name: ix_t_delivery_note_submitted_at; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_delivery_note_submitted_at ON public.t_delivery_note USING btree (submitted_at);


--
-- Name: ix_t_e2e_seeded_seeded_at; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_e2e_seeded_seeded_at ON public.t_e2e_seeded USING btree (seeded_at);


--
-- Name: ix_t_menu_deleted_at; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_menu_deleted_at ON public.t_menu USING btree (deleted_at);


--
-- Name: ix_t_menu_parent_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_menu_parent_id ON public.t_menu USING btree (parent_id);


--
-- Name: ix_t_outsource_company_deleted_at; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_outsource_company_deleted_at ON public.t_outsource_company USING btree (deleted_at);


--
-- Name: ix_t_outsource_company_name; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_outsource_company_name ON public.t_outsource_company USING btree (name);


--
-- Name: ix_t_outsource_company_process_company; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_outsource_company_process_company ON public.t_outsource_company_process USING btree (outsource_company_id);


--
-- Name: ix_t_outsource_company_process_deleted_at; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_outsource_company_process_deleted_at ON public.t_outsource_company_process USING btree (deleted_at);


--
-- Name: ix_t_outsource_company_process_process; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_outsource_company_process_process ON public.t_outsource_company_process USING btree (process_id);


--
-- Name: ix_t_outsource_quote_company_received_at; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_outsource_quote_company_received_at ON public.t_outsource_quote USING btree (outsource_company_id, received_at);


--
-- Name: ix_t_outsource_quote_company_sent_at; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_outsource_quote_company_sent_at ON public.t_outsource_quote USING btree (outsource_company_id, sent_at);


--
-- Name: ix_t_outsource_quote_deleted_at; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_outsource_quote_deleted_at ON public.t_outsource_quote USING btree (deleted_at);


--
-- Name: ix_t_outsource_quote_event_created_at; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_outsource_quote_event_created_at ON public.t_outsource_quote_event USING btree (created_at);


--
-- Name: ix_t_outsource_quote_event_quote_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_outsource_quote_event_quote_id ON public.t_outsource_quote_event USING btree (quote_id);


--
-- Name: ix_t_outsource_quote_outsource_company_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_outsource_quote_outsource_company_id ON public.t_outsource_quote USING btree (outsource_company_id);


--
-- Name: ix_t_outsource_quote_part_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_outsource_quote_part_id ON public.t_outsource_quote USING btree (part_id);


--
-- Name: ix_t_outsource_quote_process_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_outsource_quote_process_id ON public.t_outsource_quote USING btree (process_id);


--
-- Name: ix_t_outsource_quote_status; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_outsource_quote_status ON public.t_outsource_quote USING btree (status);


--
-- Name: ix_t_outsource_shipment_batch_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_outsource_shipment_batch_id ON public.t_outsource_shipment USING btree (batch_id);


--
-- Name: ix_t_outsource_shipment_outsource_company_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_outsource_shipment_outsource_company_id ON public.t_outsource_shipment USING btree (outsource_company_id);


--
-- Name: ix_t_outsource_shipment_part_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_outsource_shipment_part_id ON public.t_outsource_shipment USING btree (part_id);


--
-- Name: ix_t_outsource_shipment_process_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_outsource_shipment_process_id ON public.t_outsource_shipment USING btree (process_id);


--
-- Name: ix_t_outsource_shipment_quote_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_outsource_shipment_quote_id ON public.t_outsource_shipment USING btree (quote_id);


--
-- Name: ix_t_outsource_shipment_status; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_outsource_shipment_status ON public.t_outsource_shipment USING btree (status);


--
-- Name: ix_t_part_assembly_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_part_assembly_id ON public.t_part USING btree (assembly_id);


--
-- Name: ix_t_part_assembly_id_status; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_part_assembly_id_status ON public.t_part USING btree (assembly_id, status);


--
-- Name: ix_t_part_batch_current_holder_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_part_batch_current_holder_id ON public.t_part_batch USING btree (current_holder_id);


--
-- Name: ix_t_part_batch_current_step_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_part_batch_current_step_id ON public.t_part_batch USING btree (current_process_step_id) WHERE (current_process_step_id IS NOT NULL);


--
-- Name: ix_t_part_batch_deleted_at; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_part_batch_deleted_at ON public.t_part_batch USING btree (deleted_at);


--
-- Name: ix_t_part_batch_delivery_note_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_part_batch_delivery_note_id ON public.t_part_batch USING btree (delivery_note_id);


--
-- Name: ix_t_part_batch_holder_location; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_part_batch_holder_location ON public.t_part_batch USING btree (current_holder_id, location) WHERE (deleted_at IS NULL);


--
-- Name: ix_t_part_batch_location; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_part_batch_location ON public.t_part_batch USING btree (location);


--
-- Name: ix_t_part_batch_parent_batch_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_part_batch_parent_batch_id ON public.t_part_batch USING btree (parent_batch_id) WHERE (parent_batch_id IS NOT NULL);


--
-- Name: ix_t_part_batch_part_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_part_batch_part_id ON public.t_part_batch USING btree (part_id);


--
-- Name: ix_t_part_batch_status; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_part_batch_status ON public.t_part_batch USING btree (status);


--
-- Name: ix_t_part_batch_status_holder; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_part_batch_status_holder ON public.t_part_batch USING btree (status, current_holder_id);


--
-- Name: ix_t_part_customer_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_part_customer_id ON public.t_part USING btree (customer_id);


--
-- Name: ix_t_part_customer_status_delivery; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_part_customer_status_delivery ON public.t_part USING btree (customer_id, status, planned_delivery_date);


--
-- Name: ix_t_part_deleted_at; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_part_deleted_at ON public.t_part USING btree (deleted_at);


--
-- Name: ix_t_part_drawing_no; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_part_drawing_no ON public.t_part USING btree (drawing_no);


--
-- Name: ix_t_part_event_batch_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_part_event_batch_id ON public.t_part_event USING btree (batch_id) WHERE (batch_id IS NOT NULL);


--
-- Name: ix_t_part_event_outsource_company_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_part_event_outsource_company_id ON public.t_part_event USING btree (outsource_company_id) WHERE (outsource_company_id IS NOT NULL);


--
-- Name: ix_t_part_file_created_at; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_part_file_created_at ON public.t_part_file USING btree (created_at);


--
-- Name: ix_t_part_file_part_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_part_file_part_id ON public.t_part_file USING btree (part_id);


--
-- Name: ix_t_part_file_part_kind; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_part_file_part_kind ON public.t_part_file USING btree (part_id, kind);


--
-- Name: ix_t_part_is_urgent; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_part_is_urgent ON public.t_part USING btree (is_urgent);


--
-- Name: ix_t_part_name; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_part_name ON public.t_part USING btree (name);


--
-- Name: ix_t_part_next_process_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_part_next_process_id ON public.t_part USING btree (next_process_id);


--
-- Name: ix_t_part_order_no; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_part_order_no ON public.t_part USING btree (order_no);


--
-- Name: ix_t_part_planned_delivery_date; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_part_planned_delivery_date ON public.t_part USING btree (planned_delivery_date);


--
-- Name: ix_t_part_process_chain_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_part_process_chain_id ON public.t_part USING btree (process_chain_id) WHERE (process_chain_id IS NOT NULL);


--
-- Name: ix_t_part_request_date; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_part_request_date ON public.t_part USING btree (request_date);


--
-- Name: ix_t_part_status; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_part_status ON public.t_part USING btree (status);


--
-- Name: ix_t_part_system_delivery_date; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_part_system_delivery_date ON public.t_part USING btree (system_delivery_date);


--
-- Name: ix_t_pickup_skip_event_created_at; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_pickup_skip_event_created_at ON public.t_pickup_skip_event USING btree (created_at);


--
-- Name: ix_t_pickup_skip_event_worker_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_pickup_skip_event_worker_id ON public.t_pickup_skip_event USING btree (worker_id);


--
-- Name: ix_t_process_category; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_process_category ON public.t_process USING btree (category);


--
-- Name: ix_t_process_code; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_process_code ON public.t_process USING btree (code);


--
-- Name: ix_t_process_deleted_at; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_process_deleted_at ON public.t_process USING btree (deleted_at);


--
-- Name: ix_t_role_menu_deleted_at; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_role_menu_deleted_at ON public.t_role_menu USING btree (deleted_at);


--
-- Name: ix_t_role_menu_menu_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_role_menu_menu_id ON public.t_role_menu USING btree (menu_id);


--
-- Name: ix_t_role_menu_role; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_role_menu_role ON public.t_role_menu USING btree (role);


--
-- Name: ix_t_shelf_deleted_at; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_shelf_deleted_at ON public.t_shelf USING btree (deleted_at);


--
-- Name: ix_t_shelf_display_order; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_shelf_display_order ON public.t_shelf USING btree (display_order, code) WHERE (deleted_at IS NULL);


--
-- Name: ix_t_shelf_process_deleted_at; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_shelf_process_deleted_at ON public.t_shelf_process USING btree (deleted_at);


--
-- Name: ix_t_shelf_process_process; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_shelf_process_process ON public.t_shelf_process USING btree (process_id);


--
-- Name: ix_t_shelf_process_shelf; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_shelf_process_shelf ON public.t_shelf_process USING btree (shelf_id);


--
-- Name: ix_t_shelf_zone; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_shelf_zone ON public.t_shelf USING btree (zone);


--
-- Name: ix_t_user_deleted_at; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_user_deleted_at ON public.t_user USING btree (deleted_at);


--
-- Name: ix_t_user_role_deleted_at; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_user_role_deleted_at ON public.t_user_role USING btree (deleted_at);


--
-- Name: ix_t_user_role_scope; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_user_role_scope ON public.t_user_role USING btree (scope_type, scope_id);


--
-- Name: ix_t_user_role_user_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_user_role_user_id ON public.t_user_role USING btree (user_id);


--
-- Name: ix_t_work_type_code; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_work_type_code ON public.t_work_type USING btree (code);


--
-- Name: ix_t_work_type_deleted_at; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_work_type_deleted_at ON public.t_work_type USING btree (deleted_at);


--
-- Name: ix_t_work_type_process_deleted_at; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_work_type_process_deleted_at ON public.t_work_type_process USING btree (deleted_at);


--
-- Name: ix_t_work_type_process_process; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_work_type_process_process ON public.t_work_type_process USING btree (process_id);


--
-- Name: ix_t_work_type_process_work_type; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_work_type_process_work_type ON public.t_work_type_process USING btree (work_type_id);


--
-- Name: ix_t_worker_deleted_at; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_worker_deleted_at ON public.t_worker USING btree (deleted_at);


--
-- Name: ix_t_worker_name; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_worker_name ON public.t_worker USING btree (name);


--
-- Name: ix_t_worker_work_type_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ix_t_worker_work_type_id ON public.t_worker USING btree (work_type_id);


--
-- Name: uk_t_assembly_serial_no; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX uk_t_assembly_serial_no ON public.t_assembly USING btree (serial_no) WHERE ((deleted_at IS NULL) AND (serial_no IS NOT NULL));


--
-- Name: uk_t_menu_code; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX uk_t_menu_code ON public.t_menu USING btree (code) WHERE (deleted_at IS NULL);


--
-- Name: uk_t_outsource_company_name; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX uk_t_outsource_company_name ON public.t_outsource_company USING btree (name) WHERE (deleted_at IS NULL);


--
-- Name: uk_t_outsource_company_process; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX uk_t_outsource_company_process ON public.t_outsource_company_process USING btree (outsource_company_id, process_id) WHERE (deleted_at IS NULL);


--
-- Name: uk_t_part_file_part_kind_sha; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX uk_t_part_file_part_kind_sha ON public.t_part_file USING btree (part_id, kind, content_sha256) WHERE ((deleted_at IS NULL) AND (content_sha256 IS NOT NULL));


--
-- Name: uk_t_part_file_single; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX uk_t_part_file_single ON public.t_part_file USING btree (part_id, kind) WHERE ((deleted_at IS NULL) AND ((kind)::text = ANY (ARRAY[('DRAWING'::character varying)::text, ('3D_MODEL'::character varying)::text, ('ASSEMBLY_MASTER'::character varying)::text, ('CAD_2D'::character varying)::text])));


--
-- Name: uk_t_part_serial_no; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX uk_t_part_serial_no ON public.t_part USING btree (serial_no) WHERE (serial_no IS NOT NULL);


--
-- Name: uk_t_process_code; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX uk_t_process_code ON public.t_process USING btree (code) WHERE (deleted_at IS NULL);


--
-- Name: uk_t_role_menu_role_menu; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX uk_t_role_menu_role_menu ON public.t_role_menu USING btree (role, menu_id) WHERE (deleted_at IS NULL);


--
-- Name: uk_t_shelf_code; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX uk_t_shelf_code ON public.t_shelf USING btree (code) WHERE (deleted_at IS NULL);


--
-- Name: uk_t_shelf_process; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX uk_t_shelf_process ON public.t_shelf_process USING btree (shelf_id, process_id) WHERE (deleted_at IS NULL);


--
-- Name: uk_t_user_username; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX uk_t_user_username ON public.t_user USING btree (username) WHERE (deleted_at IS NULL);


--
-- Name: uk_t_work_type_code; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX uk_t_work_type_code ON public.t_work_type USING btree (code) WHERE (deleted_at IS NULL);


--
-- Name: uk_t_work_type_process; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX uk_t_work_type_process ON public.t_work_type_process USING btree (work_type_id, process_id) WHERE (deleted_at IS NULL);


--
-- Name: uk_t_worker_badge_code; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX uk_t_worker_badge_code ON public.t_worker USING btree (badge_code) WHERE (deleted_at IS NULL);


--
-- Name: uk_t_worker_id_card_no; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX uk_t_worker_id_card_no ON public.t_worker USING btree (id_card_no) WHERE (id_card_no IS NOT NULL);


--
-- Name: uq_chain_step_chain_order; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX uq_chain_step_chain_order ON public.t_process_chain_step USING btree (chain_id, sort_order) WHERE (deleted_at IS NULL);


--
-- Name: uq_t_applicant_name_customer_active; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX uq_t_applicant_name_customer_active ON public.t_applicant USING btree (name, customer_id) WHERE (deleted_at IS NULL);


--
-- Name: uq_t_customer_root_prefix; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX uq_t_customer_root_prefix ON public.t_customer USING btree (serial_prefix) WHERE ((deleted_at IS NULL) AND (parent_id IS NULL) AND (serial_prefix IS NOT NULL));


--
-- Name: uq_t_delivery_group_member_customer_active; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX uq_t_delivery_group_member_customer_active ON public.t_delivery_group_member USING btree (customer_id) WHERE (deleted_at IS NULL);


--
-- Name: uq_t_delivery_group_name_active; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX uq_t_delivery_group_name_active ON public.t_delivery_group USING btree (customer_id, name) WHERE (deleted_at IS NULL);


--
-- Name: uq_t_delivery_note_draft_group; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX uq_t_delivery_note_draft_group ON public.t_delivery_note USING btree (customer_id, delivery_group_id) WHERE ((deleted_at IS NULL) AND ((status)::text = 'DRAFT'::text) AND (delivery_group_id IS NOT NULL));


--
-- Name: uq_t_delivery_note_draft_leaf; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX uq_t_delivery_note_draft_leaf ON public.t_delivery_note USING btree (customer_id, leaf_customer_id) WHERE ((deleted_at IS NULL) AND ((status)::text = 'DRAFT'::text) AND (leaf_customer_id IS NOT NULL));


--
-- Name: uq_t_delivery_note_no_active; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX uq_t_delivery_note_no_active ON public.t_delivery_note USING btree (delivery_note_no) WHERE (deleted_at IS NULL);


--
-- Name: uq_t_outsource_quote_approved_part_process; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX uq_t_outsource_quote_approved_part_process ON public.t_outsource_quote USING btree (part_id, process_id) WHERE ((deleted_at IS NULL) AND ((status)::text = 'APPROVED'::text) AND (is_direct = false));


--
-- Name: uq_t_outsource_shipment_open_batch; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX uq_t_outsource_shipment_open_batch ON public.t_outsource_shipment USING btree (batch_id) WHERE ((deleted_at IS NULL) AND ((status)::text = 'OUTSOURCING'::text));


--
-- Name: uq_t_part_process_chain; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX uq_t_part_process_chain ON public.t_part USING btree (process_chain_id) WHERE ((process_chain_id IS NOT NULL) AND (deleted_at IS NULL));


--
--


