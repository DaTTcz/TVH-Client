<p align="center">
  <img src="assets/logo.png" alt="TVH Client" width="480">
</p>

<p align="center">
  Desktopový klient pro <a href="https://tvheadend.org/">TVHeadend</a> — Windows, napsaný v Rustu
  (<a href="https://github.com/emilk/egui">egui</a>/<a href="https://github.com/emilk/egui/tree/master/crates/eframe">eframe</a>).
</p>

<p align="center">
  <a href="https://www.paypal.com/paypalme/DaTTcz">
    <img src="https://img.shields.io/badge/%E2%9D%A4%EF%B8%8F_Podpo%C5%99_projekt-PayPal-ffc439?style=for-the-badge&logo=paypal&logoColor=ffc439&labelColor=003087" alt="Podpořit přes PayPal">
  </a>
  &nbsp;&nbsp;
  <a href="https://ko-fi.com/dattcz">
    <img src="https://img.shields.io/badge/%E2%98%95_Ko--fi-dattcz-ff5e5b?style=for-the-badge&logo=ko-fi&logoColor=white" alt="Podpořit na Ko-fi">
  </a>
</p>

---

## Co to umí

- **Živé TV** — seznam kanálů (číslo, logo, název, právě vysílaný pořad
  s progress barem), přehrávání ve vestavěném video okně (embedded
  [mpv](https://mpv.io/)), ovládání hlasitosti, přepínání kanálů
  klávesnicí, fullscreen "kino" režim bez rušivých panelů.
- **EPG (programový průvodce)** — časová mřížka všech kanálů s posunem
  na "teď", hledání kanálu, tooltip s popisem pořadu. **Pravé tlačítko
  na pořadu** rovnou naplánuje nahrávání — jednorázově, nebo opakovaně
  (podle názvu pořadu).
- **Nahrávky (DVR)** — plánované, hotové i neúspěšné nahrávky a pravidla
  pro opakované nahrávání na jednom místě: přehrání ve vestavěném
  přehrávači (i pro víc-gigabajtové soubory — přehrávání startuje, jakmile
  je stažen dostatečný náběh, zbytek se dotahuje na pozadí), stažení na
  disk s průběhem, pauzou a zrušením, zrušení/smazání/úprava pravidel.
- **Víc serverů** — uložení víc TVHeadend serverů, přepínání mezi nimi za
  běhu, tlačítko Test (ověří přihlášení a nabídne kanálové tagy k
  omezení seznamu kanálů).
- **Loga kanálů** cachovaná na disk (rychlý start, tichá aktualizace na
  pozadí, žádné blikání).
- **Automatická aktualizace** — appka umí sama zkontrolovat i nainstalovat
  novější verzi z GitHub Releases.
- **Klávesové zkratky** — `↑`/`↓` nebo `+`/`-` hlasitost, `PageUp`/
  `PageDown` (nebo `←`/`→` v EPG) předchozí/další kanál, `←`/`→` při
  přehrávání nahrávky přetočí o 10s, `T`/`E`/`R`/`N` přepnutí na
  záložku TV/EPG/Nahrávky/Nastavení, `Esc` opustí fullscreen.

## Stažení a instalace

Nejjednodušší cesta je hotový build ze
[GitHub Releases](https://github.com/DaTTcz/TVH-Client/releases/latest) —
stáhni `TVH-Client.exe` a [libmpv-2.dll](https://github.com/DaTTcz/TVH-Client/releases/download/v0.1.0/libmpv-2.dll) a dej je vedle sebe do stejné
složky. Instalátor není potřeba, appka nic nezapisuje mimo svoji vlastní
složku a `%APPDATA%\tvh-client\` (nastavení, cache log a přihlašovacích
údajů).

MSVC C runtime (`vcruntime140.dll`/`msvcp140.dll`) řešit nemusíš — je
zabudovaný přímo v `.exe` (statické linkování), takže cílový PC nepotřebuje
mít nainstalovaný Visual C++ Redistributable.

Při prvním spuštění tě appka pošle do **Nastavení > Připojení** — zadej
adresu serveru (např. `192.168.0.10:9981`, `http://` se doplní samo),
případně jméno/heslo, a klikni Připojit.

## Licence

[PolyForm Noncommercial 1.0.0](https://polyformproject.org/licenses/noncommercial/1.0.0/)
— volně k nekomerčnímu použití (osobní/hobby/vzdělávací účely atd.),
zdrojový kód je otevřený, komerční využití licence nedovoluje. Plné znění
je v `LICENSE`.