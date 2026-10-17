# Babu Tela

Chat de comunidades com texto, voz, vídeo e compartilhamento de tela. O app desktop e o servidor são escritos em **Go**; a interface é HTML, CSS e JavaScript, mostrada numa janela WebView2 pelo [Wails](https://wails.io).

## O que você precisa

- Go 1.22 ou mais novo
- Wails v2: `go install github.com/wailsapp/wails/v2/cmd/wails@v2.16.0`
- Node.js, só para ofuscar a interface no build final e rodar os testes: `npm install`
- NSIS, só para gerar o instalador

## Comandos

| Comando | O que faz |
|---|---|
| `npm run dev` | Abre o app em modo de desenvolvimento, recarregando a interface quando você edita `frontend/` |
| `npm test` | Compila o servidor e roda os testes de ponta a ponta (`tests/smoke.js`) |
| `npm run release` | Gera `build/bin/Babu Tela.exe`, o instalador `Babu Tela-amd64-installer.exe` e o servidor separado `babutela-server.exe` |
| `npm run publicar` | Publica o instalador gerado para a atualização automática dos apps instalados |
| `npm run server` | Roda só o servidor (o mesmo que `Iniciar servidor.bat`) |

Para lançar uma versão nova: aumente a versão em `wails.json` (`info.productVersion`) e em `internal/buildinfo/buildinfo.go`, rode `npm run release` e depois `npm run publicar`. Os apps instalados baixam o instalador, conferem o SHA-256 e oferecem "Reiniciar e instalar".

## Como funciona

O app já traz o servidor: ao abrir, ele liga um servidor na porta 3210 (Configurações → Servidor → "Hospedar servidor neste computador"). Os dados ficam em `%APPDATA%\Prosa` (o nome antigo do app): `server-data` com o banco e os arquivos, `settings.json`, e o login criptografado com a chave do usuário do Windows em `session.bin`. Se já houver um servidor do Babu Tela na porta, o app usa esse.

Na mesma rede, outras pessoas entram colocando o endereço do seu computador em "Endereço do servidor" na tela de login; as configurações do app mostram esse endereço.

### Servidor próprio na nuvem

Para um servidor sempre ligado, onde os amigos só instalam o app e criam a conta, siga [deploy/LEIAME.md](deploy/LEIAME.md): `npm run servidor` prepara a VPS e envia o servidor, e `npm run release -- -ServidorPublico https://seu.endereco` gera a versão pública do app (sem servidor embutido nem Tailscale, já apontando para o seu servidor).

### Amigos em outra cidade, sem VPS

O app traz o Tailscale como biblioteca (`tsnet`, em `tunnel.go`), então ninguém instala nada à parte:

1. Quem hospeda cria uma conta grátis no Tailscale, gera uma chave em login.tailscale.com/admin/settings/keys (marcando **Reusable**, sem **Ephemeral**) e cola em Configurações → Servidor → **Conexão pela internet**.
2. Com o status em "Online", **Gerar convite** cria um código `BT1-…` com a chave e o endereço do host na rede do Tailscale.
3. O amigo instala o app e usa **Entrar com convite de um amigo** na tela de login. O app dele entra na mesma rede e abre portas locais (`127.0.0.1:3211` para o servidor, `:3479` para chamadas) que passam pelo túnel.

As chamadas de quem usa o app desktop passam por um relé TURN embutido no app de quem hospeda (`turn.go`, com `pion/turn`), alcançado pelo túnel. Isso faz voz, vídeo e tela funcionarem entre cidades e através de qualquer roteador. O convite contém a chave: quem tiver o código entra na rede do Tailscale.

## Estrutura

| Caminho | Conteúdo |
|---|---|
| `main.go`, `app.go` | App desktop: janela, configurações, notificações, chamadas vindas da interface |
| `host.go` | Servidor embutido no app |
| `tray.go` | Ícone na bandeja, com o aviso de mensagens novas |
| `update.go` | Atualização automática |
| `winsys_windows.go` | Login criptografado (DPAPI) e "Iniciar com o Windows" |
| `internal/server/` | Servidor: API, tempo real (WebSocket), voz, uploads, limites e permissões, em SQLite |
| `cmd/babutela-server/` | Servidor separado, sem o app |
| `frontend/` | Interface. `native.js` liga a interface ao Go; `dist/` é a cópia ofuscada dos builds finais |
| `build/` | Ícone, instalador (NSIS) e scripts de build |
| `tests/smoke.js` | Testes de ponta a ponta do servidor |
| `legado-electron/` | Versão anterior em Electron e Node, guardada só como referência |

## Segurança

- Limites contra abuso: login (5 senhas erradas bloqueiam a conta por 15 minutos), cadastro, mensagens, uploads, convites, amizades e denúncias.
- Sessões expiram após 30 dias sem uso; trocar a senha ou "Sair de todos os outros dispositivos" encerra as outras na hora.
- Bloquear pessoas, denunciar mensagens (vai para a moderação do servidor) e excluir a conta.
- O build final embute a interface ofuscada; o Go já é compilado para código de máquina.
- Arquivos enviados que não são imagem, vídeo ou áudio só baixam, nunca abrem dentro do app.

## Limites conhecidos

- Voz e vídeo usam WebRTC em malha, direto entre os participantes: bom para grupos pequenos. Atrás de redes restritivas é preciso um servidor TURN (o servidor aceita `TURN_URLS` e `TURN_SECRET`).
- O instalador não é assinado digitalmente, então o Windows mostra o aviso do SmartScreen.
- Não há recuperação de senha nem verificação de e-mail.
