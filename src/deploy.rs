//! Deploy a previously exported folder into a cluster, one resource at a time:
//! dry-run, apply, check health, remove - with an activity log.

use crate::highlight;
use crate::k8s::{self, Action, Evt, Health, Level, Msg, Prereq};
use crate::widgets::{ACCENT, BAD, OK, WARN};
use crate::{export, folder, map};
use eframe::egui;
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

#[derive(Clone, PartialEq, Debug)]
enum Status {
    Pending,
    DryRunOk(String),
    Applied(String),
    Removed,
    Failed(String),
}

#[derive(PartialEq, Clone, Copy)]
enum DView {
    Edit,
    Applied,
}

struct DItem {
    file: String,
    kind: &'static str,
    api_version: String,
    name: String,
    /// Current (possibly edited) file contents, placeholder not yet substituted.
    text: String,
    /// Contents on disk, to detect unsaved edits and to revert.
    disk_text: String,
    /// YAML/shape problem in the current text.
    err: Option<String>,
    /// Edited after it was deployed: the cluster doesn't have these changes yet.
    stale: bool,
    status: Status,
    health: Option<Health>,
    /// Applied by this tool in this session (only these can be removed here).
    applied: bool,
    refused: Option<String>,
    warnings: Vec<String>,
    secret_refs: Vec<String>,
    folds: BTreeSet<usize>,
    folds_applied: BTreeSet<usize>,
}

struct Entry {
    at: String,
    ok: bool,
    msg: String,
}

pub struct Deploy {
    dir: String,
    items: Vec<DItem>,
    /// Directory the items were actually loaded from (the text box may change afterwards).
    loaded_dir: std::path::PathBuf,
    epoch: u64,
    ns: String,
    templated: bool,
    load_err: Option<String>,
    confirmed_for: Option<String>,
    log: Vec<Entry>,
    selected: Option<usize>,
    view: DView,
    prereq: Option<Result<Prereq, String>>,
    last_poll: Instant,
    pending: usize,
    remove_armed: Option<usize>,
}

pub struct Cx<'a> {
    pub rt: &'a tokio::runtime::Runtime,
    pub tx: &'a Sender<Msg>,
    pub cluster: &'a str,
}

impl Default for Deploy {
    fn default() -> Self {
        Self {
            dir: "./export".into(),
            items: vec![],
            loaded_dir: PathBuf::default(),
            epoch: 0,
            ns: String::new(),
            templated: false,
            load_err: None,
            confirmed_for: None,
            log: vec![],
            selected: None,
            view: DView::Edit,
            prereq: None,
            last_poll: Instant::now(),
            pending: 0,
            remove_armed: None,
        }
    }
}

fn now() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    format!("{:02}:{:02}:{:02}", (secs / 3600) % 24, (secs / 60) % 60, secs % 60)
}

impl Deploy {
    fn log(&mut self, ok: bool, msg: impl Into<String>) {
        self.log.push(Entry { at: now(), ok, msg: msg.into() });
    }

    // ------------------------------------------------------------- loading
    fn load_dir(&mut self) {
        self.items.clear();
        self.epoch += 1; // results of anything still in flight belong to the old folder
        self.pending = 0;
        self.ns.clear(); // never carry a namespace (or its confirmation) over from another folder
        self.confirmed_for = None;
        self.remove_armed = None;
        self.loaded_dir = std::path::PathBuf::from(&self.dir);
        self.templated = false;
        self.selected = None;
        self.load_err = None;
        self.prereq = None;
        let folder = match folder::read(std::path::Path::new(&self.dir)) {
            Ok(f) => f,
            Err(e) => {
                self.load_err = Some(e);
                return;
            }
        };
        self.templated = folder.templated;
        if let Some(ns) = folder.detected_ns {
            self.ns = ns;
        }
        self.load_err = folder.errors.first().cloned();
        for f in folder.files {
            self.items.push(DItem {
                file: f.file,
                kind: f.kind,
                api_version: f.api_version,
                name: f.name,
                disk_text: f.text.clone(),
                text: f.text,
                err: None,
                stale: false,
                status: Status::Pending,
                health: None,
                applied: false,
                refused: f.refused,
                warnings: f.warnings,
                secret_refs: f.secret_refs,
                folds: BTreeSet::new(),
                folds_applied: BTreeSet::new(),
            });
        }
        self.log(true, format!("Loaded {} files from {}", self.items.len(), self.dir));
    }

