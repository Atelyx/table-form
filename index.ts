/**
 * 表格表单视图（官方表格视图插件）。
 *
 * 关键实现约束（宿主契约）：
 * - 新增行的稳定 id 由宿主生成且 `ctx.table.addRow()` 不返回，故以「新增前后行 id 集合差集」定位新行；
 *   差集不唯一（同一时刻其它窗口也在新增）时中止且不写任何值——宁可少写，不可写到错误的行上。
 * - 写入一律逐字段 `ctx.table.updateCell`（宿主为不可变逐格合并 + 逐格协作广播），
 *   故编辑只写用户实际改动过的字段，不覆盖协作者对同一行其它字段的改动。
 * - 图片字段没有「添加」通道（图片字节由宿主导入附件目录），表单内可移除已有图片的单元格引用
 *   （updateCell 写回过滤后的 images 数组，display 展示偏好保留）。
 *
 * 入口自包含（无运行时 import；`import type` 为类型注解，转译时擦除）；JSX 经转译引用
 * React.createElement（宿主提供 React 全局）；样式只用 inline style + CSS 变量，颜色/字阶/
 * 动效/圆角一律取宿主主题 token（Tailwind 类不可依赖，伪类不可写，交互态经组件状态驱动）。
 */
import type { Context } from "@atelyx/cordis";

interface Field {
  id: string;
  name: string;
  type: string;
  options?: string[];
}

interface Row {
  id: string;
  values: Record<string, unknown>;
}

interface TableSnapshot {
  tableFile: string | null;
  fields: Field[];
  rows: Row[];
  selectedRowId: string | null;
  peerColorByRowId: Record<string, string>;
}

interface ReactApi {
  useState<T>(init: T): [T, (value: T | ((prev: T) => T)) => void];
  useEffect(effect: () => void | (() => void), deps?: readonly unknown[]): void;
  useMemo<T>(fn: () => T, deps: readonly unknown[]): T;
  useRef<T>(init: T): { current: T };
  useCallback<T extends (...args: never[]) => unknown>(fn: T, deps: readonly unknown[]): T;

  useSyncExternalStore<T>(subscribe: (onStoreChange: () => void) => () => void, getSnapshot: () => T, getServerSnapshot: () => T): T;
  memo<T>(fn: T): T;
  Fragment: unknown;
  createElement: (type: unknown, props: Record<string, unknown> | null, ...children: unknown[]) => unknown;
}

interface AtelyxCtx extends Context {
  table: {
    snapshot(): TableSnapshot;
    addRow(): void;

    updateCell(rowId: string, fieldId: string, value: string | number | { images: string[]; display?: "grid" } | undefined): void;
    removeRow(rowId: string): void;
    selectRow(rowId: string | null): void;
    resolveImage(entry: string): Promise<string>;
  };
  state: {
    read(pluginId: string): Promise<unknown>;
    write(pluginId: string, data: unknown): Promise<void>;
  };
  notification: { notify(input: { message: string; level?: string }): string };
  events: { on(name: string, cb: (payload: unknown) => void): () => void };
  slots: {
    registerTableView(opts: { kind: string; label: string; component: unknown }): () => void;

    registerUi(opts: { slot: string; component: unknown; priority?: number }): () => void;
  };
}

const React = (globalThis as { React?: ReactApi }).React as ReactApi;

/** createElement 简写（经函数转发，把 React 全局的读取推迟到调用时；共享样式层的基元组件在模块层定义）。 */
function h(type: unknown, props: Record<string, unknown> | null, ...children: unknown[]): unknown {
  return React.createElement(type, props, ...children);
}

/** 插件 id（与 package.json 的 name 一致；`ctx.state` 读写按它隔离）。 */
const PLUGIN_ID = "com.atelyx.table-form";

const STATE_VERSION = 1;
/** 撤销本次新增的时间窗（毫秒）。 */
const UNDO_WINDOW_MS = 5000;

const ROW_PICKER_LIMIT = 50;
/** 有值的字段类型（图片字段不可填写，只做展示与移除引用）。 */
const FILLABLE_TYPES = ["text", "number", "duration", "singleSelect"];

/** 空表/空图常量：快照缺失时复用同一引用，避免下游 useMemo/useCallback 每次渲染失效。 */
const EMPTY_FIELDS: Field[] = [];
const EMPTY_ROWS: Row[] = [];
const EMPTY_PEER_COLORS: Record<string, string> = {};

interface FormFieldConfig {
  fieldId: string;
  hint?: string;
  selectStyle?: "radio" | "dropdown";
}
interface FormConfig {
  title: string;
  fields: FormFieldConfig[];
  lastSelectedRowId: string | null;
}

interface FormState {
  version: number;
  forms: Record<string, FormConfig>;
}

interface NewRowGuard {
  rowId: string;
  undoUntil: number;
  /** 新增行 values 的内容签名：窗口期内该行内容被改动（含协作写入）即不允许撤销删除。 */
  rowSignature: string;
}

function emptyConfig(): FormConfig {
  return { title: "", fields: [], lastSelectedRowId: null };
}

function rowLabel(row: Row, fields: Field[], index: number): string {
  const textField = fields.find((f) => f.type === "text");
  const raw = textField ? row.values[textField.id] : undefined;
  const text = typeof raw === "string" ? raw.replace(/\s+/g, " ").trim() : "";
  return text ? text.slice(0, 40) : "行 " + (index + 1);
}

function isFillable(field: Field): boolean {
  return FILLABLE_TYPES.indexOf(field.type) >= 0;
}

function createFieldConfig(field: Field | undefined, fieldId: string): FormFieldConfig {
  return { fieldId, selectStyle: field && field.type === "singleSelect" ? "radio" : undefined };
}

function toDraftValue(field: Field, value: unknown): string {
  if (value === undefined || value === null) return "";
  if (field.type === "number" || field.type === "duration") {
    return typeof value === "number" ? String(value) : "";
  }
  if (field.type === "singleSelect" || field.type === "text") {
    return typeof value === "string" ? value : "";
  }
  return "";
}

function toWriteValue(field: Field, raw: string): string | number | undefined {
  if (field.type === "number" || field.type === "duration") {
    if (raw.trim() === "") return undefined;
    const n = Number(raw);
    return Number.isFinite(n) ? n : undefined;
  }
  return raw === "" ? undefined : raw;
}

/** 目标行内容签名（撤销新增用：只序列化该行 values，不做全表序列化）。行不存在返回 ""。 */
function rowValuesSignature(row: Row | undefined): string {
  return row ? JSON.stringify(row.values) : "";
}

function asRecord(value: unknown): Record<string, unknown> | null {
  return typeof value === "object" && value !== null ? (value as Record<string, unknown>) : null;
}

function readFieldConfig(raw: unknown): FormFieldConfig | null {
  const record = asRecord(raw);
  if (!record || typeof record.fieldId !== "string" || record.fieldId === "") return null;
  const out: FormFieldConfig = { fieldId: record.fieldId };
  if (typeof record.hint === "string") out.hint = record.hint;
  if (record.selectStyle === "dropdown" || record.selectStyle === "radio") out.selectStyle = record.selectStyle;
  return out;
}

function readStateForms(raw: unknown): Record<string, FormConfig> {
  const root = asRecord(raw);
  const forms = root ? asRecord(root.forms) : null;
  const out: Record<string, FormConfig> = {};
  if (!forms) return out;
  for (const key of Object.keys(forms)) {
    const cfg = asRecord(forms[key]);
    if (!cfg) continue;
    const rawFields = Array.isArray(cfg.fields) ? cfg.fields : [];
    const fields: FormFieldConfig[] = [];
    for (const item of rawFields) {
      const parsed = readFieldConfig(item);
      if (parsed) fields.push(parsed);
    }
    out[key] = {
      title: typeof cfg.title === "string" ? cfg.title : "",
      fields,
      lastSelectedRowId: typeof cfg.lastSelectedRowId === "string" ? cfg.lastSelectedRowId : null,
    };
  }
  return out;
}

// ===== 共享样式（inline style + CSS 变量；宿主 Tailwind 类对插件无效）=====
// 全部颜色/字号/圆角/时长取宿主主题 token：浅深两套主题与应用级「字体大小」设置自动跟随。
// 内联样式写不了伪类（:hover/:focus），交互态一律经组件状态驱动。

const borderColor = "var(--border)";
const textMuted = "var(--text-muted)";
const textPrimary = "var(--text-primary)";
const textSecondary = "var(--text-secondary)";

type Style = Record<string, string | number>;

/** 状态变化动效：统一取宿主 fast 档时长与曲线。 */
const EASE = "var(--dur-fast) var(--ease)";

/** 字阶与行高（成对取用；rem 档，随应用「字体大小」设置缩放）。 */
const FS = { h2: "var(--fs-h2)", body: "var(--fs-body)", ui: "var(--fs-ui)", caption: "var(--fs-caption)", micro: "var(--fs-micro)" } as const;
const LH = { h2: "var(--lh-h2)", body: "var(--lh-body)", ui: "var(--lh-ui)", caption: "var(--lh-caption)", micro: "var(--lh-micro)" } as const;

/** 圆角刻度：xs 4 / sm 6 / md 10（胶囊用 9999 全圆）。 */
const RADIUS = { xs: "var(--radius-xs)", sm: "var(--radius-sm)", md: "var(--radius-md)" } as const;
const RADIUS_FULL = 9999;

// ===== 基元规格（对齐宿主 Button / IconButton / Input / Badge / StatusPill / EmptyState）=====

type ButtonVariant = "primary" | "secondary" | "ghost" | "subtle";
type ButtonSize = "sm" | "md";

const BUTTON_VARIANT: Record<ButtonVariant, { background: string; color: string; hoverBackground: string; hoverColor: string }> = {
  primary: { background: "var(--accent)", color: "var(--accent-fg)", hoverBackground: "var(--accent-hover)", hoverColor: "var(--accent-fg)" },
  secondary: { background: "var(--bg-tertiary)", color: "var(--text-primary)", hoverBackground: "var(--hover)", hoverColor: "var(--text-primary)" },
  ghost: { background: "transparent", color: textSecondary, hoverBackground: "var(--hover)", hoverColor: textPrimary },
  subtle: { background: "transparent", color: textMuted, hoverBackground: "transparent", hoverColor: textPrimary },
};

/** 控件高固定（sm 24 / md 28），字号变化不撑开布局。 */
const BUTTON_SIZE: Record<ButtonSize, Style> = {
  sm: { height: 24, padding: "0 8px", fontSize: FS.micro, lineHeight: LH.micro, gap: 4, borderRadius: RADIUS.sm },
  md: { height: 28, padding: "0 12px", fontSize: FS.ui, lineHeight: LH.ui, gap: 6, borderRadius: RADIUS.sm },
};

function buttonStyle(variant: ButtonVariant, size: ButtonSize, hovered: boolean): Style {
  const v = BUTTON_VARIANT[variant];
  return {
    display: "inline-flex",
    alignItems: "center",
    justifyContent: "center",
    flexShrink: 0,
    fontWeight: 500,
    whiteSpace: "nowrap",
    cursor: "pointer",
    border: "none",
    outline: "none",
    ...BUTTON_SIZE[size],
    background: hovered ? v.hoverBackground : v.background,
    color: hovered ? v.hoverColor : v.color,
    transition: "background " + EASE + ", color " + EASE + ", box-shadow " + EASE,
  };
}

