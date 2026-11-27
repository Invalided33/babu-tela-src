# Arquitetura do Babu Tela

## Por onde passam os dados

| Fluxo | Caminho | Responsabilidade |
| --- | --- | --- |
| Chat e presença | Cliente → Worker → Durable Object `Hub` | O Hub mantém o banco SQLite e as conexões WebSocket. Confere permissões, armazena os envelopes das mensagens e distribui eventos. |
| Anexo de uma mensagem | Cliente → Worker → R2 | O Hub autoriza o envio. O Worker recebe as partes cifradas e lê o objeto no R2 sem manter o Hub ocupado durante a transferência. |
| Chamada | Cliente ↔ cliente, diretamente ou via TURN | O WebSocket entrega a sinalização. Cada par mantém sua conexão WebRTC para voz, vídeo e canais de dados. |
| Arquivo direto | PC de origem ↔ PC de destino, diretamente ou via TURN | O remetente guarda o original. O destinatário verifica e salva os pedaços. O servidor mantém a mensagem e encaminha a sinalização. |
| Música | Serviço de origem → PC do DJ → participantes | O PC de quem adicionou a faixa obtém o áudio. A fila e seus controles circulam pelo canal de dados da chamada. |

A instância principal usa o Hub chamado `main`. Cada execução dos testes remotos com `TEST_HUB` pode usar um Hub com nome e banco próprios. O código de entrada do Worker decide quais pedidos seguem direto ao R2 e quais vão para o Hub. [Entrada do Worker](../crates/worker/src/lib.rs) · [Operações com arquivos](../crates/worker/src/files.rs)

## Reconexão e presença

O cliente envia um ping a cada cinco segundos, inclusive com o microfone mudo e a janela em segundo plano. A Cloudflare responde sem acordar o Hub. Quando o Hub acorda, pode consultar o horário da última resposta automática e da última mensagem daquele cliente. Enquanto há alguém em chamada, um alarme verifica a presença a cada dez segundos. O servidor descarta conexões silenciosas por 25 segundos para retirar da sala quem perdeu energia ou rede. Esse mecanismo acompanha a presença mesmo quando as pessoas continuam trocando áudio por WebRTC. [Gateway do servidor](../crates/worker/src/gateway.rs) · [Estado das conexões](../crates/worker/src/state.rs)

## Áudio e vídeo

Cada participante transmite uma cópia da voz para cada ouvinte. O Opus reduz o envio nos períodos de silêncio (DTX). Conforme a sala cresce, o cliente diminui a taxa de voz e agrupa mais áudio em cada pacote para reduzir o custo dos cabeçalhos.

| Pessoas na sala, incluindo quem fala | Taxa de voz por conexão | Áudio por pacote |
| --- | --- | --- |
| Até 4 | 48 kb/s | 10 ms |
| 5 a 8 | 32 kb/s | 20 ms |
| 9 a 16 | 24 kb/s | 40 ms |
| 17 ou mais | 20 kb/s | 60 ms |

Para o vídeo, cada receptor pede a camada que sua interface mostra. Uma miniatura usa a camada pequena, a imagem ampliada pede a maior e um vídeo fora da tela pode parar de ser enviado para aquele receptor. Os relatórios de perda e atraso permitem reduzir a qualidade de uma conexão específica. Se a maioria apresentar congestionamento, o cliente diminui a taxa total de vídeo. Os sockets UDP pedem buffers de envio e recepção de 4 MiB para absorver rajadas. [Regras de voz e vídeo](../crates/app/src/voice/plan.rs) · [Buffers UDP](../crates/app/src/voice/udp.rs)

O cliente aplica a supressão de ruído entre o cancelamento de eco e o controle de ganho. A opção **IA** carrega o DPDFNet de 48 kHz em uma tarefa separada. Enquanto o modelo carrega, o cliente usa o supressor **Padrão**. O processamento pelo modelo acrescenta 50 ms de atraso. Em uma medição arquivada com dois clientes no mesmo PC e o canal de texto aberto, a mediana de boca a ouvido de Ana foi de 72,90 ms no modo Padrão e 123,18 ms no modo IA. Os relatórios registram as condições daquela rodada. [Implementação](../crates/app/src/voice/denoise.rs) · [Medição Padrão](../build/medidas/voz-ruido-padrao.json) · [Medição IA](../build/medidas/voz-ruido-ia.json)

## Transferência direta e retomada

Antes de anunciar o arquivo, o remetente calcula um SHA-256 para cada pedaço e um hash raiz sobre os tamanhos e os hashes, em ordem. O tamanho do pedaço começa em 4 MiB e dobra até a lista caber em 32.768 entradas. O destinatário recebe a lista pela conexão, confere a raiz com a mensagem e valida cada pedaço antes de registrar o progresso. Se a rede cair, ele retoma a partir do último pedaço conferido. Quem envia também confere o que lê do disco para detectar alterações no original. Cada origem atende até seis envios simultâneos, com até dois por destinatário. [Cálculo dos hashes](../crates/app/src/direct/hash.rs) · [Coordenação dos envios](../crates/app/src/direct/mod.rs)

