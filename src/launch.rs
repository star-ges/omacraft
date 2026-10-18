//! Prepare and launch an instance. Equivalent of Prism's launch steps:
//! resolve components, install Java, fetch libraries/assets, extract natives,
//! then build the command line and stream the game's output.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, bail};
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::auth::Account;
use crate::config::Config;
use crate::instance::Instance;
use crate::mojang::{self, Argument, AssetIndex, Features, OneOrMany, VersionJson};
use crate::net::{self, Download};
use crate::task::Task;
use crate::{java, loaders, paths};

pub const LAUNCHER_NAME: &str = "omacraft";

/// Jump straight into a world or server once the game has loaded.
#[derive(Debug, Clone)]
pub enum QuickPlay {
    /// Folder name under `saves/`.
    World(String),
    /// `host[:port]`
    Server(String),
}
const LOG_CAP: usize = 20_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Launcher,
    Info,
    Warn,
    Error,
    Debug,
}

#[derive(Debug, Clone)]
pub struct LogLine {
    pub level: LogLevel,
    pub text: String,
}

#[derive(Default)]
pub struct GameLog {
    pub lines: VecDeque<LogLine>,
    /// Lines ever pushed, including those since dropped off the front.
    pub pushed: usize,
    pub pid: Option<u32>,
    pub exited: Option<Result<i32, String>>,
    /// The session's access token, blanked out of every line (the game or a
    /// mod may print its own arguments).
    secret: Option<String>,
}

impl GameLog {
    pub fn push(&mut self, level: LogLevel, text: impl Into<String>) {
        if self.lines.len() >= LOG_CAP {
            self.lines.pop_front();
        }
        let mut text = text.into();
        if let Some(s) = &self.secret
            && text.contains(s.as_str())
        {
            text = text.replace(s.as_str(), "<token>");
        }
        self.lines.push_back(LogLine { level, text });
        self.pushed += 1;
    }

    /// Redact `token` from everything pushed from now on. Offline accounts'
    /// placeholder tokens are too short to be secrets (or to blank safely).
    pub fn set_secret(&mut self, token: &str) {
        self.secret = (token.len() > 8).then(|| token.to_string());
    }
}

pub type SharedLog = Arc<Mutex<GameLog>>;

pub fn classify(line: &str, stderr: bool) -> LogLevel {
    // log4j's default pattern: "[12:00:00] [Render thread/WARN]: ..."
    let mut end = line.len().min(80);
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    let head = &line[..end];
    if head.contains("/ERROR]") || head.contains("/FATAL]") || line.starts_with("Exception") || line.starts_with("\tat ") {
        LogLevel::Error
    } else if head.contains("/WARN]") {
        LogLevel::Warn
    } else if head.contains("/DEBUG]") || head.contains("/TRACE]") {
        LogLevel::Debug
    } else if stderr {
        LogLevel::Warn
    } else {
        LogLevel::Info
    }
}

/// Mojang's log4j configs emit `<log4j:Event>` XML so launchers can parse
/// levels reliably. Reassemble each event into a familiar one-line form.
#[derive(Default)]
struct XmlEvents {
    buf: Option<String>,
}

fn xml_attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let start = tag.find(&format!("{name}=\""))? + name.len() + 2;
    let end = tag[start..].find('"')? + start;
    Some(&tag[start..end])
}

fn xml_cdata(body: &str, tag: &str) -> Option<String> {
    let open = body.find(&format!("<{tag}>"))?;
    let rest = &body[open..];
    let start = rest.find("<![CDATA[")? + 9;
    let end = rest[start..].find("]]>")? + start;
    Some(rest[start..end].to_string())
}

impl XmlEvents {
    fn feed(&mut self, line: String, stderr: bool) -> Vec<(LogLevel, String)> {
        if self.buf.is_none() && !line.trim_start().starts_with("<log4j:Event") {
            return vec![(classify(&line, stderr), line)];
        }
        let buf = self.buf.get_or_insert_with(String::new);
        buf.push_str(&line);
        buf.push('\n');
        if !line.contains("</log4j:Event>") {
            return vec![];
        }
        let ev = self.buf.take().unwrap();
        let head = &ev[..ev.find('>').unwrap_or(ev.len())];
        let level_name = xml_attr(head, "level").unwrap_or("INFO");
        let thread = xml_attr(head, "thread").unwrap_or("main");
        let time = xml_attr(head, "timestamp")
            .and_then(|t| t.parse::<i64>().ok())
            .and_then(chrono::DateTime::from_timestamp_millis)
            .map(|t| t.with_timezone(&chrono::Local).format("%H:%M:%S").to_string())
            .unwrap_or_default();
        let level = match level_name {
            "WARN" => LogLevel::Warn,
            "ERROR" | "FATAL" => LogLevel::Error,
            "DEBUG" | "TRACE" => LogLevel::Debug,
            _ => LogLevel::Info,
        };
        let msg = xml_cdata(&ev, "log4j:Message").unwrap_or_default();
        let mut out: Vec<(LogLevel, String)> = Vec::new();
        for (i, l) in msg.lines().enumerate() {
            let text = if i == 0 { format!("[{time}] [{thread}/{level_name}]: {l}") } else { l.to_string() };
            out.push((level, text));
        }
        if let Some(tb) = xml_cdata(&ev, "log4j:Throwable") {
            out.extend(tb.lines().map(|l| (LogLevel::Error, l.to_string())));
        }
        out
    }
}

