# Downloads diretos e o disco

Medido em 26/11/2026, com duas cópias do app (perfil `medir`) no mesmo PC e
o servidor de teste na Cloudflare só para a sinalização. Beto baixa para
`%TEMP%`, no C:, um Kingston SA400 (SSD SATA sem DRAM), com a proteção em
tempo real do Windows Defender ligada. As duas versões rodaram intercaladas,
três rodadas por cenário, com a ordem invertida a cada rodada. Comandos em
[desenvolvimento.md](../../docs/desenvolvimento.md#diagnosticar-a-velocidade-do-envio-direto).

- **antes**: o download força a gravação no disco (FlushFileBuffers) a cada
  5 s, na thread que escreve, como até o commit d40ad90.
- **depois**: só no fim de cada tentativa. Depois de uma queda, a tentativa
  seguinte confere com os hashes o que está no arquivo parcial.

Na primeira série, o teste ainda gravava no mesmo disco o arquivo de origem
quando o envio começava. Na segunda, ele termina de gravá-lo antes, como
seria com quem envia em outro PC.

**Total** vai de "Baixar" até o download pronto, com a conexão e a gravação
do fim. **Dados** vai do primeiro ao último byte no Beto. **Esperou o disco**
é a parte de Dados em que o download esperou a fila do disco. **Janela
fechada** conta os segundos em que a Ana viu menos de 64 KB de janela do
Beto. **Sync do fim**: na versão depois, o log só registra o que passa de
2 s. Os números de cada rodada estão em
[disco-download.jsonl](disco-download.jsonl).

### A: 100 Mbit/s, 500 MB

`--mb 500 --sem-pausa --rede-ana atraso=20,banda=100,fila=100 --rede-beto atraso=20`

| Série | Versão | Total (s) | Dados (s) | Esperou o disco | Janela fechada | Timeouts do envio | Pior sync no meio | Sync do fim |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| fonte sem sync | antes | 50,1 · 60,5 · 50,4 | 45,6 · 53,1 · 45,6 | 6% · 19% · 2% | 0 s · 0 s · 0 s | 0 · 0 · 0 | 2,98 s · 10,51 s · 1,54 s | 0,01 s · 2,95 s · 0,11 s |
| fonte sem sync | depois | 50,2 · 55,7 · 50,5 | 45,6 · 45,6 · 45,6 | 0% · 0% · 0% | 0 s · 0 s · 0 s | 0 · 0 · 0 | — · — · — | < 2 s · 5,8 s · < 2 s |
| fonte no disco | antes | 54,9 · 50,1 · 49,9 | 50,6 · 45,6 · 45,6 | 18% · 0% · 0% | 0 s · 0 s · 0 s | 0 · 0 · 0 | 6,12 s · 0,17 s · 0,06 s | 0,04 s · 0,14 s · 0,01 s |
| fonte no disco | depois | 50,0 · 50,3 · 50,7 | 45,6 · 45,6 · 45,6 | 0% · 0% · 0% | 0 s · 0 s · 0 s | 0 · 0 · 0 | — · — · — | < 2 s · < 2 s · < 2 s |

### B: sem linha de teste, 1000 MB

`--mb 1000 --sem-pausa`

| Série | Versão | Total (s) | Dados (s) | Esperou o disco | Janela fechada | Timeouts do envio | Pior sync no meio | Sync do fim |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| fonte sem sync | antes | 13,6 · 20,6 · 10,9 | 7,6 · 12,7 · 5,6 | 42% · 34% · 11% | 2 s · 8 s · 0 s | 1 · 3 · 0 | 1,22 s · 2,85 s · 0,60 s | 0,72 s · 3,69 s · 0,05 s |
| fonte sem sync | depois | 19,0 · 21,3 · 11,4 | 5,8 · 15,3 · 5,2 | 3% · 49% · 1% | 0 s · 9 s · 0 s | 0 · 4 · 0 | — · — · — | 8,0 s · < 2 s · < 2 s |
| fonte no disco | antes | 18,5 · 14,1 · 12,2 | 12,7 · 8,7 · 6,8 | 41% · 28% · 13% | 8 s · 4 s · 1 s | 3 · 2 · 0 | 0,63 s · 1,63 s · 0,78 s | 0,54 s · 0,41 s · 0,15 s |
| fonte no disco | depois | 15,2 · 11,4 · 11,8 | 5,3 · 5,3 · 5,5 | 1% · 1% · 6% | 0 s · 0 s · 0 s | 0 · 0 · 0 | — · — · — | 5,9 s · < 2 s · < 2 s |

Na segunda rodada da primeira série, sem linha de teste, o disco não
acompanhou nenhuma das duas versões: o Windows segurou as escritas do
download enquanto ainda gravava o arquivo de origem.

## O que segura as escritas

Um programa à parte (fora do repositório) escreve 500 MB em `%TEMP%` a
12 MB/s, em pedaços de 64 KiB por um buffer de 1 MiB, como o download. A
cada 5 s, força a gravação de um dos jeitos abaixo, e no fim força com
FlushFileBuffers. Duas rodadas de cada.

| A cada 5 s | Pior escrita parada | Gravação no meio | Gravação do fim |
| --- | --- | --- | --- |
| FlushFileBuffers, na thread que escreve | 1,84 s · 6,33 s | 0,004 a 6,33 s | 0,06 s · 0,01 s |
| NtFlushBuffersFileEx só dos dados, na thread que escreve | 0,17 s · 0,14 s | 0,02 a 0,17 s | 0,03 s · 0,05 s |
| NtFlushBuffersFileEx só dos dados, em outra thread | 4,10 s · 2,67 s | 0,01 a 4,20 s | 0,04 s · 0,04 s |
| nada | 0,002 s · 0,001 s | — | 0,31 s · 0,36 s |
| nada, com FILE_FLAG_WRITE_THROUGH | 0,09 s · 0,03 s | — | 0,001 s · 0 s |

Antes desta série, 400 MB a 15 MB/s com o FlushFileBuffers em outra thread
seguraram uma escrita por 0,94 s durante uma gravação de 1,0 s: a escrita
espera mesmo com um handle à parte. Sem limite de velocidade, os 1000 MB
estavam gravados em 2,9 a 5,7 s pelo cache, com 1,5 a 1,7 s de gravação
no fim, e em 2,6 e 3,0 s com FILE_FLAG_WRITE_THROUGH, sem nada para gravar
no fim.

## Queda e retomada

`--queda --mb 300 --rede-ana atraso=20,banda=100,fila=100 --rede-beto atraso=20`:
o app do Beto caiu com 167 MiB no arquivo parcial. Ao abrir de novo, 41
pedaços (172 MB) conferiram com os hashes em 0,2 s, e vieram de novo
142,6 MB dos 314,6 MB (45%). O arquivo final saiu igual ao enviado.

No teste com pausa (`--mb 200` na mesma linha), a gravação do fim da
tentativa pausada levou 5,7 s, e a retomada esperou por ela.
