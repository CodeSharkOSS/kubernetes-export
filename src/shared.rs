//! The "Find & apply to all" tree and the Shared values panel.

use crate::common;
use crate::k8s::Item;
use crate::widgets::{ACCENT, BAD, OK, WARN, badge};
use eframe::egui;

/// Remembers (per session) whether tree picks should use wildcards for containers.
pub fn wildcard_pick(ui: &egui::Ui) -> bool {
    ui.data(|d| d.get_temp::<bool>(egui::Id::NULL.with("tree_wildcard"))).unwrap_or(true)
}

/// True if `path` is on the way to the pod spec (so the tree opens there by default).
pub fn on_pod_path(kind: &str, path: &[common::Seg]) -> bool {
    let Some(p) = common::pod_path(kind) else { return false };
    let want: Vec<&str> = p[1..].split('/').collect();
    path.len() <= want.len()
        && path.iter().zip(&want).all(|(s, w)| matches!(s, common::Seg::Key(k) if k == w))
}

pub struct TreeCx<'a> {
    pub kind: &'a str,
    pub shared: &'a std::collections::HashSet<String>,
    pub wildcard: bool,
    /// (rule path, YAML value) chosen this frame.
    pub pick: Option<(String, String)>,
}

/// Recursive tree of a resource. Hovering a row reveals "Apply to all";
/// fields that already have a shared value show a ⚑ badge instead.
pub fn tree_node(
    ui: &mut egui::Ui,
    v: &serde_json::Value,
    label: &str,
    exact: &mut Vec<common::Seg>,
    general: &mut Vec<common::Seg>,
    parent_key: &str,
    cx: &mut TreeCx,
) {
    use common::Seg;
    use serde_json::Value;

    // Per-node affordance: badge if shared, else a button while the row is hovered.
    let action = |ui: &mut egui::Ui, exact: &[Seg], general: &[Seg], cx: &mut TreeCx| {
        let pe = common::to_path(&common::compress(cx.kind, exact));
        let pg = common::to_path(&common::compress(cx.kind, general));
        if cx.shared.contains(&pe) || cx.shared.contains(&pg) {
            badge(ui, "⚑ same everywhere", ACCENT, "A shared value is set for this field (see right panel)");
        } else if ui.rect_contains_pointer(ui.max_rect())
            && ui
                .small_button("⚑ Apply to all")
                .on_hover_text("Use this value for this field in every resource that has it")
                .clicked()
        {
            let path = if cx.wildcard { pg } else { pe };
            let value = serde_yaml_ng::to_string(v).unwrap_or_default().trim_end().to_string();
            cx.pick = Some((path, value));
        }
    };

    let children: Vec<(String, Seg, Seg, &Value)> = match v {
        Value::Object(m) => m.iter().map(|(k, c)| (k.clone(), Seg::Key(k.clone()), Seg::Key(k.clone()), c)).collect(),
        Value::Array(a) => a
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let name = c.get("name").and_then(Value::as_str);
                let ex = match name {
                    Some(n) => Seg::Match("name".into(), n.into()),
                    None => Seg::Idx(i),
                };
                let ge = if matches!(parent_key, "containers" | "initContainers") { Seg::Any } else { ex.clone() };
                (name.map_or(format!("[{i}]"), str::to_string), ex, ge, c)
            })
            .collect(),
        _ => {
            ui.horizontal(|ui| {
                let val = match v {
                    Value::String(s) => s.clone(),
                    o => o.to_string(),
                };
                ui.label(egui::RichText::new(label).color(ACCENT).monospace());
                ui.monospace(val);
                action(ui, exact, general, cx);
            });
            return;
        }
    };
    let id = ui.make_persistent_id(("tree", common::to_path(exact)));
    egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, exact.len() < 2 || on_pod_path(cx.kind, exact))
        .show_header(ui, |ui| {
            ui.label(egui::RichText::new(label).color(ACCENT).monospace());
            if matches!(v, Value::Array(_)) {
                ui.weak(format!("[{}]", children.len()));
            }
            action(ui, exact, general, cx);
        })
        .body(|ui| {
            for (name, ex, ge, c) in children {
                exact.push(ex);
                general.push(ge);
                let key = if matches!(v, Value::Array(_)) { parent_key.to_string() } else { name.clone() };
                tree_node(ui, c, &name, exact, general, &key, cx);
                exact.pop();
                general.pop();
            }
        });
}

