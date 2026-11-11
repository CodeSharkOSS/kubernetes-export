//! Application state and the main window layout.

use crate::widgets::{self, ACCENT, BAD, OK, WARN, badge, step_chip};
use crate::{common, deploy, editor, export, highlight, k8s, map, report, shared};
use eframe::egui;
use k8s::{Item, Msg};
use std::collections::BTreeSet;
use std::sync::mpsc::{Receiver, Sender, channel};

#[derive(PartialEq, Clone, Copy)]
enum Mode {
    Export,
    Map,
    Deploy,
}

#[derive(PartialEq, Clone, Copy)]
enum View {
    Edit,
    Tree,
    Output,
}

pub struct App {
    deploy: deploy::Deploy,
    cluster: String,
    hits: shared::HitCache,
    mode: Mode,
    map: Option<map::MapState>,
    view: View,
    rules: Vec<common::Rule>,
    highlight: Option<usize>,
    rt: tokio::runtime::Runtime,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,

    namespaces: Vec<String>,
    source_ns: String,
    target_ns: String,
    templatize: bool,
    out_dir: String,

    items: Vec<Item>,
    selected: Option<usize>,
    filter: String,
    notes: Vec<String>,
    status: String,
    status_ok: bool,
    exported: bool,
    report: Option<export::Report>,
    show_report: bool,
    busy: bool,
}

impl App {
    pub fn new() -> Self {
        let (tx, rx) = channel();
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        let app = Self {
            deploy: deploy::Deploy::default(),
            cluster: String::new(),
            hits: shared::HitCache::default(),
            mode: Mode::Export,
            map: None,
            view: View::Edit,
            rules: vec![],
            highlight: None,
            rt,
            tx,
            rx,
            namespaces: vec![],
            source_ns: String::new(),
            target_ns: String::new(),
            templatize: true,
            out_dir: "./export".into(),
            items: vec![],
            selected: None,
            filter: String::new(),
            notes: vec![],
            status: "Connecting to cluster using local kubeconfig...".into(),
            status_ok: true,
            exported: false,
            report: None,
            show_report: false,
            busy: true,
        };
        let tx = app.tx.clone();
        app.rt.spawn(async move {
            let _ = tx.send(Msg::Namespaces(k8s::list_namespaces().await));
        });
        app
    }

    fn load(&mut self) {
        self.busy = true;
        self.items.clear();
        self.map = None;
        self.selected = None;
        self.notes.clear();
        self.exported = false;
        self.status_ok = true;
        self.status = format!("Loading resources from '{}'...", self.source_ns);
        let (tx, ns) = (self.tx.clone(), self.source_ns.clone());
        self.rt.spawn(async move {
            let _ = tx.send(Msg::Items(k8s::load_items(&ns).await));
        });
    }

