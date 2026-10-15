//! Skins and capes on the Minecraft profile (api.minecraftservices.com), and
//! their textures, cached on disk by the hash in their URL. Offline accounts
//! get vanilla's default for their UUID (Steve or Alex).

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use image::RgbaImage;
use serde::Deserialize;

use crate::auth::{self, MsaAccount};
use crate::{net, paths};

const PROFILE_URL: &str = "https://api.minecraftservices.com/minecraft/profile";
const STEVE: &str = "31f477eb1a7beee631c2ca64d06f8f68fa93a3386d04452ab27f43acdf1b60cb";
const ALEX: &str = "46acd06e8483b176e8ea39fc12fe105eb3a2a4970f5100057e9d84d4b60bdfa7";

#[derive(Debug, Clone, Deserialize)]
pub struct Profile {
    #[serde(default)]
    pub skins: Vec<Texture>,
    #[serde(default)]
    pub capes: Vec<Texture>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Texture {
    pub id: String,
    pub state: String,
    pub url: String,
    /// Skins: `CLASSIC` or `SLIM`.
    #[serde(default)]
    pub variant: Option<String>,
    /// Capes: their name, e.g. `Migrator`.
    #[serde(default)]
    pub alias: Option<String>,
}

impl Texture {
    fn active(&self) -> bool {
        self.state == "ACTIVE"
    }
}

impl Profile {
    pub fn skin(&self) -> Option<&Texture> {
        self.skins.iter().find(|s| s.active())
    }

    pub fn cape(&self) -> Option<&Texture> {
        self.capes.iter().find(|c| c.active())
    }
}

/// What the preview draws.
#[derive(Clone)]
pub struct Look {
    /// Changes whenever the textures or model do.
    pub key: String,
    pub skin: RgbaImage,
    pub slim: bool,
    pub cape: Option<RgbaImage>,
}

pub struct Loaded {
    /// Set when the Minecraft token had to be refreshed first.
    pub account: Option<MsaAccount>,
    /// None for offline accounts.
    pub profile: Option<Profile>,
    pub look: Look,
}

#[derive(Clone)]
pub enum Change {
    None,
    Upload { png: Vec<u8>, slim: bool },
    /// Keep the texture, switch between classic and slim arms. It's sent
    /// again as a file: the API's `{"url": …}` form is blocked by its firewall.
    Variant { url: String, slim: bool },
    Reset,
    Cape(Option<String>),
}

fn variant(slim: bool) -> &'static str {
    if slim { "slim" } else { "classic" }
}

/// Make `change` (if any) and return the profile as it is afterwards.
/// The token is refreshed first if it has expired, or if Minecraft rejects it.
pub async fn apply(client_id: &str, acct: &MsaAccount, change: Change) -> anyhow::Result<Loaded> {
    let mut refreshed = if acct.mc_expires <= chrono::Utc::now() { Some(auth::refresh(client_id, acct).await?) } else { None };
    let token = refreshed.as_ref().unwrap_or(acct).mc_token.clone();
    let (profile, look) = match run(&token, &acct.uuid, change.clone()).await {
        Err(e) if refreshed.is_none() && e.downcast_ref::<Rejected>().is_some() => {
            let a = auth::refresh(client_id, acct).await?;
            let r = run(&a.mc_token, &acct.uuid, change).await?;
            refreshed = Some(a);
            r
        }
        r => r?,
    };
    Ok(Loaded { account: refreshed, profile: Some(profile), look })
}

async fn run(token: &str, uuid: &str, change: Change) -> anyhow::Result<(Profile, Look)> {
    let c = net::client();
    let upload = |png: Vec<u8>, slim: bool| -> anyhow::Result<_> {
        let part = reqwest::multipart::Part::bytes(png).file_name("skin.png").mime_str("image/png")?;
        let form = reqwest::multipart::Form::new().text("variant", variant(slim)).part("file", part);
        Ok(c.post(format!("{PROFILE_URL}/skins")).multipart(form))
    };
    let req = match change {
        Change::None => None,
        Change::Upload { png, slim } => Some(upload(png, slim)?),
        Change::Variant { url, slim } => Some(upload(texture_png(&url).await?, slim)?),
        Change::Reset => Some(c.delete(format!("{PROFILE_URL}/skins/active"))),
        Change::Cape(Some(id)) => Some(c.put(format!("{PROFILE_URL}/capes/active")).json(&serde_json::json!({ "capeId": id }))),
        Change::Cape(None) => Some(c.delete(format!("{PROFILE_URL}/capes/active"))),
    };
    if let Some(req) = req {
        checked(req.bearer_auth(token).send().await?).await?;
    }
    let resp = checked(c.get(PROFILE_URL).bearer_auth(token).send().await?).await?;
    let profile: Profile = resp.json().await.context("parsing the Minecraft profile")?;
    let look = match profile.skin() {
        Some(s) => {
            let slim = s.variant.as_deref().is_some_and(|v| v.eq_ignore_ascii_case("slim"));
            let cape = match profile.cape() {
                Some(c) => Some(texture(&c.url).await?),
                None => None,
            };
            let key = format!("{}:{slim}:{}", s.url, profile.cape().map(|c| c.url.as_str()).unwrap_or_default());
            Look { key, skin: texture(&s.url).await?, slim, cape }
        }
        None => default_look(uuid).await?.look,
    };
    Ok((profile, look))
}

/// Minecraft refused the token (HTTP 401).
#[derive(Debug)]
struct Rejected;

impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("Minecraft rejected the session; sign in again")
    }
}

impl std::error::Error for Rejected {}

