# Omacraft

A Minecraft launcher in Rust (egui/eframe 0.36), modeled on Prism Launcher and drawn in
Omarchy's design language. It themes itself live from the active Omarchy theme. The README
covers features and file layout for users; this file is for working on the code.

Repo: `star-ges/omacraft` (private for now; to go public once the README has screenshots), branch `master`. The name is **Omacraft**, with a capital O
only, like Omacalc. Use lowercase `omacraft` for the binary, app-id and paths. Never write "OmaCraft".

## Build, run, test

Rust lives in `~/.cargo/bin`, which may not be on PATH: `export PATH=$HOME/.cargo/bin:$PATH`.

```sh
cargo build --release
install -Dm755 target/release/omacraft ~/.local/bin/omacraft    # check `pgrep omacraft` first
cargo test                                                      # unit tests
OMACRAFT_FIXTURE=<copy of a .minecraft> cargo test              # + NBT round-trips on real saves
cargo test -- --ignored                                         # + network tests (Modrinth)
omacraft list | import <zip|mrpack|folder|url> | export <inst> <file> | theme   # CLI, handy for checks
```

- **Toolchain:** rustfmt and clippy aren't installed (minimal rustup profile). Don't run
  `cargo fmt`; the code uses longer lines than rustfmt's defaults. Match the surrounding style.
- **Warnings:** keep the build warning-free.

## Code map

| Module | What it does |
|---|---|
| `main.rs` | tokio runtime; CLI if there are args, otherwise eframe (`with_decorations(false)`) |
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
  - New actions go in `keymap.rs` `actions!` (with a scope), plus `nav::hints`.
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

Set by the review fixes, each covered by a regression test. Keep them when changing these areas.

- **Folder imports (S2):** `fsutil::copy_tree` copies *through* every symlink, inside the instance or not
  (shared `saves`/`mods` folders come across as independent copies). Broken links and links back up the tree
  are skipped and reported as import warnings.
- **Tests (T1):** under `cfg(test)`, `paths::data_dir`/`config_dir` live in `/tmp/omacraft-test-<pid>`, so no
  test touches real data. Put test scratch under `paths::test_root()`.
- **Pack imports (C4, S4):** Modrinth version URLs (`/modpack/<slug>/version/<id or number>`) install that
  version or fail; `.mrpack` files are checked before the instance exists (HTTPS downloads, a valid SHA-1 per file).
- **Content installs (C1):** `modrinth::plan` works out the whole set before downloading. Pins beat the
  newest build, installed files count by Modrinth project (hash lookup) as well as filename, and required
  dependencies that can't be met come back in `Installed::missing` (error toast, non-zero CLI exit).
- **Installed marks (B8):** the content browser's "installed" comes from the current instance's files as
  they are now (`MetaStore::project_ids(files)`); metadata is keyed by path, size and mtime, so a replaced file
  is looked up again. Install jobs are keyed by (instance, project).
- **Frame work (R1):** nothing a pane draws runs subprocesses or reads files in bulk. Settings shows `app.fonts`
  (re-resolved only when the theme watcher sees fontconfig change); worlds are listed and duplicated on a
  `Promise`/`Task`, and the old list stays up while a refresh runs.
- **Decompression caps (S3):** compressed metadata (`instance.cfg`, `mmc-pack.json`, `modrinth.index.json`, exported
  icons, mod metadata and icons, `level.dat`) is read through `fsutil::read_capped`, which fails past
  `METADATA_LIMIT` (16 MiB). Bulk extraction (`import::extract`) has no total-bytes cap yet.
- **Export round trip (T3):** `export::tests::exports_import_back` exports a tiny instance as a zip and an
  `.mrpack` and imports both back offline. Files (incl. empty folders) must match, minus personal ones. The zip
  must keep everything `instance.cfg` carries (not the Java path) plus the custom icon; the mrpack keeps name,
  version, loader and notes (as `summary`). Add new portable metadata to that test.