O destinatário grava os pedaços sem esperar o disco e só força a gravação (FlushFileBuffers) no fim de cada tentativa, antes de registrar até onde chegou. No Windows, essa chamada segura todas as escritas no arquivo até terminar, mesmo as feitas por outra thread. Quando rodava a cada 5 s, ela chegou a levar 10,5 s no SSD SATA sem DRAM do PC de teste, e um download a 100 Mbit/s passou até 19% do tempo esperando o disco. Se o app ou o PC cair no meio de uma tentativa, a seguinte confere com os hashes os pedaços que já estão no arquivo parcial e só pede o resto, como faz um cliente BitTorrent. Num disco lento, a gravação do fim da tentativa ainda pode levar alguns segundos e atrasar a conclusão. [Download](../crates/app/src/direct/fetch.rs) · [Medições](../build/medidas/disco-download.md)

Depois do último bloco, quem envia espera o destinatário confirmar que recebeu tudo. Até 8 MiB ainda podem estar na fila da conexão nesse momento, então a espera dura enquanto os bytes pendentes continuam diminuindo e encerra após 60 s sem progresso. [Envio](../crates/app/src/direct/serve.rs)

## Controle de congestionamento

A voz e o vídeo vão por RTP, fora do SCTP. O vídeo ajusta a própria taxa pelos relatórios de perda e atraso. Os canais de dados vão por SCTP, e cada uso tem o seu controle. Os arquivos diretos usam o BBR v3 (draft-ietf-ccwg-bbr-06), com ritmo de envio e detecção de perda por tempo (RACK). O canal de dados das chamadas, que leva slides, traços e mensagens da chamada, usa CUBIC (RFC 9438). Numa perda, o CUBIC corta 30% da janela, e o Reno, metade. [BBR](../vendor/rtc-sctp/src/association/bbr.rs) · [CUBIC](../vendor/rtc-sctp/src/association/cubic.rs) · [Escolha nas chamadas](../crates/app/src/voice/rtc.rs)

As escolhas vêm de uma comparação com Reno, CUBIC e BBR em linhas de teste, com três rodadas por cenário. Com 40 ms de ida e volta e 0,1% de perda, o BBR transferiu a 72 MB/s, contra 1,5 MB/s do Reno e 2,0 MB/s do CUBIC. Num upload de 100 Mbit/s, chegou a 11,4 MB/s, contra 9,8 e 9,2 MB/s. A primeira versão do app, com BBR v1, mantinha em trânsito o dobro da capacidade do caminho mesmo quando havia perda. Com fila de 5 ms, reenviava 3,9% dos pacotes. Numa chamada por um upload de 2 Mbit/s com fila de 20 ms, um arquivo enviado ao mesmo tempo levava 10,8% da voz, contra 2,9% com Reno ou CUBIC.

O BBR v3 limita o que fica em trânsito depois de uma sondagem com perda e desacelera depois de rodadas com perda. Com fila de 5 ms, reenviou 0,1% dos pacotes, a 10,7 MB/s. Na chamada pelo upload de 2 Mbit/s, levou 1,5% da voz. Num upload de 20 Mbit/s com fila de 20 ms, levou 0,1%, contra 2,5% do v1. Com fila de 200 ms, somou 70 ms ao atraso da voz, contra 80 ms do Reno e 122 ms do CUBIC. O app tem uma diferença em relação ao draft: com menos de 63 pacotes na janela, espera de 2 a 3 s entre uma sondagem e outra. O draft sonda a cada tantas rodadas quantos pacotes a janela tem. Na chamada pelo upload de 2 Mbit/s, a regra do draft levou 4,0% da voz, e a do app, 0,8%, com a mesma velocidade.

No canal de dados da chamada, o CUBIC entregou dez slides de fotos em 24 a 34 s numa linha com 1% de perda, contra 44 a 50 s do Reno, sem diferença na voz. No upload de 2 Mbit/s, quatro slides levaram 32 s, contra 38 s do Reno, com 1,9% da voz perdida, contra 1,5%. O BBR v3 ficou perto disso, com 30 s e 1,3%. Ele exigiria timers de 1 ms durante a chamada inteira, por isso as chamadas continuam com CUBIC. [Medições](../build/medidas/controle-congestionamento.md)

## Reprodução de música

O DJ usa o yt-dlp para resolver o link em um endereço de mídia. Para arquivos progressivos, o cliente lê e decodifica enquanto o download continua. No caminho HLS, reúne os dados antes da decodificação. O áudio é convertido para PCM estéreo a 48 kHz e enviado como Opus na faixa de som do compartilhamento. A fila coordena a ordem das faixas, a pausa, o salto e o estado enviado a quem entra depois. O ajuste de volume do grupo é aplicado pelo DJ, e o volume pessoal, na saída de áudio de cada PC. [Extração](../crates/app/src/music/extractor.rs) · [Decodificação](../crates/app/src/music/stream.rs) · [Fila](../crates/app/src/music/queue.rs)