/// Cached "applies to N resources" counts, refreshed only when the rules or
/// any resource changed (cloning every resource per rule per frame is wasteful).
#[derive(Default)]
pub struct HitCache {
    key: u64,
    counts: Vec<Option<usize>>,
}

impl HitCache {
    fn refresh(&mut self, rules: &[common::Rule], items: &[Item]) {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        for r in rules {
            (r.enabled, &r.path, &r.value).hash(&mut h);
        }
        for it in items {
            (it.kind, &it.name, it.rev).hash(&mut h);
        }
        let key = h.finish();
        if key == self.key && self.counts.len() == rules.len() {
            return;
        }
        self.key = key;
        self.counts = rules
            .iter()
            .map(|r| {
                let (segs, val) = r.parsed().ok()?;
                Some(items.iter().filter(|it| common::apply_rule(&mut it.value.clone(), it.kind, &segs, &val) > 0).count())
            })
            .collect();
    }
}

pub fn common_panel(
    ui: &mut egui::Ui,
    rules: &mut Vec<common::Rule>,
    items: &[Item],
    hits: &mut HitCache,
    highlight: Option<usize>,
    browsing: bool,
    goto_tree: &mut bool,
) {
    hits.refresh(rules, items);
    ui.add_space(4.0);
    ui.heading("⚑ Shared values");
    ui.label(egui::RichText::new("The same value on every resource that has the field.").weak().small());
    ui.separator();
    let mut remove = None;
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        if rules.is_empty() {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label("Nothing shared yet.");
                ui.label(egui::RichText::new("e.g. give every workload the same node group or AWS_REGION.").weak().small());
                if browsing {
                    ui.label(egui::RichText::new("← Hover a field in the tree and press “⚑ Apply to all”.").color(ACCENT).small());
                } else if ui.button("Browse fields to share →").clicked() {
                    *goto_tree = true;
                }
            });
        }
        for (i, r) in rules.iter_mut().enumerate() {
            let stroke = if highlight == Some(i) {
                egui::Stroke::new(2.0, ACCENT)
            } else {
                ui.visuals().widgets.noninteractive.bg_stroke
            };
            egui::Frame::group(ui.style()).stroke(stroke).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.checkbox(&mut r.enabled, "").on_hover_text("Turn this shared value on/off");
                    ui.add(egui::TextEdit::singleline(&mut r.path).code_editor().desired_width((ui.available_width() - 28.0).max(60.0)));
                    if ui.small_button("🗑").on_hover_text("Remove").clicked() {
                        remove = Some(i);
                    }
                });
                ui.label(egui::RichText::new("set to").weak().small());
                ui.add(egui::TextEdit::multiline(&mut r.value).code_editor().desired_rows(1).desired_width(f32::INFINITY));
                match (r.parsed(), hits.counts.get(i).copied().flatten()) {
                    (Err(e), _) => {
                        ui.colored_label(BAD, e);
                    }
                    (Ok(_), n) => {
                        let n = n.unwrap_or(0);
                        ui.label(
                            egui::RichText::new(if n == 0 {
                                "matches no resources".to_string()
                            } else {
                                format!("→ applied to {n} resources")
                            })
                            .small()
                            .color(if n == 0 { WARN } else { OK }),
                        );
                    }
                }
            });
        }
        if !rules.is_empty() {
            ui.horizontal(|ui| {
                if browsing {
                    ui.label(egui::RichText::new("Hover another field in the tree to share more.").weak().small());
                } else if ui.button("Browse more fields →").clicked() {
                    *goto_tree = true;
                }
                ui.menu_button("Matching ▾", |ui| {
                    let mut wc = wildcard_pick(ui);
                    if ui.radio_value(&mut wc, true, "All containers").changed()
                        | ui.radio_value(&mut wc, false, "Only the container I clicked").changed()
                    {
                        ui.data_mut(|d| d.insert_temp(egui::Id::NULL.with("tree_wildcard"), wc));
                    }
                });
            });
        }
    });
    if let Some(i) = remove {
        rules.remove(i);
    }
}

