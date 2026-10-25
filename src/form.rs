//! 界面树构建：把「快照 + 配置 + 当前行 + UI 模式 + 图片焦点」投影成一棵 ViewNode 树。
//!
//! 每个可交互节点都登记一个 [`Action`] 到返回的映射表里，入口按 `node_id` 查表分派，
//! 不靠解析 id 字符串（字段 id 可能含任意字符，字符串前缀解析会歧义）。
//! 节点 id 里带的是字段在本次投影里的**序号**，不是字段 id：宿主生成的 id 可以含 `-`，
//! 拼进节点 id 会和别的节点撞名，而重复 id 让宿主整树拒收。
//! 控件种类受宿主封闭枚举约束：颜色只用主题 token，图标名必须存在于宿主形状表 `NAMES`
//! （图标名写错宿主不报错，只把按钮画成空白，控件会隐形）。

use std::collections::HashMap;

use atelyx_plugin_sdk::protocol::view::{
    Align, Axis, BoxSize, ButtonVariant, CardTone, ColorSpec, ColorToken, ControlSize, EventKind,
    ImageFit,
    ImageSource, Justify, NodeBody, PaddingSpec, SizeSpec, TextWeight, ViewNode,
};
use atelyx_plugin_sdk::view_builders::NodeExt;

use crate::config::TableFormConfig;
use crate::model::{CellValue, FieldType, Row, TableSnapshot};

/// 一次用户操作对应的语义动作（与节点 id 一一对应，供入口分派）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    NavPrev,
    NavNext,
    AddRow,
    Refresh,
    ToggleSettings,
    /// 删除当前行（宿主有撤销栈兜底）。
    RemoveRow,
    /// 文本 / 时长 / 数字 / 下拉格写入：具体读法由字段的类型在入口决定。
    SetCell { field: String },
    /// 单选按钮样式里点中某个选项：`index` 是界面渲染出的选项下标。
    SelectOption { field: String, index: usize },
    /// 单选：清除。
    ClearSelect { field: String },
    /// 图片：移除这一张（其余引用与该格显示模式保留）。
    RemoveImage { field: String, path: String },
    /// 图片：选一张本机图片导入仓库并追加进这一格。
    AddImage { field: String },
    /// 图片：点缩略图，进灯箱看这一张。
    OpenImage { field: String, path: String },
    /// 预览：翻到相邻那张（`delta` = ±1，首尾相接）。
    ImageStep { delta: isize },
    /// 预览浮层聚焦后的按键：左右翻张、Esc 关闭，具体键由入口解码。
    ImageKey,
    /// 预览：退出，回到表单。
    CloseImage,
    /// 设置：字段显示 / 隐藏切换。
    ToggleField { field: String },
    /// 设置：字段上下移动。
    MoveField { field: String, delta: isize },
    /// 设置：字段提示文字。
    SetHelp { field: String },
    /// 设置：该单选字段是否用按钮样式渲染。
    ToggleRadio { field: String },
}

/// 图片格的界面焦点：是否在看大图预览。
///
/// 预览按「行 id + 字段 id + 图片引用」锚定而不是按序号：翻行、换表、那张图被删都要把它清掉，
/// 否则预览会停在失效的引用上；协作者在同一格里增删图片时，序号会漂，引用不会。
#[derive(Clone, Debug, Default)]
pub struct ImageFocus {
    /// 预览属于哪一行；与当前行不符就作废。
    pub row: Option<String>,
    /// 大图预览指向的那张；`None` = 表单态。
    pub preview: Option<Preview>,
}

/// 预览指向的图片：字段 id + 该格内的引用。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Preview {
    pub field: String,
    pub path: String,
}

impl ImageFocus {
    /// 打开大图预览；行换了先作废旧预览，免得旧引用落到新行上。
    pub fn open(&mut self, row_id: &str, field: &str, path: &str) {
        if self.row.as_deref() != Some(row_id) {
            self.row = Some(row_id.to_string());
            self.preview = None;
        }
        self.preview = Some(Preview { field: field.to_string(), path: path.to_string() });
    }
}

/// 建树入参。
pub struct FormView<'a> {
    pub snapshot: &'a TableSnapshot,
    pub config: &'a TableFormConfig,
    pub row_index: usize,
    pub show_settings: bool,
    pub status: Option<&'a str>,
    /// 图片格焦点（主图停在哪一张、是否在预览大图）；焦点状态由入口持有。
    pub image: &'a ImageFocus,
}

/// 缩略图边长与图条高度、方块间隙；预览大图定高（铺满可用宽，四周由浮层边距留白）。
const THUMB_IMAGE: f32 = 40.0;
const THUMB_STRIP: f32 = 64.0;
const IMAGE_GAP: f32 = 4.0;
const PREVIEW_HEIGHT: f32 = 480.0;
/// 构建整棵界面树，并返回「节点 id → 动作」映射。
/// 大图预览是根列上的一层面内浮层：树里出现即打开，遮罩拦下下层交互，表单本身保持原样。
pub fn build(view: &FormView) -> (ViewNode, HashMap<String, Action>) {
    let mut actions: HashMap<String, Action> = HashMap::new();
    let mut top: Vec<ViewNode> = vec![header(view, &mut actions)];
    if let Some(status) = view.status {
        top.push(muted_sized("status", status.to_string(), 0.9));
    }
    top.push(match view.show_settings {
        true => settings(view, &mut actions),
        false => form_body(view, &mut actions),
    });
    let mut root = column("root", 12.0, top);
    if let Some(target) = preview_target(view) {
        root.children.push(preview_overlay(&target, &mut actions));
    }
    (root, actions)
}

/// 预览目标：字段不是图片列、当前行没有那张图、这一格已经不含那条引用，都回 `None`
/// （界面回表单态，与入口 `prune_image` 的清理同一口径）。
fn preview_target(view: &FormView) -> Option<PreviewTarget> {
    let preview = view.image.preview.as_ref()?;
    let record = view.snapshot.rows.get(view.row_index)?;
    if view.image.row.as_deref() != Some(record.id.as_str()) {
        return None;
    }
    let field = view.snapshot.fields.iter().find(|field| field.id == preview.field && field.kind == FieldType::Image)?;
    let paths = record.cell(field).images().to_vec();
    let index = paths.iter().position(|path| path == &preview.path)?;
    Some(PreviewTarget { paths, index })
}

/// 预览态要画的那一张：该格全部路径（算翻张范围）与当前序号。
struct PreviewTarget {
    paths: Vec<String>,
    index: usize,
}

