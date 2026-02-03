#!/usr/bin/env pwsh

# Kill rusplorer.exe if it's running
$process = Get-Process rusplorer -ErrorAction SilentlyContinue
if ($process) {
    Stop-Process $process -Force
    Write-Host "Closed running Rusplorer instance"
}

# Build release
Write-Host "Building..."
cargo build --release

if ($LASTEXITCODE -eq 0) {
    Write-Host "`nBuild successful!`nLaunching Rusplorer...`n"
    & ".\target\release\rusplorer.exe"
} else {
    Write-Host "Build failed!"
}
