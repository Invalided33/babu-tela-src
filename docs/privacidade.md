# Privacidade e segurança das conversas

O Babu Tela cifra o conteúdo das mensagens e anexos antes do envio. Os computadores autorizados guardam as chaves para lê-los. Contas, permissões, presença e entrega dependem do Worker. A proteção se aplica de forma diferente ao conteúdo, aos dados necessários para entregá-lo e às conexões feitas com serviços de terceiros.

## Quem recebe quais dados

| Destino | Dados que recebe |
| --- | --- |
| Worker e Durable Object | Conta e ID do Google, e-mail, nome, foto, status, amizades, bloqueios, servidores, canais, cargos e membros. Para encaminhar mensagens, o servidor conhece remetente, conversa, horário, tamanhos aproximados e contagens de anexos e reações. Também recebe eventos de presença, participação nas chamadas, pedidos de arquivos diretos, favoritos do GIPHY e, quando ativado, o indicador de digitação. |
| R2 | Bytes cifrados dos anexos de mensagens, com tamanho preenchido e nome genérico. Outros objetos, como avatares, imagens de servidores, listas de emoji e instaladores, são distribuídos para exibição ou download. |
| Participantes de uma conversa ou chamada | Conteúdo destinado a eles, incluindo os arquivos que podem baixar. Na música, os participantes recebem o link, o título, a fila e o áudio compartilhado pelo DJ. Numa conexão WebRTC direta, cada ponta também pode conhecer o IP da outra. |
| Moderação | Quem denunciou, qual mensagem e o motivo escrito da denúncia. A leitura do conteúdo cifrado depende das chaves de quem já podia ler o canal. |
| Cloudflare e relé TURN | Dados de conexão, como endereço IP, duração e volume de tráfego. A mídia e os dados WebRTC continuam cifrados durante o encaminhamento pelo relé. O Worker é publicado com os registros de requisições desativados pelo xtask. |
| Serviços externos | Google recebe o login. GIPHY recebe consultas, imagens solicitadas e eventos de interação associados a um identificador derivado da conta. Ao usar música, GitHub fornece as ferramentas, e os provedores de mídia recebem as requisições feitas pelo PC que adiciona a faixa. |

## Aprovar outro computador

1. Entre com a conta Google no PC novo. O app cria uma chave temporária e mostra um código de 16 letras e números.
2. Em um PC com as chaves da mesma conta, abra **Aprovar um PC novo** e digite o código que aparece no novo. O computador antigo confere a chave pelo código e cifra o material para aquele destinatário.
3. Abra as conversas no PC novo. Ele já pode obter do servidor as sessões cifradas que a conta tem direito de abrir.

A sessão do login e as chaves ficam protegidas pela DPAPI do usuário do Windows. Se perder todos os computadores que têm as chaves, **Criar uma identidade nova** permite voltar a conversar. As mensagens cifradas para a identidade anterior ficam inacessíveis e os contatos recebem um aviso de mudança. Um computador comprometido enquanto a conta do Windows está aberta pode acessar o conteúdo que a pessoa consegue ler. [Aprovação de PC](../crates/app/src/e2e/provision.rs) · [Armazenamento de chaves](../crates/app/src/e2e/store.rs)

## Conferir contatos e membros

No perfil da outra pessoa, abra **Código de segurança** e compare os 60 dígitos por um canal independente. **Marcar como conferido** registra a verificação. A primeira identidade vista fica fixada. Se aparecer outra, o app avisa e aguarda a aceitação antes de enviar à nova chave.

Cada PC mantém uma cópia de um registro que encadeia, por SHA-256, as chaves publicadas e as versões do aplicativo. O tamanho e o hash dessa lista acompanham mensagens e sinalização. A comparação entre participantes pode revelar listas incompatíveis ou uma chave publicada em nome da própria conta. Essa comparação depende do contato entre os grupos que receberam listas diferentes. [Registro e comparação entre clientes](../crates/app/src/e2e/record.rs)

Num servidor, as assinaturas de entrada formam uma cadeia a partir do dono. Um convite assinado, a aprovação de um pedido ou a garantia de outro membro autoriza a entrega das chaves. Quando o convite inclui um segredo, compartilhe o link completo apenas com o destinatário. A saída de um membro ainda depende da lista fornecida pelo servidor. Um servidor desonesto que mantenha um ex-membro nessa lista pode fazer com que ele continue a receber chaves. [Certificados de membros e convites](../crates/app/src/e2e/members.rs)

## Conteúdo e prazo das mensagens

