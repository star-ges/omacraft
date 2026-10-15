//! Headless commands, handy for Hyprland binds and the Omarchy menu:
//!   omacraft list
//!   omacraft launch <instance>
//!   omacraft create <name> <mc-version> [fabric|quilt|forge|neoforge]
//!   omacraft mod add <instance> <modrinth-slug>
//!   omacraft import <zip|mrpack|folder|url>
//!   omacraft export <instance> <file.zip|file.mrpack>
//!   omacraft theme

use std::io::IsTerminal;
use std::sync::{Arc, Mutex};

use egui::Color32;

use crate::auth::{self, Account, Accounts};
use crate::config::Config;
use crate::groups::Groups;
use crate::instance::{self, Instance};
use crate::launch::{self, GameLog};
use crate::loaders::{self, Loader, LoaderKind};
use crate::task::{Task, TaskStatus};
use crate::theme::Theme;

const USAGE: &str = "usage: omacraft [list | launch <instance> | create <name> <version> [fabric|quilt|forge|neoforge] | mod add <instance> <slug> | import <file|folder|url> | export <instance> <file.zip|file.mrpack> | theme]";

pub async fn run(args: Vec<String>) -> i32 {
    let r = match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["list"] => list(),
        ["launch", id] => launch(id).await,
        ["create", name, version] => create(name, version, None).await,
        ["create", name, version, loader] => create(name, version, Some(loader)).await,
        ["mod", "add", id, slug] => add_mod(id, slug).await,
        ["import", source] => import(source).await,
        ["export", id, dest] => export(id, dest).await,
        ["theme"] => {
            println!("{:#?}", Theme::load());
            Ok(())
        }
        ["-h" | "--help" | "help"] => {
            println!("{USAGE}");
            Ok(())
        }
        _ => {
            eprintln!("{USAGE}");
            return 2;
        }
    };
    match r {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {e:#}");
            1
        }
    }
}

/// Instances in sidebar order: ungrouped first, then each group in turn.
/// A terminal gets aligned columns under coloured group headings; a pipe
/// gets one tab-separated line per instance (id, name, version, loader, group).
fn list() -> anyhow::Result<()> {
    let instances = instance::load_all();
    let mut groups = Groups::load();
    groups.sync(&instances);
    let mut sections: Vec<(Option<&str>, Vec<&Instance>)> = vec![(None, instances.iter().filter(|i| i.group.is_empty()).collect())];
    for g in &groups.list {
        sections.push((Some(g.name.as_str()), instances.iter().filter(|i| i.group == g.name).collect()));
    }
    sections.retain(|(_, members)| !members.is_empty());

    if !std::io::stdout().is_terminal() {
        for i in sections.iter().flat_map(|(_, m)| m) {
            println!("{}\t{}\t{}\t{}\t{}", i.id, i.name, i.mc_version, i.loader.display(), i.group);
        }
        return Ok(());
    }
    if instances.is_empty() {
        println!("no instances yet; create one with `omacraft create` or in the app");
        return Ok(());
    }

    let t = Theme::load();
    let color = std::env::var_os("NO_COLOR").is_none();
    let paint = |c: Color32, s: &str| if color { format!("\x1b[38;2;{};{};{}m{s}\x1b[0m", c.r(), c.g(), c.b()) } else { s.to_string() };
    let bold = |s: String| if color { format!("\x1b[1m{s}\x1b[0m") } else { s };
    let pad = |s: &str, w: usize| format!("{s}{}", " ".repeat(w.saturating_sub(s.chars().count())));
    let name_of = |i: &Instance| {
        let n: Vec<char> = i.name.chars().collect();
        if n.len() > 40 { format!("{}…", n[..39].iter().collect::<String>()) } else { i.name.clone() }
    };
    let width = |f: &dyn Fn(&Instance) -> String| instances.iter().map(|i| f(i).chars().count()).max().unwrap_or(0);
    let (nw, vw, lw) = (width(&name_of), width(&|i| i.mc_version.clone()), width(&|i| i.loader.display()));

    for (n, (group, members)) in sections.iter().enumerate() {
        if let Some(g) = group {
            if n > 0 {
                println!();
            }
            let tint = groups.color(g).and_then(|c| t.named(c)).unwrap_or(t.accent);
            println!("{} {}", bold(paint(tint, g)), paint(t.muted, &members.len().to_string()));
        }
        for i in members {
            println!(
                "  {}  {}  {}  {}",
                paint(t.fg, &pad(&name_of(i), nw)),
                paint(t.fg, &pad(&i.mc_version, vw)),
                paint(t.fg, &pad(&i.loader.display(), lw)),
                paint(t.muted, &crate::ui::widgets::ago(i.last_played)),
            );
        }
    }
    Ok(())
}

