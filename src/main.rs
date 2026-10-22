//! 插件入口：握手后注册表格视图贡献、订阅表格变更，之后「界面即状态的投影」——
//! 任一交互事件都改状态（必要时经 [`TableProvider`] 写值）再整树重发。
//!
//! 状态收在一把异步锁后，持锁直到 `view.submit` 落地，保证最后提交的树对应最终状态
//! （宿主通知逐任务分发、回调间无先后保证）。
//!
//! 逐格写回的时机：宿主每收到一次 `table.updateCell` 就压一步撤销快照，逐键写会把撤销
//! 碎到字符级。所以键盘输入先进 [`AppState::pending`]（界面按待写值投影，输入不丢），
//! 停手 [`FLUSH_DEBOUNCE`] 后每格只写一次；翻行、新增、刷新、进设置、界面卸载这些
//! 「一次输入的收尾点」先强制写完。`table:changed` 在有待写时只登记待重读，写完再读——
//! 立刻重读会把用户正在打的字按远端值盖掉。
//!
//! 宿主按「提交的控件值 = 本机刚上报的文本」才保留输入框里的内容，所以时长格另外留一份
//! 原文草稿（[`AppState::drafts`]）：待写值是按秒存的数字，回发的必须是用户敲的那串字符。
//!
//! 未写回的输入只属于当前挂载的那张表：换表、关表、界面卸载时若还没落盘就放弃并告知，
//! 不带着旧行 id 等下一次挂载（新表里同一个 id 可能指向别的行）。

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use atelyx_plugin_sdk::protocol::event::{self, PublishParams, TableChangedPayload};
use atelyx_plugin_sdk::protocol::method::{
    ContributionSlot, NotificationLevel, SurfaceAttachedParams, SurfaceDetachedParams, ViewEventParams,
};
use atelyx_plugin_sdk::protocol::view::{EventValue, ViewEvent, ViewNode};
use atelyx_plugin_sdk::{run_stdio, view_builders, Context, PluginHandler, PluginInfo};
use tokio::sync::Mutex;

mod config;
mod form;
mod host_table;
mod model;

use config::{FormConfig, TableFormConfig};
use form::{Action, FormView, ImageFocus, Preview};
use host_table::HostTableProvider;
use model::{CellValue, FieldType, ImageCell, TableProvider, TableSnapshot};

/// 表格视图贡献 id，同时是该面挂载后的 surfaceId。
const SURFACE: &str = "form";
const PLUGIN_ID: &str = "com.atelyx.table-form";
/// 键盘输入多久无新事件算「打完了」：短了撤销仍碎，长了用户等得慌。
const FLUSH_DEBOUNCE: Duration = Duration::from_millis(500);
/// 换表 / 关表都会走到这里，措辞按「当前表格变了」而非「切到了另一张」。
const DISCARDED_HINT: &str = "当前表格已变更，未写回的输入已丢弃";
/// 时长框输入不成形时的提示：说清可接受的写法，不猜用户想表达什么。
const DURATION_HINT: &str = "时长格式无效，可用 1:30:00 / 30:00 或秒数";

type Provider = Box<dyn TableProvider>;

/// 这一格里现存图片的引用；行找不到、字段不存在或不是图片列都回 `None`。
fn image_paths(snapshot: &TableSnapshot, row_id: Option<&str>, field: &str) -> Option<Vec<String>> {
    let row = snapshot.rows.iter().find(|row| Some(row.id.as_str()) == row_id)?;
    let field = snapshot.fields.iter().find(|item| item.id == field)?;
    if field.kind != FieldType::Image {
        return None;
    }
    Some(row.cell(field).images().to_vec())
}

/// 清掉指向不存在图片的焦点：行换了就整批归零；字段不再是图片列、或那一格已经不含这条引用，
/// 就把这条焦点删掉（预览对不上时一并退出预览），让状态与界面说同一件事。
fn prune_image(focus: &mut ImageFocus, snapshot: &TableSnapshot, row_id: Option<&str>) {
    if focus.row.as_deref() != row_id {
        focus.row = row_id.map(str::to_string);
        focus.cursors.clear();
        focus.preview = None;
        return;
    }
    focus.cursors.retain(|field, path| image_paths(snapshot, row_id, field).is_some_and(|paths| paths.contains(path)));
    let stale = focus
        .preview
        .as_ref()
        .is_some_and(|preview| image_paths(snapshot, row_id, &preview.field).is_none_or(|paths| !paths.contains(&preview.path)));
    if stale {
        focus.preview = None;
    }
}

/// 界面壳的自持状态：表格快照、配置、当前行与 UI 模式；`actions` 是最近一次提交的
/// 「节点 id → 动作」映射，供事件分派查表。
#[derive(Default)]
struct AppState {
    snapshot: Option<TableSnapshot>,
    config: FormConfig,
    /// 当前行按 id 锚定（远端增删行后序号会漂，行 id 不漂）。
    current_row: Option<String>,
    show_settings: bool,
    status: Option<String>,
    actions: HashMap<String, Action>,
    /// 图片格焦点：主图停在哪一张、是否在看大图预览（均按引用锚定）。
    image: ImageFocus,
    /// 待写格子：`(行 id, 字段 id) → 值`，同一格后写的值覆盖先写的。
    pending: BTreeMap<(String, String), CellValue>,
    /// 时长框里用户敲下的原文：写回要按秒存，但提交给宿主的值必须等于已上报文本，
    /// 否则宿主会把输入框重画成规范形态，`1:` 这种中间态敲不出来。
    drafts: BTreeMap<(String, String), String>,
    /// 每登记一次待写就自增；防抖任务只对得上最新一次，旧的直接退出。
    flush_epoch: u64,
    /// 是否还有一个没醒的防抖任务：只有「还在打字」才把表格变更推迟到写完再读。
    flush_scheduled: bool,
    /// 有待写期间收到的表格变更：写完再重读。
    reload_requested: bool,
    /// 界面当前是否挂在宿主面上（未挂载时提交过去只会是无效功）。
    mounted: bool,
    /// 界面提交是否已处于失败状态：转为失败时通知一次，不逐帧弹窗。
    submit_failing: bool,
}

impl AppState {
    /// 表单配置的按键：当前表格的仓库相对路径；未打开表格 = `None`。
    fn table_key(&self) -> Option<&str> {
        self.snapshot.as_ref().filter(|s| s.is_open()).map(|s| s.path.as_str())
    }

    /// 当前行序号：锚点不在行集里（远端删行且尚未重读）则回到首行。
    fn row_index(&self) -> usize {
        let Some(snapshot) = self.snapshot.as_ref() else { return 0 };
        if snapshot.rows.is_empty() {
            return 0;
        }
        self.current_row.as_deref().and_then(|id| snapshot.row_index_of(id)).unwrap_or(0)
    }

    fn current_row_id(&self) -> Option<String> {
        let snapshot = self.snapshot.as_ref()?;
        snapshot.rows.get(self.row_index()).map(|row| row.id.clone())
    }

    /// 翻到相邻行；越界不动（首末行由界面置灰按钮，这里只兜住竞态）。
    fn step_row(&mut self, delta: isize) {
        let index = self.row_index() as isize + delta;
        let total = self.snapshot.as_ref().map(|s| s.rows.len()).unwrap_or_default();
        if total == 0 || index < 0 || index >= total as isize {
            return;
        }
        self.current_row = self.snapshot.as_ref().map(|s| s.rows[index as usize].id.clone());
    }

