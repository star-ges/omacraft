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
`XDG_CONFIG_HOME` to an empty directory.

The code isn't run through `rustfmt` and uses longer lines than its defaults, so match the
surrounding style. Keep the build free of warnings.

## Style

- **Look:** square corners, monospace text, lowercase labels, no shadows. Colours come only from
  the `Theme` tokens, never hard-coded.
- **Keys:** every action is declared in `keymap.rs`, so it can be rebound. Never hard-code a key;
  panes act on a `Nav` from `app.nav_for(zone)`, the same way their buttons do.
- **Background work:** anything slow runs on a `Promise` (with a `Task` for progress), never in a
  frame.
- **Icons in UI text:** Nerd Font glyphs, written as `\u{…}` escapes.

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
| `modrinth.rs` | `modplatform/modrinth` |
| `import.rs`: Prism/MultiMC instances, `.mrpack` | `InstanceImportTask`, `modplatform/modrinth/ModrinthInstanceCreationTask` |
| `export.rs`: instance zip, `.mrpack` | `ui/dialogs/ExportInstanceDialog`, `modplatform/modrinth/ModrinthPackExportTask` |
| `saves.rs`: `level.dat`, `servers.dat` | `minecraft/WorldList`, `ui/pages/instance/ServersPage` |
| `icons.rs` | `icons/IconList` |
| `theme.rs`, `ui/` | `ui/` (Qt) |

Forge 1.6–1.12 runs through launchwrapper, and Forge 1.13+ and every NeoForge through
[ForgeWrapper](https://github.com/PrismLauncher/ForgeWrapper), both set up from Prism's meta.
