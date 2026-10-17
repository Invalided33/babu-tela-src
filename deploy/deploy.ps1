<#
  Sends the Babu Tela server to your VPS and (re)starts it. Rodando na pasta do projeto:

    npm run servidor -- -Ip 203.0.113.10 -Dominio chat.seudominio.com.br -Setup -Email voce@email.com
        primeira vez: prepara a máquina (HTTPS, chamadas, backup, firewall) e envia o servidor
    npm run servidor
        atualizar o servidor (usa o IP e o domínio guardados em deploy\servidor.json)
    npm run servidor -- -ComDados
        levar contas, servidores e arquivos deste PC (servidor embutido) para a VPS
    npm run servidor -- -Atualizacao
        publicar o último instalador para a atualização automática dos apps

  O acesso é por SSH (o Windows já traz ssh e scp); a senha ou chave da VPS é pedida
  pelo próprio ssh.
#>
param(
  [string]$Ip,
  [string]$Dominio,
  [string]$Usuario = 'root',
  [switch]$Setup,
  [string]$Email,
  [switch]$ComDados,
  [switch]$Atualizacao
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
$env:Path = [Environment]::GetEnvironmentVariable('Path', 'Machine') + ';' + [Environment]::GetEnvironmentVariable('Path', 'User')

# IP and domain are remembered so later runs need no arguments.
$configFile = Join-Path $PSScriptRoot 'servidor.json'
$config = if (Test-Path $configFile) { Get-Content $configFile -Raw | ConvertFrom-Json } else { [pscustomobject]@{ ip = ''; dominio = '' } }
if ($Ip) { $config.ip = $Ip }
if ($Dominio) { $config.dominio = $Dominio.Trim().ToLower() -replace '^https?://', '' -replace '/.*$', '' }
if (-not $config.ip -or -not $config.dominio) { throw 'Informe -Ip e -Dominio na primeira vez, por exemplo: npm run servidor -- -Ip 203.0.113.10 -Dominio chat.seudominio.com.br -Setup -Email voce@email.com' }
if ($config.dominio -notmatch '^[a-z0-9.-]+\.[a-z]{2,}$') { throw "Domínio inválido: $($config.dominio)" }
[IO.File]::WriteAllText($configFile, ($config | ConvertTo-Json))
$target = "$Usuario@$($config.ip)"
$url = "https://$($config.dominio)"

function Remote([string]$script) {
  # PowerShell pipes text with Windows line endings; strip them before bash reads it.
  $script | ssh $target "tr -d '\r' | bash -se"
  if ($LASTEXITCODE -ne 0) { throw "Falhou no servidor (código $LASTEXITCODE)." }
}
function Upload([string]$local, [string]$remote) {
  scp -q $local "${target}:$remote"
  if ($LASTEXITCODE -ne 0) { throw "Falha ao enviar $local" }
}
$tar = Join-Path $env:SystemRoot 'System32\tar.exe'

if ($Setup) {
  if (-not $Email) { throw 'Use -Email com o seu e-mail (vai no certificado HTTPS).' }
  Write-Host "==> Preparando $($config.ip) para $url"
  Upload (Join-Path $PSScriptRoot 'setup.sh') '/tmp/babutela-setup.sh'
  Remote "sed -i 's/\r$//' /tmp/babutela-setup.sh && bash /tmp/babutela-setup.sh '$($config.dominio)' '$Email'"
}

Write-Host '==> Gerando o servidor para Linux'
node build\obfuscate.js | Out-Null
$bin = Join-Path $env:TEMP 'babutela-server-linux'
$env:GOOS = 'linux'; $env:GOARCH = 'amd64'
try { go build -tags obf -trimpath -ldflags '-s -w' -o $bin .\cmd\babutela-server } finally { Remove-Item Env:GOOS, Env:GOARCH }
if ($LASTEXITCODE -ne 0) { throw 'Falha ao gerar o servidor.' }

Write-Host '==> Enviando'
Upload $bin '/tmp/babutela-server'
Remote @'
install -m 755 /tmp/babutela-server /opt/babutela/babutela-server.new
mv -f /opt/babutela/babutela-server.new /opt/babutela/babutela-server
rm -f /tmp/babutela-server
'@
Remove-Item $bin

if ($ComDados) {
  if (Get-Process -Name 'Babu Tela' -ErrorAction SilentlyContinue) { throw 'Feche o Babu Tela (bandeja > Sair) antes de copiar os dados.' }
  $data = Join-Path $env:APPDATA 'Prosa\server-data'
  if (-not (Test-Path (Join-Path $data 'prosa.db'))) { throw "Não encontrei dados em $data" }
  Write-Host "==> Copiando dados de $data (o banco da VPS será substituído; o anterior vai para backups)"
  $dataBundle = Join-Path $env:TEMP 'babutela-data.tgz'
  & $tar -czf $dataBundle -C $data prosa.db $(if (Test-Path "$data\prosa.db-wal") { 'prosa.db-wal' }) uploads
  if ($LASTEXITCODE -ne 0) { throw 'Falha ao compactar os dados.' }
  Upload $dataBundle '/tmp/babutela-data.tgz'
  Remote @'
systemctl stop babutela || true
cd /var/lib/babutela
if [ -f prosa.db ]; then cp prosa.db "backups/antes-da-importacao-$(date +%F-%H%M).db"; fi
rm -f prosa.db prosa.db-wal prosa.db-shm
tar -xzf /tmp/babutela-data.tgz -C /var/lib/babutela
chown -R babutela:babutela /var/lib/babutela
rm -f /tmp/babutela-data.tgz
'@
  Remove-Item $dataBundle
}

if ($Atualizacao) {
  $installer = Join-Path $root 'build\bin\Babu Tela-amd64-installer.exe'
  if (-not (Test-Path $installer)) { throw "Gere o instalador antes com: npm run release -- -ServidorPublico $url" }
  $version = (Get-Content (Join-Path $root 'wails.json') -Raw | ConvertFrom-Json).info.productVersion
  $latest = Join-Path $env:TEMP 'latest.json'
  $info = [ordered]@{ version = $version; file = Split-Path $installer -Leaf; sha256 = (Get-FileHash $installer -Algorithm SHA256).Hash.ToLower() }
  [IO.File]::WriteAllText($latest, ($info | ConvertTo-Json))
  Write-Host "==> Publicando a versão $version para atualização automática"
  Remote 'rm -f /var/lib/babutela/updates/*'
  Upload $installer '/var/lib/babutela/updates/'
  Upload $latest '/var/lib/babutela/updates/'
  Remote 'chown -R babutela:babutela /var/lib/babutela/updates'
  Remove-Item $latest
}

Remote 'systemctl restart babutela && sleep 2 && systemctl is-active --quiet babutela'
try {
  $v = Invoke-RestMethod "$url/api/version" -TimeoutSec 15
  Write-Host "Pronto: $url está no ar ($($v.name) $($v.version))." -ForegroundColor Green
} catch {
  Write-Warning "O serviço subiu, mas $url ainda não respondeu. Confira se $($config.dominio) aponta para $($config.ip) e espere alguns minutos pelo certificado HTTPS."
}
