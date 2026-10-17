# Servidor próprio do Babu Tela

Com o servidor numa máquina na nuvem (VPS), o app que você distribui já vem apontando para ele. Seu amigo só instala, cria a conta, te adiciona e entra nas calls. O seu PC não precisa ficar ligado.

## O que contratar

| Item | Sugestão |
|---|---|
| VPS com **Ubuntu 24.04**, 1 vCPU e 2 GB de RAM, de preferência em **São Paulo** (menos atraso na voz) | Vultr (São Paulo), Hostinger VPS (Brasil), Magalu Cloud, AWS Lightsail (São Paulo) |
| Um endereço (domínio) | Pago: registro.br (`.com.br`, cerca de R$ 40/ano). Grátis: um subdomínio do **DuckDNS** (duckdns.org), como `babutela.duckdns.org` |

O endereço precisa apontar para o IP da VPS: no registro.br, crie um registro **A**; no DuckDNS, cole o IP no campo "current ip" do seu subdomínio.

## Passo a passo (na pasta do projeto)

1. **Preparar a VPS e enviar o servidor:**
   ```bash
   npm run servidor -- -Ip IP_DA_VPS -Dominio seu.endereco.com -Setup -Email voce@email.com
   ```
   Instala o HTTPS automático (Caddy), o TURN para as calls (coturn), o serviço, o backup diário e o firewall, e envia o servidor. O ssh pede a senha da VPS. O IP e o endereço ficam guardados em `deploy/servidor.json`.

2. **Levar sua conta e seus servidores atuais** (opcional). Feche o Babu Tela e rode:
   ```bash
   npm run servidor -- -ComDados
   ```

3. **Gerar o instalador que conecta no seu servidor:**
   ```bash
   npm run release -- -ServidorPublico https://seu.endereco.com
   ```
   O instalador fica em `build/bin/Babu Tela-amd64-installer.exe`. Instale você também e mande o mesmo arquivo para os seus amigos.

4. **Deixar publicado para a atualização automática:**
   ```bash
   npm run servidor -- -Atualizacao
   ```

## Depois

- **Atualizar o servidor:** `npm run servidor`
- **Lançar uma versão nova do app:** aumente a versão em `wails.json` e em `internal/buildinfo/buildinfo.go`, rode `npm run release -- -ServidorPublico https://seu.endereco.com` e depois `npm run servidor -- -Atualizacao`.
- **Ver os registros:** `ssh root@IP_DA_VPS journalctl -u babutela -f`
- **Backups:** `/var/lib/babutela/backups`, um por dia, guardados por 14 dias.

## Onde fica cada coisa na VPS

| Caminho | Conteúdo |
|---|---|
| `/opt/babutela/babutela-server` | o servidor (um único executável) |
| `/var/lib/babutela` | banco, arquivos enviados, atualizações e backups |
| `/etc/babutela.env` | configuração e segredo do TURN (só o root lê) |
| `/etc/caddy/Caddyfile` | HTTPS |
| `/etc/turnserver.conf` | TURN |

## Calls

Toda a voz, vídeo e tela passam pelo TURN da VPS (`TURN_RELAY_ONLY=1`): funciona entre quaisquer redes e ninguém vê o IP de ninguém. Isso usa a banda da VPS; cada pessoa assistindo a uma tela em 1080p consome uns 4,5 Mbps.

## Segurança da VPS

- Prefira entrar por chave SSH e, com ela funcionando, desative o login por senha (`PasswordAuthentication no` em `/etc/ssh/sshd_config`, depois `systemctl restart ssh`).
- Atualize o sistema de vez em quando: `apt update && apt upgrade`.
- O servidor só escuta internamente; de fora passam apenas HTTPS, SSH e as portas do TURN.
