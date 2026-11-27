# Usar o Babu Tela no Windows

## Entrar e retomar uma conversa

Escolha **Continuar com o Google**. O aplicativo abre o navegador padrão e recebe o retorno do login numa porta de uso temporário em `127.0.0.1`. O cliente usa PKCE, e o Worker troca o código de uso único pela identificação da conta. O Babu Tela identifica a pessoa pelo ID do Google, mesmo que o e-mail mude. O e-mail aparece para a própria pessoa em **Configurações → Conta**.

Na primeira entrada em outro PC, aprove o dispositivo em um computador que já tem as chaves da conta. Sem essa aprovação, as mensagens anteriores aparecem trancadas. [Como aprovar um PC e conferir identidades](privacidade.md#aprovar-outro-computador)

O instalador grava o app em `%LOCALAPPDATA%\Programs\BabuTela` e cria os atalhos para o usuário do Windows. Uma notificação do chat abre a conversa correspondente, inclusive quando é preciso iniciar o app. A cópia instalada verifica atualizações na abertura e a cada quatro horas enquanto está em uso. Quando a atualização está pronta, ela entra na próxima abertura ou ao clicar em **Reiniciar agora**. O instalador aceita a opção `/S` para instalação silenciosa. [Instalador](../crates/setup) · [Atualizador](../crates/app/src/update.rs)

## Conversas, emoji e figurinhas

Use uma conversa direta para falar com uma pessoa ou um canal de texto para falar com os membros de um servidor. Quem gerencia o servidor pode mudar sua imagem e controlar a entrada por convite e a aprovação dos pedidos. Digite **@** para procurar uma pessoa pelo nome ou nome de usuário. Uma mensagem aceita até 100 anexos e 100 mil unidades de texto UTF-16. Alguns emoji ocupam mais de uma unidade. Mensagens longas aparecem recolhidas até você abrir o texto inteiro.

O seletor de emoji pesquisa nomes e palavras-chave em português, inclusive sem acentos. Use o botão da mão para escolher um tom de pele. Em emoji com duas pessoas, o botão direito permite combinar os dois tons. Os emoji recentes ficam disponíveis no início do seletor. As listas vêm do Unicode e do CLDR por meio do servidor configurado. O app usa as fontes do Windows, as fontes incluídas para bandeiras e o Noto Color Emoji para completar a cobertura.

No seletor de figurinhas, escolha **Em alta**, pesquise ou abra **Favoritas**. A estrela salva uma favorita para a conta e um clique na imagem envia uma mensagem de figurinha. Os resultados e as animações vêm do GIPHY, que recebe as consultas e eventos de visualização, clique e envio. O conteúdo da mensagem com o ID da figurinha segue cifrado como o restante da conversa. [Detalhes sobre os dados compartilhados](privacidade.md#quem-recebe-quais-dados)

## Enviar e baixar arquivos

| Modo | Onde os dados ficam | Quando quem enviou pode fechar o app |
| --- | --- | --- |
| Anexar à mensagem | O app cifra o arquivo em blocos e envia as partes ao R2. Nome, tipo e chave seguem dentro da mensagem cifrada. | Depois que a mensagem com o anexo for confirmada. O destinatário pode baixar em outro momento. |
| **Enviar direto** | O arquivo permanece no disco de quem envia. A mensagem contém os dados para verificar a cópia, e o destinatário baixa por WebRTC. | Tanto a origem quanto o destino precisam estar com o app aberto durante a transferência. O download pode continuar quando a origem voltar. |

O espaço disponível no servidor costuma limitar o tamanho de um anexo antes do máximo técnico do upload, cerca de 996 GB por objeto cifrado. Por padrão, todos os anexos da instância dividem uma cota de 10 GB. **Enviar direto** dispensa esse espaço no servidor. [Configuração da cota](operacao.md#configurar-uma-instância)

Na caixa de mensagem, **Ctrl+V** anexa os arquivos copiados no Explorador de Arquivos ou uma imagem copiada, como um recorte da Ferramenta de Captura, uma imagem do navegador ou um desenho do Paint. A imagem entra como um PNG com a data e a hora no nome. Se o conteúdo copiado também tiver texto, como células do Excel, o app cola o texto. A exceção é o endereço que alguns navegadores copiam junto com a imagem. **Ctrl+Shift+V** cola apenas o texto.

Para usar **Enviar direto**, anexe os arquivos e marque a opção embaixo da lista antes de enviar. Ela vale para todos os anexos daquela mensagem. O app calcula os hashes antes de disponibilizar o arquivo e verifica cada pedaço que chega. Mantenha o original no mesmo lugar até terminar de compartilhar. O cartão permite pausar, continuar ou cancelar o download, e mostra se os PCs estão usando o relé. Quem enviou pode interromper a distribuição com **Parar de compartilhar** ou apagando a mensagem.

Numa conexão direta, a velocidade depende do upload de quem envia e do download de quem recebe. A transferência mantém a velocidade da linha mesmo com perdas ocasionais de pacotes, comuns no Wi-Fi e em rotas longas. Pelo relé, o app não limita a velocidade. O TURN da Cloudflare deixa passar rajadas por alguns segundos e depois descarta o que passa do limite de cada conexão, e o controle de congestionamento se ajusta. Num teste com os dois PCs pelo relé, 1000 MB passaram a cerca de 19 MB/s. Em uma de oito tentativas, a Cloudflare cortou a conexão no meio da rajada, e o app retomou do último pedaço conferido cerca de 30 s depois. [Medições](../build/medidas/rele-sem-teto.md) Os totais de cada transferência ficam em `medidas.log`, na pasta dos arquivos diretos. O arquivo mostra a velocidade, o caminho usado, o tempo de ida e volta, as perdas e se a espera foi pela rede ou pelo disco.

**Baixar** abre um diálogo para escolher o destino. Durante a transferência, o app grava um arquivo com o sufixo `.parcial` e só atribui o nome escolhido depois da conferência. No envio direto, os pedaços já verificados podem ser retomados. O Windows recebe a marca de arquivo vindo da internet. Ao terminar, use **Abrir** ou **Mostrar na pasta**.

Para usar uma imagem da conversa em outro programa, passe o mouse sobre ela e clique no botão de copiar, ou clique nela com o botão direito e escolha **Copiar imagem**. O app coloca a imagem decifrada na área de transferência do Windows, com a transparência, pronta para colar com Ctrl+V. [Área de transferência](../crates/app/src/clip.rs)

Os anexos de áudio tocam na conversa enquanto chegam e são decifrados em memória. O vídeo cifrado é baixado e decifrado em uma pasta temporária para o Media Foundation do Windows. O app apaga essa cópia ao fechar o player. O suporte a formatos de vídeo depende dos codecs instalados. [Transferência direta](../crates/app/src/direct) · [Uploads no Worker](../crates/worker/src/files.rs) · [Reprodução de mídia](../crates/app/src/media)

## Chamar, compartilhar e apresentar

Em **Configurações → Voz e vídeo**, escolha o microfone, a saída de áudio e a supressão de ruído. A opção **IA** usa o DPDFNet no próprio PC. A voz também passa por cancelamento de eco e ganho automático. Na chamada, o menu do botão direito sobre uma pessoa permite ajustar o volume de sua voz e o som de sua transmissão separadamente.

- **Tela ou janela.** Abra **Compartilhar**, escolha a origem e a qualidade. **Compartilhar o áudio** vem marcado. Para uma tela inteira, ele captura o som do PC, excluindo o Babu Tela para evitar eco. Para uma janela, captura o áudio do programa e de seus processos filhos quando o Windows permite. Se minimizar a janela compartilhada, quem assiste fica com a última imagem capturada até você restaurá-la. Conteúdo protegido pelo Windows pode aparecer preto.
- **Apresentação.** Selecione um arquivo PowerPoint (`.pptx` ou `.ppsx`) ou PDF. O app do apresentador renderiza cada slide e distribui as imagens pelo canal de dados da chamada. O público pode ampliar um slide, navegar por conta própria e usar **Voltar ao apresentador**. As notas e a prévia do próximo slide ficam com o apresentador. No PowerPoint, as entradas por clique aparecem em etapas, sem reproduzir o movimento dos efeitos. Um PDF tem uma imagem por página.
- **Anotação.** Abra **Anotar** na tela ampliada ou no slide para desenhar sobre a imagem. Quem compartilha escolhe em **Quem pode anotar** se todos participam e pode limpar as marcas. Cada pessoa também pode limpar as próprias anotações.

A voz e o som compartilhado usam faixas Opus separadas. O vídeo usa H.264 e, no compartilhamento de tela, pode usar AV1 quando a placa de quem envia e os participantes permitem. Cada par de pessoas mantém uma conexão WebRTC. Se a conexão direta falhar, os pacotes cifrados podem passar pelo TURN. A taxa de voz e o tamanho dos pacotes variam com a quantidade de pessoas, e o app pede miniaturas ou imagens maiores conforme o que cada participante vê. O envio de vídeo consome mais upload à medida que o grupo cresce. [Captura e transporte](../crates/app/src/voice) · [Apresentações](../crates/app/src/slides)

## Música na chamada

1. Entre em uma chamada e abra **Música**, no botão da nota musical. Na primeira abertura, o app prepara duas ferramentas baixadas do GitHub, yt-dlp e QuickJS-NG.
2. Cole um link de YouTube, YouTube Music, youtu.be ou SoundCloud e clique em **Adicionar**. Você pode adicionar mais links enquanto os anteriores estão em **Buscando…**. Os links enviados pelo mesmo PC entram na ordem em que foram adicionados.
3. Use **Pausar**, **Tocar** ou **Pular** para controlar a faixa atual. A lixeira retira uma faixa da fila. Qualquer participante pode usar esses controles. Quem adicionou a faixa atua como DJ e seu PC envia o áudio estéreo pela chamada. Se essa pessoa sair, suas faixas saem da fila. O próximo participante com uma faixa na fila assume a reprodução.

| Controle | Efeito |
| --- | --- |
| **Volume para todos** (0 a 100%) | Altera o nível da música enviada pelo DJ. Quem entrar depois recebe o ajuste junto com o estado da fila. |
| **Volume só para mim** (0 a 200%) | Ajusta a reprodução neste PC e fica salvo para as próximas chamadas. Enquanto a música toca, o som da tela compartilhada pelo DJ também recebe esse ajuste, pois ambos chegam pela mesma faixa. |

O servidor do Babu Tela não armazena as músicas nem a fila. O DJ obtém a mídia pela conexão do próprio PC e envia o áudio aos demais. A fila termina quando a última pessoa sai. A disponibilidade depende do serviço de origem. Se o YouTube exigir login, o yt-dlp tenta usar os cookies de sessão dos navegadores compatíveis no PC do DJ. O guia de privacidade explica quais conexões usam o Tor. [Privacidade da música e do login](privacidade.md#ip-tor-e-serviços-externos)

As ferramentas ficam em `%USERPROFILE%\.babutela\yt-dlp`. O app confere o SHA-256 do pacote do yt-dlp com o publicado na mesma release e o SHA-256 do QuickJS-NG com o valor fixado no código. Ao usar o recurso, verifica a versão do yt-dlp com intervalo de 24 horas e pode aproveitar a cópia instalada se a consulta ao GitHub falhar. [Fila, extração e reprodução](../crates/app/src/music)

## Dados no computador

| Local padrão | Conteúdo |
| --- | --- |
| `%APPDATA%\Prosa` | Configurações (`settings.json`), preferências (`prefs.json`), sessão protegida (`session.bin`), chaves e registro de identidades por conta. O nome da pasta vem da versão antiga do app. |
| `%APPDATA%\Prosa\cache` e `%APPDATA%\Prosa\tor` | Imagens em cache, arquivos de emoji e, ao usar Tor, dados locais do Arti. O cache conserva as imagens cifradas como vieram do servidor. |
| `%LOCALAPPDATA%\Prosa\direto` | Índice dos arquivos compartilhados por este PC, dos downloads diretos e de seu progresso, e as medidas de cada transferência (`medidas.log`, até 512 KB, mais uma cópia anterior). |
| `%LOCALAPPDATA%\Prosa\colados` | Imagens coladas com Ctrl+V à espera do envio. O app apaga cada uma quando a mensagem chega ao servidor ou quando o anexo é retirado. Uma imagem colada e enviada direto fica disponível por 30 dias. Depois desse prazo, o app apaga as que sobraram. |
| `%LOCALAPPDATA%\Prosa\updates` | Instalador preparado para atualização e seu estado de verificação. |
| `%TEMP%\babutela-arquivos` e `%TEMP%\babutela-midia` | Cópias decifradas de anexos abertos em outros programas e de vídeos em reprodução, respectivamente. |

O endereço do servidor vem do build (`BABUTELA_SERVER_URL`). Quem usa uma instância própria pode definir `serverUrlOverride` em `settings.json`. As preferências incluem idioma, qualidade do vídeo, dispositivos de áudio, volume local da música, ajustes por participante, pasta de download e opções de privacidade. [Configurações](../crates/app/src/shell.rs) · [Preferências](../crates/app/src/prefs.rs)