fn header(view: &FormView, actions: &mut HashMap<String, Action>) -> ViewNode {
    if view.show_settings {
        return settings_header(view, actions);
    }
    let snapshot = view.snapshot;
    let open = snapshot.is_open();
    let total = snapshot.rows.len();
    let has_row = total > 0;
    let title = if open { snapshot.display_name() } else { "表格表单".to_string() };
    let position = match (open, has_row) {
        (false, _) => "未打开表格".to_string(),
        (true, false) => "暂无数据".to_string(),
        (true, true) => format!("第 {} / {} 行", view.row_index + 1, total),
    };
    let info = column(
        "hdr-info",
        2.0,
        vec![
            text_sized("hdr-title", title, 1.15, Some(TextWeight::SemiBold), None),
            muted_sized("hdr-pos", position, 0.9),
        ],
    );
    let buttons = row(
        "hdr-actions",
        6.0,
        None,
        Some(Align::Center),
        None,
        vec![
            icon_sized(
                "hdr-prev",
                "chevron-left",
                "上一行",
                has_row && view.row_index > 0,
                ControlSize::Md,
                Action::NavPrev,
                actions,
            ),
            icon_sized(
                "hdr-next",
                "chevron-right",
                "下一行",
                has_row && view.row_index + 1 < total,
                ControlSize::Md,
                Action::NavNext,
                actions,
            ),
            label_button_sized("hdr-add", "新增行", ButtonVariant::Primary, open, ControlSize::Md, Action::AddRow, actions),
            icon_sized("hdr-remove", "trash-2", "删除当前行", has_row, ControlSize::Md, Action::RemoveRow, actions),
            icon_sized("hdr-refresh", "refresh-cw", "刷新", true, ControlSize::Md, Action::Refresh, actions),
            icon_sized("hdr-settings", "settings", "设置", open, ControlSize::Md, Action::ToggleSettings, actions),
        ],
    );
    toolbar_card(row("hdr", 12.0, Some(SizeSpec::Fill), Some(Align::Center), None, vec![info, buttons]))
}

/// 设置态只保留「字段设置 + 完成」，翻行与写值操作在这里没有意义。
fn settings_header(view: &FormView, actions: &mut HashMap<String, Action>) -> ViewNode {
    let subtitle = if view.snapshot.is_open() {
        format!("{} · 改动即时保存", view.snapshot.display_name())
    } else {
        "改动即时保存".to_string()
    };
    let info = column(
        "hdr-info",
        2.0,
        vec![
            text_sized("hdr-title", "字段设置", 1.15, Some(TextWeight::SemiBold), None),
            muted_sized("hdr-pos", subtitle, 0.9),
        ],
    );
    let done = label_button_sized("set-close", "完成", ButtonVariant::Primary, true, ControlSize::Md, Action::ToggleSettings, actions);
    let buttons = row("hdr-actions", 6.0, None, Some(Align::Center), None, vec![done]);
    toolbar_card(row("hdr", 12.0, Some(SizeSpec::Fill), Some(Align::Center), None, vec![info, buttons]))
}

fn toolbar_card(line: ViewNode) -> ViewNode {
    card_tone("hdr-card", CardTone::Muted, 10.0, 12.0, vec![line])
}

fn form_body(view: &FormView, actions: &mut HashMap<String, Action>) -> ViewNode {
    if !view.snapshot.is_open() {
        return empty_state("未打开表格", "打开一个 .atb 表格后即可按行填写。", false, actions);
    }
    let Some(record) = view.snapshot.rows.get(view.row_index) else {
        return empty_state("还没有数据行", "点「新增行」开始填写。", true, actions);
    };
    let fields = view.config.visible_fields(view.snapshot);
    if fields.is_empty() {
        let hint = column("form-fields", 8.0, vec![muted("empty", "没有可显示的字段，点右上角设置调出字段。")]);
        return scroll("form", vec![card("form-sheet", vec![hint])]);
    }
    let mut blocks: Vec<ViewNode> = Vec::with_capacity(fields.len() * 2);
    for (slot, field) in fields.iter().enumerate() {
        if slot > 0 {
            blocks.push(node(&format!("div-{slot}"), NodeBody::Divider {}));
        }
        blocks.push(field_block(view, record, slot, field, actions));
    }
    scroll("form", vec![card("form-sheet", vec![column("form-fields", 14.0, blocks)])])
}

fn empty_state(title: &str, hint: &str, can_add: bool, actions: &mut HashMap<String, Action>) -> ViewNode {
    let mut parts = vec![
        text_aligned("empty-title", title, 1.1, Some(TextWeight::SemiBold), None, Align::Center),
        text_aligned("empty", hint, 0.95, None, Some(ColorToken::TextMuted), Align::Center),
    ];
    if can_add {
        parts.push(row(
            "empty-actions",
            0.0,
            None,
            Some(Align::Center),
            Some(Justify::Center),
            vec![label_button_sized(
                "empty-add",
                "新增行",
                ButtonVariant::Primary,
                true,
                ControlSize::Md,
                Action::AddRow,
                actions,
            )],
        ));
    }
    let body = column("empty-col", 10.0, parts);
    scroll("form", vec![card_tone("empty-card", CardTone::Default, 10.0, 28.0, vec![body])])
}

fn field_block(
    view: &FormView,
    record: &Row,
    slot: usize,
    field: &crate::model::Field,
    actions: &mut HashMap<String, Action>,
) -> ViewNode {
    let key = field.id.as_str();
    let mut parts: Vec<ViewNode> = vec![
        text_sized(
            &format!("label-{slot}"),
            field.name.clone(),
            0.92,
            Some(TextWeight::SemiBold),
            Some(ColorToken::TextSecondary),
        ),
        control(view, record, slot, field, actions),
    ];
    if let Some(help) = view.config.help_text(key) {
        parts.push(muted_sized(&format!("help-{slot}"), help.to_string(), 0.85));
    }
    column(&format!("field-{slot}"), 6.0, parts)
}

fn control(
    view: &FormView,
    record: &Row,
    slot: usize,
    field: &crate::model::Field,
    actions: &mut HashMap<String, Action>,
) -> ViewNode {
    let key = field.id.as_str();
    let value = record.cell(field);
    // 存着的值与字段类型对不上（类型刚被改过、或文件被外部写过）：照原样只读显示。
    // 给个空输入框等于把用户的原值藏起来，一敲就覆盖掉。
    if matches!(value, CellValue::Foreign(_)) {
        return plain(&format!("ctl-{slot}"), value.as_edit_text());
    }
    match field.kind {
        FieldType::Text => editable(input(&format!("ctl-{slot}"), value.as_edit_text(), "请输入"), key, actions),
        // 时长按秒存、按 h:mm:ss 读写，走文本框才能收下 `1:30:00` 这种写法
        FieldType::Duration => editable(input(&format!("ctl-{slot}"), value.as_edit_text(), "如 1:30:00"), key, actions),
        FieldType::Number => editable(number_node(&format!("ctl-{slot}"), value.as_number(), "请输入"), key, actions),
        FieldType::SingleSelect => select_control(view, record, slot, field, actions),
        FieldType::Image => image_control(record, slot, field, actions),
        // 宿主未列举（或将来新增）的类型：只读显示原值，不登记写入口
        FieldType::Other(_) => plain(&format!("ctl-{slot}"), value.as_edit_text()),
    }
}

/// 可写控件统一挂 `change` 事件并登记写入动作。
fn editable(node: ViewNode, field: &str, actions: &mut HashMap<String, Action>) -> ViewNode {
    bind(node, Action::SetCell { field: field.to_string() }, actions)
}

/// 给节点挂 `change` 事件并登记对应动作：声明了事件宿主才上行，登记了动作入口才认得。
fn bind(mut view_node: ViewNode, action: Action, actions: &mut HashMap<String, Action>) -> ViewNode {
    view_node.events = vec![EventKind::Change];
    actions.insert(view_node.id.clone(), action);
    view_node
}

/// 给节点挂 `click` 事件并登记对应动作（图片、按钮类节点）。
fn bind_click(mut view_node: ViewNode, action: Action, actions: &mut HashMap<String, Action>) -> ViewNode {
    view_node.events = vec![EventKind::Click];
    actions.insert(view_node.id.clone(), action);
    view_node
}

