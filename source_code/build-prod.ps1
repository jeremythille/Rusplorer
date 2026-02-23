#!/usr/bin/env pwsh

# Change to the script's directory
$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
Set-Location $scriptDir

# Get root path (parent of current directory)
$rootPath = Split-Path -Parent (Get-Location)

# Kill rusplorer.exe if it's running
$process = Get-Process rusplorer -ErrorAction SilentlyContinue
if ($process) {
    Stop-Process $process -Force
    Write-Host "Closed running Rusplorer instance"
}

# Build production (full optimization, slower build)
Write-Host "Building (prod - optimized for runtime speed)..."
cargo build --release

if ($LASTEXITCODE -eq 0) {
    Write-Host "`nBuild successful!"
    
    # Clean up build artifacts (keep only the exe)
    Write-Host "Cleaning up build artifacts..."
    $releaseDir = "target/release"
    @("deps", "build", "incremental", ".fingerprint", "examples") | ForEach-Object {
        $path = Join-Path $releaseDir $_
        if (Test-Path $path) {
            Remove-Item $path -Recurse -Force -ErrorAction SilentlyContinue
        }
    }
    # Remove .d metadata files and other artifacts (keeping only .exe and config)
    Get-ChildItem $releaseDir -File -Exclude "*.exe", "rusplorer.config.json" | Remove-Item -Force -ErrorAction SilentlyContinue
    # Explicitly remove .pdb and .d files
    Remove-Item (Join-Path $releaseDir "*.pdb") -Force -ErrorAction SilentlyContinue
    Remove-Item (Join-Path $releaseDir "*.d") -Force -ErrorAction SilentlyContinue
    # Remove .cargo-lock if present
    Remove-Item ".cargo-lock" -Force -ErrorAction SilentlyContinue
    Remove-Item ".\target\release\.cargo-lock" -Force -ErrorAction SilentlyContinue
    
    # Copy rusplorer.exe to root
    $exePath = Join-Path $releaseDir "rusplorer.exe"
    if (Test-Path $exePath) {
        Write-Host "Copying rusplorer.exe to root..."
        Copy-Item $exePath -Destination "$rootPath/rusplorer.exe" -Force
        Write-Host "Binary ready: $rootPath/rusplorer.exe"
    }
    
    # Remove the entire target folder since we have the exe
    Write-Host "Cleaning up target folder..."
    Remove-Item "target" -Recurse -Force -ErrorAction SilentlyContinue
    Remove-Item "$rootPath/target" -Recurse -Force -ErrorAction SilentlyContinue
    
    Write-Host "`nLaunching Rusplorer...`n"
    Start-Process "$rootPath/rusplorer.exe"
    Write-Host "Rusplorer launched in background. Close this window when done."
    Read-Host "Press Enter to exit"
} else {
    Write-Host "Build failed!"
    Read-Host "Press Enter to exit"
}
