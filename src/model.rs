//! 数据模型与表格数据来源接缝。
//!
//! 界面壳不自己解析 `.atb`：表格真源经 [`TableProvider`] 接缝进出，UI 与写值路径只认这个接缝。
//! 本模块只认「值的 JSON 形态」，不认识宿主 Context——对接实现见 `host_table.rs`。
//!
//! 宿主存值口径：text = 字符串、number / duration = 数字（时长单位是秒）、singleSelect = 选项文案、
//! image = `{images: [仓库相对路径], display?}`；缺键 = 空单元格。写回按同一口径编码：文本 / 数字 /
//! 时长 / 单选的空值编成 `null`（宿主据此删键），图片格的空值是空列表。图片原值里除 `images`
//! 外的键（如九宫格显示模式）必须原样带回，否则移除一张图会顺手改掉该格的显示模式。

use std::collections::BTreeMap;

use serde_json::{Map, Value};

/// 字段类型：决定界面渲染哪种控件、单元格存哪种值。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FieldType {
    Text,
    Number,
    Duration,
    SingleSelect,
    Image,
    /// 宿主未列举（或将来新增）的类型：原样保留名字，界面只读。
    Other(String),
}

/// 一个字段定义。
#[derive(Clone, Debug)]
pub struct Field {
    pub id: String,
    pub name: String,
    pub kind: FieldType,
    /// 仅 [`FieldType::SingleSelect`] 使用：可选项文案（按下标存取）。
    pub options: Vec<String>,
}

/// 单元格值：与 [`FieldType`] 一一对应。
#[derive(Clone, Debug, PartialEq)]
pub enum CellValue {
    /// 文本（原串，格式由用户自持）。
    Text(String),
    Number(Option<f64>),
    /// 时长：存秒数，界面按 `1:30:00` 读写。
    Duration(Option<f64>),
    /// 单选：`None` = 未选；`Some` = 选项文案。
    Choice(Option<String>),
    Images(ImageCell),
    /// 与字段类型不符或类型未知的存值：只读投影，永不写回（不猜测改写用户数据）。
    Foreign(Value),
}

/// 图片单元格：路径列表 + 原值里除 `images` 外的键（写回时原样带回）。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ImageCell {
    pub paths: Vec<String>,
    pub extra: Map<String, Value>,
}

impl ImageCell {
    /// 去掉第 `index` 张：越界回 `None`（界面已过期时不猜用户想删哪张）；其余路径与 `extra` 不动。
    pub fn without(&self, index: usize) -> Option<Self> {
        if index >= self.paths.len() {
            return None;
        }
        let mut next = self.clone();
        next.paths.remove(index);
        Some(next)
    }
}

impl FieldType {
    /// 宿主协议的字段类型名 → 插件类型；未列举的按原名收进 [`FieldType::Other`]。
    pub fn from_wire(name: &str) -> Self {
        match name {
            "text" => FieldType::Text,
            "number" => FieldType::Number,
            "duration" => FieldType::Duration,
            "singleSelect" => FieldType::SingleSelect,
            "image" => FieldType::Image,
            other => FieldType::Other(other.to_string()),
        }
    }

    /// 该类型字段的空值。
    pub fn empty_cell(&self) -> CellValue {
        match self {
            FieldType::Text => CellValue::Text(String::new()),
            FieldType::Number => CellValue::Number(None),
            FieldType::Duration => CellValue::Duration(None),
            FieldType::SingleSelect => CellValue::Choice(None),
            FieldType::Image => CellValue::Images(ImageCell::default()),
            FieldType::Other(_) => CellValue::Foreign(Value::Null),
        }
    }
}

impl CellValue {
    /// 按字段类型解码宿主存值：缺键按空值，形态不符收进 [`CellValue::Foreign`]。
    pub fn from_wire(kind: &FieldType, raw: Option<&Value>) -> Self {
        match kind {
            FieldType::Text => text_cell(raw),
            FieldType::Number | FieldType::Duration => number_cell(kind, raw),
            FieldType::SingleSelect => choice_cell(raw),
            FieldType::Image => image_cell(raw),
            FieldType::Other(_) => CellValue::Foreign(raw.cloned().unwrap_or(Value::Null)),
        }
    }

