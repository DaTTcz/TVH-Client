# TVH Client

Desktopový klient pro TVHeadend v Rustu (GUI: `egui`/`eframe`).

## Stav (kolo 3 — menu, auto-start, auto-update)

Kolo 1 (connect, TVH REST/M3U klient, digest auth, seznam kanálů) a kolo 2
(video přes vestavěný mpv) jsou hotové a **u tebe funkční** (potvrzeno,
včetně opravy pozice videa). Tohle kolo přidává:

- [x] Horní menu **TV / EPG / Nahrávky / Nastavení**, Nastavení má
      podzáložky **Připojení / Kontrola verze / O programu**
      (`src/app.rs`)
- [x] Appka se po spuštění sama pokusí přihlásit z uložených údajů a
      rovnou ukáže TV (seznam kanálů); když se to nepovede (nebo nic není
      uložené), skočí na Nastavení > Připojení
- [x] Kontrola aktualizací + self-update přes GitHub Releases
      (`src/update.rs`) — appka umí zjistit, jestli existuje novější tag,
      stáhnout `.exe` a sama se vyměnit (viz sekce "Aktualizace" níž)
- [x] `.github/workflows/release.yml` — při pushnutí tagu `vX.Y.Z` se na
      GitHubu sama zkompiluje release verze a přiloží se k releasu
- [x] `LICENSE` — PolyForm Noncommercial 1.0.0

Zatím **chybí** (další kolo):

- [ ] EPG (název pořadu, co běží teď/potom) — záložka EPG je zatím jen
      placeholder ("připravujeme")
- [ ] Nahrávání (DVR) — záložka Nahrávky je zatím jen placeholder

## Důležité — nemám tu jak zkompilovat

Pořád píšu bez `cargo`/`rustc` po ruce (sandbox bez Rust toolchainu), takže
stejně jako minule: API jsem ověřoval přes crates.io/docs.rs zdrojáky
(včetně skutečného zdrojového kódu `libmpv2` a `egui_glow`, ne jen
dokumentace), ale **první build u tebe je první reálný test**.

Tohle kolo je výrazně riskantnější než minulé — embedded mpv přes OpenGL
render API je křehká věc (self-referenční struktura pro `RenderContext`,
ruční `unsafe impl Send/Sync`, sdílení GL kontextu s eframe). Očekávej víc
kol oprav než u kola 1. Pošli mi prosím vždy celý výstup `cargo build`.

## Video playback (mpv) — Windows setup

Tohle je nejrizikovější nová část. `libmpv2` (crate, co embeduje mpv) se
za běhu linkuje proti skutečné knihovně mpv — tu je potřeba mít staženou
zvlášť, není součástí `cargo build`.

**Pozor na dva různé "mpv" balíčky:**

- GitHub releases mpv-playeru (`github.com/mpv-player/mpv/releases`) mají
  jen samotný přehrávač (`mpv.exe`) — **to nestačí**, chybí tam knihovna i
  hlavičky.
- Potřebný je balíček z jiného, samostatného projektu, který dělá právě
  Windows buildy knihovny (ne jen přehrávače):
  <https://sourceforge.net/projects/mpv-player-windows/files/libmpv/> —
  stáhni `mpv-dev-x86_64-<datum>-git-<hash>.7z` (bez `-v3` přípony — ta
  vyžaduje novější CPU s AVX2).

Postup:

1. Stáhni a rozbal výše zmíněný `mpv-dev-x86_64-*.7z`. Uvnitř najdeš
   `libmpv-2.dll` a `libmpv.dll.a` (a složku `include/`).
2. `libmpv.dll.a` je bohužel ve formátu pro MinGW/GCC linker, ne pro MSVC
   `link.exe`, který Rust na Windows používá standardně — přímo použitelný
   není. Musí se z DLL vygenerovat MSVC import knihovna (`mpv.lib`).
3. Zkopíruj `libmpv-2.dll` do `vendor/mpv/` v tomhle projektu (je tam už
   připravený pomocný skript `make-lib.ps1`).
4. Otevři **"x64 Native Tools Command Prompt for VS"** (Start menu →
   Visual Studio — potřebuješ ho nainstalované i jinak, MSVC linker to
   vyžaduje pro Rust na Windows obecně), přejdi do `vendor\mpv` a spusť:
   ```
   powershell -ExecutionPolicy Bypass -File make-lib.ps1
   ```
   Skript přes `dumpbin /exports` přečte, co `libmpv-2.dll` exportuje,
   postaví z toho `.def` soubor a `lib.exe` z něj udělá `mpv.lib` —
   přesně tam, kde ho linker hledá (`.cargo/config.toml` už je
   nastavený).
5. `libmpv-2.dll` zůstane potřeba i za běhu — zkopíruj ji vedle výsledného
   `.exe`, tj. po `cargo build` do `target/debug/libmpv-2.dll` (a pro
   release build obdobně do `target/release/`). Bez toho appka při
   spuštění spadne na chybějící DLL.

