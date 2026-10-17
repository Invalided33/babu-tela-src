#!/usr/bin/env bash
# Prepares a fresh Ubuntu 24.04 (or Debian 12) machine to run the Babu Tela
# server: Caddy (HTTPS + reverse proxy), coturn (TURN, so calls work between
# any two networks), a locked-down systemd service, daily database backups and
# a firewall. The server itself is one Go binary that deploy.ps1 sends.
# Usually run by deploy.ps1 -Setup; by hand:
#   sudo bash setup.sh chat.seudominio.com.br voce@email.com
# Safe to run again: secrets and data are kept.
set -euo pipefail

DOMAIN="${1:?Uso: sudo bash setup.sh chat.seudominio.com.br voce@email.com}"
EMAIL="${2:?Informe um e-mail para o certificado HTTPS (Let's Encrypt)}"
APP_DIR=/opt/babutela
DATA_DIR=/var/lib/babutela
ENV_FILE=/etc/babutela.env
TURN_MIN=49160
TURN_MAX=49200

[ "$(id -u)" -eq 0 ] || { echo "Rode como root (sudo)."; exit 1; }
export DEBIAN_FRONTEND=noninteractive

echo "==> Pacotes do sistema"
apt-get update -q
apt-get install -y -q curl ca-certificates gnupg ufw sqlite3 coturn openssl \
  debian-keyring debian-archive-keyring apt-transport-https

echo "==> Caddy"
if ! command -v caddy >/dev/null; then
  curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/gpg.key' | gpg --dearmor --yes -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
  curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt' > /etc/apt/sources.list.d/caddy-stable.list
  apt-get update -q
  apt-get install -y -q caddy
fi

echo "==> Usuário e pastas"
id babutela >/dev/null 2>&1 || useradd --system --home-dir "$DATA_DIR" --shell /usr/sbin/nologin babutela
mkdir -p "$APP_DIR" "$DATA_DIR"/uploads "$DATA_DIR"/updates "$DATA_DIR"/backups
chown -R babutela:babutela "$DATA_DIR"
chmod 750 "$DATA_DIR"

echo "==> Configuração (segredos ficam em $ENV_FILE)"
if [ ! -f "$ENV_FILE" ]; then
  cat > "$ENV_FILE" <<EOF
PROSA_DATA=$DATA_DIR
PORT=3210
HOST=127.0.0.1
TRUST_PROXY=1
TURN_URLS=turn:$DOMAIN:3478?transport=udp,turn:$DOMAIN:3478?transport=tcp
TURN_SECRET=$(openssl rand -hex 32)
TURN_RELAY_ONLY=1
EOF
  chmod 600 "$ENV_FILE"
fi
# shellcheck disable=SC1090
. "$ENV_FILE"
PUBLIC_IP="$(curl -fsS --max-time 5 https://api.ipify.org || hostname -I | awk '{print $1}')"

echo "==> TURN (coturn)"
cat > /etc/turnserver.conf <<EOF
listening-port=3478
fingerprint
use-auth-secret
static-auth-secret=$TURN_SECRET
realm=$DOMAIN
external-ip=$PUBLIC_IP
min-port=$TURN_MIN
max-port=$TURN_MAX
no-cli
no-multicast-peers
no-tlsv1
no-tlsv1_1
# Never relay into private networks (including this machine).
denied-peer-ip=0.0.0.0-0.255.255.255
denied-peer-ip=10.0.0.0-10.255.255.255
denied-peer-ip=100.64.0.0-100.127.255.255
denied-peer-ip=127.0.0.0-127.255.255.255
denied-peer-ip=169.254.0.0-169.254.255.255
denied-peer-ip=172.16.0.0-172.31.255.255
denied-peer-ip=192.168.0.0-192.168.255.255
EOF
if [ -f /etc/default/coturn ]; then
  sed -i 's/^#\?TURNSERVER_ENABLED=.*/TURNSERVER_ENABLED=1/' /etc/default/coturn
fi
systemctl enable coturn >/dev/null
systemctl restart coturn

echo "==> HTTPS (Caddy)"
cat > /etc/caddy/Caddyfile <<EOF
{
	email $EMAIL
}

$DOMAIN {
	encode gzip
	request_body {
		max_size 30MB
	}
	header {
		Strict-Transport-Security "max-age=31536000"
		X-Content-Type-Options nosniff
		Referrer-Policy no-referrer
		-Server
	}
	reverse_proxy 127.0.0.1:3210
}
EOF
systemctl enable caddy >/dev/null
systemctl reload caddy 2>/dev/null || systemctl restart caddy

echo "==> Serviço"
cat > /etc/systemd/system/babutela.service <<EOF
[Unit]
Description=Babu Tela
After=network-online.target
Wants=network-online.target

[Service]
User=babutela
Group=babutela
EnvironmentFile=$ENV_FILE
WorkingDirectory=$APP_DIR
ExecStart=$APP_DIR/babutela-server
Restart=always
RestartSec=3
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
PrivateDevices=true
ReadWritePaths=$DATA_DIR

[Install]
WantedBy=multi-user.target
EOF
systemctl daemon-reload
systemctl enable babutela >/dev/null

echo "==> Backup diário do banco (04:00, guarda 14 dias)"
cat > /etc/cron.d/babutela-backup <<EOF
0 4 * * * babutela sqlite3 $DATA_DIR/prosa.db ".backup '$DATA_DIR/backups/prosa-\$(date +\%F).db'" && find $DATA_DIR/backups -name 'prosa-*.db' -mtime +14 -delete
EOF

echo "==> Firewall"
ufw allow OpenSSH >/dev/null
ufw allow 80/tcp >/dev/null
ufw allow 443/tcp >/dev/null
ufw allow 443/udp >/dev/null
ufw allow 3478 >/dev/null
ufw allow "$TURN_MIN:$TURN_MAX/udp" >/dev/null
ufw --force enable >/dev/null

echo
echo "Pronto. Servidor preparado para https://$DOMAIN"
echo "IP público detectado: $PUBLIC_IP (o endereço $DOMAIN precisa apontar para ele)."
if [ -x "$APP_DIR/babutela-server" ]; then
  systemctl restart babutela
  echo "Serviço reiniciado."
fi
