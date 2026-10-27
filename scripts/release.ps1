# release.ps1 — build + sign + publish a Modpack Manager update to the LAN.
#
# Strict rule: NOTHING is advertised until you answer "y" at the publish
# prompt. Building the app alone never makes it available to friends.
#
# Usage:
#   .\release.ps1                    # build from tauri.conf.json version
#   .\release.ps1 -Bump patch        # bump 0.1.0 -> 0.1.1 everywhere, build
#   .\release.ps1 -SkipBuild         # reuse the newest installer on disk
#   .\release.ps1 -BundleDir <dir>   # publish an already-signed bundle dir
#   .\release.ps1 -AllowSameVersion  # republish an identical version number
#   .\release.ps1 -Notes "..."       # release notes for the update card
#
# Signing key: the FIRST run generates one into %LOCALAPPDATA%\modpack-manager\
# update-sign.key — keep it safe. The public key it prints must be pasted into
# UPDATE_PUBKEY in src-tauri/src/appupdate.rs before the first real release.
# A key stored inside this repository will trigger a loud warning.
#
# Requires: `minisign` or `rsign` on PATH (winget install jedisct1.minisign,
# or cargo install rsign2), plus `cargo tauri` / `npm run tauri` for builds.

param(
    [switch]$SkipBuild,
    [switch]$AllowSameVersion,
    [string]$BundleDir = "",
    [string]$Notes = "",
    [string]$Bump = "",
    [string]$KeyPath = ""
)

$ErrorActionPreference = "Stop"
$Repo = Split-Path -Parent $PSScriptRoot
$TauriConf = Join-Path $Repo "src-tauri\tauri.conf.json"
$CargoToml = Join-Path $Repo "src-tauri\Cargo.toml"
$AppData = Join-Path $env:LOCALAPPDATA "modpack-manager"
$SeedDir = Join-Path $AppData "updates-seed"

function Fail($msg) { Write-Host "x $msg" -ForegroundColor Red; exit 1 }

# ---------------------------------------------------------------- signing tool
$Signer = $null
foreach ($c in "minisign", "rsign") {
    if (Get-Command $c -ErrorAction SilentlyContinue) { $Signer = $c; break }
}
if (-not $Signer) {
    # Fall back to the repo's own relsign example (needs cargo only).
    if (Get-Command cargo -ErrorAction SilentlyContinue) {
        $Signer = "relsign"
    } else {
        Fail "no signer found. Install: winget install jedisct1.minisign  (or)  cargo install rsign2"
    }
}
function Invoke-SignGen($pub, $key) {
    if ($Signer -eq "minisign") { & minisign -G -p $pub -s $key }
    elseif ($Signer -eq "rsign") { & rsign generate -p $pub -s $key }
    else { & cargo run -q --manifest-path (Join-Path $Repo "src-tauri\Cargo.toml") --example relsign -- gen $pub $key }
}
function Invoke-Sign($key, $msg, $sigOut) {
    if ($Signer -eq "minisign") { & minisign -S -x $sigOut -s $key -m $msg }
    elseif ($Signer -eq "rsign") { & rsign sign -x $sigOut -s $key $msg }
    else { & cargo run -q --manifest-path (Join-Path $Repo "src-tauri\Cargo.toml") --example relsign -- sign $key $msg $sigOut }
}

# ---------------------------------------------------------------- key
if (-not $KeyPath) {
    if ($env:MMPK_SIGN_KEY) { $KeyPath = $env:MMPK_SIGN_KEY }
    else { $KeyPath = Join-Path $AppData "update-sign.key" }
}
if (-not (Test-Path $KeyPath)) {
    New-Item -ItemType Directory -Force (Split-Path -Parent $KeyPath) | Out-Null
    Write-Host "Generating a new update signing key at $KeyPath"
    Invoke-SignGen (Join-Path $AppData "update-sign.pub") $KeyPath
    Write-Host ""
    Write-Host "!! Paste this public key into UPDATE_PUBKEY in src-tauri/src/appupdate.rs:"
    Get-Content (Join-Path $AppData "update-sign.pub")
    Write-Host ""
}
if ($KeyPath.StartsWith($Repo, [StringComparison]::OrdinalIgnoreCase)) {
    Write-Host "WARNING: signing key is inside the repository — move it out or it may leak." -ForegroundColor Yellow
    $w = Read-Host "continue anyway? [y/N]"
    if ($w -ne "y" -and $w -ne "Y") { Fail "aborted" }
}

