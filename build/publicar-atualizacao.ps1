<#
  Publishes the installers in build\release for automatic updates, on the
  server of this PC: your version in updates\ and your friends' version in
  updates\public\ (each app checks its own folder). Each gets a latest.json
  with version, file and SHA-256, which the app checks before installing.

  npm run publicar
#>
param([string]$Pasta = (Join-Path $env:APPDATA 'Prosa\server-data\updates'))
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$version = (Get-Content (Join-Path $root 'wails.json') -Raw | ConvertFrom-Json).info.productVersion

function Publish([string]$installer, [string]$folder) {
  if (-not (Test-Path $installer)) { throw "Não achei $installer. Rode antes: npm run release" }
  New-Item -ItemType Directory -Force $folder | Out-Null
  # Files left by earlier versions (installers, Electron's latest.yml).
  Get-ChildItem $folder -File | Where-Object { $_.Extension -in '.exe', '.yml', '.blockmap', '.json' } | Remove-Item
  $name = (Split-Path $installer -Leaf) -replace ' ', '-'
  Copy-Item $installer (Join-Path $folder $name)
  $latest = [ordered]@{ version = $version; file = $name; sha256 = (Get-FileHash $installer -Algorithm SHA256).Hash.ToLower() }
  [IO.File]::WriteAllText((Join-Path $folder 'latest.json'), ($latest | ConvertTo-Json))
}

Publish (Join-Path $root 'build\release\Babu Tela Servidor Setup.exe') $Pasta
Publish (Join-Path $root 'build\release\Babu Tela Setup.exe') (Join-Path $Pasta 'public')
Write-Host "Versão $version publicada (sua versão e a dos amigos). Os apps se atualizam ao abrir." -ForegroundColor Green
