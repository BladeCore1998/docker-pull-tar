[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$workspaceRoot = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$targetTriple = 'x86_64-pc-windows-msvc'
$sourceExe = Join-Path $workspaceRoot 'src-tauri/target/x86_64-pc-windows-msvc/release/docker-pull-tar.exe'
$portableDir = Join-Path $workspaceRoot 'release/portable'
$portableExe = Join-Path $portableDir 'DockerPull.exe'
$ciWasDefined = Test-Path Env:CI
$previousCi = if ($ciWasDefined) { $env:CI } else { $null }

Push-Location $workspaceRoot
try {
    $legacyPattern = 'axios|electron|fastapi|uvicorn|pyinstaller|127\.0\.0\.1:8000|localhost:8000|ws://'
    $scanTargets = @(
        'package.json',
        'src',
        'src-tauri/src',
        'src-tauri/tauri.conf.json',
        'src-tauri/capabilities'
    )

    $rg = Get-Command rg -ErrorAction SilentlyContinue
    if ($null -ne $rg) {
        $legacyMatches = & $rg.Source --line-number --ignore-case --regexp $legacyPattern @scanTargets 2>&1
        $legacyExitCode = $LASTEXITCODE
    }
    else {
        $scanFiles = foreach ($target in $scanTargets) {
            $item = Get-Item -LiteralPath $target
            if ($item.PSIsContainer) {
                Get-ChildItem -LiteralPath $item.FullName -File -Recurse
            }
            else {
                $item
            }
        }
        $legacyMatches = $scanFiles | Select-String -Pattern $legacyPattern -CaseSensitive:$false
        $legacyExitCode = if ($legacyMatches) { 0 } else { 1 }
    }
    if ($legacyExitCode -eq 0) {
        Write-Error "Legacy runtime references were found:`n$($legacyMatches -join [Environment]::NewLine)"
    }
    if ($legacyExitCode -gt 1) {
        throw "Legacy runtime scan failed with exit code $legacyExitCode."
    }

    $env:CI = 'true'
    & pnpm exec tauri build --target $targetTriple
    if ($LASTEXITCODE -ne 0) {
        throw "Tauri build failed with exit code $LASTEXITCODE."
    }

    if (-not (Test-Path -LiteralPath $sourceExe -PathType Leaf)) {
        throw "Expected release executable was not produced: $sourceExe"
    }

    New-Item -ItemType Directory -Path $portableDir -Force | Out-Null
    Copy-Item -LiteralPath $sourceExe -Destination $portableExe -Force

    $artifact = Get-Item -LiteralPath $portableExe
    $hash = Get-FileHash -LiteralPath $portableExe -Algorithm SHA256
    Write-Host "Portable executable: $($artifact.FullName)"
    Write-Host "Size (bytes): $($artifact.Length)"
    Write-Host "SHA-256: $($hash.Hash)"
}
finally {
    if ($ciWasDefined) {
        $env:CI = $previousCi
    }
    else {
        Remove-Item Env:CI -ErrorAction SilentlyContinue
    }
    Pop-Location
}