async fn checked(resp: reqwest::Response) -> anyhow::Result<reqwest::Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let text = resp.text().await.unwrap_or_default();
    match status.as_u16() {
        401 => Err(Rejected.into()),
        429 => bail!("Too many skin changes; wait a minute and try again"),
        _ => bail!("Minecraft services: {status}{}", error_detail(&text).map(|m| format!(": {m}")).unwrap_or_default()),
    }
}

/// The useful part of an error body: the JSON message, or the heading of
/// an HTML error page (the API's firewall answers with one).
fn error_detail(body: &str) -> Option<String> {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
        return v["errorMessage"].as_str().or(v["error"].as_str()).map(str::to_string);
    }
    let body = body.trim();
    if body.starts_with('<') {
        let inner = |tag: &str| {
            let start = body.find(&format!("<{tag}>"))? + tag.len() + 2;
            let end = body[start..].find(&format!("</{tag}>"))? + start;
            Some(body[start..end].trim().to_string()).filter(|s| !s.is_empty())
        };
        return inner("h2").or_else(|| inner("h1")).or_else(|| inner("title"));
    }
    let line = body.lines().next().unwrap_or_default();
    (!line.is_empty()).then(|| line.chars().take(160).collect())
}

/// Vanilla's default for a UUID, as the game picked it before 1.19.3:
/// Alex when Java's `UUID.hashCode()` is odd.
pub fn default_is_slim(uuid: &str) -> bool {
    let hex: String = uuid.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    let (Ok(most), Ok(least)) = (u64::from_str_radix(hex.get(..16).unwrap_or(""), 16), u64::from_str_radix(hex.get(16..).unwrap_or(""), 16))
    else {
        return false;
    };
    let hilo = most ^ least;
    (((hilo >> 32) as i32) ^ (hilo as i32)) & 1 == 1
}

pub async fn default_look(uuid: &str) -> anyhow::Result<Loaded> {
    let slim = default_is_slim(uuid);
    let hash = if slim { ALEX } else { STEVE };
    let skin = texture(&format!("https://textures.minecraft.net/texture/{hash}")).await.context("downloading the default skin")?;
    Ok(Loaded { account: None, profile: None, look: Look { key: hash.to_string(), skin, slim, cape: None } })
}

fn texture_cache() -> PathBuf {
    paths::data_dir().join("cache").join("textures")
}

/// A skin or cape texture. Their URLs end in the content hash, so a cached
/// copy never goes stale.
async fn texture(url: &str) -> anyhow::Result<RgbaImage> {
    Ok(image::load_from_memory(&texture_png(url).await?).context("decoding texture")?.to_rgba8())
}

async fn texture_png(url: &str) -> anyhow::Result<Vec<u8>> {
    let hash = url.rsplit('/').next().filter(|h| !h.is_empty() && h.chars().all(|c| c.is_ascii_alphanumeric())).context("odd texture URL")?;
    let path = texture_cache().join(format!("{hash}.png"));
    let bytes = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(_) => {
            let url = url.replacen("http://", "https://", 1);
            let resp = net::client().get(&url).send().await?.error_for_status()?;
            let b = resp.bytes().await?.to_vec();
            tokio::fs::create_dir_all(texture_cache()).await?;
            tokio::fs::write(&path, &b).await?;
            b
        }
    };
    Ok(bytes)
}

/// A skin file to upload: a PNG of 64×64, or legacy 64×32.
pub fn read_skin_file(path: &Path) -> anyhow::Result<(Vec<u8>, RgbaImage)> {
    let png = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    if image::guess_format(&png).ok() != Some(image::ImageFormat::Png) {
        bail!("A skin must be a PNG file");
    }
    let img = image::load_from_memory(&png)?.to_rgba8();
    if img.dimensions() != (64, 64) && img.dimensions() != (64, 32) {
        bail!("A skin must be 64×64 (or legacy 64×32); this one is {}×{}", img.width(), img.height());
    }
    Ok((png, img))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_follows_java_hash_code() {
        // hilo = 1 → hashCode 1 (odd): Alex.
        assert!(default_is_slim("00000000-0000-0001-0000-000000000000"));
        // hilo = 0 → 0: Steve.
        assert!(!default_is_slim("00000000000000000000000000000000"));
        // Upper and lower halves cancel out: 0x1_00000001 → 1 ^ 1 = 0.
        assert!(!default_is_slim("00000001000000010000000000000000"));
        assert!(!default_is_slim("not a uuid"));
    }

    #[test]
    fn error_details_stay_short() {
        let html = "<!DOCTYPE html><html><head><title>Service unavailable</title></head><body><div id='message'><h2>The request is blocked.</h2></div></body></html>";
        assert_eq!(error_detail(html).as_deref(), Some("The request is blocked."));
        assert_eq!(error_detail(r#"{"errorMessage":"Bad skin"}"#).as_deref(), Some("Bad skin"));
        assert_eq!(error_detail(""), None);
    }

    #[test]
    fn profile_parses() {
        let p: Profile = serde_json::from_str(
            r#"{"id":"x","name":"n","skins":[{"id":"s","state":"ACTIVE","url":"http://textures.minecraft.net/texture/ab","textureKey":"ab","variant":"SLIM"}],
               "capes":[{"id":"c1","state":"INACTIVE","url":"u","alias":"Migrator"},{"id":"c2","state":"ACTIVE","url":"v","alias":"Pan"}]}"#,
        )
        .unwrap();
        assert_eq!(p.skin().unwrap().variant.as_deref(), Some("SLIM"));
        assert_eq!(p.cape().unwrap().alias.as_deref(), Some("Pan"));
    }
}
