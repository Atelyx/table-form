//! 表格数据接缝的宿主实现：把宿主表格能力面（快照读、逐格写、增删行、选中态、图片导入）
//! 映射成 [`TableProvider`]，让 UI 与写值路径不认识宿主细节。
//!
//! 写值只走 `table.updateCell` 逐格改：宿主在同一出口合并落盘并广播协作补丁，插件不碰 `.atb` 磁盘。
//! 图片导入落 `.atelyx/temp/tables/<表格 id>/`，与宿主自己贴图的附件目录同址，删表时连带清理。

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use atelyx_plugin_sdk::protocol::method::{
    TableField, TableSelection, TableSnapshot as WireSnapshot, TableRow,
};

use crate::model::{image_ext_of, CellValue, Field, FieldType, Row, TableProvider, TableSnapshot};
use atelyx_plugin_sdk::Context;

pub struct HostTableProvider {
    ctx: Context,
}

impl HostTableProvider {
    pub fn new(ctx: &Context) -> Self {
        Self { ctx: ctx.clone() }
    }
}

#[async_trait::async_trait]
impl TableProvider for HostTableProvider {
    async fn snapshot(&self) -> Result<Option<TableSnapshot>, String> {
        let wire = self.ctx.table_snapshot().await.map_err(|e| e.to_string())?;
        to_snapshot(wire)
    }

    async fn update_cell(&self, row_id: &str, field_id: &str, value: &CellValue) -> Result<(), String> {
        let wire = value
            .to_wire()
            .ok_or_else(|| "该单元格的值形态不符，已停止写回".to_string())?;
        self.ctx
            .table_update_cell(row_id, field_id, wire)
            .await
            .map_err(|e| e.to_string())
    }

    async fn add_row(&self) -> Result<String, String> {
        self.ctx.table_add_row().await.map_err(|e| e.to_string())
    }

    async fn remove_row(&self, row_id: &str) -> Result<(), String> {
        self.ctx.table_remove_row(row_id).await.map_err(|e| e.to_string())
    }

    async fn select_row(&self, row_id: &str) -> Result<(), String> {
        self.ctx.table_select_row(row_id).await.map_err(|e| e.to_string())
    }

    async fn import_image(&self, table_id: &str, source: &str) -> Result<String, String> {
        let ext = image_ext_of(source).ok_or_else(|| "只支持 png / jpg / webp / gif 图片".to_string())?;
        // 文件名唯一即路径唯一：纳秒 + 进程内计数，不依赖外部随机源
        let path = format!(".atelyx/temp/tables/{table_id}/{}.{ext}", attachment_name());
        self.ctx.vault_import_file(&path, source).await.map_err(|e| e.to_string())?;
        Ok(path)
    }
}

