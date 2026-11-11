//! The "Export report" window contents.

use crate::export;
use crate::map;
use crate::widgets::{ACCENT, BAD, OK, WARN};
use eframe::egui;

pub fn report_view(ui: &mut egui::Ui, r: &export::Report, secrets: &[(String, usize)]) {
    let failed = r.failed();
    if failed == 0 {
        ui.label(egui::RichText::new(format!("✔ {} resources exported", r.ok())).color(OK).heading());
    } else {
        ui.label(egui::RichText::new(format!("✖ {} succeeded, {failed} failed", r.ok())).color(BAD).heading());
    }
    ui.horizontal(|ui| {
        ui.label("Folder:");
        ui.monospace(r.dir.display().to_string());
        if ui.small_button("Copy").clicked() {
            ui.ctx().copy_text(r.dir.display().to_string());
        }
    });
    // deploy script line
    match &r.script_error {
        None => {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("✔").color(OK));
                ui.monospace("deploy.sh");
                let cmd = if r.templated {
                    format!("cd {} && ./deploy.sh <new-namespace>", r.dir.display())
                } else {
                    format!("cd {} && ./deploy.sh", r.dir.display())
                };
                ui.label(egui::RichText::new(&cmd).monospace().color(ACCENT));
                if ui.small_button("Copy").clicked() {
                    ui.ctx().copy_text(cmd);
                }
            });
        }
        Some(e) => {
            ui.colored_label(BAD, format!("✖ deploy.sh: {e}"));
        }
    }
    if !r.ignored.is_empty() {
        ui.label(
            egui::RichText::new(format!(
                "ℹ {} other .yaml file(s) already in this folder are not part of this export and deploy.sh will not apply them: {}",
                r.ignored.len(),
                r.ignored.iter().take(5).cloned().collect::<Vec<_>>().join(", ")
            ))
            .small()
            .color(WARN),
        );
    }
    if !secrets.is_empty() {
        egui::Frame::new()
            .fill(WARN.gamma_multiply(0.12))
            .stroke(egui::Stroke::new(1.0, WARN.gamma_multiply(0.6)))
            .corner_radius(6.0)
            .inner_margin(8.0)
            .show(ui, |ui| {
                ui.label(egui::RichText::new("Create these Secrets in the target namespace before deploying:").color(WARN));
                for (n, c) in secrets {
                    ui.monospace(format!("{n}  (used by {c})"));
                }
            });
    }
    ui.separator();
    egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
        egui::Grid::new("report").striped(true).num_columns(5).spacing([14.0, 4.0]).show(ui, |ui| {
            for h in ["", "Resource", "File", "Size", "Notes"] {
                ui.strong(h);
            }
            ui.end_row();
            for f in &r.files {
                match &f.error {
                    None => ui.label(egui::RichText::new("✔").color(OK)),
                    Some(_) => ui.label(egui::RichText::new("✖").color(BAD)),
                };
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(f.kind).color(map::kind_color(f.kind)));
                    ui.label(&f.resource);
                });
                ui.monospace(&f.file);
                ui.label(format!("{:.1} KB", f.bytes as f32 / 1024.0));
                ui.vertical(|ui| {
                    if let Some(e) = &f.error {
                        ui.colored_label(BAD, e);
                    }
                    for n in &f.notes {
                        ui.label(egui::RichText::new(n).small().weak());
                    }
                });
                ui.end_row();
            }
        });
    });
}

