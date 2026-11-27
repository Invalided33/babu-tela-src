# Envio direto pelo relé, sem o teto de 5,5 MB/s

Medido em 26/11/2026 com `cargo xtask direto --so-rele` (TURN da Cloudflare
por UDP) e `--so-tcp` (por TCP/TLS), `--sem-pausa`, duas cópias do app
(perfil `medir`) no mesmo PC. Os dois lados passam pelo relé, cada um com a
sua alocação. **com teto**: o envio limitado a 5,5 MB/s em mensagens de
16 KiB, como até o commit dfa5277. **sem teto**: só o controle de
congestionamento (BBRv3) decide a velocidade.

**Total** vai de "Baixar" até o download pronto, com a conexão. **Dados** vai
do primeiro ao último byte no Beto, em cada tentativa. **Reenviado** é a
parte do arquivo que foi de novo.

| Relé | Versão | MB | Total (s) | Dados (s) | MB/s nos dados | Reenviado |
| --- | --- | --- | --- | --- | --- | --- |
| UDP | com teto | 200 | 48,2 | 38,1 | 5,50 | 0,00% |
| UDP | sem teto | 200 | 18,0 | 7,6 | 27,76 | 1,06% |
| UDP | sem teto | 1000 | 64,5 | 54,0 | 19,41 | 4,03% |
| UDP | sem teto | 1000 | 102,3 | 4,5 e 46,5 | 27,95 e 19,95 | 3,61% |
| UDP | sem teto | 1000 | 64,7 | 54,6 | 19,22 | 2,72% |
| TCP/TLS | com teto | 200 | 47,4 | 38,1 | 5,50 | 0,08% |
| TCP/TLS | sem teto | 1000 | 65,9 | 54,7 | 19,17 | 2,18% |
| TCP/TLS | sem teto | 1000 | 66,1 | 54,6 | 19,19 | 2,80% |
| TCP/TLS | sem teto | 1000 | 68,5 | 58,4 | 17,96 | 0,45% |

Com o teto, 1000 MB levariam uns 191 s só de dados. Sem ele, o envio por
UDP passa os primeiros 4 a 6 s a cerca de 34 MB/s, com poucas perdas.
Depois a Cloudflare descarta de 4 a 9% dos pacotes, e o BBRv3 fica em 17 a
19 MB/s até o fim. Pelo TCP/TLS, as perdas vêm desde o começo, de 1 a 5%, e
a velocidade cai de 32 para 17 a 18 MB/s em uns 12 s. Nenhuma rodada teve
timeout.

Na segunda rodada de 1000 MB por UDP, a alocação parou de entregar qualquer
coisa depois de 132,7 MB, sem perda antes. O Beto viu a conexão cair 29 s
depois, e a tentativa seguinte começou 3 s mais tarde, por outra alocação,
e continuou do último pedaço conferido até o fim. As outras sete
tentativas foram até o fim sem isso.
