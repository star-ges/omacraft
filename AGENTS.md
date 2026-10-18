# Omacraft

A Minecraft launcher in Rust (egui/eframe 0.36), modeled on Prism Launcher and drawn in
Omarchy's design language. It themes itself live from the active Omarchy theme. The README
covers features and file layout for users; this file is for working on the code.

The name is **Omacraft**, with a capital O only, like Omacalc. Use lowercase `omacraft` for the
binary, app-id and paths. Never write "OmaCraft".

## Build, run, test

```sh
cargo build --release
install -Dm755 target/release/omacraft ~/.local/bin/omacraft    # check `pgrep omacraft` first
cargo test                                                      # unit tests
OMACRAFT_FIXTURE=<copy of a .minecraft> cargo test              # + NBT round-trips on real saves
cargo test -- --ignored                                         # + network tests (Modrinth)
omacraft list | import <zip|mrpack|folder|url> | export <inst> <file> | reclaim | theme   # CLI, handy for checks
```

- **Style:** don't run `cargo fmt`; the code uses longer lines than rustfmt's defaults. Match the
  surrounding style.
- **Warnings:** keep the build warning-free.
- **Vendored crate:** `vendor/smithay-clipboard` is smithay-clipboard 0.7.3 patched (`[patch.crates-io]`) to take
  file drops: winit has no drag and drop on Wayland, and Hyprland sends drops only to a client's first data
  device, which is the clipboard's. The patch is `src/drop.rs` plus the `DataDeviceHandler` drop methods in
  `src/state.rs`; when egui moves to a new smithay-clipboard, re-apply it or drop it if winit gains Wayland drops.
- **Binary size:** the release profile strips symbols, and `zip` is trimmed to Deflate (flate2/zlib-rs),
  Deflate64 and `time`. Keep new dependencies' default features in check. LTO is off on purpose: full LTO
  with codegen-units=1 saved 7 MB (35 to 28) but tripled build times.

## Code map

