//! Accounts: Microsoft (device code -> Xbox Live -> XSTS -> Minecraft) and
//! offline. Mirrors Prism's MSADeviceCodeStep/XboxUserStep/XboxAuthorizationStep/
//! LauncherLoginStep/MinecraftProfileStep chain.

use anyhow::{Context, bail};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{net, paths};

const SCOPE: &str = "XboxLive.SignIn XboxLive.offline_access";
const DEVICE_CODE_URL: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0/devicecode";
const TOKEN_URL: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0/token";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Account {
    Msa(MsaAccount),
    Offline { name: String, uuid: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MsaAccount {
    pub name: String,
    pub uuid: String,
    pub refresh_token: String,
    pub mc_token: String,
    pub mc_expires: DateTime<Utc>,
    pub xuid: String,
}

impl Account {
    pub fn name(&self) -> &str {
        match self {
            Account::Msa(a) => &a.name,
            Account::Offline { name, .. } => name,
        }
    }

    pub fn uuid(&self) -> &str {
        match self {
            Account::Msa(a) => &a.uuid,
            Account::Offline { uuid, .. } => uuid,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Account::Msa(_) => "microsoft",
            Account::Offline { .. } => "offline",
        }
    }

    pub fn offline(name: &str) -> Self {
        Account::Offline { name: name.to_string(), uuid: offline_uuid(name) }
    }
}

/// Java's `UUID.nameUUIDFromBytes("OfflinePlayer:<name>")`, which is what
/// vanilla servers in offline mode assign.
pub fn offline_uuid(name: &str) -> String {
    use md5::{Digest, Md5};
    let mut h: [u8; 16] = Md5::digest(format!("OfflinePlayer:{name}").as_bytes()).into();
    h[6] = (h[6] & 0x0f) | 0x30;
    h[8] = (h[8] & 0x3f) | 0x80;
    hex::encode(h)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Accounts {
    pub accounts: Vec<Account>,
    pub active: Option<String>,
}

impl Accounts {
    pub fn load() -> Self {
        std::fs::read_to_string(paths::accounts_file())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Written whole and owner-only, even over an older, wider-mode file.
    pub fn save(&self) -> anyhow::Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        crate::fsutil::write_atomic(&paths::accounts_file(), json.as_bytes(), 0o600)?;
        Ok(())
    }

    pub fn active(&self) -> Option<&Account> {
        let uuid = self.active.as_deref()?;
        self.accounts.iter().find(|a| a.uuid() == uuid)
    }

    pub fn upsert(&mut self, acct: Account) {
        let uuid = acct.uuid().to_string();
        match self.accounts.iter_mut().find(|a| a.uuid() == uuid) {
            Some(slot) => *slot = acct,
            None => self.accounts.push(acct),
        }
        if self.active.is_none() {
            self.active = Some(uuid);
        }
    }

    /// Store refreshed tokens, but only for an account that's still here: a
    /// refresh that finishes after the user removed it mustn't bring it back.
    /// Returns whether it was stored.
    pub fn refresh(&mut self, acct: Account) -> bool {
        match self.accounts.iter_mut().find(|a| a.uuid() == acct.uuid()) {
            Some(slot) => {
                *slot = acct;
                true
            }
            None => false,
        }
    }

    pub fn remove(&mut self, uuid: &str) {
        self.accounts.retain(|a| a.uuid() != uuid);
        if self.active.as_deref() == Some(uuid) {
            self.active = self.accounts.first().map(|a| a.uuid().to_string());
        }
    }
}

// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    pub interval: u64,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

pub async fn begin_device_code(client_id: &str) -> anyhow::Result<DeviceCode> {
    let resp = net::client()
        .post(DEVICE_CODE_URL)
        .form(&[("client_id", client_id), ("scope", SCOPE)])
        .send()
        .await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        bail!("device code request failed ({status}): {text}");
    }
    Ok(serde_json::from_str(&text)?)
}

/// Poll until the user finishes signing in, then run the full chain.
pub async fn finish_device_code(client_id: &str, code: DeviceCode) -> anyhow::Result<MsaAccount> {
    let deadline = Utc::now() + Duration::seconds(code.expires_in as i64);
    let mut interval = code.interval.max(1);
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
        if Utc::now() > deadline {
            bail!("sign-in code expired");
        }
        let resp: TokenResponse = net::client()
            .post(TOKEN_URL)
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("client_id", client_id),
                ("device_code", &code.device_code),
            ])
            .send()
            .await?
            .json()
            .await?;
        match (resp.access_token, resp.error.as_deref()) {
            (Some(access), _) => {
                let refresh = resp.refresh_token.context("no refresh token returned")?;
                return login_with_msa(&access, refresh).await;
            }
            (None, Some("authorization_pending")) => {}
            (None, Some("slow_down")) => interval += 5,
            (None, err) => bail!(
                "sign-in failed: {} {}",
                err.unwrap_or("unknown"),
                resp.error_description.unwrap_or_default()
            ),
        }
    }
}