    fn poll(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Deploy(e) => {
                    self.deploy.on_event(e);
                    continue;
                }
                Msg::Prereq(r) => {
                    self.deploy.on_prereq(r);
                    continue;
                }
                Msg::Folder(which, path) => {
                    let p = path.display().to_string();
                    match which {
                        k8s::FolderFor::DeploySource => self.deploy.set_dir(p),
                        k8s::FolderFor::ExportDest => self.out_dir = p,
                    }
                    continue;
                }
                _ => {}
            }
            self.busy = false;
            self.status_ok = true;
            match msg {
                Msg::Namespaces(Ok((ctx, list))) => {
                    self.cluster = ctx.split(" (").next().unwrap_or_default().to_string();
                    self.status = format!("Connected to {}", ctx.trim_start_matches("https://").trim_end_matches('/'));
                    if self.source_ns.is_empty() {
                        self.source_ns = list.first().cloned().unwrap_or_default();
                    }
                    self.namespaces = list;
                }
                Msg::Items(Ok((items, notes))) => {
                    self.status = format!("Loaded {} resources from '{}'", items.len(), self.source_ns);
                    self.items = items;
                    self.notes = notes;
                }
                Msg::Deploy(_) | Msg::Prereq(_) | Msg::Folder(..) => {}
                Msg::Namespaces(Err(e)) | Msg::Items(Err(e)) => {
                    self.status_ok = false;
                    self.status = format!("Error: {e:#}");
                }
            }
        }
    }

    fn mode(&self) -> export::Mode {
        if self.templatize {
            export::Mode::Template
        } else {
            export::Mode::Replace(self.target_ns.trim().to_string())
        }
    }

    fn step(&self) -> usize {
        if self.items.is_empty() {
            1
        } else if self.exported {
            3
        } else {
            2
        }
    }

    /// Secret names referenced by the loaded resources -> how many resources use them.
    fn secret_refs(&self) -> Vec<(String, usize)> {
        let mut m: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::default();
        for it in self.items.iter().filter(|i| i.selected) {
            for w in &it.warnings {
                if let Some(n) = w.strip_prefix("References Secret '").and_then(|r| r.split('\'').next()) {
                    *m.entry(n.to_string()).or_default() += 1;
                }
            }
        }
        m.into_iter().collect()
    }

    fn can_export(&self) -> Result<(), &'static str> {
        if self.busy {
            Err("Please wait for loading to finish")
        } else if !self.items.iter().any(|i| i.selected) {
            Err("Tick at least one resource to export")
        } else if !self.templatize && !export::valid_ns(self.target_ns.trim()) {
            Err("Enter a valid namespace (lowercase letters, digits and '-')")
        } else if self.out_dir.trim().is_empty() {
            Err("Choose a folder to save to")
        } else {
            Ok(())
        }
    }

    fn do_export(&mut self) {
        match export::write_all(
            &self.items,
            &self.source_ns,
            &self.mode(),
            &self.rules,
            std::path::Path::new(&self.out_dir),
        ) {
            Ok(r) => {
                self.status = if r.failed() == 0 {
                    format!("Exported {} files to {}", r.ok(), r.dir.display())
                } else {
                    format!("Export finished with {} error(s) - see report", r.failed())
                };
                self.status_ok = r.failed() == 0;
                self.exported = r.failed() == 0;
                self.report = Some(r);
                self.show_report = true;
            }
            Err(e) => {
                self.status = format!("Export failed: {e:#}");
                self.status_ok = false;
            }
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll();
        if self.busy {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
        }
        let step = self.step();

        // ---------- top: progress + source ----------
        self.top_panel(ui, step);

        if self.mode == Mode::Deploy {
            let cx = deploy::Cx { rt: &self.rt, tx: &self.tx, cluster: &self.cluster };
            self.deploy.ui(ui, &cx);
            return;
        }

        if self.items.is_empty() {
            self.mode = Mode::Export;
            egui::CentralPanel::default().show(ui, |ui| self.welcome(ui));
            return;
        }

        if self.mode == Mode::Map {
            self.map_view(ui);
            return;
        }

        self.report_window(ui);
        self.bottom_panel(ui);
        self.shared_panel(ui);

        // ---------- left: resources ----------
        let avail = ui.available_width();
        egui::Panel::left("list")
            .resizable(true)
            .default_size((avail * 0.25).max(260.0))
            .size_range(200.0..=(avail * 0.6).max(200.0))
            .show(ui, |ui| self.resource_list(ui));

        self.inspector(ui);
    }
}