    fn field_shape(&self, field: &str) -> Option<(FieldType, Vec<String>)> {
        let snapshot = self.snapshot.as_ref()?;
        let field = snapshot.fields.iter().find(|f| f.id == field)?;
        Some((field.kind.clone(), field.options.clone()))
    }

    /// 投影用的快照：把待写格子盖到真源值上，再按时长框的原文覆盖该格。界显示的就是
    /// 将要落盘的值，宿主才不会把用户已上报的文本按旧值重画回去。
    fn projected(&self) -> TableSnapshot {
        let mut snapshot = self.snapshot.clone().unwrap_or_default();
        for ((row_id, field_id), value) in &self.pending {
            if let Some(row) = snapshot.rows.iter_mut().find(|r| &r.id == row_id) {
                row.cells.insert(field_id.clone(), value.clone());
            }
        }
        for ((row_id, field_id), text) in &self.drafts {
            if let Some(row) = snapshot.rows.iter_mut().find(|r| &r.id == row_id) {
                row.cells.insert(field_id.clone(), CellValue::Text(text.clone()));
            }
        }
        snapshot
    }

    /// 放弃还没写回去的输入：这些值只属于当前挂载期间的这张表，表格换走或界面卸载后
    /// 再写就是往不确定的行上写。返回是否真的放弃了东西（供上层决定要不要告知）。
    fn discard_pending(&mut self) -> bool {
        let had = !self.pending.is_empty();
        self.pending.clear();
        self.drafts.clear();
        self.reload_requested = false;
        // 让还没醒的防抖任务失配：这一批已经作废，不能再被写出去
        self.flush_epoch += 1;
        self.flush_scheduled = false;
        had
    }

    /// 写入成功后同步本地快照，免得等下一次重读期间界面显示旧值。
    fn apply_cell(&mut self, row_id: &str, field_id: &str, value: &CellValue) {
        if let Some(row) = self.snapshot.as_mut().and_then(|s| s.rows.iter_mut().find(|r| r.id == row_id)) {
            row.cells.insert(field_id.to_string(), value.clone());
        }
    }

    /// 用新快照替换当前快照，并尽量让用户停在同一行：同表按行 id 跟随，行被远端删掉
    /// 就落到原序号位置；换表或表格关了就回首行 / 空态。
    fn adopt(&mut self, snapshot: Option<TableSnapshot>) {
        let same_table = match (self.snapshot.as_ref(), snapshot.as_ref()) {
            (Some(old), Some(new)) => old.path == new.path,
            _ => false,
        };
        let previous_index = self.row_index();
        let previous_row = self.current_row.clone();
        self.snapshot = snapshot;
        let Some(current) = self.snapshot.as_ref() else {
            self.current_row = None;
            self.show_settings = false;
            return;
        };
        if !same_table {
            self.current_row = None;
            self.show_settings = false;
            // 图片焦点属于原来那张表的行，留着会把旧引用落到新表的行上
            self.image = ImageFocus::default();
        } else {
            self.current_row = previous_row
                .filter(|id| current.row_index_of(id).is_some())
                .or_else(|| current.rows.get(previous_index).map(|row| row.id.clone()));
        }
    }
}
/// 入口状态：`provider` 要等握手拿到 [`Context`] 才能建，故先占位、`on_ready` 里落一次。
struct TableForm {
    state: Arc<Mutex<AppState>>,
    provider: Arc<tokio::sync::OnceCell<Provider>>,
    /// 最近一次建好的树：渲染持状态锁时更新，托管循环的构建器同步取用。
    tree: Arc<std::sync::Mutex<Option<ViewNode>>>,
}

impl TableForm {
    fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(AppState::default())),
            provider: Arc::new(tokio::sync::OnceCell::new()),
            tree: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// 停手后写回待写格子：旧任务醒来发现序号已被后来的按键推进，就交给最新那次收尾。
    fn schedule_flush(&self, ctx: &Context, epoch: u64) {
        let state = self.state.clone();
        let provider = self.provider.clone();
        let tree = self.tree.clone();
        let ctx = ctx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(FLUSH_DEBOUNCE).await;
            let mut st = state.lock().await;
            if st.flush_epoch != epoch {
                return;
            }
            st.flush_scheduled = false;
            report(&ctx, settle(provider.get(), &mut st, false).await).await;
            render(&ctx, &mut st, &tree).await;
        });
    }
}

#[async_trait::async_trait]
impl PluginHandler for TableForm {
    async fn on_ready(&self, ctx: &Context) {
        // 先挂构建器：`host:surface.attached` 与 ready 并发到达，回调里的提交要到落位
        let tree = Arc::clone(&self.tree);
        ctx.ui()
            .mount(SURFACE, move || {
                tree.lock()
                    .unwrap()
                    .clone()
                    .unwrap_or_else(|| view_builders::column("root", Vec::new()))
            })
            .await;
        if let Err(e) = ctx.subscribe(event::name::TABLE_CHANGED).await {
            eprintln!("订阅表格变更失败：{e}");
            notify(ctx, "订阅表格变更失败，界面不会随表格自动刷新", NotificationLevel::Warning).await;
        }
        if let Err(e) = ctx.register(ContributionSlot::TableView, SURFACE, "表格表单", Some("table"), None).await {
            eprintln!("注册表格视图贡献失败：{e}");
            notify(ctx, &format!("注册表格视图失败：{e}"), NotificationLevel::Error).await;
        }
        let _ = self.provider.set(Box::new(HostTableProvider::new(ctx)));
        let mut st = self.state.lock().await;
        st.config = load_config(ctx).await;
        report(ctx, settle(self.provider.get(), &mut st, true).await).await;
        render(ctx, &mut st, &self.tree).await;
    }

    async fn on_surface_attached(&self, ctx: &Context, params: SurfaceAttachedParams) {
        if params.surface_id != SURFACE {
            return;
        }
        let mut st = self.state.lock().await;
        st.mounted = true;
        // 挂载即一次全新投影：图片焦点属于上一次挂载，不带过来
        st.image = ImageFocus::default();
        // 上一次挂载留下的待写值属于当时那张表（卸载时写不出去才会留下），
        // 挂载点重新确认前一律放弃，不拿去赌新表里有没有同一个行 id。
        if st.discard_pending() {
            notify(ctx, DISCARDED_HINT, NotificationLevel::Warning).await;
        }
        // 挂载即一次全新投影：以宿主当前打开的表格为准
        report(ctx, settle(self.provider.get(), &mut st, true).await).await;
        render(ctx, &mut st, &self.tree).await;
    }

    async fn on_surface_detached(&self, ctx: &Context, params: SurfaceDetachedParams) {
        if params.surface_id != SURFACE {
            return;
        }
        let mut st = self.state.lock().await;
        st.mounted = false;
        // 卸载前把在打的字写完：面已经不在了，待写值没机会再被投影出去
        report(ctx, settle(self.provider.get(), &mut st, false).await).await;
        // 写不出去的（多半是表格已先一步关闭）就地作废，不留到下一次挂载
        if st.discard_pending() {
            notify(ctx, "视图已关闭，未写回的输入已丢弃", NotificationLevel::Warning).await;
        }
    }

    async fn on_host_event(&self, ctx: &Context, params: PublishParams) {
        if params.name != event::name::TABLE_CHANGED {
            return;
        }
        let Ok(payload) = serde_json::from_value::<TableChangedPayload>(params.payload) else {
            eprintln!("表格变更事件不可解析，忽略");
            return;
        };
        let mut st = self.state.lock().await;
        if !st.mounted {
            return; // 未挂载时无需刷新，重新挂载会重读
        }
        match note_table_change(&mut st, payload.path.as_deref()) {
            TableChange::Defer => return,
            TableChange::Switched { dropped: true } => {
                notify(ctx, DISCARDED_HINT, NotificationLevel::Warning).await;
            }
            TableChange::Switched { dropped: false } | TableChange::Reload => {}
        }
        report(ctx, settle(self.provider.get(), &mut st, true).await).await;
        render(ctx, &mut st, &self.tree).await;
    }