# ---------------------------------------------------------------- version / build
$Version = $null
if ($BundleDir) {
    $manPath = Join-Path $BundleDir "latest.json"
    if (-not (Test-Path $manPath)) { Fail "no latest.json in $BundleDir" }
    $Version = (Get-Content $manPath -Raw | ConvertFrom-Json).version
} else {
    if ($Bump) {
        $conf = Get-Content $TauriConf -Raw | ConvertFrom-Json
        $parts = $conf.version.Split(".")
        switch ($Bump) {
            "major" { $parts[0] = [int]$parts[0] + 1; $parts[1] = 0; $parts[2] = 0 }
            "minor" { $parts[1] = [int]$parts[1] + 1; $parts[2] = 0 }
            "patch" { $parts[2] = [int]$parts[2] + 1 }
            default { Fail "-Bump must be major|minor|patch" }
        }
        $newVer = $parts -join "."
        $utf8NoBom = New-Object System.Text.UTF8Encoding($false)
        foreach ($f in @($TauriConf, $CargoToml, (Join-Path $Repo "package.json"))) {
            # -Encoding UTF8 is REQUIRED on the read: without it PS5 decodes
            # via the ANSI codepage and every bump re-mangles UTF-8 (Cargo.toml's
            # em-dash grew to 141MB of mojibake before GitHub rejected it).
            $text = Get-Content $f -Raw -Encoding UTF8
            $text = $text -replace '(?m)("version"\s*:\s*|^version = )"[^"]+"', "`$1`"$newVer`""
            [System.IO.File]::WriteAllText($f, $text, $utf8NoBom)
        }
        Write-Host "Bumped version -> $newVer"
    }
    $Version = (Get-Content $TauriConf -Raw | ConvertFrom-Json).version

    if (-not $SkipBuild) {
        Write-Host "Building v$Version (npm run tauri build)…"
        Push-Location $Repo
        try { & npm run tauri build } finally { Pop-Location }
        if ($LASTEXITCODE -ne 0) { Fail "build failed" }
    }
}

# ---------------------------------------------------------------- payload + manifest
if ($BundleDir) {
    $Stage = $BundleDir
    if (-not (Test-Path (Join-Path $Stage "latest.json.minisig"))) {
        Fail "$BundleDir has no latest.json.minisig — sign it first"
    }
} else {
    $NsisDir = Join-Path $Repo "src-tauri\target\release\bundle\nsis"
    $Setup = Get-ChildItem $NsisDir -Filter "*_x64-setup.exe" -ErrorAction SilentlyContinue |
        Sort-Object LastWriteTime -Descending | Select-Object -First 1
    if (-not $Setup) { Fail "no NSIS installer found in $NsisDir — build first" }
    if ($Setup.Name -notlike "*_$Version`_*") {
        Write-Host "WARNING: newest installer is $($Setup.Name), expected *_$($Version)_* — using it anyway" -ForegroundColor Yellow
    }

    $Stage = Join-Path $env:TEMP "mmpk-release-$Version"
    if (Test-Path $Stage) { Remove-Item -Recurse -Force $Stage }
    New-Item -ItemType Directory -Force $Stage | Out-Null
    Copy-Item $Setup.FullName (Join-Path $Stage $Setup.Name)

    $manifest = [ordered]@{
        app     = "com.modpackmanager.app"
        version = $Version
        channel = "stable"
        arch    = "x86_64"
        notes   = $Notes
        pubDate = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ssZ")
        file    = $Setup.Name
        sha256  = (Get-FileHash $Setup.FullName -Algorithm SHA256).Hash.ToLower()
        size    = $Setup.Length
    }
    $manJson = $manifest | ConvertTo-Json -Compress
    [System.IO.File]::WriteAllText((Join-Path $Stage "latest.json"), $manJson)

    # Sign the manifest itself — the signature binds app+version+hash, so a
    # forged version field or swapped payload fails verification.
    Invoke-Sign $KeyPath (Join-Path $Stage "latest.json") (Join-Path $Stage "latest.json.minisig")
    if ($LASTEXITCODE -ne 0) { Fail "signing failed" }
    Write-Host "signed manifest -> v$Version ($($Setup.Name), $([math]::Round($Setup.Length/1MB,1)) MB)"
}

# ---------------------------------------------------------------- same-version guard
$seedMan = Join-Path $SeedDir "latest.json"
if (-not $AllowSameVersion -and (Test-Path $seedMan)) {
    $oldVer = (Get-Content $seedMan -Raw | ConvertFrom-Json).version
    if ($oldVer -eq $Version) {
        Fail "v$Version is already published. Bump the version (or pass -AllowSameVersion for an emergency re-ship)."
    }
}

# ---------------------------------------------------------------- explicit publish
$ans = Read-Host "publish v$Version to LAN? [y/N]"
if ($ans -ne "y" -and $ans -ne "Y") { Fail "not published — nothing advertised" }

# Copy-then-activate: stage as *.tmp, rename only when complete, so peers can
# never fetch a half-written bundle.
New-Item -ItemType Directory -Force $SeedDir | Out-Null
foreach ($f in (Get-ChildItem $Stage -File)) {
    $tmp = Join-Path $SeedDir ($f.Name + ".tmp")
    Copy-Item $f.FullName $tmp -Force
    Move-Item $tmp (Join-Path $SeedDir $f.Name) -Force
}
Write-Host "✓ v$Version published — this app now seeds it on the LAN." -ForegroundColor Green
Write-Host "  A running copy picks up the new seed on its next announce tick (~3s)."
