//! Small shared UI pieces and the colour palette.

use eframe::egui;

pub const ACCENT: egui::Color32 = egui::Color32::from_rgb(90, 160, 255);
pub const WARN: egui::Color32 = egui::Color32::from_rgb(230, 170, 60);
pub const OK: egui::Color32 = egui::Color32::from_rgb(120, 200, 120);
pub const BAD: egui::Color32 = egui::Color32::from_rgb(230, 90, 90);

pub fn step_chip(ui: &mut egui::Ui, n: usize, label: &str, current: usize) {
    let (icon, color) = match n.cmp(&current) {
        std::cmp::Ordering::Less => ("✔".to_string(), OK),
        std::cmp::Ordering::Equal => (n.to_string(), ACCENT),
        std::cmp::Ordering::Greater => (n.to_string(), ui.visuals().weak_text_color()),
    };
    let text = egui::RichText::new(format!(" {icon}  {label} ")).color(color);
    let text = if n == current { text.strong() } else { text };
    egui::Frame::new()
        .stroke(egui::Stroke::new(if n == current { 1.5 } else { 1.0 }, color))
        .corner_radius(12.0)
        .inner_margin(egui::Margin::symmetric(6, 2))
        .show(ui, |ui| ui.label(text));
}

pub fn badge(ui: &mut egui::Ui, text: &str, color: egui::Color32, hover: &str) {
    ui.label(egui::RichText::new(text).color(color).small()).on_hover_text(hover);
}


/// Opens the system folder picker without blocking the UI; the chosen folder
/// arrives later as `Msg::Folder`.
pub fn browse_folder(tx: &std::sync::mpsc::Sender<crate::k8s::Msg>, ctx: &egui::Context, which: crate::k8s::FolderFor, start: &str) {
    let (tx, ctx, start) = (tx.clone(), ctx.clone(), start.to_string());
    std::thread::spawn(move || {
        let mut dlg = rfd::FileDialog::new().set_title(match which {
            crate::k8s::FolderFor::DeploySource => "Choose the exported folder to deploy",
            crate::k8s::FolderFor::ExportDest => "Choose where to save the export",
        });
        if let Ok(abs) = std::fs::canonicalize(&start) {
            dlg = dlg.set_directory(abs);
        }
        if let Some(p) = dlg.pick_folder() {
            let _ = tx.send(crate::k8s::Msg::Folder(which, p));
            ctx.request_repaint();
        }
    });
}