/// Everything needed to spawn the JVM.
struct Prepared {
    java: PathBuf,
    version: VersionJson,
    classpath: Vec<PathBuf>,
    assets_root: PathBuf,
    asset_index: String,
    virtual_assets: Option<PathBuf>,
    logging_arg: Option<String>,
    /// Loader-specific JVM flags (ForgeWrapper's paths).
    extra_jvm: Vec<String>,
}

pub async fn resolve_version(inst: &Instance) -> anyhow::Result<VersionJson> {
    let manifest = mojang::fetch_manifest().await?;
    let vanilla = mojang::fetch_version(&manifest, &inst.mc_version).await?;
    Ok(match loaders::profile(&inst.loader, &inst.mc_version).await? {
        Some(child) => vanilla.merge_child(child),
        None => vanilla,
    })
}

/// Games launch on x86_64 Linux only: Mojang's Java runtimes and the older versions'
/// natives are picked for it, and elsewhere they'd be the wrong ones, half matched.
pub fn check_platform(os: &str, arch: &str) -> anyhow::Result<()> {
    if os != "linux" || arch != "x86_64" {
        bail!("unsupported platform: Omacraft launches games on x86_64 Linux only, and this is {arch} {os}");
    }
    Ok(())
}

async fn prepare(task: &Task, inst: &Instance, cfg: &Config, features: &Features) -> anyhow::Result<Prepared> {
    check_platform(std::env::consts::OS, std::env::consts::ARCH)?;
    task.stage("Resolving version", 0);
    let mut version = resolve_version(inst).await?;
    // merge_child renames to the loader id; the client jar is always vanilla's.
    version.inherits_from = Some(inst.mc_version.clone());

    let java = if !inst.java_path.trim().is_empty() {
        PathBuf::from(inst.java_path.trim())
    } else if !cfg.java_override.trim().is_empty() {
        PathBuf::from(cfg.java_override.trim())
    } else {
        java::ensure_runtime(task, &version.java_component()).await?
    };
    if !java.exists() {
        bail!("Java not found at {}", java.display());
    }

    // Client jar + libraries.
    let mut downloads = Vec::new();
    let client = version
        .downloads
        .as_ref()
        .context("version has no client download")?
        .client
        .clone();
    let client_jar = version.client_jar();
    downloads.push(Download { url: client.url, path: client_jar.clone(), sha1: client.sha1, size: client.size });

    let mut classpath = Vec::new();
    let mut natives = Vec::new();
    for lib in version.libraries.iter().filter(|l| l.applies(features)) {
        if let Some(d) = lib.artifact() {
            if !classpath.contains(&d.path) {
                classpath.push(d.path.clone());
            }
            downloads.push(d);
        }
        if let Some(d) = lib.native() {
            natives.push((d.path.clone(), lib.extract.as_ref().map(|e| e.exclude.clone()).unwrap_or_default()));
            downloads.push(d);
        }
    }
    // Forge's installer and processor jars: fetched, not on the classpath.
    let mut installer = None;
    for lib in &version.maven_files {
        if let Some(d) = lib.artifact() {
            if lib.name.ends_with(":installer") {
                installer = Some(d.path.clone());
            }
            downloads.push(d);
        }
    }
    // ForgeWrapper runs the installer's processors on first launch, writing
    // the patched client into the libraries dir.
    let mut extra_jvm = Vec::new();
    if version.main_class == loaders::FORGE_WRAPPER_MAIN {
        let installer = installer.context("Forge profile has no installer")?;
        extra_jvm.push(format!("-Dforgewrapper.librariesDir={}", paths::libraries_dir().display()));
        extra_jvm.push(format!("-Dforgewrapper.installer={}", installer.display()));
        extra_jvm.push(format!("-Dforgewrapper.minecraft={}", client_jar.display()));
    }
    classpath.push(client_jar);

    // Logging config (patched log4j configs from Mojang).
    let mut logging_arg = None;
    if let Some(lc) = version.logging.as_ref().and_then(|l| l.client.as_ref()) {
        let path = paths::assets_dir().join("log_configs").join(&lc.file.id);
        downloads.push(Download {
            url: lc.file.url.clone(),
            path: path.clone(),
            sha1: Some(lc.file.sha1.clone()),
            size: Some(lc.file.size),
        });
        logging_arg = Some(lc.argument.replace("${path}", &path.to_string_lossy()));
    }
    net::download_all(task, "Libraries", downloads, 12).await?;

    // Assets.
    let assets_root = paths::assets_dir();
    let index_ref = version.asset_index.clone().context("version has no asset index")?;
    let index_path = assets_root.join("indexes").join(format!("{}.json", index_ref.id));
    net::download_all(
        task,
        "Asset index",
        vec![Download {
            url: index_ref.url.clone(),
            path: index_path.clone(),
            sha1: Some(index_ref.sha1.clone()),
            size: Some(index_ref.size),
        }],
        1,
    )
    .await?;
    let index: AssetIndex = serde_json::from_slice(&tokio::fs::read(&index_path).await?)?;
    let objects: Vec<Download> = index
        .objects
        .values()
        .map(|o| Download {
            url: format!("{}/{}", mojang::RESOURCES_URL, o.rel()),
            path: assets_root.join("objects").join(o.rel()),
            sha1: Some(o.hash.clone()),
            size: Some(o.size),
        })
        .collect();
    net::download_all(task, "Assets", objects, 32).await?;

    // Pre-1.6 versions read loose files instead of the object store.
    let mut virtual_assets = None;
    if index.is_virtual || index.map_to_resources {
        task.stage("Reconstructing legacy assets", index.objects.len() as u64);
        let target = if index.map_to_resources {
            inst.game_dir().join("resources")
        } else {
            assets_root.join("virtual").join(&index_ref.id)
        };
        let root = assets_root.clone();
        let objs = index.objects.clone();
        let t = target.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            for (name, o) in objs {
                let dest = t.join(&name);
                if !dest.exists() {
                    std::fs::create_dir_all(dest.parent().unwrap())?;
                    std::fs::copy(root.join("objects").join(o.rel()), &dest)?;
                }
            }
            Ok(())
        })
        .await??;
        virtual_assets = Some(target);
    }

    // Legacy natives: unpack into a per-instance dir every launch.
    task.stage("Extracting natives", natives.len() as u64);
    let natives_dir = inst.natives_dir();
    let _ = std::fs::remove_dir_all(&natives_dir);
    std::fs::create_dir_all(&natives_dir)?;
    for (jar, exclude) in natives {
        extract_natives(&jar, &natives_dir, &exclude).with_context(|| format!("extracting {}", jar.display()))?;
        task.inc(1);
    }

    Ok(Prepared {
        java,
        asset_index: index_ref.id,
        version,
        classpath,
        assets_root,
        virtual_assets,
        logging_arg,
        extra_jvm,
    })
}

