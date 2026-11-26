# Reno, CUBIC e BBR no SCTP do app

Medido em 25 e 26/11/2026, com duas cópias do app (perfil `medir`) no mesmo
PC, uma linha de teste nos sockets de cada uma (`voice/netem.rs`) e o
servidor de teste na Cloudflare só para a sinalização. Cada cenário rodou
três vezes com cada controle, alternando os controles. Os números são a
mediana das rodadas, com o menor e o maior entre parênteses. Comandos em
[desenvolvimento.md](../../docs/desenvolvimento.md#comparar-controles-de-congestionamento).

- **bbr1**: o BBR v1 que o app usava nos arquivos antes.
- **bbr3**: o BBR v3 (draft-ietf-ccwg-bbr-06) que o substituiu, forçado
  com `--cc bbr` (também no canal da chamada, nos cenários de slides).
  Reno e CUBIC não mudaram com ele.
- **ab-\***: o BBR v3 sondando como o draft (`bbr3draft`, a cada tantas
  rodadas quantos pacotes tem a janela) e como o app (`bbr3`, de 2 a 3 s
  quando a janela tem menos de 63 pacotes), três rodadas alternadas.
- **final-\***: uma rodada do app como fica, com os padrões (BBR v3 nos
  arquivos, CUBIC nas chamadas), depois das últimas correções do BBR v3.
- **MB/s dados**: do primeiro ao último byte no Beto, em MB de 10^6 bytes.
- **MB/s de ponta a ponta**: do "Baixar" ao fim, com a conexão e o fim, em MiB/s.
- **srtt**: o tempo de ida e volta que o SCTP de quem envia mediu, em ms.
- **reenvio %**: pacotes de dados reenviados, sobre os do arquivo.
- **voz**: o que o Beto ouve da Ana, cujo upload leva o arquivo ou os slides.
  Boca a ouvido em ms; perdida é a porcentagem de pacotes que não chegou;
  "secou" conta as vezes em que o som acabou antes do próximo pacote.

O cenário `p1` rodou depois da correção do fim do envio: antes dela, o Reno
não terminava os 8 MB (quem enviava fechava a conexão 30 s depois de passar
o último bloco, com dados ainda na fila). Os cenários `*-padrao` usam o
padrão das chamadas, CUBIC, com a regra do RFC 9438 §5.8 para o tempo sem
dados; `slides-lento-reno` rodou ao lado deles, para comparar. Numa das três
rodadas de `gargalo` com o BBR v3, o disco do Beto parou a gravação por uns
7 s; a janela do SCTP dele foi a zero, e a rodada fez 8,1 MB/s. Em
`final-gargalo`, o download esperou o disco 27% do tempo: a rede andou a
11,8 MB/s, e a tabela mostra 9,7.

## Transferências (Ana envia, Beto baixa)
| cenário | cc | n | ok | MB/s dados | MB/s de ponta a ponta | srtt mediana | srtt p95 | reenvio % | timeouts |
|---|---|---|---|---|---|---|---|---|---|
| loop | reno | 3 | 3 | 108.8 (106.2–138.8) | 68.5 (67.5–74.1) | 22 (21–26) | 40 (38–41) | 0.00 (0.00–0.00) | 2 (1–2) |
| loop | cubic | 3 | 3 | 79.7 (66.2–85.1) | 53.8 (46.3–60.3) | 33 (26–40) | 40 (40–201) | 0.00 (0.00–0.00) | 3 (3–5) |
| loop | bbr1 | 3 | 3 | 138.7 (55.4–156.3) | 83.3 (41.3–83.7) | 8 (8–11) | 32 (31–32) | 0.00 (0.00–0.00) | 1 (1–4) |
| loop | bbr3 | 3 | 3 | 139.2 (111.2–148.4) | 76.9 (64.9–80.6) | 11 (10–11) | 35 (33–38) | 0.00 (0.00–0.00) | 1 (0–2) |
| p01 | reno | 3 | 3 | 1.5 (1.5–1.5) | 1.4 (1.4–1.4) | 39 (39–39) | 40 (39–40) | 0.10 (0.10–0.21) | 0 (0–1) |
| p01 | cubic | 3 | 3 | 2.0 (1.8–2.4) | 1.8 (1.6–2.1) | 39 (39–39) | 39 (39–39) | 0.15 (0.10–0.65) | 1 (0–1) |
| p01 | bbr1 | 3 | 3 | 72.0 (68.7–72.7) | 16.7 (16.1–16.7) | 80 (75–85) | 80 (75–85) | 0.00 (0.00–0.00) | 0 (0–0) |
| p01 | bbr3 | 3 | 3 | 71.8 (70.2–71.9) | 15.4 (14.9–16.9) | 86 (74–100) | 86 (74–100) | 0.00 (0.00–0.00) | 0 (0–0) |
| p1 | reno | 3 | 3 | 0.2 (0.2–0.3) | 0.2 (0.2–0.2) | 80 (80–80) | 80 (80–80) | 1.83 (1.50–2.06) | 6 (4–7) |
| p1 | cubic | 3 | 3 | 0.4 (0.4–0.6) | 0.3 (0.3–0.4) | 80 (79–80) | 80 (80–80) | 1.52 (1.50–1.79) | 3 (1–4) |
| p1 | bbr1 | 3 | 3 | 7.6 (4.2–8.1) | 1.2 (0.9–1.3) | 79 (79–79) | 79 (79–79) | 0.01 (0.00–0.03) | 0 (0–1) |
| p1 | bbr3 | 3 | 3 | 7.6 (7.5–7.8) | 1.2 (1.0–1.2) | 79 (79–79) | 79 (79–79) | 0.00 (0.00–0.00) | 0 (0–0) |
| gargalo | reno | 3 | 3 | 9.8 (9.8–9.9) | 7.7 (6.7–7.8) | 43 (42–47) | 58 (57–60) | 1.06 (1.06–1.06) | 2 (2–2) |
| gargalo | cubic | 3 | 3 | 9.2 (9.1–9.7) | 7.3 (6.0–7.3) | 66 (65–67) | 106 (103–106) | 1.06 (1.05–1.06) | 2 (2–2) |
| gargalo | bbr1 | 3 | 3 | 11.5 (11.2–11.5) | 8.4 (7.2–8.7) | 81 (80–81) | 84 (84–85) | 0.00 (0.00–0.00) | 0 (0–0) |
| gargalo | bbr3 | 3 | 3 | 11.4 (8.1–11.4) | 8.1 (6.3–8.7) | 55 (53–58) | 74 (70–81) | 0.00 (0.00–0.00) | 0 (0–3) |
| raso | reno | 3 | 3 | 7.0 (6.7–7.4) | 5.8 (5.6–5.9) | 39 (39–39) | 39 (39–39) | 0.06 (0.06–0.08) | 1 (1–2) |
| raso | cubic | 3 | 3 | 7.4 (7.3–8.4) | 6.1 (6.0–6.8) | 39 (39–39) | 39 (39–39) | 0.08 (0.01–0.09) | 1 (0–1) |
| raso | bbr1 | 3 | 3 | 11.3 (9.5–11.4) | 8.7 (7.5–8.7) | 39 (39–39) | 39 (39–64) | 3.85 (3.74–3.86) | 0 (0–2) |
| raso | bbr3 | 3 | 3 | 10.7 (10.7–10.8) | 8.3 (8.3–8.4) | 39 (39–39) | 39 (39–39) | 0.11 (0.09–0.12) | 0 (0–0) |
| final-loop | padrao | 1 | 1 | 144.4 | 86.2 | 10 | 34 | 0.00 | 0 |
| final-p01 | padrao | 3 | 3 | 70.6 (61.7–72.8) | 16.3 (15.9–16.9) | 73 (47–130) | 73 (47–130) | 0.00 (0.00–0.00) | 0 (0–0) |
| final-p1 | padrao | 1 | 1 | 8.0 | 0.5 | 79 | 79 | 0.00 | 0 |
| final-gargalo | padrao | 1 | 1 | 9.7 | 7.7 | 53 | 75 | 0.00 | 0 |
| final-raso | padrao | 1 | 1 | 10.7 | 8.3 | 39 | 39 | 0.08 | 0 |

## Chamada com um arquivo indo (voz que o Beto ouve da Ana, cujo upload leva o arquivo)
| cenário | cc | n | MB/s dados | voz p50 sem arquivo | voz p50 com | voz p95 com | voz máx com | voz perdida % com | secou | reenvio % arquivo |
|---|---|---|---|---|---|---|---|---|---|---|
| chamada | reno | 3 | 2.18 (2.18–2.18) | 142 (140–143) | 222 (221–223) | 296 (281–302) | 323 (301–332) | 0.2 (0.1–0.3) | 5 (5–6) | 2.13 (2.13–2.14) |
| chamada | cubic | 3 | 2.33 (2.33–2.33) | 138 (137–140) | 260 (258–277) | 340 (338–377) | 368 (350–387) | 0.3 (0.2–0.4) | 6 (6–7) | 2.92 (2.91–2.94) |
| chamada | bbr1 | 3 | 2.30 (2.30–2.31) | 141 (140–142) | 209 (201–210) | 241 (239–250) | 241 (240–260) | 0.0 (0.0–0.0) | 2 (2–2) | 0.00 (0.00–0.00) |
| chamada | bbr3 | 3 | 2.29 (2.29–2.29) | 136 (131–137) | 206 (201–227) | 280 (266–307) | 306 (301–327) | 0.0 (0.0–0.0) | 5 (4–5) | 0.00 (0.00–0.00) |
| chamada-raso | reno | 3 | 1.84 (1.75–1.86) | 139 (132–141) | 149 (142–150) | 169 (162–180) | 179 (172–190) | 0.1 (0.1–0.2) | 3 (2–4) | 0.40 (0.09–0.41) |
| chamada-raso | cubic | 3 | 2.03 (2.02–2.27) | 138 (134–140) | 148 (144–159) | 168 (154–180) | 188 (164–190) | 0.0 (0.0–0.1) | 2 (2–3) | 0.49 (0.07–0.51) |
| chamada-raso | bbr1 | 3 | 2.29 (2.17–2.30) | 137 (132–138) | 177 (162–178) | 187 (172–188) | 187 (172–188) | 2.5 (2.5–3.0) | 2 (2–3) | 1.96 (1.95–2.00) |
| chamada-raso | bbr3 | 3 | 2.24 (2.23–2.26) | 134 (132–135) | 152 (144–155) | 172 (154–185) | 182 (164–215) | 0.1 (0.0–0.2) | 3 (2–3) | 0.23 (0.21–0.23) |
| chamada-lento | reno | 3 | 0.17 (0.16–0.18) | 139 (131–142) | 169 (161–171) | 182 (169–201) | 192 (179–221) | 2.9 (1.9–3.4) | 7 (6–9) | 2.09 (1.92–2.23) |
| chamada-lento | cubic | 3 | 0.19 (0.19–0.20) | 139 (138–141) | 168 (159–180) | 189 (188–191) | 189 (188–211) | 2.9 (2.9–3.3) | 5 (3–6) | 2.46 (2.03–2.49) |
| chamada-lento | bbr1 | 3 | 0.22 (0.22–0.22) | 137 (135–140) | 177 (175–180) | 195 (190–206) | 210 (207–215) | 10.8 (9.4–15.9) | 17 (14–27) | 2.60 (2.43–2.83) |
| chamada-lento | bbr3 | 3 | 0.22 (0.22–0.22) | 139 (136–143) | 158 (155–162) | 179 (165–182) | 188 (175–192) | 1.5 (1.1–1.9) | 4 (2–6) | 0.57 (0.49–0.69) |
| ab-chamada-lento | bbr3draft | 3 | 0.22 (0.22–0.22) | 140 (132–141) | 172 (170–181) | 200 (172–206) | 217 (182–218) | 4.1 (3.8–4.2) | 7 (6–9) | 2.86 (2.63–2.98) |
| ab-chamada-lento | bbr3 | 3 | 0.22 (0.21–0.22) | 147 (140–148) | 166 (160–168) | 188 (170–196) | 188 (170–196) | 0.8 (0.5–1.9) | 2 (2–6) | 0.57 (0.29–0.57) |
| final-chamada | padrao | 1 | 2.29 | 139 | 209 | 269 | 319 | 0.0 | 4 | 0.00 |
| final-chamada-raso | padrao | 1 | 2.25 | 138 | 148 | 178 | 188 | 0.1 | 4 | 0.21 |
| final-chamada-lento | padrao | 1 | 0.22 | 137 | 157 | 177 | 187 | 1.6 | 4 | 0.46 |

## Slides na chamada (pelo canal de dados da própria chamada)
| cenário | cc | n | ok | MB de figuras | segundos | MB/s | voz p50 sem | voz p50 com | voz p95 com | voz perdida % com | secou |
|---|---|---|---|---|---|---|---|---|---|---|---|
| slides-perda | reno | 3 | 3 | 15.5 (15.5–15.5) | 44.3 (43.8–49.9) | 0.35 (0.31–0.35) | 137 (136–141) | 137 (136–141) | 137 (136–141) | 0.9 (0.8–1.2) | 0 (0–0) |
| slides-perda | cubic | 3 | 3 | 15.5 (15.5–15.5) | 24.1 (22.9–26.9) | 0.64 (0.58–0.68) | 135 (132–147) | 135 (132–147) | 135 (132–147) | 0.9 (0.7–1.0) | 0 (0–1) |
| slides-perda | bbr1 | 3 | 3 | 15.5 (15.5–15.5) | 20.8 (19.4–21.8) | 0.75 (0.71–0.80) | 138 (137–145) | 139 (137–145) | 145 (139–147) | 1.0 (0.8–1.3) | 0 (0–1) |
| slides-perda | bbr3 | 3 | 3 | 15.5 (15.5–15.5) | 22.9 (19.5–30.9) | 0.68 (0.50–0.80) | 137 (133–145) | 137 (133–145) | 145 (133–147) | 1.3 (0.6–1.3) | 0 (0–1) |
| slides-gargalo | reno | 3 | 3 | 15.5 (15.5–15.5) | 23.5 (23.3–23.5) | 0.66 (0.66–0.67) | 137 (134–144) | 154 (154–157) | 164 (157–164) | 0.0 (0.0–0.0) | 1 (1–1) |
| slides-gargalo | cubic | 3 | 3 | 15.5 (15.5–15.5) | 23.6 (23.5–23.6) | 0.66 (0.66–0.66) | 143 (132–144) | 152 (152–164) | 163 (152–164) | 0.0 (0.0–0.0) | 1 (1–1) |
| slides-gargalo | bbr1 | 3 | 3 | 15.5 (15.5–15.5) | 23.6 (23.5–23.7) | 0.66 (0.65–0.66) | 135 (133–136) | 155 (153–156) | 165 (163–166) | 0.0 (0.0–0.0) | 1 (1–1) |
| slides-gargalo | bbr3 | 3 | 3 | 15.5 (15.5–15.5) | 23.5 (23.4–24.2) | 0.66 (0.64–0.66) | 134 (130–141) | 151 (141–154) | 164 (161–171) | 0.0 (0.0–0.0) | 1 (1–1) |
| slides-lento | reno | 3 | 3 | 6.2 (6.2–6.2) | 38.6 (37.5–39.3) | 0.16 (0.16–0.17) | 139 (134–143) | 169 (154–173) | 173 (170–174) | 1.3 (1.1–2.3) | 4 (4–5) |
| slides-lento | cubic | 3 | 3 | 6.2 (6.2–6.2) | 32.3 (31.9–32.7) | 0.19 (0.19–0.19) | 144 (142–148) | 172 (164–178) | 182 (178–183) | 1.9 (1.6–2.0) | 3 (3–4) |
| slides-lento | bbr1 | 3 | 3 | 6.2 (6.2–6.2) | 29.7 (29.7–29.8) | 0.21 (0.21–0.21) | 138 (131–141) | 171 (168–171) | 211 (188–216) | 17.9 (14.9–20.2) | 60 (39–63) |
| slides-lento | bbr3 | 3 | 3 | 6.2 (6.2–6.2) | 30.0 (29.8–30.1) | 0.21 (0.21–0.21) | 140 (132–143) | 160 (152–163) | 172 (170–183) | 1.3 (1.1–1.4) | 6 (5–6) |
| ab-slides-lento | bbr3draft | 3 | 3 | 6.2 (6.2–6.2) | 29.8 (29.7–29.9) | 0.21 (0.21–0.21) | 148 (134–149) | 177 (174–179) | 187 (184–189) | 4.5 (4.4–6.7) | 7 (6–13) |
| ab-slides-lento | bbr3 | 3 | 3 | 6.2 (6.2–6.2) | 30.1 (29.9–30.1) | 0.21 (0.21–0.21) | 140 (134–146) | 160 (154–165) | 176 (174–180) | 1.1 (0.9–1.6) | 6 (4–7) |
| slides-perda-padrao | padrao | 4 | 4 | 15.5 (15.5–15.5) | 27.1 (23.7–33.6) | 0.58 (0.46–0.66) | 138 (134–141) | 138 (133–141) | 138 (133–141) | 1.0 (0.8–1.2) | 0 (0–1) |
| slides-lento-padrao | padrao | 4 | 4 | 6.2 (6.2–6.2) | 32.4 (32.2–33.3) | 0.19 (0.19–0.19) | 138 (133–145) | 165 (159–175) | 182 (163–187) | 1.9 (1.7–2.9) | 3 (2–5) |
| slides-lento-reno | reno | 3 | 3 | 6.2 (6.2–6.2) | 38.2 (38.2–38.3) | 0.16 (0.16–0.16) | 139 (135–142) | 165 (162–169) | 179 (175–191) | 1.5 (1.3–1.6) | 4 (3–7) |
