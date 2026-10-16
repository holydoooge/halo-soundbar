#!/usr/bin/env pwsh
param([switch]$Check)

$ErrorActionPreference = 'Stop'
Push-Location $PSScriptRoot
try {
    if ($env:OS -ne 'Windows_NT') { throw 'Halo Soundbar requires Windows.' }
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        throw 'Install Rust with the MSVC toolchain before building.'
    }
    # Cargo restores the pinned windows-rs dependency automatically.
    if ($Check) {
        cargo test --locked --all-targets
        if ($LASTEXITCODE -ne 0) { throw 'Tests failed.' }
        cargo clippy --locked --all-targets -- -D warnings
        if ($LASTEXITCODE -ne 0) { throw 'Clippy failed.' }
    }
    cargo build --locked --release --bin halo-soundbar
    if ($LASTEXITCODE -ne 0) { throw 'Build failed. Check the MSVC build tools and Windows SDK.' }
    Write-Host 'Built: target/release/halo-soundbar.exe' -ForegroundColor Green
} finally {
    Pop-Location
}
