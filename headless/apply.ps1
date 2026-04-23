# Clone RustDesk, apply our patches, and build the rustdesk-headless binary
# on Windows (PowerShell). Mirror of apply.sh for Linux/macOS.
#
# Usage:
#   .\headless\apply.ps1 [-Target <dir>]
#
# Re-running is safe — patches are idempotent and the script skips steps
# already done.
#
# Prerequisites:
#   - Visual Studio 2022 Build Tools (MSVC + Windows SDK)
#   - Rust via https://rustup.rs/
#   - Git for Windows
#   - cmake, nasm, yasm (choco install cmake nasm yasm)

param(
    [string]$Target = (Join-Path (Split-Path -Parent $PSScriptRoot) "rustdesk")
)

$ErrorActionPreference = "Stop"

$Here      = $PSScriptRoot
$RepoRoot  = Split-Path -Parent $Here
$VcpkgDir  = if ($env:VCPKG_DIR) { $env:VCPKG_DIR } else { Join-Path $RepoRoot "vcpkg" }
$Repo      = if ($env:RUSTDESK_REPO) { $env:RUSTDESK_REPO } else { "https://github.com/rustdesk/rustdesk.git" }
$Ref       = if ($env:RUSTDESK_REF)  { $env:RUSTDESK_REF }  else { "master" }

function Write-Step($msg) { Write-Host "==> $msg" -ForegroundColor Cyan }

Write-Step "Checking host build prerequisites"
$missing = @()
foreach ($cmd in @("git", "cargo", "cmake", "nasm", "yasm")) {
    if (-not (Get-Command $cmd -ErrorAction SilentlyContinue)) {
        $missing += $cmd
    }
}
if ($missing.Count -gt 0) {
    Write-Host "Missing build tools: $($missing -join ', ')"
    Write-Host "Install via Chocolatey (run as Administrator):"
    Write-Host "  choco install -y git rustup.install cmake nasm yasm visualstudio2022buildtools"
    Write-Host "Then in a regular shell:"
    Write-Host "  rustup default stable-msvc"
    exit 1
}

Write-Step "Cloning RustDesk into $Target"
# A CI cache may have restored rustdesk\target\ (creating rustdesk\ as a
# side effect) before this step runs; in that case `git clone` aborts on
# a non-empty destination. Move target\ aside, wipe, clone, restore.
$SavedTarget = $null
if ((Test-Path $Target) -and -not (Test-Path (Join-Path $Target ".git"))) {
    $targetDir = Join-Path $Target "target"
    if (Test-Path $targetDir) {
        $SavedTarget = "$Target.target.saved"
        if (Test-Path $SavedTarget) { Remove-Item -Recurse -Force $SavedTarget }
        Move-Item -Path $targetDir -Destination $SavedTarget -Force
    }
    Remove-Item -Recurse -Force $Target
}
if (-not (Test-Path (Join-Path $Target ".git"))) {
    git clone --depth 1 --branch $Ref $Repo $Target
}
if ($SavedTarget -and (Test-Path $SavedTarget)) {
    Move-Item -Path $SavedTarget -Destination (Join-Path $Target "target") -Force
}
git -C $Target submodule update --init --recursive

Write-Step "Patching src/lib.rs (module visibility)"
$libRs = Join-Path $Target "src/lib.rs"
if (Select-String -Path $libRs -Pattern "^pub mod ui_session_interface;" -Quiet) {
    Write-Host "  already patched"
} else {
    # Use `patch` if available, else apply the visibility change inline.
    $patchFile = Join-Path $Here "lib.rs.patch"
    if (Get-Command patch -ErrorAction SilentlyContinue) {
        Push-Location $Target
        try { cmd /c "patch -p1 < `"$patchFile`"" } finally { Pop-Location }
    } else {
        (Get-Content $libRs) `
            -replace '^mod ui_session_interface;', 'pub mod ui_session_interface;' `
            | Set-Content $libRs
    }
}

Write-Step "Adding [[bin]] rustdesk-headless to Cargo.toml"
$cargoToml = Join-Path $Target "Cargo.toml"
if (Select-String -Path $cargoToml -Pattern 'name = "rustdesk-headless"' -Quiet) {
    Write-Host "  already added"
} else {
    $content = Get-Content $cargoToml -Raw
    $insertion = @"


[[bin]]
name = "rustdesk-headless"
path = "src/bin/headless.rs"
required-features = []
"@
    # Insert after the service.rs [[bin]] block.
    $pattern = '(\[\[bin\]\][^\[]*?path = "src/service\.rs"[^\n]*\n)'
    if ($content -match $pattern) {
        $content = [regex]::Replace($content, $pattern, "`$1" + $insertion, 'Singleline')
        Set-Content -Path $cargoToml -Value $content -NoNewline
    } else {
        Write-Error "Could not locate service.rs [[bin]] block in Cargo.toml"
        exit 1
    }
}

Write-Step "Copying headless.rs into the RustDesk tree"
$binDir = Join-Path $Target "src/bin"
New-Item -ItemType Directory -Force -Path $binDir | Out-Null
Copy-Item -Force (Join-Path $Here "headless.rs") (Join-Path $binDir "headless.rs")

Write-Step "Bootstrapping vcpkg + codecs (one-time, ~30-60 min)"
if (-not (Test-Path (Join-Path $VcpkgDir "vcpkg.exe"))) {
    git clone https://github.com/microsoft/vcpkg.git $VcpkgDir
    & (Join-Path $VcpkgDir "bootstrap-vcpkg.bat") -disableMetrics
}
$env:VCPKG_ROOT = $VcpkgDir
& (Join-Path $VcpkgDir "vcpkg.exe") install --triplet x64-windows-static libvpx libyuv opus aom

Write-Step "cargo build --release --bin rustdesk-headless"
Push-Location $Target
try {
    $env:VCPKG_ROOT = $VcpkgDir
    cargo build --release --bin rustdesk-headless
} finally {
    Pop-Location
}

$bin = Join-Path $Target "target/release/rustdesk-headless.exe"
Write-Step "Done. Binary: $bin"
Get-Item $bin
