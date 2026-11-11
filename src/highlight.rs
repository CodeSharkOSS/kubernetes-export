//! Minimal line-based YAML syntax highlighting for the editor / preview.

use std::fmt::Write as _;
use eframe::egui::{self, Color32, FontId, TextFormat, text::LayoutJob};

struct Palette {
    key: Color32,
    string: Color32,
    number: Color32,
    keyword: Color32,
    comment: Color32,
    punct: Color32,
    special: Color32,
    plain: Color32,
}

fn palette(dark: bool) -> Palette {
    if dark {
        Palette {
            key: Color32::from_rgb(110, 175, 255),
            string: Color32::from_rgb(150, 210, 120),
            number: Color32::from_rgb(235, 160, 90),
            keyword: Color32::from_rgb(200, 140, 235),
            comment: Color32::from_rgb(120, 130, 140),
            punct: Color32::from_rgb(150, 150, 160),
            special: Color32::from_rgb(255, 110, 110),
            plain: Color32::from_rgb(220, 220, 220),
        }
    } else {
        Palette {
            key: Color32::from_rgb(20, 90, 190),
            string: Color32::from_rgb(40, 130, 40),
            number: Color32::from_rgb(190, 100, 20),
            keyword: Color32::from_rgb(140, 60, 180),
            comment: Color32::from_rgb(130, 140, 150),
            punct: Color32::from_rgb(110, 110, 120),
            special: Color32::from_rgb(200, 30, 30),
            plain: Color32::from_rgb(30, 30, 30),
        }
    }
}

fn scalar_color(v: &str, p: &Palette) -> Color32 {
    let t = v.trim();
    if t.contains("__NAMESPACE__") || t.contains("<REDACTED>") {
        p.special
    } else if t.starts_with('"') || t.starts_with('\'') {
        p.string
    } else if matches!(t, "true" | "false" | "null" | "~") {
        p.keyword
    } else if !t.is_empty() && t.parse::<f64>().is_ok() {
        p.number
    } else {
        p.plain
    }
}

pub fn yaml_job(text: &str, font: &FontId, dark: bool, wrap_width: f32) -> LayoutJob {
    let p = palette(dark);
    let mut job = LayoutJob::default();
    job.wrap.max_width = wrap_width;
    let mut put = |s: &str, c: Color32| {
        if !s.is_empty() {
            job.append(s, 0.0, TextFormat::simple(font.clone(), c));
        }
    };
    for line in text.split_inclusive('\n') {
        let (body, nl) = match line.strip_suffix('\n') {
            Some(b) => (b, "\n"),
            None => (line, ""),
        };
        let indent_len = body.len() - body.trim_start().len();
        put(&body[..indent_len], p.plain);
        let mut rest = &body[indent_len..];

        if rest.starts_with('#') {
            put(rest, p.comment);
            put(nl, p.plain);
            continue;
        }
        // list markers, possibly nested ("- - x")
        while let Some(r) = rest.strip_prefix("- ") {
            put("- ", p.punct);
            rest = r;
        }
        if rest == "-" {
            put("-", p.punct);
            rest = "";
        }
        // key: value (key must be unquoted text up to the first ": " or a trailing ':')
        let key_end = if rest.starts_with('"') || rest.starts_with('\'') {
            None
        } else {
            rest.find(": ").or_else(|| rest.strip_suffix(':').map(str::len))
        };
        match key_end {
            Some(i) => {
                put(&rest[..i], p.key);
                put(":", p.punct);
                let val = &rest[i + 1..];
                let (val, comment) = match val.find(" #") {
                    Some(c) => (&val[..c], &val[c..]),
                    None => (val, ""),
                };
                let lead = val.len() - val.trim_start().len();
                put(&val[..lead], p.plain);
                put(&val[lead..], scalar_color(val, &p));
                put(comment, p.comment);
            }
            None => put(rest, scalar_color(rest, &p)),
        }
        put(nl, p.plain);
    }
    job
}

/// Layouter closure for `TextEdit::layouter`.
pub fn layouter(ui: &egui::Ui) -> impl FnMut(&egui::Ui, &dyn egui::TextBuffer, f32) -> std::sync::Arc<egui::Galley> + use<> {
    let font = egui::TextStyle::Monospace.resolve(ui.style());
    let dark = ui.visuals().dark_mode;
    move |ui, text, wrap| {
        let _ = wrap; // never wrap: keeps one galley row per line for guides/folds
        let job = yaml_job(text.as_str(), &font, dark, f32::INFINITY);
        ui.fonts_mut(|f| f.layout_job(job))
    }
}

fn indent_of(l: &str) -> usize {
    l.len() - l.trim_start().len()
}