    async fn on_view_event(&self, ctx: &Context, params: ViewEventParams) {
        if params.surface_id != SURFACE {
            return;
        }
        let mut st = self.state.lock().await;
        let Some(action) = st.actions.get(&params.node_id).cloned() else {
            // 未知节点：多为并发回调下界面已过期（另一任务先重绘），忽略并留痕，绝不静默吞掉。
            eprintln!("忽略未登记的界面事件：node = {}", params.node_id);
            return;
        };
        let typing = matches!(action, Action::SetCell { .. });
        let mut boundary_failed = false;
        if !typing {
            // 除逐键输入外的动作都是一次输入的收尾点：先把待写格子落地再动状态
            let failures = settle(self.provider.get(), &mut st, matches!(action, Action::Refresh)).await;
            boundary_failed = !failures.is_empty();
            report(ctx, failures).await;
        }
        match action {
            Action::SetCell { field } => self.on_input(ctx, &mut st, &field, &params.event).await,
            Action::NavPrev => {
                st.step_row(-1);
                st.status = None;
            }
            Action::NavNext => {
                st.step_row(1);
                st.status = None;
            }
            Action::AddRow => report(ctx, add_row(self.provider.get(), &mut st).await).await,
            Action::Refresh => {
                // 重读失败时状态条里已经是失败原因，不能再报「已刷新」
                if !boundary_failed {
                    st.status = Some("已刷新".to_string());
                }
            }
            Action::ToggleSettings => {
                st.show_settings = !st.show_settings;
                // 预览与设置面板互斥：进出设置都退回表单
                st.image.preview = None;
                st.status = None;
            }
            Action::SelectOption { field, index } => {
                let option = st
                    .field_shape(&field)
                    .and_then(|(_, options)| options.get(index).cloned())
                    .map(|option| CellValue::Choice(Some(option)));
                match option {
                    Some(value) => self.commit(ctx, &mut st, &field, value).await,
                    None => {
                        // 界面已过期（选项刚被改过）：按下标取不到就不猜，提示重选
                        eprintln!("忽略越界的单选项：field = {field}, index = {index}");
                        st.status = Some("选项已变化，请重新选择".to_string());
                    }
                }
            }
            Action::ClearSelect { field } => self.commit(ctx, &mut st, &field, CellValue::Choice(None)).await,
            Action::RemoveImage { field, path } => match image_without(&st, &field, &path) {
                Some(value) => self.commit(ctx, &mut st, &field, value).await,
                None => {
                    eprintln!("忽略找不到引用的移除请求：field = {field}, path = {path}");
                    st.status = Some("这一格已经没有那张图片".to_string());
                }
            },
            Action::ShowImage { field, path } => {
                if let Some(row_id) = st.current_row_id() {
                    st.image.focus(&row_id, &field, &path);
                }
            }
            Action::OpenImage { field } => match (st.current_row_id(), shown_image(&st, &field)) {
                (Some(row_id), Some(path)) => st.image.open(&row_id, &field, &path),
                // 界面已过期（这一格刚被清空）：点了没反应要说得出原因，不留静默失败
                _ => {
                    eprintln!("忽略没有可预览图片的请求：field = {field}");
                    st.status = Some("这一格已经没有可预览的图片".to_string());
                }
            },
            Action::ImageStep { delta } => step_image(&mut st, delta),
            Action::CloseImage => st.image.preview = None,
            Action::ToggleField { field } => match event_bool(&params.event) {
                Some(show) => {
                    edit_config(&mut st, |cfg| cfg.toggle_hidden(&field, show));
                    persist_config(ctx, &st).await;
                }
                None => eprintln!("忽略不带勾选状态的字段显隐事件：field = {field}"),
            },
            Action::ToggleRadio { field } => match event_bool(&params.event) {
                Some(on) => {
                    edit_config(&mut st, |cfg| cfg.set_radio(&field, on));
                    persist_config(ctx, &st).await;
                }
                None => eprintln!("忽略不带勾选状态的单选样式事件：field = {field}"),
            },
            Action::SetHelp { field } => match event_text(&params.event) {
                Some(text) => {
                    edit_config(&mut st, |cfg| cfg.set_help(&field, &text));
                    persist_config(ctx, &st).await;
                }
                None => eprintln!("忽略不带文本的提示编辑事件：field = {field}"),
            },
            Action::MoveField { field, delta } => {
                if let Some(snapshot) = st.snapshot.clone() {
                    let key = snapshot.path.clone();
                    st.config.table_mut(&key).move_field(&snapshot, &field, delta);
                }
                persist_config(ctx, &st).await;
            }
        }
        render(ctx, &mut st, &self.tree).await;
    }
}

// ---------- 渲染 ----------

/// 由当前状态建树并整树提交；持锁调用，确保「最后落地的树 = 最终状态」。
async fn render(ctx: &Context, st: &mut AppState, tree_slot: &std::sync::Mutex<Option<ViewNode>>) {
    if !st.mounted {
        return;
    }
    let snapshot = st.projected();
    let config = st.table_key().map(|key| st.config.table(key)).unwrap_or_default();
    // 建树前先把图片焦点对到真图上：焦点指向的那张已被删掉时，界面与状态不得各说各话
    let row_id = st.current_row_id();
    prune_image(&mut st.image, &snapshot, row_id.as_deref());
    let (tree, actions) = form::build(&FormView {
        snapshot: &snapshot,
        config: &config,
        row_index: st.row_index(),
        show_settings: st.show_settings,
        status: st.status.as_deref(),
        image: &st.image,
    });
    st.actions = actions;
    *tree_slot.lock().unwrap() = Some(tree);
    match ctx.ui().notify(SURFACE).await {
        Ok(_) => st.submit_failing = false,
        // 提交被拒 = 界面停在旧树上，用户看不出差别，转成失败时通知一次
        Err(e) => {
            eprintln!("提交界面失败：{e}");
            if !st.submit_failing {
                st.submit_failing = true;
                notify(ctx, &format!("界面提交失败：{e}"), NotificationLevel::Error).await;
            }
        }
    }
}

// ---------- 写值 ----------

impl TableForm {
    /// 键盘输入：文本 / 数字 / 时长记进待写并重排防抖；下拉一次点定，直接写。
    async fn on_input(&self, ctx: &Context, st: &mut AppState, field: &str, event: &ViewEvent) {
        let Some((kind, options)) = st.field_shape(field) else {
            st.status = Some("数据未就绪".to_string());
            return;
        };
        let Some(row_id) = st.current_row_id() else {
            st.status = Some("数据未就绪".to_string());
            return;
        };
        match record_input(st, &row_id, &kind, &options, field, event) {
            Typed::Queued => {
                let epoch = st.flush_epoch;
                self.schedule_flush(ctx, epoch);
            }
            Typed::Hinted(hint) => st.status = Some(hint.to_string()),
            Typed::Discrete(value) => self.commit(ctx, st, field, value).await,
            Typed::Ignored => eprintln!("忽略与字段类型不符的控件事件：field = {field}"),
        }
    }