- **Identidade e sessões.** A identidade Ed25519 assina as mensagens e a chave pública X-Wing da conta. O X-Wing combina X25519 com ML-KEM-768 para distribuir a chave de cada sessão aos leitores. A sessão de envio de cada PC muda quando os leitores mudam, após sete dias ou após 2.000 mensagens. O servidor entrega a cada conta sua cópia selada da chave. Para ler conversas permanentes mais tarde, as chaves correspondentes são preservadas. [Chaves e identidades](../crates/app/src/e2e/keys.rs) · [Sessões e envelopes](../crates/app/src/e2e/msg.rs)
- **Mensagens e reações.** O envelope protege texto, respostas, referências a figurinhas e dados dos anexos. Cada mensagem usa uma chave derivada por HKDF, com AES-256-GCM e assinatura do remetente. O tamanho é preenchido em faixas, com mínimo de 512 bytes. As reações incluem o emoji cifrado e uma marca por mensagem para contar reações iguais. Uma falha de autenticação pode aparecer como **Mensagem recusada**.
- **Anexos.** Cada arquivo enviado ao R2 ganha uma chave própria, entregue dentro do envelope. A cifra autentica blocos de 64 KiB, a posição e o fim do arquivo. O destinatário pode salvar uma cópia decifrada. Abrir um anexo em um programa externo também pode deixar uma cópia em `%TEMP%\babutela-arquivos`. [Cifra de arquivos](../crates/app/src/e2e/file.rs)
- **Mensagens temporárias.** No relógio da conversa, escolha **1 dia** ou **7 dias** para as novas mensagens. Em conversas diretas, as duas pessoas podem mudar o prazo. Nos canais, a escolha cabe a quem tem permissão de gerenciar canais. O prazo é assinado no envelope, o cliente retira a mensagem no vencimento e o servidor remove os dados na limpeza seguinte. Essas conversas usam sessões diárias e chaves efêmeras. O cliente descarta chaves diárias com mais de oito dias. Quem recebeu a mensagem ainda pode copiar o texto ou salvar o anexo enquanto consegue abri-lo.

Se todos os PCs de uma conta ficarem fechados por mais de uma semana, podem faltar chaves diárias para receber novas mensagens temporárias. Nesse caso, o remetente vê um aviso com o nome de quem ficará sem acesso àquela mensagem. Ao reabrir o app, a conta pode publicar chaves para os próximos envios. [Aviso sobre chaves indisponíveis](../crates/app/src/core/sealing.rs)

## IP, Tor e serviços externos

Em **Configurações → Privacidade**, **Esconder meu IP** faz as novas conexões de chamada e transferência direta passarem pelo TURN. Os outros participantes veem o endereço do relé. Para deixar de enviar eventos de digitação, desligue **Mostrar quando estou digitando**.

**Conectar pelo Tor** reinicia o app e usa o Arti integrado. A API, o gateway WebSocket, o GIPHY e os downloads que usam o cliente HTTP do app passam pelo Tor. Chamadas e arquivos diretos usam TURN por TCP ou TLS através do Tor. A primeira conexão pode demorar. Se a conexão falhar, o app oferece a opção de abrir sem o Tor. O servidor continua identificando a conta autenticada. [Rede do aplicativo](../crates/app/src/net.rs) · [Tor](../crates/app/src/tor.rs)

O navegador usado em **Continuar com o Google** segue sua própria configuração de rede. O recurso **Música** também tem conexões próprias fora do Tor integrado. Ao abrir a janela de música, a consulta e o download das ferramentas podem acessar o GitHub. Ao adicionar um link, o yt-dlp consulta a plataforma e o app baixa a mídia. Esses serviços podem ver o IP de saída da conexão do DJ. Se o YouTube pedir autenticação, o yt-dlp tenta usar uma sessão existente no Chrome, Edge, Firefox, Brave, Opera ou Vivaldi. O áudio entre participantes segue a conexão cifrada da chamada. [Ferramentas de música](../crates/app/src/music/ytdlp.rs) · [Leitura dos links](../crates/app/src/music/extractor.rs) · [Download da mídia](../crates/app/src/music/stream.rs)

## Chamadas, arquivos diretos e atualizações

As ofertas, respostas e candidatos do WebRTC são selados e assinados para o outro participante, com identificação da chamada ou do arquivo e janela de validade de dez minutos. Cada lado inclui um segredo aleatório de 32 bytes na descrição protegida. A implementação local de DTLS mistura o segredo combinado ao handshake ECDHE. Assim, a derivação das chaves da mídia e dos canais de dados também depende da sinalização protegida por X-Wing. As assinaturas de identidade e das atualizações usam Ed25519, um algoritmo clássico. [Sinalização](../crates/app/src/e2e/signals.rs) · [Integração DTLS](../crates/app/src/e2e/dtls.rs)

O atualizador verifica a entrada no registro de versões, o SHA-256 do instalador e as assinaturas Ed25519 de `latest.json`. O projeto pode exigir mais de um assinante por meio de `assinantes.txt`. Uma instalação confia nas chaves públicas incluídas no seu build. O registro permite conferir a publicação da versão, mas comprovar a correspondência binária com determinado código-fonte exige um build reproduzível. Essa verificação se aplica às atualizações do Babu Tela. As ferramentas de música têm verificação de download própria. [Verificação do instalador](../crates/app/src/update.rs) · [Assinatura de releases](../crates/xtask/src/signing.rs)