impl App {
    fn top_panel(&mut self, ui: &mut egui::Ui, step: usize) {
    egui::Panel::top("top").show(ui, |ui| {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let before = self.mode;
            ui.selectable_value(&mut self.mode, Mode::Export, "📦 Export");
            ui.add_enabled_ui(!self.items.is_empty(), |ui| {
                ui.selectable_value(&mut self.mode, Mode::Map, "🗺 Map")
                    .on_disabled_hover_text("Load a namespace first");
            });
            ui.selectable_value(&mut self.mode, Mode::Deploy, "🚀 Deploy")
                .on_hover_text("Deploy an exported folder into a cluster, one resource at a time");
            if self.mode != before {
                self.map = None; // rebuild so edits that change labels/refs show up
            }
            ui.separator();
            match self.mode {
                Mode::Export => {
                    step_chip(ui, 1, "Choose namespace", step);
                    ui.label("›");
                    step_chip(ui, 2, "Review & customize", step);
                    ui.label("›");
                    step_chip(ui, 3, "Export", step);
                }
                Mode::Map => {
                    ui.label(egui::RichText::new(format!("Map of '{}'", self.source_ns)).strong());
                }
                Mode::Deploy => {
                    ui.label(egui::RichText::new("Deploy an exported folder").strong());
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if self.busy {
                    ui.spinner();
                }
                ui.label(egui::RichText::new(&self.status).color(if self.status_ok {
                    ui.visuals().weak_text_color()
                } else {
                    BAD
                }));
            });
        });
        ui.add_space(2.0);
        if !self.items.is_empty() && self.mode != Mode::Deploy {
            ui.horizontal(|ui| {
                ui.label("Source namespace");
                egui::ComboBox::from_id_salt("ns").selected_text(&self.source_ns).show_ui(ui, |ui| {
                    for n in &self.namespaces {
                        ui.selectable_value(&mut self.source_ns, n.clone(), n);
                    }
                });
                if ui
                    .add_enabled(!self.busy, egui::Button::new("↻ Reload"))
                    .on_hover_text("Re-read this namespace from the cluster (discards your edits)")
                    .clicked()
                {
                    self.load();
                }
            });
        }
        ui.add_space(4.0);
    });
    }

    fn map_view(&mut self, ui: &mut egui::Ui) {
        if self.map.is_none() {
            self.map = Some(map::MapState::new(&self.items));
        }
        let mut open = None;
        egui::Panel::right("map_side").resizable(true).default_size(300.0).size_range(240.0..=420.0).show(ui, |ui| {
            open = self.map.as_mut().and_then(|m| m.sidebar(ui));
        });
        egui::CentralPanel::default().show(ui, |ui| {
            if let Some(m) = self.map.as_mut() {
                m.canvas(ui);
            }
        });
        if let Some(i) = open {
            self.mode = Mode::Export;
            self.selected = Some(i);
            self.view = View::Edit;
        }
    }

    fn report_window(&mut self, ui: &mut egui::Ui) {
    if let (true, Some(r)) = (self.show_report, &self.report) {
        let mut open = true;
        let refs = self.secret_refs();
        egui::Window::new("Export report")
            .open(&mut open)
            .default_width(760.0)
            .default_height(480.0)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ui.ctx(), |ui| report::report_view(ui, r, &refs));
        self.show_report = open;
    }
    }

    fn bottom_panel(&mut self, ui: &mut egui::Ui) {
    egui::Panel::bottom("bottom").show(ui, |ui| {
        ui.add_space(6.0);
        let n_sel = self.items.iter().filter(|i| i.selected).count();
        let n_edit = self.items.iter().filter(|i| i.selected && i.edited).count();
        let n_rules = self.rules.iter().filter(|r| r.enabled).count();
        ui.horizontal(|ui| {
            ui.strong("Deploy into");
            ui.radio_value(&mut self.templatize, true, "Any namespace (decide at deploy time)")
                .on_hover_text("Files use a __NAMESPACE__ placeholder; deploy.sh <namespace> fills it in.");
            ui.radio_value(&mut self.templatize, false, "This namespace:")
                .on_hover_text("The old namespace is rewritten to the one you type.");
            ui.add_enabled(
                !self.templatize,
                egui::TextEdit::singleline(&mut self.target_ns).hint_text("e.g. qa-copy").desired_width(160.0),
            );
        });
        ui.horizontal(|ui| {
            ui.strong("Save to");
            ui.add(egui::TextEdit::singleline(&mut self.out_dir).desired_width(260.0));
            if ui.button("📁 Browse…").on_hover_text("Pick the folder with a file browser").clicked() {
                widgets::browse_folder(&self.tx, ui.ctx(), k8s::FolderFor::ExportDest, &self.out_dir);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let ok = self.can_export();
                let btn = egui::Button::new(
                    egui::RichText::new(format!("Export {n_sel} resources")).strong().color(egui::Color32::WHITE),
                )
                .fill(if ok.is_ok() { egui::Color32::from_rgb(40, 110, 200) } else { egui::Color32::from_gray(70) })
                .min_size(egui::vec2(160.0, 30.0));
                let resp = ui.add_enabled(ok.is_ok(), btn);
                let resp = match ok {
                    Err(why) => resp.on_disabled_hover_text(why),
                    Ok(()) => resp,
                };
                if resp.clicked() {
                    self.do_export();
                }
                if self.report.is_some() && ui.button("📋 Last export report").clicked() {
                    self.show_report = true;
                }
                ui.weak(format!(
                    "{n_sel} of {} selected · {n_edit} edited · {n_rules} shared value(s)",
                    self.items.len()
                ));
            });
        });
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("🔒 Secrets are never read or exported").small().color(OK));
            if self.exported {
                let how = if self.templatize {
                    format!("Next: cd {} && ./deploy.sh <new-namespace>", self.out_dir)
                } else {
                    format!("Next: cd {} && ./deploy.sh", self.out_dir)
                };
                ui.label(egui::RichText::new(how).small().monospace().color(ACCENT));
            }
        });
        ui.add_space(4.0);
    });
    }

    fn shared_panel(&mut self, ui: &mut egui::Ui) {
    let mut goto_tree = false;
    let total_w = ui.available_width();
    egui::Panel::right("common")
        .resizable(true)
        .default_size(340.0_f32.min(total_w * 0.35))
        .size_range(260.0..=(total_w * 0.45).max(260.0))
        .show(ui, |ui| {
        let browsing = self.view == View::Tree && self.selected.is_some();
        shared::common_panel(ui, &mut self.rules, &self.items, &mut self.hits, self.highlight, browsing, &mut goto_tree);
    });
    if goto_tree {
        self.view = View::Tree;
        let is_workload = |i: &Item| common::pod_path(i.kind).is_some();
        // Land on a workload (where node selectors / env vars live) unless one is already open.
        let cur_ok = self.selected.and_then(|i| self.items.get(i)).is_some_and(is_workload);
        if !cur_ok {
            self.selected = self
                .items
                .iter()
                .position(|i| i.selected && is_workload(i))
                .or_else(|| self.items.iter().position(is_workload))
                .or(self.selected)
                .or(Some(0));
        }
        self.status = "Hover a field in the tree and press “⚑ Apply to all”".into();
        self.status_ok = true;
    }
    }

    fn inspector(&mut self, ui: &mut egui::Ui) {
    // ---------- center: inspector ----------
    let mode = self.mode();
    let mut new_rule: Option<(String, String)> = None;
    egui::CentralPanel::default().show(ui, |ui| {
        let Some(idx) = self.selected.filter(|&i| i < self.items.len()) else {
            self.overview(ui);
            return;
        };
        let item = &mut self.items[idx];
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(item.kind).color(ACCENT).strong());
            ui.heading(&item.name);
            if item.edited {
                badge(ui, "✎ edited", ACCENT, "You changed this resource");
            }
        });
        ui.label(egui::RichText::new(format!("→ {}", export::file_name(item))).weak().small());
        if !item.warnings.is_empty() {
            egui::Frame::new()
                .fill(WARN.gamma_multiply(0.12))
                .stroke(egui::Stroke::new(1.0, WARN.gamma_multiply(0.6)))
                .corner_radius(6.0)
                .inner_margin(8.0)
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(egui::RichText::new("Needs your attention").strong().color(WARN));
                    for w in &item.warnings {
                        ui.label(format!("• {w}"));
                    }
                });
        }
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.view, View::Edit, "✎ Edit");
            ui.selectable_value(&mut self.view, View::Tree, "⚑ Find & apply to all");
            ui.selectable_value(&mut self.view, View::Output, "👁 Result");
        });
        ui.label(
            egui::RichText::new(match self.view {
                View::Edit => "Change this file only. Anything is editable; invalid YAML is flagged as you type.",
                View::Tree => "Hover any field and press “Apply to all” to give it the same value in every resource that has it.",
                View::Output => "Read-only preview of exactly what will be written (your edits + shared values + namespace change).",
            })
            .weak()
            .small(),
        );
        ui.separator();
        egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| match self.view {
            View::Edit => editor::yaml_editor(ui, item),
            View::Tree => {
                let shared_paths: std::collections::HashSet<String> =
                    self.rules.iter().map(|r| r.path.clone()).collect();
                let mut cx = shared::TreeCx { kind: item.kind, shared: &shared_paths, wildcard: shared::wildcard_pick(ui), pick: None };
                shared::tree_node(ui, &item.value, "root", &mut vec![], &mut vec![], "", &mut cx);
                new_rule = cx.pick;
            }
            View::Output => {
                let mut text = export::render(item, &self.source_ns, &mode, &self.rules);
                highlight::code_view(ui, &mut text, false, &mut item.fold_out);
            }
        });
    });
    if let Some((path, value)) = new_rule {
        if let Some(i) = self.rules.iter().position(|r| r.path == path) { self.highlight = Some(i) } else {
            self.rules.push(common::Rule { enabled: true, path, value });
            self.highlight = Some(self.rules.len() - 1);
        }
    }
    }
}