Zdroje k tomuhle postupu (pro případ, že by se něco časem změnilo):
[mpv Windows compile docs](https://github.com/mpv-player/mpv/blob/master/DOCS/compile-windows.md),
[Using libmpv in Rust](https://connorslade.com/writing/tutorial/using-libmpv-in-rust),
[Generate a DEF file from a DLL](https://duerrenberger.dev/blog/2018/06/02/generate-a-def-file-from-a-dll/).

Pokud `make-lib.ps1` selže nebo balíček vypadá jinak, než tu popisuju
(názvy souborů se čas od času mění), pošli mi chybovou hlášku / obsah
archivu a upravíme to.

## Jak začít (Windows)

1. Nainstaluj Rust: <https://rustup.rs> (pokud ještě nemáš z kola 1)
2. Nastav mpv podle sekce výše (`vendor/mpv/mpv.lib`)
3. V této složce spusť:

   ```powershell
   cargo build
   ```

4. Pošli mi sem výstup (hlavně chybové hlášky), pokud build neprojde napoprvé.
5. Až to půjde zkompilovat, zkopíruj `libmpv-2.dll` do `target/debug/` a spusť:

   ```powershell
   cargo run
   ```

6. Rychlá kontrola logiky bez GUI (digest auth + M3U parser mají unit testy):

   ```powershell
   cargo test
   ```

Poznámka k TLS: `reqwest` defaultně používá `rustls` místo `native-tls`/
OpenSSL, takže by se build neměl zaseknout na chybějících systémových TLS
knihovnách na Windows.

## Aktualizace (GitHub Releases + self-update)

Repozitář: <https://github.com/DaTTcz/TVH-Client>

Jak vydat novou verzi:

1. Zvedni verzi v `Cargo.toml` (`version = "..."`).
2. `git commit`, `git push`.
3. `git tag vX.Y.Z && git push origin vX.Y.Z`.
4. GitHub Actions (`.github/workflows/release.yml`) sama zkompiluje
   Windows release build a přiloží `tvh-client.exe` (+ `libmpv-2.dll`
   pro pohodlí) k automaticky vytvořenému GitHub Release pro ten tag.

Appka sama v **Nastavení > Kontrola verze** umí:

- zjistit přes GitHub API (`/repos/DaTTcz/TVH-Client/releases/latest`),
  jestli existuje novější tag, než je zabudovaná verze (`CARGO_PKG_VERSION`)
- stáhnout `tvh-client.exe` z toho releasu a **sama se vyměnit** — stáhne
  nový `.exe` vedle sebe, spustí odpojený PowerShell skript, který počká,
  přejmenuje starý `.exe` stranou (Windows dovolí přejmenovat běžící
  soubor, i když ho nejde přepsat), nasadí nový a appku restartuje

Tenhle self-update trik jsem nikde nezkoušel kompilovat, takže stejně jako
zbytek — očekávej, že první pokus o update bude potřebovat ladění. Chyby
při stahování/instalaci appka ukáže přímo v záložce Kontrola verze
(úspěch se pozná tak, že se appka sama zavře a znovu spustí).

**Poznámka k CI:** krok stahování mpv v `release.yml` je natvrdo na
konkrétní datovaný soubor ze Sourceforge (stejný, co používáš lokálně) —
časem zastará a bude potřeba tu URL v workflow souboru aktualizovat.

## Licence

[PolyForm Noncommercial 1.0.0](https://polyformproject.org/licenses/noncommercial/1.0.0/)
— volně k nekomerčnímu použití (osobní / hobby / vzdělávací účely atd.),
zdrojový kód je otevřený, ale komerční využití licence nedovoluje. Plné
znění je v `LICENSE`.

## Struktura projektu

```
tvh-client/
├── Cargo.toml
├── LICENSE             - PolyForm Noncommercial 1.0.0
├── README.md
├── .cargo/
│   └── config.toml    - linker search path pro vendor/mpv/mpv.lib
├── .github/workflows/
│   └── release.yml    - build + GitHub Release při push tagu vX.Y.Z
├── vendor/mpv/
│   ├── make-lib.ps1   - vygeneruje mpv.lib z libmpv-2.dll (viz výše)
│   └── libmpv-2.dll   - sem patří DLL (negitovaná, viz .gitignore)
└── src/
    ├── main.rs         - eframe bootstrap (okno, glow renderer, spuštění appky)
    ├── app.rs          - UI: horní menu, TV/EPG/Nahrávky/Nastavení
    ├── update.rs       - kontrola verze + self-update přes GitHub Releases
    ├── player/
    │   └── mpv.rs      - MpvPlayer: mpv embedded přes OpenGL render API
    └── tvh/
        ├── mod.rs      - TVHeadend REST/M3U klient
        ├── digest.rs   - Digest/Basic auth (MD5, SHA-256, SHA-512-256)
        └── m3u.rs      - M3U playlist parser
```

## Testování proti tvému serveru

Appka se hned po spuštění pokusí připojit z uložených údajů. Napoprvé
(nic ještě není uložené) tě sama pošle do **Nastavení > Připojení** —
zadej adresu ve tvaru `192.168.0.10:9981` (schéma `http://` se doplní
automaticky, pokud ho nenapíšeš), jméno/heslo nech prázdné, pokud server
auth nevyžaduje, zaškrtni "Zapamatovat" a klikni Připojit.

Po připojení tě appka přepne na záložku TV — klikni na kanál vlevo, mělo
by se spustit přehrávání v hlavním panelu. Pokud mpv nejde zinicializovat
(chybí DLL, špatný GL kontext apod.), appka to neshodí — jen ti to napíše
jako chybu (v Nastavení > Připojení / O programu) a seznam kanálů pořád
funguje, jen bez videa.

Pokud něco selže, pošli mi sem chybovou hlášku i to, co `cargo run` vypsal
do konzole.
