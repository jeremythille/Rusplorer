#!/usr/bin/env pwsh

# Handle both running from root or from within source_code/
if ((Split-Path -Leaf $PWD) -eq "source_code") {
    # Already in source_code/
    $rootPath = Split-Path -Parent $PWD
} else {
    # Running from root, go into source_code/
    $sourceCodePath = Join-Path $PWD "source_code"
    if (Test-Path $sourceCodePath) {
        Set-Location $sourceCodePath
        $rootPath = Split-Path -Parent $PWD
    } else {
        Write-Host "Error: source_code/ folder not found!"
        Read-Host "Press Enter to exit"
        exit 1
    }
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