/// 单选格：渲染出的选项列表要把当前值算进去——选项被改过时，宿主不动行值，
/// 只显示原选项列表会让已有内容看着像空值。
fn select_control(
    view: &FormView,
    record: &Row,
    slot: usize,
    field: &crate::model::Field,
    actions: &mut HashMap<String, Action>,
) -> ViewNode {
    let key = field.id.as_str();
    let current = match record.cell(field) {
        CellValue::Choice(option) => option,
        _ => None,
    };
    let mut rendered = field.options.clone();
    if let Some(option) = &current {
        if !rendered.iter().any(|o| o == option) {
            rendered.push(option.clone());
        }
    }
    let selected = current.as_deref().and_then(|option| rendered.iter().position(|o| o == option));
    if view.config.is_radio(key) {
        let mut buttons: Vec<ViewNode> = rendered
            .iter()
            .enumerate()
            .map(|(index, option)| {
                let variant = if selected == Some(index) { ButtonVariant::Primary } else { ButtonVariant::Ghost };
                label_button_sized(
                    &format!("ctl-{slot}-o{index}"),
                    option.clone(),
                    variant,
                    true,
                    ControlSize::Md,
                    Action::SelectOption { field: key.to_string(), index },
                    actions,
                )
            })
            .collect();
        buttons.push(label_button_sized(
            &format!("ctl-{slot}-clr"),
            "清除",
            ButtonVariant::Ghost,
            true,
            ControlSize::Md,
            Action::ClearSelect { field: key.to_string() },
            actions,
        ));
        row(&format!("ctl-{slot}"), 6.0, None, Some(Align::Center), None, buttons)
    } else {
        editable(
            dropdown_node(&format!("ctl-{slot}"), rendered, selected.map(|i| i as u32), "请选择"),
            key,
            actions,
        )
    }
}

/// 图片格：一条横向缩略图（末尾一枚「添加图片」钮，空格子只给添加钮）。点缩略图进灯箱看大图。
fn image_control(
    record: &Row,
    slot: usize,
    field: &crate::model::Field,
    actions: &mut HashMap<String, Action>,
) -> ViewNode {
    let key = field.id.as_str();
    let paths = record.cell(field).images().to_vec();
    if paths.is_empty() {
        // 空格不画图区，直接给一枚添加钮
        return row(
            &format!("ctl-{slot}"),
            8.0,
            None,
            Some(Align::Center),
            None,
            vec![label_button_sized(
                &format!("img-add-{slot}"),
                "添加图片",
                ButtonVariant::Ghost,
                true,
                ControlSize::Md,
                Action::AddImage { field: key.to_string() },
                actions,
            )],
        );
    }
    let mut tiles: Vec<ViewNode> = paths
        .iter()
        .enumerate()
        .map(|(index, path)| thumb_tile(slot, key, index, path, actions))
        .collect();
    tiles.push(label_button_sized(
        &format!("img-add-{slot}"),
        "添加",
        ButtonVariant::Ghost,
        true,
        ControlSize::Md,
        Action::AddImage { field: key.to_string() },
        actions,
    ));
    strip(&format!("img-strip-{slot}"), THUMB_STRIP, IMAGE_GAP, tiles)
}

/// 缩略图方块：点开灯箱看这张，悬停右上角浮现移除钮。
///
/// 叠层的尺寸由**第一个**子节点决定，其后的子节点被压进同一个方框里：底图是定方，
/// 角标列铺满同一方框后右对齐，移除钮正好贴右上角。
fn thumb_tile(slot: usize, field: &str, index: usize, path: &str, actions: &mut HashMap<String, Action>) -> ViewNode {
    let base = match image_source(path) {
        Some(source) => bind_click(
            image_node(&format!("img-th-{slot}-{index}"), source, ImageFit::Cover, SizeSpec::Fixed(THUMB_IMAGE), SizeSpec::Fixed(THUMB_IMAGE)),
            Action::OpenImage { field: field.to_string(), path: path.to_string() },
            actions,
        ),
        // 读不到内容的引用保留原文，至少能看出引用了什么
        None => plain(&format!("img-th-{slot}-{index}"), path.to_string()),
    };
    let remove = hover_icon_sized(
        &format!("img-rm-{slot}-{index}"),
        "x",
        "移除本张",
        ControlSize::Sm,
        Action::RemoveImage { field: field.to_string(), path: path.to_string() },
        actions,
    );
    let corner = column_aligned(&format!("img-rm-wrap-{slot}-{index}"), 0.0, Some(Align::End), vec![remove]);
    let tile = node(&format!("img-tile-{slot}-{index}"), NodeBody::Stack {}).with_children(vec![base, corner]);
    thumb_card(&format!("img-card-{slot}-{index}"), vec![tile])
}

/// 预览浮层：形式与宿主表格的图片灯箱一致——整面遮罩上一张 Contain 大图，点图片外任意处
/// 收起，没有关闭钮；多张时翻张钮贴左右缘居中、「N / M」贴底居中。图片节点声明 `key`：
/// 点图片取得键盘焦点，随后左右键翻张、Esc 关闭——落在图片身上的按下被图片节点截住
/// （取焦点的手势），不会落到底层收起热区，点图才不会误关。
fn preview_overlay(target: &PreviewTarget, actions: &mut HashMap<String, Action>) -> ViewNode {
    let count = target.paths.len();
    let path = &target.paths[target.index];
    let mut parts: Vec<ViewNode> = vec![{
        // 底层收起热区：铺满整面，图片外的按下都落到这里
        let mut catcher = fill_layer("pv-catcher");
        catcher.events = vec![EventKind::Click];
        actions.insert(catcher.id.clone(), Action::CloseImage);
        catcher
    }];
    parts.push(preview_center(match image_source(path) {
        Some(source) => {
            let mut image = image_node("pv-image", source, ImageFit::Contain, SizeSpec::Fill, SizeSpec::Fixed(PREVIEW_HEIGHT));
            image.events = vec![EventKind::Key];
            actions.insert(image.id.clone(), Action::ImageKey);
            image
        }
        None => muted("pv-image", format!("这张图片无法读取：{path}")),
    }));
    if count > 1 {
        parts.push(overlay_slot(
            "pv-prev-wrap",
            Some(Align::Center),
            None,
            icon_sized("pv-prev", "chevron-left", "上一张", true, ControlSize::Md, Action::ImageStep { delta: -1 }, actions),
        ));
        parts.push(overlay_slot(
            "pv-next-wrap",
            Some(Align::Center),
            Some(Justify::End),
            icon_sized("pv-next", "chevron-right", "下一张", true, ControlSize::Md, Action::ImageStep { delta: 1 }, actions),
        ));
        parts.push(overlay_slot(
            "pv-count-wrap",
            Some(Align::End),
            Some(Justify::Center),
            text_sized("pv-count", format!("{}/{}", target.index + 1, count), 0.9, None, Some(ColorToken::Text)),
        ));
    }
    node("pv-overlay", NodeBody::Overlay { scrim: None })
        .with_children(vec![node("pv-view", NodeBody::Stack {}).with_children(parts)])
}

/// 灯箱贴边槽：高度铺满浮层、宽度随内容的行，把单个子项摆到指定角落。
/// 宽度不能声明 Fill：撑满的行自己就是整行，外层容器的 justify 对齐会失效（宿主 Row 同款规则）。
fn overlay_slot(id: &str, cross_align: Option<Align>, justify: Option<Justify>, child: ViewNode) -> ViewNode {
    node(
        id,
        NodeBody::Row {
            gap: None,
            padding: Some(PaddingSpec::All(8.0)),
            justify,
            cross_align,
            size: Some(BoxSize { height: Some(SizeSpec::Fill), ..BoxSize::default() }),
        },
    )
    .with_children(vec![child])
}