fn extract_natives(jar: &Path, dest: &Path, exclude: &[String]) -> anyhow::Result<()> {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(jar)?)?;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        let name = entry.name()?.to_string();
        if entry.is_dir() || exclude.iter().any(|e| name.starts_with(e.as_str())) || name.starts_with("META-INF") {
            continue;
        }
        let Some(rel) = entry.enclosed_name() else { continue };
        let out = dest.join(rel);
        std::fs::create_dir_all(out.parent().unwrap())?;
        std::io::copy(&mut entry, &mut std::fs::File::create(out)?)?;
    }
    Ok(())
}

fn expand(args: &[Argument], features: &Features, out: &mut Vec<String>) {
    for a in args {
        match a {
            Argument::Plain(s) => out.push(s.clone()),
            Argument::Conditional { rules, value } => {
                if mojang::rules_allow(rules, features) {
                    match value {
                        OneOrMany::One(s) => out.push(s.clone()),
                        OneOrMany::Many(v) => out.extend(v.iter().cloned()),
                    }
                }
            }
        }
    }
}

/// Split user-written arguments like a shell would, without running one:
/// whitespace separates, `'…'` is literal, `"…"` allows `\"` and `\\`, and a
/// backslash outside quotes escapes the next character.
pub fn split_args(s: &str) -> anyhow::Result<Vec<String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_arg = false;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if in_arg {
                    out.push(std::mem::take(&mut cur));
                    in_arg = false;
                }
            }
            '\'' => {
                in_arg = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => cur.push(c),
                        None => bail!("unmatched ' in: {s}"),
                    }
                }
            }
            '"' => {
                in_arg = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(c @ ('"' | '\\')) => cur.push(c),
                            Some(c) => cur.extend(['\\', c]),
                            None => bail!("unmatched \" in: {s}"),
                        },
                        Some(c) => cur.push(c),
                        None => bail!("unmatched \" in: {s}"),
                    }
                }
            }
            '\\' => {
                in_arg = true;
                cur.push(chars.next().unwrap_or('\\'));
            }
            c => {
                in_arg = true;
                cur.push(c);
            }
        }
    }
    if in_arg {
        out.push(cur);
    }
    Ok(out)
}

