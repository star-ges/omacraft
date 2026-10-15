//! Export an instance as an instance zip or a Modrinth pack, choosing which
//! parts of the game folder go in.

use std::path::PathBuf;

use egui::{RichText, Ui};

use super::nav::Nav;
use super::widgets as w;
use super::{App, ToastKind};
use crate::export::{Entry, Format, Request};
use crate::instance::Instance;
use crate::task::Promise;

pub struct ExportDialog {
    inst: Instance,
    format: Format,
    version: String,
    entries: Option<Vec<(Entry, bool)>>,
    entries_p: Option<Promise<Vec<Entry>>>,
    cursor: usize,
    save_p: Option<Promise<Option<PathBuf>>>,
}

impl ExportDialog {
    pub fn new(app: &App, ctx: &egui::Context, inst: Instance) -> Self {
        let i = inst.clone();
        let entries_p = Promise::spawn(&app.rt, ctx, async move { Ok(tokio::task::spawn_blocking(move || crate::export::entries(&i)).await?) });
        Self { inst, format: Format::Mrpack, version: "1.0.0".into(), entries: None, entries_p: Some(entries_p), cursor: 0, save_p: None }
    }
}

/// Returns the export to start, or `Err(())` to close without one.
pub fn show(app: &mut App, s: &mut ExportDialog, ui: &mut Ui) -> Option<Result<Request, ()>> {
    let t = app.t.clone();
    let screen = ui.ctx().content_rect();
    ui.set_width((screen.width() - 120.0).clamp(480.0, 760.0));
    ui.set_height((screen.height() - 120.0).clamp(300.0, 520.0));

    if let Some(r) = s.entries_p.as_ref().and_then(|p| p.take()) {
        s.entries_p = None;
        match r {
            Ok(list) => s.entries = Some(list.into_iter().map(|e| { let on = e.default; (e, on) }).collect()),
            Err(e) => app.toast(ToastKind::Error, format!("Couldn't read the instance: {e:#}")),
        }
    }
    let mut out = None;
    if let Some(r) = s.save_p.as_ref().and_then(|p| p.take()) {
        s.save_p = None;
        match r {
            Ok(Some(mut dest)) => {
                if Format::from_path(&dest) != Some(s.format) {
                    dest.as_mut_os_string().push(format!(".{}", s.format.ext()));
                }
                let include = s.entries.iter().flatten().filter(|(_, on)| *on).map(|(e, _)| e.name.clone()).collect();
                out = Some(Ok(Request { inst: s.inst.clone(), format: s.format, dest, include, version: s.version.trim().to_string() }));
            }
            Ok(None) => {}
            Err(e) => app.toast(ToastKind::Error, format!("File picker failed: {e:#}")),
        }
    }

    ui.horizontal(|ui| {
        w::heading(ui, &t, "export instance");
        ui.add_space(16.0);
        w::segmented(ui, &t, &mut s.format, &[(Format::Mrpack, "modrinth pack"), (Format::Zip, "instance zip")]);
    });
    ui.add_space(10.0);

    let nav = app.modal_nav();
    let key = |n: Nav| nav == Some(n);
    let busy = s.save_p.is_some();
    ui.columns(2, |cols| {
        let ui = &mut cols[0];
        w::section(ui, &t, "include");
        match &mut s.entries {
            None => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new("measuring…").color(t.muted));
                });
            }
            Some(list) if list.is_empty() => {
                ui.label(RichText::new("the game folder is empty").color(t.muted));
            }
            Some(list) => {
                let moved = nav.filter(|n| n.is_move());
                if let Some(n) = moved {
                    s.cursor = n.step(list.len(), Some(s.cursor)).unwrap_or(0);
                }
                s.cursor = s.cursor.min(list.len() - 1);
                if key(Nav::Toggle) {
                    list[s.cursor].1 ^= true;
                }
                egui::ScrollArea::vertical().id_salt("export-entries").max_height(ui.available_height() - 4.0).auto_shrink(false).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    for (i, (e, on)) in list.iter_mut().enumerate() {
                        let glyph = if *on { "\u{f0132}" } else { "\u{f0131}" };
                        let name = if e.is_dir { format!("{}/", e.name) } else { e.name.clone() };
                        let r = w::menu_row(ui, &t, i == s.cursor, glyph, &name, &w::size_label(e.size));
                        if i == s.cursor {
                            w::focus_mark(ui, &t, r.rect);
                            if moved.is_some() {
                                r.scroll_to_me(None);
                            }
                        }
                        if r.clicked() {
                            *on ^= true;
                            s.cursor = i;
                        }
                    }
                });
            }
        }

        let ui = &mut cols[1];
        match s.format {
            Format::Mrpack => {
                w::section(ui, &t, "pack version");
                w::text_input(ui, &mut s.version, "1.0.0", ui.available_width(), false);
                ui.add_space(6.0);
                w::para(
                    ui,
                    RichText::new("Mods and packs that Modrinth hosts are listed as downloads; everything else is bundled in the pack. Opens in Omacraft, Prism and the Modrinth App.")
                        .color(t.muted),
                );
            }
            Format::Zip => {
                ui.add_space(6.0);
                w::para(
                    ui,
                    RichText::new("A full copy of the chosen files, with the instance's settings and icon. Opens in Omacraft, Prism, PolyMC and MultiMC, and works offline.")
                        .color(t.muted),
                );
            }
        }
        if let Some(list) = &s.entries {
            let (n, size) = list.iter().filter(|(_, on)| *on).fold((0, 0), |(n, b), (e, _)| (n + 1, b + e.size));
            ui.add_space(8.0);
            ui.label(RichText::new(format!("{n} of {} items  ·  {} before compression", list.len(), w::size_label(size))).color(t.muted));
        }

        ui.with_layout(egui::Layout::bottom_up(egui::Align::Max), |ui| {
            ui.horizontal(|ui| {
                let ready = s.entries.is_some() && !busy;
                let go = ui.add_enabled_ui(ready, |ui| w::primary_button(ui, &t, "export…")).inner.clicked()
                    || (ready && (key(Nav::Activate) || ui.input(|i| i.key_pressed(egui::Key::Enter))));
                if w::button(ui, "cancel").clicked() {
                    out = Some(Err(()));
                }
                if busy {
                    ui.spinner();
                }
                if go {
                    s.save_p = Some(save_as(app, ui.ctx(), &s.inst.name, s.format));
                }
            });
        });
    });
    out
}

/// Ask where to save, via the XDG portal.
fn save_as(app: &App, ctx: &egui::Context, name: &str, format: Format) -> Promise<Option<PathBuf>> {
    let file = format!("{}.{}", name.replace('/', "-"), format.ext());
    Promise::spawn(&app.rt, ctx, async move {
        let (label, ext) = match format {
            Format::Mrpack => ("Modrinth pack", "mrpack"),
            Format::Zip => ("Instance zip", "zip"),
        };
        let picked = rfd::AsyncFileDialog::new()
            .set_directory(dirs::download_dir().or_else(dirs::home_dir).unwrap_or_default())
            .set_title("Export instance")
            .set_file_name(file)
            .add_filter(label, &[ext])
            .save_file()
            .await;
        Ok(picked.map(|f| f.path().to_path_buf()))
    })
}