impl App {
    fn welcome(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.15);
            ui.label(egui::RichText::new("Copy a Kubernetes namespace to somewhere new").size(26.0).strong());
            ui.label(egui::RichText::new("Export it as YAML, adjust it, and deploy it into another namespace. Secrets are never read.").weak());
            ui.add_space(16.0);
            ui.horizontal(|ui| {
                ui.add_space((ui.available_width() / 2.0 - 190.0).max(0.0));
                ui.label("Namespace");
                egui::ComboBox::from_id_salt("ns_welcome").selected_text(&self.source_ns).width(200.0).show_ui(ui, |ui| {
                    for n in &self.namespaces {
                        ui.selectable_value(&mut self.source_ns, n.clone(), n);
                    }
                });
                if ui
                    .add_enabled(
                        !self.busy && !self.source_ns.is_empty(),
                        egui::Button::new(egui::RichText::new("Load resources").strong()).min_size(egui::vec2(130.0, 26.0)),
                    )
                    .clicked()
                {
                    self.load();
                }
            });
            ui.add_space(28.0);
            let tile_w = 230.0;
            let gap = 12.0;
            let total = tile_w * 3.0 + gap * 2.0 + 3.0 * 20.0; // widths + gaps + frame margins
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = gap;
                ui.add_space(((ui.available_width() - total) / 2.0).max(0.0));
                for (icon, title, text) in [
                    ("👁", "Review", "See every resource. Flags show what references Secrets or had values hidden."),
                    ("✎", "Customize", "Edit any file, or set one value (a node group, a region…) across all of them at once."),
                    ("📦", "Export", "Choose the target namespace, or leave it open and decide at deploy time."),
                ] {
                    egui::Frame::group(ui.style()).inner_margin(10.0).show(ui, |ui| {
                        ui.vertical(|ui| {
                            ui.set_width(tile_w);
                            ui.set_min_height(100.0);
                            ui.label(egui::RichText::new(format!("{icon}  {title}")).strong().size(16.0));
                            ui.add_space(4.0);
                            ui.add(egui::Label::new(egui::RichText::new(text).weak()).wrap());
                        });
                    });
                }
            });
        });
    }

    fn overview(&mut self, ui: &mut egui::Ui) {
        ui.heading(format!("{} resources in '{}'", self.items.len(), self.source_ns));
        ui.label(egui::RichText::new("← Pick a resource to inspect or edit it. Untick anything you don't want exported.").weak());
        ui.add_space(8.0);
        let refs = self.secret_refs();
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.set_width(ui.available_width());
            if refs.is_empty() {
                ui.label("✔ No resources reference Secrets.");
            } else {
                ui.label(egui::RichText::new("Create these Secrets in the target namespace yourself").strong().color(WARN));
                ui.label(egui::RichText::new("They are referenced by your selected resources but never exported.").weak().small());
                for (name, n) in refs {
                    ui.horizontal(|ui| {
                        ui.monospace(name);
                        ui.weak(format!("used by {n} resource(s)"));
                    });
                }
            }
        });
        ui.add_space(8.0);
        if ui.button("⚑ Set one value across all resources…").clicked() {
            self.view = View::Tree;
            self.selected = self.items.iter().position(|i| i.selected).or(Some(0));
        }
    }

    fn resource_list(&mut self, ui: &mut egui::Ui) {
        ui.add_space(2.0);
        ui.add(
            egui::TextEdit::singleline(&mut self.filter)
                .hint_text("🔍 Search resources")
                .desired_width(f32::INFINITY),
        );
        ui.horizontal(|ui| {
            if ui.small_button("Select all").clicked() {
                self.items.iter_mut().for_each(|i| i.selected = true);
            }
            if ui.small_button("Select none").clicked() {
                self.items.iter_mut().for_each(|i| i.selected = false);
            }
            if ui.small_button("Overview").clicked() {
                self.selected = None;
            }
        });
        ui.separator();
        let f = self.filter.to_lowercase();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            let kinds: BTreeSet<&'static str> = self.items.iter().map(|i| i.kind).collect();
            for kind in kinds {
                let idxs: Vec<usize> = (0..self.items.len())
                    .filter(|&i| {
                        self.items[i].kind == kind && (f.is_empty() || self.items[i].name.to_lowercase().contains(&f))
                    })
                    .collect();
                if idxs.is_empty() {
                    continue;
                }
                let n_sel = idxs.iter().filter(|&&i| self.items[i].selected).count();
                let id = ui.make_persistent_id(("kind", kind));
                egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, true)
                    .show_header(ui, |ui| {
                        let mut all = n_sel == idxs.len();
                        if ui.checkbox(&mut all, "").on_hover_text("Include / exclude all of this kind").changed() {
                            for &i in &idxs {
                                self.items[i].selected = all;
                            }
                        }
                        ui.label(egui::RichText::new(kind).strong());
                        ui.weak(format!("{n_sel}/{}", idxs.len()));
                    })
                    .body(|ui| {
                        for i in idxs {
                            ui.horizontal(|ui| {
                                ui.checkbox(&mut self.items[i].selected, "");
                                let it = &self.items[i];
                                let (edited, warn, name) = (it.edited, !it.warnings.is_empty(), it.name.clone());
                                if ui.selectable_label(self.selected == Some(i), name).clicked() {
                                    self.selected = Some(i);
                                }
                                if warn {
                                    badge(ui, "⚠", WARN, "Has a note - references a Secret or had a value hidden");
                                }
                                if edited {
                                    badge(ui, "✎", ACCENT, "Edited");
                                }
                            });
                        }
                    });
            }
            if !self.notes.is_empty() {
                ui.separator();
                for n in &self.notes {
                    ui.small(egui::RichText::new(n).weak());
                }
            }
        });
    }
}