/// 灯箱中图槽：铺满整面的行，图片带四周遮罩边距、垂直水平居中。
fn preview_center(child: ViewNode) -> ViewNode {
    node(
        "pv-center",
        NodeBody::Row {
            gap: None,
            padding: Some(PaddingSpec::All(24.0)),
            justify: Some(Justify::Center),
            cross_align: Some(Align::Center),
            size: Some(BoxSize { width: Some(SizeSpec::Fill), height: Some(SizeSpec::Fill), ..BoxSize::default() }),
        },
    )
    .with_children(vec![child])
}

/// 铺满父容器的空白行：浮层里当收起热区（整面底层）。
fn fill_layer(id: &str) -> ViewNode {
    node(
        id,
        NodeBody::Row {
            gap: None,
            padding: None,
            justify: None,
            cross_align: None,
            size: Some(BoxSize { width: Some(SizeSpec::Fill), height: Some(SizeSpec::Fill), ..BoxSize::default() }),
        },
    )
}

/// 图片引用 → 图源：http(s) 外链走远程图，本机绝对路径（盘符 / UNC / 根相对）走文件源，
/// 其余按仓库相对路径读仓库；空串、含 `..` 的越界段与其他协议（`data:` 等）回 `None`，
/// 界面保留纯文本，至少看得出引用了什么。
fn image_source(path: &str) -> Option<ImageSource> {
    let path = path.trim();
    if path.is_empty() || path.split(['/', '\\']).any(|segment| segment == "..") {
        return None;
    }
    let lower = path.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return Some(ImageSource::Url { url: path.to_string(), local: None });
    }
    let bytes = path.as_bytes();
    let absolute = lower.starts_with('/')
        || bytes.first() == Some(&b'\\')
        || (bytes.get(1) == Some(&b':') && matches!(bytes.get(2), Some(b'\\') | Some(b'/')));
    if absolute {
        return Some(ImageSource::File { path: path.to_string() });
    }
    // 剩余的带冒号片段是别的协议、不是仓库路径；不猜它的读法
    (bytes.first() != Some(&b':') && !path.contains(':')).then(|| ImageSource::Vault { path: path.to_string() })
}

fn image_node(id: &str, source: ImageSource, fit: ImageFit, width: SizeSpec, height: SizeSpec) -> ViewNode {
    node(id, NodeBody::Image { source, fit: Some(fit), width: Some(width), height: Some(height) })
}

fn settings(view: &FormView, actions: &mut HashMap<String, Action>) -> ViewNode {
    let ordered = view.config.ordered_fields(view.snapshot);
    let last = ordered.len().saturating_sub(1);
    let mut cards: Vec<ViewNode> = Vec::with_capacity(ordered.len());
    for (slot, field) in ordered.iter().enumerate() {
        let key = field.id.as_str();
        let toggle = checkbox(
            &format!("set-en-{slot}"),
            !view.config.is_hidden(key),
            field.name.clone(),
            Action::ToggleField { field: key.to_string() },
            actions,
        );
        let moves = row(
            &format!("set-move-{slot}"),
            2.0,
            None,
            Some(Align::Center),
            None,
            vec![
                icon_sized(
                    &format!("set-up-{slot}"),
                    "arrow-up",
                    "上移",
                    slot > 0,
                    ControlSize::Sm,
                    Action::MoveField { field: key.to_string(), delta: -1 },
                    actions,
                ),
                icon_sized(
                    &format!("set-dn-{slot}"),
                    "arrow-down",
                    "下移",
                    slot < last,
                    ControlSize::Sm,
                    Action::MoveField { field: key.to_string(), delta: 1 },
                    actions,
                ),
            ],
        );
        let head = row(
            &format!("set-row-{slot}"),
            8.0,
            Some(SizeSpec::Fill),
            Some(Align::Center),
            Some(Justify::SpaceBetween),
            vec![toggle, moves],
        );
        // 提示文字经 Change 逐键写入配置（Blur 不带值，无法读回）
        let mut body = vec![
            head,
            bind(
                input(&format!("set-help-{slot}"), view.config.help_text(key).unwrap_or(""), "填写提示（可选）"),
                Action::SetHelp { field: key.to_string() },
                actions,
            ),
        ];
        if field.kind == FieldType::SingleSelect {
            body.push(checkbox(
                &format!("set-radio-{slot}"),
                view.config.is_radio(key),
                "单选按钮样式".to_string(),
                Action::ToggleRadio { field: key.to_string() },
                actions,
            ));
        }
        let card = card_tone(
            &format!("set-card-{slot}"),
            CardTone::Default,
            10.0,
            12.0,
            vec![column(&format!("set-col-{slot}"), 8.0, body)],
        );
        cards.push(card);
    }
    let columns = if ordered.len() >= 2 { 2 } else { 1 };
    let grid = node("set-grid", NodeBody::Grid { columns: Some(columns), min_column_width: None, gap: Some(10.0), row_height: None }).with_children(cards);
    let list = column(
        "settings-col",
        10.0,
        vec![
            muted_sized("set-hint", "勾选控制是否出现在表单中，箭头调整顺序，提示显示在字段下方。", 0.85),
            grid,
        ],
    );
    scroll("settings", vec![list])
}

// ---------- 节点构造辅助（直接构 NodeBody 以带 weight/color/enabled/placeholder 等字段） ----------

fn node(id: &str, body: NodeBody) -> ViewNode {
    ViewNode {
        id: id.to_string(),
        body,
        interaction: None,
        grow: None,
        children: Vec::new(),
        events: Vec::new(),
    }
}

fn column(id: &str, gap: f32, children: Vec<ViewNode>) -> ViewNode {
    column_aligned(id, gap, None, children)
}

/// 交叉轴取向可调的列：`cross_align` 决定子项在列内的水平摆位（居中主图、右对齐角标）。
fn column_aligned(id: &str, gap: f32, cross_align: Option<Align>, children: Vec<ViewNode>) -> ViewNode {
    node(
        id,
        NodeBody::Column {
            gap: Some(gap),
            padding: None,
            cross_align,
            align: None,
            size: Some(BoxSize { width: Some(SizeSpec::Fill), ..BoxSize::default() }),
        },
    )
    .with_children(children)
}

fn row(
    id: &str,
    gap: f32,
    width: Option<SizeSpec>,
    cross_align: Option<Align>,
    justify: Option<Justify>,
    children: Vec<ViewNode>,
) -> ViewNode {
    let size = width.map(|width| BoxSize { width: Some(width), ..BoxSize::default() });
    node(id, NodeBody::Row { gap: Some(gap), padding: None, justify, cross_align, size })
        .with_children(children)
}

fn scroll(id: &str, children: Vec<ViewNode>) -> ViewNode {
    node(id, NodeBody::Scroll { direction: None, gap: None, height: None, follow_end: None }).with_children(children)
}

/// 横向图条：固定高度、内容按自身宽度溢出（宿主对横向滚动按 `height` 做图条）。
fn strip(id: &str, height: f32, gap: f32, children: Vec<ViewNode>) -> ViewNode {
    node(id, NodeBody::Scroll { direction: Some(Axis::Horizontal), gap: Some(gap), height: Some(height), follow_end: None }).with_children(children)
}

fn card(id: &str, children: Vec<ViewNode>) -> ViewNode {
    card_tone(id, CardTone::Default, 10.0, 18.0, children)
}