    /// Re-validate after the user edits a file; invalidates stale dry-run results.
    fn on_edit(&mut self, i: usize) {
        let it = &mut self.items[i];
        let v: Value = match serde_yaml_ng::from_str(&it.text) {
            Ok(v) => v,
            Err(e) => {
                it.err = Some(format!("Invalid YAML: {e}"));
                return;
            }
        };
        if v["kind"].as_str() != Some(it.kind) {
            it.err = Some(format!("`kind` must stay '{}'", it.kind));
            return;
        }
        let av = v["apiVersion"].as_str().unwrap_or("");
        if k8s::resource_for(av, it.kind).is_none() {
            it.err = Some(format!("unsupported apiVersion '{av}' for {}", it.kind));
            return;
        }
        let name = v["metadata"]["name"].as_str().unwrap_or("");
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.') {
            it.err = Some("metadata.name is required (lowercase letters, digits, '-' and '.')".into());
            return;
        }
        it.err = None;
        it.api_version = av.to_string();
        it.name = name.to_string();
        let (w, refs) = folder::analyze(&v, it.kind, &it.text);
        it.warnings = w;
        it.secret_refs = refs;
        match it.status {
            Status::DryRunOk(_) | Status::Failed(_) => it.status = Status::Pending,
            Status::Applied(_) => it.stale = true,
            _ => {}
        }
        self.templated |= it.text.contains(export::PLACEHOLDER);
        self.prereq = None; // referenced Secrets may have changed
    }

    fn save(&mut self, i: usize) {
        let path = self.loaded_dir.join(&self.items[i].file);
        let text = self.items[i].text.clone();
        match std::fs::write(&path, &text) {
            Ok(()) => {
                self.items[i].disk_text = text;
                self.log(true, format!("Saved {}", path.display()));
            }
            Err(e) => self.log(false, format!("Couldn't save {}: {e}", path.display())),
        }
    }

    fn revert(&mut self, i: usize) {
        let it = &mut self.items[i];
        it.text = it.disk_text.clone();
        it.folds.clear();
        self.on_edit(i);
    }

    /// The manifest as it will be sent: placeholder substituted, namespace forced.
    fn manifest(&self, i: usize) -> Result<Value, String> {
        folder::manifest(&self.items[i].text, &self.ns)
    }

    fn secrets_needed(&self) -> Vec<String> {
        let mut s: Vec<String> = self.items.iter().flat_map(|i| i.secret_refs.clone()).collect();
        s.sort();
        s.dedup();
        s
    }

    fn gate_ok(&self, cx: &Cx) -> bool {
        export::valid_ns(&self.ns) && self.confirmed_for.as_deref() == Some(self.gate_key(cx).as_str())
    }

    fn gate_key(&self, cx: &Cx) -> String {
        format!("{}|{}", cx.cluster, self.ns)
    }

    // ------------------------------------------------------------- actions
    fn spawn(&mut self, cx: &Cx, ctx: &egui::Context, idx: Option<usize>, action: Action, fut: impl std::future::Future<Output = Result<String, String>> + Send + 'static) {
        self.pending += 1;
        let (tx, ctx, epoch) = (cx.tx.clone(), ctx.clone(), self.epoch);
        cx.rt.spawn(async move {
            let result = fut.await;
            let _ = tx.send(Msg::Deploy(Evt { epoch, idx, action, result, health: None }));
            ctx.request_repaint();
        });
    }

    fn act(&mut self, cx: &Cx, ctx: &egui::Context, i: usize, action: Action) {
        // Defence in depth: the buttons are disabled without the confirmation, but never rely on that alone.
        if matches!(action, Action::Apply | Action::Remove | Action::CreateNamespace) && !self.gate_ok(cx) {
            self.log(false, "Refused: confirm the cluster and namespace first");
            return;
        }
        if i >= self.items.len() {
            return;
        }
        let ns = self.ns.clone();
        let (kind, av, name) = (self.items[i].kind, self.items[i].api_version.clone(), self.items[i].name.clone());
        match action {
            Action::DryRun | Action::Apply => {
                let obj = match self.manifest(i) {
                    Ok(o) => o,
                    Err(e) => {
                        self.items[i].status = Status::Failed(e.clone());
                        self.log(false, format!("{kind}/{name}: {e}"));
                        return;
                    }
                };
                let dry = action == Action::DryRun;
                self.spawn(cx, ctx, Some(i), action, async move { k8s::apply(&ns, &obj, dry).await });
            }
            Action::Remove => self.spawn(cx, ctx, Some(i), action, async move { k8s::remove(&ns, &av, kind, &name).await }),
            Action::Health => {
                let (tx, ctx, epoch) = (cx.tx.clone(), ctx.clone(), self.epoch);
                cx.rt.spawn(async move {
                    let (result, health) = match k8s::health(&ns, &av, kind, &name).await {
                        Ok(h) => (Ok(h.detail.clone()), Some(h)),
                        Err(e) => (Err(e), None),
                    };
                    let _ = tx.send(Msg::Deploy(Evt { epoch, idx: Some(i), action, result, health }));
                    ctx.request_repaint();
                });
            }
            Action::CreateNamespace => self.spawn(cx, ctx, None, action, async move { k8s::create_namespace(&ns).await }),
        }
    }

    /// Run one action over many files sequentially in a single task: gentler on the
    /// API server (and on the auth plugin) and the log comes out in file order.
    fn act_batch(&mut self, cx: &Cx, ctx: &egui::Context, action: Action) {
        let ns = self.ns.clone();
        let epoch = self.epoch;
        let (tx, ctx) = (cx.tx.clone(), ctx.clone());
        let mut jobs: Vec<(usize, String, &'static str, String, Option<Value>)> = vec![];
        for i in 0..self.items.len() {
            if self.items[i].refused.is_some() || self.items[i].err.is_some() {
                continue;
            }
            let obj = if action == Action::DryRun {
                match self.manifest(i) {
                    Ok(o) => Some(o),
                    Err(e) => {
                        self.items[i].status = Status::Failed(e);
                        continue;
                    }
                }
            } else {
                None
            };
            jobs.push((i, self.items[i].api_version.clone(), self.items[i].kind, self.items[i].name.clone(), obj));
        }
        if action == Action::DryRun {
            self.pending += jobs.len();
        }
        cx.rt.spawn(async move {
            for (i, av, kind, name, obj) in jobs {
                let (result, health) = match (action, obj) {
                    (Action::DryRun, Some(o)) => (k8s::apply(&ns, &o, true).await, None),
                    _ => match k8s::health(&ns, &av, kind, &name).await {
                        Ok(h) => (Ok(h.detail.clone()), Some(h)),
                        Err(e) => (Err(e), None),
                    },
                };
                let _ = tx.send(Msg::Deploy(Evt { epoch, idx: Some(i), action, result, health }));
                ctx.request_repaint();
            }
        });
    }

    fn check_prereqs(&mut self, cx: &Cx, ctx: &egui::Context) {
        if !export::valid_ns(&self.ns) {
            return;
        }
        let (tx, ctx, ns) = (cx.tx.clone(), ctx.clone(), self.ns.clone());
        cx.rt.spawn(async move {
            let _ = tx.send(Msg::Prereq(k8s::prereqs(&ns).await));
            ctx.request_repaint();
        });
    }

    /// A folder chosen in the file browser: use it and load it straight away.
    pub fn set_dir(&mut self, dir: String) {
        self.dir = dir;
        self.load_dir();
    }

    pub fn on_prereq(&mut self, r: Result<Prereq, String>) {
        self.prereq = Some(r);
    }

    pub fn on_event(&mut self, e: Evt) {
        if e.epoch != self.epoch || e.idx.is_some_and(|i| i >= self.items.len()) {
            return; // belongs to a previously loaded folder
        }
        if e.action != Action::Health {
            self.pending = self.pending.saturating_sub(1); // background health polls don't block the buttons
        }
        let label = e.idx.map_or_else(|| "namespace".into(), |i| format!("{}/{}", self.items[i].kind, self.items[i].name));
        match (e.action, e.result) {
            (Action::Health, r) => {
                if let Some(i) = e.idx {
                    match (r, e.health) {
                        (Ok(_), Some(h)) => {
                            if h.level == Level::Missing && self.items[i].applied {
                                self.items[i].applied = false;
                            }
                            self.items[i].health = Some(h);
                        }
                        (Err(m), _) => self.items[i].health = Some(Health { level: Level::Failed, detail: m }),
                        _ => {}
                    }
                }
            }
            (a, Ok(msg)) => {
                self.log(true, format!("{label}: {msg}"));
                if let Some(i) = e.idx {
                    let it = &mut self.items[i];
                    match a {
                        Action::DryRun => it.status = Status::DryRunOk(msg),
                        Action::Apply => {
                            it.status = Status::Applied(msg);
                            it.stale = false;
                            it.applied = true;
                            it.health = None;
                            self.last_poll = Instant::now().checked_sub(Duration::from_secs(10)).unwrap_or_else(Instant::now); // check health soon
                        }
                        Action::Remove => {
                            it.status = Status::Removed;
                            it.applied = false;
                            it.health = None;
                        }
                        _ => {}
                    }
                } else if a == Action::CreateNamespace {
                    self.prereq = None; // re-check next frame
                }
            }
            (a, Err(m)) => {
                self.log(false, format!("{label}: {m}"));
                if let Some(i) = e.idx
                    && a != Action::Remove
                {
                    self.items[i].status = Status::Failed(m);
                }
            }
        }
    }

    // ------------------------------------------------------------------ UI
    pub fn ui(&mut self, ui: &mut egui::Ui, cx: &Cx) {
        let ctx = ui.ctx().clone();
        if self.pending > 0 {
            ctx.request_repaint_after(Duration::from_millis(150));
        }
        // prerequisites auto-check when namespace is valid and not yet checked
        if self.prereq.is_none() && !self.items.is_empty() && export::valid_ns(&self.ns) && self.pending == 0 {
            self.check_prereqs(cx, &ctx);
            self.prereq = Some(Err("checking…".into()));
        }
        // gentle health polling of things we applied
        let polling: Vec<usize> = (0..self.items.len())
            .filter(|&i| matches!(self.items[i].status, Status::Applied(_)) && self.items[i].health.as_ref().is_none_or(|h| h.level != Level::Ready))
            .collect();
        if !polling.is_empty() {
            ctx.request_repaint_after(Duration::from_secs(1));
            if self.last_poll.elapsed() > Duration::from_secs(3) {
                self.last_poll = Instant::now();
                for i in polling {
                    self.act(cx, &ctx, i, Action::Health);
                }
            }
        }

        let gate_ok = self.gate_ok(cx);

        egui::Panel::top("deploy_top").show(ui, |ui| self.target_card(ui, cx, &ctx, gate_ok));

        egui::Panel::bottom("deploy_log").resizable(true).default_size(150.0).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.strong("Activity log");
                if ui.small_button("Clear").clicked() {
                    self.log.clear();
                }
            });
            egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
                for e in &self.log {
                    ui.horizontal(|ui| {
                        ui.weak(&e.at);
                        ui.label(egui::RichText::new(if e.ok { "✔" } else { "✖" }).color(if e.ok { OK } else { BAD }));
                        ui.label(&e.msg);
                    });
                }
            });
        });

        if self.items.is_empty() {
            egui::CentralPanel::default().show(ui, |ui| {
                ui.add_space(30.0);
                ui.vertical_centered(|ui| {
                    ui.label(egui::RichText::new("Deploy an exported folder").size(24.0).strong());
                    ui.label(egui::RichText::new("Load the folder above, then dry-run and deploy resources one at a time, checking health as you go.").weak());
                    if let Some(e) = &self.load_err {
                        ui.colored_label(BAD, e);
                    }
                });
            });
            return;
        }

        egui::Panel::left("deploy_list").resizable(true).default_size(340.0).show(ui, |ui| self.list(ui, cx, &ctx, gate_ok));
        egui::CentralPanel::default().show(ui, |ui| self.details(ui, cx, &ctx, gate_ok));
    }

    fn target_card(&mut self, ui: &mut egui::Ui, cx: &Cx, ctx: &egui::Context, gate_ok: bool) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.strong("Export folder");
            ui.add(egui::TextEdit::singleline(&mut self.dir).desired_width(280.0));
            if ui.button("📁 Browse…").on_hover_text("Pick the folder with a file browser").clicked() {
                crate::widgets::browse_folder(cx.tx, ctx, k8s::FolderFor::DeploySource, &self.dir);
            }
            if ui.button("Load").clicked() {
                self.load_dir();
            }
            ui.separator();
            ui.strong("Deploy into namespace");
            let before = self.ns.clone();
            ui.add(
                egui::TextEdit::singleline(&mut self.ns)
                    .hint_text(if self.templated { "required" } else { "namespace" })
                    .desired_width(160.0),
            );
            if self.ns != before {
                self.prereq = None;
            }
        });
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(format!("Cluster: {}", cx.cluster)).monospace().small());
            if !self.ns.is_empty() && !export::valid_ns(&self.ns) {
                ui.colored_label(BAD, "not a valid namespace name");
            }
            match &self.prereq {
                Some(Ok(p)) if !p.ns_exists => {
                    ui.colored_label(WARN, "namespace doesn't exist yet");
                    if ui.add_enabled(gate_ok, egui::Button::new("Create namespace")).on_disabled_hover_text("Tick the confirmation first").clicked() {
                        self.act(cx, ctx, 0, Action::CreateNamespace);
                    }
                }
                Some(Ok(_)) => {
                    ui.colored_label(OK, "✔ namespace exists");
                }
                Some(Err(m)) if m != "checking…" => {
                    ui.colored_label(WARN, format!("couldn't check namespace: {m}"));
                }
                _ => {}
            }
        });
        let key = self.gate_key(cx);
        let mut on = self.confirmed_for.as_deref() == Some(key.as_str());
        let enabled = export::valid_ns(&self.ns) && !self.items.is_empty();
        if ui
            .add_enabled(enabled, egui::Checkbox::new(&mut on, format!("I confirm: changes will be made in namespace '{}' on this cluster", self.ns)))
            .changed()
        {
            self.confirmed_for = on.then_some(key);
        }
        if self.templated && self.ns.is_empty() && !self.items.is_empty() {
            ui.colored_label(WARN, "These files are templated: enter the namespace to deploy into.");
        }
        ui.add_space(4.0);
    }

    fn list(&mut self, ui: &mut egui::Ui, cx: &Cx, ctx: &egui::Context, gate_ok: bool) {
        ui.add_space(4.0);
        let next = self.items.iter().position(|i| {
            i.refused.is_none() && i.err.is_none() && matches!(i.status, Status::Pending | Status::DryRunOk(_) | Status::Removed)
        });
        ui.horizontal_wrapped(|ui| {
            let label = next.map_or("▶ Deploy next".to_string(), |i| format!("▶ Deploy next: {}", self.items[i].name));
            if ui
                .add_enabled(gate_ok && next.is_some() && self.pending == 0, egui::Button::new(egui::RichText::new(label).strong()))
                .on_disabled_hover_text("Needs the confirmation ticked and an undeployed resource")
                .clicked()
                && let Some(i) = next
            {
                self.selected = Some(i);
                self.act(cx, ctx, i, Action::Apply);
            }
        });
        ui.horizontal(|ui| {
            if ui.add_enabled(export::valid_ns(&self.ns), egui::Button::new("Dry-run all")).on_hover_text("Server-side validation of every file. Creates nothing.").clicked() {
                self.act_batch(cx, ctx, Action::DryRun);
            }
            if ui.add_enabled(export::valid_ns(&self.ns), egui::Button::new("↻ Check cluster")).on_hover_text("Read-only: look up each resource's live health").clicked() {
                self.act_batch(cx, ctx, Action::Health);
            }
        });
        ui.separator();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for i in 0..self.items.len() {
                let it = &self.items[i];
                let (icon, color) = match (&it.refused, &it.status, it.health.as_ref().map(|h| h.level)) {
                    (Some(_), _, _) => ("⛔", BAD),
                    (_, Status::Failed(_), _) => ("✖", BAD),
                    (_, Status::Applied(_), Some(Level::Ready)) => ("●", OK),
                    (_, Status::Applied(_), Some(Level::Failed)) => ("●", BAD),
                    (_, Status::Applied(_), _) => ("◐", WARN),
                    (_, Status::DryRunOk(_), _) => ("◌", ACCENT),
                    (_, Status::Removed, _) => ("⊘", egui::Color32::GRAY),
                    _ => ("○", egui::Color32::GRAY),
                };
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(icon).color(color));
                    ui.label(egui::RichText::new(it.kind).color(map::kind_color(it.kind)).small());
                    if ui.selectable_label(self.selected == Some(i), &it.name).clicked() {
                        self.selected = Some(i);
                    }
                    if let Some(h) = &it.health {
                        ui.weak(&h.detail);
                    }
                    if !it.warnings.is_empty() {
                        ui.label(egui::RichText::new("⚠").color(WARN));
                    }
                });
            }
        });
    }

    fn details(&mut self, ui: &mut egui::Ui, cx: &Cx, ctx: &egui::Context, gate_ok: bool) {
        let Some(i) = self.selected.filter(|&i| i < self.items.len()) else {
            self.prereq_card(ui);
            return;
        };
        if self.remove_armed.is_some_and(|a| a != i) {
            self.remove_armed = None; // armed on another resource: don't leave it primed
        }
        let (kind, name, refused) = (self.items[i].kind, self.items[i].name.clone(), self.items[i].refused.clone());
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(kind).color(map::kind_color(kind)).strong());
            ui.heading(&name);
        });
        ui.label(egui::RichText::new(format!("→ {}", self.items[i].file)).weak().small());
        if let Some(r) = &refused {
            ui.colored_label(BAD, format!("⛔ {r}"));
        }
        for w in &self.items[i].warnings {
            ui.colored_label(WARN, format!("⚠ {w}"));
        }
        self.action_buttons(ui, cx, ctx, i, gate_ok, refused.is_some());
        self.status_lines(ui, i);
        self.file_view(ui, i);
    }

    fn action_buttons(&mut self, ui: &mut egui::Ui, cx: &Cx, ctx: &egui::Context, i: usize, gate_ok: bool, refused: bool) {
        let valid = export::valid_ns(&self.ns);
        let idle = self.pending == 0;
        ui.horizontal(|ui| {
            let ok = !refused && valid && self.items[i].err.is_none();
            if ui.add_enabled(ok && idle, egui::Button::new("1 · Dry run")).on_hover_text("Ask the API server to validate this file. Nothing is created.").clicked() {
                self.act(cx, ctx, i, Action::DryRun);
            }
            if ui
                .add_enabled(ok && idle && gate_ok, egui::Button::new(egui::RichText::new("2 · Deploy").strong()))
                .on_disabled_hover_text("Tick the confirmation at the top first")
                .clicked()
            {
                self.act(cx, ctx, i, Action::Apply);
            }
            if ui.add_enabled(ok && idle, egui::Button::new("3 · Check health")).clicked() {
                self.act(cx, ctx, i, Action::Health);
            }
            let can_remove = self.items[i].applied && gate_ok && idle;
            let armed = self.remove_armed == Some(i);
            if ui
                .add_enabled(can_remove, egui::Button::new(if armed { "Click again to confirm removal" } else { "Remove" }))
                .on_disabled_hover_text("Only resources deployed by this tool in this session can be removed here")
                .clicked()
            {
                if armed {
                    self.remove_armed = None;
                    self.act(cx, ctx, i, Action::Remove);
                } else {
                    self.remove_armed = Some(i);
                }
            }
        });
    }

    fn status_lines(&self, ui: &mut egui::Ui, i: usize) {
        match &self.items[i].status {
            Status::DryRunOk(m) => ui.colored_label(ACCENT, format!("Dry run: {m}")),
            Status::Applied(m) => ui.colored_label(OK, format!("Deployed ({m})")),
            Status::Failed(m) => ui.colored_label(BAD, format!("Failed: {m}")),
            Status::Removed => ui.weak("Removed from cluster"),
            Status::Pending => ui.weak("Not deployed yet"),
        };
        if let Some(h) = &self.items[i].health {
            let c = match h.level {
                Level::Ready => OK,
                Level::Progressing => WARN,
                Level::Failed | Level::Missing => BAD,
            };
            ui.colored_label(c, format!("Health: {:?} - {}", h.level, h.detail));
        }
        if self.items[i].stale {
            ui.colored_label(WARN, "Edited after deploying - the cluster doesn't have these changes yet. Deploy again to apply them.");
        }
    }

    fn file_view(&mut self, ui: &mut egui::Ui, i: usize) {
        ui.separator();
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.view, DView::Edit, "✎ Edit file");
            ui.selectable_value(&mut self.view, DView::Applied, "👁 As applied");
            if self.view == DView::Edit {
                ui.separator();
                let dirty = self.items[i].text != self.items[i].disk_text;
                if ui.add_enabled(dirty && self.items[i].err.is_none(), egui::Button::new("💾 Save to file")).on_hover_text("Write your edits back to the file in the export folder").clicked() {
                    self.save(i);
                }
                if ui.add_enabled(dirty, egui::Button::new("↺ Revert")).on_hover_text("Discard edits and reload the file's contents").clicked() {
                    self.revert(i);
                }
                if dirty {
                    ui.label(egui::RichText::new("● unsaved (used for deploying, not yet written to the file)").small().color(WARN));
                }
            }
        });
        match self.view {
            DView::Edit => {
                ui.label(egui::RichText::new("Edit anything. __NAMESPACE__ is replaced with the namespace above when deploying; edits take effect for Dry run and Deploy immediately.").weak().small());
                if let Some(e) = &self.items[i].err {
                    ui.colored_label(BAD, e);
                }
                egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
                    let it = &mut self.items[i];
                    if highlight::code_view(ui, &mut it.text, true, &mut it.folds) {
                        self.on_edit(i);
                    }
                });
            }
            DView::Applied => {
                let mut text = match self.manifest(i) {
                    Ok(v) => serde_yaml_ng::to_string(&v).unwrap_or_default(),
                    Err(e) => e,
                };
                egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
                    let it = &mut self.items[i];
                    highlight::code_view(ui, &mut text, false, &mut it.folds_applied);
                });
            }
        }
    }

    fn prereq_card(&mut self, ui: &mut egui::Ui) {
        ui.heading("Before you start");
        ui.label(egui::RichText::new("Pick a resource on the left. Suggested order: Dry-run all → deploy one at a time → check health.").weak());
        ui.add_space(8.0);
        let needed = self.secrets_needed();
        if needed.is_empty() {
            ui.label("✔ These files reference no Secrets.");
            return;
        }
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(egui::RichText::new("Secrets these resources need (not included in the export)").strong().color(WARN));
            let present = match &self.prereq {
                Some(Ok(p)) => p.secrets.clone(),
                _ => None,
            };
            for n in needed {
                ui.horizontal(|ui| match &present {
                    Some(list) if list.contains(&n) => {
                        ui.label(egui::RichText::new("✔").color(OK));
                        ui.monospace(&n);
                        ui.weak("exists in the namespace");
                    }
                    Some(_) => {
                        ui.label(egui::RichText::new("✖").color(BAD));
                        ui.monospace(&n);
                        ui.weak("missing - create it before deploying workloads that use it");
                    }
                    None => {
                        ui.monospace(&n);
                        ui.weak("(can't check: not permitted or namespace not found)");
                    }
                });
            }
            ui.label(egui::RichText::new("Only Secret names are looked up (metadata only). Secret contents are never read.").small().weak());
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn loads_folder_in_script_order_and_substitutes_namespace() {
        let dir = std::env::temp_dir().join(format!("k8s-deploy-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let items = vec![
            crate::k8s::Item::new("Deployment", "app".into(), json!({"apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":"app","namespace":"dev"},
                "spec":{"template":{"spec":{"containers":[{"name":"c","env":[{"name":"A","valueFrom":{"secretKeyRef":{"name":"creds","key":"k"}}}]}]}}}}), vec![]),
            crate::k8s::Item::new("ConfigMap", "cfg".into(), json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"cfg","namespace":"dev"}}), vec![]),
        ];
        export::write_all(&items, "dev", &export::Mode::Template, &[], &dir).unwrap();
        std::fs::write(dir.join("secret-x.yaml"), "apiVersion: v1\nkind: Secret\nmetadata:\n  name: x\n").unwrap();

        let mut d = Deploy { dir: dir.display().to_string(), ..Default::default() };
        d.load_dir();
        // deploy.sh order: ConfigMap before Deployment; the stray Secret file is not in the script
        let order: Vec<&str> = d.items.iter().map(|i| i.kind).collect();
        assert_eq!(order, ["ConfigMap", "Deployment"]);
        assert!(d.templated);
        assert_eq!(d.secrets_needed(), vec!["creds".to_string()]);
        d.ns = "qa-copy".into();
        let m = d.manifest(1).unwrap();
        assert_eq!(m["metadata"]["namespace"], "qa-copy");

        // editing: invalid YAML and kind changes are rejected; valid edits reset a dry-run result
        d.items[1].status = Status::DryRunOk("ok".into());
        d.items[1].text = d.items[1].text.replace("name: app", "name: app2");
        d.on_edit(1);
        assert!(d.items[1].err.is_none() && d.items[1].name == "app2" && d.items[1].status == Status::Pending);
        d.items[1].text = "kind: [".into();
        d.on_edit(1);
        assert!(d.items[1].err.is_some());
        d.revert(1);
        assert!(d.items[1].err.is_none() && d.items[1].name == "app");
        d.items[1].text = d.items[1].text.replace("kind: Deployment", "kind: Secret");
        d.on_edit(1);
        assert!(d.items[1].err.as_deref().unwrap().contains("kind"));
        d.revert(1);

        // a Secret file is refused if it is loaded directly
        std::fs::remove_file(dir.join("deploy.sh")).unwrap();
        d.load_dir();
        let sec = d.items.iter().find(|i| i.file == "secret-x.yaml").unwrap();
        assert!(sec.refused.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mutating_actions_need_confirmation_and_stale_events_are_ignored() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let cx = Cx { rt: &rt, tx: &tx, cluster: "https://example" };
        let ctx = egui::Context::default();
        let mut d = Deploy::default();
        d.items.push(DItem {
            file: "a.yaml".into(),
            kind: "ConfigMap",
            api_version: "v1".into(),
            name: "a".into(),
            text: "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: a\n".into(),
            disk_text: String::new(),
            err: None,
            stale: false,
            status: Status::Pending,
            health: None,
            applied: false,
            refused: None,
            warnings: vec![],
            secret_refs: vec![],
            folds: BTreeSet::default(),
            folds_applied: BTreeSet::default(),
        });
        d.ns = "qa".into();
        // no confirmation: every mutating action is refused and nothing is spawned
        for a in [Action::Apply, Action::Remove, Action::CreateNamespace] {
            d.act(&cx, &ctx, 0, a);
        }
        assert_eq!(d.pending, 0);
        assert!(rx.try_recv().is_err());
        assert_eq!(d.log.iter().filter(|e| !e.ok && e.msg.starts_with("Refused")).count(), 3);
        // confirmation for a different namespace doesn't count
        d.confirmed_for = Some("https://example|other".into());
        assert!(!d.gate_ok(&cx));
        d.confirmed_for = Some("https://example|qa".into());
        assert!(d.gate_ok(&cx));

        // events from a previously loaded folder are dropped
        d.epoch = 5;
        d.on_event(Evt { epoch: 4, idx: Some(0), action: Action::Apply, result: Ok("created".into()), health: None });
        assert_eq!(d.items[0].status, Status::Pending);
        d.on_event(Evt { epoch: 5, idx: Some(9), action: Action::Apply, result: Ok("created".into()), health: None });
        d.on_event(Evt { epoch: 5, idx: Some(0), action: Action::Apply, result: Ok("created".into()), health: None });
        assert!(matches!(d.items[0].status, Status::Applied(_)));
    }
}