/** 文字按钮（自带 hover 态；disabled 收敛为统一弱化）。 */
function Button(props: {
  variant?: ButtonVariant;
  size?: ButtonSize;
  disabled?: boolean;
  title?: string;
  style?: Style;
  onClick?: (e: unknown) => void;
  children?: unknown;
}) {
  const [hovered, setHovered] = React.useState(false);
  const [ring, ringProps] = useFocusRing();
  const variant = props.variant || "secondary";
  return h(
    "button",
    {
      type: "button",
      disabled: props.disabled,
      title: props.title,
      onClick: props.onClick,
      onMouseEnter: () => setHovered(true),
      onMouseLeave: () => setHovered(false),
      ...ringProps,
      style: {
        ...buttonStyle(variant, props.size || "sm", hovered && !props.disabled),
        boxShadow: ring ? "var(--focus-ring)" : "none",
        ...(props.disabled ? { opacity: 0.4, cursor: "not-allowed" } : null),
        ...props.style,
      },
    },
    props.children,
  );
}

/** 正方形图标按钮档（xs 20 / sm 24）。 */
const ICON_BUTTON_SIZE: Record<"xs" | "sm", Style> = {
  xs: { width: 20, height: 20, borderRadius: RADIUS.xs },
  sm: { width: 24, height: 24, borderRadius: RADIUS.sm },
};

/** 图标按钮（自带 hover 态；`label` 同时作无障碍名与悬停提示）。 */
function IconButton(props: {
  variant?: ButtonVariant;
  size?: "xs" | "sm";
  label: string;
  icon: unknown;
  disabled?: boolean;
  style?: Style;
  onClick?: (e: unknown) => void;
}) {
  const [hovered, setHovered] = React.useState(false);
  const [ring, ringProps] = useFocusRing();
  const variant = props.variant || "subtle";
  return h(
    "button",
    {
      type: "button",
      "aria-label": props.label,
      title: props.label,
      disabled: props.disabled,
      onClick: props.onClick,
      onMouseEnter: () => setHovered(true),
      onMouseLeave: () => setHovered(false),
      ...ringProps,
      style: {
        display: "inline-flex",
        alignItems: "center",
        justifyContent: "center",
        flexShrink: 0,
        border: "none",
        outline: "none",
        cursor: "pointer",
        ...ICON_BUTTON_SIZE[props.size || "sm"],
        background: hovered && !props.disabled ? BUTTON_VARIANT[variant].hoverBackground : BUTTON_VARIANT[variant].background,
        color: hovered && !props.disabled ? BUTTON_VARIANT[variant].hoverColor : BUTTON_VARIANT[variant].color,
        transition: "background " + EASE + ", color " + EASE + ", box-shadow " + EASE,
        boxShadow: ring ? "var(--focus-ring)" : "none",
        ...(props.disabled ? { opacity: 0.4, cursor: "not-allowed" } : null),
        ...props.style,
      },
    },
    props.icon,
  );
}

/** 输入框外框（对齐宿主 Input）：focused = accent 边 + 焦点环。 */
function inputStyle(focused: boolean): Style {
  return {
    width: "100%",
    boxSizing: "border-box",
    padding: "4px 8px",
    fontSize: FS.ui,
    lineHeight: LH.ui,
    borderRadius: RADIUS.sm,
    border: "1px solid " + (focused ? "var(--accent)" : "var(--input-border)"),
    background: "var(--input-bg)",
    color: textPrimary,
    outline: "none",
    boxShadow: focused ? "var(--focus-ring)" : "none",
    transition: "border-color " + EASE + ", box-shadow " + EASE,
  };
}

/** 跟踪焦点态（供无自身编辑态概念的输入框：设置项、搜索框）。 */
function useFocused(): [boolean, { onFocus: () => void; onBlur: () => void }] {
  const [focused, setFocused] = React.useState(false);
  return [focused, { onFocus: () => setFocused(true), onBlur: () => setFocused(false) }];
}

/** 键盘焦点环（内联样式写不了 :focus-visible：事件 + matches 检测，鼠标点击不亮环）。 */
function useFocusRing(): [boolean, { onFocus: (e: { currentTarget: Element }) => void; onBlur: () => void }] {
  const [on, setOn] = React.useState(false);
  return [on, { onFocus: (e) => setOn(e.currentTarget.matches(":focus-visible")), onBlur: () => setOn(false) }];
}

/** 单行文本输入（自带焦点态）。 */
function TextField(props: { value: string; placeholder?: string; style?: Style; onChange: (value: string) => void }) {
  const [focused, focusProps] = useFocused();
  return h("input", {
    value: props.value,
    placeholder: props.placeholder,
    onChange: (e: { target: { value: string } }) => props.onChange(e.target.value),
    ...focusProps,
    style: { ...inputStyle(focused), ...props.style },
  });
}

/** 下拉选择（自带焦点态，样式同输入框）。 */
function SelectField(props: { value: string; style?: Style; onChange: (value: string) => void; options: Array<{ value: string; label: string }> }) {
  const [focused, focusProps] = useFocused();
  return h(
    "select",
    {
      value: props.value,
      onChange: (e: { target: { value: string } }) => props.onChange(e.target.value),
      ...focusProps,
      style: { ...inputStyle(focused), cursor: "pointer", ...props.style },
    },
    props.options.map((o) => h("option", { key: o.value, value: o.value }, o.label)),
  );
}

/** 单选选项的胶囊按钮：选中 = accent 实底（同宿主 Radio 勾选态），未选中 = 中性底。 */
function chipStyle(selected: boolean, hovered: boolean): Style {
  return {
    display: "inline-flex",
    alignItems: "center",
    padding: "3px 10px",
    fontSize: FS.caption,
    lineHeight: LH.caption,
    fontWeight: 500,
    borderRadius: RADIUS.sm,
    cursor: "pointer",
    border: "1px solid " + (selected ? "var(--accent)" : borderColor),
    background: selected ? "var(--accent)" : hovered ? "var(--hover)" : "var(--bg-tertiary)",
    color: selected ? "var(--accent-fg)" : hovered ? textPrimary : textSecondary,
    transition: "background " + EASE + ", border-color " + EASE + ", color " + EASE,
  };
}

/** 单选选项胶囊（自带 hover 态；点选即写）。 */
function SelectChip(props: { selected: boolean; disabled?: boolean; onClick: () => void; children?: unknown }) {
  const [hovered, setHovered] = React.useState(false);
  const [ring, ringProps] = useFocusRing();
  return h("button", {
    type: "button",
    disabled: props.disabled,
    onClick: props.onClick,
    onMouseEnter: () => setHovered(true),
    onMouseLeave: () => setHovered(false),
    ...ringProps,
    style: {
      ...chipStyle(props.selected, hovered && !props.disabled),
      boxShadow: ring ? "var(--focus-ring)" : "none",
      ...(props.disabled ? { opacity: 0.5, cursor: "not-allowed" } : null),
    },
  }, props.children);
}

/** 浮层统一视觉（设置面板、行选择下拉）：overlay 底 + 浮起投影。 */
const POPUP_STYLE: Style = {
  borderRadius: RADIUS.md,
  border: "1px solid " + borderColor,
  background: "var(--bg-overlay)",
  boxShadow: "var(--shadow-pop)",
};

/** 状态胶囊（对齐宿主 StatusPill）：色点 + 文字双重编码，状态不靠颜色单独表意。 */
function StatusPill(props: { color: string; label: string }) {
  return h(
    "span",
    { style: { display: "inline-flex", alignItems: "center", gap: 6, fontSize: FS.micro, lineHeight: LH.micro, whiteSpace: "nowrap", color: props.color } },
    [
      h("span", { key: "dot", style: { width: 6, height: 6, borderRadius: RADIUS_FULL, flexShrink: 0, background: props.color } }),
      props.label,
    ],
  );
}

/** 空态（对齐宿主 EmptyState 形态）：图标底座 + 标题 + 说明 + 行动。 */
function EmptyState(props: { icon?: unknown; title: string; description?: string; action?: unknown; compact?: boolean }) {
  return h(
    "div",
    {
      style: {
        display: "flex",
        flexDirection: "column",
        alignItems: "center",
        justifyContent: "center",
        textAlign: "center",
        gap: 8,
        padding: props.compact ? "16px 12px" : "40px 24px",
      },
    },
    [
      props.icon
        ? h(
            "div",
            { key: "icon", style: { width: 40, height: 40, borderRadius: RADIUS.md, display: "flex", alignItems: "center", justifyContent: "center", flexShrink: 0, background: "var(--bg-tertiary)", color: textMuted } },
            props.icon,
          )
        : null,
      h("div", { key: "title", style: { fontSize: FS.body, lineHeight: LH.body, fontWeight: 500, color: textPrimary } }, props.title),
      props.description
        ? h("div", { key: "desc", style: { fontSize: FS.ui, lineHeight: LH.ui, color: textMuted, maxWidth: "42ch" } }, props.description)
        : null,
      props.action ? h("div", { key: "action", style: { marginTop: 4 } }, props.action) : null,
    ],
  );
}

const IMAGE_GRID_GAP = 4;

/** 方块边长上限：框很宽时也不会把单张图撑成巨块（多图时每行仍两端顶格）。 */
const IMAGE_MAX_TILE = 112;

/** 图片预览框高度：图片多时框内纵向滚动。 */
const IMAGE_FRAME_HEIGHT = 150;

/** 图片预览框：铺满宽度 + 固定高度（内层纵向滚动），下沉面。 */
const IMAGE_FRAME_STYLE: Style = {
  width: "100%",
  maxWidth: "100%",
  height: IMAGE_FRAME_HEIGHT,
  display: "flex",
  flexDirection: "column",
  borderRadius: RADIUS.sm,
  overflow: "hidden",
  border: "1px solid " + borderColor,
  background: "var(--bg-sunken)",
};

const NAV_AREA_WIDTH = 72;
const CONTENT_MAX_WIDTH = 640;

/** 表单内容列（唯一滚动容器，故其滑动条落在本列表最右缘；翻行区是本列内部成员）。 */
const SCROLL_AREA_STYLE: Style = {
  flex: 1,
  minWidth: 0,
  minHeight: 0,
  overflow: "auto",
  display: "flex",
  alignItems: "stretch",
};

const CARD_STYLE: Style = {
  maxWidth: CONTENT_MAX_WIDTH,
  margin: "0 auto",
  display: "flex",
  flexDirection: "column",
  gap: 12,
  paddingBottom: 16,
};

/** 字段行卡片：label 左列（固定宽）+ 值区伸展；编辑态 accent 描边 + 焦点环。 */
function fieldRowStyle(editing: boolean, hovered: boolean): Style {
  return {
    display: "flex",
    alignItems: "flex-start",
    gap: 12,
    padding: "12px",
    borderRadius: RADIUS.md,
    border: "1px solid " + (editing ? "var(--accent)" : borderColor),
    background: editing
      ? "color-mix(in srgb, var(--accent) 5%, transparent)"
      : hovered
        ? "var(--hover)"
        : "transparent",
    boxShadow: editing ? "var(--focus-ring)" : "none",
    transition: "border-color " + EASE + ", background " + EASE + ", box-shadow " + EASE,
  };
}