pub async fn refresh(client_id: &str, acct: &MsaAccount) -> anyhow::Result<MsaAccount> {
    let resp: TokenResponse = net::client()
        .post(TOKEN_URL)
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", client_id),
            ("refresh_token", &acct.refresh_token),
            ("scope", SCOPE),
        ])
        .send()
        .await?
        .json()
        .await?;
    let access = resp.access_token.with_context(|| {
        format!(
            "refresh failed, sign in again: {}",
            resp.error_description.or(resp.error).unwrap_or_default()
        )
    })?;
    let refresh = resp.refresh_token.unwrap_or_else(|| acct.refresh_token.clone());
    login_with_msa(&access, refresh).await
}

async fn post_json(url: &str, body: serde_json::Value) -> anyhow::Result<serde_json::Value> {
    let resp = net::client()
        .post(url)
        .header("Accept", "application/json")
        .header("x-xbl-contract-version", "1")
        .json(&body)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text)
            && let Some(x) = v.get("XErr").and_then(|x| x.as_u64())
        {
            bail!("{}", xerr_message(x));
        }
        bail!("POST {url} failed ({status}): {text}");
    }
    Ok(serde_json::from_str(&text)?)
}

fn xerr_message(code: u64) -> String {
    match code {
        2148916233 => "This Microsoft account has no Xbox profile. Sign in once at minecraft.net to create one.".into(),
        2148916235 => "Xbox Live is not available in this account's region.".into(),
        2148916236 | 2148916237 => "This account needs adult verification on the Xbox site.".into(),
        2148916238 => "This is a child account; an adult must add it to a Microsoft family.".into(),
        c => format!("Xbox Live rejected the sign-in (XErr {c})."),
    }
}

async fn login_with_msa(msa_token: &str, refresh_token: String) -> anyhow::Result<MsaAccount> {
    let xbl = post_json(
        "https://user.auth.xboxlive.com/user/authenticate",
        json!({
            "Properties": {
                "AuthMethod": "RPS",
                "SiteName": "user.auth.xboxlive.com",
                "RpsTicket": format!("d={msa_token}"),
            },
            "RelyingParty": "http://auth.xboxlive.com",
            "TokenType": "JWT",
        }),
    )
    .await?;
    let xbl_token = xbl["Token"].as_str().context("no Xbox token")?;

    let xsts = post_json(
        "https://xsts.auth.xboxlive.com/xsts/authorize",
        json!({
            "Properties": { "SandboxId": "RETAIL", "UserTokens": [xbl_token] },
            "RelyingParty": "rp://api.minecraftservices.com/",
            "TokenType": "JWT",
        }),
    )
    .await?;
    let xsts_token = xsts["Token"].as_str().context("no XSTS token")?;
    let claims = &xsts["DisplayClaims"]["xui"][0];
    let uhs = claims["uhs"].as_str().context("no user hash")?;
    let xuid = claims["xid"].as_str().unwrap_or_default().to_string();

    let mc = post_json(
        "https://api.minecraftservices.com/authentication/login_with_xbox",
        json!({ "identityToken": format!("XBL3.0 x={uhs};{xsts_token}") }),
    )
    .await
    .context("Minecraft services rejected the login. If you set your own client ID in settings, it must be approved for the Minecraft API.")?;
    let mc_token = mc["access_token"].as_str().context("no Minecraft token")?.to_string();
    let expires_in = mc["expires_in"].as_i64().unwrap_or(86400);

    let resp = net::client()
        .get("https://api.minecraftservices.com/minecraft/profile")
        .bearer_auth(&mc_token)
        .send()
        .await?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        bail!("This account doesn't own Minecraft: Java Edition (or hasn't picked a username yet).");
    }
    let profile: serde_json::Value = resp.error_for_status()?.json().await?;

    Ok(MsaAccount {
        name: profile["name"].as_str().context("no profile name")?.to_string(),
        uuid: profile["id"].as_str().context("no profile id")?.to_string(),
        refresh_token,
        mc_token,
        mc_expires: Utc::now() + Duration::seconds(expires_in - 60),
        xuid,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn late_refresh_does_not_resurrect() {
        let mut a = Accounts::default();
        a.upsert(Account::offline("Steve"));
        assert!(a.refresh(Account::offline("Steve")));
        a.remove(&offline_uuid("Steve"));
        assert!(!a.refresh(Account::offline("Steve")));
        assert!(a.accounts.is_empty() && a.active.is_none());
    }

    #[test]
    fn accounts_file_is_owner_only() {
        let path = paths::accounts_file();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let mut a = Accounts::default();
        a.upsert(Account::offline("Alex"));
        a.save().unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        std::fs::remove_file(path).unwrap();
    }
}
