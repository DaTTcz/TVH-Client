//! Kontrola aktualizací a self-update přes GitHub Releases
//! (`https://github.com/DaTTcz/TVH-Client`).
//!
//! ## Jak self-update funguje
//!
//! Windows nedovolí přepsat/smazat soubor běžícího `.exe`, ale dovolí ho
//! **přejmenovat** (souborový handle zůstává platný). Na tomhle stojí
//! standardní trik pro self-update na Windows bez instalátoru:
//!
//! 1. Stáhneme nový `.exe` vedle starého jako `tvh-client.exe.new`.
//! 2. Vytvoříme a spustíme *odpojený* (detached) PowerShell skript, který
//!    nezávisí na běhu tohohle procesu.
//! 3. Tenhle proces hned skončí (`std::process::exit`), čímž uvolní
//!    zámek na `tvh-client.exe`.
//! 4. Skript počká, přejmenuje starý `.exe` stranou, přejmenuje nový na
//!    jeho místo, znovu appku spustí a uklidí po sobě.
//!
//! Release, ze kterého se stahuje, musí obsahovat asset přesně
//! pojmenovaný podle [`ASSET_NAME`] - o to se stará
//! `.github/workflows/release.yml`.

use serde::Deserialize;
use std::process::Command;

pub const REPO_OWNER: &str = "DaTTcz";
pub const REPO_NAME: &str = "TVH-Client";
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Musí odpovídat názvu assetu nahrávaného v `.github/workflows/release.yml`.
pub const ASSET_NAME: &str = "tvh-client.exe";

#[derive(Debug, Clone)]
pub struct ReleaseInfo {
    pub version: String,
    pub is_newer: bool,
    pub download_url: String,
}

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
    assets: Vec<GithubAsset>,
}

#[derive(Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
}

fn http_client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .user_agent(concat!("tvh-client/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| e.to_string())
}

/// Zjistí nejnovější release na GitHubu a porovná ho s aktuální verzí.
/// Volat z pozadí (síťové volání) - viz `app.rs` `start_update_check`.
pub fn check_latest() -> Result<ReleaseInfo, String> {
    let url = format!("https://api.github.com/repos/{REPO_OWNER}/{REPO_NAME}/releases/latest");
    let client = http_client()?;
    let resp = client.get(&url).send().map_err(|e| e.to_string())?;
    let status = resp.status();
    let body = resp.text().map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("GitHub API vrátilo {status}: {body}"));
    }

    let release: GithubRelease =
        serde_json::from_str(&body).map_err(|e| format!("Nečekaná odpověď GitHubu: {e}"))?;

    let latest = release.tag_name.trim_start_matches('v').to_string();
    let is_newer = is_version_newer(&latest, CURRENT_VERSION);

    let asset = release
        .assets
        .iter()
        .find(|a| a.name == ASSET_NAME)
        .ok_or_else(|| format!("V release {} chybí soubor {ASSET_NAME}", release.tag_name))?;

    Ok(ReleaseInfo {
        version: latest,
        is_newer,
        download_url: asset.browser_download_url.clone(),
    })
}

/// Jednoduché porovnání "X.Y.Z" verzí po jednotlivých číslech (bez
/// samostatné semver závislosti - obě verze jsou vždy prosté "a.b.c").
fn is_version_newer(candidate: &str, current: &str) -> bool {
    fn parts(v: &str) -> Vec<u64> {
        v.split('.').map(|p| p.parse().unwrap_or(0)).collect()
    }
    let a = parts(candidate);
    let b = parts(current);
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        if x != y {
            return x > y;
        }
    }
    false
}

/// Stáhne nový `.exe` a spustí odpojený aktualizační skript - viz modulová
/// dokumentace výše. Při úspěchu appku sama ukončí (`std::process::exit`)
/// a tahle funkce se tedy nikdy nevrátí normální cestou; chyby před tím
/// se vrací normálně přes `Result`.
pub fn download_and_apply(download_url: &str) -> Result<(), String> {
    #[cfg(not(windows))]
    {
        let _ = download_url;
        return Err("Automatická aktualizace je zatím jen pro Windows.".to_string());
    }

    #[cfg(windows)]
    {
        let current_exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let dir = current_exe
            .parent()
            .ok_or_else(|| "Nepodařilo se určit adresář appky".to_string())?;
        let new_path = dir.join("tvh-client.exe.new");
        let old_path = dir.join("tvh-client.exe.old");

        let client = http_client()?;
        let bytes = client
            .get(download_url)
            .send()
            .map_err(|e| e.to_string())?
            .bytes()
            .map_err(|e| e.to_string())?;
        std::fs::write(&new_path, &bytes).map_err(|e| e.to_string())?;

        let script_path = dir.join("tvh-client-update.ps1");
        let script = format!(
            r#"$ErrorActionPreference = "SilentlyContinue"
Start-Sleep -Seconds 1
for ($i = 0; $i -lt 30; $i++) {{
    try {{
        if (Test-Path "{old}") {{ Remove-Item -Force "{old}" }}
        Rename-Item -Path "{current}" -NewName (Split-Path "{old}" -Leaf) -Force -ErrorAction Stop
        break
    }} catch {{
        Start-Sleep -Milliseconds 500
    }}
}}
Rename-Item -Path "{new}" -NewName (Split-Path "{current}" -Leaf) -Force
Start-Process -FilePath "{current}"
Start-Sleep -Milliseconds 500
Remove-Item -Force "{old}" -ErrorAction SilentlyContinue
Remove-Item -Force -LiteralPath $MyInvocation.MyCommand.Path -ErrorAction SilentlyContinue
"#,
            old = old_path.display(),
            current = current_exe.display(),
            new = new_path.display(),
        );
        std::fs::write(&script_path, script).map_err(|e| e.to_string())?;

        {
            use std::os::windows::process::CommandExt;
            // CREATE_NO_WINDOW | DETACHED_PROCESS - skript nesmí zdědit
            // konzoli ani životnost tohohle procesu, protože ten hned
            // skončí.
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            const DETACHED_PROCESS: u32 = 0x0000_0008;
            Command::new("powershell")
                .args([
                    "-NoProfile",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-File",
                    &script_path.to_string_lossy(),
                ])
                .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS)
                .spawn()
                .map_err(|e| e.to_string())?;
        }

        std::process::exit(0);
    }
}