fn card_tone(id: &str, tone: CardTone, radius: f32, padding: f32, children: Vec<ViewNode>) -> ViewNode {
    node(
        id,
        NodeBody::Card {
            gap: None,
            padding: Some(PaddingSpec::All(padding)),
            accent: None,
            tone: Some(tone),
            radius: Some(radius),
            bordered: Some(true),
            align: None,
            size: None,
        },
    )
    .with_children(children)
}

/// 缩略图方卡：按内容收缩（宿主卡片自带主题色，插件不写死色值）。
fn thumb_card(id: &str, children: Vec<ViewNode>) -> ViewNode {
    node(
        id,
        NodeBody::Card {
            gap: None,
            padding: Some(PaddingSpec::All(2.0)),
            accent: None,
            tone: Some(CardTone::Muted),
            radius: Some(6.0),
            bordered: Some(true),
            align: None,
            size: Some(BoxSize { width: Some(SizeSpec::Shrink), ..BoxSize::default() }),
        },
    )
    .with_children(children)
}

fn plain(id: &str, content: impl Into<String>) -> ViewNode {
    text_sized(id, content, 1.0, None, None)
}

fn text_sized(
    id: &str,
    content: impl Into<String>,
    size: f32,
    weight: Option<TextWeight>,
    color: Option<ColorToken>,
) -> ViewNode {
    node(
        id,
        NodeBody::Text {
            content: content.into(),
            size: Some(size),
            weight,
            color: color.map(ColorSpec::Token),
            align: None,
            family: None,
            wrap: None,
            max_lines: None,
        },
    )
}

fn text_aligned(
    id: &str,
    content: impl Into<String>,
    size: f32,
    weight: Option<TextWeight>,
    color: Option<ColorToken>,
    align: Align,
) -> ViewNode {
    node(
        id,
        NodeBody::Text {
            content: content.into(),
            size: Some(size),
            weight,
            color: color.map(ColorSpec::Token),
            align: Some(align),
            family: None,
            wrap: None,
            max_lines: None,
        },
    )
}

fn muted(id: &str, content: impl Into<String>) -> ViewNode {
    muted_sized(id, content, 1.0)
}

fn muted_sized(id: &str, content: impl Into<String>, size: f32) -> ViewNode {
    text_sized(id, content, size, None, Some(ColorToken::TextMuted))
}

fn input(id: &str, value: impl Into<String>, placeholder: &str) -> ViewNode {
    node(
        id,
        NodeBody::TextInput {
            value: value.into(),
            placeholder: Some(placeholder.to_string()),
            multiline: None,
            password: None,
            read_only: None,
            report: None,
        },
    )
}

fn number_node(id: &str, value: Option<f64>, placeholder: &str) -> ViewNode {
    node(
        id,
        // 步进 1：宿主在输入框右侧渲染上下步进按钮，微调比敲键盘顺手
        NodeBody::NumberInput { value, min: None, max: None, step: Some(1.0), placeholder: Some(placeholder.to_string()), read_only: None, report: None },
    )
}

fn dropdown_node(id: &str, options: Vec<String>, selected: Option<u32>, placeholder: &str) -> ViewNode {
    node(id, NodeBody::Dropdown { options, selected, placeholder: Some(placeholder.to_string()), read_only: None })
}

fn icon_node(id: &str, glyph: &str, tooltip: &str, enabled: bool, size: ControlSize, hover_only: bool) -> ViewNode {
    node(
        id,
        NodeBody::IconButton {
            icon: glyph.to_string(),
            tooltip: Some(tooltip.to_string()),
            enabled: Some(enabled),
            size: Some(size),
            variant: None,
            hover_only: hover_only.then_some(true),
        },
    )
}

fn icon_sized(
    id: &str,
    glyph: &str,
    tooltip: &str,
    enabled: bool,
    size: ControlSize,
    action: Action,
    actions: &mut HashMap<String, Action>,
) -> ViewNode {
    bind_click(icon_node(id, glyph, tooltip, enabled, size, false), action, actions)
}

/// 悬停才浮现的角标钮：常驻会把图片本身盖掉（图片上的移除钮）。
fn hover_icon_sized(
    id: &str,
    glyph: &str,
    tooltip: &str,
    size: ControlSize,
    action: Action,
    actions: &mut HashMap<String, Action>,
) -> ViewNode {
    bind_click(icon_node(id, glyph, tooltip, true, size, true), action, actions)
}

fn label_button_sized(
    id: &str,
    label: impl Into<String>,
    variant: ButtonVariant,
    enabled: bool,
    size: ControlSize,
    action: Action,
    actions: &mut HashMap<String, Action>,
) -> ViewNode {
    let mut n = node(
        id,
        NodeBody::Button {
            label: label.into(),
            icon: None,
            variant: Some(variant),
            enabled: Some(enabled),
            size: Some(size),
        },
    );
    n.events = vec![EventKind::Click];
    actions.insert(n.id.clone(), action);
    n
}