const FIELD_LABEL_STYLE: Style = {
  width: 128,
  flexShrink: 0,
  minWidth: 0,
  paddingTop: 4,
  display: "flex",
  flexDirection: "column",
  gap: 2,
};

/** 灯箱指针控件：深色全屏遮罩上的浅色反馈，非主题色（对齐宿主灯箱）。 */
const LIGHTBOX_HOVER = "rgba(255,255,255,0.1)";

const LIGHTBOX_NAV_STYLE: Style = {
  position: "absolute",
  left: 16,
  top: "50%",
  transform: "translateY(-50%)",
  width: 40,
  height: 40,
  border: "none",
  borderRadius: RADIUS_FULL,
  cursor: "pointer",
};

/**
 * 翻行区：位于内容滚动区**内部**，故滑动条落在滚动区最右缘（两个箭头都在其左侧）；
 * `align-self: stretch` 撑满内容全高、`sticky top: 0` 在滚动时钉住可视位置 ——
 * 既铺满可视高度、又不随内容滚走，且不依赖任何高度测量。箭头恒占位、只切透明度，避免布局跳动。
 */
const NAV_AREA_STYLE: Style = {
  position: "sticky",
  top: 0,
  zIndex: 5,
  flexShrink: 0,
  alignSelf: "stretch",
  width: NAV_AREA_WIDTH,
  display: "flex",
  alignItems: "center",
  justifyContent: "center",
  border: "none",
  padding: 0,
  outline: "none",
  transition: "background " + EASE,
};

/** 整视图空态（未开表/载入中）的居中容器。 */
const EMPTY_VIEW_STYLE: Style = {
  flex: 1,
  minHeight: 0,
  display: "flex",
  alignItems: "center",
  justifyContent: "center",
};