    /// 编码成宿主 `updateCell` 的 value；`None` = 不可写。文本 / 数字 / 时长 / 单选的空值编成
    /// `null`（宿主删键）。
    pub fn to_wire(&self) -> Option<Value> {
        match self {
            CellValue::Text(s) if s.is_empty() => Some(Value::Null),
            CellValue::Text(s) => Some(Value::String(s.clone())),
            CellValue::Number(v) | CellValue::Duration(v) => match v {
                None => Some(Value::Null),
                // 非有限数编出去只剩 `null`，等于删键丢内容；这类值不写
                Some(n) if n.is_finite() => Some(number_wire(*n)),
                Some(_) => None,
            },
            CellValue::Choice(Some(option)) => Some(Value::String(option.clone())),
            CellValue::Choice(None) => Some(Value::Null),
            CellValue::Images(cell) => {
                let mut object = cell.extra.clone();
                let list = cell.paths.iter().cloned().map(Value::String).collect();
                object.insert("images".to_string(), Value::Array(list));
                Some(Value::Object(object))
            }
            CellValue::Foreign(_) => None,
        }
    }

    /// 文本控件取值：时长按 `1:30:00` 显示，数字转文本，Foreign 给可辨识的原文。
    pub fn as_edit_text(&self) -> String {
        match self {
            CellValue::Text(s) => s.clone(),
            CellValue::Number(v) => v.map(trim_number).unwrap_or_default(),
            CellValue::Duration(v) => v.map(format_duration).unwrap_or_default(),
            CellValue::Choice(option) => option.clone().unwrap_or_default(),
            CellValue::Images(cell) => cell.paths.first().cloned().unwrap_or_default(),
            CellValue::Foreign(value) => foreign_text(value),
        }
    }

    pub fn as_number(&self) -> Option<f64> {
        match self {
            CellValue::Number(v) => *v,
            _ => None,
        }
    }

    pub fn images(&self) -> &[String] {
        match self {
            CellValue::Images(cell) => &cell.paths,
            _ => &[],
        }
    }
}

/// 数字与时长共用数字存法，按字段类型落回对应单元格值。
fn cell_of_seconds(kind: &FieldType, seconds: f64) -> CellValue {
    match kind {
        FieldType::Duration => CellValue::Duration(Some(seconds)),
        _ => CellValue::Number(Some(seconds)),
    }
}

fn text_cell(raw: Option<&Value>) -> CellValue {
    match raw {
        Some(Value::String(s)) => CellValue::Text(s.clone()),
        // 缺键与 null 都是空文本
        None | Some(Value::Null) => CellValue::Text(String::new()),
        Some(other) => CellValue::Foreign(other.clone()),
    }
}

fn number_cell(kind: &FieldType, raw: Option<&Value>) -> CellValue {
    match raw {
        // 空串与缺键、null 都是空值：宿主把空串与缺键视为等值
        None | Some(Value::Null) => kind.empty_cell(),
        Some(Value::String(text)) if text.is_empty() => kind.empty_cell(),
        Some(Value::Number(n)) => match n.as_f64().filter(|v| v.is_finite()) {
            Some(v) => cell_of_seconds(kind, v),
            None => CellValue::Foreign(raw.cloned().unwrap_or(Value::Null)),
        },
        Some(other) => CellValue::Foreign(other.clone()),
    }
}

fn choice_cell(raw: Option<&Value>) -> CellValue {
    match raw {
        None | Some(Value::Null) => CellValue::Choice(None),
        Some(Value::String(option)) if option.is_empty() => CellValue::Choice(None),
        Some(Value::String(option)) => CellValue::Choice(Some(option.clone())),
        Some(other) => CellValue::Foreign(other.clone()),
    }
}

fn image_cell(raw: Option<&Value>) -> CellValue {
    let Some(raw) = raw.filter(|v| !v.is_null()) else {
        return CellValue::Images(ImageCell::default());
    };
    if let Some(list) = raw.as_array() {
        return image_list(list, Map::new(), raw);
    }
    match raw.get("images").and_then(Value::as_array) {
        Some(list) => {
            let mut extra = raw.as_object().cloned().unwrap_or_default();
            extra.remove("images");
            image_list(list, extra, raw)
        }
        None => CellValue::Foreign(raw.clone()),
    }
}