- **Saving:** write config/state files with `fsutil::write_atomic` (unique temp + rename; accounts at 0600).

## Testing the GUI safely

- **Isolate data:** never test against the user's data. Set `XDG_DATA_HOME`/`XDG_CONFIG_HOME` to a scratch dir.
- **Driving it:** add temporary hooks in `App::new` / `App::ui`, then remove them before committing.
  - `OMACRAFT_DEBUG_VIEW=<instance-id>:<tab>` sets the starting view.
  - `OMACRAFT_DEBUG_KEYS=l,j,shift+j,space` injects one key every few frames as `egui::Event::Key`.
  - Never inject input through the compositor (e.g. Hyprland's send-key dispatchers): it types into the user's focused window.
- **Never let a game window open on the user's desktop:** LWJGL uses Wayland, so unset
  `WAYLAND_DISPLAY` as well as `DISPLAY` when test-launching Minecraft.
- **Plain-system simulation:** `HOME=<scratch>` plus a `FONTCONFIG_FILE` that only lists
  `/usr/share/fonts/liberation` mimics a non-Omarchy distro.

## Working with the user

- **Commits:** commit and push only when asked. End commit messages with the Co-Authored-By line.
  Git and `gh` are set up for HTTPS.
- **Microsoft sign-in:** the Azure client ID in `config::DEFAULT_CLIENT_ID` is Omacraft's own and
  approved by Mojang. Don't show it in the UI; the settings field says "use default". Never use Prism's ID.
- **Prism's icons:** CC BY-SA 4.0 (per each SVG's metadata), not GPL-3, so they don't affect the code's
  license. Keep the README credit when adding or editing them. `fabricmc`/`quiltmc`/`neoforged` are those
  projects' logos: artwork under Apache 2.0 / CC0 / CC BY 4.0 (sources in the README), no published trademark
  policy from any of them. Use them only to label the loader, unmodified, with the "not affiliated" line kept.

## Public release

The history was squashed to one commit before going public (the old commits carried a personal email).
The git email is the GitHub noreply address; keep it that way.

- **Client ID:** stays in the source, like Prism's, so source builds sign in out of the box. The README asks
  forks to register their own Azure app.
- **Binary size:** the release profile has LTO, strip and codegen-units=1 (49 MB to 29 MB); `zip` is down to
  Deflate (flate2/zlib-rs), Deflate64 and `time`. Keep new dependencies' default features in check.
- **License:** MIT (no code came from Prism), in `LICENSE`; the README's License section points at Credits
  for the assets' own terms.
- **Private notes:** machine-specific notes go in the gitignored `CLAUDE.local.md`, review notes in the
  excluded `REVIEW-*.md`. Nothing personal goes in tracked files.

## Review findings

IDs refer to `REVIEW-FINDINGS.md` (local only, excluded via `.git/info/exclude`; full scenarios and fixes are
there). Add a regression test with each fix rather than a separate test matrix. Remove items as they land,
and note any rule a fix sets under "Behaviour rules" above.

### Deferred (ultra-low priority)

- **S8:** size-only cache check is a deliberate speed trade-off (corruption risk, not a security hole). If it
  bites, record verified hashes by path/size/mtime (like `content_meta`) or add a "verify files" action.
- **C5:** non-x86 architecture matching. Omarchy is x86_64 only; once the repo is public, an explicit
  "unsupported architecture" error would be kinder than partial matching.

## Parked (README "Not yet")

Not planned; the user doesn't need these. Notes kept for if that changes.

- **CurseForge:** the user doesn't use it.
  - Needs a CurseForge API key (application on the developer console).
  - Many files disallow third-party downloads; Prism opens their pages and watches `~/Downloads`.
  - Today `import.rs` bails on CurseForge packs with a clear error, and content search is Modrinth-only.
- **LiteLoader, Forge before 1.6:** legacy loaders, only worth it for old packs. Pre-1.6 Forge patches the jar instead of using launchwrapper.
