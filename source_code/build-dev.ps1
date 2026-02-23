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

# Build dev (fast compilation, slower runtime)
Write-Host "Building (dev - optimized for build speed)..."
cargo build

if ($LASTEXITCODE -eq 0) {
    Write-Host "`nBuild successful!"
    
    # Copy rusplorer.exe to root
    $debugExe = ".\target\debug\rusplorer.exe"
    if (Test-Path $debugExe) {
        Write-Host "Copying rusplorer.exe to root..."
        Copy-Item $debugExe -Destination "$rootPath/rusplorer.exe" -Force
        Write-Host "Binary ready: $rootPath/rusplorer.exe"
    }
    
    # Remove the target folder
    Write-Host "Cleaning up target folder..."
    Remove-Item "target" -Recurse -Force -ErrorAction SilentlyContinue
    Remove-Item "$rootPath/target" -Recurse -Force -ErrorAction SilentlyContinue
    
    Write-Host "`nLaunching Rusplorer...`n"
    Start-Process "$rootPath/rusplorer.exe"
} else {
    Write-Host "Build failed!"
}
