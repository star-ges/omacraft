//! An instance's components (Minecraft + loader), and changing them.

use egui::{RichText, Ui};

use super::nav::{Nav, Zone};
use super::version_picker::VersionPicker;
use super::widgets as w;
use super::App;
use super::{Toast, ToastKind};
use crate::instance::Instance;
use crate::loaders::Loader;
use crate::shares::Kind;
use crate::world_sets;

#[derive(Default)]
pub struct VersionState {
    /// Open picker, for this instance id.
    picker: Option<(String, VersionPicker)>,
    /// A member leaving its world set to change version, waiting on its own copy of the worlds.
    leaving: Option<Leave>,
}

/// The version change [`after_sharing`] saves once the leave is done.
struct Leave {
    id: String,
    set: String,
    mc: String,
    loader: Loader,
}

impl VersionState {
    pub fn picker_open(&self) -> bool {
        self.picker.is_some()
    }

    pub fn close(&mut self) {
        self.picker = None;
    }

    pub fn cycle_loader(&mut self) {
        if let Some((_, p)) = &mut self.picker {
            p.cycle_kind();
        }
    }
}

pub fn show(app: &mut App, ui: &mut Ui, inst: &Instance) {
    let t = app.t.clone();
    if app.version.picker.as_ref().is_some_and(|(id, _)| *id != inst.id) {
        app.version.picker = None;
    }

    w::section(ui, &t, "components");
    w::rich_row(ui, &t, false, w::Lead::Image(crate::icons::source("grass")), "Minecraft", &inst.mc_version, "");
    match &inst.loader {
        Loader::Vanilla => {}
        Loader::Fabric(v) => {
            w::rich_row(ui, &t, false, w::Lead::Image(crate::icons::source("fabricmc")), "Fabric Loader", v, "");
        }
        Loader::Quilt(v) => {
            w::rich_row(ui, &t, false, w::Lead::Image(crate::icons::source("quiltmc")), "Quilt Loader", v, "");
        }
        Loader::Forge(v) => {
            w::rich_row(ui, &t, false, w::Lead::Image(crate::icons::source("gear")), "Forge", v, "");
        }
        Loader::NeoForge(v) => {
            w::rich_row(ui, &t, false, w::Lead::Image(crate::icons::source("neoforged")), "NeoForge", v, "");
        }
    }
    ui.add_space(10.0);

    let main_nav = app.nav_for(Zone::Main);
    let Some((_, picker)) = &mut app.version.picker else {
        ui.horizontal(|ui| {
            let running = app.is_running(&inst.id);
            let r = ui.add_enabled_ui(!running, |ui| w::button(ui, "change version…")).inner;
            if app.focus == Zone::Main {
                w::focus_mark(ui, &t, r.rect);
            }
            if r.clicked() || (!running && main_nav == Some(Nav::Activate)) {
                let p = VersionPicker::new(app.cfg.show_snapshots, Some((&inst.mc_version, &inst.loader)));
                app.version.picker = Some((inst.id.clone(), p));
            }
            if running {
                ui.label(RichText::new("stop the game to change versions").color(t.muted));
            }
        });
        return;
    };

    let mut picker = std::mem::replace(picker, VersionPicker::new(false, None));
    picker.poll(app, ui.ctx());
    let side_nav = app.nav_for(Zone::Side);
    let apply_key = main_nav == Some(Nav::Activate) || side_nav == Some(Nav::Activate);
    let mut close = false;
    let sel = picker.selection();
    let changed = sel.as_ref().is_some_and(|(mc, l)| *mc != inst.mc_version || *l != inst.loader);
    ui.horizontal(|ui| {
        if (ui.add_enabled_ui(changed, |ui| w::primary_button(ui, &t, "apply")).inner.clicked() || (changed && apply_key))
            && let Some((mc, loader)) = sel.clone()
        {
            let mut after = inst.clone();
            after.mc_version = mc.clone();
            after.loader = loader.clone();
            let members: Vec<String> = world_sets::others(&inst.world_set, &inst.id, &app.instances).map(|i| i.name.clone()).collect();
            if !members.is_empty() && world_sets::mismatch(&after, &inst.world_set, &app.instances).is_some() {
                // Everyone in a world set plays the same game: ask what to do.
                app.modal = Some(super::Modal::SetVersion(Box::new(SetVersion { id: inst.id.clone(), name: inst.name.clone(), set: inst.world_set.clone(), members, mc, loader })));
            } else {
                set_version(app, &[inst.id.clone()], &mc, &loader);
            }
            close = true;
        }
        if w::button(ui, "cancel").clicked() {
            close = true;
        }
        if let Some((mc, loader)) = &sel
            && changed
        {
            let text = if *loader == Loader::Vanilla { mc.clone() } else { format!("{mc}  ·  {}", loader.display()) };
            ui.label(RichText::new(format!("→ {text}")).color(t.accent));
        }
    });
    ui.add_space(6.0);
    ui.columns(2, |cols| {
        let focus = app.focus;
        picker.versions_ui(app, &mut cols[0], main_nav, focus == Zone::Main);
        let h = cols[1].available_height() - 60.0;
        picker.loader_ui(app, &mut cols[1], h, side_nav, focus == Zone::Side);
    });
    if close {
        app.version.picker = None;
    } else if let Some((_, slot)) = &mut app.version.picker {
        *slot = picker;
    }
}