| Module | What it does |
|---|---|
| `main.rs`, `cli.rs` | tokio runtime; the CLI if there are args, otherwise eframe (`with_decorations(false)`) |
| `paths.rs`, `config.rs` | `~/.local/share/omacraft`, `~/.config/omacraft/config.toml` (incl. `[keys]`, `collapsed_groups`, `msa_client_id`) |
| `theme.rs` | Reads Omarchy `colors.toml` + `shell.toml`; fonts via fontconfig; live watcher; Tokyo Night fallback |
| `mojang.rs`, `loaders.rs`, `java.rs`, `launch.rs` | Version JSON + rules, loader profiles (Fabric/Quilt meta; Forge/NeoForge from Prism's meta: launchwrapper `+tweakers` for 1.6–1.12, ForgeWrapper + `mavenFiles` for 1.13+), Mojang Java runtimes, prepare + spawn (incl. QuickPlay) |
| `auth.rs` | MSA device code → XBL → XSTS → Minecraft; offline accounts |
| `skins.rs` | Profile skins/capes (upload, classic/slim, cape, reset; refresh + retry on 401), textures cached by hash in `cache/textures/`, Steve/Alex default by UUID. Skins always go up as multipart PNGs: the API's firewall 403s the JSON `{"url": …}` form, so classic/slim re-uploads the cached texture |
| `instance.rs`, `groups.rs` | `instance.toml` per instance; `groups.toml` (order, colour by palette name) |
| `modrinth.rs`, `content_meta.rs` | Search/install by `ContentKind`, mod updates (`check_updates`/`update` via `version_files/update`); name/icon from jar or pack, else Modrinth hash lookup |
| `import.rs` | Prism/MultiMC folder or zip, `.mrpack` file or URL, auto-detects other launchers |
| `export.rs` | Instance zip (MultiMC format) or `.mrpack` (Modrinth-hosted files via `content_meta::hosted`, rest in `overrides/`) |
| `saves.rs` | `level.dat` and `servers.dat` via fastnbt `Value` (round-trips unknown fields) |
| `keymap.rs` | Every action, default chords, scopes, parse/label, conflict detection |
| `shares.rs` | Shared folders, by `Kind`: share sets (`shared/sets/<name>/{resourcepacks,shaderpacks,schematics}`, `share_set` in instance.toml) and world sets (`shared/worlds/<name>/saves`, `world_set`); `apply` turns an instance's folders into relative links or back into copies |
| `world_sets.rs`, `backups.rs` | World sets' rules: one version + loader per set, `omacraft.toml` play records in each world, the pre-launch mod check; reflink snapshots of worlds (`backups/worlds/`) and of an instance's `mods` + `config` (`backups/instances/`), restore, rollback, pruning |
| `store.rs` | Content store: Modrinth installs and `.mrpack` files go through `cache/content/<sha1>` and are reflinked into instances; `reclaim` merges duplicates (`FIDEDUPERANGE`) and prunes the store |
| `icons.rs`, `fsutil.rs`, `net.rs`, `task.rs` | Instance icons, reflink copy, HTTP + downloads, background `Task`/`Promise` |
| `ui/mod.rs` | `App`, top/status bars, modals, key routing, launching, imports |
| `ui/nav.rs` | Focus zones, `Nav`, hints bar and `?` help, both built from the keymap |
| `ui/instances.rs` | Sidebar (groups, drag-and-drop, context menu) + instance header + tab dispatch |
| `vanilla_options.rs`, `shader_config.rs` | The configs tab's other parts: `options.txt` as named, sectioned fields (`KNOWN` table; unknown keys under "other"); shader pack options read from the pack (`#define`s, `const`s with `// [values]`, `shaders.properties` screens and sliders, `lang/en_us.lang` names), saved to `shaderpacks/<pack>.txt` |
| `mod_config.rs`, `ui/config.rs` | Configs tab (minecraft · mods · shaders): `config/` files grouped by mod (folding, kept in `<data>/config-view.toml`); TOML (`toml_edit` spans) and JSON/JSON5 (own parser) as fields with comment hints (range, allowed values, default); edits splice only the value's bytes; conflict-checked saves |
| `mod_sets.rs`, `bisect.rs`, `ui/mod_sets.rs` | Mod sets (`mod_sets.toml`: named on/off arrangements keyed by jar mod id, else versionless file name); the search for the breaking mod (`bisect.toml`: dependency-closed halving, a confirming step, exact restore); the mods tab's set line, sets dialog, search dialogs and banner |
| `crash.rs` | Crash analysis: suspects from crash reports and logs (loader errors, unmet dependencies, mixins, entrypoints, Suspected Mods, stack frames matched to jars by package), non-mod causes; jar facts cached by path/size/mtime |
| `ui/{version,content,worlds,servers,screenshots,logs}.rs` | Per-instance tabs (content also has mod updates and the rollback dialogs; worlds has the backups pane; log history has the crash analysis panel) |
| `ui/{new_instance,export,version_picker,icon_picker,group_manager,settings,accounts}.rs` | Dialogs and pages |
| `ui/skin_view.rs` | 3D player model: one coloured quad per texel, rotated, projected and painted back to front as an `egui::Mesh` |
| `ui/widgets.rs` | The widget kit, listed under Patterns below |

## Patterns to follow

- **Omarchy look:** square corners, monospace text, lowercase UI labels, no shadows.
  - Colours come only from `Theme` tokens (`t.accent`, `t.fg_a(alpha)`, `t.named("blue")`…).
  - Modals and toasts are bordered with `t.border_active`.
  - The single filled button on a screen is `w::primary_button`.
- **Text clips by default:** the global wrap mode is `Extend`.
  - Paragraphs that should wrap use `w::para`, which also avoids the justified spacing inside `ui.columns`.
  - Single lines that may be long use `Label::truncate()`.
  - To keep text from overlapping buttons, lay the buttons out first (right-to-left), then the text in the space left.
- **Rows:** use `w::menu_row` / `lead_row` / `drag_row` / `rich_row` / `rich_row_dim`.
  - Mark the selection in the focused pane with `w::focus_mark`.
  - Scroll to the selection only after a keyboard move, never every frame.
- **Keyboard:** the screen has zones (`Zone::List/Main/Side`).
  - `App::keys` turns key presses into a `Nav` through the `Keymap`.
  - Each pane takes it with `app.nav_for(zone)` and acts on it like its buttons do (`|| key(Nav::X)`), so mouse and keyboard can't drift apart.
  - Dialogs use `app.modal_nav()`. While one is open `nav_for` returns nothing, so the panes drawn under it
    can't take its keys.
  - New actions go in `keymap.rs` `actions!` (with a scope), plus `nav::hints`. The `?` hint always comes first.
  - Never hard-code a key.
- **Background work:** `Promise::spawn(&app.rt, ctx, fut)` and poll `take()` each frame.
  - Long jobs carry a `Task` so the status bar shows progress.
- **Destructive actions:** go through `Modal::Confirm` with a `ConfirmAction`.
- **Icons in UI text:** use Nerd Font glyphs.
  - A symbols-only Nerd Font is bundled as a fallback (`assets/fonts`), so they render anywhere.
  - Avoid Unicode symbols that plain fonts lack: ⏎ ▶ ▸ ▾ were swapped out for this reason.
  - Write private-use glyphs as `\u{f44b}` escapes; they can get lost when pasted through tools.
- **Theme fallback:** off Omarchy the theme falls back to Omarchy's stock Tokyo Night. The test
  `fallback_is_omarchy_tokyo_night` keeps `Theme::default()` identical to it.

## Behaviour rules

Each is covered by a regression test. Keep them when changing these areas, and add a regression
test (and a rule here) with any fix that sets a new one.

- **Folder imports:** `fsutil::copy_tree` copies *through* every symlink, inside the instance or not
  (shared `saves`/`mods` folders come across as independent copies). Broken links and links back up the tree
  are skipped and reported as import warnings.
- **Tests:** under `cfg(test)`, `paths::data_dir`/`config_dir` live in `/tmp/omacraft-test-<pid>`, so no
  test touches real data. Put test scratch under `paths::test_root()`.
- **Pack imports:** Modrinth version URLs (`/modpack/<slug>/version/<id or number>`) install that
  version or fail; `.mrpack` files are checked before the instance exists (HTTPS downloads, a valid SHA-1 per file).
- **Mod updates:** `modrinth::check_updates` offers a file only if Modrinth's newest compatible build is newer *by date*
  than the installed one (a hand-installed beta isn't "updated" to an older release). Nothing is written until each
  checked file is found again by SHA-1 (the very path checked if it still matches, else the same file enabled or
  disabled since; a removed or replaced file stops the whole update), no two entries share a file or a new name,
  the game still matches the one the list was checked for (`modrinth::game_key`), and the new versions'
  required dependencies are resolved by `resolve`, the pinned resolver installs use. Then `back_up_for_update` backs
  up every world no snapshot covers (`backups::cover_worlds`, which lists `saves` strictly: an unreadable folder
  fails rather than looking empty; any failure but a world open in a game stops the update), and after that `mods` + `config`, so a rollback's cutoff comes after the update's world copies. An update
  keeps a disabled mod disabled. The mods tab offers only entries still valid for the files and game
  (`current_updates`) and doesn't switch or remove mods while a mod job runs. Tests:
  `backups::tests::an_unreadable_saves_folder_fails_the_update_backups`,
  `modrinth::tests::{updates_only_go_forward, an_update_stops_if_a_world_cant_be_backed_up,
  an_update_follows_the_file_as_it_is_now, update_dependencies_keep_their_pins, finds_and_applies_updates}` (the last
  needs network), `ui::content::tests::found_updates_go_stale_with_the_game_or_the_file`.
- **Job exclusion:** GUI jobs are keyed by what they touch. Mod updates and rollbacks are tracked per job
  (`ContentState::mod_jobs`) until their result is taken. World jobs are keyed by the folder their worlds really live
  in (`worlds::saves_key`, followed through links), so instances sharing worlds, by setting or by a leftover link, wait
  for each other's backups, restores, duplicates and update backups (`worlds::jobs_on`). A launch waits while the
  instance's mods are being swapped, its worlds are copied or restored through any instance, or shared folders are
  moving (`worlds::launch_blocked`). Mods aren't switched, removed or installed while a game starts
  (`Session::starting`, until Java has a pid), since its play records are read then. Tests: `ui::worlds::tests::jobs_on_shared_worlds_block_every_member`,
  `ui::content::tests::every_mod_job_stays_tracked_until_it_ends`.
- **Content installs:** `modrinth::plan` works out the whole set before downloading. Pins beat the
  newest build, installed files count by Modrinth project (hash lookup) as well as filename, and required
  dependencies that can't be met come back in `Installed::missing` (error toast, non-zero CLI exit).
- **Installed marks:** the content browser's "installed" comes from the current instance's files as
  they are now (`MetaStore::project_ids(files)`); metadata is keyed by path, size and mtime, so a replaced file
  is looked up again. Install jobs are keyed by (instance, project).
- **Frame work:** nothing a pane draws runs subprocesses or reads files in bulk. Settings shows `app.fonts`
  (re-resolved only when the theme watcher sees fontconfig change); worlds are listed and duplicated on a
  `Promise`/`Task`, and the old list stays up while a refresh runs.
- **Decompression caps:** compressed metadata (`instance.cfg`, `mmc-pack.json`, `modrinth.index.json`, exported
  icons, mod metadata and icons, `level.dat`) is read through `fsutil::read_capped`, which fails past
  `METADATA_LIMIT` (16 MiB). Bulk extraction (`import::extract`) has no total-bytes cap yet.
- **Export round trip:** `export::tests::exports_import_back` exports a tiny instance as a zip and an
  `.mrpack` and imports both back offline. Files (incl. empty folders) must match, minus personal ones. The zip
  must keep everything `instance.cfg` carries (not the Java path) plus the custom icon; the mrpack keeps name,
  version, loader and notes (as `summary`). Add new portable metadata to that test.
- **Content store:** instances never link to `cache/content`: they get reflinked copies (or the download is
  moved in on filesystems without reflinks), so every instance file is independent. `reclaim` merges only
  through the kernel's dedupe ioctl, which compares the bytes first, and deletes only store entries no instance
  file matches. Tests: `store::tests` (run with `TMPDIR` on btrfs to cover the reflink path, since `/tmp` is tmpfs).
- **Shared folders:** sharing turns the instance's folder into a relative link into `shared/`, moving its files in
  (identical files kept once, other clashes renamed `name (<instance id>).ext`, folders merged except unzipped packs).
  Links inside a folder being shared are first replaced by copies of what they point to (relative ones would point
  elsewhere from `shared/`), like folder imports; a broken or looping one stops the join before anything moves.
  Unsharing a set folder replaces the link with a full copy; moving between sets only relinks. `shares::apply` is idempotent and
  runs at every launch, so new, imported or running-at-the-time instances catch up. Deleting an instance removes
  only its links. Export follows top-level share links only; reclaim counts each shared file once. A share link is
  exactly `../../../shared/sets/<set>/<same folder>` (or that absolute path) *and* resolves there
  (`shares::is_share_link`): anything else, like `shared/../x` or a set folder swapped for a link, is not followed by
  export, and `apply` won't move files into or copy them out of it. Leaving a set copies into `.<folder>.copying`, renames it
  `.<folder>.copied` once whole and swaps it in (`RENAME_EXCHANGE`, else remove + rename); `apply` first recovers
  any of those states, so a crash at any step is finished (or a half copy discarded) by the next launch. `apply` and
  `delete_set` hold an `flock` on `shared/.lock` (any process), and files move in with `fsutil::rename_noreplace`,
  taking the next clash name if one appears meanwhile, so nothing is ever moved over another file. A set counts as
  in use while any instance's folder still links into it (`shares::sets_in_use`), not just by `share_set`, so a
  leave that failed halfway keeps it from being deleted until the next `apply` finishes. Moving shared folders and
  reading or writing them exclude each other in the GUI: no export or pack install starts while `app.sharing` runs,
  and a share set change waits for exports and installs in the instance or any member of the old or new set
  (`App::set_busy`, members from `instances::set_members`). Tests:
  `shares::tests` (incl. `a_failed_leave_keeps_the_set_from_being_deleted`, `only_links_that_really_lead_into_a_set_count`,
  `an_interrupted_leave_is_finished_by_the_next_apply`, `moving_in_locks_out_other_processes_and_never_replaces`), `shares::tests::links_inside_a_shared_folder_become_copies`, `ui::instances::tests::set_members_count_instances_still_linked_in`, `export::tests::shared_folders_export_like_normal_ones` (exports are self-contained: real files,
  mods included).
- **World sets:** a world set shares `saves` through the same `shares::apply` as share sets (worlds clash whole,
  never merge). Every member plays the same Minecraft version and loader kind (`world_sets::same_game`; the loader's
  own version may differ): joining a set that plays something else is refused, and changing one member's version
  asks to change the whole set or leave it (`Modal::SetVersion`). Changes to a world set also wait while another
  member runs or duplicates a world (a leave would copy a world mid-write). Leaving a set to change version saves the
  new version only once the instance's saves no longer link into the set (`version::after_sharing`); a failed leave
  keeps it in the set on its old version, and a launch refuses while saves link into a world set the instance isn't in
  (`shares::check_world_link`). When a game exits, each world it played gets an `omacraft.toml` of what the game
  started with (`world_sets::Loadout`, read just before Java is spawned: instance, version, loader, and every mod the enabled jars
  declare, from `world_sets::mod_inventory`: all of Forge's `[[mods]]` and `mcmod.info`'s entries, and nested jars
  (Fabric/Quilt `jars`, Forge jarjar, 4 deep, capped), unlike `content_meta`'s one-name-per-file display metadata).
  A world counts as played if this game's pid held its `session.lock` (polled with `F_GETLK` every 3 s; Java's
  `FileChannel.tryLock` is a POSIX lock) and no other game was seen holding it afterwards, or, never seen locked by
  another game, its `level.dat` changed during the run. Each play is dated by its last sign (lock last seen held, or
  `level.dat`'s mtime), never the exit; records are written one game at a time, and a record of a later play is
  never replaced by an earlier one. Before any instance
  launches (world set or not: a mod rollback leaves the same gap), any world last played with a mod it lacks, a newer mod version or a newer Minecraft gets a log warning
  and a snapshot first, unless it's open in another game or a snapshot taken since its last play covers it. Never
  blocks the launch. Tests: `world_sets::tests` (incl. `the_inventory_lists_every_mod_a_jar_declares`,
  `a_nested_mod_going_back_counts_as_drift`, `records_name_the_mods_the_game_started_with`,
  `a_world_another_game_played_after_this_one_is_not_this_ones`), `backups::tests`,
  `shares::tests::{world_sets_share_saves_apart_from_share_sets, a_failed_world_set_leave_blocks_launch}`,
  `ui::version::tests::a_failed_leave_keeps_the_member_in_its_set`.
- **Backups:** snapshots are reflink copies made under a dot-name and renamed in whole, with a `<stamp>.toml`
  (`auto` marks the prunable ones). When a game exits, each world it played gets an automatic snapshot, the oldest
  pruned past `backups_keep` (0 = off); snapshots by hand, by the pre-launch check and before a restore are never
  pruned. Restoring a world or rolling back mods snapshots what's there first, stages the copy beside the snapshots
  (never in `saves/`, where the game would list it) and swaps it in (`fsutil::swap_in`); a world open in a game is
  refused (checked again just before the swap). Each shelf (a world's or an instance's snapshots) has an `flock`
  beside it (`.<shelf>.lock`), held through every snapshot, restore, delete and prune by any process, so jobs from
  different world set members queue instead of clearing each other's stages; `.partial` stages are cleared only under
  it, and deleting from the UI gives up after 200 ms. A lock file is only ever deleted by its holder, and
  `fsutil::lock` checks after locking that the file is still the one at its path, so nobody holds a lock on a deleted
  one. `swap_in` journals the move (`<stage>.swap`, the copy known by
  inode) and, without `RENAME_EXCHANGE`, renames the old folder aside instead of deleting it. `backups::recover`
  finishes any swap cut short before launch: it waits for jobs holding a shelf, and if any shelf can't be put right
  the game isn't started. The worlds tab runs a best-effort pass (`recover_worlds`) before listing. Worlds that are gone but have
  snapshots stay listed. After a mod rollback, each world last played with a mod it took away or made older is
  offered for restore, newest snapshot from before the rolled-back update preselected; if none predates it, none is.
  Deleting an instance deletes its backups (a world set's stay with the set): `backups::lock_instance` takes every
  shelf's lock first, and the delete is refused while a job anywhere holds one. Tests: `backups::tests` (incl.
  `jobs_on_one_shelf_wait_for_each_other`, `an_interrupted_restore_is_finished_before_launch`,
  `launch_recovery_waits_for_a_busy_shelf_and_fails_loudly`, `deleting_an_instance_waits_for_its_backup_jobs`),
  `fsutil::tests::{swap_in_finishes_after_a_crash_at_any_step, locks_exclude_other_processes,
  a_lock_file_deleted_by_its_holder_still_excludes}`,
  `ui::content::tests::a_rollback_offers_the_worlds_played_with_newer_mods`, `ui::worlds::tests`.
- **Config editor:** an edit replaces only the value's own bytes (`mod_config::set`), so comments, layout and key
  order survive. Never re-serialize a config file. `mod_config::save` refuses when the file on disk isn't what was
  loaded (the game rewrites configs), unless the user chooses to keep their changes. Only files that resolve inside
  `config/` open (`mod_config::resolve`), up to `MAX_OPEN` (1 MiB). Leaving a file, tab, instance or the app with
  unsaved changes asks first (`config::guard`), and play refuses while the instance has unsaved config changes.
  File groups and settings headers start folded and are remembered open (`config-view.toml`, UI state only: never
  inside an instance). Forge's lone `#.` comment lines aren't descriptions (`forge_spacer_dots_are_not_descriptions`).
  `options.txt` keeps each value as written (quoted stays quoted, a choice like graphics `1` stays a number).
  Shader packs are only read, zipped or not (entries with `..` skipped, size-capped); the one file written is the
  pack's settings file, which holds only options changed from the pack's defaults (Iris's own rule), and a switch
  counts as an option only if the code tests it (`#ifdef`/`defined`). A profile is one more field (`<profile>`):
  choosing it sets only the options it names (following `profile.OTHER`), and it reads "custom" when the options
  match no profile, as in the game. "restore defaults" puts every field with a known default back (vanilla defaults
  are in `KNOWN`/`BINDINGS`), as an unsaved edit. Key bindings are set by pressing the key (`config::capture` takes
  the next key, mouse button or newly held modifier; esc cancels), written in GLFW's names (`key.keyboard.w`).
  "use this pack" / "turn shaders off" change only `shaderPack`/`enableShaders` in Iris's `config/iris.properties`
  (Java-escaped, as Iris writes it) or `shaderPack` in OptiFine's `optionsshaders.txt`, and not while the game runs
  (it saves its own choice on quit). Tests: `vanilla_options::tests`, `shader_config::tests` (incl.
  `profiles_set_their_options_and_are_recognised`, `using_a_pack_changes_only_its_lines`).
  Widgets must not change a value just by drawing it: sliders use `SliderClamping::Edits`, `DragValue` uses
  `clamp_existing_to_range(false)`, and nothing rounds. Tests: `mod_config::tests` (incl.
  `toml_edits_round_trip_byte_for_byte`, `json5_edits_round_trip_byte_for_byte`, `only_files_inside_config_open`,
  `a_file_changed_on_disk_is_not_overwritten`), `ui::config::tests`.
- **Mod sets:** live in `mod_sets.toml` beside `instance.toml`, never in it, and aren't exported. A set names mods by
  `mod_sets::key`: the jar's own id, else `file:` and the file name without its version, so updates keep membership.
  Switching only renames (`.disabled`), runs as a mod job, and is refused while the game starts or runs, mods install
  or update, or a search has them. A mod switched on that the active set hasn't seen joins it (`ModSets::absorb`);
  mods a set has never seen are off after a switch, and the mods tab offers to add them. The sets dialog edits a set's mods as a checklist without touching the mods folder (`ModSets::set_mods`); ticking a mod ticks what it requires (`mod_sets::with_needs`), and an unticked mod a ticked one requires is flagged. Tests: `mod_sets::tests`
  (incl. `switching_sets_is_exact`, `membership_survives_an_update_rename`,
  `new_mods_join_the_active_set_and_stay_off_elsewhere`, `sets_are_edited_without_touching_the_mods`).
- **Finding the breaking mod:** `bisect.toml` keeps the exact original on/off state (and each file's SHA-1), and every
  end (stop, keep, turn it off) restores it before anything else. Every new arrangement (first step, next step, ending)
  is saved with `applied = false` *before* any file is renamed, then switched (idempotent), then marked applied; answers
  are refused unless the files are exactly the step's, and `bisect::recover` (run before every launch; an error stops the
  game, and the banner's "try again") finishes a switch cut short or undoes one made by hand. The record is deleted only
  once every original is found and as it should be: a file renamed since is found by its SHA-1 (the only unclaimed file
  with it), one that can't be found keeps the record (`unresolved`, shown in the banner with "forget the search"). Twins
  (`x.jar` and `x.jar.disabled`) are refused at the start. Every operation holds `.bisect.lock`, so a stop and an
  answer never interleave. Each step's mods are closed over required dependencies (`Bisect::closure`; `bisect::jar_links` counts
  the ids and requirements of nested jars, as deep as `world_sets::mod_inventory` follows them, for the outer file, and
  the sets checklist uses the same links),
  so no mod runs without what it needs; "turn it off" also disables whatever needs the culprit. Kept mods bring
  everything they need into the fixed baseline (`keep` is their closure), which is never a suspect, so the confirming
  step never turns a kept mod off; a check that did (an older record) doesn't confirm. A single suspect is
  only "found" after a step with everything but it on. While a search exists, the mods tab doesn't toggle, remove,
  install, update, roll back or switch sets, and says so once (the banner); nor does the crash panel, whose toggles go
  through the same check (`ui::mod_sets::ModsBusy::by_hand`, which also refuses before the search's file has been read).
  The search's own actions (answer, stop, finish, try again, forget) run only as mod jobs, refused while a game starts or
  runs or any mod job or install runs (`ModsBusy::search_action`), checked when drawn and again when acted on. A crash analysis starts a search with its
  suspects first (`mod_sets::open_bisect`). Answers are only sent from the question dialog, where a click selects and "answer" sends, so a slip can be corrected. Tests: `bisect::tests` (incl.
  `a_search_is_saved_before_any_mod_is_switched`, `a_switch_cut_short_at_any_point_is_finished`,
  `an_original_renamed_by_hand_is_found_by_its_bytes`, `an_original_that_cant_be_found_keeps_the_search`,
  `twins_are_refused_at_the_start`, `a_stop_waits_for_an_answer_being_switched`,
  `kept_mods_keep_what_they_need_through_the_confirming_step`, `a_nested_mods_needs_count_for_its_jar`), `ui::mod_sets::tests::search_controls_share_one_admission_check`.
- **Crash analysis:** `crash::analyze_with` only says "certain" for what a loader or Minecraft states outright; stack
  frames and Suspected Mods are "likely" at best, frames below the top 8 "possible", and with nothing likely the
  headline says no single mod stands out. Frames from Minecraft, loaders, Java and common libraries (`LIBRARIES`,
  `NOT_MODS`) never point at a mod. Analysis runs on a Promise; a log only gets a jar index if `looks_crashed`. A
  game that exits with a new crash report or a failed exit gets a notice in its log tab, cleared on the next launch;
  a JVM killed by SIGKILL/SIGTERM/SIGINT counts as stopped, any other signal as a crash. Tests: `crash::tests`.
- **Screenshots stay per instance:** never pool them on disk (a shared pool needed guessed ownership, which lost or
  misattributed files). The screenshots tab's "all instances" toggle (config `all_screenshots`) lists every
  instance's folder in the background, each tile captioned with its instance. Test:
  `ui::screenshots::tests::merged_view_lists_every_instance_with_its_owner`.
- **Game settings are copied, never shared:** "settings" in the new-instance dialog copies another instance's
  `options.txt` (plus OptiFine's `optionsof.txt`/`optionsshaders.txt`, `instance::SETTINGS_FILES`) once, at creation.
  Sharing it live would lose keys: older versions drop keys they don't know when they save, and mod keybinds differ
  between instances. Test: `instance::tests::copy_settings_takes_only_settings_files`.
- **File drops:** files dropped on an instance's mods, resource packs or shader packs tab are copied into that folder
  (`Instance::add_content`, run as an install job under the same guards as Modrinth installs); anywhere else, or a
  `.mrpack`, opens the import dialog. Everything is checked before anything is copied: each file must be what the
  tab lists (`.jar` mods; `.zip` or folder packs), and a name already there, enabled or not, is skipped if identical
  and refused if not, never replaced. On the worlds tab a world folder (with `level.dat`) or a `.zip` of one world
  is added to `saves` (`saves::add_world`): copied or extracted beside `saves`, never inside it, then moved in under
  the next free name (`<name> (2)`), without its `session.lock`. A zip with several worlds, or a folder open in a
  game, is refused. It runs as a world job (`jobs_on` its saves, not while shared folders move), so launches wait
  for it. Tests: `instance::tests::dropped_content_is_checked_before_anything_is_copied`,
  `saves::tests::dropped_worlds_are_added_beside_the_others`.
- **Saving:** write config/state files with `fsutil::write_atomic` (unique temp + rename; accounts at 0600).

## Testing the GUI safely

- **Isolate data:** never test against real data. Set `XDG_DATA_HOME`/`XDG_CONFIG_HOME` to a scratch dir.
- **Driving it:** add temporary hooks in `App::new` / `App::ui`, then remove them before committing.
  - `OMACRAFT_DEBUG_VIEW=<instance-id>:<tab>` sets the starting view.
  - `OMACRAFT_DEBUG_KEYS=l,j,shift+j,space` injects one key every few frames as `egui::Event::Key`.
  - Never inject input through the compositor (e.g. Hyprland's send-key dispatchers): it types into whatever window has focus.
- **Never let a game window open on the desktop:** LWJGL uses Wayland, so unset
  `WAYLAND_DISPLAY` as well as `DISPLAY` when test-launching Minecraft.
- **Plain-system simulation:** `HOME=<scratch>` plus a `FONTCONFIG_FILE` that only lists
  `/usr/share/fonts/liberation` mimics a non-Omarchy distro.

## Sign-in and licensing

- **Microsoft sign-in:** the Azure client ID in `config::DEFAULT_CLIENT_ID` is Omacraft's own and
  approved by Mojang. It stays in the source so source builds sign in out of the box; the README asks
  forks to register their own. Don't show it in the UI; the settings field says "use default". Never use
  Prism's ID.
- **License:** MIT, in `LICENSE`. No code came from Prism.
- **Prism's icons:** CC BY-SA 4.0 (per each SVG's metadata), not GPL-3, so they don't affect the code's
  license. Keep the README credit when adding or editing them.
- **Loader logos:** `fabricmc`/`quiltmc`/`neoforged` are those projects' logos: artwork under Apache 2.0 /
  CC0 / CC BY 4.0 (sources in the README), no published trademark policy from any of them. Use them only to
  label the loader, unmodified, with the "not affiliated" line kept.

## Not supported

- **CurseForge:** needs a CurseForge API key (an application on their developer console), and many files
  disallow third-party downloads (Prism opens their pages and watches `~/Downloads`). Today `import.rs` bails
  on CurseForge packs with a clear error, and content search is Modrinth-only.
- **LiteLoader, Forge before 1.6:** legacy loaders, only worth it for old packs. Pre-1.6 Forge patches the
  jar instead of using launchwrapper.
- **Non-x86_64:** Java runtime and native matching assume x86_64. An explicit "unsupported architecture"
  error would be kinder than the current partial matching.
