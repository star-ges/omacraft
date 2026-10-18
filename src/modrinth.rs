//! Modrinth: search and install mods, resource packs and shader packs into an
//! instance (Prism's modplatform).

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;

use anyhow::{Context, bail};
use serde::Deserialize;

use crate::content_meta;
use crate::instance::{ContentKind, Instance};
use crate::net::{self, Download};
use crate::store;
use crate::task::Task;

const API: &str = "https://api.modrinth.com/v2";

#[derive(Debug, Clone, Deserialize)]
pub struct SearchResponse {
    pub hits: Vec<Hit>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Hit {
    pub project_id: String,
    pub slug: String,
    pub title: String,
    pub description: String,
    pub author: String,
    pub downloads: u64,
    #[serde(default)]
    pub icon_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Version {
    pub id: String,
    pub project_id: String,
    #[serde(default)]
    pub version_number: String,
    pub files: Vec<VersionFile>,
    #[serde(default)]
    pub loaders: Vec<String>,
    #[serde(default)]
    pub dependencies: Vec<Dependency>,
    #[serde(default)]
    pub date_published: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub game_versions: Vec<String>,
    #[serde(default)]
    pub version_type: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct VersionFile {
    pub url: String,
    pub filename: String,
    pub primary: bool,
    pub size: u64,
    pub hashes: Hashes,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Hashes {
    pub sha1: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Dependency {
    pub project_id: Option<String>,
    /// Set when the dependency is pinned to one version.
    pub version_id: Option<String>,
    pub dependency_type: String,
}

fn enc(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn project_type(kind: ContentKind) -> &'static str {
    match kind {
        ContentKind::Mods => "mod",
        ContentKind::ResourcePacks => "resourcepack",
        ContentKind::ShaderPacks => "shader",
    }
}

pub async fn search(kind: ContentKind, query: &str, inst: &Instance) -> anyhow::Result<SearchResponse> {
    let mut facets = vec![
        format!(r#"["project_type:{}"]"#, project_type(kind)),
        format!(r#"["versions:{}"]"#, inst.mc_version),
    ];
    if kind == ContentKind::Mods
        && let Some(l) = inst.loader.modrinth()
    {
        facets.push(format!(r#"["categories:{l}"]"#));
    }
    let facets = format!("[{}]", facets.join(","));
    let index = if query.trim().is_empty() { "downloads" } else { "relevance" };
    let url = format!("{API}/search?query={}&facets={}&index={index}&limit=40", enc(query), enc(&facets));
    net::get_json(&url).await
}

/// Modpacks, for the import dialog: any game version, most downloaded first when there's no query.
pub async fn search_packs(query: &str) -> anyhow::Result<SearchResponse> {
    let facets = enc(r#"[["project_type:modpack"]]"#);
    let index = if query.trim().is_empty() { "downloads" } else { "relevance" };
    net::get_json(&format!("{API}/search?query={}&facets={facets}&index={index}&limit=40", enc(query))).await
}

/// A modpack's versions, newest first (as Modrinth lists them).
pub async fn pack_versions(project: &str) -> anyhow::Result<Vec<Version>> {
    net::get_json(&format!("{API}/project/{}/version", enc(project))).await
}

async fn compatible_version(kind: ContentKind, project: &str, inst: &Instance) -> anyhow::Result<Version> {
    let mut url = format!("{API}/project/{}/version?game_versions={}", enc(project), enc(&format!(r#"["{}"]"#, inst.mc_version)));
    let loader = match kind {
        ContentKind::Mods => Some(inst.loader.modrinth().context("vanilla instances can't load mods")?),
        ContentKind::ResourcePacks => Some("minecraft"),
        // Shaders are tagged by the shader mod (iris, optifine, canvas…).
        ContentKind::ShaderPacks => None,
    };
    if let Some(l) = loader {
        url.push_str(&format!("&loaders={}", enc(&format!(r#"["{l}"]"#))));
    }
    let versions: Vec<Version> = net::get_json(&url).await?;
    let pick = versions.iter().position(|v| loader.is_some() || v.loaders.iter().any(|l| l == "iris")).unwrap_or(0);
    versions
        .into_iter()
        .nth(pick)
        .with_context(|| format!("no build of {project} for {} {}", loader.unwrap_or(""), inst.mc_version))
}

/// What `install` did.
#[derive(Debug, Default)]
pub struct Installed {
    /// Filenames written.
    pub files: Vec<String>,
    /// The Modrinth project of each of `files`, in the same order.
    pub projects: Vec<String>,
    /// Required dependencies that couldn't be installed, each with why.
    pub missing: Vec<String>,
}

/// A required dependency: a project (its newest compatible build), a pinned
/// version, or both (the pin wins).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Ref {
    project: Option<String>,
    version: Option<String>,
}

/// Where `plan` gets versions from: Modrinth, or a fixture in tests.
trait Source {
    async fn version(&self, id: &str) -> anyhow::Result<Version>;
    async fn latest(&self, project: &str) -> anyhow::Result<Version>;
    async fn title(&self, project: &str) -> Option<String>;
}

struct Api<'a> {
    kind: ContentKind,
    inst: &'a Instance,
}

impl Source for Api<'_> {
    async fn version(&self, id: &str) -> anyhow::Result<Version> {
        net::get_json(&format!("{API}/version/{}", enc(id))).await
    }

    async fn latest(&self, project: &str) -> anyhow::Result<Version> {
        compatible_version(self.kind, project, self.inst).await
    }

    async fn title(&self, project: &str) -> Option<String> {
        #[derive(Deserialize)]
        struct P {
            title: String,
        }
        net::get_json::<P>(&format!("{API}/project/{}", enc(project))).await.ok().map(|p| p.title)
    }
}

/// Every version to install for `project`: its newest compatible build, then
/// (for mods) its required dependencies, pinned ones at their pinned version.
/// `installed` maps project id → version id of what the instance already has.
/// Returns the versions plus the required dependencies that can't be met.
/// An empty plan means the project is already installed.
async fn plan(src: &impl Source, kind: ContentKind, project: &str, installed: &HashMap<String, String>) -> anyhow::Result<(Vec<Version>, Vec<String>)> {
    let root = src.latest(project).await?;
    if installed.contains_key(&root.project_id) {
        return Ok((Vec::new(), Vec::new()));
    }
    Ok(resolve(src, kind, vec![root], installed).await)
}

/// `roots`, versions already picked, then (for mods) everything they require that
/// `installed` lacks, pinned ones at their pinned version. Returns them in that order,
/// plus the required dependencies that can't be met (including a pin on a version other
/// than the one installed or picked).
async fn resolve(src: &impl Source, kind: ContentKind, roots: Vec<Version>, installed: &HashMap<String, String>) -> (Vec<Version>, Vec<String>) {
    let mut pinned: HashSet<String> = roots.iter().map(|v| v.project_id.clone()).collect();
    let mut missing = Vec::new();
    let mut seen = HashSet::new();
    let mut queue = VecDeque::new();
    if kind == ContentKind::Mods {
        for v in &roots {
            queue.extend(v.dependencies.iter().filter(|d| d.dependency_type == "required").map(|d| Ref { project: d.project_id.clone(), version: d.version_id.clone() }));
        }
    }
    let mut plan = roots;
    while let Some(r) = queue.pop_front() {
        if !seen.insert(r.clone()) {
            continue;
        }
        // An unpinned dependency that's already there or planned needs nothing more.
        if r.version.is_none()
            && let Some(p) = &r.project
            && (installed.contains_key(p) || plan.iter().any(|v| &v.project_id == p))
        {
            continue;
        }
        let got = match (&r.version, &r.project) {
            (Some(id), _) => src.version(id).await,
            (None, Some(p)) => src.latest(p).await,
            (None, None) => continue,
        };
        let v = match got {
            Ok(v) => v,
            Err(e) => {
                let name = match (&r.project, &r.version) {
                    (Some(p), _) => src.title(p).await.unwrap_or_else(|| p.clone()),
                    (None, v) => format!("version {}", v.as_deref().unwrap_or_default()),
                };
                missing.push(format!("{name}: {e:#}"));
                continue;
            }
        };
        let is_pin = r.version.is_some();
        if let Some(have) = installed.get(&v.project_id) {
            if is_pin && *have != v.id {
                let name = src.title(&v.project_id).await.unwrap_or_else(|| v.project_id.clone());
                missing.push(format!("{name} {} is required, but another version is installed", v.version_number));
            }
            continue;
        }
        match plan.iter().position(|p| p.project_id == v.project_id) {
            Some(i) if plan[i].id == v.id => continue,
            // A pin replaces the newest build picked for an unpinned reference.
            Some(i) if is_pin && !pinned.contains(&v.project_id) => {
                plan.remove(i);
            }
            Some(i) => {
                if is_pin {
                    let name = src.title(&v.project_id).await.unwrap_or_else(|| v.project_id.clone());
                    missing.push(format!("{name}: needs both {} and {}", plan[i].version_number, v.version_number));
                }
                continue;
            }
            None => {}
        }
        if is_pin {
            pinned.insert(v.project_id.clone());
        }
        if kind == ContentKind::Mods {
            for d in v.dependencies.iter().filter(|d| d.dependency_type == "required") {
                queue.push_back(Ref { project: d.project_id.clone(), version: d.version_id.clone() });
            }
        }
        plan.push(v);
    }
    (plan, missing)
}

/// Project id → version id for the instance's files Modrinth knows. Empty if
/// the lookup fails; `install` still skips files with the same name.
async fn installed_versions(inst: &Instance, kind: ContentKind) -> HashMap<String, String> {
    let files = inst.content(kind);
    let hashes = tokio::task::spawn_blocking(move || files.iter().filter(|f| !f.is_dir).filter_map(|f| net::sha1_file(&f.path)).collect())
        .await
        .unwrap_or_default();
    match content_meta::versions(hashes).await {
        Ok(m) => m.into_values().collect(),
        Err(e) => {
            eprintln!("looking up installed {}: {e:#}", kind.dir());
            HashMap::new()
        }
    }
}

/// Install the newest compatible version of a project (plus, for mods, its
/// required dependencies). The whole set is worked out before anything is
/// downloaded; a dependency that can't be met is reported, not fatal.
pub async fn install(kind: ContentKind, project: &str, inst: &Instance) -> anyhow::Result<Installed> {
    let dir = inst.game_dir().join(kind.dir());
    tokio::fs::create_dir_all(&dir).await?;
    let installed = installed_versions(inst, kind).await;
    let (plan, missing) = plan(&Api { kind, inst }, kind, project, &installed).await?;
    let (files, projects) = fetch_new(inst, kind, &plan).await?;
    Ok(Installed { files, projects, missing })
}

/// Download each version's primary file into `inst`'s `kind` folder, unless a file of
/// that name is there already. Returns the filenames written and their projects.
async fn fetch_new(inst: &Instance, kind: ContentKind, plan: &[Version]) -> anyhow::Result<(Vec<String>, Vec<String>)> {
    let dir = inst.game_dir().join(kind.dir());
    let existing: HashSet<String> = inst.content(kind).into_iter().map(|m| m.name.trim_end_matches(".disabled").to_string()).collect();
    let (mut files, mut projects) = (Vec::new(), Vec::new());
    for v in plan {
        let file = v.files.iter().find(|f| f.primary).or(v.files.first()).context("version has no files")?;
        if !existing.contains(&file.filename) {
            store::fetch(&Download {
                url: file.url.clone(),
                path: dir.join(&file.filename),
                sha1: Some(file.hashes.sha1.clone()),
                size: Some(file.size),
            })
            .await?;
            files.push(file.filename.clone());
            projects.push(v.project_id.clone());
        }
    }
    Ok((files, projects))
}

/// A newer build of an installed mod.
#[derive(Debug, Clone)]
pub struct Update {
    /// The installed file as checked. Enabled or not, as it is when updated, stays so.
    pub path: PathBuf,
    /// The checked file's sha1, so a file replaced since isn't updated as if it were the same.
    pub sha1: String,
    /// The game it was picked for ([`game_key`]).
    pub game: String,
    pub title: String,
    /// The installed version's number, and the one to update to.
    pub from: String,
    pub to: Version,
}

/// An installed file: path, sha1.
type Held = (PathBuf, String);

/// The files in `held` with a newer build: `current` and `latest` map each sha1 to
/// Modrinth's version of that file and its newest compatible version. Newer by date,
/// so a beta installed by hand isn't "updated" to an older release.
fn pick_updates(held: &[Held], current: &HashMap<String, Version>, latest: &HashMap<String, Version>, titles: &HashMap<String, String>, game: &str) -> Vec<Update> {
    let mut out: Vec<Update> = held
        .iter()
        .filter_map(|(path, sha1)| {
            let (cur, new) = (current.get(sha1)?, latest.get(sha1)?);
            let newer = match (cur.date_published, new.date_published) {
                (Some(a), Some(b)) => b > a,
                _ => true,
            };
            let same = new.id == cur.id || new.files.iter().any(|f| f.hashes.sha1.eq_ignore_ascii_case(sha1));
            (newer && !same && new.project_id == cur.project_id).then(|| Update {
                path: path.clone(),
                sha1: sha1.clone(),
                game: game.to_string(),
                title: titles.get(&cur.project_id).cloned().unwrap_or_else(|| cur.version_number.clone()),
                from: cur.version_number.clone(),
                to: new.clone(),
            })
        })
        .collect();
    out.sort_by_key(|u| u.title.to_lowercase());
    out
}

/// Modrinth's versions for these sha1s (`/version_files`), or with `filter`, the newest
/// version each file could update to (`/version_files/update`).
async fn versions_by_hash(hashes: &[String], filter: Option<serde_json::Value>) -> anyhow::Result<HashMap<String, Version>> {
    let mut out = HashMap::new();
    for chunk in hashes.chunks(500) {
        let mut body = serde_json::json!({ "hashes": chunk, "algorithm": "sha1" });
        let url = match &filter {
            Some(f) => {
                body.as_object_mut().unwrap().extend(f.as_object().cloned().unwrap_or_default());
                format!("{API}/version_files/update")
            }
            None => format!("{API}/version_files"),
        };
        let got: HashMap<String, Version> = net::client().post(url).json(&body).send().await?.error_for_status()?.json().await?;
        out.extend(got);
    }
    Ok(out)
}

/// Project id → title.
async fn titles(ids: Vec<String>) -> HashMap<String, String> {
    #[derive(Deserialize)]
    struct P {
        id: String,
        title: String,
    }
    let mut out = HashMap::new();
    for chunk in ids.chunks(100) {
        let Ok(list) = serde_json::to_string(chunk) else { continue };
        if let Ok(ps) = net::get_json::<Vec<P>>(&format!("{API}/projects?ids={}", enc(&list))).await {
            out.extend(ps.into_iter().map(|p| (p.id, p.title)));
        }
    }
    out
}

/// The Minecraft version and loader updates are picked for: a list checked for one
/// game is no good for another.
pub fn game_key(inst: &Instance) -> String {
    format!("{} {}", inst.mc_version, inst.loader.modrinth().unwrap_or("vanilla"))
}

/// The instance's mods with a newer compatible build on Modrinth.
pub async fn check_updates(inst: &Instance) -> anyhow::Result<Vec<Update>> {
    let loader = inst.loader.modrinth().context("vanilla instances don't load mods")?;
    let files = inst.mods();
    let held: Vec<Held> = tokio::task::spawn_blocking(move || files.into_iter().filter(|f| !f.is_dir).filter_map(|f| Some((f.path.clone(), net::sha1_file(&f.path)?))).collect()).await?;
    let hashes: Vec<String> = held.iter().map(|(.., h)| h.clone()).collect();
    let current = versions_by_hash(&hashes, None).await?;
    let latest = versions_by_hash(&hashes, Some(serde_json::json!({ "loaders": [loader], "game_versions": [inst.mc_version] }))).await?;
    let mut ids: Vec<String> = current.values().map(|v| v.project_id.clone()).collect();
    ids.sort();
    ids.dedup();
    Ok(pick_updates(&held, &current, &latest, &titles(ids).await, &game_key(inst)))
}

/// What `update` did.
#[derive(Debug, Default)]
pub struct Updated {
    /// "title from → to", one per mod updated.
    pub done: Vec<String>,
    /// Dependencies the new versions brought in.
    pub added: Vec<String>,
    /// Required dependencies that couldn't be installed, each with why.
    pub missing: Vec<String>,
}

/// The backups an update takes before it changes anything. Worlds come first: played
/// with the new mods they can't go back on their own, so each gets a copy from before
/// unless one already covers it. The mods and config are snapshotted last, so every world
/// copy for this update is older than it, and a rollback to it picks those.
pub(crate) fn back_up_for_update(inst: &Instance, reason: &str, keep: u32) -> anyhow::Result<()> {
    crate::backups::cover_worlds(inst, reason)?;
    crate::backups::snapshot_instance(inst, reason, keep)?;
    Ok(())
}

/// Where `u`'s file is now and whether it's enabled: the very file that was checked if
/// it's still there, else the same file enabled or disabled since. Fails if it's gone
/// or isn't the file that was checked.
fn locate(u: &Update) -> anyhow::Result<(PathBuf, bool)> {
    let name = u.path.file_name().context("no file name")?.to_string_lossy();
    let base = name.trim_end_matches(".disabled");
    let dir = u.path.parent().context("no folder")?;
    let (on, off) = ((dir.join(base), true), (dir.join(format!("{base}.disabled")), false));
    let order = if u.path == off.0 { [off, on] } else { [on, off] };
    order
        .into_iter()
        .find(|(p, _)| net::sha1_file(p).is_some_and(|h| h.eq_ignore_ascii_case(&u.sha1)))
        .with_context(|| format!("{} changed since the update check; check again", u.title))
}

/// Update mods to the versions `check_updates` found. Nothing is written until each
/// file is found as checked (enabled or not), the game is still the one they were picked
/// for, and every required dependency is worked out. Then their mods, config and worlds
/// are backed up (stopping here if any of that fails), so it can all be rolled back.
pub async fn update(inst: &Instance, updates: Vec<Update>, keep: u32, task: &Task) -> anyhow::Result<Updated> {
    task.stage("Checking the mods", 0);
    let game = game_key(inst);
    if updates.iter().any(|u| u.game != game) {
        bail!("The instance's game changed since the update check; check again");
    }
    let found = updates.clone();
    let at: Vec<(PathBuf, bool)> = tokio::task::spawn_blocking(move || found.iter().map(locate).collect::<anyhow::Result<_>>()).await??;
    let dir = inst.game_dir().join(ContentKind::Mods.dir());
    let mut files = Vec::new();
    for u in &updates {
        let file = u.to.files.iter().find(|f| f.primary).or(u.to.files.first()).with_context(|| format!("{}: the new version has no files", u.title))?;
        if file.filename.contains(['/', '\\']) || file.filename.starts_with('.') {
            bail!("{}: odd file name {:?}", u.title, file.filename);
        }
        files.push(file.clone());
    }
    // Each update replaces its own file with its own new one: two entries for one file
    // (copies of a jar, enabled and not), or two landing on one name, would undo each other.
    let dests: Vec<PathBuf> = files.iter().zip(&at).map(|(f, (_, on))| dir.join(if *on { f.filename.clone() } else { format!("{}.disabled", f.filename) })).collect();
    for (i, u) in updates.iter().enumerate() {
        if let Some(j) = (0..i).find(|&j| at[j].0 == at[i].0 || dests[j] == dests[i]) {
            bail!("{} and {} are the same file, or would be once updated; remove one copy and check again", updates[j].title, u.title);
        }
    }

    // The new versions may need mods the old ones didn't, pinned ones at their version.
    let roots: Vec<Version> = updates.iter().map(|u| u.to.clone()).collect();
    let needs = roots.iter().flat_map(|v| &v.dependencies).any(|d| d.dependency_type == "required");
    let (plan, missing) = if needs {
        task.stage("Checking dependencies", 0);
        let mut have = installed_versions(inst, ContentKind::Mods).await;
        // What's being updated counts as its new version.
        for v in &roots {
            have.remove(&v.project_id);
        }
        resolve(&Api { kind: ContentKind::Mods, inst }, ContentKind::Mods, roots, &have).await
    } else {
        (roots, Vec::new())
    };

    task.stage("Backing up mods, config and worlds", 0);
    let names: Vec<String> = updates.iter().map(|u| u.title.clone()).collect();
    let reason = match names.as_slice() {
        [one] => format!("before updating {one}"),
        many => format!("before updating {} mods: {}", many.len(), many.join(", ")),
    };
    let i = inst.clone();
    tokio::task::spawn_blocking(move || back_up_for_update(&i, &reason, keep)).await?.context("backing up first; nothing was updated")?;

    let mut out = Updated { missing, ..Default::default() };
    for (((u, file), (path, _)), dest) in updates.iter().zip(&files).zip(&at).zip(dests) {
        task.stage(format!("Updating {}", u.title), 0);
        store::fetch(&Download { url: file.url.clone(), path: dest.clone(), sha1: Some(file.hashes.sha1.clone()), size: Some(file.size) })
            .await
            .with_context(|| format!("updating {} (roll back to undo the updates so far)", u.title))?;
        if dest != *path {
            tokio::fs::remove_file(path).await.with_context(|| format!("removing the old {}", u.title))?;
        }
        out.done.push(format!("{} {} → {}", u.title, u.from, u.to.version_number));
    }
    if plan.len() > updates.len() {
        task.stage("Installing dependencies", 0);
        out.added = fetch_new(inst, ContentKind::Mods, &plan[updates.len()..]).await.context("installing dependencies (roll back to undo the updates)")?.0;
    }
    Ok(out)
}

pub fn format_count(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{:.1}k", n as f64 / 1e3),
        _ => format!("{:.1}M", n as f64 / 1e6),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loaders::Loader;

    /// Versions newest first; `latest` takes the first of a project.
    struct Fixture(Vec<Version>);

    impl Source for Fixture {
        async fn version(&self, id: &str) -> anyhow::Result<Version> {
            self.0.iter().find(|v| v.id == id).cloned().context("no such version")
        }

        async fn latest(&self, project: &str) -> anyhow::Result<Version> {
            self.0.iter().find(|v| v.project_id == project).cloned().context("no compatible build")
        }

        async fn title(&self, _: &str) -> Option<String> {
            None
        }
    }

    /// `deps` are required (project, version) references.
    fn v(id: &str, project: &str, deps: &[(Option<&str>, Option<&str>)]) -> Version {
        Version {
            id: id.into(),
            project_id: project.into(),
            version_number: id.into(),
            files: Vec::new(),
            loaders: Vec::new(),
            date_published: None,
            game_versions: Vec::new(),
            version_type: String::new(),
            dependencies: deps
                .iter()
                .map(|(p, v)| Dependency { project_id: p.map(Into::into), version_id: v.map(Into::into), dependency_type: "required".into() })
                .collect(),
        }
    }

    fn ids(plan: &[Version]) -> Vec<&str> {
        plan.iter().map(|v| v.id.as_str()).collect()
    }

    #[tokio::test]
    async fn plans_pinned_dependencies() {
        let src = Fixture(vec![
            v("app", "app", &[(None, Some("lib-1")), (Some("api"), Some("api-1")), (Some("api"), None)]),
            v("lib-2", "lib", &[]),
            v("lib-1", "lib", &[]),
            v("api-2", "api", &[]),
            v("api-1", "api", &[]),
        ]);
        let (plan, missing) = plan(&src, ContentKind::Mods, "app", &HashMap::new()).await.unwrap();
        // A version-only reference is followed, and a pin beats the newest build.
        assert_eq!(ids(&plan), ["app", "lib-1", "api-1"]);
        assert!(missing.is_empty(), "{missing:?}");
    }

    #[tokio::test]
    async fn pin_replaces_an_earlier_unpinned_pick() {
        let src = Fixture(vec![
            v("app", "app", &[(Some("a"), None), (Some("b"), None)]),
            v("a", "a", &[(Some("lib"), None)]),
            v("b", "b", &[(Some("lib"), Some("lib-1"))]),
            v("lib-2", "lib", &[]),
            v("lib-1", "lib", &[]),
        ]);
        let (plan, _) = plan(&src, ContentKind::Mods, "app", &HashMap::new()).await.unwrap();
        assert_eq!(ids(&plan), ["app", "a", "b", "lib-1"]);
    }

    #[tokio::test]
    async fn reports_missing_dependencies() {
        let src = Fixture(vec![
            v("app", "app", &[(Some("gone"), None), (None, Some("gone-1")), (Some("x"), Some("x-1")), (Some("y"), Some("y-1"))]),
            v("x-1", "x", &[(Some("z"), Some("z-1"))]),
            v("y-1", "y", &[(Some("z"), Some("z-2"))]),
            v("z-1", "z", &[]),
            v("z-2", "z", &[]),
        ]);
        let (plan, missing) = plan(&src, ContentKind::Mods, "app", &HashMap::new()).await.unwrap();
        assert_eq!(ids(&plan), ["app", "x-1", "y-1", "z-1"]);
        assert_eq!(missing.len(), 3, "{missing:?}");
        assert!(missing[0].starts_with("gone:"));
        assert!(missing[1].starts_with("version gone-1:"));
        assert!(missing[2].contains("needs both z-1 and z-2"));
        // The project the user picked failing is an error, not a warning.
        assert!(plan_err(&src, "nope").await);
    }

    async fn plan_err(src: &Fixture, project: &str) -> bool {
        plan(src, ContentKind::Mods, project, &HashMap::new()).await.is_err()
    }

    #[tokio::test]
    async fn skips_what_is_installed() {
        let src = Fixture(vec![
            v("app", "app", &[(Some("api"), None), (Some("lib"), Some("lib-2"))]),
            v("api-2", "api", &[]),
            v("lib-2", "lib", &[]),
        ]);
        // Installed under any filename, matched by project.
        let installed = HashMap::from([("api".to_string(), "api-1".to_string()), ("lib".to_string(), "lib-1".to_string())]);
        let (plan, missing) = plan(&src, ContentKind::Mods, "app", &installed).await.unwrap();
        assert_eq!(ids(&plan), ["app"]);
        assert_eq!(missing, ["lib lib-2 is required, but another version is installed"]);
        let installed = HashMap::from([("app".to_string(), "old".to_string())]);
        assert!(super::plan(&src, ContentKind::Mods, "app", &installed).await.unwrap().0.is_empty());
    }

    #[test]
    fn updates_only_go_forward() {
        let at = |d: u32| Some(chrono::DateTime::parse_from_rfc3339(&format!("2026-01-{d:02}T00:00:00Z")).unwrap().to_utc());
        let ver = |id: &str, project: &str, day: u32, sha1: &str| Version {
            date_published: at(day),
            files: vec![VersionFile { url: String::new(), filename: format!("{id}.jar"), primary: true, size: 1, hashes: Hashes { sha1: sha1.into() } }],
            ..v(id, project, &[])
        };
        let held: Vec<Held> = ["a", "b", "c", "d"].iter().map(|h| (PathBuf::from(format!("{h}.jar")), h.to_string())).collect();
        let current = HashMap::from([
            ("a".to_string(), ver("a-1", "pa", 1, "a")),
            ("b".to_string(), ver("b-1", "pb", 1, "b")),
            ("c".to_string(), ver("c-2", "pc", 5, "c")),
            ("d".to_string(), ver("d-1", "pd", 1, "d")),
        ]);
        let latest = HashMap::from([
            // Newer: an update.
            ("a".to_string(), ver("a-2", "pa", 3, "a2")),
            ("b".to_string(), ver("b-2", "pb", 3, "b2")),
            // The newest compatible build is older than what's installed (a beta, say): no.
            ("c".to_string(), ver("c-1", "pc", 2, "c1")),
            // Already the newest.
            ("d".to_string(), ver("d-1", "pd", 1, "d")),
        ]);
        let titles = HashMap::from([("pa".to_string(), "Alpha".to_string())]);
        let ups = pick_updates(&held, &current, &latest, &titles, "1.21.1 fabric");
        assert_eq!(ups.iter().map(|u| (u.title.as_str(), u.to.id.as_str())).collect::<Vec<_>>(), [("Alpha", "a-2"), ("b-1", "b-2")]);
        assert_eq!(ups[0].from, "a-1");
    }

    /// G15: a world that can't be backed up stops the update before any mod changes.
    #[tokio::test]
    async fn an_update_stops_if_a_world_cant_be_backed_up() {
        use std::os::unix::fs::PermissionsExt;
        let mut inst = Instance::new("update stops", "1.21.1", Loader::Fabric("0.16.5".into()));
        inst.assign_id().unwrap();
        let mods = inst.game_dir().join("mods");
        std::fs::create_dir_all(&mods).unwrap();
        std::fs::write(mods.join("a-1.jar"), "a1").unwrap();
        let world = inst.saves_dir().join("W");
        std::fs::create_dir_all(world.join("region")).unwrap();
        std::fs::write(world.join("level.dat"), "L").unwrap();
        let unreadable = world.join("region/r.0.0.mca");
        std::fs::write(&unreadable, "R").unwrap();
        std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::File::open(&unreadable).is_ok() {
            eprintln!("skipped: running as root, the file is still readable");
            std::fs::remove_dir_all(inst.dir()).unwrap();
            return;
        }
        let file = VersionFile { url: "https://cdn.modrinth.com/a-2.jar".into(), filename: "a-2.jar".into(), primary: true, size: 2, hashes: Hashes { sha1: "0".repeat(40) } };
        let up = Update { path: mods.join("a-1.jar"), sha1: net::sha1_file(&mods.join("a-1.jar")).unwrap(), game: game_key(&inst), title: "A".into(), from: "1".into(), to: Version { files: vec![file], ..v("a-2", "a", &[]) } };
        let e = update(&inst, vec![up], 5, &Task::new("t")).await.unwrap_err();
        assert!(format!("{e:#}").contains("couldn't back up W"), "{e:#}");
        let left: Vec<String> = std::fs::read_dir(&mods).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(left, ["a-1.jar"], "nothing changed");
        assert!(crate::backups::instance_snapshots(&inst.id).is_empty(), "the mods snapshot comes after the worlds'");
        std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::remove_dir_all(inst.dir()).unwrap();
        crate::backups::forget_instance(&inst.id).unwrap();
    }

    /// G18: an update's dependencies keep their pins: an installed other version is
    /// reported, a version-only reference is followed, and a missing pinned project gets
    /// the pinned build, not its newest.
    #[tokio::test]
    async fn update_dependencies_keep_their_pins() {
        let src = Fixture(vec![
            v("lib-2", "lib", &[]),
            v("only-1", "only", &[]),
            v("other-3", "other", &[]),
            v("other-2", "other", &[]),
        ]);
        let a2 = v("a-2", "a", &[(Some("lib"), Some("lib-2")), (None, Some("only-1")), (Some("other"), Some("other-2"))]);
        let installed = HashMap::from([("lib".to_string(), "lib-1".to_string())]);
        let (plan, missing) = resolve(&src, ContentKind::Mods, vec![a2], &installed).await;
        assert_eq!(ids(&plan), ["a-2", "only-1", "other-2"]);
        assert_eq!(missing, ["lib lib-2 is required, but another version is installed"]);
    }

    /// The bytes as a file in the content store, so `store::fetch` needs no network. Returns its sha1.
    fn in_store(bytes: &str) -> String {
        let tmp = crate::paths::test_root().join(format!("blob-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(tmp.parent().unwrap()).unwrap();
        std::fs::write(&tmp, bytes).unwrap();
        let sha1 = net::sha1_file(&tmp).unwrap();
        std::fs::create_dir_all(store::dir()).unwrap();
        std::fs::rename(&tmp, store::dir().join(&sha1)).unwrap();
        sha1
    }

    fn mod_names(dir: &std::path::Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        names
    }

    /// G17: an update follows the file as it is when it runs (disabled since the check
    /// stays disabled), and refuses a list checked for another game or a replaced file.
    #[tokio::test]
    async fn an_update_follows_the_file_as_it_is_now() {
        let mut inst = Instance::new("update now", "1.21.1", Loader::Fabric("0.16.5".into()));
        inst.assign_id().unwrap();
        let mods = inst.game_dir().join("mods");
        std::fs::create_dir_all(&mods).unwrap();
        std::fs::write(mods.join("a-1.jar"), "a1").unwrap();
        let new = in_store("a2");
        let file = VersionFile { url: "https://cdn.modrinth.com/a-2.jar".into(), filename: "a-2.jar".into(), primary: true, size: 2, hashes: Hashes { sha1: new } };
        let up = Update {
            path: mods.join("a-1.jar"),
            sha1: net::sha1_file(&mods.join("a-1.jar")).unwrap(),
            game: game_key(&inst),
            title: "A".into(),
            from: "1".into(),
            to: Version { files: vec![file], ..v("a-2", "a", &[]) },
        };

        // Checked for another game: nothing happens.
        let mut other = inst.clone();
        other.mc_version = "1.20.1".into();
        assert!(update(&other, vec![up.clone()], 5, &Task::new("t")).await.is_err());
        // The file was replaced since the check: nothing happens.
        std::fs::write(mods.join("a-1.jar"), "a1 rebuilt").unwrap();
        assert!(format!("{:#}", update(&inst, vec![up.clone()], 5, &Task::new("t")).await.unwrap_err()).contains("changed since"));
        assert_eq!(mod_names(&mods), ["a-1.jar"]);

        // Disabled since the check: updated, and still disabled.
        std::fs::write(mods.join("a-1.jar"), "a1").unwrap();
        std::fs::rename(mods.join("a-1.jar"), mods.join("a-1.jar.disabled")).unwrap();
        update(&inst, vec![up.clone()], 5, &Task::new("t")).await.unwrap();
        assert_eq!(mod_names(&mods), ["a-2.jar.disabled"]);

        // Identical enabled and disabled copies: updating the disabled one replaces that
        // one, not its enabled twin.
        std::fs::remove_file(mods.join("a-2.jar.disabled")).unwrap();
        std::fs::write(mods.join("a-1.jar"), "a1").unwrap();
        std::fs::write(mods.join("a-1.jar.disabled"), "a1").unwrap();
        let off = Update { path: mods.join("a-1.jar.disabled"), ..up.clone() };
        in_store("a2");
        update(&inst, vec![off.clone()], 5, &Task::new("t")).await.unwrap();
        assert_eq!(mod_names(&mods), ["a-1.jar", "a-2.jar.disabled"]);
        // Two entries that end up at one file stop before anything changes.
        std::fs::remove_file(mods.join("a-2.jar.disabled")).unwrap();
        std::fs::write(mods.join("a-1.jar.disabled"), "a1").unwrap();
        std::fs::remove_file(mods.join("a-1.jar")).unwrap();
        let err = update(&inst, vec![up.clone(), off], 5, &Task::new("t")).await.unwrap_err();
        assert!(format!("{err:#}").contains("same file"), "{err:#}");
        assert_eq!(mod_names(&mods), ["a-1.jar.disabled"]);
        std::fs::remove_dir_all(inst.dir()).unwrap();
        crate::backups::forget_instance(&inst.id).unwrap();
    }

    /// Hits the network; run with `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn modpacks_are_found_with_their_versions() {
        let hits = search_packs("fabulously optimized").await.unwrap().hits;
        let fo = hits.iter().find(|h| h.slug == "fabulously-optimized").expect("found by name");
        let versions = pack_versions(&fo.project_id).await.unwrap();
        assert!(versions.len() > 10);
        assert!(versions.iter().all(|v| v.project_id == fo.project_id && !v.game_versions.is_empty()));
        assert!(versions.windows(2).all(|w| w[0].date_published >= w[1].date_published), "newest first");
        assert!(!search_packs("").await.unwrap().hits.is_empty());
    }

    /// Hits the network; run with `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn finds_and_applies_updates() {
        let mut inst = Instance::new("updates", "1.21.1", Loader::Fabric("0.16.5".into()));
        inst.assign_id().unwrap();
        // The oldest Fabric build of Sodium for 1.21.1, disabled to check that sticks.
        let url = format!("{API}/project/sodium/version?game_versions={}&loaders={}", enc(r#"["1.21.1"]"#), enc(r#"["fabric"]"#));
        let versions: Vec<Version> = net::get_json(&url).await.unwrap();
        let old = versions.last().unwrap();
        let f = old.files.iter().find(|f| f.primary).unwrap();
        let path = inst.game_dir().join("mods").join(format!("{}.disabled", f.filename));
        store::fetch(&Download { url: f.url.clone(), path: path.clone(), sha1: Some(f.hashes.sha1.clone()), size: Some(f.size) }).await.unwrap();

        let ups = check_updates(&inst).await.unwrap();
        assert_eq!(ups.len(), 1, "{ups:?}");
        assert_eq!(ups[0].title, "Sodium");
        let to = ups[0].to.files.iter().find(|f| f.primary).unwrap().filename.clone();
        let done = update(&inst, ups, 5, &Task::new("t")).await.unwrap();
        assert_eq!(done.done.len(), 1);
        assert!(!path.exists());
        assert!(inst.game_dir().join("mods").join(format!("{to}.disabled")).exists());
        assert!(check_updates(&inst).await.unwrap().is_empty(), "up to date now");

        // And back.
        let snap = crate::backups::instance_snapshots(&inst.id).remove(0);
        crate::backups::roll_back(&inst, &snap, 5).unwrap();
        assert!(path.exists());
        std::fs::remove_dir_all(inst.dir()).unwrap();
        crate::backups::forget_instance(&inst.id).unwrap();
    }

    /// Hits the network; run with `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn installs_packs() {
        let mut inst = Instance::new("t", "1.21.1", Loader::Fabric("0.16.5".into()));
        inst.assign_id().unwrap();
        inst.save().unwrap();
        for (kind, slug) in [(ContentKind::ShaderPacks, "complementary-reimagined"), (ContentKind::ResourcePacks, "fresh-animations")] {
            let hits = search(kind, "", &inst).await.unwrap().hits;
            assert!(!hits.is_empty(), "{kind:?} search empty");
            let done = install(kind, slug, &inst).await.unwrap();
            println!("{kind:?}: {done:?}");
            assert_eq!(inst.content(kind).len(), 1);
        }
        std::fs::remove_dir_all(inst.dir()).unwrap();
    }
}