/// Save these instances with Minecraft `mc` and `loader`.
fn set_version(app: &mut App, ids: &[String], mc: &str, loader: &Loader) {
    let what = if *loader == Loader::Vanilla { mc.to_string() } else { format!("{mc} with {}", loader.display()) };
    let mut done = Vec::new();
    let mut mods = false;
    for i in app.instances.iter_mut().filter(|i| ids.contains(&i.id)) {
        let mc_changed = i.mc_version != mc;
        i.mc_version = mc.to_string();
        i.loader = loader.clone();
        match i.save() {
            Ok(()) => {
                done.push(i.name.clone());
                mods |= mc_changed && !i.mods().is_empty();
            }
            Err(e) => app.toasts.push(Toast::error(format!("Save failed for {}: {e}", i.name))),
        }
    }
    if !done.is_empty() {
        app.toasts.push(Toast::info(format!("{} now {} {what}", done.join(", "), if done.len() == 1 { "uses" } else { "use" })));
    }
    if mods {
        app.toasts.push(Toast::info("Mods are built for one Minecraft version; check yours still match."));
    }
}

/// Once shared folders have moved: save the version change of a member that left its
/// world set, if it really left.
pub fn after_sharing(app: &mut App) {
    let Some(l) = app.version.leaving.take() else { return };
    let Some(i) = app.instances.iter_mut().find(|i| i.id == l.id) else { return };
    if leave_done(i, &l.set) {
        set_version(app, std::slice::from_ref(&l.id), &l.mc, &l.loader);
    } else {
        let name = i.name.clone();
        app.toast(ToastKind::Error, format!("{name} couldn't get its own copy of “{}”'s worlds, so it stays in the set on its old version", l.set));
    }
}

/// Whether `inst` has left world set `set` on disk: its saves no longer link into a
/// world set. If not, it's put back in `set`.
fn leave_done(inst: &mut Instance, set: &str) -> bool {
    if crate::shares::linked_world_set(inst).is_none() {
        return true;
    }
    inst.world_set = set.to_string();
    false
}

/// A world set member's version change, waiting on what to do with the rest of the set.
pub struct SetVersion {
    id: String,
    name: String,
    set: String,
    /// The other members' names.
    members: Vec<String>,
    mc: String,
    loader: Loader,
}

/// The dialog for [`SetVersion`]. Returns true once it's done.
pub fn set_version_dialog(app: &mut App, s: &SetVersion, ui: &mut Ui) -> bool {
    let t = app.t.clone();
    let activate = app.modal_nav() == Some(Nav::Activate);
    ui.set_width(460.0);
    w::heading(ui, &t, "change the world set's version?");
    ui.add_space(8.0);
    let what = if s.loader == Loader::Vanilla { s.mc.clone() } else { format!("{} with {}", s.mc, s.loader.display()) };
    w::para(
        ui,
        format!(
            "{} shares the worlds of “{}” with {}, and every instance in a world set plays the same Minecraft version and loader. Change them all to {what}, or have {} leave the set with its own copy of the worlds?",
            s.name,
            s.set,
            s.members.join(", "),
            s.name
        ),
    );
    ui.add_space(14.0);
    let mut done = false;
    ui.horizontal(|ui| {
        let all = format!("change all {}", s.members.len() + 1);
        if w::primary_button(ui, &t, &all).clicked() || activate {
            let ids: Vec<String> = app.instances.iter().filter(|i| i.id == s.id || i.world_set == s.set).map(|i| i.id.clone()).collect();
            let busy = ids.iter().find_map(|id| {
                let name = app.instances.iter().find(|i| &i.id == id).map(|i| i.name.clone()).unwrap_or_default();
                app.instance_busy(id).map(|why| format!("{name}: {why}"))
            });
            match busy {
                Some(why) => app.toast(ToastKind::Error, format!("Can't change the set's version ({why})")),
                None => set_version(app, &ids, &s.mc, &s.loader),
            }
            done = true;
        }
        if w::button(ui, "leave the set").clicked() {
            if let Some(why) = app.instance_busy(&s.id).map(String::from).or_else(|| app.set_busy(Kind::Worlds, &s.set, &s.id)) {
                app.toast(ToastKind::Error, format!("Can't leave “{}” while {why}", s.set));
            } else if let Some(i) = app.instances.iter_mut().find(|i| i.id == s.id) {
                // Nothing is saved until the copy is made: a failed leave keeps it in the
                // set on its old version, never the new version on the set's worlds.
                i.world_set.clear();
                let i = i.clone();
                app.version.leaving = Some(Leave { id: s.id.clone(), set: s.set.clone(), mc: s.mc.clone(), loader: s.loader.clone() });
                app.start_sharing(ui.ctx(), vec![i]);
            }
            done = true;
        }
        if w::button(ui, "cancel").clicked() {
            done = true;
        }
    });
    done
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_leave_keeps_the_member_in_its_set() {
        let set = format!("set {}", uuid::Uuid::new_v4().simple());
        let mut a = Instance::new("leaver", "1.21.1", Loader::Vanilla);
        a.world_set = set.clone();
        a.assign_id().unwrap();
        crate::shares::apply(&a).unwrap();
        // The leave's copy didn't happen: saves still link into the set.
        a.world_set.clear();
        assert!(!leave_done(&mut a, &set));
        assert_eq!(a.world_set, set, "put back in the set it couldn't leave");
        // Once it has its own saves, the leave counts and the version can change.
        a.world_set.clear();
        crate::shares::apply(&a).unwrap();
        assert!(leave_done(&mut a, &set));
        assert!(a.world_set.is_empty());
        std::fs::remove_dir_all(a.dir()).unwrap();
        crate::shares::delete_set(Kind::Worlds, &set).unwrap();
    }
}

