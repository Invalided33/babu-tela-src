# Reno, CUBIC e BBR no SCTP do app

Medido em 25/11/2026, com duas cópias do app (perfil `medir`) no mesmo PC, uma
linha de teste nos sockets de cada uma (`voice/netem.rs`) e o servidor de
teste na Cloudflare só para a sinalização. Cada cenário rodou três vezes com
cada controle, alternando os controles. Os números são a mediana das rodadas,
com o menor e o maior entre parênteses. Comandos em
[desenvolvimento.md](../../docs/desenvolvimento.md#comparar-controles-de-congestionamento).

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
padrão novo das chamadas, CUBIC, com a regra do RFC 9438 §5.8 para o tempo
sem dados; `slides-lento-reno` rodou ao lado deles, para comparar.

## Transferências (Ana envia, Beto baixa)
| cenário | cc | n | ok | MB/s dados | MB/s de ponta a ponta | srtt mediana | srtt p95 | reenvio % | timeouts |
|---|---|---|---|---|---|---|---|---|---|
| loop | reno | 3 | 3 | 108.8 (106.2–138.8) | 68.5 (67.5–74.1) | 22 (21–26) | 40 (38–41) | 0.00 (0.00–0.00) | 2 (1–2) |
| loop | cubic | 3 | 3 | 79.7 (66.2–85.1) | 53.8 (46.3–60.3) | 33 (26–40) | 40 (40–201) | 0.00 (0.00–0.00) | 3 (3–5) |
| loop | bbr | 3 | 3 | 138.7 (55.4–156.3) | 83.3 (41.3–83.7) | 8 (8–11) | 32 (31–32) | 0.00 (0.00–0.00) | 1 (1–4) |
| p01 | reno | 3 | 3 | 1.5 (1.5–1.5) | 1.4 (1.4–1.4) | 39 (39–39) | 40 (39–40) | 0.10 (0.10–0.21) | 0 (0–1) |
| p01 | cubic | 3 | 3 | 2.0 (1.8–2.4) | 1.8 (1.6–2.1) | 39 (39–39) | 39 (39–39) | 0.15 (0.10–0.65) | 1 (0–1) |
| p01 | bbr | 3 | 3 | 72.0 (68.7–72.7) | 16.7 (16.1–16.7) | 80 (75–85) | 80 (75–85) | 0.00 (0.00–0.00) | 0 (0–0) |
| p1 | reno | 3 | 3 | 0.2 (0.2–0.3) | 0.2 (0.2–0.2) | 80 (80–80) | 80 (80–80) | 1.83 (1.50–2.06) | 6 (4–7) |
| p1 | cubic | 3 | 3 | 0.4 (0.4–0.6) | 0.3 (0.3–0.4) | 80 (79–80) | 80 (80–80) | 1.52 (1.50–1.79) | 3 (1–4) |
| p1 | bbr | 3 | 3 | 7.6 (4.2–8.1) | 1.2 (0.9–1.3) | 79 (79–79) | 79 (79–79) | 0.01 (0.00–0.03) | 0 (0–1) |
| gargalo | reno | 3 | 3 | 9.8 (9.8–9.9) | 7.7 (6.7–7.8) | 43 (42–47) | 58 (57–60) | 1.06 (1.06–1.06) | 2 (2–2) |
| gargalo | cubic | 3 | 3 | 9.2 (9.1–9.7) | 7.3 (6.0–7.3) | 66 (65–67) | 106 (103–106) | 1.06 (1.05–1.06) | 2 (2–2) |
| gargalo | bbr | 3 | 3 | 11.5 (11.2–11.5) | 8.4 (7.2–8.7) | 81 (80–81) | 84 (84–85) | 0.00 (0.00–0.00) | 0 (0–0) |
| raso | reno | 3 | 3 | 7.0 (6.7–7.4) | 5.8 (5.6–5.9) | 39 (39–39) | 39 (39–39) | 0.06 (0.06–0.08) | 1 (1–2) |
| raso | cubic | 3 | 3 | 7.4 (7.3–8.4) | 6.1 (6.0–6.8) | 39 (39–39) | 39 (39–39) | 0.08 (0.01–0.09) | 1 (0–1) |
| raso | bbr | 3 | 3 | 11.3 (9.5–11.4) | 8.7 (7.5–8.7) | 39 (39–39) | 39 (39–64) | 3.85 (3.74–3.86) | 0 (0–2) |

## Chamada com um arquivo indo (voz que o Beto ouve da Ana, cujo upload leva o arquivo)
| cenário | cc | n | MB/s dados | voz p50 sem arquivo | voz p50 com | voz p95 com | voz máx com | voz perdida % com | secou | reenvio % arquivo |
|---|---|---|---|---|---|---|---|---|---|---|
| chamada | reno | 3 | 2.18 (2.18–2.18) | 142 (140–143) | 222 (221–223) | 296 (281–302) | 323 (301–332) | 0.2 (0.1–0.3) | 5 (5–6) | 2.13 (2.13–2.14) |
| chamada | cubic | 3 | 2.33 (2.33–2.33) | 138 (137–140) | 260 (258–277) | 340 (338–377) | 368 (350–387) | 0.3 (0.2–0.4) | 6 (6–7) | 2.92 (2.91–2.94) |
| chamada | bbr | 3 | 2.30 (2.30–2.31) | 141 (140–142) | 209 (201–210) | 241 (239–250) | 241 (240–260) | 0.0 (0.0–0.0) | 2 (2–2) | 0.00 (0.00–0.00) |
| chamada-raso | reno | 3 | 1.84 (1.75–1.86) | 139 (132–141) | 149 (142–150) | 169 (162–180) | 179 (172–190) | 0.1 (0.1–0.2) | 3 (2–4) | 0.40 (0.09–0.41) |
| chamada-raso | cubic | 3 | 2.03 (2.02–2.27) | 138 (134–140) | 148 (144–159) | 168 (154–180) | 188 (164–190) | 0.0 (0.0–0.1) | 2 (2–3) | 0.49 (0.07–0.51) |
| chamada-raso | bbr | 3 | 2.29 (2.17–2.30) | 137 (132–138) | 177 (162–178) | 187 (172–188) | 187 (172–188) | 2.5 (2.5–3.0) | 2 (2–3) | 1.96 (1.95–2.00) |
| chamada-lento | reno | 3 | 0.17 (0.16–0.18) | 139 (131–142) | 169 (161–171) | 182 (169–201) | 192 (179–221) | 2.9 (1.9–3.4) | 7 (6–9) | 2.09 (1.92–2.23) |
| chamada-lento | cubic | 3 | 0.19 (0.19–0.20) | 139 (138–141) | 168 (159–180) | 189 (188–191) | 189 (188–211) | 2.9 (2.9–3.3) | 5 (3–6) | 2.46 (2.03–2.49) |
| chamada-lento | bbr | 3 | 0.22 (0.22–0.22) | 137 (135–140) | 177 (175–180) | 195 (190–206) | 210 (207–215) | 10.8 (9.4–15.9) | 17 (14–27) | 2.60 (2.43–2.83) |

## Slides na chamada (pelo canal de dados da própria chamada)
| cenário | cc | n | ok | MB de figuras | segundos | MB/s | voz p50 sem | voz p50 com | voz p95 com | voz perdida % com | secou |
|---|---|---|---|---|---|---|---|---|---|---|---|
| slides-perda | reno | 3 | 3 | 15.5 (15.5–15.5) | 44.3 (43.8–49.9) | 0.35 (0.31–0.35) | 137 (136–141) | 137 (136–141) | 137 (136–141) | 0.9 (0.8–1.2) | 0 (0–0) |
| slides-perda | cubic | 3 | 3 | 15.5 (15.5–15.5) | 24.1 (22.9–26.9) | 0.64 (0.58–0.68) | 135 (132–147) | 135 (132–147) | 135 (132–147) | 0.9 (0.7–1.0) | 0 (0–1) |
| slides-perda | bbr | 3 | 3 | 15.5 (15.5–15.5) | 20.8 (19.4–21.8) | 0.75 (0.71–0.80) | 138 (137–145) | 139 (137–145) | 145 (139–147) | 1.0 (0.8–1.3) | 0 (0–1) |
| slides-gargalo | reno | 3 | 3 | 15.5 (15.5–15.5) | 23.5 (23.3–23.5) | 0.66 (0.66–0.67) | 137 (134–144) | 154 (154–157) | 164 (157–164) | 0.0 (0.0–0.0) | 1 (1–1) |
| slides-gargalo | cubic | 3 | 3 | 15.5 (15.5–15.5) | 23.6 (23.5–23.6) | 0.66 (0.66–0.66) | 143 (132–144) | 152 (152–164) | 163 (152–164) | 0.0 (0.0–0.0) | 1 (1–1) |
| slides-gargalo | bbr | 3 | 3 | 15.5 (15.5–15.5) | 23.6 (23.5–23.7) | 0.66 (0.65–0.66) | 135 (133–136) | 155 (153–156) | 165 (163–166) | 0.0 (0.0–0.0) | 1 (1–1) |
| slides-lento | reno | 3 | 3 | 6.2 (6.2–6.2) | 38.6 (37.5–39.3) | 0.16 (0.16–0.17) | 139 (134–143) | 169 (154–173) | 173 (170–174) | 1.3 (1.1–2.3) | 4 (4–5) |
| slides-lento | cubic | 3 | 3 | 6.2 (6.2–6.2) | 32.3 (31.9–32.7) | 0.19 (0.19–0.19) | 144 (142–148) | 172 (164–178) | 182 (178–183) | 1.9 (1.6–2.0) | 3 (3–4) |
| slides-lento | bbr | 3 | 3 | 6.2 (6.2–6.2) | 29.7 (29.7–29.8) | 0.21 (0.21–0.21) | 138 (131–141) | 171 (168–171) | 211 (188–216) | 17.9 (14.9–20.2) | 60 (39–63) |
| slides-perda-padrao | padrao | 4 | 4 | 15.5 (15.5–15.5) | 27.1 (23.7–33.6) | 0.58 (0.46–0.66) | 138 (134–141) | 138 (133–141) | 138 (133–141) | 1.0 (0.8–1.2) | 0 (0–1) |
| slides-lento-padrao | padrao | 4 | 4 | 6.2 (6.2–6.2) | 32.4 (32.2–33.3) | 0.19 (0.19–0.19) | 138 (133–145) | 165 (159–175) | 182 (163–187) | 1.9 (1.7–2.9) | 3 (2–5) |
| slides-lento-reno | reno | 3 | 3 | 6.2 (6.2–6.2) | 38.2 (38.2–38.3) | 0.16 (0.16–0.16) | 139 (135–142) | 165 (162–169) | 179 (175–191) | 1.5 (1.3–1.6) | 4 (3–7) |
