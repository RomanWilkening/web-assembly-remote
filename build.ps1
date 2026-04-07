# Build script for Windows (PowerShell)
# Prerequisites: Rust toolchain, wasm-pack, FFmpeg with AMF support

param(
    [switch]$Release
)

$ErrorActionPreference = "Stop"
$root = $PSScriptRoot

$buildType = if ($Release) { "--release" } else { "--dev" }
$targetDir = if ($Release) { "release" } else { "debug" }

Write-Host "=== Building WASM client ===" -ForegroundColor Cyan

# Install wasm-pack if not present
if (-not (Get-Command wasm-pack -ErrorAction SilentlyContinue)) {
    Write-Host "Installing wasm-pack..."
    cargo install wasm-pack
}

# Build the WASM client
Push-Location "$root/client"
wasm-pack build --target web $(if ($Release) { "--release" } else { "--dev" })
Pop-Location

# Prepare server static directory
$staticDir = "$root/server/static"
if (Test-Path $staticDir) { Remove-Item -Recurse -Force $staticDir }
New-Item -ItemType Directory -Path $staticDir | Out-Null

# Copy web assets
Copy-Item -Recurse "$root/client/web/*" "$staticDir/"

# Copy WASM build output
New-Item -ItemType Directory -Path "$staticDir/pkg" -Force | Out-Null
Copy-Item "$root/client/pkg/*.js"   "$staticDir/pkg/"
Copy-Item "$root/client/pkg/*.wasm" "$staticDir/pkg/"
Copy-Item "$root/client/pkg/*.d.ts" "$staticDir/pkg/" -ErrorAction SilentlyContinue

Write-Host "=== Building server ===" -ForegroundColor Cyan

Push-Location "$root/server"
cargo build $buildType
Pop-Location

Write-Host ""
Write-Host "=== Build complete ===" -ForegroundColor Green
Write-Host "Binary: server/target/$targetDir/wasm-remote-server.exe"
Write-Host ""
Write-Host "Run with:"
Write-Host "  cd server"
Write-Host "  ./target/$targetDir/wasm-remote-server.exe --encoder h264_amf"
Write-Host ""
Write-Host "Then open http://localhost:9090 in Chrome/Edge."
