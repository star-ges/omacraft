use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, bail};
use futures::StreamExt;
use sha1::{Digest, Sha1};

use crate::task::Task;

pub const USER_AGENT: &str = concat!("omacraft/", env!("CARGO_PKG_VERSION"), " (minecraft launcher)");

pub fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(std::time::Duration::from_secs(15))
            .build()
            .expect("http client")
    })
}

pub async fn get_json<T: serde::de::DeserializeOwned>(url: &str) -> anyhow::Result<T> {
    let resp = client().get(url).send().await.with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        bail!("GET {url}: HTTP {status}");
    }
    resp.json().await.with_context(|| format!("parsing {url}"))
}

/// Fetch JSON, caching it on disk so the launcher still works offline.
pub async fn get_json_cached<T: serde::de::DeserializeOwned>(url: &str, cache: &Path) -> anyhow::Result<T> {
    match client().get(url).send().await.and_then(|r| r.error_for_status()) {
        Ok(resp) => {
            let bytes = resp.bytes().await?;
            let v = serde_json::from_slice(&bytes).with_context(|| format!("parsing {url}"))?;
            if let Some(dir) = cache.parent() {
                tokio::fs::create_dir_all(dir).await?;
            }
            tokio::fs::write(cache, &bytes).await?;
            Ok(v)
        }
        Err(e) => {
            let bytes = tokio::fs::read(cache).await.map_err(|_| e)?;
            Ok(serde_json::from_slice(&bytes)?)
        }
    }
}

/// First 12 hex chars of a SHA-1, for cache keys.
pub fn short_hash(bytes: &[u8]) -> String {
    hex::encode(Sha1::digest(bytes))[..12].to_string()
}

pub fn sha1_file(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    Some(hex::encode(Sha1::digest(&bytes)))
}

#[derive(Debug, Clone)]
pub struct Download {
    pub url: String,
    pub path: PathBuf,
    pub sha1: Option<String>,
    pub size: Option<u64>,
}

impl Download {
    /// Present and (if we know the hash) intact.
    pub fn is_satisfied(&self) -> bool {
        let Ok(meta) = std::fs::metadata(&self.path) else {
            return false;
        };
        if let Some(size) = self.size
            && meta.len() != size
        {
            return false;
        }
        match &self.sha1 {
            // Size matched; hashing every library on every launch is slow,
            // so only hash when size is unknown.
            Some(h) if self.size.is_none() => sha1_file(&self.path).as_deref() == Some(h.as_str()),
            _ => true,
        }
    }
}

pub async fn download(d: &Download) -> anyhow::Result<()> {
    let mut last_err = None;
    for attempt in 0..3 {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(500 * attempt)).await;
        }
        match download_once(d).await {
            Ok(()) => return Ok(()),
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap())
}

async fn download_once(d: &Download) -> anyhow::Result<()> {
    let resp = client()
        .get(&d.url)
        .send()
        .await
        .with_context(|| format!("GET {}", d.url))?
        .error_for_status()
        .with_context(|| format!("GET {}", d.url))?;
    if let Some(dir) = d.path.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    // A temp file of our own, so concurrent downloads of the same file (or of
    // foo.jar and foo.json) never write into each other's.
    let name = d.path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = d.path.with_file_name(format!(".{name}.{}.part", uuid::Uuid::new_v4().simple()));
    let r = write_verified(d, resp, &tmp).await;
    match r {
        Ok(()) => tokio::fs::rename(&tmp, &d.path).await.map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            e.into()
        }),
        Err(e) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            Err(e)
        }
    }
}

async fn write_verified(d: &Download, resp: reqwest::Response, tmp: &Path) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut hasher = Sha1::new();
    let mut file = tokio::fs::OpenOptions::new().write(true).create_new(true).open(tmp).await?;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        hasher.update(&chunk);
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    if let Some(expected) = &d.sha1 {
        let got = hex::encode(hasher.finalize());
        if !got.eq_ignore_ascii_case(expected) {
            bail!("checksum mismatch for {} (expected {expected}, got {got})", d.url);
        }
    }
    Ok(())
}

/// One download per destination. The asset index lists the same object under
/// several names; two different expected files for one path is an error.
fn dedupe(items: Vec<Download>) -> anyhow::Result<Vec<Download>> {
    let mut seen: std::collections::HashMap<PathBuf, usize> = std::collections::HashMap::new();
    let mut out: Vec<Download> = Vec::with_capacity(items.len());
    for d in items {
        match seen.get(&d.path) {
            Some(&i) => {
                let same = |a: &Option<String>, b: &Option<String>| match (a, b) {
                    (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
                    _ => true,
                };
                let o = &mut out[i];
                if !same(&o.sha1, &d.sha1) || o.size.zip(d.size).is_some_and(|(a, b)| a != b) {
                    bail!("conflicting downloads for {}", d.path.display());
                }
                // Keep whichever knows more.
                o.sha1 = o.sha1.take().or(d.sha1);
                o.size = o.size.or(d.size);
            }
            None => {
                seen.insert(d.path.clone(), out.len());
                out.push(d);
            }
        }
    }
    Ok(out)
}

/// Download everything missing, `parallel` at a time, ticking the task.
pub async fn download_all(task: &Task, label: &str, items: Vec<Download>, parallel: usize) -> anyhow::Result<()> {
    let items = dedupe(items)?;
    let missing: Vec<Download> = tokio::task::spawn_blocking(move || {
        items.into_iter().filter(|d| !d.is_satisfied()).collect()
    })
    .await?;
    if missing.is_empty() {
        return Ok(());
    }
    task.stage(format!("{label} ({})", missing.len()), missing.len() as u64);
    let results: Vec<anyhow::Result<()>> = futures::stream::iter(missing)
        .map(|d| async move {
            let r = download(&d).await;
            task.inc(1);
            r
        })
        .buffer_unordered(parallel)
        .collect()
        .await;
    results.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dl(path: &str, sha1: Option<&str>) -> Download {
        Download { url: format!("https://x/{path}"), path: path.into(), sha1: sha1.map(String::from), size: None }
    }

    #[test]
    fn dedupes_same_destination() {
        let out = dedupe(vec![dl("a", Some("AB")), dl("b", None), dl("a", Some("ab")), dl("b", Some("cd"))]).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].sha1.as_deref(), Some("cd"));
        assert!(dedupe(vec![dl("a", Some("ab")), dl("a", Some("cd"))]).is_err());
    }
}