fn substitute(arg: &str, vars: &HashMap<&str, String>) -> String {
    let mut s = arg.to_string();
    // Only touch ${...} tokens we know; unknown ones are left as-is.
    for (k, v) in vars {
        let token = format!("${{{k}}}");
        if s.contains(&token) {
            s = s.replace(&token, v);
        }
    }
    s
}

fn build_command(
    p: &Prepared,
    inst: &Instance,
    cfg: &Config,
    account: &Account,
    token: &str,
    features: &Features,
    quick: Option<&QuickPlay>,
) -> anyhow::Result<Vec<String>> {
    let game_dir = inst.game_dir();
    let cp = std::env::join_paths(&p.classpath).context("a library path contains ':'")?.to_string_lossy().into_owned();
    let (w, h) = inst.window.unwrap_or((854, 480));
    let user_type = match account {
        Account::Msa(_) => "msa",
        Account::Offline { .. } => "legacy",
    };
    let xuid = match account {
        Account::Msa(a) => a.xuid.clone(),
        _ => "0".into(),
    };
    let game_assets = p.virtual_assets.clone().unwrap_or_else(|| p.assets_root.clone());
    let vars: HashMap<&str, String> = HashMap::from([
        ("auth_player_name", account.name().to_string()),
        ("auth_uuid", account.uuid().to_string()),
        ("auth_access_token", token.to_string()),
        ("auth_session", format!("token:{token}:{}", account.uuid())),
        ("auth_xuid", xuid),
        ("clientid", String::new()),
        ("user_type", user_type.into()),
        ("user_properties", "{}".into()),
        ("version_name", inst.mc_version.clone()),
        ("version_type", LAUNCHER_NAME.into()),
        ("game_directory", game_dir.to_string_lossy().into_owned()),
        ("assets_root", p.assets_root.to_string_lossy().into_owned()),
        ("game_assets", game_assets.to_string_lossy().into_owned()),
        ("assets_index_name", p.asset_index.clone()),
        ("natives_directory", inst.natives_dir().to_string_lossy().into_owned()),
        ("library_directory", paths::libraries_dir().to_string_lossy().into_owned()),
        ("classpath_separator", ":".into()),
        ("classpath", cp.clone()),
        ("launcher_name", LAUNCHER_NAME.into()),
        ("launcher_version", env!("CARGO_PKG_VERSION").into()),
        ("resolution_width", w.to_string()),
        ("resolution_height", h.to_string()),
        ("quickPlayPath", game_dir.join("quickPlay/log.json").to_string_lossy().into_owned()),
        ("quickPlaySingleplayer", match quick {
            Some(QuickPlay::World(w)) => w.clone(),
            _ => String::new(),
        }),
        ("quickPlayMultiplayer", match quick {
            Some(QuickPlay::Server(s)) => s.clone(),
            _ => String::new(),
        }),
    ]);

    let mem = inst.memory_mb.unwrap_or(cfg.default_memory_mb).max(512);
    let mut jvm = vec![format!("-Xmx{mem}M"), format!("-Xms{}M", (mem / 4).max(512))];
    jvm.extend(split_args(&cfg.global_jvm_args).context("global JVM arguments")?);
    jvm.extend(split_args(&inst.jvm_args).context("instance JVM arguments")?);
    if let Some(l) = &p.logging_arg {
        jvm.push(l.clone());
    }
    jvm.extend(p.extra_jvm.iter().cloned());
    match p.version.arguments.as_ref().filter(|a| !a.jvm.is_empty()) {
        Some(a) => expand(&a.jvm, features, &mut jvm),
        None => {
            jvm.push("-Djava.library.path=${natives_directory}".into());
            jvm.push("-cp".into());
            jvm.push("${classpath}".into());
        }
    }

    let mut game = Vec::new();
    if let Some(a) = &p.version.arguments {
        expand(&a.game, features, &mut game);
    }
    if let Some(legacy) = &p.version.minecraft_arguments {
        game.extend(legacy.split_whitespace().map(String::from));
    }
    for t in &p.version.tweakers {
        game.extend(["--tweakClass".into(), t.clone()]);
    }
    if features.custom_resolution && p.version.arguments.is_none() {
        game.extend(["--width".into(), w.to_string(), "--height".into(), h.to_string()]);
    }
    // Before 1.20 there's no quick play, but --server/--port still work.
    if let Some(QuickPlay::Server(addr)) = quick
        && !supports_quick_play(&p.version)
    {
        let (host, port) = split_address(addr);
        game.extend(["--server".into(), host, "--port".into(), port.to_string()]);
    }

    let mut cmd = Vec::new();
    cmd.extend(jvm.iter().map(|a| substitute(a, &vars)));
    cmd.push(p.version.main_class.clone());
    cmd.extend(game.iter().map(|a| substitute(a, &vars)));
    Ok(cmd)
}