    /// 离散写（点选、清除、移除图片）：先把待写批次落地，再写这一格，先后顺序不乱。
    async fn commit(&self, ctx: &Context, st: &mut AppState, field: &str, value: CellValue) {
        let Some(provider) = self.provider.get() else {
            st.status = Some("数据未就绪".to_string());
            return;
        };
        report(ctx, flush_pending(provider.as_ref(), st).await).await;
        let Some(row_id) = st.current_row_id() else {
            st.status = Some("数据未就绪".to_string());
            return;
        };
        let failures = write_cells(provider.as_ref(), st, vec![((row_id, field.to_string()), value)]).await;
        report(ctx, failures).await;
    }
}

/// 写完待写格子并按需重读快照（一次用户输入的收尾）。返回需要通知用户的失败原因。
async fn settle(provider: Option<&Provider>, st: &mut AppState, force_reload: bool) -> Vec<String> {
    let Some(provider) = provider else {
        return failure(st, "表格数据未就绪".to_string());
    };
    let mut failures = flush_pending(provider.as_ref(), st).await;
    if force_reload || st.reload_requested {
        st.reload_requested = false;
        failures.extend(reload_snapshot(provider.as_ref(), st).await);
    }
    failures
}

/// 把待写格子一次性写回；无待写则不产生任何写。
async fn flush_pending(provider: &dyn TableProvider, st: &mut AppState) -> Vec<String> {
    if st.pending.is_empty() {
        return Vec::new();
    }
    // 让在途的防抖任务全部失配：这一批由当前调用写完
    st.flush_epoch += 1;
    st.flush_scheduled = false;
    let batch = std::mem::take(&mut st.pending);
    let keys: Vec<(String, String)> = batch.keys().cloned().collect();
    let failures = write_cells(provider, st, batch).await;
    // 原文草稿只销这一批里写成功的格子：写失败的连着值一起留着，没参与本批次的更不动——
    // 否则点缩略图这类不写值的动作也会把用户正在敲的 `1:` 抹成宿主重画的规范形态。
    for key in keys {
        if !st.pending.contains_key(&key) {
            st.drafts.remove(&key);
        }
    }
    failures
}

/// 逐格写回：一格一次写，成功同步本地快照；失败的格子退回待写（输入继续显示在界面上，
/// 下一次收尾再写），并留下可读原因。值只会写进登记时锚定的那个 `(行 id, 字段 id)`。
async fn write_cells<I>(provider: &dyn TableProvider, st: &mut AppState, batch: I) -> Vec<String>
where
    I: IntoIterator<Item = ((String, String), CellValue)>,
{
    let mut failures: Vec<String> = Vec::new();
    for (key, value) in batch {
        let (row_id, field_id) = key;
        match provider.update_cell(&row_id, &field_id, &value).await {
            // 成功即销账：这一格早先失败留下的旧待值不能再写出去
            Ok(()) => {
                st.pending.remove(&(row_id.clone(), field_id.clone()));
                st.apply_cell(&row_id, &field_id, &value);
            }
            Err(e) => {
                let label = st
                    .snapshot
                    .as_ref()
                    .and_then(|s| s.fields.iter().find(|f| f.id == field_id))
                    .map(|f| f.name.clone())
                    .unwrap_or(field_id.clone());
                let msg = format!("「{label}」写入失败：{e}");
                st.status = Some(msg.clone());
                st.pending.insert((row_id, field_id), value);
                failures.push(msg);
            }
        }
    }
    failures
}

async fn reload_snapshot(provider: &dyn TableProvider, st: &mut AppState) -> Vec<String> {
    match provider.snapshot().await {
        Ok(snapshot) => {
            st.adopt(snapshot);
            Vec::new()
        }
        Err(e) => failure(st, format!("读取表格失败：{e}")),
    }
}

/// 新增一行并把界面定位到它。只有新宿主快照里确有这一行才允许锚定：定位不到就停在原行
/// 报错，不退回首行——那会把用户接下来敲的字写进别的行。
async fn add_row(provider: Option<&Provider>, st: &mut AppState) -> Vec<String> {
    let Some(provider) = provider else {
        return failure(st, "表格数据未就绪".to_string());
    };
    let id = match provider.add_row().await {
        Ok(id) => id,
        Err(e) => return failure(st, format!("新增行失败：{e}")),
    };
    // 新行的空单元格与总行数都要以宿主为准：紧接着重读一次
    let mut failures = reload_snapshot(provider.as_ref(), st).await;
    if st.snapshot.as_ref().is_some_and(|snapshot| snapshot.row_index_of(&id).is_some()) {
        st.current_row = Some(id);
        return failures;
    }
    let msg = "新行未出现在表格中，请刷新后重试".to_string();
    failures.push(msg.clone());
    st.status = Some(msg);
    eprintln!("宿主返回的新行 id 不在快照里：{id}");
    failures
}

/// 记下用户可见的失败原因，并作为待通知项返回。
fn failure(st: &mut AppState, msg: String) -> Vec<String> {
    st.status = Some(msg.clone());
    vec![msg]
}

/// `table:changed` 的处置决定（只动状态，便于时序用例覆盖）。
#[derive(Debug, PartialEq, Eq)]
enum TableChange {
    /// 换表或关表：待写格子属于原来那张表，作废；`dropped` = 确实作废掉了东西。
    Switched { dropped: bool },
    /// 同一张表的内容结算、且还在打字：推迟重读，写完再读。
    Defer,
    /// 现在就重读。
    Reload,
}

fn note_table_change(st: &mut AppState, path: Option<&str>) -> TableChange {
    // 路径对不上就是换表 / 关表：按「宁可少写不可错写」放弃旧表的输入，不拿去赌新表里有同 id 的行
    if path != st.table_key() {
        return TableChange::Switched { dropped: st.discard_pending() };
    }
    // 只有防抖任务还挂着才算「正在打字」：写失败留下的待值不该把重读一直挡住
    if !st.pending.is_empty() && st.flush_scheduled {
        st.reload_requested = true;
        return TableChange::Defer;
    }
    TableChange::Reload
}

/// 去掉图片格里的这一张引用，其余路径与该格显示模式等原键保留。
/// 引用 → 序号在投影快照上现算：界面显示的就是投影那一份（含未写回的待值），而只有此刻的
/// 序号才对应这一张——沿用点击时算好的序号会把删除落到别的图上。
fn image_without(st: &AppState, field: &str, path: &str) -> Option<CellValue> {
    let (_, cell) = image_slot(st, field)?;
    let index = cell.paths.iter().position(|item| item == path)?;
    cell.without(index).map(CellValue::Images)
}

/// 当前行里这一图片格的落位：行 id + 图片格现值。字段不是图片列、值形态不符都回 `None`
/// （不拿新路径去盖用户原来的非图片内容）。
fn image_slot(st: &AppState, field: &str) -> Option<(String, ImageCell)> {
    let snapshot = st.projected();
    let row = snapshot.rows.get(st.row_index())?;
    let field = snapshot.fields.iter().find(|item| item.id == field)?;
    let CellValue::Images(cell) = row.cell(field) else { return None };
    Some((row.id.clone(), cell))
}

/// 界面正在显示的那一张的引用：与 `form` 的主图取法一致（焦点行不符、这一格已不含那张，都回第 0 张）。
fn shown_image(st: &AppState, field: &str) -> Option<String> {
    let (row_id, cell) = image_slot(st, field)?;
    if cell.paths.is_empty() {
        return None;
    }
    let index = st.image.cursor(&row_id, field, &cell.paths);
    Some(cell.paths[index].clone())
}

