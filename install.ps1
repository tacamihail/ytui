# ytui installer for Windows
#
# Downloads ytui.exe from the latest github release, falling back to a source
# build when no usable release asset exists (a fork, an unreleased commit or an
# offline machine). Also installs what the player needs at runtime (yt-dlp and
# Deno) and drops everything in %USERPROFILE%\.local\bin. Nothing is downloaded
# at runtime except audio itself. Safe to rerun: an existing config is kept.
#
#   powershell -ExecutionPolicy Bypass -File .\install.ps1
#   .\install.ps1 -SkipDiscover     # do not scan the LAN for a resolver
#   .\install.ps1 -FromSource       # ignore the release, build the checkout
#
# Windows PowerShell 5.1 and PowerShell 7+ are both supported.
[CmdletBinding()]
param(
    [switch]$SkipDiscover,
    [switch]$FromSource
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
# PowerShell 5.1 on an older Windows still defaults to TLS 1.0 for downloads.
try {
    [Net.ServicePointManager]::SecurityProtocol =
        [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
} catch {
    # PowerShell 7 ignores this and uses the system default.
}

$Dir = Split-Path -Parent $MyInvocation.MyCommand.Path
$UserHome = $env:USERPROFILE
if (-not $UserHome) {
    $UserHome = [Environment]::GetFolderPath('UserProfile')
}
$BinDir = Join-Path $UserHome '.local\bin'
$DenoDir = Join-Path $UserHome '.deno\bin'
$CfgDir = Join-Path $UserHome '.config\ytui'
$Installed = Join-Path $BinDir 'ytui.exe'

# Tools installed by an earlier run (or by install.sh under WSL) are used even
# when this shell has not picked up the PATH change yet.
foreach ($toolDir in @($BinDir, $DenoDir)) {
    if (Test-Path $toolDir) {
        $env:Path = "$toolDir;$env:Path"
    }
}

function Info([string]$msg) { Write-Host ":: $msg" -ForegroundColor DarkGray }
function Ok([string]$msg) { Write-Host " + $msg" -ForegroundColor Green }
function Warn([string]$msg) { Write-Host " ! $msg" -ForegroundColor Yellow }
function Die([string]$msg) { Write-Host " ! $msg" -ForegroundColor Yellow; exit 1 }

function Have([string]$name) {
    return [bool](Get-Command $name -ErrorAction SilentlyContinue)
}

function Download([string]$url, [string]$dst) {
    Info "downloading $url"
    Invoke-WebRequest -UseBasicParsing -Uri $url -OutFile $dst
}

# Prefer USERPROFILE: the HOME a Git Bash session exports is a POSIX path that
# a native program cannot use. The Rust side picks the same directory.
Info "home: $UserHome"

# --- architecture ------------------------------------------------------------
$Arch = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64' -or $env:PROCESSOR_ARCHITEW6432 -eq 'ARM64') {
    'aarch64'
} else {
    'x86_64'
}
$RustTarget = if ($Arch -eq 'aarch64') { 'aarch64-pc-windows-msvc' } else { 'x86_64-pc-windows-msvc' }
$DenoTarget = if ($Arch -eq 'aarch64') { 'aarch64-pc-windows-msvc' } else { 'x86_64-pc-windows-msvc' }

# --- yt-dlp ------------------------------------------------------------------
# Needs a recent build: YouTube breaks old extractors within weeks. The .exe
# asset is a standalone binary and does not need Python.
function YtDlpIsNewEnough {
    if (-not (Have 'yt-dlp')) { return $false }
    try {
        $v = (& yt-dlp --version | Select-Object -First 1)
        $year = [int]($v.Split('.')[0])
        return $year -ge 2025
    } catch {
        return $false
    }
}

if (YtDlpIsNewEnough) {
    Ok "yt-dlp $(& yt-dlp --version | Select-Object -First 1)"
} else {
    Info "installing a fresh yt-dlp into $BinDir"
    New-Item -ItemType Directory -Force -Path $BinDir | Out-Null
    $dst = Join-Path $BinDir 'yt-dlp.exe'
    Download 'https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp.exe' $dst
    $env:Path = "$BinDir;$env:Path"
    if (YtDlpIsNewEnough) {
        Ok "yt-dlp $(& yt-dlp --version | Select-Object -First 1)"
    } else {
        Warn "yt-dlp was downloaded but does not run; get it from https://github.com/yt-dlp/yt-dlp"
    }
}

# --- deno --------------------------------------------------------------------
# yt-dlp solves YouTube's JS challenges with a JS runtime; deno is the only one
# enabled by default. Without it most videos fail with "Unable to extract
# uploader id".
if (Have 'deno') {
    Ok "deno $((& deno --version | Select-Object -First 1))"
} else {
    Info 'installing Deno (yt-dlp needs it to resolve YouTube streams)'
    New-Item -ItemType Directory -Force -Path $DenoDir | Out-Null
    $zip = Join-Path $env:TEMP 'deno.zip'
    $tmp = Join-Path $env:TEMP "ytui-deno-$PID"
    try {
        Download "https://github.com/denoland/deno/releases/latest/download/deno-$DenoTarget.zip" $zip
        Expand-Archive -Path $zip -DestinationPath $tmp -Force
        Copy-Item (Join-Path $tmp 'deno.exe') (Join-Path $DenoDir 'deno.exe') -Force
        $env:Path = "$DenoDir;$env:Path"
        Ok "deno $((& deno --version | Select-Object -First 1))"
    } catch {
        Warn 'could not install Deno. Get it from https://deno.com or YouTube playback will fail'
    } finally {
        Remove-Item $zip -ErrorAction SilentlyContinue
        Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
    }
}

# --- prebuilt release --------------------------------------------------------
# The release assets are built by .github/workflows/release.yml, so a Windows
# machine without Visual Studio Build Tools never needs a linker. The asset is
# verified with the SHA256SUMS published next to it.
$ReleaseBase = 'https://github.com/tacamihail/ytui/releases/latest/download'
$Asset = if ($Arch -eq 'aarch64') {
    'ytui-aarch64-pc-windows-msvc.exe'
} else {
    'ytui-x86_64-pc-windows-msvc.exe'
}

function Get-ReleaseBinary {
    $tmp = Join-Path $env:TEMP "ytui-$PID.exe"
    $sums = Join-Path $env:TEMP "ytui-sums-$PID.txt"
    try {
        Download "$ReleaseBase/$Asset" $tmp
    } catch {
        Warn "no prebuilt release downloaded ($($_.Exception.Message.Trim()))"
        Remove-Item $tmp -Force -ErrorAction SilentlyContinue
        return $null
    }
    try {
        Invoke-WebRequest -UseBasicParsing -Uri "$ReleaseBase/SHA256SUMS" -OutFile $sums
        $line = Select-String -Path $sums -Pattern "\s$([regex]::Escape($Asset))$" | Select-Object -First 1
        if ($line) {
            $want = ($line.Line -split '\s+')[0].ToLowerInvariant()
            $got = (Get-FileHash -Path $tmp -Algorithm SHA256).Hash.ToLowerInvariant()
            if ($want -ne $got) {
                Warn 'the downloaded ytui.exe did not match SHA256SUMS, discarding it'
                Remove-Item $tmp -Force -ErrorAction SilentlyContinue
                return $null
            }
            Ok "checksum $($want.Substring(0, 12))… verified"
        } else {
            Warn "SHA256SUMS has no entry for $Asset"
        }
    } catch {
        Warn 'could not verify the download (SHA256SUMS unavailable)'
    } finally {
        Remove-Item $sums -Force -ErrorAction SilentlyContinue
    }
    # A binary that cannot print its version would not run here either.
    try {
        $v = (& $tmp --version 2>$null)
        if ($LASTEXITCODE -ne 0 -or -not $v) {
            Warn 'the downloaded ytui.exe does not run on this machine, discarding it'
            Remove-Item $tmp -Force -ErrorAction SilentlyContinue
            return $null
        }
        Ok "downloaded $v"
    } catch {
        Warn 'the downloaded ytui.exe does not run on this machine, discarding it'
        Remove-Item $tmp -Force -ErrorAction SilentlyContinue
        return $null
    }
    return $tmp
}

$prebuilt = $null
if (-not $FromSource) {
    $prebuilt = Get-ReleaseBinary
}
if ($prebuilt) {
    New-Item -ItemType Directory -Force -Path $BinDir | Out-Null
    try {
        Copy-Item $prebuilt $Installed -Force
    } catch {
        Die "cannot replace $Installed; close any running ytui and rerun the installer"
    } finally {
        Remove-Item $prebuilt -Force -ErrorAction SilentlyContinue
    }
    Ok "installed -> $Installed"
}

# --- source build ------------------------------------------------------------
# Only needed when the release could not be used. The MSVC toolchain needs
# link.exe, which Rust finds through Visual Studio rather than PATH. A missing
# VC toolset is the most common first-build failure, so point at the fix before
# spending minutes on a build that cannot link.
if (-not $prebuilt) {
    $MinMajor = 1
    $MinMinor = 83

    function RustIsNewEnough {
        if (-not (Have 'rustc')) { return $false }
        try {
            $line = (& rustc --version) -split '\s+'
            $parts = $line[1] -split '\.'
            return ([int]$parts[0] -gt $MinMajor) -or
                   (([int]$parts[0] -eq $MinMajor) -and ([int]$parts[1] -ge $MinMinor))
        } catch {
            return $false
        }
    }

    # A rustup toolchain may exist but not be first in PATH.
    $CargoBin = Join-Path $UserHome '.cargo\bin'
    if (Test-Path (Join-Path $CargoBin 'cargo.exe')) {
        $env:Path = "$CargoBin;$env:Path"
    }

    if (-not (RustIsNewEnough)) {
        if (Have 'rustup') {
            Info "updating the Rust toolchain (needs $MinMajor.$MinMinor+)"
            & rustup toolchain install stable --profile minimal
            & rustup default stable | Out-Null
        } else {
            Info "installing Rust via rustup (needs $MinMajor.$MinMinor+)"
            $rustupInit = Join-Path $env:TEMP 'rustup-init.exe'
            Download "https://static.rust-lang.org/rustup/dist/$RustTarget/rustup-init.exe" $rustupInit
            & $rustupInit -y --profile minimal
            Remove-Item $rustupInit -ErrorAction SilentlyContinue
        }
        $env:Path = "$CargoBin;$env:Path"
    }

    if (-not (RustIsNewEnough)) {
        Die "Rust $MinMajor.$MinMinor+ is required; run 'rustup default stable' or see https://rustup.rs"
    }
    Ok "rustc $(((& rustc --version) -split '\s+')[1])"

    $MsvcHint = $null
    $RustHost = ''
    try {
        $RustHost = ((& rustc -vV | Where-Object { $_ -like 'host:*' }) -join '').Trim()
    } catch {
        # The version check above already failed if rustc is unusable.
    }
    if ($RustHost -notmatch 'gnu$') {
        $vswhere = $null
        if (${env:ProgramFiles(x86)}) {
            $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
        }
        $vcTools = $false
        if ($vswhere -and (Test-Path $vswhere)) {
            try {
                $vcPath = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
                $vcTools = -not [string]::IsNullOrWhiteSpace(($vcPath -join ''))
            } catch {
                $vcTools = $false
            }
        }
        if (-not $vcTools) {
            $MsvcHint = 'install "Visual Studio Build Tools" with the "Desktop development with C++" workload: https://visualstudio.microsoft.com/visual-cpp-build-tools/'
            Warn 'no Visual C++ build tools found'
            Warn $MsvcHint
        }
    }

    Info 'building ytui (the first build downloads crates, 1-3 min)'
    Push-Location $Dir
    try {
        & cargo build --release
        if ($LASTEXITCODE -ne 0) {
            if ($MsvcHint) {
                Die "build failed. $MsvcHint"
            }
            Die 'build failed; close any running ytui and try again'
        }
    } finally {
        Pop-Location
    }

    $built = Join-Path $Dir 'target\release\ytui.exe'
    if (-not (Test-Path $built)) {
        Die "$built was not produced"
    }

    New-Item -ItemType Directory -Force -Path $BinDir | Out-Null
    try {
        Copy-Item $built $Installed -Force
    } catch {
        Die "cannot replace $Installed; close any running ytui and rerun the installer"
    }
    Ok "installed -> $Installed"
}

# --- config ------------------------------------------------------------------
New-Item -ItemType Directory -Force -Path $CfgDir | Out-Null
if ($prebuilt) {
    # A release install updates by downloading the next release, so a checkout
    # from an earlier source install must not steer `ytui update` back to it.
    Remove-Item (Join-Path $CfgDir 'source') -Force -ErrorAction SilentlyContinue
} else {
    # Remember the checkout so `ytui update` can pull and rebuild it later.
    [System.IO.File]::WriteAllText((Join-Path $CfgDir 'source'), $Dir)
}
$cfg = Join-Path $CfgDir 'config.json'
if (-not (Test-Path $cfg)) {
    Copy-Item (Join-Path $Dir 'config.example.json') $cfg
    Ok "seeded config -> $cfg"
} else {
    Ok 'config already present'
}

# --- resolver discovery ------------------------------------------------------
# Optional: a resolver on the LAN/tailnet removes the yt-dlp/deno spike from
# this machine. Keep it short (10 s) and never overwrite a resolver the user
# already configured, including a public one with a token.
if (-not $SkipDiscover -and $env:YTUI_SKIP_DISCOVER -ne '1') {
    $configured = $false
    try {
        $json = Get-Content -Raw -Path $cfg | ConvertFrom-Json
        $configured = -not [string]::IsNullOrWhiteSpace($json.resolver_url)
    } catch {
        # A hand-edited or broken file is left for the player to report.
    }
    if (-not $configured) {
        Info 'looking for a resolver on the local network (optional, up to 10s)'
        & $Installed discover --save --timeout 10 --quiet
    }
}

# --- PATH --------------------------------------------------------------------
function Add-ToUserPath([string]$dir) {
    $old = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (-not $old) { $old = '' }
    $entries = @($old.Split(';') | Where-Object { $_ -ne '' })
    if ($entries -contains $dir) { return }
    $new = (@($entries + $dir) -join ';')
    [Environment]::SetEnvironmentVariable('Path', $new, 'User')
    Ok "added $dir to the user PATH (open a new terminal)"
}

Add-ToUserPath $BinDir
Add-ToUserPath $DenoDir

Write-Host ''
Write-Host '  ytui installed.' -ForegroundColor Cyan
Write-Host '  ytui doctor                 check yt-dlp + audio device'
Write-Host '  ytui auth <cookies.txt>     connect Liked Music / playlists / subscriptions'
Write-Host '  ytui                        open the player ( / search - ? help - q quit )'
Write-Host ''
Write-Host '  Windows Terminal renders the player much better than legacy conhost.' -ForegroundColor DarkGray
