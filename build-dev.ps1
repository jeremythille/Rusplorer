#!/usr/bin/env pwsh

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
    Start-Process ".\target\debug\rusplorer.exe"
} else {
    Write-Host "Build failed!"
}
