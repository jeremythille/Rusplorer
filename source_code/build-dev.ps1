#!/usr/bin/env pwsh

# Get the directory where this script is located
$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path

# Change to the script directory (which should be source_code/)
Set-Location $scriptDir

# Set rootPath to the parent directory (where the repository root is)
$rootPath = Split-Path -Parent $scriptDir

# Verify we have Cargo.toml
if (-not (Test-Path "Cargo.toml")) {
    Write-Host "Error: Cargo.toml not found in script directory: $scriptDir"
    Read-Host "Press Enter to exit"
    exit 1
}

# Kill rusplorer.exe if it's running
$process = Get-Process rusplorer -ErrorAction SilentlyContinue
if ($process) {
    Stop-Process $process -Force
    Write-Host "Closed running Rusplorer instance"
}

# Build dev (fast compilation, slower runtime)
Write-Host "Building (dev - optimized for build speed)..."
cargo build

if ($LASTEXITCODE -eq 0) {
    Write-Host "`nBuild successful!`nLaunching Rusplorer...`n"
    # Launch from root, using the PATH binary if available, else search in target/debug
    if (Test-Path "$rootPath/rusplorer.exe") {
        Start-Process "$rootPath/rusplorer.exe"
    } else {
        Start-Process ".\target\debug\rusplorer.exe"
    }
} else {
    Write-Host "Build failed!"
}
