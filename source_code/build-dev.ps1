#!/usr/bin/env pwsh

# Change to the script's directory
$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
Set-Location $scriptDir

# Kill rusplorer-dev.exe if it's running (leave production instances alone)
$process = Get-Process rusplorer-dev -ErrorAction SilentlyContinue
if ($process) {
    Stop-Process $process -Force
    Write-Host "Closed running Rusplorer-dev instance"
}

# Build dev (fast compilation, slower runtime)
Write-Host "Building (dev - fast iteration)..."
cargo build

if ($LASTEXITCODE -eq 0) {
    Write-Host "`nBuild successful!"

    # Rename to rusplorer-dev.exe in place
    $debugDir = ".\target\debug"
    Copy-Item "$debugDir\rusplorer.exe" -Destination "$debugDir\rusplorer-dev.exe" -Force

    Write-Host "`nLaunching Rusplorer-dev...`n"
    Start-Process "$debugDir\rusplorer-dev.exe"
} else {
    Write-Host "Build failed!"
    Read-Host "Press Enter to exit"
}
