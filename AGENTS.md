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
| `modrinth.rs`, `content_meta.rs` | Search/install by `ContentKind`; name/icon from jar or pack, else Modrinth hash lookup |
| `import.rs` | Prism/MultiMC folder or zip, `.mrpack` file or URL, auto-detects other launchers |
| `export.rs` | Instance zip (MultiMC format) or `.mrpack` (Modrinth-hosted files via `content_meta::hosted`, rest in `overrides/`) |
| `saves.rs` | `level.dat` and `servers.dat` via fastnbt `Value` (round-trips unknown fields) |
| `keymap.rs` | Every action, default chords, scopes, parse/label, conflict detection |
| `shares.rs` | Shared folders, by `Kind`: share sets (`shared/sets/<name>/{resourcepacks,shaderpacks,schematics}`, `share_set` in instance.toml) and world sets (`shared/worlds/<name>/saves`, `world_set`); `apply` turns an instance's folders into relative links or back into copies |
| `world_sets.rs`, `backups.rs` | World sets' rules: one version + loader per set, `omacraft.toml` play records in each world, the pre-launch mod check; reflink world snapshots under `backups/worlds/` |
| `store.rs` | Content store: Modrinth installs and `.mrpack` files go through `cache/content/<sha1>` and are reflinked into instances; `reclaim` merges duplicates (`FIDEDUPERANGE`) and prunes the store |
| `icons.rs`, `fsutil.rs`, `net.rs`, `task.rs` | Instance icons, reflink copy, HTTP + downloads, background `Task`/`Promise` |
| `ui/mod.rs` | `App`, top/status bars, modals, key routing, launching, imports |
| `ui/nav.rs` | Focus zones, `Nav`, hints bar and `?` help, both built from the keymap |
| `ui/instances.rs` | Sidebar (groups, drag-and-drop, context menu) + instance header + tab dispatch |
| `ui/{version,content,worlds,servers,screenshots,logs}.rs` | Per-instance tabs |
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
  - Dialogs use `app.modal_nav()`.
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
  any of those states, so a crash at any step is finished (or a half copy discarded) by the next launch. A set counts as
  in use while any instance's folder still links into it (`shares::sets_in_use`), not just by `share_set`, so a
  leave that failed halfway keeps it from being deleted until the next `apply` finishes. Moving shared folders and
  reading or writing them exclude each other in the GUI: no export or pack install starts while `app.sharing` runs,
  and a share set change waits for exports and installs in the instance or any member of the old or new set
  (`App::set_busy`, members from `instances::set_members`). Tests:
  `shares::tests` (incl. `a_failed_leave_keeps_the_set_from_being_deleted`, `only_links_that_really_lead_into_a_set_count`,
  `an_interrupted_leave_is_finished_by_the_next_apply`), `shares::tests::links_inside_a_shared_folder_become_copies`, `ui::instances::tests::set_members_count_instances_still_linked_in`, `export::tests::shared_folders_export_like_normal_ones` (exports are self-contained: real files,
  mods included).
- **World sets:** a world set shares `saves` through the same `shares::apply` as share sets (worlds clash whole,
  never merge). Every member plays the same Minecraft version and loader kind (`world_sets::same_game`; the loader's
  own version may differ): joining a set that plays something else is refused, and changing one member's version
  asks to change the whole set or leave it (`Modal::SetVersion`). Changes to a world set also wait while another
  member runs or duplicates a world (a leave would copy a world mid-write). When a game exits, each world it played
  gets an `omacraft.toml` (instance, version, loader, mod id → version from the jars). A world counts as played if
  this game's pid held its `session.lock` (polled with `F_GETLK` every 3 s; Java's `FileChannel.tryLock` is a POSIX
  lock) or, never seen locked by another game, its `level.dat` changed during the run. Before a world-set member
  launches, any world last played with a mod it lacks, a newer mod version or a newer Minecraft gets a log warning
  and a snapshot first, unless it's open in another game or a snapshot taken since its last play covers it. Never
  blocks the launch. Tests: `world_sets::tests`, `backups::tests`,
  `shares::tests::world_sets_share_saves_apart_from_share_sets`.
- **Screenshots stay per instance:** never pool them on disk (a shared pool needed guessed ownership, which lost or
  misattributed files). The screenshots tab's "all instances" toggle (config `all_screenshots`) lists every
  instance's folder in the background, each tile captioned with its instance. Test:
  `ui::screenshots::tests::merged_view_lists_every_instance_with_its_owner`.
- **Game settings are copied, never shared:** "settings" in the new-instance dialog copies another instance's
  `options.txt` (plus OptiFine's `optionsof.txt`/`optionsshaders.txt`, `instance::SETTINGS_FILES`) once, at creation.
  Sharing it live would lose keys: older versions drop keys they don't know when they save, and mod keybinds differ
  between instances. Test: `instance::tests::copy_settings_takes_only_settings_files`.
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
