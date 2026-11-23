# Babu Tela

Babu Tela é um aplicativo de comunidades para **Windows 10 e 11 (64 bits)**. Você pode conversar por texto, entrar em chamadas de voz e vídeo, compartilhar a tela, apresentar slides e ouvir música com o grupo. Mensagens e anexos são cifrados no computador de quem envia. O servidor ainda precisa de alguns metadados para entregar o conteúdo.

**Para usar**, baixe o instalador, abra o Babu Tela e entre com sua conta do Google. A página de download informa a versão, o tamanho e o SHA-256 do arquivo. [Baixar para Windows](https://invalided33.github.io/babu-tela/)

**Para desenvolver**, este repositório reúne o aplicativo nativo em Rust e egui, o servidor em WebAssembly para Cloudflare Workers, o instalador e as ferramentas de compilação e teste.

| Quero… | Guia |
| --- | --- |
| Entrar, enviar arquivos e usar a chamada | [Uso no Windows](docs/uso.md) |
| Aprovar um novo PC ou saber quem pode ver quais dados | [Privacidade e segurança](docs/privacidade.md) |
| Entender como chat, arquivos, áudio e vídeo chegam aos participantes | [Arquitetura](docs/arquitetura.md) |
| Compilar, testar uma mudança ou comparar desempenho | [Pré-requisitos](#o-que-você-precisa) e [testes](docs/desenvolvimento.md) |
| Hospedar uma instância ou distribuir um instalador | [Operação](docs/operacao.md) |

## O que você precisa

Para **compilar o app** no Windows x64, instale o Rust estável com a toolchain MSVC, o Visual Studio Build Tools com **Desenvolvimento para desktop com C++** e **Ferramentas do CMake para Windows**, e o NASM. O CMake compila a libopus. O NASM monta rotinas dos codecs AV1 e H.264. [Rustup](https://rustup.rs/) · [NASM](https://www.nasm.us/)

Na raiz do repositório, use o PowerShell para iniciar o app.

```powershell
cargo xtask dev
```

O comando compila e abre o app em modo de desenvolvimento. Ele usa o servidor do projeto por padrão. Para compilar contra outra instância, defina a variável de ambiente `BABUTELA_SERVER_URL` com a URL HTTPS antes de executar o comando. O `xtask` procura o CMake instalado pelo Visual Studio e o NASM no `PATH` ou nas pastas usuais de instalação. Para usar o Cargo diretamente, coloque os dois no `PATH`. Se adicionou o NASM depois de compilar o OpenH264, rode `cargo clean -p openh264-sys2` antes de recompilar.

## Na Cloudflare

Quem hospeda uma instância configura o Worker, o login do Google e os serviços opcionais em `build/cloudflare.json`. O arquivo contém credenciais e está no `.gitignore`. O comando `cargo xtask servidor` instala as ferramentas de WebAssembly necessárias e publica na conta configurada. [Configuração e primeiro deploy](docs/operacao.md#configurar-uma-instância)

## Comandos

Execute os comandos na raiz do repositório. `cargo xtask ajuda` mostra as opções principais.

| Comando | Resultado |
|---|---|
| `cargo xtask dev` | Abre o app de desenvolvimento no Windows. |
| `cargo xtask testar [--so-servidor] [URL]` | Roda os testes Rust e os testes de ponta a ponta em um banco de teste do Worker. Com `--so-servidor`, executa apenas a etapa remota. |
| `cargo xtask chamada` | Abre de duas a seis cópias do app e verifica a chamada em um banco de teste remoto. |
| `cargo xtask direto` | Transfere um arquivo entre dois apps e verifica o resultado depois de pausar e retomar. |
| `cargo xtask medir [cenários...]` | Mede um ou mais cenários com um banco de teste remoto e grava o resultado em `build/medidas/`. |
| `cargo xtask icones` | Regenera os SVGs em `crates/app/assets/icons/`. |
| `cargo xtask modelo [--atualizar]` | Compara a revisão do DPDFNet com a da Ceva. A opção atualiza o modelo local e o NOTICE depois de verificar o hash, a conversão e os testes. |
| `cargo xtask chave` | Cria a chave de assinatura local, se necessário, e exibe a parte pública. |
| `cargo xtask assinar VERSÃO SHA256` | Gera uma assinatura adicional para o instalador conferido pelo assinante. |
| `cargo xtask release [--endereco URL] [--so-instalador]` | Gera localmente `build/release/Babu Tela Setup.exe`. Com `--so-instalador`, pode reutilizar o app compactado no build anterior. |
| `cargo xtask servidor` | Compila o Worker e publica o código e os segredos na Cloudflare. |
| `cargo xtask emoji` | Sincroniza os arquivos Unicode e CLDR com o bucket R2 e confere a cópia servida pelo Worker. |
| `cargo xtask publicar` | Publica no R2 o instalador e o manifesto assinado para atualização dos apps. Registra a versão no servidor. |
| `cargo xtask pagina` | Publica o instalador no GitHub Releases e atualiza a página de download no repositório indicado por `pagina`. |
| `cargo xtask release update` | Gera e distribui uma nova versão do app, do servidor e da página de download. |

Para executar os testes locais ou medir um cenário específico, consulte [Testes](#testes). Para preparar chaves, gerar o instalador e distribuir uma versão, siga o [guia de operação](docs/operacao.md#gerar-o-instalador).

## Como funciona

O aplicativo Windows usa um WebSocket para o chat em tempo real. O Worker encaminha as operações de conta e conversa para o Durable Object `main`, que mantém o estado em SQLite. Ao enviar uma mensagem, o app já a mostra como pendente e usa um identificador de envio para reconhecer a confirmação. Se ela demorar, a repetição por HTTP usa o mesmo identificador para evitar duplicatas.

Os anexos comuns sobem cifrados para o R2 e continuam disponíveis quando quem os enviou fecha o app. Com **Enviar direto**, o arquivo permanece no PC de origem e a transferência pode continuar quando os dois computadores estiverem conectados. Chamadas, apresentações e música usam WebRTC entre participantes, com TURN quando é preciso um relé. Na chamada, o botão **Música** abre a fila coletiva e os controles de volume. [Guia de uso do Babu Tela](docs/uso.md)

### Login com o Google

Ao escolher **Continuar com o Google**, o app abre o navegador e recebe o retorno em uma porta local. Em outro PC, o login dá acesso à conta e a aprovação por um dispositivo anterior recupera as chaves das conversas. [Como aprovar outro computador](docs/privacidade.md#aprovar-outro-computador)

## Privacidade

Mensagens, anexos e sinalização são cifrados no cliente. O servidor recebe metadados como remetente, conversa, horário e participantes de uma chamada. As opções de privacidade controlam o indicador de digitação, o uso do relé para ocultar o IP dos demais participantes e o Tor integrado. O navegador do login e as conexões com provedores de música seguem sua própria configuração de rede. [Privacidade, chaves e serviços externos](docs/privacidade.md)

## Estrutura

| Caminho | Responsabilidade |
| --- | --- |
| [crates/app](crates/app/) | Cliente Windows. A interface está em `src/ui/`, o estado em `src/core.rs` e as preferências em `src/prefs.rs`. Os módulos `voice/`, `music/`, `slides/`, `ink/`, `media/` e `direct/` ficam em `src/`. |
| [crates/app/src/e2e](crates/app/src/e2e/) | Chaves, verificação de identidades e criptografia de ponta a ponta. A integração com o chat está em `src/core/sealing.rs` do cliente. |
| [crates/worker](crates/worker/) | API HTTP e WebSocket, autenticação, permissões e sinalização. O Durable Object mantém o estado do chat em SQLite. R2 armazena os arquivos enviados ao servidor. |
| [crates/setup](crates/setup/) e [crates/buildinfo](crates/buildinfo/) | Instalador e desinstalador por usuário, versão da aplicação e convenções compartilhadas entre cliente, instalador e servidor. A versão do produto vem do `Cargo.toml` da raiz. |
| [crates/denoise](crates/denoise/) | Supressão de ruído de voz. A pasta `model/` contém o modelo DPDFNet, sua origem, alterações e licença. |
| [crates/xtask](crates/xtask/) | Compilação, assinatura, publicação e testes. O modelo da página de download fica em `pagina/index.html`. |
| [build](build/) | Ícones, configuração do build nativo e relatórios de medição. A configuração privada `build/cloudflare.json` e as saídas de `build/release/` são locais. |

O Worker tem workspace e `Cargo.lock` próprios. Ele gera WebAssembly para publicação e permite rodar seus testes Rust no PC. A raiz substitui quatro dependências pelo código em `vendor/` usando `[patch.crates-io]`. Os manifests de cada cópia descrevem a diferença em relação à versão de origem.

| Dependência local | Motivo |
| --- | --- |
| [rtc](vendor/rtc/Cargo.toml) e [rtc-dtls](vendor/rtc-dtls/Cargo.toml) | Preservam a primeira mensagem do canal de dados e incorporam ao DTLS o segredo extra negociado pelos clientes. [Correção do canal no projeto de origem](https://github.com/webrtc-rs/rtc/pull/292) |
| [saturating-time](vendor/saturating-time/Cargo.toml) | Encerra a busca pelo menor instante representável quando o passo é inferior à resolução do relógio do Windows. |
| [tor-dirclient](vendor/tor-dirclient/Cargo.toml) | Aplica o prazo de dez segundos à inatividade da conexão durante o download do diretório Tor, permitindo transferências lentas que continuam progredindo. |

## Idiomas

Em **Configurações → Idioma**, escolha português ou inglês. No primeiro uso, o cliente adota português para localidades `pt` e inglês para as demais localidades identificadas no Windows. A escolha fica em `prefs.json` e aparece já no quadro seguinte da interface. Os nomes de busca de emoji vêm do CLDR em português.

Para acrescentar uma mensagem, inclua a mesma chave nos dois catálogos e preserve os marcadores como `{nome}`. Use `t!("chave")` para texto simples e `tf!("chave", nome = valor)` para interpolação. Uma chave ausente recorre primeiro ao catálogo português e depois ao próprio identificador. Os testes comparam as chaves e os marcadores nos dois idiomas. [Catálogos](crates/app/assets/lang/) · [Implementação e testes](crates/app/src/lang.rs)

## Testes

Na raiz do projeto, rode os dois workspaces para validar o código localmente.

```powershell
cargo test --locked --workspace
cargo test --locked --manifest-path crates/worker/Cargo.toml
```

Para validar um servidor publicado, `cargo xtask testar` executa também a suíte de ponta a ponta com um banco de teste isolado. Os cenários de chamada, transferência direta, medição e automação da interface têm instruções próprias, incluindo como fornecer a configuração de teste e interpretar o resultado. [Guia de desenvolvimento e testes](docs/desenvolvimento.md)

## Segurança

- O login pelo Google usa PKCE, confere o parâmetro `state` e recebe o retorno em `127.0.0.1`. O Worker guarda o hash do token de sessão e invalida sessões após 30 dias sem uso. A rota de criação de sessões de teste exige um banco selecionado com `TEST_HUB`. [Login do cliente](crates/app/src/google.rs) · [Sessões do Worker](crates/worker/src/state.rs)
- Cada parte de um upload usa uma autorização assinada com `PEPPER`, vinculada ao arquivo e válida por sete dias. O servidor confere tamanhos e limita operações por conta. Para um arquivo direto, o Worker confere o vínculo com a mensagem, o acesso e o bloqueio entre contas. O cliente valida o hash de cada pedaço ao receber. [Uploads](crates/worker/src/files.rs) · [Arquivos diretos](crates/worker/src/direct.rs)
- Ao abrir uma mídia externamente, o cliente verifica o tipo e a extensão e a grava decifrada em `%TEMP%\babutela-arquivos`. Para os demais anexos, oferece **Salvar**. Os arquivos que o usuário salva ficam acessíveis às aplicações que tenham permissão de leitura no PC. [Abertura de anexos](crates/app/src/core/sealing.rs)
- O atualizador verifica as assinaturas exigidas, o registro público da versão e o SHA-256 do instalador antes de executá-lo. O instalador gerado pelo projeto pode provocar um aviso de editor desconhecido no Windows SmartScreen. [Verificação das atualizações](crates/app/src/update.rs)

## Limites conhecidos

- **Histórico e dispositivos.** Perder todos os PCs com as chaves de uma conta pode tornar o histórico ilegível. Quem entra em uma conversa passa a receber as sessões de que participa. A reprodução de vídeo cifrado no chat exige o download completo.
- **Rede.** Em chamadas em malha, o upload cresce com o número de participantes. O cliente distribui a banda entre voz e vídeo e adapta a qualidade da imagem. Usar Tor aumenta a latência e requer uma rota de relé para as chamadas. A opção **Esconder meu IP** depende de TURN também nos arquivos diretos. O transporte por relé e o armazenamento R2 contam no consumo da conta Cloudflare.
- **Enviar direto.** O destinatário precisa que o arquivo original continue disponível no PC que o enviou e que os dois apps estejam abertos para transferir. Uma interrupção pode ser retomada. O status invisível ainda permite servir o arquivo, e uma segunda instalação da mesma conta não pode baixá-lo por esse mecanismo.
- **Tela e áudio.** A captura do som do processo e a sobreposição de anotações dependem das APIs do Windows. Conteúdo protegido pode aparecer preto na captura. A disponibilidade do codificador de vídeo por hardware depende do driver e da placa. O cliente tem um caminho H.264 por software e decodifica AV1 no processador. O histórico de testes do projeto registra compartilhamento por GPU em uma NVIDIA RTX 5050 e chamadas entre até seis clientes no mesmo PC. [Implementação da captura](crates/app/src/voice/hw.rs)
- **Apresentações.** O app aceita arquivos de até 300 MiB e envia imagens de até 1920 × 1080. Um PPTX pode revelar elementos a cada clique. Efeitos de movimento, saída ou ênfase, transições e mídia embutida não são reproduzidos. Um PDF exportado preserva melhor a aparência de um slide cujo gráfico, efeito ou fonte difere no cliente. A conversão automática de `.ppt` antigo depende do PowerPoint instalado. [Renderização](crates/app/src/slides/render.rs) · [Etapas por clique](crates/app/src/slides/timing.rs)
- **Serviços externos.** O seletor de figurinhas compartilha a cota da chave GIPHY entre os usuários daquela instalação do servidor. Uma figurinha removida pelo provedor pode deixar de aparecer. A reprodução de música depende de acesso do PC que fornece a faixa à origem do áudio e às ferramentas de extração.

## Créditos

- Dados de emoji do Unicode e nomes em português do CLDR, © Unicode, Inc., sob a Licença Unicode v3. O cliente consulta a cópia do servidor e tem os endereços oficiais como alternativa. A licença integra a tela **Configurações → Licenças**. [Licença incluída](crates/app/assets/UNICODE-LICENSE.txt) · [Origens dos dados](crates/buildinfo/src/lib.rs)
- Fonte Noto Color Emoji, © 2022 Google Inc., sob SIL Open Font License 1.1. As bandeiras usam a fonte Twemoji Country Flags de talkjs (MIT) e desenhos Twemoji, © X Corp. e colaboradores (CC BY 4.0). [Licença da fonte Noto](crates/app/assets/fonts/NotoColorEmoji-OFL.txt) · [Twemoji Country Flags](https://github.com/talkjs/country-flag-emoji-polyfill) · [Twemoji](https://github.com/twitter/twemoji)
- Supressão de ruído com o modelo DPDFNet da Ceva, sob Apache 2.0, executado com tract. [Origem e mudanças no modelo](crates/denoise/model/NOTICE) · [Licença do modelo](crates/denoise/model/LICENSE) · [tract](https://github.com/sonos/tract)
- Slides de PowerPoint renderizados pelo BetterOffice (Apache 2.0) com tiny-skia e rustybuzz. O leitor de PDF é o do Windows. [BetterOffice](https://github.com/openooxml/betteroffice) · [tiny-skia](https://github.com/linebender/tiny-skia) · [rustybuzz](https://github.com/harfbuzz/rustybuzz)
- O cliente usa egui, webrtc-rs, libopus, OpenH264, Symphonia e Sonora. A decodificação AV1 usa rav1d (BSD-2-Clause), derivado do dav1d, © VideoLAN, autores do dav1d e Internet Security Research Group. [rav1d](https://github.com/memorysafety/rav1d) · [Dependências do cliente](Cargo.lock)
- Figurinhas **Powered by GIPHY**. Os recursos de atribuição estão em `crates/app/assets/giphy/`. Babu Tela é um cliente independente. [GIPHY](https://giphy.com) · [Termos da API](https://support.giphy.com/hc/en-us/articles/360028134111-GIPHY-API-Terms-of-Service)
- Para a música em grupo, o cliente obtém as ferramentas yt-dlp e QuickJS-NG nas distribuições oficiais e verifica os hashes dos downloads. [yt-dlp](https://github.com/yt-dlp/yt-dlp) · [QuickJS-NG](https://github.com/quickjs-ng/quickjs) · [Gerenciamento das ferramentas](crates/app/src/music/ytdlp.rs)
