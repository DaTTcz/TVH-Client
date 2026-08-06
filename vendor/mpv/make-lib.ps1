# Vygeneruje MSVC import knihovnu (mpv.lib) z libmpv-2.dll.
#
# Sourceforge "libmpv" balicek (mpv-dev-x86_64-*.7z) je postaveny MinGW
# toolchainem a obsahuje jen "libmpv.dll.a" (GNU formát), který MSVC
# linker (link.exe) neumí přečíst. Tenhle skript z DLL vytáhne seznam
# exportovaných funkcí přes `dumpbin` (součást Visual Studia) a z nich
# postaví .def soubor, ze kterého `lib.exe` udělá skutečnou MSVC .lib.
#
# Spouštět z "x64 Native Tools Command Prompt for VS" (tam jsou dumpbin
# i lib.exe na PATH):
#
#   cd vendor\mpv
#   powershell -ExecutionPolicy Bypass -File make-lib.ps1
#
# Očekává, že libmpv-2.dll je ve stejné složce jako tenhle skript
# (zkopíruj ho sem z rozbaleného mpv-dev-x86_64-*.7z).

param(
    [string]$Dll = "libmpv-2.dll",
    [string]$Def = "mpv.def",
    [string]$Lib = "mpv.lib"
)

if (-not (Test-Path $Dll)) {
    Write-Error "Nenalezen '$Dll' v aktuálním adresáři. Zkopíruj sem libmpv-2.dll z rozbaleného mpv-dev-x86_64-*.7z a spusť skript znovu."
    exit 1
}

$dumpbin = Get-Command dumpbin -ErrorAction SilentlyContinue
if (-not $dumpbin) {
    Write-Error "dumpbin.exe nenalezen na PATH. Spouštěj tenhle skript z 'x64 Native Tools Command Prompt for VS' (Start menu -> Visual Studio -> ...)."
    exit 1
}

Write-Host "Čtu exportované symboly z $Dll..."
$lines = & dumpbin /exports $Dll

"EXPORTS" | Out-File -FilePath $Def -Encoding ascii

$count = 0
foreach ($line in $lines) {
    if ($line -match '^\s*\d+\s+[0-9A-Fa-f]+\s+[0-9A-Fa-f]+\s+([A-Za-z0-9_@]+)\s*$') {
        $Matches[1] | Out-File -FilePath $Def -Append -Encoding ascii
        $count++
    }
}

if ($count -eq 0) {
    Write-Error "V $Dll se nenašly žádné exportované funkce - něco je asi špatně (32bit vs 64bit DLL? Spustil jsi to z x64 promptu?)."
    exit 1
}

Write-Host "Zapsáno $count exportů do $Def."
Write-Host "Vytvářím $Lib..."

& lib "/def:$Def" "/name:$Dll" "/out:$Lib" "/machine:x64"

if ($LASTEXITCODE -eq 0 -and (Test-Path $Lib)) {
    Write-Host "Hotovo: $Lib je připravený. Nezapomeň zkopírovat $Dll i do target\debug\ (a target\release\), aby appka nespadla na chybějící DLL při spuštění."
} else {
    Write-Error "lib.exe selhal (exit code $LASTEXITCODE). Zkontroluj výstup výše."
}
