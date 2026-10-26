//! 表单配置：每个表格各自的字段取舍、顺序、提示与单选渲染样式。
//!
//! 本模块只做纯数据与派生逻辑（不碰宿主）；读写经 `state.read`/`state.write` 在入口处完成，
//! 整份配置序列化为一个 JSON 存进插件自持数据，按表格稳定 id 分键（宿主未给 id 的表退回
//! 仓库相对路径），切表格即换一份配置。

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::model::{Field, TableSnapshot};

/// 全部表格的配置集合（插件自持数据的根结构）。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FormConfig {
    /// key = 表格稳定 id（改名 / 移动后不变）；宿主未给 id 的表退回仓库相对路径。
    pub forms: BTreeMap<String, TableFormConfig>,
}

/// 单张表的表单配置。缺字段按默认处理（全显示、原字段序、无提示、下拉样式）。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TableFormConfig {
    /// 展示顺序（字段 id）；未列出的字段按表格原序补在末尾。
    pub order: Vec<String>,
    /// 关闭（不进表单）的字段 id。设置面板里仍可见可开关。
    pub hidden: BTreeSet<String>,
    /// 字段提示文案。
    pub help: BTreeMap<String, String>,
    /// 以「单选按钮」而非下拉渲染的 SingleSelect 字段 id。
    pub radio: BTreeSet<String>,
}

impl FormConfig {
    pub fn table(&self, path: &str) -> TableFormConfig {
        self.forms.get(path).cloned().unwrap_or_default()
    }

    /// 取（不存在则按默认建）某表的配置可变引用，供设置面板就地修改。
    pub fn table_mut(&mut self, path: &str) -> &mut TableFormConfig {
        self.forms.entry(path.to_string()).or_default()
    }
}

impl TableFormConfig {
    /// 按 `order` 排好、补齐未列字段后的全字段序列（供设置面板遍历，含隐藏字段）。
    pub fn ordered_fields<'a>(&self, snapshot: &'a TableSnapshot) -> Vec<&'a Field> {
        let mut picked: Vec<&Field> = Vec::with_capacity(snapshot.fields.len());
        for id in &self.order {
            if let Some(f) = snapshot.fields.iter().find(|f| &f.id == id) {
                if !picked.iter().any(|p| p.id == f.id) {
                    picked.push(f);
                }
            }
        }
        for f in &snapshot.fields {
            if !picked.iter().any(|p| p.id == f.id) {
                picked.push(f);
            }
        }
        picked
    }

    /// 表单实际渲染的字段：按顺序遍历、剔除关闭项。
    pub fn visible_fields<'a>(&self, snapshot: &'a TableSnapshot) -> Vec<&'a Field> {
        self.ordered_fields(snapshot).into_iter().filter(|f| !self.hidden.contains(&f.id)).collect()
    }

    pub fn is_hidden(&self, field_id: &str) -> bool {
        self.hidden.contains(field_id)
    }

    pub fn toggle_hidden(&mut self, field_id: &str, show: bool) {
        if show {
            self.hidden.remove(field_id);
        } else {
            self.hidden.insert(field_id.to_string());
        }
    }

    pub fn help_text(&self, field_id: &str) -> Option<&str> {
        self.help.get(field_id).map(|s| s.as_str()).filter(|s| !s.is_empty())
    }

    pub fn set_help(&mut self, field_id: &str, text: &str) {
        if text.is_empty() {
            self.help.remove(field_id);
        } else {
            self.help.insert(field_id.to_string(), text.to_string());
        }
    }

    pub fn is_radio(&self, field_id: &str) -> bool {
        self.radio.contains(field_id)
    }

    pub fn set_radio(&mut self, field_id: &str, on: bool) {
        if on {
            self.radio.insert(field_id.to_string());
        } else {
            self.radio.remove(field_id);
        }
    }

    /// 交换相邻字段展示位置；`field_id` 不在当前序列内则忽略。
    pub fn move_field(&mut self, snapshot: &TableSnapshot, field_id: &str, delta: isize) {
        let mut order: Vec<String> = self.ordered_fields(snapshot).into_iter().map(|f| f.id.clone()).collect();
        let Some(pos) = order.iter().position(|id| id == field_id) else { return };
        let len = order.len() as isize;
        let target = pos as isize + delta;
        if target < 0 || target >= len {
            return;
        }
        let from = order.remove(pos);
        order.insert(target as usize, from);
        self.order = order;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{FieldType, Row};
    use std::collections::BTreeMap as Map;

    fn snapshot() -> TableSnapshot {
        let fields = vec![
            Field { id: "a".into(), name: "A".into(), kind: FieldType::Text, options: vec![] },
            Field { id: "b".into(), name: "B".into(), kind: FieldType::Number, options: vec![] },
            Field { id: "c".into(), name: "C".into(), kind: FieldType::SingleSelect, options: vec!["x".into()] },
        ];
        TableSnapshot { path: "任务.atb".into(), fields, rows: vec![Row { id: "r1".into(), cells: Map::new() }], ..Default::default() }
    }

    #[test]
    fn order_appends_missing_and_visible_filters_hidden() {
        let s = snapshot();
        let mut cfg = TableFormConfig { order: vec!["c".into(), "a".into()], ..Default::default() };
        cfg.hidden.insert("a".into());
        let ordered: Vec<&str> = cfg.ordered_fields(&s).iter().map(|f| f.id.as_str()).collect();
        assert_eq!(ordered, ["c", "a", "b"]);
        let visible: Vec<&str> = cfg.visible_fields(&s).iter().map(|f| f.id.as_str()).collect();
        assert_eq!(visible, ["c", "b"]);
    }

    #[test]
    fn move_field_swaps_within_bounds() {
        let s = snapshot();
        let mut cfg = TableFormConfig::default();
        cfg.move_field(&s, "a", 1);
        assert_eq!(cfg.order, ["b", "a", "c"]);
        // 越界不动
        cfg.move_field(&s, "b", -1);
        assert_eq!(cfg.order, ["b", "a", "c"]);
    }

    #[test]
    fn serde_roundtrip_preserves_all_keys() {
        let mut cfg = TableFormConfig { order: vec!["b".into(), "a".into()], ..Default::default() };
        cfg.hidden.insert("a".into());
        cfg.radio.insert("c".into());
        cfg.set_help("a", "标题提示");
        let json = serde_json::to_string(&cfg).unwrap();
        let back: TableFormConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.order, cfg.order);
        assert!(back.is_hidden("a"));
        assert!(back.is_radio("c"));
        assert_eq!(back.help_text("a"), Some("标题提示"));
    }
}