fn supports_quick_play(v: &VersionJson) -> bool {
    v.arguments.as_ref().is_some_and(|a| a.game.iter().any(|g| g.mentions("${quickPlayMultiplayer}")))
}

/// Split `host[:port]`, defaulting to Minecraft's port.
pub fn split_address(addr: &str) -> (String, u16) {
    match addr.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') || h.starts_with('[') => (h.trim_matches(['[', ']']).to_string(), p.parse().unwrap_or(25565)),
        _ => (addr.to_string(), 25565),
    }
}

pub struct LaunchResult {
    pub played: std::time::Duration,
    pub exit: Result<i32, String>,
}

/// Everything up to a running Java process.
#[allow(clippy::too_many_arguments)]
async fn start(
    task: &Task,
    inst: &Instance,
    cfg: &Config,
    account: &Account,
    token: &str,
    log: &SharedLog,
    quick: Option<&QuickPlay>,
    features: &Features,
) -> anyhow::Result<(tokio::process::Child, crate::world_sets::Loadout)> {
    let prepared = prepare(task, inst, cfg, features).await?;
    std::fs::create_dir_all(inst.game_dir())?;
    // Catches instances created or imported since sharing was set up, and settings
    // changed while this one was running. A failure here shouldn't stop the game.
    let i = inst.clone();
    let shared = tokio::task::spawn_blocking(move || crate::shares::apply(&i)).await?;
    // Other shared folders may lag, but never play a world set's worlds from outside it.
    if let Err(e) = crate::shares::check_world_link(inst) {
        return Err(match &shared {
            Err(why) => anyhow::anyhow!("{e:#} (shared folders: {why:#})"),
            Ok(_) => e,
        });
    }
    // A restore or mod rollback a crash cut short is finished first, so no world or
    // mods folder is missing when the game starts. It waits for backup jobs elsewhere,
    // and anything it can't put right stops the launch.
    task.stage("Checking backups", 0);
    let i = inst.clone();
    tokio::task::spawn_blocking(move || crate::backups::recover(&i)).await?.context("a restore or rollback was left unfinished; the game wasn't started")?;
    // A search for the breaking mod's switch cut short (or a mod switched by hand during
    // one) is put right, so the game plays the step it's asked about.
    let i = inst.clone();
    tokio::task::spawn_blocking(move || crate::bisect::recover(&i)).await?.context("the mods couldn't be switched for the search for the breaking mod; the game wasn't started")?;
    // Worlds may have been played with mods this one lacks: in another instance of its
    // world set, or here before a mod rollback.
    task.stage("Checking worlds", 0);
    let i = inst.clone();
    let worlds = tokio::task::spawn_blocking(move || crate::world_sets::check(&i)).await?;
    let args = build_command(&prepared, inst, cfg, account, token, features, quick)?;

    {
        let mut l = log.lock().unwrap();
        l.push(LogLevel::Launcher, format!("Omacraft {} launching {}", env!("CARGO_PKG_VERSION"), inst.name));
        l.push(LogLevel::Launcher, format!("minecraft {}  ·  {}", inst.mc_version, inst.loader.display()));
        l.push(LogLevel::Launcher, format!("java {}", prepared.java.display()));
        l.push(LogLevel::Launcher, format!("game dir {}", inst.game_dir().display()));
        match &shared {
            Ok(notes) => notes.iter().for_each(|n| l.push(LogLevel::Warn, n.clone())),
            Err(e) => l.push(LogLevel::Warn, format!("shared folders: {e:#}")),
        }
        worlds.into_iter().for_each(|n| l.push(LogLevel::Warn, n));
        match quick {
            Some(QuickPlay::World(w)) if supports_quick_play(&prepared.version) => l.push(LogLevel::Launcher, format!("opening world {w}")),
            Some(QuickPlay::World(_)) => l.push(LogLevel::Warn, "this version can't open a world on launch; pick it from the menu"),
            Some(QuickPlay::Server(s)) => l.push(LogLevel::Launcher, format!("joining {s}")),
            None => {}
        }
        // `push` blanks the access token.
        let shown: Vec<&str> = args.iter().map(|a| a.as_str()).filter(|a| !a.contains(std::path::MAIN_SEPARATOR) || a.len() < 200).collect();
        l.push(LogLevel::Launcher, format!("args {}", shown.join(" ")));
    }

    // The mods the game will load, read before it starts: the records written when it
    // exits describe these, not whatever the mods folder holds by then.
    let i = inst.clone();
    let loadout = tokio::task::spawn_blocking(move || crate::world_sets::Loadout::of(&i)).await?;

    task.stage("Starting game", 0);
    let child = tokio::process::Command::new(&prepared.java)
        .args(&args)
        .current_dir(inst.game_dir())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(false)
        .spawn()
        .with_context(|| format!("starting {}", prepared.java.display()))?;
    Ok((child, loadout))
}

