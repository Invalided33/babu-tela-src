<#
  Sends the Babu Tela server to your VPS and (re)starts it.

  Exemplos, rodando na pasta do projeto:
    npm run deploy -- -Server 203.0.113.10 -Setup -Email voce@email.com   # primeira vez: prepara a máquina e envia o código
    npm run deploy -- -Server 203.0.113.10                                 # atualizar o código do servidor
    npm run deploy -- -Server 203.0.113.10 -WithData                       # levar contas, servidores e arquivos deste PC
    npm run deploy -- -Server 203.0.113.10 -Updates                        # publicar o último instalador para atualização automática

  O domínio vem de "babuTela.publicServerUrl" no package.json. O acesso é por SSH
  (o Windows 10/11 já traz o ssh e o scp); a senha ou chave do servidor é pedida
  pelo próprio ssh.
#>
param(
  [Parameter(Mandatory)][string]$Server,
  [string]$User = 'root',
  [switch]$Setup,
  [string]$Email,
  [switch]$WithData,
  [switch]$Updates
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$pkg = Get-Content (Join-Path $root 'package.json') -Raw -Encoding UTF8 | ConvertFrom-Json
$url = "$($pkg.babuTela.publicServerUrl)".TrimEnd('/')
if ($url -notmatch '^https://([a-z0-9.-]+\.[a-z]{2,})$') {
  throw 'Defina "babuTela.publicServerUrl" no package.json, por exemplo "https://chat.seudominio.com.br".'
}
$domain = $Matches[1]
$target = "$User@$Server"
# Windows' own tar: another tar on PATH (Git's, say) misreads C:\ paths.
$tar = Join-Path $env:SystemRoot 'System32\tar.exe'

function Remote([string]$script) {
  # One ssh call per step. PowerShell pipes text with Windows line endings, so
  # the carriage returns are stripped before bash reads the script.
  $script | ssh $target "tr -d '\r' | bash -se"
  if ($LASTEXITCODE -ne 0) { throw "Falhou no servidor (código $LASTEXITCODE)." }
}
function Upload([string]$local, [string]$remote) {
  scp -q $local "${target}:$remote"
  if ($LASTEXITCODE -ne 0) { throw "Falha ao enviar $local" }
}

$stage = Join-Path $env:TEMP 'babutela-deploy'
if (Test-Path $stage) { Remove-Item -LiteralPath $stage -Recurse -Force }
New-Item -ItemType Directory -Path $stage | Out-Null

if ($Setup) {
  if (-not $Email) { throw 'Use -Email com o seu e-mail (vai no certificado HTTPS).' }
  Write-Host "==> Preparando $Server para https://$domain"
  Upload (Join-Path $PSScriptRoot 'setup.sh') '/tmp/babutela-setup.sh'
  Remote "sed -i 's/\r$//' /tmp/babutela-setup.sh && bash /tmp/babutela-setup.sh '$domain' '$Email'"
}

Write-Host '==> Empacotando o servidor'
New-Item -ItemType Directory -Path (Join-Path $stage 'server'), (Join-Path $stage 'app') | Out-Null
Copy-Item (Join-Path $root 'server\index.js'), (Join-Path $root 'server\db.js') (Join-Path $stage 'server')
# The server also serves the web version of the app.
Copy-Item (Join-Path $root 'app\renderer') (Join-Path $stage 'app\renderer') -Recurse
$serverPkg = [ordered]@{
  name = $pkg.name; productName = $pkg.productName; version = $pkg.version; private = $true
  dependencies = [ordered]@{ express = $pkg.dependencies.express; multer = $pkg.dependencies.multer; ws = $pkg.dependencies.ws }
}
[IO.File]::WriteAllText((Join-Path $stage 'package.json'), ($serverPkg | ConvertTo-Json -Depth 4))
$bundle = Join-Path $env:TEMP 'babutela-server.tgz'
& $tar -czf $bundle -C $stage .
if ($LASTEXITCODE -ne 0) { throw 'Falha ao compactar.' }

Write-Host '==> Enviando e instalando'
Upload $bundle '/tmp/babutela-server.tgz'
Remote @'
rm -rf /opt/babutela.new && mkdir -p /opt/babutela.new
tar -xzf /tmp/babutela-server.tgz -C /opt/babutela.new
cd /opt/babutela.new && npm install --omit=dev --no-audit --no-fund --loglevel=error
rm -rf /opt/babutela.old
if [ -d /opt/babutela ]; then mv /opt/babutela /opt/babutela.old; fi
mv /opt/babutela.new /opt/babutela
rm -f /tmp/babutela-server.tgz
'@

if ($WithData) {
  if (Get-Process -Name 'Babu Tela', 'Prosa' -ErrorAction SilentlyContinue) {
    throw 'Feche o Babu Tela (bandeja > Sair) antes de copiar os dados.'
  }
  $data = Join-Path $env:APPDATA 'Prosa\server-data'
  if (-not (Test-Path (Join-Path $data 'prosa.db'))) { throw "Não encontrei dados em $data" }
  Write-Host "==> Copiando dados de $data (o banco do servidor será substituído)"
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
}

if ($Updates) {
  $latest = Join-Path $root 'dist\latest.yml'
  if (-not (Test-Path $latest)) { throw 'Gere o instalador antes com: npm run dist:public' }
  $version = ((Get-Content $latest | Select-String '^version:').ToString() -split ':\s*')[1].Trim()
  $exe = Join-Path $root "dist\$($pkg.productName) Setup $version.exe"
  Write-Host "==> Publicando a versão $version para atualização automática"
  Remote 'rm -f /var/lib/babutela/updates/*'
  foreach ($f in @($exe, "$exe.blockmap", $latest)) { Upload $f '/var/lib/babutela/updates/' }
  Remote 'chown -R babutela:babutela /var/lib/babutela/updates'
}

Remote 'systemctl restart babutela && sleep 2 && systemctl is-active --quiet babutela'
try {
  $v = Invoke-RestMethod "$url/api/version" -TimeoutSec 15
  Write-Host "Pronto: $url está no ar ($($v.name) $($v.version))." -ForegroundColor Green
} catch {
  Write-Warning "O serviço subiu, mas $url ainda não respondeu. Confira se o DNS de $domain aponta para $Server e espere alguns minutos pelo certificado HTTPS."
}
Remove-Item -LiteralPath $stage -Recurse -Force
