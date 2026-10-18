# Contributing

## Build and test

```sh
cargo build --release
cargo test                                          # unit tests
OMACRAFT_FIXTURE=<copy of a .minecraft> cargo test  # adds NBT round-trips on real saves
cargo test -- --ignored                             # adds network tests (Modrinth)
```

Tests never touch your real data: under `cfg(test)` the data and config dirs live in
`/tmp/omacraft-test-<pid>`. To try the GUI on scratch data, set `XDG_DATA_HOME` and
`XDG_CONFIG_HOME` to an empty directory. When test-launching Minecraft, unset `WAYLAND_DISPLAY`
as well as `DISPLAY` so no game window opens on your desktop (LWJGL uses Wayland).

The content store's reflink path is only covered when the tests run on btrfs (`/tmp` is usually
tmpfs): `TMPDIR=<dir on btrfs> cargo test store::`.

`vendor/smithay-clipboard` is smithay-clipboard 0.7.3, patched to receive file drops on Wayland
(winit has none there). Its own tests: `cargo test --manifest-path vendor/smithay-clipboard/Cargo.toml --lib`.

The code isn't run through `rustfmt` and uses longer lines than its defaults, so match the
surrounding style. Keep the build free of warnings.

## Style

- **Look:** square corners, monospace text, lowercase labels, no shadows. Colours come only from
  the `Theme` tokens, never hard-coded.
- **Keys:** every action is declared in `keymap.rs`, so it can be rebound. Never hard-code a key;
  panes act on a `Nav` from `app.nav_for(zone)`, the same way their buttons do.
- **Background work:** anything slow runs on a `Promise` (with a `Task` for progress), never in a
  frame. Nothing a pane draws runs subprocesses or reads files in bulk.
- **Text:** it clips by default. Wrap paragraphs with `w::para`, truncate long single lines, and lay
  buttons out before the text beside them.
- **Destructive actions:** go through a confirmation (`Modal::Confirm`).
- **Saving:** write config and state files with `fsutil::write_atomic`.
- **Icons in UI text:** Nerd Font glyphs, written as `\u{…}` escapes.

## Behaviour rules

Much of Omacraft moves, links or rewrites the player's files: shared folders, backups, mod updates and
rollbacks, the search for the breaking mod, config edits. [AGENTS.md](AGENTS.md) lists the rules each of
these keeps (no file is ever replaced by a move, every multi-step change can be finished or undone after
a crash, jobs on the same files wait for each other), each with the regression tests that cover it.
Read the rule before changing an area, and add a test (and a rule) with any fix that sets a new one.

## Theme reloads

`omarchy theme set` swaps the theme directory with `mv`, so Omacraft polls the theme files and
fontconfig rather than holding an inotify watch on a directory that is about to be replaced.

## Code map (vs. Prism)

| Omacraft | Prism |
|---|---|
| `mojang.rs`: version JSON, rules, maven paths, profile merging | `minecraft/MojangVersionFormat`, `Library`, `Rule` |
| `loaders.rs`: Fabric/Quilt meta, Prism meta for Forge/NeoForge | `meta/` + component patches |
| `java.rs`: Mojang runtime manifests | `java/download/ManifestDownloadTask` |
| `launch.rs`: prepare, natives, args, process | `minecraft/launch/*`, `LauncherPartLaunch` |
| `auth.rs`: MSA device code chain | `minecraft/auth/steps/*` |
| `skins.rs`, `ui/skin_view.rs`: profile skins/capes, 3D preview | `minecraft/skins/*` |
| `instance.rs`, `groups.rs`: `instance.toml`, sidebar groups | `minecraft/MinecraftInstance`, `InstanceList` |
| `modrinth.rs`: search, install, dependency resolution, mod updates | `modplatform/modrinth` |
| `content_meta.rs`: mod and pack names, icons, Modrinth hash lookup | `minecraft/mod/` (`ModFolderModel`, `LocalModParseTask`) |
| `import.rs`: Prism/MultiMC instances, `.mrpack`, Modrinth pack links | `InstanceImportTask`, `modplatform/modrinth/ModrinthInstanceCreationTask` |
| `ui/new_instance.rs`: new instance, import, Modrinth modpack browser | `ui/dialogs/NewInstanceDialog`, `ui/pages/modplatform/modrinth/ModrinthPage` |
| `export.rs`: instance zip, `.mrpack` | `ui/dialogs/ExportInstanceDialog`, `modplatform/modrinth/ModrinthPackExportTask` |
| `saves.rs`: `level.dat`, `servers.dat`, dropped worlds | `minecraft/WorldList`, `ui/pages/instance/ServersPage` |
| `icons.rs` | `icons/IconList` |
| `theme.rs`, `ui/` | `ui/` (Qt) |

Omacraft's own, with no Prism counterpart:

| Module | What it does |
|---|---|
| `store.rs` | Content store: downloads kept once by SHA-1, reflinked into instances; `reclaim` dedupes with `FIDEDUPERANGE` |
| `shares.rs` | Share sets and world sets: an instance's folders become relative links into `shared/` |
| `world_sets.rs` | One version and loader per world set, play records (`omacraft.toml`) in each world, the pre-launch mod check |
| `backups.rs` | Reflink snapshots of worlds and of `mods` + `config`, restore, rollback, pruning, crash recovery |
| `fsutil.rs` | Reflink copies, `rename_noreplace`, journalled `swap_in`, `flock` helpers, capped reads |
| `crash.rs` | Crash analysis: suspects from crash reports and logs, with how sure it is |
| `mod_config.rs`, `vanilla_options.rs`, `shader_config.rs`, `ui/config.rs` | The configs tab: mod configs (TOML, JSON, JSON5), `options.txt`, shader pack options; edits splice only the value's bytes |
| `mod_sets.rs`, `bisect.rs`, `ui/mod_sets.rs` | Mod sets, and the search for the breaking mod |
| `keymap.rs`, `ui/nav.rs` | Every action and its keys (rebindable), focus zones, the hints bar and `?` help |
| `vendor/smithay-clipboard` | Wayland file drops |

Forge 1.6–1.12 runs through launchwrapper, and Forge 1.13+ and every NeoForge through
[ForgeWrapper](https://github.com/PrismLauncher/ForgeWrapper), both set up from Prism's meta.
