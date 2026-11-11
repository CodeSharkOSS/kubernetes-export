//! Whole-manifest YAML editor with validation and re-redaction.

use crate::{highlight, k8s::Item, sanitize};
use eframe::egui;

/// Applies an edited YAML buffer to the item. Keeps the last valid value while
/// the text is invalid. Anything credential-like is re-redacted so the editor
/// can't be used to smuggle secrets into the export.
pub fn apply_edit(item: &mut Item) {
    let Some(buf) = item.buf.as_deref() else { return };
    let mut v: serde_json::Value = match serde_yaml_ng::from_str(buf) {
        Ok(v) => v,
        Err(e) => {
            item.err = Some(format!("Invalid YAML: {e}"));
            return;
        }
    };
    let kind = v.get("kind").and_then(|k| k.as_str());
    let name = v.pointer("/metadata/name").and_then(|n| n.as_str()).map(str::to_owned);
    if kind != Some(item.kind) {
        item.err = Some(format!("`kind` must stay '{}' (Secrets can't be created here)", item.kind));
        return;
    }
    let Some(name) = name else {
        item.err = Some("metadata.name is required".into());
        return;
    };
    if name.is_empty()
        || name.len() > 253
        || !name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.')
    {
        item.err = Some("metadata.name may only contain lowercase letters, digits, '-' and '.'".into());
        return;
    }
    item.err = None;
    let before = v.clone();
    item.warnings = sanitize::redact(&mut v, item.kind);
    if v != before {
        // Something sensitive was typed in: show the redacted text instead.
        item.buf = serde_yaml_ng::to_string(&v).ok();
        item.warnings.push("Credential-like values you entered were redacted".into());
    }
    item.name = name;
    item.edited = v != item.original;
    item.value = v;
    item.rev += 1;
}

pub fn yaml_editor(ui: &mut egui::Ui, item: &mut Item) {
    ui.horizontal(|ui| {
        if ui.add_enabled(item.edited, egui::Button::new("Reset to cluster value")).clicked() {
            item.value = item.original.clone();
            item.buf = None;
            item.err = None;
            item.edited = false;
            item.rev += 1;
            item.fold_edit.clear();
        }
        match &item.err {
            Some(e) => ui.colored_label(egui::Color32::from_rgb(230, 90, 90), e),
            None if item.edited => ui.colored_label(egui::Color32::from_rgb(120, 200, 120), "✎ modified"),
            None => ui.weak("unmodified"),
        };
    });
    let buf = item
        .buf
        .get_or_insert_with(|| serde_yaml_ng::to_string(&item.value).unwrap_or_default());
    let changed = highlight::code_view(ui, buf, true, &mut item.fold_edit);
    if changed {
        apply_edit(item);
    }
}

