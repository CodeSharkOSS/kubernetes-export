//! Interactive map of the namespace: pan (drag), zoom (scroll), hover to
//! trace connections, click to select.

use crate::graph::{self, Graph, Layout, NODE_H, NODE_W};
use crate::k8s::Item;
use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, Vec2};
use std::collections::{BTreeMap, BTreeSet, HashSet};

pub struct MapState {
    graph: Graph,
    layout: Layout,
    pan: Vec2,
    zoom: f32,
    fit: bool,
    pub selected: Option<usize>,
    search: String,
    hidden: BTreeSet<String>,
}

pub fn kind_color(kind: &str) -> Color32 {
    match kind {
        "Ingress" => Color32::from_rgb(170, 120, 230),
        "Service" => Color32::from_rgb(80, 150, 240),
        "Deployment" | "StatefulSet" | "DaemonSet" | "Job" | "CronJob" => Color32::from_rgb(80, 190, 120),
        "HorizontalPodAutoscaler" => Color32::from_rgb(60, 190, 190),
        "PodDisruptionBudget" => Color32::from_rgb(90, 170, 170),
        "NetworkPolicy" => Color32::from_rgb(230, 140, 70),
        "ConfigMap" => Color32::from_rgb(220, 190, 70),
        "PersistentVolumeClaim" => Color32::from_rgb(190, 140, 100),
        "Secret" => Color32::from_rgb(150, 150, 160),
        "ServiceAccount" => Color32::from_rgb(230, 120, 170),
        _ => Color32::from_rgb(210, 100, 100), // RBAC
    }
}

impl MapState {
    pub fn new(items: &[Item]) -> Self {
        let graph = graph::build(items);
        let layout = graph::layout(&graph);
        Self {
            graph,
            layout,
            pan: Vec2::ZERO,
            zoom: 1.0,
            fit: true,
            selected: None,
            search: String::new(),
            hidden: BTreeSet::new(),
        }
    }

    fn visible(&self, n: usize) -> bool {
        !self.hidden.contains(&self.graph.nodes[n].kind)
    }

    fn neighbours(&self, n: usize) -> Vec<(usize, &'static str, bool)> {
        // (other node, label, outgoing?)
        self.graph
            .edges
            .iter()
            .filter_map(|e| {
                if e.from == n {
                    Some((e.to, e.label, true))
                } else if e.to == n {
                    Some((e.from, e.label, false))
                } else {
                    None
                }
            })
            .collect()
    }