/// 附件目录里的新文件名：`img-<十六进制纳秒>-<进程内序号>`，纳秒区分时刻、同刻内序号递增，不会重名。
fn attachment_name() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("img-{nanos:x}-{}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// 宿主快照 → 插件快照：未打开表格回 `None`，形态不足以求出是哪张表时报错（读错表比读不到更危险）。
fn to_snapshot(wire: WireSnapshot) -> Result<Option<TableSnapshot>, String> {
    if !wire.open {
        return Ok(None);
    }
    // 表格路径同时是表单配置的按键；缺路径就无法定位是哪张表，宁可不读也不读错表
    let path = wire.path.ok_or_else(|| "宿主快照未给出表格路径".to_string())?;
    let fields = fields_of(&wire.fields);
    let rows = rows_of(&wire.rows, &fields);
    Ok(Some(TableSnapshot { path, id: wire.id, selected_row: selection_row(&wire.selection), fields, rows }))
}

/// 快照里的用户选区只收整行（表单按行工作）；格 / 区 / 列 / 全选都不引导翻行。
fn selection_row(selection: &Option<TableSelection>) -> Option<String> {
    match selection {
        Some(TableSelection::Row { row_id }) => Some(row_id.clone()),
        _ => None,
    }
}

fn fields_of(wire: &[TableField]) -> Vec<Field> {
    wire.iter()
        .map(|field| Field {
            id: field.id.clone(),
            name: field.name.clone(),
            kind: FieldType::from_wire(&field.field_type),
            options: field.options.clone(),
        })
        .collect()
}

/// 按字段取值：行里指向已删字段的残留键不收（界面不显示，也永不被改写）。
fn rows_of(wire: &[TableRow], fields: &[Field]) -> Vec<Row> {
    wire.iter()
        .map(|row| Row {
            id: row.id.clone(),
            cells: fields
                .iter()
                .map(|field| (field.id.clone(), CellValue::from_wire(&field.kind, row.values.get(&field.id))))
                .collect(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use atelyx_plugin_sdk::protocol::method::TableSnapshot as WireSnapshot;
    use serde_json::json;

    fn wire() -> WireSnapshot {
        serde_json::from_value(json!({
            "open": true,
            "path": "任务/看板.atb",
            "fields": [
                { "id": "f1", "name": "标题", "type": "text" },
                { "id": "f2", "name": "状态", "type": "singleSelect", "options": ["未开始", "进行中"] },
                { "id": "f3", "name": "时长", "type": "duration" },
                { "id": "f4", "name": "封面", "type": "image" }
            ],
            "rows": [
                { "id": "r1", "values": { "f1": "登录页", "f2": "进行中", "f3": 90,
                    "f4": { "images": [".atelyx/temp/tables/t/img-1.png"], "display": "grid" },
                    "f9": { "value": "已删字段的残留值" } } }
            ]
        }))
        .unwrap()
    }

    #[test]
    fn wire_snapshot_maps_to_model() {
        let snapshot = to_snapshot(wire()).unwrap().expect("打开的表格应有快照");
        assert_eq!(snapshot.display_name(), "看板");

        let row = &snapshot.rows[0];
        let title = snapshot.fields[0].clone();
        assert_eq!(row.cell(&title), CellValue::Text("登录页".into()));
        assert_eq!(row.cell(&snapshot.fields[2]), CellValue::Duration(Some(90.0)));
        assert_eq!(row.cell(&snapshot.fields[2]).as_edit_text(), "1:30");
        // 残留键不在字段表里，因此不进快照、也不会被改写
        assert!(!row.cells.contains_key("f9"));
        let CellValue::Images(image) = row.cell(&snapshot.fields[3]) else {
            panic!("图片格应解成 ImageCell");
        };
        assert_eq!(image.extra.get("display"), Some(&json!("grid")));
    }

    #[test]
    fn closed_or_pathless_snapshot_is_rejected() {
        let closed: WireSnapshot = serde_json::from_value(json!({ "open": false })).unwrap();
        assert!(to_snapshot(closed).unwrap().is_none(), "未打开表格不该有快照");
        // 开着却没给路径：不知道是哪张表，报错而不是读成一张空表
        let pathless: WireSnapshot = serde_json::from_value(json!({ "open": true })).unwrap();
        assert_eq!(to_snapshot(pathless).unwrap_err(), "宿主快照未给出表格路径");
    }

    #[test]
    fn only_whole_row_selection_steers_the_form() {
        let mut wire = wire();
        wire.id = Some("t-1".into());
        wire.selection = Some(TableSelection::Row { row_id: "r1".into() });
        let snapshot = to_snapshot(wire.clone()).unwrap().expect("打开的表格应有快照");
        assert_eq!(snapshot.id.as_deref(), Some("t-1"));
        assert_eq!(snapshot.selected_row.as_deref(), Some("r1"));
        // 格选区不引导翻行：表单按行工作
        wire.selection = Some(TableSelection::Cell { row_id: "r1".into(), field_id: "f1".into() });
        let snapshot = to_snapshot(wire).unwrap().expect("打开的表格应有快照");
        assert_eq!(snapshot.selected_row, None);
    }

    #[test]
    fn image_import_rejects_non_image_files_before_touching_the_vault() {
        assert_eq!(image_ext_of("照片.PNG"), Some("png"));
        assert_eq!(image_ext_of("压缩包.zip"), None);
    }

    #[test]
    fn unwritable_cell_value_is_blocked_before_host_call() {
        // Foreign 值没有可写形态，接缝必须在调用宿主之前就停住
        assert_eq!(CellValue::Foreign(json!({"a": 1})).to_wire(), None);
        assert_eq!(CellValue::Text("原文".into()).to_wire(), Some(json!("原文")));
    }
}
