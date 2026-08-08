//! Kontrola aktualizací a self-update přes GitHub Releases
//! (`https://github.com/DaTTcz/TVH-Client`).
//!
//! ## Jak self-update funguje
//!
//! Windows nedovolí přepsat/smazat soubor běžícího `.exe`, ale dovolí ho
//! **přejmenovat/přesunout** (souborový handle zůstává platný) - na tomhle
//! stojí self-update na Windows bez instalátoru:
//!
//! 1. Stáhneme nový `.exe` vedle starého jako `TVH-Client.exe.new`.
//! 2. Vytvoříme a spustíme `.bat` skript přes `cmd /C`.
//! 3. Tenhle proces hned skončí (`std::process::exit`), čímž uvolní zámek
//!    na `TVH-Client.exe`.
//! 4. Skript chvíli počká, `move /y`-em přesune nový `.exe` přes starý,
//!    appku znovu spustí a smaže sám sebe.
//!
//! Release, ze kterého se stahuje, musí obsahovat asset přesně
//! pojmenovaný podle [`ASSET_NAME`] - o to se stará
//! `.github/workflows/release.yml`.
//!
//! **Pozn.:** první verze tohohle mechanismu používala skrytý/odpojený
//! (`CREATE_NO_WINDOW | DETACHED_PROCESS`) PowerShell skript - u Davida se
//! projevilo, že appka se sice zavřela, ale nic se dál nestalo (žádné
//! přejmenování, žádný restart, žádná stopa proč). Nejpravděpodobnější
//! vysvětlení: takhle vypadající skrytý proces (bez okna, bez rodiče,
//! přejmenovává běžící `.exe`) je přesně to, co antivir/Defender
//! heuristicky vyhodnocuje jako podezřelé chování a potichu ho zabije.
//! Tahle verze proto dělá to samé, co Davidova jiná appka, u které update
//! spolehlivě funguje: obyčejný `cmd /C batch.bat` bez skrývání okna (na
//! chvíli blikne konzole) a `move /y` místo dvou přejmenování. Jednodušší
//! a míň nápadné pro AV, i když o trochu méně "hezké" vizuálně.
//!
//! Skript si loguje každý krok do `TVH-Client-update.log` vedle appky a
//! **nemaže sám sebe (ani log) při neúspěchu** - takže pokud by se update
//! znovu jen tiše "ztratil", zbyde `TVH-Client.exe.new` +
//! `TVH-Client-update.bat` + `TVH-Client-update.log` vedle appky, ze
//! kterých jde vyčíst, kde přesně to selhalo. [`download_and_apply`] si
//! tyhle tři soubory před dalším pokusem sama uklidí (best-effort), takže
//! i po neúspěchu jde zkusit aktualizaci znovu bez ručního zásahu.

use serde::Deserialize;
use std::process::Command;

pub const REPO_OWNER: &str = "DaTTcz";
pub const REPO_NAME: &str = "TVH-Client";
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Musí odpovídat názvu assetu nahrávaného v `.github/workflows/release.yml`.
pub const ASSET_NAME: &str = "TVH-Client.exe";

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

/// Stáhne nový `.exe` a spustí aktualizační `.bat` skript - viz modulová
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
        let new_path = dir.join("TVH-Client.exe.new");
        let script_path = dir.join("TVH-Client-update.bat");
        let log_path = dir.join("TVH-Client-update.log");

        // Sebe-úklid po případném předchozím neúspěšném pokusu (viz
        // module doc) - kdyby tu ležel starý `.new`/`.bat`/log ze
        // selhání, ať nepletou tenhle běh. Best-effort, chybu ignorujeme -
        // pokud se to nepovede smazat teď, přepíše se to za chvíli stejně.
        let _ = std::fs::remove_file(&new_path);
        let _ = std::fs::remove_file(&script_path);
        let _ = std::fs::remove_file(&log_path);

        let client = http_client()?;
        let bytes = client
            .get(download_url)
            .send()
            .map_err(|e| e.to_string())?
            .bytes()
            .map_err(|e| e.to_string())?;
        std::fs::write(&new_path, &bytes).map_err(|e| e.to_string())?;

        // Obyčejný, viditelný `cmd /C` batch skript (na chvíli bliknutí
        // konzole) - žádné skryté/odpojené okno, viz module doc pro proč.
        // `move /y` přepíše cílový soubor, pokud existuje, a na rozdíl od
        // dvou samostatných přejmenování (starý stranou, nový na jeho
        // místo) je to jeden krok - míň příležitostí něco pokazit.
        let script = format!(
            r#"@echo off
setlocal
set "LOG={log}"
echo [%date% %time%] start >>"%LOG%"
timeout /t 2 /nobreak >nul
move /y "{new}" "{current}" >>"%LOG%" 2>&1
if errorlevel 1 (
    echo [%date% %time%] FATAL: move selhal, errorlevel %errorlevel% - stara appka je nedotcena >>"%LOG%"
    exit /b 1
)
echo [%date% %time%] move OK, spoustim novou appku >>"%LOG%"
start "" "{current}"
echo [%date% %time%] hotovo, uklizim >>"%LOG%"
del "%LOG%" >nul 2>&1
del "%~f0"
"#,
            log = log_path.display(),
            new = new_path.display(),
            current = current_exe.display(),
        );
        std::fs::write(&script_path, script).map_err(|e| e.to_string())?;

        Command::new("cmd")
            .args(["/C", &script_path.to_string_lossy()])
            .spawn()
            .map_err(|e| e.to_string())?;

        std::process::exit(0);
    }
}