/** 入口：注册表格表单视图（注册随插件启停经 fiber 生命周期撤销）。 */
export default function apply(ctx: AtelyxCtx): void {
  const table = ctx.table;
  /** 订阅表格变更（fiber 级注册，停用随插件撤销）。 */
  const onTableChanged = (cb: () => void): (() => void) => ctx.events.on("table:changed", cb);

  /** 读表单配置表（失败/损坏 = 空表，不阻塞填表）。 */
  async function readForms(): Promise<Record<string, FormConfig>> {
    try {
      return readStateForms(await ctx.state.read(PLUGIN_ID));
    } catch (e) {
      console.error("[table-form] 读取表单配置失败", e);
      return {};
    }
  }

  /** 写表单配置表的串行队列：读改写必须互斥，否则连续操作会互相覆盖。 */
  let writeChain: Promise<unknown> = Promise.resolve();

  /** 写表单配置表：整体读 → 改一张表 → 整体写（写前重读 + 串行，避免多实例互相覆盖）。 */
  function writeForm(tableFile: string, update: (cfg: FormConfig) => FormConfig): Promise<boolean> {
    const run = writeChain.then(async () => {
      try {
        const forms = await readForms();
        forms[tableFile] = update(forms[tableFile] || emptyConfig());
        const next: FormState = { version: STATE_VERSION, forms };
        await ctx.state.write(PLUGIN_ID, next);
        return true;
      } catch (e) {
        console.error("[table-form] 保存表单配置失败", e);
        ctx.notification.notify({ message: "表单配置保存失败，本次改动仅当前会话有效", level: "error" });
        return false;
      }
    });
    // 队列自身不因单次失败中断（失败已在内部收敛为 false）
    writeChain = run.catch(() => undefined);
    return run;
  }

  /**
   * 新增一行并定位其 id：addRow 不返回 id，用「新增前后行 id 集合差集」定位；
   * 差集不唯一（同一时刻其它窗口也在新增）返回 null，调用方放弃本次写入。
   */
  function addRowAndGetId(): string | null {
    const before = new Set(table.snapshot().rows.map((r) => r.id));
    table.addRow();
    const created = table.snapshot().rows.filter((r) => !before.has(r.id));
    return created.length === 1 ? created[0].id : null;
  }

  /**
   * 写一个单元格（对齐宿主表格的「失焦即写」）：
   * - 已有目标行：直接按字段写该格（宿主为逐格不可变更新 + 逐格协作广播，不碰同行其它字段）
   * - 尚无目标行（新增态）：先 addRow 再差集定位新行，定位失败则中止不写任何值
   * 只写当前这一个字段（不做多字段批量填值），故不需要多字段失败清单。
   */
  function writeField(
    field: Field,
    raw: string,
    rowId: string | null,
  ): { ok: boolean; rowId: string | null; created: boolean; conflict: boolean; error: string | null } {
    const miss = { ok: false, rowId, created: false, conflict: false, error: null };
    const value = toWriteValue(field, raw);
    if (value === undefined) return miss; // 空值/非法数字：不写（清空用显式文本再失焦）
    let target = rowId;
    if (target === null) {
      const created = addRowAndGetId();
      if (created === null) {
        return { ok: false, rowId: null, created: false, conflict: true, error: null };
      }
      target = created;
    }
    try {
      table.updateCell(target, field.id, value);
    } catch (e) {
      console.error("[table-form] 写入单元格失败", e);
      return { ok: false, rowId: target, created: target !== rowId, conflict: false, error: "写入失败，请重试" };
    }
    return { ok: true, rowId: target, created: target !== rowId, conflict: false, error: null };
  }

  // ===== 图片解析（进程级共享缓存）=====

  /** dataURL 缓存（entry → 解析 Promise）：磁贴展示与灯箱共用，同一张图只解析一次。 */
  const imageCache = new Map<string, Promise<string>>();

  /**
   * 解析图片条目为 dataURL：并发调用共享同一个 Promise；单条失败收敛为 ""（不拖累其余），
   * 失败结果不进缓存——下次（如重开灯箱）自动重试。
   */
  function resolveImageCached(entry: string): Promise<string> {
    let p = imageCache.get(entry);
    if (!p) {
      p = table.resolveImage(entry).catch((e) => {
        console.error("[table-form] 读取图片失败", entry, e);
        return "";
      });
      void p.then((url) => {
        if (url === "") imageCache.delete(entry);
      });
      imageCache.set(entry, p);
    }
    return p;
  }

  // ===== 表单配置的共享源 =====
  // 配置同时被两个挂载点消费：表格编辑器的视图（FormView）与表格工具条右上角的设置弹层
  // （toolbar/table/right 槽位）。两者读同一份状态、经同一套函数写盘，故挂在插件作用域上共享。

  interface SettingsSnapshot {
    tableFile: string | null;
    fields: Field[];
    config: FormConfig | null;
  }
  const store = {
    snap: { tableFile: null, fields: EMPTY_FIELDS, config: null } as SettingsSnapshot,
    listeners: new Set<() => void>(),
  };
  const emptySnap: SettingsSnapshot = { tableFile: null, fields: EMPTY_FIELDS, config: null };

  function publish(next: SettingsSnapshot): void {
    store.snap = next;
    for (const fn of Array.from(store.listeners)) fn();
  }
  function subscribeSnap(fn: () => void): () => void {
    store.listeners.add(fn);
    return () => {
      store.listeners.delete(fn);
    };
  }
  function currentSnap(): SettingsSnapshot {
    return store.snap;
  }

  /** 按表格列序把新启用的字段插到对应位置（而非一律追加到末尾）。 */
  function insertByTableOrder(fieldsCfg: FormFieldConfig[], added: FormFieldConfig, tableFields: Field[]): FormFieldConfig[] {
    const tableIndex = tableFields.findIndex((f) => f.id === added.fieldId);
    if (tableIndex < 0) return fieldsCfg.concat([added]);
    let insertAt = fieldsCfg.length;
    for (let i = 0; i < fieldsCfg.length; i++) {
      const idx = tableFields.findIndex((f) => f.id === fieldsCfg[i].fieldId);
      if (idx < 0) continue;
      if (idx > tableIndex) {
        insertAt = i;
        break;
      }
    }
    const next = fieldsCfg.slice();
    next.splice(insertAt, 0, added);
    return next;
  }

  function setFieldEnabled(fieldId: string, enabled: boolean): void {
    const snap = store.snap;
    if (!snap.tableFile || !snap.config) return;
    const exists = snap.config.fields.some((f) => f.fieldId === fieldId);
    if (exists === enabled) return;
    const added = createFieldConfig(snap.fields.find((f) => f.id === fieldId), fieldId);
    const nextFields = enabled
      ? insertByTableOrder(snap.config.fields, added, snap.fields)
      : snap.config.fields.filter((f) => f.fieldId !== fieldId);
    publish({ ...snap, config: { ...snap.config, fields: nextFields } });
    void writeForm(snap.tableFile, (prev) => {
      const base = prev || emptyConfig();
      return {
        ...base,
        fields: enabled
          ? insertByTableOrder(base.fields, added, table.snapshot().fields)
          : base.fields.filter((f) => f.fieldId !== fieldId),
      };
    });
  }

  function moveFieldConfig(fieldId: string, delta: number): void {
    const snap = store.snap;
    if (!snap.tableFile || !snap.config) return;
    // 以配置序为准定位（面板行序可能与配置序错位：已从表格删除的字段不渲染但仍占配置位）
    const from = snap.config.fields.findIndex((f) => f.fieldId === fieldId);
    const to = from + delta;
    if (from < 0 || to < 0 || to >= snap.config.fields.length) return;
    const swap = (list: FormFieldConfig[]): FormFieldConfig[] => {
      const next = list.slice();
      const hold = next[from];
      next[from] = next[to];
      next[to] = hold;
      return next;
    };
    publish({ ...snap, config: { ...snap.config, fields: swap(snap.config.fields) } });
    void writeForm(snap.tableFile, (prev) => {
      const base = prev || emptyConfig();
      return { ...base, fields: swap(base.fields) };
    });
  }

  function patchFieldConfig(fieldId: string, patch: Partial<FormFieldConfig>): void {
    const snap = store.snap;
    if (!snap.tableFile || !snap.config) return;
    const apply = (fieldsCfg: FormFieldConfig[]): FormFieldConfig[] =>
      fieldsCfg.map((f) => (f.fieldId === fieldId ? { ...f, ...patch } : f));
    publish({ ...snap, config: { ...snap.config, fields: apply(snap.config.fields) } });
    void writeForm(snap.tableFile, (prev) => {
      const base = prev || emptyConfig();
      return { ...base, fields: apply(base.fields) };
    });
  }

  // ===== 表单设置（表格工具条右上角入口，与视图共享同一份配置）=====

  /** 订阅共享快照（getSnapshot 须返回稳定引用，故直接返回 store.snap）。 */
  function useSettingsSnapshot(): SettingsSnapshot {
    return React.useSyncExternalStore(subscribeSnap, currentSnap, () => emptySnap);
  }

  /**
   * 设置弹层内容：**按表格全部字段**渲染（关闭只把该字段从表单里移出，行本身仍在，可再点回来），
   * 排序为「已启用（配置顺序）在前 + 未启用（表格列序）在后」；已启用项显示填写提示与单选样式。
   */
  function FormSettingsPanel() {
    const snap = useSettingsSnapshot();
    const { fields, config } = snap;
    const rows: Array<{ target: FormFieldConfig | null; field: Field }> = [];
    if (config) {
      for (const target of config.fields) {
        const field = fields.find((f) => f.id === target.fieldId);
        if (field) rows.push({ target, field });
      }
      for (const field of fields) {
        if (!config.fields.some((f) => f.fieldId === field.id)) rows.push({ target: null, field });
      }
    }
    const missingCount = config ? config.fields.length - rows.filter((r) => r.target).length : 0;
    return h("div", { style: { display: "flex", flexDirection: "column", gap: 8 } }, [
      h("div", { key: "h", style: { display: "flex", alignItems: "center", gap: 8 } }, [
        h("span", { key: "t", style: { fontSize: FS.caption, lineHeight: LH.caption, fontWeight: 600, color: textPrimary } }, "表单设置"),
        h("span", { key: "sp", style: { flex: 1 } }),
        h(
          "span",
          { key: "c", style: { fontSize: FS.micro, lineHeight: LH.micro, color: textMuted, fontVariantNumeric: "tabular-nums" } },
          "启用 " + rows.filter((r) => r.target).length + " / 共 " + fields.length + " 个字段",
        ),
      ]),
      rows.length > 0
        ? rows.map(({ target, field }) =>
            h(
              "div",
              {
                key: field.id,
                style: {
                  display: "flex",
                  flexDirection: "column",
                  gap: 4,
                  padding: "6px 8px",
                  borderRadius: RADIUS.sm,
                  border: "1px solid " + borderColor,
                  background: target ? "color-mix(in srgb, var(--text-primary) 4%, transparent)" : "transparent",
                  opacity: target ? 1 : 0.65,
                  transition: "background " + EASE,
                },
              },
              [
                h("div", { key: "row", style: { display: "flex", alignItems: "center", gap: 6 } }, [
                  h(IconButton, {
                    key: "sw",
                    size: "xs",
                    label: target ? "从表单中移出该字段" : "把该字段加入表单",
                    icon: target ? h(EyeIcon, { size: 14 }) : h(EyeOffIcon, { size: 14 }),
                    onClick: () => setFieldEnabled(field.id, !target),
                    style: target ? { color: "var(--accent)" } : undefined,
                  }),
                  h("span", { key: "n", style: { flex: 1, fontSize: FS.caption, lineHeight: LH.caption, color: textPrimary, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" } }, field.name),
                  h("span", { key: "ty", style: { fontSize: FS.micro, lineHeight: LH.micro, color: textMuted } }, describeType(field)),
                  // 排序只在已启用字段间有效
                  target
                    ? [
                        h(IconButton, { key: "up", size: "xs", label: "上移", icon: h(UpIcon, { size: 13 }), onClick: () => moveFieldConfig(field.id, -1) }),
                        h(IconButton, { key: "down", size: "xs", label: "下移", icon: h(DownIcon, { size: 13 }), onClick: () => moveFieldConfig(field.id, 1) }),
                      ]
                    : null,
                ]),
                target
                  ? h("div", { key: "cfg", style: { display: "flex", alignItems: "center", gap: 6 } }, [
                      h(TextField, {
                        key: "hint",
                        value: target.hint || "",
                        placeholder: "填写提示",
                        onChange: (value: string) => patchFieldConfig(field.id, { hint: value }),
                        style: { flex: 1, width: "auto", padding: "2px 6px", fontSize: FS.caption, lineHeight: LH.caption },
                      }),
                      field.type === "singleSelect"
                        ? h(SelectField, {
                            key: "style",
                            value: target.selectStyle || "radio",
                            onChange: (value: string) => patchFieldConfig(field.id, { selectStyle: value === "dropdown" ? "dropdown" : "radio" }),
                            options: [
                              { value: "radio", label: "按钮" },
                              { value: "dropdown", label: "下拉" },
                            ],
                            style: { width: 84, padding: "2px 6px", fontSize: FS.caption, lineHeight: LH.caption },
                          })
                        : null,
                    ])
                  : null,
              ],
            ),
          )
        : h(
            "div",
            { key: "empty", style: { fontSize: FS.micro, lineHeight: LH.micro, color: textMuted } },
            fields.length === 0 ? "该表格还没有字段，请先在表格视图添加字段。" : "正在载入表单配置…",
          ),
      missingCount > 0
        ? h("div", { key: "missing", style: { fontSize: FS.micro, lineHeight: LH.micro, color: textMuted } }, "有 " + missingCount + " 个已配置字段已从表格中删除，配置保留但不再显示。")
        : null,
    ]);
  }

  /**
   * 表格工具条右上角的「表单设置」入口。
   * （宿主表格面板的 `···` 菜单没有插件接入口——`ToolbarTable` 只挂了 `toolbar/table/right`，
   * 菜单项本身硬编码；故经本槽位贡献一个同风格的按钮，点开浮层即设置。）
   */
  function FormSettingsMenu() {
    const snap = useSettingsSnapshot();
    const [anchor, setAnchor] = React.useState(null as { right: number; top: number } | null);
    const [hovered, setHovered] = React.useState(false);
    const [ring, ringProps] = useFocusRing();
    const triggerRef = React.useRef<HTMLElement | null>(null);
    const boxRef = React.useRef<HTMLElement | null>(null);

    React.useEffect(() => {
      if (!anchor) return;
      const onDown = (e: MouseEvent) => {
        const el = e.target as Node | null;
        if (el && triggerRef.current && triggerRef.current.contains(el)) return;
        if (el && boxRef.current && boxRef.current.contains(el)) return;
        setAnchor(null);
      };
      const onKey = (e: KeyboardEvent) => {
        if (e.key === "Escape") setAnchor(null);
      };
      document.addEventListener("mousedown", onDown);
      window.addEventListener("keydown", onKey);
      return () => {
        document.removeEventListener("mousedown", onDown);
        window.removeEventListener("keydown", onKey);
      };
    }, [anchor]);

    if (!snap.tableFile) return null;

    const open = !!anchor;
    return h("span", { style: { flexShrink: 0, position: "relative", display: "inline-flex" } }, [
      h(
        "button",
        {
          key: "trigger",
          ref: triggerRef,
          type: "button",
          onClick: (e: { currentTarget: HTMLElement }) => {
            const rect = e.currentTarget.getBoundingClientRect();
            setAnchor((prev) => (prev ? null : { right: rect.right, top: rect.bottom + 4 }));
          },
          onMouseEnter: () => setHovered(true),
          onMouseLeave: () => setHovered(false),
          ...ringProps,
          title: "配置参与表单填写的字段",
          // 展开态是动态条件色（对齐宿主「···」菜单触发器），按 style 覆盖 variant 色
          style: {
            ...buttonStyle("subtle", "sm", hovered),
            boxShadow: ring ? "var(--focus-ring)" : "none",
            ...(open ? { color: "var(--accent)", background: "var(--accent-soft)" } : null),
          },
        },
        [h(SlidersIcon, { key: "i", size: 13 }), h("span", { key: "t" }, "表单设置")],
      ),
      anchor
        ? h(
            "div",
            {
              key: "panel",
              ref: boxRef,
              style: {
                ...POPUP_STYLE,
                position: "fixed",
                top: anchor.top,
                right: Math.max(8, window.innerWidth - anchor.right),
                zIndex: 300,
                width: 272,
                maxHeight: "60vh",
                overflowY: "auto",
                padding: 12,
              },
            },
            h(FormSettingsPanel, { key: "panel-body" }),
          )
        : null,
    ]);
  }

  interface RowPickerProps {
    rows: Row[];
    fields: Field[];
    selectedRowId: string | null;
    currentRowId: string | null;
    onPick: (rowId: string) => void;
  }

  function RowPicker(props: RowPickerProps) {
    const [open, setOpen] = React.useState(false);
    const [query, setQuery] = React.useState("");
    const [hoverId, setHoverId] = React.useState(null as string | null);
    const boxRef = React.useRef<{ contains(node: unknown): boolean } | null>(null);

    React.useEffect(() => {
      if (!open) return;
      const onDocDown = (e: MouseEvent) => {
        const target = e.target as unknown;
        if (boxRef.current && !boxRef.current.contains(target)) setOpen(false);
      };
      document.addEventListener("mousedown", onDocDown);
      return () => document.removeEventListener("mousedown", onDocDown);
    }, [open]);

    const selectedIndex = props.rows.findIndex((r) => r.id === props.currentRowId);
    const matched = React.useMemo(() => {
      const all = props.rows.map((row, index) => ({
        id: row.id,
        label: "第 " + (index + 1) + " 行",
        detail: rowLabel(row, props.fields, index),
      }));
      const q = query.trim().toLowerCase();
      const list =
        q === ""
          ? all
          : all.filter((item) => item.detail.toLowerCase().indexOf(q) >= 0 || item.label.indexOf(q) >= 0);
      return { list: list.slice(0, ROW_PICKER_LIMIT), total: list.length };
    }, [props.rows, props.fields, query]);

    return h(
      "div",
      { ref: boxRef, style: { position: "relative", display: "flex", alignItems: "center", flexShrink: 0 } },
      [
        h(
          Button,
          {
            key: "trigger",
            variant: "ghost",
            size: "sm",
            onClick: () => setOpen((v) => !v),
            title: "选择要编辑的行",
            style: { maxWidth: 240 },
          },
          [
            h("span", { key: "t", style: { overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" } },
              selectedIndex >= 0
                ? "第 " + (selectedIndex + 1) + " 行"
                : "选择要编辑的行"),
            h(ChevronIcon, { key: "i", size: 12 }),
          ],
        ),
        open
          ? h(
              "div",
              {
                key: "list",
                style: {
                  ...POPUP_STYLE,
                  position: "absolute",
                  top: 30,
                  left: 0,
                  zIndex: 20,
                  width: 320,
                  maxHeight: 320,
                  overflow: "auto",
                  padding: 8,
                },
              },
              [
                h(TextField, {
                  key: "q",
                  value: query,
                  placeholder: "搜索行内容…",
                  onChange: (value: string) => setQuery(value),
                  style: { marginBottom: 6 },
                }),
                h(
                  "div",
                  { key: "items", style: { display: "flex", flexDirection: "column", gap: 2 } },
                  matched.list.length === 0
                    ? h("div", { style: { padding: "6px 8px", fontSize: FS.caption, lineHeight: LH.caption, color: textMuted } }, "没有匹配的行")
                    : matched.list.map((item) =>
                        h(
                          "button",
                          {
                            key: item.id,
                            type: "button",
                            onClick: () => {
                              props.onPick(item.id);
                              setOpen(false);
                              setQuery("");
                            },
                            onMouseEnter: () => setHoverId(item.id),
                            onMouseLeave: () => setHoverId(null),
                            style: {
                              display: "flex",
                              alignItems: "center",
                              gap: 6,
                              width: "100%",
                              padding: "4px 8px",
                              fontSize: FS.caption,
                              lineHeight: LH.caption,
                              textAlign: "left",
                              borderRadius: RADIUS.sm,
                              border: "none",
                              cursor: "pointer",
                              background:
                                item.id === props.currentRowId
                                  ? "color-mix(in srgb, var(--accent) 14%, transparent)"
                                  : item.id === hoverId
                                    ? "var(--hover)"
                                    : "transparent",
                              color: textPrimary,
                              transition: "background " + EASE,
                            },
                          },
                          [
                            h("span", { key: "c", style: { width: 8, height: 8, borderRadius: RADIUS_FULL, flexShrink: 0, background: "var(--accent)", opacity: item.id === props.selectedRowId ? 1 : 0 } }),
                            h("span", { key: "n", style: { flexShrink: 0, color: textMuted, fontVariantNumeric: "tabular-nums" } }, item.label),
                            h("span", { key: "l", style: { overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" } }, item.detail),
                          ],
                        ),
                      ),
                ),
                matched.total > matched.list.length
                  ? h(
                      "div",
                      { key: "more", style: { padding: "6px 8px", fontSize: FS.micro, lineHeight: LH.micro, color: textMuted } },
                      "仅显示前 " + ROW_PICKER_LIMIT + " 条（共 " + matched.total + " 条匹配，可搜索缩小范围）",
                    )
                  : null,
              ],
            )
          : null,
      ],
    );
  }

  // ===== 图片字段预览（平铺正方形 + 点击看原图 + 悬停移除）=====

  /** 图片单元格值 → 图片条目数组（兼容两种已知形态：归一化 `{images}` 与磁盘直读的字符串数组）。 */
  function readImageEntries(field: Field, value: unknown): string[] {
    if (field.type !== "image") return [];
    // store 直接使用磁盘行（仅远端补丁经 normalizeTableRow），故 string[] 形态必须一并接受
    if (Array.isArray(value)) return value.filter((e): e is string => typeof e === "string");
    const rec = asRecord(value);
    const raw = rec ? rec.images : undefined;
    return Array.isArray(raw) ? raw.filter((e): e is string => typeof e === "string") : [];
  }

  function useImageSources(entries: string[]): Map<string, string> {
    const mapRef = React.useRef(new Map<string, string>());
    const [, force] = React.useState(0);
    const aliveRef = React.useRef(true);
    React.useEffect(() => {
      aliveRef.current = true;
      return () => {
        aliveRef.current = false;
      };
    }, []);
    const key = entries.join("\n");
    React.useEffect(() => {
      let cancelled = false;
      for (const entry of entries) {
        if (mapRef.current.has(entry)) continue;
        void resolveImageCached(entry).then((url) => {
          if (cancelled || !aliveRef.current || url === "") return;
          mapRef.current.set(entry, url);
          force((n) => n + 1);
        });
      }
      return () => {
        cancelled = true;
      };
      // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [key]);
    return mapRef.current;
  }

  /** 放大预览：全屏遮罩 + 居中大图 + 左右循环切换 + 计数器；Esc/点遮罩关闭，←/→ 切换。
   *  读取失败的图（url = ""）显示占位块，不阻塞其余图片浏览。 */
  interface LightboxProps {
    urls: string[];
    index: number;
    onIndex: (index: number) => void;
    onClose: () => void;
  }
  /** 灯箱指针按钮：深色全屏遮罩上的浅色 hover 反馈（非主题色，对齐宿主灯箱）。 */
  function LightboxButton(props: { title: string; onClick: (e: { stopPropagation: () => void }) => void; style?: Style; children?: unknown }) {
    const [hovered, setHovered] = React.useState(false);
    return h(
      "button",
      {
        type: "button",
        title: props.title,
        "aria-label": props.title,
        onClick: props.onClick,
        onMouseEnter: () => setHovered(true),
        onMouseLeave: () => setHovered(false),
        style: {
          display: "flex",
          alignItems: "center",
          justifyContent: "center",
          border: "none",
          cursor: "pointer",
          color: "var(--text-primary)",
          background: hovered ? LIGHTBOX_HOVER : "transparent",
          transition: "background " + EASE,
          ...props.style,
        },
      },
      props.children,
    );
  }

  function ImageLightbox(props: LightboxProps) {
    const count = props.urls.length;
    const latest = React.useRef(props);
    latest.current = props;
    React.useEffect(() => {
      const onKey = (e: KeyboardEvent) => {
        const p = latest.current;
        const total = p.urls.length;
        if (e.key === "Escape") p.onClose();
        if (total > 1 && e.key === "ArrowLeft") p.onIndex((p.index - 1 + total) % total);
        if (total > 1 && e.key === "ArrowRight") p.onIndex((p.index + 1) % total);
      };
      window.addEventListener("keydown", onKey);
      return () => window.removeEventListener("keydown", onKey);
    }, []);
    const url = props.urls[props.index];
    return h(
      "div",
      {
        "data-lightbox": "",
        onClick: props.onClose,
        style: {
          position: "fixed",
          inset: 0,
          zIndex: 200,
          display: "flex",
          alignItems: "center",
          justifyContent: "center",
          background: "rgba(0,0,0,0.85)",
        },
      },
      [
        url === ""
          ? h(
              "div",
              {
                key: "ph",
                onClick: (e: { stopPropagation: () => void }) => e.stopPropagation(),
                style: {
                  padding: "24px 32px",
                  borderRadius: RADIUS.md,
                  border: "1px solid rgba(255,255,255,0.25)",
                  color: "rgba(255,255,255,0.8)",
                  fontSize: FS.ui,
                  lineHeight: LH.ui,
                },
              },
              "这张图片读取失败",
            )
          : h("img", {
              key: "img",
              src: url,
              alt: "放大预览",
              draggable: false,
              onClick: (e: { stopPropagation: () => void }) => e.stopPropagation(),
              style: { maxWidth: "90vw", maxHeight: "90vh", objectFit: "contain", userSelect: "none" },
            }),
        h(
          LightboxButton,
          {
            key: "close",
            onClick: (e: { stopPropagation: () => void }) => {
              e.stopPropagation();
              props.onClose();
            },
            title: "关闭 (Esc)",
            style: { position: "absolute", top: 16, right: 16, width: 36, height: 36, borderRadius: RADIUS.sm },
          },
          h(CloseIcon, { size: 20 }),
        ),
        count > 1
          ? h(
              "div",
              { key: "nav", onClick: (e: { stopPropagation: () => void }) => e.stopPropagation() },
              [
                h(
                  LightboxButton,
                  { key: "prev", onClick: () => props.onIndex((props.index - 1 + count) % count), title: "上一张 (←)", style: LIGHTBOX_NAV_STYLE },
                  h(ArrowIcon, { size: 24, dir: "left" }),
                ),
                h("div", { key: "n", style: { position: "absolute", bottom: 16, left: "50%", transform: "translateX(-50%)", padding: "2px 10px", borderRadius: RADIUS_FULL, fontSize: FS.caption, lineHeight: LH.caption, background: "rgba(0,0,0,0.6)", color: "var(--text-primary)", fontVariantNumeric: "tabular-nums" } }, props.index + 1 + " / " + count),
                h(
                  LightboxButton,
                  { key: "next", onClick: () => props.onIndex((props.index + 1) % count), title: "下一张 (→)", style: { ...LIGHTBOX_NAV_STYLE, left: "auto", right: 16 } },
                  h(ArrowIcon, { size: 24, dir: "right" }),
                ),
              ],
            )
          : null,
      ],
    );
  }

  interface ImagePreviewProps {
    entries: string[];
    hasRow: boolean;
    canRemove: boolean;
    onRemove: (index: number) => void;
  }

  /** 图片磁贴：外层容器承接 hover（浮现移除按钮），内层按钮负责点开灯箱。 */
  function ImageTile(props: { url: string; index: number; size: number; canRemove: boolean; onOpen: () => void; onRemove: () => void }) {
    const [hover, setHover] = React.useState(false);
    return h(
      "div",
      {
        onMouseEnter: () => setHover(true),
        onMouseLeave: () => setHover(false),
        style: {
          position: "relative",
          width: props.size,
          height: props.size,
          flexShrink: 0,
          borderRadius: RADIUS.sm,
          overflow: "hidden",
          border: "1px solid " + (hover ? "var(--accent)" : borderColor),
          background: "var(--bg-tertiary)",
          transition: "border-color " + EASE,
        },
      },
      [
        h(
          "button",
          {
            key: "img",
            type: "button",
            onClick: props.onOpen,
            title: "点击查看原图（第 " + (props.index + 1) + " 张）",
            style: { display: "block", width: "100%", height: "100%", padding: 0, border: "none", background: "transparent", cursor: "zoom-in" },
          },
          props.url
            ? h("img", { src: props.url, alt: String(props.index + 1), draggable: false, style: { width: "100%", height: "100%", objectFit: "cover", display: "block" } })
            : h("span", { style: { fontSize: FS.micro, lineHeight: LH.micro, color: textMuted } }, "加载中…"),
        ),
        props.canRemove && hover
          ? h(
              "button",
              {
                key: "rm",
                type: "button",
                onClick: (e: { stopPropagation: () => void }) => {
                  e.stopPropagation();
                  props.onRemove();
                },
                title: "从单元格移除这张图片",
                style: {
                  position: "absolute",
                  top: 4,
                  right: 4,
                  width: 20,
                  height: 20,
                  display: "flex",
                  alignItems: "center",
                  justifyContent: "center",
                  padding: 0,
                  border: "none",
                  borderRadius: RADIUS_FULL,
                  background: "rgba(0,0,0,0.55)",
                  color: "#fff",
                  cursor: "pointer",
                },
              },
              h(CloseIcon, { size: 12 }),
            )
          : null,
      ],
    );
  }

  /** 图片字段预览：**平铺正方形**（每张 1:1），多图时每行两端顶格、彼此只留 4px 间隙，行满换行；
   *  方块大小有上限，避免框很宽时单张图被撑成巨块；超出框高在框内纵向滚动。点任意一张看原图。 */
  function ImagePreview(props: ImagePreviewProps) {
    const entries = props.entries;
    const [tileSize, setTileSize] = React.useState(0);
    const middleRef = React.useRef<HTMLDivElement | null>(null);
    const [lightbox, setLightbox] = React.useState(null as { urls: string[]; index: number } | null);
    const srcMap = useImageSources(entries);

    React.useEffect(() => {
      const el = middleRef.current;
      if (!el) return;
      const measure = () => {
        const width = el.clientWidth - 8; // 去掉左右各 4px 内衬
        if (width <= 0) return;
        const cols = Math.max(1, Math.floor((width + IMAGE_GRID_GAP) / (IMAGE_MAX_TILE + IMAGE_GRID_GAP)));
        const size = Math.min(IMAGE_MAX_TILE, (width - (cols - 1) * IMAGE_GRID_GAP) / cols);
        setTileSize(Math.max(24, size));
      };
      measure();
      const observer = new ResizeObserver(measure);
      observer.observe(el);
      return () => observer.disconnect();
    }, []);

    // 灯箱复用共享缓存（未命中的条目并行补齐）；失败项落 ""（灯箱内显示占位）并提示一次
    const openLightboxAt = (i: number) => {
      void Promise.all(entries.map((entry) => resolveImageCached(entry))).then((urls) => {
        const failed = urls.filter((u) => u === "").length;
        if (failed > 0) {
          ctx.notification.notify({ message: "有 " + failed + " 张图片读取失败，将以占位图显示", level: "warning" });
        }
        setLightbox({ urls, index: i });
      });
    };

    if (entries.length === 0) {
      return h(
        "div",
        { style: { padding: "8px 12px", borderRadius: RADIUS.sm, border: "1px dashed " + borderColor, fontSize: FS.micro, lineHeight: LH.micro, color: textMuted } },
        props.hasRow ? "该行此字段暂无图片" : "选中一行后显示该行的图片",
      );
    }

    return h("div", { style: { position: "relative", width: "100%" } }, [
      h("div", { key: "frame", style: IMAGE_FRAME_STYLE }, [
        h(
          "div",
          {
            key: "grid",
            ref: middleRef,
            style: { flex: 1, minHeight: 0, padding: 4, overflowY: "auto", display: "flex", flexWrap: "wrap", alignContent: "flex-start", gap: IMAGE_GRID_GAP },
          },
          entries.map((entry, i) =>
            h(ImageTile, {
              key: entry,
              url: srcMap.get(entry) || "",
              index: i,
              size: tileSize > 0 ? tileSize : IMAGE_MAX_TILE,
              canRemove: props.canRemove,
              onOpen: () => openLightboxAt(i),
              onRemove: () => props.onRemove(i),
            }),
          ),
        ),
      ]),
      lightbox
        ? h(ImageLightbox, {
            key: "lightbox",
            urls: lightbox.urls,
            index: lightbox.index,
            onIndex: (i: number) => setLightbox({ urls: lightbox.urls, index: i }),
            onClose: () => setLightbox(null),
          })
        : null,
    ]);
  }

  function Svg(props: { size?: number; children?: unknown }) {
    return h(
      "svg",
      {
        width: props.size || 14,
        height: props.size || 14,
        viewBox: "0 0 24 24",
        fill: "none",
        stroke: "currentColor",
        strokeWidth: 2,
        strokeLinecap: "round",
        strokeLinejoin: "round",
        style: { flexShrink: 0 },
      },
      props.children,
    );
  }
  function ChevronIcon(props: { size?: number }) {
    return h(Svg, { size: props.size }, h("path", { d: "M6 9l6 6 6-6" }));
  }
  function ArrowIcon(props: { size?: number; dir: "left" | "right" }) {
    return h(Svg, { size: props.size }, h("path", { d: props.dir === "left" ? "M15 18l-6-6 6-6" : "M9 6l6 6-6 6" }));
  }
  function UndoIcon(props: { size?: number }) {
    return h(Svg, { size: props.size }, h("path", { d: "M3 7v6h6" }), h("path", { d: "M3 13a9 9 0 1 0 3-7" }));
  }
  function PlusIcon(props: { size?: number }) {
    return h(Svg, { size: props.size }, h("path", { d: "M12 5v14" }), h("path", { d: "M5 12h14" }));
  }
  function EyeOffIcon(props: { size?: number }) {
    return h(Svg, { size: props.size }, h("path", { d: "M17.9 17.9A10.5 10.5 0 0 1 12 20c-7 0-11-8-11-8a19.8 19.8 0 0 1 5.1-6.1" }), h("path", { d: "M9.9 4.2A10.9 10.9 0 0 1 12 4c7 0 11 8 11 8a19.9 19.9 0 0 1-2.2 3.2" }), h("path", { d: "M1 1l22 22" }));
  }
  function EyeIcon(props: { size?: number }) {
    return h(Svg, { size: props.size }, h("path", { d: "M1 12s4-8 11-8 11 8 11 8-4 8-11 8-11-8-11-8z" }), h("circle", { cx: 12, cy: 12, r: 3 }));
  }
  function UpIcon(props: { size?: number }) {
    return h(Svg, { size: props.size }, h("path", { d: "M12 19V5" }), h("path", { d: "M5 12l7-7 7 7" }));
  }
  function DownIcon(props: { size?: number }) {
    return h(Svg, { size: props.size }, h("path", { d: "M12 5v14" }), h("path", { d: "M19 12l-7 7-7-7" }));
  }
  function CloseIcon(props: { size?: number }) {
    return h(Svg, { size: props.size }, h("path", { d: "M18 6L6 18" }), h("path", { d: "M6 6l12 12" }));
  }
  function SlidersIcon(props: { size?: number }) {
    return h(
      Svg,
      { size: props.size },
      h("path", { d: "M4 21v-7" }),
      h("path", { d: "M4 10V3" }),
      h("path", { d: "M12 21v-9" }),
      h("path", { d: "M12 8V3" }),
      h("path", { d: "M20 21v-5" }),
      h("path", { d: "M20 12V3" }),
      h("path", { d: "M1 14h6" }),
      h("path", { d: "M9 8h6" }),
      h("path", { d: "M17 16h6" }),
    );
  }
  /** 空状态引导图标（表单纸张轮廓）。 */
  function FormIcon(props: { size?: number }) {
    return h(
      Svg,
      { size: props.size },
      h("rect", { x: 4, y: 3, width: 16, height: 18, rx: 2 }),
      h("path", { d: "M8 8h8" }),
      h("path", { d: "M8 12h8" }),
      h("path", { d: "M8 16h5" }),
    );
  }

  function FormView() {
    const [snap, setSnap] = React.useState(null as TableSnapshot | null);
    const [config, setConfig] = React.useState(null as FormConfig | null);
    /** 配置是否已载入（载入前不落行选择默认值，避免与磁盘记录的选择打架）。 */
    const [configReady, setConfigReady] = React.useState(false);
    const [editingFieldId, setEditingFieldId] = React.useState(null as string | null);
    /** Esc 放弃编辑的信号（自增；输入框据此把本地值退回表格当前值）。 */
    const [escapeSignal, setEscapeSignal] = React.useState(0);
    const [conflict, setConflict] = React.useState(null as string | null);
    const [savedAt, setSavedAt] = React.useState(0);
    const [saveState, setSaveState] = React.useState("idle");
    const [rowId, setRowId] = React.useState(null as string | null);
    const [newRow, setNewRow] = React.useState(null as NewRowGuard | null);
    const [now, setNow] = React.useState<number>(Date.now());
    const aliveRef = React.useRef(true);
    const stateRef = React.useRef({
      config: null as FormConfig | null,
      tableFile: null as string | null,
      currentRowId: null as string | null,
    });

    // ===== 数据接入：初次快照 + 表格变更订阅 =====
    React.useEffect(() => {
      aliveRef.current = true;
      const push = () => {
        if (aliveRef.current) setSnap(table.snapshot());
      };
      push();
      const off = onTableChanged(push);
      return () => {
        aliveRef.current = false;
        off();
      };
    }, []);

    const tableFile = snap ? snap.tableFile : null;
    const fields = snap ? snap.fields : EMPTY_FIELDS;
    const rows = snap ? snap.rows : EMPTY_ROWS;
    const selectedRowId = snap ? snap.selectedRowId : null;
    const peerColorByRowId = snap ? snap.peerColorByRowId : EMPTY_PEER_COLORS;

    // ===== 配置加载 / 恢复 / 保存（每个表格文件一份）=====
    // 切表时宿主按文件重挂载本视图，但 store 的 tableFile 会短暂停留在上一个文件（读盘异步）——
    // 用世代号让在途的旧表配置读取失效，避免把旧表配置套到新表上
    const loadGenRef = React.useRef(0);
    const loadConfig = React.useCallback(async (file: string, gen: number) => {
      const forms = await readForms();
      if (loadGenRef.current !== gen) return;
      const cfg = forms[file];
      if (cfg) {
        setConfig(cfg);
        setRowId(cfg.lastSelectedRowId);
        setConfigReady(true);
        return;
      }
      // 首次进入该表：按列序启用全部字段（含图片字段——图片字段不可填写，只展示与移除引用），零配置即可用
      const seeded = emptyConfig();
      const snapshot = table.snapshot();
      if (snapshot.tableFile !== file) return; // 读盘期间已切走：不播种、不落盘
      seeded.fields = snapshot.fields.map((f) => createFieldConfig(f, f.id));
      setConfig(seeded);
      setRowId(null);
      setConfigReady(true);
      await writeForm(file, () => seeded);
    }, []);

    React.useEffect(() => {
      if (stateRef.current.tableFile === tableFile) return;
      stateRef.current.tableFile = tableFile;
      stateRef.current.config = null;
      loadGenRef.current += 1;
      setEditingFieldId(null);
      setConflict(null);
      setSavedAt(0);
      setSaveState("idle");
      setNewRow(null);
      setConfig(null);
      setConfigReady(false);
      setRowId(null);
      if (!tableFile) return;
      void loadConfig(tableFile, loadGenRef.current);
    }, [tableFile, loadConfig]);

    stateRef.current.config = config;
    stateRef.current.currentRowId = rowId;

    const currentRow = rowId ? rows.find((r) => r.id === rowId) : undefined;

    // ===== 当前行被删除（可能由协作者删除）→ 回新增态并提示 =====
    // 只在「确认存在 → 确认消失」的跳变时触发，避免快照未跟上 rowId 时的误报
    const rowExistsRef = React.useRef(false);
    React.useEffect(() => {
      if (!rowId) {
        rowExistsRef.current = false;
        return;
      }
      if (rowExistsRef.current && !currentRow) {
        setRowId(null);
        setEditingFieldId(null);
        setSaveState("idle");
        setNewRow(null);
        setConflict("正在编辑的行已被删除（可能由其他协作者删除），已回到新增态");
      }
      rowExistsRef.current = !!currentRow;
    }, [rowId, currentRow]);

    // ===== 撤销窗口计时 =====
    React.useEffect(() => {
      if (!newRow) return;
      const t = setTimeout(() => setNow(() => Date.now()), UNDO_WINDOW_MS + 50);
      return () => clearTimeout(t);
    }, [newRow]);

    // 撤销只看新增行自身的内容签名：该行被填入内容（含协作写入）即不允许删除，其它行的变化不影响
    const newRowRow = newRow ? rows.find((r) => r.id === newRow.rowId) : undefined;
    const canUndoNewRow = !!newRow && now < newRow.undoUntil && rowValuesSignature(newRowRow) === newRow.rowSignature;

    // 把运行态发布给共享源（设置弹层据此渲染；未打开表格时收敛为 null）
    React.useEffect(() => {
      publish({ tableFile, fields, config });
    }, [tableFile, fields, config]);
    React.useEffect(() => () => publish(emptySnap), []);

    // 设置弹层可能先于/独立于本视图改动配置（同一份共享源）：配置变了就同步进来，
    // 否则视图会拿着旧配置渲染、并在下次发布时把弹层的改动覆盖掉
    const storeConfig = React.useSyncExternalStore(
      subscribeSnap,
      () => store.snap.config,
      () => null,
    );
    React.useEffect(() => {
      if (!configReady || !storeConfig) return;
      if (storeConfig === stateRef.current.config) return;
      setConfig(storeConfig);
    }, [storeConfig, configReady]);

    const pickRow = React.useCallback((nextRowId: string) => {
      const cfg = stateRef.current.config;
      const file = stateRef.current.tableFile;
      if (!cfg || !file) return;
      const snapshot = table.snapshot();
      const row = snapshot.rows.find((r) => r.id === nextRowId);
      if (!row) return;
      setEditingFieldId(null);
      setConflict(null);
      setSaveState("idle");
      setRowId(nextRowId);
      table.selectRow(nextRowId);
      void writeForm(file, (prev) => ({ ...(prev || emptyConfig()), lastSelectedRowId: nextRowId }));
    }, []);

    const writeFieldAt = React.useCallback((field: Field, raw: string, targetRowId: string | null): boolean => {
      const result = writeField(field, raw, targetRowId);
      if (result.conflict) {
        setConflict("新增失败：同一时刻检测到其它新增操作，本次未写入，请重试");
        setSaveState("idle");
        return false;
      }
      if (!result.ok) {
        if (result.error) setConflict(result.error);
        return false;
      }
      if (result.created && result.rowId) {
        const snapshot = table.snapshot();
        const index = snapshot.rows.findIndex((r) => r.id === result.rowId);
        const createdRow = snapshot.rows.find((r) => r.id === result.rowId);
        const newRowId = result.rowId;
        setRowId(newRowId);
        setNewRow({
          rowId: newRowId,
          undoUntil: Date.now() + UNDO_WINDOW_MS,
          rowSignature: rowValuesSignature(createdRow),
        });
        // 同步更新镜像：同一事件循环内的后续提交必须写这一新行，不能重复建行
        stateRef.current.currentRowId = newRowId;
        table.selectRow(newRowId);
        const file = stateRef.current.tableFile;
        if (file) void writeForm(file, (prev) => ({ ...(prev || emptyConfig()), lastSelectedRowId: newRowId }));
        setSaveState("created:" + (index >= 0 ? "第 " + (index + 1) + " 行" : "新行"));
      } else {
        setSaveState("saved");
      }
      setSavedAt(Date.now());
      return true;
    }, []);

    /** 单格提交：值与表格当前值相同则不写。 */
    const commitField = React.useCallback((field: Field, raw: string) => {
      const snapshot = table.snapshot();
      const currentRowId = stateRef.current.currentRowId;
      const targetRow = currentRowId ? snapshot.rows.find((r) => r.id === currentRowId) : undefined;
      if (targetRow && toDraftValue(field, targetRow.values[field.id]) === raw) {
        setEditingFieldId(null);
        return;
      }
      writeFieldAt(field, raw, currentRowId);
      setEditingFieldId(null);
    }, [writeFieldAt]);

    const cancelField = React.useCallback(() => {
      setEscapeSignal((n) => n + 1);
      setEditingFieldId(null);
    }, []);

    const clearField = React.useCallback((field: Field) => {
      const target = stateRef.current.currentRowId;
      if (target) table.updateCell(target, field.id, undefined);
      setEditingFieldId(null);
      setSavedAt(Date.now());
      setSaveState("saved");
    }, []);

    /** 移除图片字段单元格里的第 index 张图片引用（display 展示偏好保留）。 */
    const removeImageAt = React.useCallback((fieldId: string, index: number) => {
      const target = stateRef.current.currentRowId;
      if (!target) return;
      const snapshot = table.snapshot();
      const row = snapshot.rows.find((r) => r.id === target);
      const field = snapshot.fields.find((f) => f.id === fieldId);
      if (!row || !field) return;
      const images = readImageEntries(field, row.values[fieldId]);
      if (index < 0 || index >= images.length) return;
      const next = images.slice();
      next.splice(index, 1);
      const display = asRecord(row.values[fieldId])?.display === "grid" ? "grid" : undefined;
      try {
        table.updateCell(target, fieldId, display ? { images: next, display } : { images: next });
      } catch (e) {
        console.error("[table-form] 移除图片失败", e);
        ctx.notification.notify({ message: "移除图片失败，请重试", level: "error" });
        return;
      }
      setSavedAt(Date.now());
      setSaveState("saved");
    }, []);

    const moveRow = React.useCallback((delta: number) => {
      if (rows.length === 0) return;
      const current = rowId ? rows.findIndex((r) => r.id === rowId) : delta > 0 ? -1 : 0;
      const next = (current + delta + rows.length) % rows.length;
      pickRow(rows[next].id);
    }, [rows, rowId, pickRow]);

    // 进入某表格时默认载入第一行（无行数据则保持新增态）：
    // 让表单视图一打开就有明确的行号与内容。每个表格文件只做一次——用户主动切到新增态后不再抢回。
    const defaultPickedForRef = React.useRef<string | null>(null);
    React.useEffect(() => {
      if (!tableFile || !configReady) return;
      if (defaultPickedForRef.current === tableFile) return;
      defaultPickedForRef.current = tableFile;
      if (rowId || rows.length === 0) return;
      pickRow(rows[0].id);
    }, [tableFile, configReady, rowId, rows, pickRow]);

    const fieldRows = React.useMemo(() => {
      const byId = new Map(fields.map((field) => [field.id, field]));
      const out: Array<{ target: FormFieldConfig; field: Field; image: boolean }> = [];
      for (const target of config ? config.fields : []) {
        const field = byId.get(target.fieldId);
        if (field) out.push({ target, field, image: !isFillable(field) });
      }
      return out;
    }, [config, fields]);

    const addRowExplicitly = React.useCallback(() => {
      const created = addRowAndGetId();
      if (created === null) {
        setConflict("新增失败：同一时刻检测到其它新增操作，本次未新增，请重试");
        return;
      }
      setNewRow(null);
      setConflict(null);
      setSaveState("created:新行");
      setSavedAt(Date.now());
      pickRow(created);
    }, [pickRow]);

    const currentRowIndex = rowId ? rows.findIndex((r) => r.id === rowId) + 1 : 0;

    const undoNewRow = React.useCallback(() => {
      if (!newRow) return;
      const removedId = newRow.rowId;
      // 先离开该行再删除，避免触发行删除检测的「已被协作者删除」提示
      rowExistsRef.current = false;
      setRowId(null);
      setNewRow(null);
      setSaveState("idle");
      setConflict(null);
      table.removeRow(removedId);
    }, [newRow]);

    if (!tableFile) {
      return h("div", { style: EMPTY_VIEW_STYLE }, h(EmptyState, { key: "e", icon: h(FormIcon, { size: 20 }), title: "未打开表格" }));
    }
    if (!snap || !config) {
      return h("div", { style: EMPTY_VIEW_STYLE }, h(EmptyState, { key: "e", icon: h(FormIcon, { size: 20 }), title: "正在载入表单…" }));
    }

    const peerColor = (rowId && peerColorByRowId[rowId]) || null;
    const rowValue = (field: Field): string => (currentRow ? toDraftValue(field, currentRow.values[field.id]) : "");

    // ===== 顶栏：行选择 + 翻行 + 新增 + 协作提示 =====
    const renderTopBar = () =>
      h(
        "div",
        { key: "bar", style: { display: "flex", alignItems: "center", gap: 8, padding: "8px 12px", borderBottom: "1px solid " + borderColor, flexShrink: 0, flexWrap: "wrap" } },
        [
          h(RowPicker, { key: "picker", rows, fields, selectedRowId, currentRowId: rowId, onPick: pickRow }),
          h("div", { key: "nav", style: { display: "flex", alignItems: "center", gap: 2 } }, [
            h(IconButton, { key: "prev", variant: "ghost", size: "sm", label: "上一行", icon: h(ArrowIcon, { size: 14, dir: "left" }), disabled: rows.length === 0, onClick: () => moveRow(-1) }),
            h(IconButton, { key: "next", variant: "ghost", size: "sm", label: "下一行", icon: h(ArrowIcon, { size: 14, dir: "right" }), disabled: rows.length === 0, onClick: () => moveRow(1) }),
          ]),
          h(Button, { key: "new", variant: "ghost", size: "sm", onClick: addRowExplicitly, title: "新增一行空行" }, [
            h(PlusIcon, { key: "i", size: 12 }),
            h("span", { key: "t" }, "新增一行"),
          ]),
          h("span", { key: "sp", style: { flex: 1 } }),
          peerColor
            ? h("span", { key: "peers", style: { display: "flex", alignItems: "center", gap: 6, fontSize: FS.micro, lineHeight: LH.micro, color: textMuted } }, [
                h("span", { key: "dot", style: { width: 6, height: 6, borderRadius: RADIUS_FULL, background: peerColor } }),
                h("span", { key: "t" }, "他人正在编辑本行"),
              ])
            : null,
        ],
      );

    // ===== 内容头：行号 + 总数；空表引导 =====
    const renderHeader = () =>
      h("div", { style: { display: "flex", flexDirection: "column", gap: 8 } }, [
        h("div", { key: "t", style: { display: "flex", alignItems: "baseline", gap: 8 } }, [
          h("span", { key: "n", style: { fontSize: FS.h2, lineHeight: LH.h2, fontWeight: 600, color: textPrimary } }, rowId && currentRowIndex > 0 ? "第 " + currentRowIndex + " 行" : "新增行"),
          h("span", { key: "r", style: { fontSize: FS.caption, lineHeight: LH.caption, color: textMuted, fontVariantNumeric: "tabular-nums" } }, "共 " + rows.length + " 行"),
        ]),
        rows.length === 0
          ? h(
              "div",
              { key: "er", style: { display: "flex", alignItems: "center", gap: 8, padding: "8px 12px", borderRadius: RADIUS.sm, border: "1px dashed " + borderColor, fontSize: FS.caption, lineHeight: LH.caption, color: textMuted } },
              [
                h(FormIcon, { key: "i", size: 16 }),
                h("span", { key: "t", style: { flex: 1 } }, "表格还没有数据行——填写下方第一个值，即会自动在表格末尾新增一行"),
                h(Button, { key: "b", variant: "ghost", size: "sm", onClick: addRowExplicitly }, "新增空行"),
              ],
            )
          : rowId
            ? null
            : h("div", { key: "s", style: { fontSize: FS.caption, lineHeight: LH.caption, color: textMuted } }, "填写第一个值时会自动在表格末尾新增一行，之后逐格写入"),
      ]);

    const renderConflict = () =>
      h(
        "div",
        { style: { display: "flex", alignItems: "center", gap: 8, padding: "8px 12px", fontSize: FS.caption, lineHeight: LH.caption, borderRadius: RADIUS.sm, border: "1px solid " + borderColor, background: "color-mix(in srgb, var(--accent) 6%, var(--bg-secondary))", color: textPrimary } },
        [
          h("span", { key: "t", style: { flex: 1 } }, conflict),
          h(Button, { key: "x", variant: "secondary", size: "sm", onClick: () => setConflict(null) }, "知道了"),
        ],
      );

    const renderFooter = () =>
      h("div", { style: { display: "flex", alignItems: "center", gap: 8, marginTop: 4, flexWrap: "wrap" } }, [
        savedAt > 0
          ? h(StatusPill, {
              key: "st",
              color: "var(--success)",
              label: saveState === "saved" ? "已写入表格" : "已" + saveState.slice("created:".length) + "新增，继续填写",
            })
          : null,
        newRow && canUndoNewRow
          ? h(Button, { key: "undo", variant: "ghost", size: "sm", onClick: undoNewRow, title: "撤销本次新增（该行内容未被改动时才可用）" }, [
              h(UndoIcon, { key: "i", size: 12 }),
              h("span", { key: "t" }, "撤销本次新增"),
            ])
          : null,
      ]);

    const renderFieldRow = (item: { target: FormFieldConfig; field: Field; image: boolean }) =>
      item.image
        ? h(ImageRow, {
            key: item.field.id,
            name: item.field.name,
            entries: readImageEntries(item.field, currentRow ? currentRow.values[item.field.id] : undefined),
            hasRow: !!currentRow,
            canRemove: !!currentRow,
            onRemove: (index: number) => removeImageAt(item.field.id, index),
          })
        : h(FieldRow, {
            key: item.field.id,
            field: item.field,
            hint: item.target.hint,
            display: rowValue(item.field),
            editing: editingFieldId === item.field.id,
            escapeSignal,
            canUseButtons: !!rowId,
            selectStyle: item.target.selectStyle || "radio",
            onEnterEdit: () => setEditingFieldId(item.field.id),
            onCancel: cancelField,
            onCommit: (raw: string) => commitField(item.field, raw),
            onClear: () => clearField(item.field),
          });

    return h("div", { style: { flex: 1, minHeight: 0, display: "flex", flexDirection: "column" } }, [
      renderTopBar(),
      // 主体：内容列是唯一滚动容器，两个翻行区作为其内部成员 ——
      // 滑动条落在滚动区最右缘（两箭头都在其左侧），箭头用 sticky 铺满可视高度且不随内容滚走
      h("div", { key: "body", style: { flex: 1, minHeight: 0, overflow: "hidden", display: "flex", alignItems: "stretch" } }, [
        h("div", { key: "scroll", style: SCROLL_AREA_STYLE }, [
          h(HoverArea, {
            key: "prev",
            areaStyle: NAV_AREA_STYLE,
            title: "上一行",
            onClick: () => moveRow(-1),
            children: h(ArrowIcon, { size: 26, dir: "left" }),
          }),
          h("div", { key: "content", style: { flex: 1, minWidth: 0, padding: 16 } }, [
            h("div", { key: "card", style: CARD_STYLE }, [
              renderHeader(),
              conflict ? renderConflict() : null,
              fieldRows.length === 0
                ? h(EmptyState, {
                    key: "empty",
                    compact: true,
                    icon: h(FormIcon, { size: 20 }),
                    title: "当前表格没有可显示的字段",
                    description: "用表格工具条右上角的「表单设置」配置参与填写的字段",
                  })
                : fieldRows.map(renderFieldRow),
              renderFooter(),
            ]),
          ]),
          h(HoverArea, {
            key: "next",
            areaStyle: NAV_AREA_STYLE,
            title: "下一行",
            onClick: () => moveRow(1),
            children: h(ArrowIcon, { size: 26, dir: "right" }),
          }),
        ]),
      ]),
    ]);
  }

  function describeType(field: Field): string {
    if (field.type === "text") return "文本";
    if (field.type === "number") return "数字";
    if (field.type === "duration") return "时长";
    if (field.type === "singleSelect") return "单选";
    if (field.type === "image") return "图片";
    return field.type;
  }

  interface FieldRowProps {
    field: Field;
    hint: string | undefined;
    display: string;

    editing: boolean;

    escapeSignal: number;
    canUseButtons: boolean;
    selectStyle: "radio" | "dropdown";

    onEnterEdit: () => void;
    onCancel: () => void;
    onCommit: (raw: string) => void;
    onClear: () => void;
  }

  /**
   * 单字段行：输入框常驻但**只读**，双击进入编辑（与宿主表格一致）；失焦（或文本框 Ctrl/⌘+Enter）
   * 即写入表格，Esc 放弃本次编辑；无保存/提交按钮。单选类没有「只读/编辑」两态，点选项或改下拉即写。
   * 编辑中的输入框不采纳外部值回流——协作者写入不得覆盖正在输入的本地值。
   */
  function FieldRow(props: FieldRowProps) {
    const [input, setInput] = React.useState(props.display);
    const [hovered, setHovered] = React.useState(false);
    /** Esc 放弃标记：紧随其后的失焦不得再提交。 */
    const escapedRef = React.useRef(false);
    const focusRef = (el: unknown) => {
      const node = el as { focus?: () => void } | null;
      if (props.editing && node && node.focus) node.focus();
    };
    React.useEffect(() => {
      // 非编辑态才采纳外部值（含协作者写入），避免覆盖正在输入的本地值
      if (!props.editing) setInput(props.display);
    }, [props.display, props.editing]);
    React.useEffect(() => {
      // 进入编辑 / Esc 放弃：以表格当前值重置本地值
      if (props.editing) setInput(props.display);
      // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [props.editing, props.escapeSignal]);

    const labelNode = h("div", { key: "label", style: FIELD_LABEL_STYLE }, [
      h("span", { key: "n", style: { fontSize: FS.caption, lineHeight: LH.caption, color: textSecondary, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" } }, props.field.name),
      h("span", { key: "ty", style: { fontSize: FS.micro, lineHeight: LH.micro, color: textMuted } }, describeType(props.field)),
    ]);
    // 编辑态即焦点态：进入编辑 = 聚焦（focusRef 同步落焦），故焦点样式直接由 editing 驱动
    const controlStyle = { ...inputStyle(props.editing), cursor: props.editing ? "text" : "default" };
    const onEsc = (e: { key: string; stopPropagation: () => void }) => {
      if (e.key !== "Escape") return;
      e.stopPropagation();
      escapedRef.current = true;
      props.onCancel();
    };
    const onBlur = () => {
      if (escapedRef.current) {
        escapedRef.current = false;
        return;
      }
      if (!props.editing) return; // 非编辑态的聚焦（双击第一下）不提交
      props.onCommit(input);
    };

    const onDoubleClick = () => {
      if (!props.editing) props.onEnterEdit();
    };

    let control: unknown;
    if (props.field.type === "text" || (props.field.type !== "number" && props.field.type !== "duration" && props.field.type !== "singleSelect")) {
      control = h("textarea", {
        key: "e",
        rows: 3,
        readOnly: !props.editing,
        ref: props.editing ? focusRef : undefined,
        value: input,
        placeholder: props.hint || "（空）",
        title: props.editing ? undefined : "双击编辑",
        onDoubleClick,
        onChange: (e: { target: { value: string } }) => setInput(e.target.value),
        onBlur,
        onKeyDown: (e: { key: string; ctrlKey: boolean; metaKey: boolean; preventDefault: () => void; stopPropagation: () => void }) => {
          onEsc(e);
          if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) {
            e.preventDefault();
            props.onCommit(input);
          }
        },
        style: { ...controlStyle, padding: "6px 8px", resize: "vertical", fontFamily: "inherit" },
      });
    } else if (props.field.type === "number" || props.field.type === "duration") {
      // 只把合法数字串交给 number 输入（否则该输入无法显示、React 报受控告警）
      const numeric = input.trim() !== "" && Number.isFinite(Number(input)) ? input : "";
      control = h("div", { key: "e", onDoubleClick, style: { display: "flex", alignItems: "center", gap: 6 } }, [
        h("input", {
          key: "i",
          type: "number",
          readOnly: !props.editing,
          ref: props.editing ? focusRef : undefined,
          value: numeric,
          placeholder: props.hint || (props.field.type === "duration" ? "秒" : ""),
          title: props.editing ? undefined : "双击编辑",
          onChange: (e: { target: { value: string } }) => setInput(e.target.value),
          onBlur,
          onKeyDown: onEsc,
          style: { ...controlStyle, width: 200 },
        }),
        props.field.type === "duration" ? h("span", { key: "u", style: { fontSize: FS.caption, lineHeight: LH.caption, color: textMuted } }, "秒") : null,
      ]);
    } else if (props.selectStyle === "dropdown") {
      control = h(SelectField, {
        key: "e",
        value: input,
        onChange: (value: string) => {
          setInput(value);
          props.onCommit(value); // 点选即写
        },
        options: [{ value: "", label: "（空）" }].concat(
          (props.field.options || []).map((option) => ({ value: option, label: option })),
        ),
      });
    } else {
      // 选项胶囊：点选即写；「清空」显式置空。新增态下不响应（避免误点建行）
      const options = props.field.options || [];
      control = h(
        "div",
        { key: "e", style: { display: "flex", flexWrap: "wrap", alignItems: "center", gap: 6, opacity: props.canUseButtons ? 1 : 0.5 } },
        [
          h(SelectChip, {
            key: "__clear",
            selected: input === "",
            disabled: !props.canUseButtons,
            onClick: props.onClear,
          }, "清空"),
        ].concat(
          options.map((option) =>
            h(SelectChip, {
              key: option,
              selected: input === option,
              disabled: !props.canUseButtons,
              onClick: () => props.onCommit(option),
            }, option),
          ),
        ),
      );
    }
    return h(
      "div",
      {
        onMouseEnter: () => setHovered(true),
        onMouseLeave: () => setHovered(false),
        style: fieldRowStyle(props.editing, hovered),
      },
      [labelNode, h("div", { key: "c", style: { flex: 1, minWidth: 0 } }, [control])],
    );
  }

  function HoverArea(props: { areaStyle: Style; title: string; onClick: () => void; children: unknown }) {
    const [hover, setHover] = React.useState(false);
    return h(
      "button",
      {
        type: "button",
        title: props.title,
        onClick: props.onClick,
        onMouseEnter: () => setHover(true),
        onMouseLeave: () => setHover(false),
        style: {
          ...props.areaStyle,
          background: hover ? "var(--hover)" : "transparent",
          color: hover ? textPrimary : "transparent",
          cursor: "pointer",
        },
      },
      h("span", { style: { display: "flex", alignItems: "center", justifyContent: "center", opacity: hover ? 1 : 0, transition: "opacity " + EASE } }, props.children),
    );
  }

  interface ImageRowProps {
    name: string;
    entries: string[];
    hasRow: boolean;
    canRemove: boolean;
    onRemove: (index: number) => void;
  }

  /** 图片字段行：无添加通道（图片字节由宿主导入附件目录），选中已有行时可移除单张图片引用。 */
  function ImageRow(props: ImageRowProps) {
    return h("div", { style: fieldRowStyle(false, false) }, [
      h("div", { key: "label", style: FIELD_LABEL_STYLE }, [
        h("span", { key: "n", style: { fontSize: FS.caption, lineHeight: LH.caption, color: textSecondary, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" } }, props.name),
        h("span", { key: "ty", style: { fontSize: FS.micro, lineHeight: LH.micro, color: textMuted } }, "图片"),
      ]),
      h("div", { key: "c", style: { flex: 1, minWidth: 0 } }, [
        h(ImagePreview, { key: "img", entries: props.entries, hasRow: props.hasRow, canRemove: props.canRemove, onRemove: props.onRemove }),
      ]),
    ]);
  }

  ctx.slots.registerTableView({ kind: PLUGIN_ID, label: "表格表单", component: FormView });
  // 表格工具条右上角「表单设置」入口（浮层）：宿主表格面板的 `···` 菜单无插件接入口，故经本槽位贡献
  ctx.slots.registerUi({ slot: "toolbar/table/right", component: FormSettingsMenu });
}