/// If line `i` starts a block (next non-blank line is indented deeper),
/// returns the exclusive end line of that block.
pub fn fold_end(lines: &[&str], i: usize) -> Option<usize> {
    let ind = indent_of(lines[i]);
    let next = lines[i + 1..].iter().find(|l| !l.trim().is_empty())?;
    if indent_of(next) <= ind {
        return None;
    }
    let mut end = i + 1;
    for (j, l) in lines.iter().enumerate().skip(i + 1) {
        if l.trim().is_empty() || indent_of(l) > ind {
            end = j + 1;
        } else {
            break;
        }
    }
    // don't swallow trailing blank lines
    while end > i + 1 && lines[end - 1].trim().is_empty() {
        end -= 1;
    }
    Some(end)
}

/// YAML view with indent guides and fold arrows. Folded blocks make the view
/// read-only (the displayed text no longer equals the buffer).
/// Returns true if the buffer was edited.
pub fn code_view(
    ui: &mut egui::Ui,
    text: &mut String,
    editable: bool,
    folds: &mut std::collections::BTreeSet<usize>,
) -> bool {
    let orig_lines: Vec<&str> = text.split('\n').collect();
    // Build the displayed text; `map[d]` = original line index of display line d.
    let mut display = String::new();
    let mut map: Vec<usize> = vec![];
    let mut i = 0;
    while i < orig_lines.len() {
        let mut line = orig_lines[i].to_string();
        let mut next = i + 1;
        if let Some(end) = folds.contains(&i).then(|| fold_end(&orig_lines, i)).flatten() {
            let _ = write!(line, "  … ({} lines)", end - i - 1);
            next = end;
        }
        if !map.is_empty() {
            display.push('\n');
        }
        display.push_str(&line);
        map.push(i);
        i = next;
    }
    let foldable: Vec<bool> = (0..orig_lines.len()).map(|i| fold_end(&orig_lines, i).is_some()).collect();
    let folded = !folds.is_empty();

    if folded {
        ui.horizontal(|ui| {
            ui.colored_label(
                egui::Color32::from_rgb(230, 170, 60),
                "Sections are collapsed - read-only until expanded.",
            );
            if ui.small_button("Expand all").clicked() {
                folds.clear();
            }
        });
    }

    let font = egui::TextStyle::Monospace.resolve(ui.style());
    let char_w = ui.fonts_mut(|f| f.glyph_width(&font, ' '));
    let gutter_w = 18.0;
    let mut changed = false;
    let mut toggles: Vec<usize> = vec![];

    ui.horizontal_top(|ui| {
        let (gutter, _) = ui.allocate_exact_size(egui::vec2(gutter_w, 0.0), egui::Sense::hover());
        let mut lay = layouter(ui);
        let out = if editable && !folded {
            egui::TextEdit::multiline(text)
                .code_editor()
                .layouter(&mut lay)
                .desired_rows(30)
                .desired_width(f32::INFINITY)
                .show(ui)
        } else {
            let mut ro: &str = &display;
            egui::TextEdit::multiline(&mut ro)
                .code_editor()
                .layouter(&mut lay)
                .desired_rows(30)
                .desired_width(f32::INFINITY)
                .show(ui)
        };
        changed = out.response.changed();
        if changed {
            return; // lines shifted; redraw guides next frame
        }
        let dark = ui.visuals().dark_mode;
        let guide = if dark {
            Color32::from_white_alpha(28)
        } else {
            Color32::from_black_alpha(35)
        };
        let painter = ui.painter();
        let shown: Vec<&str> = display.split('\n').collect();
        for (d, row) in out.galley.rows.iter().enumerate().take(shown.len()) {
            let line = shown[d];
            let top = out.galley_pos.y + row.pos.y;
            let bottom = top + row.size.y;
            // indent guides every 2 columns
            let ind = indent_of(line);
            for col in (0..ind).step_by(2) {
                let x = out.galley_pos.x + char_w * col as f32 + 1.0;
                painter.line_segment([egui::pos2(x, top), egui::pos2(x, bottom)], (1.0, guide));
            }
            // fold arrow
            let o = map[d];
            if foldable[o] {
                let rect = egui::Rect::from_min_size(
                    egui::pos2(gutter.left(), top),
                    egui::vec2(gutter_w, bottom - top),
                );
                let id = ui.make_persistent_id(("fold", o));
                let resp = ui.interact(rect, id, egui::Sense::click());
                let glyph = if folds.contains(&o) { "▸" } else { "▾" };
                let col = if resp.hovered() { ui.visuals().strong_text_color() } else { ui.visuals().weak_text_color() };
                painter.text(rect.center(), egui::Align2::CENTER_CENTER, glyph, font.clone(), col);
                if resp.clicked() {
                    toggles.push(o);
                }
            }
        }
    });
    for t in toggles {
        if !folds.remove(&t) {
            folds.insert(t);
        }
        ui.ctx().request_repaint();
    }
    changed
}