async fn create(name: &str, version: &str, loader: Option<&str>) -> anyhow::Result<()> {
    let kind = match loader {
        None => LoaderKind::Vanilla,
        Some(l) => LoaderKind::parse(l).ok_or_else(|| anyhow::anyhow!("unknown loader {l}"))?,
    };
    let loader = match kind {
        LoaderKind::Vanilla => Loader::Vanilla,
        k => {
            let versions = loaders::versions_for(k, version).await?;
            let v = versions.iter().find(|b| b.preferred).or(versions.first()).map(|b| b.version.clone());
            let v = v.ok_or_else(|| anyhow::anyhow!("{} has no build for {version}", k.label()))?;
            k.with(v)
        }
    };
    let mut inst = Instance::new(name, version, loader);
    inst.assign_id()?;
    inst.save()?;
    println!("{}", inst.id);
    Ok(())
}

fn find(id: &str) -> anyhow::Result<Instance> {
    instance::load_all()
        .into_iter()
        .find(|i| i.id == id || i.name == id)
        .ok_or_else(|| anyhow::anyhow!("no instance {id}"))
}

async fn add_mod(id: &str, slug: &str) -> anyhow::Result<()> {
    let inst = find(id)?;
    let done = crate::modrinth::install(crate::instance::ContentKind::Mods, slug, &inst).await?;
    for f in &done.files {
        println!("{f}");
    }
    if !done.missing.is_empty() {
        anyhow::bail!("required dependencies not installed:\n  {}", done.missing.join("\n  "));
    }
    Ok(())
}

async fn import(source: &str) -> anyhow::Result<()> {
    let task = Task::new("import");
    let req = crate::import::Request { source: source.to_string(), name: None, group: String::new() };
    let fut = tokio::spawn(crate::import::import(task.clone(), req));
    // Print stage changes while it runs.
    let mut last = String::new();
    while !fut.is_finished() {
        let stage = task.stage_text();
        if stage != last {
            eprintln!("{stage}");
            last = stage;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    let crate::import::Imported { inst, warnings } = fut.await??;
    for w in warnings {
        eprintln!("warning: {w}");
    }
    println!("imported {} ({}, {}) as {}", inst.name, inst.mc_version, inst.loader.display(), inst.id);
    Ok(())
}

/// Export with the default selection (no worlds, screenshots or logs).
async fn export(id: &str, dest: &str) -> anyhow::Result<()> {
    let inst = find(id)?;
    let dest = std::path::absolute(dest)?;
    let format = crate::export::Format::from_path(&dest).ok_or_else(|| anyhow::anyhow!("export to a .zip (instance zip) or .mrpack (Modrinth pack)"))?;
    if dest.exists() {
        anyhow::bail!("{} already exists", dest.display());
    }
    let include = crate::export::default_include(&inst);
    let req = crate::export::Request { inst, format, dest, include, version: "1.0.0".into() };
    let task = Task::new("export");
    let fut = tokio::spawn(crate::export::export(task.clone(), req));
    let mut last = String::new();
    while !fut.is_finished() {
        let stage = task.stage_text();
        if stage != last {
            eprintln!("{stage}");
            last = stage;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    let out = fut.await??;
    match format {
        crate::export::Format::Mrpack => println!("exported {} ({} files from Modrinth)", out.path.display(), out.hosted),
        crate::export::Format::Zip => println!("exported {}", out.path.display()),
    }
    Ok(())
}

async fn launch(id: &str) -> anyhow::Result<()> {
    let mut inst = find(id)?;
    let cfg = Config::load();
    let mut accounts = Accounts::load();
    let mut account = accounts.active().cloned().ok_or_else(|| anyhow::anyhow!("no active account; add one in the app"))?;
    if let Account::Msa(a) = &account
        && a.mc_expires <= chrono::Utc::now()
    {
        let cid = cfg.client_id();
        account = Account::Msa(auth::refresh(&cid, a).await?);
        if accounts.refresh(account.clone()) {
            accounts.save()?;
        }
    }
    let token = match &account {
        Account::Msa(a) => a.mc_token.clone(),
        Account::Offline { .. } => "0".into(),
    };

    let task = Task::new("launch");
    let log = Arc::new(Mutex::new(GameLog::default()));
    // Mirror progress and game output to the terminal.
    let (t2, l2) = (task.clone(), log.clone());
    let printer = tokio::spawn(async move {
        let mut last_stage = String::new();
        let mut printed = 0usize;
        loop {
            let stage = t2.stage_text();
            if stage != last_stage && !stage.is_empty() {
                eprintln!(":: {stage}");
                last_stage = stage;
            }
            {
                let l = l2.lock().unwrap();
                // Counted by `pushed`: once the log is full, old lines drop off the front.
                let new = (l.pushed - printed).min(l.lines.len());
                for line in l.lines.iter().skip(l.lines.len() - new) {
                    println!("{}", line.text);
                }
                printed = l.pushed;
                if l.exited.is_some() {
                    break;
                }
            }
            if let TaskStatus::Failed(_) = t2.status() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    });
    let res = launch::launch(task, inst.clone(), cfg, account, token, log, None).await;
    let _ = printer.await;
    let res = res?;
    inst.last_played = Some(chrono::Utc::now());
    inst.played_secs += res.played.as_secs();
    inst.save()?;
    match res.exit {
        Ok(0) => Ok(()),
        Ok(c) => anyhow::bail!("game exited with code {c}"),
        Err(e) => anyhow::bail!("game {e}"),
    }
}