/// 预览里翻到相邻那张：首尾相接，并把主图焦点一起带到这一张——退出预览后显示的就是它。
/// 预览指向的那张已不在格子里（被删了、或字段不再是图片列）时什么都不动。
fn step_image(st: &mut AppState, delta: isize) {
    let Some(preview) = st.image.preview.clone() else { return };
    let Some((row_id, cell)) = image_slot(st, &preview.field) else { return };
    let Some(current) = cell.paths.iter().position(|path| path == &preview.path) else { return };
    let next = (current as isize + delta).rem_euclid(cell.paths.len() as isize) as usize;
    let path = cell.paths[next].clone();
    st.image.focus(&row_id, &preview.field, &path);
    st.image.preview = Some(Preview { field: preview.field, path });
}

// ---------- 配置读写（插件自持数据，失败不静默） ----------

async fn load_config(ctx: &Context) -> FormConfig {
    match ctx.state_read().await {
        Ok(value) if !value.is_null() => serde_json::from_value::<FormConfig>(value).unwrap_or_else(|e| {
            // 解析失败按空配置继续（宿主已备份损坏原文），但留痕避免静默覆盖历史配置
            eprintln!("解析已存配置失败，按默认配置继续：{e}");
            FormConfig::default()
        }),
        Ok(_) => FormConfig::default(),
        Err(e) => {
            eprintln!("读取配置失败，按默认配置继续：{e}");
            FormConfig::default()
        }
    }
}

async fn persist_config(ctx: &Context, st: &AppState) {
    if st.table_key().is_none() {
        return; // 没有打开表格就没有可配置的表，配置改动留在内存里等下一次落盘
    }
    let value = match serde_json::to_value(&st.config) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("序列化配置失败：{e}");
            return;
        }
    };
    if let Err(e) = ctx.state_write(value).await {
        let msg = format!("表单配置保存失败：{e}，改动仅在界面生效");
        eprintln!("{msg}");
        notify(ctx, &msg, NotificationLevel::Warning).await;
    }
}

// ---------- 纯函数：输入记账、事件值解码与通知 ----------

/// 一次控件事件记账后的走向。
#[derive(Debug, PartialEq)]
enum Typed {
    /// 已记进待写并推进防抖序号。
    Queued,
    /// 输入不成形：待写不动，把原因显示出来。
    Hinted(&'static str),
    /// 与这一格无关的迟到事件。
    Ignored,
    /// 一次点定的值（下拉），由调用方立即写。
    Discrete(CellValue),
}

/// 把一次输入事件记进状态：待写值、时长原文草稿、防抖序号。
///
/// 时长的原文必须单独留：写回的是秒数，而宿主只在本机「已上报文本 = 提交回来的值」时保留
/// 输入框内容，回发规范形态会把 `1:` 这种中间态重画掉，用户就敲不出 `1:30:00`。
fn record_input(st: &mut AppState, row_id: &str, kind: &FieldType, options: &[String], field: &str, event: &ViewEvent) -> Typed {
    let Some(decoded) = cell_from_event(kind, options, event) else {
        return Typed::Ignored;
    };
    // 时长格无论敲成什么样都先把原文留住：这一帧要回发的值必须等于用户看到的文本
    if *kind == FieldType::Duration {
        if let Some(text) = event_text(event) {
            st.drafts.insert((row_id.to_string(), field.to_string()), text);
        }
    }
    match decoded {
        Ok(value) if matches!(kind, FieldType::Text | FieldType::Number | FieldType::Duration) => {
            st.pending.insert((row_id.to_string(), field.to_string()), value);
            st.flush_epoch += 1;
            st.flush_scheduled = true;
            st.status = None;
            Typed::Queued
        }
        Ok(value) => Typed::Discrete(value),
        Err(hint) => Typed::Hinted(hint),
    }
}

/// 按字段类型把控件事件值解码成单元格值。
/// `None` = 该事件与此格无关（迟到的旧控件事件），`Some(Err)` = 用户输入不成形（提示、不写）。
fn cell_from_event(kind: &FieldType, options: &[String], event: &ViewEvent) -> Option<Result<CellValue, &'static str>> {
    let value = match event {
        ViewEvent::Change { value } => value,
        // 界面只声明 change，其余事件不会上行（迟到的旧控件事件也落在这里）
        _ => return None,
    };
    match kind {
        FieldType::Text => match value {
            EventValue::Text(text) => Some(Ok(CellValue::Text(text.clone()))),
            _ => None,
        },
        FieldType::Number => match value {
            // 宿主在数字框清空时上行 null，与「值为 0」区分
            EventValue::Number(n) => Some(Ok(CellValue::Number(Some(*n)))),
            EventValue::Null => Some(Ok(CellValue::Number(None))),
            _ => None,
        },
        FieldType::Duration => match value {
            EventValue::Text(text) if text.trim().is_empty() => Some(Ok(CellValue::Duration(None))),
            EventValue::Text(text) => Some(
                model::parse_duration(text)
                    .map(|seconds| CellValue::Duration(Some(seconds)))
                    .ok_or(DURATION_HINT),
            ),
            _ => None,
        },
        FieldType::SingleSelect => match value {
            EventValue::Number(index) => Some(match_options(options, *index)),
            _ => None,
        },
        // 图片格与未知类型的格子没有可写的输入事件
        FieldType::Image | FieldType::Other(_) => None,
    }
}

/// 下拉按下标取选项；下标越界多半是界面已过期（选项刚被改过），提示而不是写错值。
fn match_options(options: &[String], index: f64) -> Result<CellValue, &'static str> {
    options
        .get(index as usize)
        .cloned()
        .map(|option| CellValue::Choice(Some(option)))
        .ok_or("选项已变化，请重新选择")
}

fn event_text(event: &ViewEvent) -> Option<String> {
    match event {
        ViewEvent::Change { value: EventValue::Text(s) } => Some(s.clone()),
        _ => None,
    }
}

fn event_bool(event: &ViewEvent) -> Option<bool> {
    match event {
        ViewEvent::Change { value: EventValue::Bool(b) } => Some(*b),
        _ => None,
    }
}

/// 就地处编辑当前表的配置；未打开表格则跳过（没有可配置的表）。
fn edit_config<F: FnOnce(&mut TableFormConfig)>(st: &mut AppState, f: F) {
    let Some(key) = st.table_key().map(str::to_string) else { return };
    f(st.config.table_mut(&key));
}

/// 一批失败合并发一条通知：逐格弹窗会把真正要看的原因刷掉。
async fn report(ctx: &Context, failures: Vec<String>) {
    let Some(first) = failures.first() else { return };
    let body = match failures.len() {
        1 => first.clone(),
        more => format!("{first}（另有 {} 处失败）", more - 1),
    };
    notify(ctx, &body, NotificationLevel::Error).await;
}

async fn notify(ctx: &Context, body: &str, level: NotificationLevel) {
    if let Err(e) = ctx.notify("表格表单", Some(body), level).await {
        eprintln!("发通知失败：{e}");
    }
}