fn checkbox(id: &str, checked: bool, label: impl Into<String>, action: Action, actions: &mut HashMap<String, Action>) -> ViewNode {
    let mut n = node(id, NodeBody::Checkbox { checked, label: Some(label.into()), read_only: None });
    n.events = vec![EventKind::Change];
    actions.insert(n.id.clone(), action);
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TableFormConfig;
    use crate::model::{Field, ImageCell};
    use serde_json::json;
    use std::collections::BTreeMap;

    fn snapshot_with(cover: &[&str]) -> TableSnapshot {
        let fields = vec![
            Field { id: "t".into(), name: "标题".into(), kind: FieldType::Text, options: vec![] },
            Field { id: "d".into(), name: "时长".into(), kind: FieldType::Duration, options: vec![] },
            Field { id: "n".into(), name: "工时".into(), kind: FieldType::Number, options: vec![] },
            Field { id: "s".into(), name: "状态".into(), kind: FieldType::SingleSelect, options: vec!["进行中".into()] },
            Field { id: "i".into(), name: "封面".into(), kind: FieldType::Image, options: vec![] },
            Field { id: "u".into(), name: "公式".into(), kind: FieldType::Other("formula".into()), options: vec![] },
        ];
        let mut cells: BTreeMap<String, CellValue> = BTreeMap::new();
        cells.insert("t".to_string(), CellValue::Text("登录页".to_string()));
        cells.insert("d".to_string(), CellValue::Duration(Some(5400.0)));
        cells.insert("n".to_string(), CellValue::Number(Some(8.0)));
        cells.insert("s".to_string(), CellValue::Choice(Some("进行中".to_string())));
        let paths = cover.iter().map(|p| p.to_string()).collect();
        cells.insert("i".to_string(), CellValue::Images(ImageCell { paths, ..ImageCell::default() }));
        cells.insert("u".to_string(), CellValue::Foreign(json!(12)));
        let row = Row { id: "r1".to_string(), cells };
        TableSnapshot { path: "任务/看板.atb".to_string(), fields, rows: vec![row], ..Default::default() }
    }

    fn build_with(snapshot: &TableSnapshot, settings: bool) -> (ViewNode, HashMap<String, Action>) {
        build_view(snapshot, settings, &ImageFocus::default())
    }

    /// 带图片焦点建树：主图停在哪一张、是不是在看大图，都由 `ImageFocus` 说了算。
    fn build_view(snapshot: &TableSnapshot, settings: bool, image: &ImageFocus) -> (ViewNode, HashMap<String, Action>) {
        build(&FormView { snapshot, config: &TableFormConfig::default(), row_index: 0, show_settings: settings, status: None, image })
    }

    /// 正在预览 `path` 那一张的界面（图片字段固定为 `i`）。
    fn previewing(snapshot: &TableSnapshot, path: &str) -> ImageFocus {
        let mut focus = ImageFocus::default();
        focus.open(&snapshot.rows[0].id, "i", path);
        focus
    }

    /// 界面里的节点 id 带的是字段在本次投影里的序号；测试按同一算法取名，不写死序号。
    fn slot_of(snapshot: &TableSnapshot, config: &TableFormConfig, field: &str) -> usize {
        config.visible_fields(snapshot).iter().position(|f| f.id == field).expect("字段应在表单里")
    }

    fn ctl_id(snapshot: &TableSnapshot, config: &TableFormConfig, field: &str) -> String {
        format!("ctl-{}", slot_of(snapshot, config, field))
    }

    fn text_of(tree: &ViewNode, id: &str) -> Option<String> {
        tree.find(id).and_then(|node| match &node.body {
            NodeBody::Text { content, .. } => Some(content.clone()),
            _ => None,
        })
    }

    fn enabled_of(tree: &ViewNode, id: &str) -> Option<bool> {
        tree.find(id).and_then(|node| match &node.body {
            NodeBody::Button { enabled, .. } | NodeBody::IconButton { enabled, .. } => *enabled,
            _ => None,
        })
    }

    /// 收集「声明了事件的节点 id」：声明了事件宿主才上行，上行就必须有得查的动作。
    fn eventful(node: &ViewNode, out: &mut Vec<String>) {
        if !node.events.is_empty() {
            out.push(node.id.clone());
        }
        for child in &node.children {
            eventful(child, out);
        }
    }

    #[test]
    fn form_and_settings_trees_validate_and_actions_resolve() {
        let snapshot = snapshot_with(&["assets/a.png"]);
        let config = TableFormConfig::default();
        for settings in [false, true] {
            let (tree, actions) = build(&FormView {
                snapshot: &snapshot,
                config: &config,
                row_index: 0,
                show_settings: settings,
                status: None,
                image: &ImageFocus::default(),
            });
            tree.validate().expect("界面树应通过结构校验");
            for id in actions.keys() {
                assert!(tree.find(id).is_some(), "动作登记了不存在的节点：{id}");
            }
            let mut declared = Vec::new();
            eventful(&tree, &mut declared);
            for id in declared {
                assert!(actions.contains_key(&id), "节点声明了事件却没有动作：{id}");
            }
        }
    }

    #[test]
    fn field_ids_never_leak_into_node_ids() {
        // 字段 id 可以含 `-`：按 id 拼接节点名会让两个字段撞出同一个 id，整树被宿主拒收
        let fields = vec![
            Field { id: "a".into(), name: "A".into(), kind: FieldType::SingleSelect, options: vec!["x".into(), "y".into()] },
            Field { id: "a-o1".into(), name: "B".into(), kind: FieldType::Text, options: vec![] },
        ];
        let snapshot = TableSnapshot {
            path: "撞名.atb".into(),
            fields,
            rows: vec![Row { id: "r".into(), cells: BTreeMap::new() }],
            ..Default::default()
        };
        let mut config = TableFormConfig::default();
        config.set_radio("a", true);
        let (tree, actions) = build(&FormView { snapshot: &snapshot, config: &config, row_index: 0, show_settings: false, status: None, image: &ImageFocus::default() });
        tree.validate().expect("节点 id 不该撞名");
        assert!(actions.contains_key(&ctl_id(&snapshot, &config, "a-o1")));
    }

    #[test]
    fn thumbs_open_their_own_image_and_each_can_be_removed() {
        let snapshot = snapshot_with(&["assets/a.png", "assets/b.png"]);
        let config = TableFormConfig::default();
        let (tree, actions) = build_with(&snapshot, false);
        let slot = slot_of(&snapshot, &config, "i");
        for (index, path) in ["assets/a.png", "assets/b.png"].iter().enumerate() {
            let id = format!("img-th-{slot}-{index}");
            assert_eq!(
                actions.get(&id),
                Some(&Action::OpenImage { field: "i".to_string(), path: path.to_string() }),
                "每张缩略图应打开它自己那一张的灯箱"
            );
            assert!(tree.find(&id).is_some());
            assert_eq!(
                actions.get(&format!("img-rm-{slot}-{index}")),
                Some(&Action::RemoveImage { field: "i".to_string(), path: path.to_string() }),
                "每张缩略图都带移除钮"
            );
        }
    }

    #[test]
    fn remove_button_only_shows_on_hover() {
        let snapshot = snapshot_with(&["assets/a.png"]);
        let config = TableFormConfig::default();
        let (tree, _) = build_with(&snapshot, false);
        let slot = slot_of(&snapshot, &config, "i");
        let NodeBody::IconButton { hover_only, .. } = &tree.find(&format!("img-rm-{slot}-0")).expect("移除钮应在树里").body else {
            panic!("移除钮应是图标钮");
        };
        assert_eq!(*hover_only, Some(true), "常驻角标会把缩略图本身盖掉");
    }

    #[test]
    fn thumb_badge_sits_on_its_own_image() {
        let snapshot = snapshot_with(&["assets/a.png"]);
        let config = TableFormConfig::default();
        let (tree, actions) = build_with(&snapshot, false);
        let slot = slot_of(&snapshot, &config, "i");
        let stack = tree.find(&format!("img-tile-{slot}-0")).expect("缩略图叠层应在树里");
        // 叠层尺寸由第一个子节点决定：底图在前、角标列在后
        assert_eq!(stack.children.len(), 2);
        assert!(matches!(stack.children[0].body, NodeBody::Image { .. }));
        assert!(matches!(stack.children[1].body, NodeBody::Column { .. }));
        assert_eq!(actions.get(&format!("img-th-{slot}-0")), Some(&Action::OpenImage { field: "i".to_string(), path: "assets/a.png".to_string() }));
        assert_eq!(actions.get(&format!("img-rm-{slot}-0")), Some(&Action::RemoveImage { field: "i".to_string(), path: "assets/a.png".to_string() }));
    }

    #[test]
    fn strip_is_a_horizontal_image_rail() {
        let snapshot = snapshot_with(&["assets/a.png", "assets/b.png"]);
        let config = TableFormConfig::default();
        let (tree, _) = build_with(&snapshot, false);
        let slot = slot_of(&snapshot, &config, "i");
        let rail = tree.find(&format!("img-strip-{slot}")).expect("缩略图条应是滚动容器");
        let NodeBody::Scroll { direction, height, .. } = &rail.body else {
            panic!("缩略图条应是滚动容器");
        };
        assert_eq!(*direction, Some(Axis::Horizontal));
        assert_eq!(*height, Some(THUMB_STRIP));
        // 条里是缩略图方块加末尾一枚添加钮
        assert_eq!(rail.children.len(), 3);
    }

    #[test]
    fn preview_enlarges_the_current_image_and_walks_the_list() {
        let snapshot = snapshot_with(&["assets/a.png", "assets/b.png", "assets/c.png"]);
        let (tree, actions) = build_view(&snapshot, false, &previewing(&snapshot, "assets/b.png"));
        tree.validate().expect("预览态应成树");
        let NodeBody::Image { source, fit, width, height } = &tree.find("pv-image").expect("预览大图应在树里").body else {
            panic!("预览应是图片节点");
        };
        assert_eq!(source, &ImageSource::Vault { path: "assets/b.png".to_string() });
        assert_eq!(fit, &Some(ImageFit::Contain));
        assert_eq!((*width, *height), (Some(SizeSpec::Fill), Some(SizeSpec::Fixed(PREVIEW_HEIGHT))));
        assert_eq!(text_of(&tree, "pv-count").as_deref(), Some("2/3"));
        assert_eq!(actions.get("pv-prev"), Some(&Action::ImageStep { delta: -1 }));
        assert_eq!(actions.get("pv-next"), Some(&Action::ImageStep { delta: 1 }));
        // 形式同宿主灯箱：无关闭钮与按钮条，收起靠图外热区；键盘翻张声明在图片节点上（点图取焦点）
        assert!(tree.find("pv-bar").is_none() && tree.find("pv-close").is_none());
        assert_eq!(actions.get("pv-catcher"), Some(&Action::CloseImage));
        assert_eq!(actions.get("pv-image"), Some(&Action::ImageKey));
        assert_eq!(tree.find("pv-image").expect("大图应在树里").events, vec![EventKind::Key]);
        // 浮层叠在表单之上，表单本身原样保留
        assert!(matches!(&tree.find("pv-overlay").expect("预览浮层应在树里").body, NodeBody::Overlay { .. }));
        assert!(tree.find(&ctl_id(&snapshot, &TableFormConfig::default(), "t")).is_some(), "浮层之下表单应还在");
        // 只有一张时不画翻张钮与计数，收起热区常在
        let single = snapshot_with(&["assets/a.png"]);
        let (tree, actions) = build_view(&single, false, &previewing(&single, "assets/a.png"));
        assert!(tree.find("pv-prev").is_none() && tree.find("pv-count").is_none());
        assert_eq!(actions.get("pv-catcher"), Some(&Action::CloseImage));
    }

    #[test]
    fn preview_needs_a_live_target() {
        let snapshot = snapshot_with(&["assets/a.png"]);
        let config = TableFormConfig::default();
        // 预览指向的那张已经没了：不画预览，表单回到眼前
        let (tree, _) = build_view(&snapshot, false, &previewing(&snapshot, "assets/gone.png"));
        assert!(tree.find("pv-image").is_none());
        assert!(tree.find(&ctl_id(&snapshot, &config, "t")).is_some(), "应退回表单态");
        // 焦点属于别的行：同样不预览
        let elsewhere = ImageFocus {
            row: Some("r9".to_string()),
            preview: Some(Preview { field: "i".into(), path: "assets/a.png".into() }),
        };
        let (tree, _) = build_view(&snapshot, false, &elsewhere);
        assert!(tree.find("pv-image").is_none(), "焦点不属当前行时不该预览");
    }

    #[test]
    fn previewing_an_unreadable_reference_says_so() {
        let snapshot = snapshot_with(&["../走出仓库.png"]);
        let (tree, _) = build_view(&snapshot, false, &previewing(&snapshot, "../走出仓库.png"));
        assert_eq!(text_of(&tree, "pv-image").as_deref(), Some("这张图片无法读取：../走出仓库.png"));
    }

    #[test]
    fn external_references_render_as_real_images() {
        let snapshot = snapshot_with(&["https://example.com/a.png", "C:\\相册\\b.png", "assets/c.png", "../out.png", "data:image/png;base64,x"]);
        let (tree, _) = build_with(&snapshot, false);
        let slot = slot_of(&snapshot, &TableFormConfig::default(), "i");
        for (index, source) in [
            ImageSource::Url { url: "https://example.com/a.png".into(), local: None },
            ImageSource::File { path: "C:\\相册\\b.png".into() },
            ImageSource::Vault { path: "assets/c.png".into() },
        ]
        .into_iter()
        .enumerate()
        {
            let NodeBody::Image { source: got, .. } = &tree.find(&format!("img-th-{slot}-{index}")).expect("可读引用应出真图").body else {
                panic!("第 {index} 张应是图片节点");
            };
            assert_eq!(got, &source);
        }
        // 越界段与其他协议：保留原文，不猜读法
        for index in [3, 4] {
            assert!(matches!(&tree.find(&format!("img-th-{slot}-{index}")).expect("不可读引用应保留原文").body, NodeBody::Text { .. }));
        }
    }

    #[test]
    fn controls_follow_field_kinds() {
        let snapshot = snapshot_with(&["assets/a.png", "备注 (v2).png"]);
        let config = TableFormConfig::default();
        let (tree, actions) = build_with(&snapshot, false);
        let NodeBody::TextInput { value, placeholder, .. } = &tree.find(&ctl_id(&snapshot, &config, "d")).expect("时长框应在树里").body else {
            panic!("时长格应是文本框");
        };
        assert_eq!((value.as_str(), placeholder.as_deref()), ("1:30:00", Some("如 1:30:00")));
        assert!(matches!(&tree.find(&ctl_id(&snapshot, &config, "n")).expect("数字框应在树里").body, NodeBody::NumberInput { value: Some(8.0), .. }));
        // 缩略图是定方、铺满裁切的小方块
        let slot = slot_of(&snapshot, &config, "i");
        for (index, path) in ["assets/a.png", "备注 (v2).png"].iter().enumerate() {
            let NodeBody::Image { source, fit, width, height } =
                &tree.find(&format!("img-th-{slot}-{index}")).expect("缩略图应在树里").body
            else {
                panic!("仓库内路径应走图片节点");
            };
            assert_eq!(source, &ImageSource::Vault { path: path.to_string() });
            assert_eq!(fit, &Some(ImageFit::Cover));
            assert_eq!((*width, *height), (Some(SizeSpec::Fixed(THUMB_IMAGE)), Some(SizeSpec::Fixed(THUMB_IMAGE))));
        }
        // 未列举类型只读：既不登记写入口，也不是输入控件
        assert!(matches!(&tree.find(&ctl_id(&snapshot, &config, "u")).expect("未知类型格应在树里").body, NodeBody::Text { .. }));
        assert!(!actions.contains_key(&ctl_id(&snapshot, &config, "u")), "只读格不得有写动作");
    }

    #[test]
    fn single_image_still_gets_tile_and_add() {
        let snapshot = snapshot_with(&["assets/a.png"]);
        let config = TableFormConfig::default();
        let (tree, actions) = build_with(&snapshot, false);
        let slot = slot_of(&snapshot, &config, "i");
        assert!(matches!(&tree.find(&format!("img-th-{slot}-0")).expect("唯一那张缩略图应在树里").body, NodeBody::Image { .. }));
        assert_eq!(actions.get(&format!("img-add-{slot}")), Some(&Action::AddImage { field: "i".to_string() }), "添加钮常在条尾");
    }

    #[test]
    fn unreadable_references_stay_visible_and_removable() {
        let snapshot = snapshot_with(&["assets/a.png", "../走出仓库.png"]);
        let config = TableFormConfig::default();
        let (tree, actions) = build_with(&snapshot, false);
        let slot = slot_of(&snapshot, &config, "i");
        assert!(matches!(&tree.find(&format!("img-th-{slot}-0")).expect("首张图应在树里").body, NodeBody::Image { .. }));
        // 读不到内容的引用：缩略图保留原文，也仍然能移除
        assert_eq!(text_of(&tree, &format!("img-th-{slot}-1")).as_deref(), Some("../走出仓库.png"));
        assert_eq!(actions.get(&format!("img-rm-{slot}-1")), Some(&Action::RemoveImage { field: "i".to_string(), path: "../走出仓库.png".to_string() }));
        // 读不到的引用不登记看大图：点开只会是一块占位
        assert!(!actions.contains_key(&format!("img-th-{slot}-1")), "不可读引用不该有灯箱动作");
    }

    /// 单字段的数字格：界面里它占第 0 号节点。
    fn number_snapshot(value: CellValue) -> TableSnapshot {
        let fields = vec![Field { id: "n".into(), name: "工时".into(), kind: FieldType::Number, options: vec![] }];
        let mut cells = BTreeMap::new();
        cells.insert("n".to_string(), value);
        TableSnapshot { path: "任务.atb".into(), fields, rows: vec![Row { id: "r".into(), cells }], ..Default::default() }
    }

    #[test]
    fn value_unkind_to_its_field_stays_visible_and_readonly() {
        // 存的值与字段类型对不上：既要把原值显示出来，也不能给一个能覆盖它的输入框
        let snapshot = number_snapshot(CellValue::Foreign(json!({"note": "外部写过的形态"})));
        let (tree, actions) = build_with(&snapshot, false);
        let id = ctl_id(&snapshot, &TableFormConfig::default(), "n");
        assert!(matches!(&tree.find(&id).expect("只读格应在树里").body, NodeBody::Text { content, .. } if content.contains("外部写过的形态")));
        assert!(!actions.contains_key(&id), "值形态不符的格不得有写动作");
    }

    #[test]
    fn current_choice_shows_even_when_options_changed() {
        // 选项被改过而行值没动：当前值仍要显示出来，不能看着像空单元格
        let fields = vec![Field { id: "s".into(), name: "状态".into(), kind: FieldType::SingleSelect, options: vec!["进行中".into()] }];
        let mut cells = BTreeMap::new();
        cells.insert("s".to_string(), CellValue::Choice(Some("已暂停".into())));
        let snapshot = TableSnapshot { path: "任务.atb".into(), fields, rows: vec![Row { id: "r".into(), cells }], ..Default::default() };

        let config = TableFormConfig::default();
        let id = ctl_id(&snapshot, &config, "s");
        let (tree, _) = build(&FormView { snapshot: &snapshot, config: &config, row_index: 0, show_settings: false, status: None, image: &ImageFocus::default() });
        let NodeBody::Dropdown { options, selected, .. } = &tree.find(&id).expect("单选格应是下拉").body else {
            panic!("单选格应是下拉");
        };
        assert_eq!(options, &["进行中".to_string(), "已暂停".to_string()], "当前值应补进选项末尾");
        assert_eq!(*selected, Some(1));

        let mut config = TableFormConfig::default();
        config.set_radio("s", true);
        let (tree, _) = build(&FormView { snapshot: &snapshot, config: &config, row_index: 0, show_settings: false, status: None, image: &ImageFocus::default() });
        let id = ctl_id(&snapshot, &config, "s");
        assert!(matches!(&tree.find(&format!("{id}-o1")).expect("当前值应有对应按钮").body, NodeBody::Button { variant: Some(ButtonVariant::Primary), .. }));
    }

    #[test]
    fn closed_table_shows_empty_state_and_disables_writes() {
        let closed = TableSnapshot::default();
        let (tree, actions) = build_with(&closed, false);
        assert_eq!(text_of(&tree, "hdr-pos").as_deref(), Some("未打开表格"));
        assert_eq!(text_of(&tree, "hdr-title").as_deref(), Some("表格表单"));
        assert_eq!(enabled_of(&tree, "hdr-add"), Some(false));
        assert_eq!(enabled_of(&tree, "hdr-settings"), Some(false));
        // 刷新保持可用：表格刚被打开时靠它读到第一份快照
        assert_eq!(enabled_of(&tree, "hdr-refresh"), Some(true));
        assert!(text_of(&tree, "empty").is_some());
        assert!(!actions.contains_key("empty-add"), "未打开表格时无处可新增");
        assert!(actions.values().all(|action| !matches!(action, Action::SetCell { .. })), "空态不得有输入格");
    }

    #[test]
    fn open_table_without_rows_shows_the_row_empty_state() {
        let snapshot = TableSnapshot { path: "任务.atb".into(), fields: vec![title_field()], rows: Vec::new(), ..Default::default() };
        let (tree, actions) = build_with(&snapshot, false);
        assert_eq!(text_of(&tree, "hdr-pos").as_deref(), Some("暂无数据"));
        assert_eq!(text_of(&tree, "empty").as_deref(), Some("点「新增行」开始填写。"));
        assert_eq!(enabled_of(&tree, "hdr-add"), Some(true));
        assert_eq!(actions.get("empty-add"), Some(&Action::AddRow), "空态应能直接新增行");
        assert!(actions.values().all(|action| !matches!(action, Action::SetCell { .. })));
    }

    fn title_field() -> Field {
        Field { id: "t".into(), name: "标题".into(), kind: FieldType::Text, options: vec![] }
    }

    #[test]
    fn radio_style_renders_option_buttons_and_clear() {
        let snapshot = snapshot_with(&[]);
        let mut config = TableFormConfig::default();
        config.set_radio("s", true);
        let slot = slot_of(&snapshot, &config, "s");
        let (tree, actions) = build(&FormView { snapshot: &snapshot, config: &config, row_index: 0, show_settings: false, status: None, image: &ImageFocus::default() });
        tree.validate().expect("按钮式单选应成树");
        let id = format!("ctl-{slot}-o0");
        assert_eq!(enabled_of(&tree, &id), Some(true));
        assert_eq!(actions.get(&id), Some(&Action::SelectOption { field: "s".to_string(), index: 0 }));
        // 当前值就是第 0 项：按钮高亮，不显示成未选
        assert!(matches!(&tree.find(&id).unwrap().body, NodeBody::Button { variant: Some(ButtonVariant::Primary), .. }));
        assert!(actions.contains_key(&format!("ctl-{slot}-clr")), "应能清除单选");
    }

    #[test]
    fn form_fields_are_grouped_in_one_sheet() {
        let snapshot = snapshot_with(&["assets/a.png"]);
        let config = TableFormConfig::default();
        let (tree, _) = build_with(&snapshot, false);
        assert!(matches!(&tree.find("hdr-card").expect("工具栏应是卡片").body, NodeBody::Card { tone: Some(CardTone::Muted), .. }));
        assert!(matches!(&tree.find("form-sheet").expect("表单应是一张纸卡").body, NodeBody::Card { .. }));
        let fields = config.visible_fields(&snapshot).len();
        for slot in 1..fields {
            assert!(tree.find(&format!("div-{slot}")).is_some(), "字段之间应有分隔线");
        }
        assert!(tree.find("div-0").is_none(), "首个字段前不画分隔线");
    }

    #[test]
    fn settings_header_keeps_done_visible_and_cards_form_a_grid() {
        let snapshot = snapshot_with(&[]);
        let (tree, actions) = build_with(&snapshot, true);
        assert_eq!(text_of(&tree, "hdr-title").as_deref(), Some("字段设置"));
        assert_eq!(actions.get("set-close"), Some(&Action::ToggleSettings));
        assert!(matches!(
            &tree.find("set-close").expect("完成按钮应在头部").body,
            NodeBody::Button { variant: Some(ButtonVariant::Primary), .. }
        ));
        let NodeBody::Grid { columns, .. } = &tree.find("set-grid").expect("字段设置应成网格").body else {
            panic!("字段设置应是网格");
        };
        assert_eq!(*columns, Some(2));
        assert!(tree.find("set-card-0").is_some());
    }
}