/// 把图片引用数组解成 [`ImageCell`]；数组里混着非字符串条目时整格收
/// [`CellValue::Foreign`]——只挑字符串解会把其余条目在写回时悄悄删掉。
/// `extra` 是该格除路径外的原键（裸数组形态没有原键，传空表）。
fn image_list(list: &[Value], extra: Map<String, Value>, raw: &Value) -> CellValue {
    match list.iter().map(|v| v.as_str().map(str::to_string)).collect::<Option<Vec<String>>>() {
        Some(paths) => CellValue::Images(ImageCell { paths, extra }),
        None => CellValue::Foreign(raw.clone()),
    }
}

/// 整数按整数存，与宿主数值口径一致（避免 `8` 与 `8.0` 反复判成改动）。
fn number_wire(n: f64) -> Value {
    if n.fract() == 0.0 && n.abs() < SAFE_INT {
        Value::from(n as i64)
    } else {
        Value::from(n)
    }
}

/// 能精确转成 `i64` 的量级上限：超过它 `as i64` 会饱和成另一个数，把用户的数据改写掉。
const SAFE_INT: f64 = 9.0e15;

/// 数字文本：整数不带小数点，超出精确范围时保留最短科学计数法原文。
fn trim_number(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < SAFE_INT {
        format!("{}", n as i64)
    } else {
        n.to_string()
    }
}

/// 秒 → 时长文本。带小数或超出精确范围的秒原样显示，免得格式化抹掉精度后再写回。
pub fn format_duration(seconds: f64) -> String {
    if seconds.fract() != 0.0 || !seconds.is_finite() || seconds.abs() >= SAFE_INT {
        return trim_number(seconds);
    }
    let sign = if seconds < 0.0 { "-" } else { "" };
    let total = seconds.abs() as i64;
    let (h, m, s) = (total / 3600, total % 3600 / 60, total % 60);
    if h > 0 {
        format!("{sign}{h}:{m:02}:{s:02}")
    } else if m > 0 {
        format!("{sign}{m}:{s:02}")
    } else {
        format!("{sign}{s}")
    }
}

/// 时长文本 → 秒。接受 `1:30:00`、`30:00`、`90`，允许整体带负号；只有最末一段允许小数。
pub fn parse_duration(text: &str) -> Option<f64> {
    let text = text.trim();
    let (sign, body) = match text.strip_prefix('-').filter(|rest| !rest.is_empty()) {
        Some(rest) => (-1.0, rest),
        None => (1.0, text),
    };
    let parts: Vec<&str> = body.split(':').collect();
    if parts.len() > 3 || parts.iter().any(|part| part.trim().is_empty()) {
        return None;
    }
    let mut total = 0.0;
    for (i, part) in parts.iter().enumerate() {
        let value = part.trim().parse::<f64>().ok()?;
        if value < 0.0 || !value.is_finite() {
            return None;
        }
        if i + 1 < parts.len() && value.fract() != 0.0 {
            return None;
        }
        let unit = match parts.len() - i {
            3 => 3600.0,
            2 => 60.0,
            _ => 1.0,
        };
        total += value * unit;
    }
    // 每段都有限不代表合计有限：`1e308:00` 乘上单位就溢出，溢出值写出去等于删键
    if !total.is_finite() {
        return None;
    }
    Some(total * sign)
}

/// 可导入图片格的文件扩展名 → 仓库附件统一扩展名（白名单外拒绝，与宿主图片附件口径一致）。
pub fn image_ext_of(file_name: &str) -> Option<&'static str> {
    let ext = std::path::Path::new(file_name).extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some("png"),
        "jpg" | "jpeg" => Some("jpg"),
        "webp" => Some("webp"),
        "gif" => Some("gif"),
        _ => None,
    }
}