    // ----------------------------------------------------------- sidebar
    /// Returns an item index the user wants to open in the editor.
    pub fn sidebar(&mut self, ui: &mut egui::Ui) -> Option<usize> {
        let mut open = None;
        ui.add_space(4.0);
        ui.heading("🗺 Namespace map");
        ui.add(egui::TextEdit::singleline(&mut self.search).hint_text("🔍 Highlight by name").desired_width(f32::INFINITY));
        ui.separator();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            // Selected node
            if let Some(sel) = self.selected.filter(|&s| s < self.graph.nodes.len()) {
                let (kind, name, ghost, issue, item) = {
                    let n = &self.graph.nodes[sel];
                    (n.kind.clone(), n.name.clone(), n.ghost, n.issue.clone(), n.item)
                };
                ui.label(egui::RichText::new(&kind).color(kind_color(&kind)).strong());
                ui.heading(&name);
                if ghost && kind == "Secret" {
                    ui.label(egui::RichText::new("🔒 Referenced only. Secrets are never read or exported.").weak().small());
                }
                if let Some(i) = issue {
                    ui.colored_label(Color32::from_rgb(230, 170, 60), format!("⚠ {i}"));
                }
                let nb = self.neighbours(sel);
                let mut jump = None;
                for (title, out) in [("Connects to", true), ("Used by", false)] {
                    let list: Vec<_> = nb.iter().filter(|(_, _, o)| *o == out).collect();
                    if list.is_empty() {
                        continue;
                    }
                    ui.add_space(6.0);
                    ui.strong(title);
                    for (o, label, _) in list {
                        let n = &self.graph.nodes[*o];
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new("●").color(kind_color(&n.kind)));
                            if ui.link(format!("{} / {}", n.kind, n.name)).clicked() {
                                jump = Some(*o);
                            }
                            ui.weak(*label);
                        });
                    }
                }
                if let Some(j) = jump {
                    self.selected = Some(j);
                }
                if let Some(i) = item {
                    ui.add_space(8.0);
                    if ui.button("Open in editor →").clicked() {
                        open = Some(i);
                    }
                }
                ui.separator();
            } else {
                ui.label(egui::RichText::new("Hover a box to trace its connections. Click to pin it. Drag to pan, scroll to zoom.").weak().small());
                ui.separator();
            }

            // Problems
            let problems: Vec<usize> =
                (0..self.graph.nodes.len()).filter(|&i| self.graph.nodes[i].issue.is_some()).collect();
            if !problems.is_empty() {
                ui.strong(egui::RichText::new(format!("⚠ {} problem(s)", problems.len())).color(Color32::from_rgb(230, 170, 60)));
                for p in problems {
                    let n = &self.graph.nodes[p];
                    if ui.link(format!("{} / {}", n.kind, n.name)).on_hover_text(n.issue.clone().unwrap_or_default()).clicked() {
                        self.selected = Some(p);
                    }
                }
                ui.separator();
            }

            // Legend / filter
            ui.strong("Show");
            let mut counts: BTreeMap<String, usize> = BTreeMap::new();
            for n in &self.graph.nodes {
                *counts.entry(n.kind.clone()).or_default() += 1;
            }
            for (kind, n) in counts {
                let mut on = !self.hidden.contains(&kind);
                ui.horizontal(|ui| {
                    ui.checkbox(&mut on, "");
                    ui.label(egui::RichText::new("●").color(kind_color(&kind)));
                    ui.label(format!("{kind} ({n})"));
                });
                if on {
                    self.hidden.remove(&kind);
                } else {
                    self.hidden.insert(kind);
                }
            }
            ui.add_space(6.0);
            ui.label(egui::RichText::new(format!("{} resources · {} connections", self.graph.nodes.len(), self.graph.edges.len())).weak().small());
            if ui.button("⤢ Fit to window").clicked() {
                self.fit = true;
            }
        });
        open
    }

    // ------------------------------------------------------------ canvas
    pub fn canvas(&mut self, ui: &mut egui::Ui) {
        let (resp, painter) = ui.allocate_painter(ui.available_size(), Sense::click_and_drag());
        let rect = resp.rect;
        if self.graph.nodes.is_empty() {
            painter.text(rect.center(), Align2::CENTER_CENTER, "Nothing to map", FontId::proportional(16.0), Color32::GRAY);
            return;
        }
        self.pan_zoom(ui, &resp, rect);
        let z = self.zoom;
        let to_screen = |w: [f32; 2]| rect.min + self.pan + Vec2::new(w[0], w[1]) * z;
        let node_rect = |n: usize| Rect::from_min_size(to_screen(self.layout.pos[n]), Vec2::new(NODE_W, NODE_H) * z);

        // hover
        let hovered = resp.hover_pos().and_then(|p| {
            (0..self.graph.nodes.len()).filter(|&n| self.visible(n)).find(|&n| node_rect(n).contains(p))
        });
        if resp.clicked() {
            self.selected = hovered;
        }
        let focus = hovered.or(self.selected);
        let mut related: HashSet<usize> = HashSet::new();
        if let Some(f) = focus {
            related.insert(f);
            related.extend(self.neighbours(f).into_iter().map(|(o, _, _)| o));
        }
        let q = self.search.to_lowercase();
        let matches = |n: usize| q.is_empty() || self.graph.nodes[n].name.to_lowercase().contains(&q);

        let painter = painter.with_clip_rect(rect);
        // column headers
        for (x, title) in &self.layout.cols {
            let p = to_screen([*x + NODE_W / 2.0, 6.0]);
            painter.text(p, Align2::CENTER_TOP, title, FontId::proportional((12.0 * z).clamp(9.0, 16.0)), Color32::from_gray(140));
        }

        // edges
        for e in &self.graph.edges {
            if !self.visible(e.from) || !self.visible(e.to) {
                continue;
            }
            let active = focus.is_some_and(|f| e.from == f || e.to == f);
            let muted = focus.is_some() || !q.is_empty();
            self.draw_edge(&painter, e, (node_rect(e.from), node_rect(e.to)), z, active, muted);
        }

        // nodes
        for n in 0..self.graph.nodes.len() {
            if !self.visible(n) {
                continue;
            }
            let r = node_rect(n);
            if !r.intersects(rect) {
                continue;
            }
            let dim = (focus.is_some() && !related.contains(&n)) || !matches(n);
            let hot = hovered == Some(n) || self.selected == Some(n);
            self.draw_node(&painter, n, r, z, &NodeLook { dim, hot, search_hit: !q.is_empty() && matches(n) });
        }
        if let Some(issue) = hovered.and_then(|h| self.graph.nodes[h].issue.as_ref()) {
            resp.on_hover_text(format!("⚠ {issue}"));
        }
    }

    fn pan_zoom(&mut self, ui: &egui::Ui, resp: &egui::Response, rect: Rect) {
    if self.fit {
        let [w, h] = self.layout.size;
        let z = ((rect.width() - 60.0) / w).min((rect.height() - 60.0) / h).clamp(0.25, 1.2);
        self.zoom = z;
        self.pan = Vec2::new((rect.width() - w * z) / 2.0, (rect.height() - h * z) / 2.0);
        self.fit = false;
    }
    // pan / zoom
    if resp.dragged() {
        self.pan += resp.drag_delta();
    }
    if let Some(p) = resp.hover_pos() {
        let (scroll, zd) = ui.input(|i| (i.smooth_scroll_delta.y, i.zoom_delta()));
        let factor = zd * (1.0 + scroll * 0.002);
        if (factor - 1.0).abs() > f32::EPSILON {
            let z2 = (self.zoom * factor).clamp(0.2, 2.5);
            let rel = p - rect.min;
            let world = (rel - self.pan) / self.zoom;
            self.pan = rel - world * z2;
            self.zoom = z2;
        }
    }
    }

    fn draw_edge(&self, painter: &egui::Painter, e: &graph::Edge, (a, b): (Rect, Rect), z: f32, active: bool, muted: bool) {
        let (ca, cb) = (self.layout.col_of[e.from], self.layout.col_of[e.to]);
        let (pa, da, pb, db) = if ca < cb {
            (a.right_center(), 1.0, b.left_center(), -1.0)
        } else if ca > cb {
            (a.left_center(), -1.0, b.right_center(), 1.0)
        } else {
            (a.right_center(), 1.0, b.right_center(), 1.0)
        };
        let off = (((pb.x - pa.x).abs() * 0.5).max(50.0 * z)).min(220.0 * z);
        let (c1, c2) = (pa + Vec2::new(da * off, 0.0), pb + Vec2::new(db * off, 0.0));
        let color = if active {
            Color32::from_rgb(110, 175, 255)
        } else if muted {
            Color32::from_white_alpha(14)
        } else {
            Color32::from_white_alpha(60)
        };
        let stroke = Stroke::new(if active { 2.2 } else { 1.2 }, color);
        painter.add(egui::epaint::CubicBezierShape::from_points_stroke([pa, c1, c2, pb], false, Color32::TRANSPARENT, stroke));
        // arrow head
        let d = (pb - c2).normalized();
        let perp = Vec2::new(-d.y, d.x);
        let tip = pb;
        painter.add(egui::Shape::convex_polygon(
            vec![tip, tip - d * 9.0 * z + perp * 4.5 * z, tip - d * 9.0 * z - perp * 4.5 * z],
            color,
            Stroke::NONE,
        ));
        if active {
            let t = 0.5f32;
            let mt = 1.0 - t;
            let mid = pa.to_vec2() * mt.powi(3) + c1.to_vec2() * 3.0 * mt * mt * t + c2.to_vec2() * 3.0 * mt * t * t + pb.to_vec2() * t.powi(3);
            let mid = Pos2::new(mid.x, mid.y);
            let galley = painter.layout_no_wrap(e.label.to_string(), FontId::proportional(11.0), Color32::WHITE);
            let r = Rect::from_center_size(mid, galley.size() + Vec2::new(8.0, 4.0));
            painter.rect_filled(r, 4.0, Color32::from_rgb(40, 70, 120));
            painter.galley(r.center() - galley.size() / 2.0, galley, Color32::WHITE);
        }
    }

    fn draw_node(&self, painter: &egui::Painter, n: usize, r: Rect, z: f32, look: &NodeLook) {
        let &NodeLook { dim, hot, search_hit } = look;
        let node = &self.graph.nodes[n];
        let a = if dim { 0.25 } else { 1.0 };
        let col = kind_color(&node.kind);
        let (fill, stroke_col) = if node.ghost && node.issue.is_some() {
            (Color32::from_rgb(70, 30, 30), Color32::from_rgb(230, 90, 90))
        } else if node.ghost {
            (Color32::from_gray(38), Color32::from_gray(110))
        } else {
            (col.gamma_multiply(0.22), col)
        };
        painter.rect(
            r,
            6.0 * z,
            fill.gamma_multiply(a),
            Stroke::new(if hot { 2.5 } else { 1.2 }, if hot { Color32::WHITE } else { stroke_col.gamma_multiply(a) }),
            egui::StrokeKind::Inside,
        );
        if search_hit {
            painter.rect_stroke(r.expand(3.0), 8.0 * z, Stroke::new(2.0, Color32::from_rgb(255, 220, 80)), egui::StrokeKind::Outside);
        }
        let prefix = if node.ghost && node.kind == "Secret" { "🔒 " } else if node.ghost { "? " } else { "" };
        let mut name = format!("{prefix}{}", node.name);
        let max_chars = ((NODE_W - 30.0) / 7.0) as usize;
        if name.chars().count() > max_chars {
            name = name.chars().take(max_chars - 1).collect::<String>() + "…";
        }
        let fs = 12.5 * z;
        if fs >= 5.0 {
            painter.text(
                r.min + Vec2::new(8.0 * z, 4.0 * z),
                Align2::LEFT_TOP,
                &node.kind,
                FontId::proportional(fs * 0.75),
                col.gamma_multiply(a),
            );
            painter.text(
                r.min + Vec2::new(8.0 * z, 15.0 * z),
                Align2::LEFT_TOP,
                name,
                FontId::proportional(fs),
                Color32::WHITE.gamma_multiply(a),
            );
        }
        if node.issue.is_some() {
            painter.text(r.right_top() + Vec2::new(-6.0 * z, 3.0 * z), Align2::RIGHT_TOP, "⚠", FontId::proportional(13.0 * z), Color32::from_rgb(230, 170, 60).gamma_multiply(a));
        }
    }
}


/// How a node should be drawn this frame.
struct NodeLook {
    dim: bool,
    hot: bool,
    search_hit: bool,
}