<#
  Builds the two versions of Babu Tela, using build\host-config.json
  (dominio, duckdnsToken, email):

    build\release\Babu Tela Servidor Setup.exe   your version: the server, already set up
                                                 with the address and the DuckDNS token
    build\release\Babu Tela Setup.exe            your friends' version: connects only to
                                                 https://<dominio>; carries no token

  npm run release
#>
$ErrorActionPreference = 'Stop'
Set-Location (Split-Path -Parent $PSScriptRoot)
$env:Path = [Environment]::GetEnvironmentVariable('Path', 'Machine') + ';' + [Environment]::GetEnvironmentVariable('Path', 'User') +
  ";$env:USERPROFILE\go\bin;${env:ProgramFiles(x86)}\NSIS"

# The version lives in two places; refuse to build if they disagree.
$version = (Get-Content wails.json -Raw | ConvertFrom-Json).info.productVersion
if (-not (Select-String -Path internal\buildinfo\buildinfo.go -Pattern "Version = `"$([regex]::Escape($version))`"" -Quiet)) {
  throw "A versão em wails.json ($version) não bate com internal\buildinfo\buildinfo.go. Atualize as duas."
}

$configFile = 'build\host-config.json'
if (-not (Test-Path $configFile)) { throw "Crie $configFile com dominio e duckdnsToken." }
$config = Get-Content $configFile -Raw | ConvertFrom-Json
if ($config.dominio -notmatch '^[a-z0-9.-]+\.[a-z]{2,}$') { throw "Domínio inválido em ${configFile}: $($config.dominio)" }
# Port 33443, not 443: home routers usually keep 443 for themselves (see
# publicHTTPSPort in publichost.go).
$url = "https://$($config.dominio):33443"

Write-Host '==> Ofuscando a interface'
node build\obfuscate.js
if ($LASTEXITCODE -ne 0) { throw 'Falha ao ofuscar a interface.' }

$out = 'build\release'
New-Item -ItemType Directory -Force $out | Out-Null
Get-ChildItem $out -File | Remove-Item

function Build([string]$label, [string]$tags, [string]$ldflags, [string]$installerName) {
  Write-Host "==> $label $version"
  wails build -clean -nsis -tags $tags -skipbindings -trimpath -ldflags $ldflags
  if ($LASTEXITCODE -ne 0) { throw "Falha no build: $label" }
  Copy-Item 'build\bin\Babu Tela-amd64-installer.exe' (Join-Path $out $installerName)
}

Build 'Sua versão (servidor)' 'obf' "-s -w -X main.hostDomain=$($config.dominio) -X main.hostDuckToken=$($config.duckdnsToken) -X main.hostEmail=$($config.email)" 'Babu Tela Servidor Setup.exe'
Build 'Versão dos amigos' 'obf,public' "-s -w -X main.publicServerURL=$url" 'Babu Tela Setup.exe'

# The friends' build must never carry the token.
$friendsExe = [IO.File]::ReadAllText((Resolve-Path 'build\bin\Babu Tela.exe'), [Text.Encoding]::GetEncoding(28591))
if ($config.duckdnsToken -and $friendsExe.Contains($config.duckdnsToken)) {
  Remove-Item (Join-Path $out 'Babu Tela Setup.exe')
  throw 'O token apareceu na versão dos amigos; o instalador dela foi apagado.'
}

Write-Host '==> Servidor separado'
go build -tags obf -trimpath -ldflags '-s -w' -o build\bin\babutela-server.exe .\cmd\babutela-server
if ($LASTEXITCODE -ne 0) { throw 'Falha no build do servidor.' }

Write-Host "Pronto. Os amigos se conectam em $url." -ForegroundColor Green
Get-ChildItem $out -File | Select-Object Name, @{ n = 'MB'; e = { [math]::Round($_.Length / 1MB, 1) } } | Format-Table -AutoSize