fn foreign_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(true) => "是".to_string(),
        Value::Bool(false) => "否".to_string(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// 一行记录：字段 id → 单元格值（缺字段按类型空值处理）。
#[derive(Clone, Debug)]
pub struct Row {
    pub id: String,
    pub cells: BTreeMap<String, CellValue>,
}

impl Row {
    /// 取某字段当前值；缺项返回该类型空值。
    pub fn cell(&self, field: &Field) -> CellValue {
        self.cells.get(&field.id).cloned().unwrap_or_else(|| field.kind.empty_cell())
    }
}

/// 表格快照：一次读取到的字段结构与行数据，界面与写值都以它为投影真源。
#[derive(Clone, Debug, Default)]
pub struct TableSnapshot {
    /// 表格文件的仓库相对路径；`table:changed` 按它判断是不是同一张表。
    pub path: String,
    /// 表格稳定 id（宿主分配，改名 / 移动后不变）；表单配置与图片附件目录都按它落位。
    pub id: Option<String>,
    pub fields: Vec<Field>,
    pub rows: Vec<Row>,
    /// 快照时刻用户在网格里正选中的行（整行选区才收，其余选区形态不进表单）。
    pub selected_row: Option<String>,
}

impl TableSnapshot {
    /// 是否处于「宿主已打开某张表格」的状态（未打开时快照为默认值）。
    pub fn is_open(&self) -> bool {
        !self.path.is_empty()
    }

    /// 顶栏标题用的表名：路径末段去掉 `.atb` 后缀。
    pub fn display_name(&self) -> String {
        let tail = self.path.rsplit(['/', '\\']).next().unwrap_or_default();
        tail.strip_suffix(".atb").unwrap_or(tail).to_string()
    }

    pub fn row_index_of(&self, row_id: &str) -> Option<usize> {
        self.rows.iter().position(|r| r.id == row_id)
    }
}

/// 数据接缝：宿主表格能力面向插件侧的抽象。所有方法以 `Result<_, String>` 回可读失败原因
/// （供上层转成用户通知，不静默）。`add_row` 必须返回新行 id，界面据此精确定位新增行——
/// 不靠行集差集猜（差集不唯一时写错行的风险不可接受）。
#[async_trait::async_trait]
pub trait TableProvider: Send + Sync {
    /// 读当前打开表格的快照；`Ok(None)` = 宿主当前没有打开表格。
    async fn snapshot(&self) -> Result<Option<TableSnapshot>, String>;
    async fn update_cell(&self, row_id: &str, field_id: &str, value: &CellValue) -> Result<(), String>;
    async fn add_row(&self) -> Result<String, String>;
    /// 删除一行（宿主有撤销栈兜底）。
    async fn remove_row(&self, row_id: &str) -> Result<(), String>;
    /// 写宿主选中态：翻行 / 新增后让网格视图跟上，切回去看到的是同一行。
    async fn select_row(&self, row_id: &str) -> Result<(), String>;
    /// 把本机图片文件导入表格附件目录（`.atelyx/temp/tables/<表格 id>/`，删表时宿主连带清理），
    /// 返回仓库相对路径；`Ok` 之前文件已确实落到仓库。非图片扩展名拒绝。
    async fn import_image(&self, table_id: &str, source: &str) -> Result<String, String>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn numeric_cells_follow_host_shape() {
        // 整数存整数、清空存 null，与宿主 number_value / 删键口径一致
        assert_eq!(number_wire(8.0), json!(8));
        assert_eq!(number_wire(8.5), json!(8.5));
        assert_eq!(CellValue::Number(None).to_wire(), Some(json!(null)));
        assert_eq!(CellValue::Text(String::new()).to_wire(), Some(json!(null)));
        assert_eq!(CellValue::Choice(None).to_wire(), Some(json!(null)));
        // 空串文本 ≈ 缺键，不写空串出去
        assert_eq!(CellValue::Number(Some(8.0)).to_wire(), Some(json!(8)));
    }

    #[test]
    fn cells_decode_absent_and_mismatched_values() {
        assert_eq!(CellValue::from_wire(&FieldType::Text, None), CellValue::Text(String::new()));
        assert_eq!(CellValue::from_wire(&FieldType::Number, Some(&json!(""))), CellValue::Number(None));
        assert_eq!(
            CellValue::from_wire(&FieldType::Duration, Some(&json!(90))),
            CellValue::Duration(Some(90.0))
        );
        assert_eq!(
            CellValue::from_wire(&FieldType::SingleSelect, Some(&json!("进行中"))),
            CellValue::Choice(Some("进行中".into()))
        );
        // 数字格里冒出字符串：不猜、不改写，收进只读的 Foreign
        assert_eq!(
            CellValue::from_wire(&FieldType::Number, Some(&json!("abc"))),
            CellValue::Foreign(json!("abc"))
        );
        assert_eq!(CellValue::Foreign(json!("abc")).to_wire(), None);
    }

    #[test]
    fn image_cell_keeps_extra_keys_on_write() {
        let raw = json!({"images": ["a/atb/x.png", "y.png"], "display": "grid"});
        let cell = CellValue::from_wire(&FieldType::Image, Some(&raw));
        let CellValue::Images(image) = &cell else {
            panic!("图片格应解成 ImageCell");
        };
        assert_eq!(image.paths, ["a/atb/x.png", "y.png"]);
        // 移除首张后写回仍带 display，不会把该格显示模式改掉
        let mut kept = image.clone();
        kept.paths.remove(0);
        assert_eq!(
            CellValue::Images(kept).to_wire(),
            Some(json!({"display": "grid", "images": ["y.png"]}))
        );
    }

    #[test]
    fn bare_image_array_is_readable() {
        let cell = CellValue::from_wire(&FieldType::Image, Some(&json!(["p.png"])));
        assert_eq!(cell.images(), ["p.png"]);
        assert_eq!(cell.to_wire(), Some(json!({"images": ["p.png"]})));
    }

    #[test]
    fn image_list_with_non_string_entry_stays_unwritable() {
        // 数组里混着非字符串条目：整格收只读。只挑字符串解会在写回时把那条内容悄悄删掉
        let raw = json!({"images": ["a.png", 123], "display": "grid"});
        let cell = CellValue::from_wire(&FieldType::Image, Some(&raw));
        assert_eq!(cell, CellValue::Foreign(raw));
        assert_eq!(cell.to_wire(), None);
        let bare = CellValue::from_wire(&FieldType::Image, Some(&json!(["a.png", false])));
        assert_eq!(bare, CellValue::Foreign(json!(["a.png", false])));
    }

    #[test]
    fn duration_text_and_seconds_roundtrip() {
        assert_eq!(parse_duration("1:30:00"), Some(5400.0));
        assert_eq!(parse_duration("30:00"), Some(1800.0));
        assert_eq!(parse_duration("90"), Some(90.0));
        assert_eq!(parse_duration(" 1:2:3 "), Some(3723.0));
        assert_eq!(parse_duration("1:30:45.5"), Some(5445.5));
        assert_eq!(format_duration(5400.0), "1:30:00");
        assert_eq!(format_duration(1800.0), "30:00");
        assert_eq!(format_duration(45.0), "45");
        // 带小数的秒原样显示，不会被格式化抹平后再写回
        assert_eq!(format_duration(5445.5), "5445.5");
        // 负数秒（宿主侧允许）格式化与解析都要接得住，否则这种格改不动
        assert_eq!(format_duration(-5400.0), "-1:30:00");
        assert_eq!(parse_duration("-1:30:00"), Some(-5400.0));
        assert_eq!(parse_duration("-45"), Some(-45.0));
        for bad in ["", "abc", "1:2:3:4", "1:", "1.5:30", "1:-30", "-"] {
            assert_eq!(parse_duration(bad), None, "应判为非法时长：{bad}");
        }
    }

    #[test]
    fn overflowing_durations_are_rejected_not_written_as_null() {
        // 每段都有限、合计却溢出：这种值一旦写出去就是 null，等于把原时长删掉
        assert_eq!(parse_duration("1e308:00"), None);
        assert_eq!(parse_duration("1e308"), Some(1e308), "单个有限段不构成溢出");
        assert_eq!(CellValue::Duration(Some(f64::INFINITY)).to_wire(), None);
        assert_eq!(CellValue::Number(Some(f64::INFINITY)).to_wire(), None);
    }

    #[test]
    fn huge_values_survive_text_roundtrip() {
        // 超出 i64 精确范围：格式化与编码都不能把它饱和成另一个数
        let huge = 1e17;
        assert_eq!(CellValue::Number(Some(huge)).to_wire(), Some(json!(1e17)));
        assert_eq!(parse_duration(&format_duration(huge)), Some(huge));
        assert_eq!(parse_duration(&format_duration(-huge)), Some(-huge));
        assert_eq!(format_duration(huge), "100000000000000000");
    }

    #[test]
    fn unknown_field_type_stays_readable_and_unwritable() {
        let kind = FieldType::from_wire("formula");
        assert_eq!(kind, FieldType::Other("formula".into()));
        let cell = CellValue::from_wire(&kind, Some(&json!(12)));
        assert_eq!(cell.as_edit_text(), "12");
        assert_eq!(cell.to_wire(), None);
    }

    #[test]
    fn display_name_strips_path_and_extension() {
        let snapshot = TableSnapshot { path: "任务/看板 v2.atb".into(), ..Default::default() };
        assert_eq!(snapshot.display_name(), "看板 v2");
    }
}
