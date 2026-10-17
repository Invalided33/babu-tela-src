# Servidor próprio do Babu Tela

Este guia coloca o servidor numa máquina na nuvem (VPS), com HTTPS, TURN para as chamadas e backup diário. Depois disso, o app que você distribui se conecta sempre ao seu domínio, sem depender do seu PC ligado.

## O que você precisa contratar

| Item | Sugestão | Custo aproximado |
|---|---|---|
| VPS com **Ubuntu 24.04**, 1 vCPU e 2 GB de RAM | Hetzner, Contabo, DigitalOcean ou Magalu Cloud (servidor no Brasil, menos atraso na voz) | R$ 25 a R$ 60 por mês |
| Domínio | registro.br (`.com.br`) | cerca de R$ 40 por ano |

## Passo a passo

1. **Crie a VPS** com Ubuntu 24.04 e anote o IP público. Se o provedor oferecer, cadastre uma chave SSH em vez de senha.

2. **Aponte o domínio para a VPS.** No painel do domínio, crie um registro **A** com o nome que quiser (por exemplo `chat`) apontando para o IP da VPS. Pode levar alguns minutos para valer.

3. **Diga ao projeto qual é o endereço.** No `package.json`, preencha:
   ```json
   "babuTela": { "publicServerUrl": "https://chat.seudominio.com.br" }
   ```

4. **Prepare a VPS e envie o servidor** (um comando, na pasta do projeto):
   ```bash
   npm run deploy -- -Server IP_DA_VPS -Setup -Email voce@email.com
   ```
   Isso instala Node.js, Caddy (HTTPS automático), coturn (TURN), o serviço, o backup diário e o firewall, e depois envia o código. O ssh vai pedir a senha ou a chave da VPS. No fim, o script avisa se `https://chat.seudominio.com.br` já está respondendo.

5. **Leve suas contas e servidores atuais** (opcional). Feche o Babu Tela e rode:
   ```bash
   npm run deploy -- -Server IP_DA_VPS -WithData
   ```
   Isso copia o banco e os arquivos do servidor embutido deste PC para a VPS. Se a VPS já tinha dados, eles são guardados em `/var/lib/babutela/backups` antes.

6. **Gere o instalador público e publique para atualização automática:**
   ```bash
   npm run dist:public
   ```
   ```bash
   npm run deploy -- -Server IP_DA_VPS -Updates
   ```
   O instalador público conecta direto no seu domínio e não leva o código do servidor. Instale-o no seu PC e distribua esse arquivo para os outros.

## Depois

- **Atualizar o servidor:** `npm run deploy -- -Server IP_DA_VPS`
- **Lançar uma versão nova do app:** aumente `version` no `package.json`, rode `npm run dist:public` e depois `npm run deploy -- -Server IP_DA_VPS -Updates`. Os apps instalados se atualizam ao abrir.
- **Ver os registros do servidor:** `ssh root@IP_DA_VPS journalctl -u babutela -f`
- **Backups:** ficam em `/var/lib/babutela/backups` (um por dia, 14 dias). Para guardar uma cópia fora da VPS: `scp root@IP_DA_VPS:/var/lib/babutela/backups/prosa-AAAA-MM-DD.db .`

## Onde fica cada coisa na VPS

| Caminho | Conteúdo |
|---|---|
| `/opt/babutela` | código do servidor (a versão anterior fica em `/opt/babutela.old`) |
| `/var/lib/babutela` | banco, arquivos enviados, atualizações e backups |
| `/etc/babutela.env` | configuração e segredo do TURN (só o root lê) |
| `/etc/caddy/Caddyfile` | HTTPS e proxy |
| `/etc/turnserver.conf` | TURN |

## Privacidade nas chamadas

Por padrão (`TURN_RELAY_ONLY=1` em `/etc/babutela.env`), toda a voz e o vídeo passam pelo TURN da VPS, e os participantes não veem o IP uns dos outros. Isso usa a banda da VPS: cada pessoa assistindo a uma tela em 1080p consome uns 4,5 Mbps. Para chamadas diretas entre os participantes, troque para `TURN_RELAY_ONLY=0` e rode `systemctl restart babutela`.

## Segurança da VPS

- Prefira entrar por chave SSH. Com a chave funcionando, desative o login por senha em `/etc/ssh/sshd_config` (`PasswordAuthentication no`) e rode `systemctl restart ssh`.
- Mantenha o sistema atualizado: `apt update && apt upgrade` de vez em quando.
- O servidor Node só escuta em `127.0.0.1`. De fora, só passam HTTPS (Caddy), SSH e as portas do TURN.