#[tokio::main]
async fn main() {
    let info = PluginInfo { id: PLUGIN_ID.into(), name: "表格表单".into(), version: env!("CARGO_PKG_VERSION").into() };
    if let Err(e) = run_stdio(info, TableForm::new()).await {
        eprintln!("插件启动失败：{e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use model::{Field, Row};
    use serde_json::{json, Map};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex as StdMutex;

    /// 记录写请求、可编排快照与新增行结果的假接缝。
    #[derive(Clone, Default)]
    struct FakeWire {
        writes: Arc<StdMutex<Vec<(String, String, CellValue)>>>,
        snapshot: Arc<StdMutex<Option<TableSnapshot>>>,
        added: Arc<StdMutex<Option<String>>>,
        fail: Arc<AtomicBool>,
    }

    struct FakeProvider {
        wire: FakeWire,
    }

    impl FakeProvider {
        fn new() -> Self {
            Self { wire: FakeWire::default() }
        }

        fn taken(&self) -> Vec<(String, String, CellValue)> {
            std::mem::take(&mut *self.wire.writes.lock().unwrap())
        }
    }

    #[async_trait::async_trait]
    impl TableProvider for FakeProvider {
        async fn snapshot(&self) -> Result<Option<TableSnapshot>, String> {
            Ok(self.wire.snapshot.lock().unwrap().clone())
        }

        async fn update_cell(&self, row_id: &str, field_id: &str, value: &CellValue) -> Result<(), String> {
            if self.wire.fail.load(Ordering::Relaxed) {
                return Err("行不存在".to_string());
            }
            self.wire.writes.lock().unwrap().push((row_id.to_string(), field_id.to_string(), value.clone()));
            Ok(())
        }

        async fn add_row(&self) -> Result<String, String> {
            self.wire.added.lock().unwrap().clone().ok_or_else(|| "假接缝未备好新行".to_string())
        }
    }

    fn title_field() -> Field {
        Field { id: "f".into(), name: "标题".into(), kind: FieldType::Text, options: vec![] }
    }

    fn duration_field() -> Field {
        Field { id: "d".into(), name: "时长".into(), kind: FieldType::Duration, options: vec![] }
    }

    /// 带文本格与时长格的快照：写回与投影时序用例用它。
    fn snapshot_typed(rows: &[&str]) -> TableSnapshot {
        let fields = vec![title_field(), duration_field()];
        let rows = rows.iter().map(|id| Row { id: id.to_string(), cells: BTreeMap::new() }).collect();
        TableSnapshot { path: "任务.atb".into(), fields, rows }
    }

    fn snapshot_with(rows: &[&str]) -> TableSnapshot {
        TableSnapshot { rows: snapshot_typed(rows).rows, fields: vec![title_field()], path: "任务.atb".into() }
    }

    fn state_with(rows: &[&str]) -> AppState {
        state_from(snapshot_with(rows))
    }

    fn state_typed(rows: &[&str]) -> AppState {
        state_from(snapshot_typed(rows))
    }

    fn state_from(snapshot: TableSnapshot) -> AppState {
        let current_row = snapshot.rows.first().map(|row| row.id.clone());
        AppState { snapshot: Some(snapshot), current_row, ..AppState::default() }
    }

    fn change(value: EventValue) -> ViewEvent {
        ViewEvent::Change { value }
    }

    fn change_text(text: &str) -> ViewEvent {
        change(EventValue::Text(text.to_string()))
    }

    #[test]
    fn number_and_duration_events_decode() {
        assert_eq!(cell_from_event(&FieldType::Number, &[], &change(EventValue::Number(8.0))), Some(Ok(CellValue::Number(Some(8.0)))));
        // 数字框清空上行 null，与「值为 0」区分
        assert_eq!(cell_from_event(&FieldType::Number, &[], &change(EventValue::Null)), Some(Ok(CellValue::Number(None))));
        assert_eq!(
            cell_from_event(&FieldType::Duration, &[], &change_text("1:30:00")),
            Some(Ok(CellValue::Duration(Some(5400.0))))
        );
        assert_eq!(cell_from_event(&FieldType::Duration, &[], &change_text("  ")), Some(Ok(CellValue::Duration(None))));
        let half_typed = cell_from_event(&FieldType::Duration, &[], &change_text("1:")).unwrap();
        assert_eq!(half_typed.unwrap_err(), DURATION_HINT, "不成形的时长只提示，不当成 0 写出去");
        // 文本格清空编成 null（宿主删键），不把空串写出去
        let cleared = cell_from_event(&FieldType::Text, &[], &change_text("")).unwrap().unwrap();
        assert_eq!(cleared.to_wire(), Some(serde_json::Value::Null));
    }

    #[test]
    fn select_option_out_of_range_is_hinted_not_written() {
        let options = vec!["进行中".to_string(), "已完成".to_string()];
        let picked = cell_from_event(&FieldType::SingleSelect, &options, &change(EventValue::Number(1.0)));
        assert_eq!(picked.unwrap().unwrap(), CellValue::Choice(Some("已完成".into())));
        let stale = cell_from_event(&FieldType::SingleSelect, &options, &change(EventValue::Number(9.0)));
        assert!(stale.unwrap().is_err(), "越界下标不能写成别的选项");
    }

    #[test]
    fn image_and_unknown_cells_take_no_input_events() {
        assert_eq!(cell_from_event(&FieldType::Image, &[], &change_text("x")), None);
        assert_eq!(cell_from_event(&FieldType::Other("formula".into()), &[], &change_text("x")), None);
    }

    #[tokio::test]
    async fn pending_cells_coalesce_to_one_write_each() {
        let provider = FakeProvider::new();
        let mut st = state_with(&["r1", "r2"]);
        for text in ["一", "二", "三"] {
            st.pending.insert(("r1".to_string(), "f".to_string()), CellValue::Text(text.to_string()));
        }
        st.pending.insert(("r2".to_string(), "f".to_string()), CellValue::Text("甲".to_string()));
        assert!(flush_pending(&provider, &mut st).await.is_empty());
        let writes = provider.taken();
        assert_eq!(writes.len(), 2, "同一串的多次输入每格只写一次");
        assert_eq!(writes[0].2, CellValue::Text("三".to_string()));
        assert!(st.pending.is_empty());
        // 写成功的值立刻进本地快照，不等下一次重读
        assert_eq!(st.snapshot.as_ref().unwrap().rows[0].cell(&title_field()), CellValue::Text("三".to_string()));
    }

    #[tokio::test]
    async fn failed_cell_stays_pending_and_reports_the_field() {
        let provider = FakeProvider::new();
        provider.wire.fail.store(true, Ordering::Relaxed);
        let mut st = state_with(&["r1"]);
        st.pending.insert(("r1".to_string(), "f".to_string()), CellValue::Text("半截输入".to_string()));
        let failures = flush_pending(&provider, &mut st).await;
        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains("标题"), "失败原因要能定位到字段：{failures:?}");
        assert_eq!(
            st.pending.get(&("r1".to_string(), "f".to_string())),
            Some(&CellValue::Text("半截输入".to_string())),
            "写失败的输入不得凭空消失"
        );
        assert!(!st.flush_scheduled, "没有防抖任务在等这一格，重读不该被它挡住");
    }

    #[tokio::test]
    async fn flush_releases_the_typed_text_of_duration_cells() {
        let provider = FakeProvider::new();
        let mut st = state_typed(&["r1"]);
        st.pending.insert(("r1".to_string(), "d".to_string()), CellValue::Duration(Some(5400.0)));
        st.drafts.insert(("r1".to_string(), "d".to_string()), "1:30:".to_string());
        assert!(flush_pending(&provider, &mut st).await.is_empty());
        assert!(st.drafts.is_empty(), "写回后回到按秒存的规范形态");
        assert_eq!(st.projected().rows[0].cell(&duration_field()).as_edit_text(), "1:30:00");
    }

    #[tokio::test]
    async fn a_write_only_releases_the_draft_it_wrote() {
        let provider = FakeProvider::new();
        let mut st = state_typed(&["r1"]);
        // 只有标题格在待写，时长格里用户正敲着 `1:`：这一批写Complete也不该动那格的原文
        st.pending.insert(("r1".to_string(), "f".to_string()), CellValue::Text("登录页".to_string()));
        st.drafts.insert(("r1".to_string(), "d".to_string()), "1:".to_string());
        assert!(flush_pending(&provider, &mut st).await.is_empty());
        assert_eq!(st.drafts.get(&("r1".to_string(), "d".to_string())).map(String::as_str), Some("1:"), "没参与这一批的草稿要留着");
        assert_eq!(st.projected().rows[0].cell(&duration_field()).as_edit_text(), "1:");
    }

    #[tokio::test]
    async fn a_failed_write_keeps_both_value_and_draft() {
        let provider = FakeProvider::new();
        provider.wire.fail.store(true, Ordering::Relaxed);
        let mut st = state_typed(&["r1"]);
        st.pending.insert(("r1".to_string(), "d".to_string()), CellValue::Duration(Some(5400.0)));
        st.drafts.insert(("r1".to_string(), "d".to_string()), "1:30:".to_string());
        assert_eq!(flush_pending(&provider, &mut st).await.len(), 1);
        assert_eq!(st.drafts.get(&("r1".to_string(), "d".to_string())).map(String::as_str), Some("1:30:"), "写失败要连着原文一起留着");
    }

    #[tokio::test]
    async fn a_later_success_writes_off_an_earlier_failure() {
        // 同一格：早先写失败退回过 pending，随后一次离散写成功——旧待值不能再被下一次收尾写出去
        let provider = FakeProvider::new();
        let mut st = image_state(&["assets/a.png"]);
        let key = ("r1".to_string(), "i".to_string());
        st.pending.insert(key.clone(), CellValue::Foreign(json!(["assets/gone.png"])));
        let fresh = CellValue::Images(ImageCell { paths: vec!["assets/a.png".to_string()], ..ImageCell::default() });
        assert!(write_cells(&provider, &mut st, vec![(key.clone(), fresh.clone())]).await.is_empty());
        assert!(!st.pending.contains_key(&key), "写成功的格子必须销掉同键的旧待值");
        assert_eq!(st.snapshot.as_ref().unwrap().rows[0].cells[&key.1], fresh);
        assert_eq!(provider.taken(), vec![("r1".to_string(), "i".to_string(), fresh)]);
    }

    #[test]
    fn half_typed_duration_keeps_text_and_previous_value() {
        let mut st = state_typed(&["r1"]);
        let kind = FieldType::Duration;
        assert_eq!(record_input(&mut st, "r1", &kind, &[], "d", &change_text("1:30:0")), Typed::Queued);
        assert_eq!(st.pending.get(&("r1".to_string(), "d".to_string())), Some(&CellValue::Duration(Some(5400.0))));
        assert!(st.flush_scheduled, "敲完一段就该等防抖收尾");
        // 敲出中间态：待写值不动、原文留着，宿主才不会把输入框重画成别的样子
        assert_eq!(record_input(&mut st, "r1", &kind, &[], "d", &change_text("1:30:")), Typed::Hinted(DURATION_HINT));
        assert_eq!(st.drafts.get(&("r1".to_string(), "d".to_string())).map(String::as_str), Some("1:30:"));
        assert_eq!(st.projected().rows[0].cell(&duration_field()).as_edit_text(), "1:30:");
    }

    #[test]
    fn dropdown_value_is_written_at_once_without_touching_drafts() {
        let mut st = state_with(&["r1"]);
        let options = vec!["进行中".to_string()];
        let picked = record_input(&mut st, "r1", &FieldType::SingleSelect, &options, "s", &change(EventValue::Number(0.0)));
        assert_eq!(picked, Typed::Discrete(CellValue::Choice(Some("进行中".into()))));
        assert!(st.pending.is_empty() && st.flush_epoch == 0, "点选不走防抖");
    }

    #[test]
    fn table_change_defers_only_while_a_flush_is_armed() {
        let key = "任务.atb";
        let mut st = state_with(&["r1"]);
        st.pending.insert(("r1".to_string(), "f".to_string()), CellValue::Text("正在打".to_string()));
        st.flush_scheduled = true;
        assert_eq!(note_table_change(&mut st, Some(key)), TableChange::Defer, "打字期间不重读，远端值不盖掉本机输入");
        assert!(st.reload_requested, "重读要补上，不能丢掉这次变更");
        // 写失败留下的待值后面没有任务在等：不能让它把重读一直挡住
        st.flush_scheduled = false;
        assert_eq!(note_table_change(&mut st, Some(key)), TableChange::Reload);
    }

    #[test]
    fn switching_table_discards_values_that_belong_to_the_old_one() {
        let mut st = state_with(&["r1"]);
        st.pending.insert(("r1".to_string(), "f".to_string()), CellValue::Text("旧表的输入".to_string()));
        st.drafts.insert(("r1".to_string(), "d".to_string()), "1:".to_string());
        st.flush_scheduled = true;
        assert_eq!(note_table_change(&mut st, Some("另一张.atb")), TableChange::Switched { dropped: true });
        assert!(st.pending.is_empty() && st.drafts.is_empty(), "旧表的待写不得活过换表");
        assert!(!st.flush_scheduled && !st.reload_requested, "作废后不再有在途收尾");
    }

    #[tokio::test]
    async fn added_row_anchors_only_after_the_host_shows_it() {
        let mut st = state_with(&["r1", "r2"]);
        let provider = FakeProvider::new();
        *provider.wire.added.lock().unwrap() = Some("r3".to_string());
        // 重读回来还是原来两行：宿主没给出新行就不锚定，绝不退回首行
        *provider.wire.snapshot.lock().unwrap() = Some(snapshot_with(&["r1", "r2"]));
        let boxed: Provider = Box::new(provider);
        let failures = add_row(Some(&boxed), &mut st).await;
        assert_eq!(failures.len(), 1, "定位不到新行必须报出来：{failures:?}");
        assert_eq!(st.current_row_id().as_deref(), Some("r1"), "写值仍停在原来那行，不写到没把握的行");

        let provider = FakeProvider::new();
        *provider.wire.added.lock().unwrap() = Some("r3".to_string());
        *provider.wire.snapshot.lock().unwrap() = Some(snapshot_with(&["r1", "r2", "r3"]));
        let boxed: Provider = Box::new(provider);
        assert!(add_row(Some(&boxed), &mut st).await.is_empty());
        assert_eq!(st.current_row_id().as_deref(), Some("r3"), "新行进了快照才锚定过去");
    }

    #[test]
    fn projection_shows_not_yet_written_values() {
        let mut st = state_with(&["r1"]);
        st.pending.insert(("r1".to_string(), "f".to_string()), CellValue::Text("正在打".to_string()));
        assert_eq!(st.projected().rows[0].cell(&title_field()).as_edit_text(), "正在打");
    }

    #[test]
    fn adopt_keeps_the_row_and_falls_back_when_it_is_gone() {
        let mut st = state_with(&["r1", "r2", "r3"]);
        st.current_row = Some("r2".to_string());
        st.adopt(Some(snapshot_with(&["r1", "r2", "r3"])));
        assert_eq!(st.current_row.as_deref(), Some("r2"), "同一张表重读应停在同一行");
        // 远端删掉当前行：落到原序号位置
        st.adopt(Some(snapshot_with(&["r1", "r3"])));
        assert_eq!(st.row_index(), 1);
        assert_eq!(st.current_row_id().as_deref(), Some("r3"));
        // 换表回首行
        let mut other = snapshot_with(&["x1"]);
        other.path = "另一张.atb".to_string();
        st.adopt(Some(other));
        assert_eq!(st.row_index(), 0);
        // 表格关闭：进空态
        st.adopt(None);
        assert_eq!(st.current_row, None);
        assert_eq!(st.table_key(), None);
    }

    #[test]
    fn row_step_clamps_at_both_ends() {
        let mut st = state_with(&["r1", "r2"]);
        st.step_row(-1);
        assert_eq!(st.current_row.as_deref(), Some("r1"));
        st.step_row(1);
        assert_eq!(st.current_row.as_deref(), Some("r2"));
        st.step_row(1);
        assert_eq!(st.current_row.as_deref(), Some("r2"), "末行不能再往前");
    }

    /// 带图片格的状态：格子里除了路径还留一个 `display` 原键，写回必须带回去。
    fn image_state(paths: &[&str]) -> AppState {
        state_from(image_snapshot(paths))
    }

    /// 图片格快照：除图片列 `i` 还有一列文本 `t`，供「焦点指向非图片列」的用例。
    fn image_snapshot(paths: &[&str]) -> TableSnapshot {
        let fields = vec![
            Field { id: "i".into(), name: "封面".into(), kind: FieldType::Image, options: vec![] },
            Field { id: "t".into(), name: "标题".into(), kind: FieldType::Text, options: vec![] },
        ];
        let mut extra = Map::new();
        extra.insert("display".to_string(), json!("grid"));
        let mut cells = BTreeMap::new();
        let paths = paths.iter().map(|p| p.to_string()).collect();
        cells.insert("i".to_string(), CellValue::Images(ImageCell { paths, extra }));
        TableSnapshot { path: "任务/看板.atb".into(), fields, rows: vec![Row { id: "r1".into(), cells }] }
    }

    /// 主图停在 `path` 那一张、且正在看大图。
    fn preview_focus(row: &str, field: &str, path: &str) -> ImageFocus {
        ImageFocus {
            row: Some(row.to_string()),
            cursors: [(field.to_string(), path.to_string())].into(),
            preview: Some(Preview { field: field.to_string(), path: path.to_string() }),
        }
    }

    #[test]
    fn removing_one_image_keeps_the_rest_and_the_cell_keys() {
        let st = image_state(&["assets/a.png", "assets/b.png"]);
        let value = image_without(&st, "i", "assets/a.png").expect("应能移除第一张");
        let CellValue::Images(cell) = &value else { panic!("应还是图片格") };
        assert_eq!(cell.paths, ["assets/b.png"]);
        assert_eq!(cell.extra.get("display"), Some(&json!("grid")), "移除一张不该改掉显示模式");
        assert!(image_without(&st, "i", "assets/ghost.png").is_none(), "这一格不含那条引用就不该删掉任何一张");
    }

    #[test]
    fn removing_an_image_follows_the_reference_not_the_position() {
        // 移除请求带的是引用：同一格被并发改过、这张图的序号已变，落点仍是它自己
        let st = image_state(&["assets/a.png", "assets/b.png", "assets/c.png"]);
        let value = image_without(&st, "i", "assets/b.png").expect("按引用应能找到那一张");
        let CellValue::Images(cell) = &value else { panic!("应还是图片格") };
        assert_eq!(cell.paths, ["assets/a.png", "assets/c.png"], "只能删掉点名的那一张");
    }

    #[test]
    fn preview_steps_wrap_around_the_list() {
        let mut st = image_state(&["a.png", "b.png", "c.png"]);
        let row = st.current_row_id().unwrap();
        st.image.open(&row, "i", "a.png");
        step_image(&mut st, -1);
        assert_eq!(st.image.preview, Some(Preview { field: "i".into(), path: "c.png".into() }), "第一张往前应到最后一张");
        assert_eq!(st.image.cursors.get("i").map(String::as_str), Some("c.png"), "主图焦点要跟着预览走，退出预览后显示的就是这张");
        step_image(&mut st, 1);
        assert_eq!(st.image.preview, Some(Preview { field: "i".into(), path: "a.png".into() }));
        assert_eq!(st.image.cursors.get("i").map(String::as_str), Some("a.png"));
    }

    #[test]
    fn step_image_leaves_a_dead_preview_alone() {
        // 那张图已经不在格子里：什么都不动，交给下一次投影把预览收掉
        let mut st = image_state(&[]);
        let row = st.current_row_id().unwrap();
        st.image.open(&row, "i", "a.png");
        step_image(&mut st, 1);
        assert_eq!(st.image.preview, Some(Preview { field: "i".into(), path: "a.png".into() }));
    }

    #[test]
    fn shown_image_counts_the_projection_not_just_the_disk() {
        let mut st = image_state(&["a.png"]);
        assert_eq!(shown_image(&st, "i").as_deref(), Some("a.png"));
        // 待写里已排进第二张：界面显示的是投影那一份，焦点也按它找
        let next = CellValue::Images(ImageCell { paths: vec!["a.png".into(), "b.png".into()], ..ImageCell::default() });
        st.pending.insert(("r1".to_string(), "i".to_string()), next);
        st.image.focus("r1", "i", "b.png");
        assert_eq!(shown_image(&st, "i").as_deref(), Some("b.png"));
        // 待写把这一格清空的：没有可预览的一张
        let mut cleared = image_state(&["a.png"]);
        cleared.pending.insert(("r1".to_string(), "i".to_string()), CellValue::Images(ImageCell::default()));
        assert_eq!(shown_image(&cleared, "i"), None, "格子里没图就不该进预览");
    }

    #[test]
    fn switching_tables_drops_the_image_focus() {
        let mut st = image_state(&["a.png", "b.png"]);
        st.image.focus("r1", "i", "b.png");
        let mut other = snapshot_with(&["x1"]);
        other.path = "另一张.atb".to_string();
        st.adopt(Some(other));
        assert!(st.image.cursors.is_empty() && st.image.row.is_none(), "旧表的图片焦点不该带过来");
    }

    #[test]
    fn prune_drops_focus_that_no_longer_points_at_an_image() {
        let snapshot = image_snapshot(&["assets/a.png", "assets/b.png"]);

        let mut image = preview_focus("r1", "i", "assets/b.png");
        prune_image(&mut image, &snapshot, Some("r1"));
        assert_eq!(image.preview, Some(Preview { field: "i".to_string(), path: "assets/b.png".to_string() }), "这一格还含那条引用时焦点应留着");

        // 那张被换掉了：指向它的焦点与预览都作废
        let shrunk = image_snapshot(&["assets/c.png"]);
        let mut image = preview_focus("r1", "i", "assets/b.png");
        prune_image(&mut image, &shrunk, Some("r1"));
        assert_eq!(image.preview, None, "预览目标没了就退出预览");
        assert!(!image.cursors.contains_key("i"), "找不到引用的焦点应删掉，界面回第一张");

        // 指向非图片列的焦点（字段刚改过类型）：一并清掉
        let mut image = preview_focus("r1", "t", "assets/a.png");
        prune_image(&mut image, &snapshot, Some("r1"));
        assert!(image.cursors.is_empty() && image.preview.is_none(), "非图片列的焦点该清");

        // 翻行：旧行的引用对不上新行，整批归零
        let mut image = preview_focus("r1", "i", "assets/b.png");
        prune_image(&mut image, &snapshot, Some("r9"));
        assert_eq!(image.row.as_deref(), Some("r9"));
        assert!(image.cursors.is_empty() && image.preview.is_none());
    }
}