/// Prepare, spawn, and wait for the game. Output streams into `log`.
pub async fn launch(
    task: Task,
    inst: Instance,
    cfg: Config,
    account: Account,
    token: String,
    log: SharedLog,
    quick: Option<QuickPlay>,
) -> anyhow::Result<LaunchResult> {
    let features = Features {
        custom_resolution: inst.window.is_some(),
        quick_play_singleplayer: matches!(quick, Some(QuickPlay::World(_))),
        quick_play_multiplayer: matches!(quick, Some(QuickPlay::Server(_))),
    };
    log.lock().unwrap().set_secret(&token);
    let (mut child, loadout) = match start(&task, &inst, &cfg, &account, &token, &log, quick.as_ref(), &features).await {
        Ok(c) => c,
        Err(e) => {
            // Leave nothing waiting on a game that never started.
            let msg = format!("{e:#}");
            task.finish(&Err(anyhow::anyhow!("{msg}")));
            log.lock().unwrap().exited = Some(Err(msg));
            return Err(e);
        }
    };
    log.lock().unwrap().pid = child.id();
    task.finish(&Ok(()));
    let started = Instant::now();

    let pump = |r: Box<dyn tokio::io::AsyncRead + Unpin + Send>, stderr: bool, log: SharedLog| {
        tokio::spawn(async move {
            let mut lines = BufReader::new(r).lines();
            let mut xml = XmlEvents::default();
            while let Ok(Some(line)) = lines.next_line().await {
                for (level, text) in xml.feed(line, stderr) {
                    log.lock().unwrap().push(level, text);
                }
            }
        })
    };
    let a = pump(Box::new(child.stdout.take().unwrap()), false, log.clone());
    let b = pump(Box::new(child.stderr.take().unwrap()), true, log.clone());
    // Note which worlds this game holds open, to record what they were played with.
    // The game is running now, so nothing below may return early.
    let child_pid = child.id().unwrap_or(0);
    let watcher = || crate::world_sets::PlayWatch::new(inst.saves_dir(), child_pid);
    let mut watch = watcher();
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(3));
    let status = loop {
        tokio::select! {
            s = child.wait() => break s,
            _ = tick.tick() => watch = tokio::task::spawn_blocking(move || { watch.look(); watch }).await.unwrap_or_else(|_| watcher()),
        }
    };
    let _ = tokio::join!(a, b);
    let (i, keep) = (inst.clone(), cfg.backups_keep);
    let (played, failed) = tokio::task::spawn_blocking(move || {
        let plays = watch.played();
        let mut failed = crate::world_sets::record_plays(&loadout, &plays);
        let played: Vec<std::path::PathBuf> = plays.into_iter().map(|(w, _)| w).collect();
        for w in &played {
            failed.extend(crate::backups::after_play(w, crate::backups::Owner::of(&i), &i.name, keep));
        }
        (played, failed)
    })
    .await
    .unwrap_or_default();

    let exit = match status {
        // Stopped by the user (or the system) vs. a JVM that crashed itself (SIGABRT, SIGSEGV).
        Ok(s) => s.code().ok_or_else(|| match std::os::unix::process::ExitStatusExt::signal(&s) {
            Some(n) if n != libc::SIGKILL && n != libc::SIGTERM && n != libc::SIGINT => format!("crashed (signal {n})"),
            _ => "killed by signal".to_string(),
        }),
        Err(e) => Err(e.to_string()),
    };
    {
        let mut l = log.lock().unwrap();
        l.pid = None;
        let msg = match &exit {
            Ok(0) => "game exited normally".to_string(),
            Ok(c) => format!("game exited with code {c}"),
            Err(e) => format!("game {e}"),
        };
        l.push(if exit == Ok(0) { LogLevel::Launcher } else { LogLevel::Error }, msg);
        if !played.is_empty() {
            let names: Vec<String> = played.iter().filter_map(|w| w.file_name()).map(|n| n.to_string_lossy().into_owned()).collect();
            l.push(LogLevel::Launcher, format!("recorded the mods {} was played with", names.join(", ")));
            if cfg.backups_keep > 0 {
                l.push(LogLevel::Launcher, format!("backed up {}", names.join(", ")));
            }
        }
        failed.into_iter().for_each(|f| l.push(LogLevel::Warn, f));
        l.exited = Some(exit.clone());
    }
    Ok(LaunchResult { played: started.elapsed(), exit })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loaders::Loader;

    #[test]
    fn games_launch_on_x86_64_linux_only() {
        assert!(check_platform("linux", "x86_64").is_ok());
        for (os, arch) in [("linux", "aarch64"), ("linux", "x86"), ("linux", "riscv64"), ("macos", "x86_64"), ("windows", "x86_64")] {
            let e = check_platform(os, arch).unwrap_err().to_string();
            assert!(e.contains("x86_64 Linux only") && e.contains(arch), "{os} {arch}: {e}");
        }
    }

    fn args_for(version: VersionJson, quick: Option<QuickPlay>) -> Vec<String> {
        let inst = Instance::new("t", &version.id, Loader::Vanilla);
        let features = Features {
            custom_resolution: false,
            quick_play_singleplayer: matches!(quick, Some(QuickPlay::World(_))),
            quick_play_multiplayer: matches!(quick, Some(QuickPlay::Server(_))),
        };
        let p = Prepared {
            java: "java".into(),
            version,
            classpath: vec![],
            assets_root: "/a".into(),
            asset_index: "x".into(),
            virtual_assets: None,
            logging_arg: None,
            extra_jvm: vec![],
        };
        build_command(&p, &inst, &Config::default(), &Account::offline("Steve"), "0", &features, quick.as_ref()).unwrap()
    }

    fn has_pair(args: &[String], flag: &str, value: &str) -> bool {
        args.windows(2).any(|w| w[0] == flag && w[1] == value)
    }

    #[test]
    fn quick_play_modern() {
        let json = r#"{"id":"1.21.1","mainClass":"M","arguments":{"game":["--username","${auth_player_name}",
            {"rules":[{"action":"allow","features":{"has_quick_plays_support":true}}],"value":["--quickPlayPath","${quickPlayPath}"]},
            {"rules":[{"action":"allow","features":{"is_quick_play_singleplayer":true}}],"value":["--quickPlaySingleplayer","${quickPlaySingleplayer}"]},
            {"rules":[{"action":"allow","features":{"is_quick_play_multiplayer":true}}],"value":["--quickPlayMultiplayer","${quickPlayMultiplayer}"]}],"jvm":[]}}"#;
        let v: VersionJson = serde_json::from_str(json).unwrap();
        let world = args_for(v.clone(), Some(QuickPlay::World("My World".into())));
        assert!(has_pair(&world, "--quickPlaySingleplayer", "My World"));
        assert!(!world.iter().any(|a| a == "--quickPlayMultiplayer"));
        assert!(world.iter().any(|a| a == "--quickPlayPath"));
        let server = args_for(v.clone(), Some(QuickPlay::Server("mc.example.com:25570".into())));
        assert!(has_pair(&server, "--quickPlayMultiplayer", "mc.example.com:25570"));
        assert!(!server.iter().any(|a| a == "--server"));
        let plain = args_for(v, None);
        assert!(!plain.iter().any(|a| a.starts_with("--quickPlay")));
    }

    #[test]
    fn quick_play_legacy_server() {
        let json = r#"{"id":"1.8.9","mainClass":"M","minecraftArguments":"--username ${auth_player_name}"}"#;
        let v: VersionJson = serde_json::from_str(json).unwrap();
        let server = args_for(v, Some(QuickPlay::Server("mc.example.com:25570".into())));
        assert!(has_pair(&server, "--server", "mc.example.com") && has_pair(&server, "--port", "25570"));
    }

    #[test]
    fn legacy_forge_tweak_class() {
        let json = r#"{"id":"1.12.2","mainClass":"net.minecraft.launchwrapper.Launch","minecraftArguments":"--username ${auth_player_name}","+tweakers":["net.minecraftforge.fml.common.launcher.FMLTweaker"]}"#;
        let args = args_for(serde_json::from_str(json).unwrap(), None);
        assert!(has_pair(&args, "--tweakClass", "net.minecraftforge.fml.common.launcher.FMLTweaker"));
        assert!(args.iter().any(|a| a == "net.minecraft.launchwrapper.Launch"));
    }

    #[test]
    fn split_args_quotes() {
        let v = |s: &str| split_args(s).unwrap();
        assert_eq!(v("  -Xss2M   -XX:+UseZGC "), ["-Xss2M", "-XX:+UseZGC"]);
        assert_eq!(v(r#"-Dexample.path="/tmp/my folder" -Da=b"#), ["-Dexample.path=/tmp/my folder", "-Da=b"]);
        assert_eq!(v(r#"'-Dx=a "b" c' -Dy=it\'s"#), [r#"-Dx=a "b" c"#, "-Dy=it's"]);
        assert_eq!(v(r#""-Dq=say \"hi\"" "-Dw=C:\dir" """#), [r#"-Dq=say "hi""#, r"-Dw=C:\dir", ""]);
        assert_eq!(v(r"-Dp=/tmp/my\ folder"), ["-Dp=/tmp/my folder"]);
        assert!(v("").is_empty());
        assert!(split_args(r#"-Dp="/tmp/my folder"#).is_err());
        assert!(split_args("-Dp='x").is_err());
    }

    #[test]
    fn quoted_jvm_arg_survives_substitution() {
        let json = r#"{"id":"1.21.1","mainClass":"M","arguments":{"game":[],"jvm":["-cp","${classpath}"]}}"#;
        let v: VersionJson = serde_json::from_str(json).unwrap();
        let mut inst = Instance::new("t", &v.id, Loader::Vanilla);
        inst.jvm_args = r#"-Dexample.path="/tmp/my folder" -Dgame=${game_directory}"#.into();
        let p = Prepared {
            java: "java".into(),
            version: v,
            classpath: vec![],
            assets_root: "/a".into(),
            asset_index: "x".into(),
            virtual_assets: None,
            logging_arg: None,
            extra_jvm: vec![],
        };
        let features = Features { custom_resolution: false, quick_play_singleplayer: false, quick_play_multiplayer: false };
        let args = build_command(&p, &inst, &Config::default(), &Account::offline("Steve"), "0", &features, None).unwrap();
        assert!(args.iter().any(|a| a == "-Dexample.path=/tmp/my folder"));
        assert!(args.iter().any(|a| *a == format!("-Dgame={}", inst.game_dir().display())));
        assert_eq!(args.iter().position(|a| a == "M"), Some(args.len() - 1));
        inst.jvm_args = r#"-Dp="/tmp"#.into();
        assert!(build_command(&p, &inst, &Config::default(), &Account::offline("Steve"), "0", &features, None).is_err());
    }

    #[test]
    fn classify_handles_multibyte_at_cut() {
        let line = format!("{}é [main/WARN]", "a".repeat(79));
        assert_eq!(classify(&line, false), LogLevel::Info);
        assert_eq!(classify("[12:00:00] [Render thread/WARN]: ü", false), LogLevel::Warn);
    }

    #[test]
    fn log_redacts_token() {
        let mut l = GameLog::default();
        l.set_secret("eyJhbGciOiJIUzI1NiJ9.secret");
        l.push(LogLevel::Info, "--accessToken eyJhbGciOiJIUzI1NiJ9.secret --version 1.21");
        assert_eq!(l.lines[0].text, "--accessToken <token> --version 1.21");
        let mut offline = GameLog::default();
        offline.set_secret("0");
        offline.push(LogLevel::Info, "port 25570");
        assert_eq!(offline.lines[0].text, "port 25570");
    }

    #[tokio::test]
    #[ignore = "network"]
    async fn failed_spawn_finishes_task_and_log() {
        let mut inst = Instance::new("nojava", "1.21.1", Loader::Vanilla);
        inst.java_path = "/nonexistent/java".into();
        let (task, log) = (Task::new("t"), SharedLog::default());
        // Resolves the version, then fails on the missing Java. Either way the
        // launch must end the task and the log, or `omacraft launch` hangs.
        let r = launch(task.clone(), inst, Config::default(), Account::offline("Steve"), "0".into(), log.clone(), None).await;
        assert!(r.is_err());
        assert!(matches!(task.status(), crate::task::TaskStatus::Failed(_)));
        assert!(matches!(log.lock().unwrap().exited, Some(Err(_))));
    }

    #[test]
    fn addresses() {
        assert_eq!(split_address("play.example.com"), ("play.example.com".into(), 25565));
        assert_eq!(split_address("example.com:25566"), ("example.com".into(), 25566));
        assert_eq!(split_address("[::1]:25570"), ("::1".into(), 25570));
        assert_eq!(split_address("::1"), ("::1".into(), 25565));
    }
}
