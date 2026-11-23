# Desenvolvimento e testes

Execute os comandos em PowerShell a partir da raiz do repositório, com as ferramentas de compilação do Windows no `PATH`. A suíte do Worker usa um workspace separado. [Pré-requisitos e configuração](../README.md)

## Testes locais

```powershell
cargo test --locked --workspace
cargo test --locked --manifest-path crates/worker/Cargo.toml
```

Esses comandos executam os testes Rust e dispensam credenciais da Cloudflare. Para investigar uma área específica do cliente, filtre os testes pelo caminho do módulo.

```powershell
cargo test --locked -p babutela lang::tests
cargo test --locked -p babutela music::
```

Os testes de música cobrem a ordem dos links colados, a mudança de DJ, o instante em que o relógio da faixa começa a contar e o volume recebido por quem entra depois. Um servidor HTTP local simula a origem do áudio para testar respostas parciais, interrupções e reprodução antes do fim do download. [Testes do módulo de música](../crates/app/src/music/)

Alguns testes opcionais dependem de dispositivos de áudio ou vídeo, de dados externos ou de uma medição específica. Para o teste do seletor de emoji, indique uma pasta com `emoji-test.txt`, `annotations.json` e `annotationsDerived.json`. Para o teste da supressão de ruído, forneça pares `NOME_noisy.f32` e `NOME_clean.f32` de áudio mono a 48 kHz em float32 cru. Substitua as pastas dos exemplos pelas que contêm os dados. [Dados de avaliação do DPDFNet](https://huggingface.co/datasets/Ceva-IP/DPDFNet_EvalSet)

```powershell
$env:BABUTELA_EMOJI_DADOS = 'C:\dados\emoji'
cargo test -p babutela the_real_lists -- --ignored --nocapture
$env:BABUTELA_RUIDO_AVAL = 'C:\dados\fala'
cargo test -p babutela real_noisy_speech -- --ignored --nocapture
```

## Ponta a ponta

`cargo xtask testar` executa os testes dos dois workspaces e depois exercita a API e o WebSocket de um Worker publicado. Cada rodada usa um Durable Object de teste separado, identificado por um nome novo e pelo segredo `TEST_HUB`. A suíte cobre autenticação, permissões, criptografia, amizades, denúncias, sinalização de chamadas e arquivos diretos. Com R2 ativo, cobre também multipartes, cancelamento e leitura por intervalo. [Implementação dos testes de ponta a ponta](../crates/xtask/src/e2e.rs)

Com `build/cloudflare.json` configurado para o Worker, use um dos comandos abaixo. Para `testar`, também é possível fornecer o segredo em `BABUTELA_TEST_HUB` e o endereço em `BABUTELA_URL` ou como argumento HTTP(S). Os testes `chamada`, `direto` e `medir` leem `endereco` e `segredos.TEST_HUB` do arquivo de configuração.

| Comando | O que exercita |
| --- | --- |
| `cargo xtask testar --so-servidor` | Executa apenas a etapa de ponta a ponta contra o Worker |
| `cargo xtask chamada --pessoas 3 --tela --anotar` | O áudio de teste chega aos demais apps e os participantes recebem os traços sobre a tela, inclusive quem entra depois |
| `cargo xtask chamada --pessoas 2 --calado --so-tcp` | Uma pessoa permanece em silêncio e o áudio da outra passa pelo relé TCP/TLS |
| `cargo xtask direto --mb 200` | O segundo app baixa um arquivo do primeiro, pausa, retoma e compara o resultado byte a byte |
| `cargo xtask medir arquivos` | Mede upload e download via servidor com 64 MiB e compara o arquivo enviado pelo próprio app com a cópia baixada e decifrada por ele |

`chamada` abre de duas a seis cópias do cliente na área de trabalho. Aceita `--segundos` de 10 a 90 (padrão 20), `--janela "Título"`, `--placa` para testar a captura pela GPU, ou `--apresentar "C:\dados\slides.pptx"` para apresentar um PPTX ou PDF. `--anotar` requer pelo menos três participantes e uma tela, janela ou apresentação. `direto` também aceita `--so-tcp`. Os cenários com relé exigem TURN e consomem tráfego do serviço. Nos dois comandos e em `medir`, `--exe target\release\babutela.exe` usa um cliente previamente compilado. [Chamada](../crates/xtask/src/call.rs) · [Transferência direta](../crates/xtask/src/direto.rs)

## Medições

Sem argumentos, `cargo xtask medir` executa `mensagens`, `ocioso`, `figurinhas`, `voz`, `tela`, `midia` e `arquivos`. Também aceita vários cenários por nome. O perfil `medir` gera um app otimizado com compilação incremental. Os cenários adicionais `captura` e `qualidades` comparam caminhos de captura e resoluções na mesma chamada. [Cenários de medição](../crates/xtask/src/medir.rs)

```powershell
cargo xtask medir voz --segundos 30 --ruido ia --rotulo voz-ia
cargo xtask medir voz --segundos 30 --ruido padrao --rotulo voz-padrao --comparar voz-ia
```

O comando salva o JSON em `build/medidas/` com o nome indicado em `--rotulo`. A opção `--segundos` aceita de 5 a 120 (padrão 20). Use `--env NOME=VALOR` para passar uma variável às cópias do app, como `--env BABUTELA_TESTE_MIDIA=processador`. Os valores de `--ruido` são `ia`, `padrao` e `desligada`. O cenário de figurinhas usa um serviço simulado local. Com `--giphy`, consulta o GIPHY usando `segredos.GIPHY_API_KEY` e salva uma imagem em `build/giphy/`.

As estatísticas combinam tempos instrumentados no cliente com amostras dos processos Windows. Compare rodadas no mesmo PC e com as mesmas janelas, rede e carga de fundo. Os relatórios antigos que mediam CPU por tempo de execução ou exibiam mensagens em texto claro têm condições diferentes das atuais. [Instrumentação do cliente](../crates/app/src/probe.rs) · [Relatórios versionados](../build/medidas/)

## Teste da interface

O modo `--smoke-report` abre uma janela, executa os passos de `PROSA_SMOKE_ACTIONS`, grava o estado observado em JSON e encerra. Quando a captura funciona, o PNG recebe o mesmo nome-base do relatório. No exemplo, use o app de debug já compilado em uma sessão PowerShell de teste.

```powershell
$env:PROSA_USERDATA = Join-Path $env:TEMP 'babutela-perfil-teste'
$env:PROSA_SMOKE_ACTIONS = 'wait:500'
& .\target\debug\babutela.exe "--smoke-report=$env:TEMP\babutela-teste.json"
```

Para testar uma conversa, use a sessão de teste em `PROSA_SMOKE_TOKEN` e indique `<segredo>:<nome>` em `BABUTELA_TEST_HUB` para o cliente. O cenário precisa de um servidor e um canal de voz se os passos usarem `go:first-server|join`. Os comandos se encadeiam com `|`.

| Área | Exemplos de passos |
| --- | --- |
| Interface | `hover:engrenagem:geral`, `click:400:300`, `keys:texto`, `enter`, `open:licencas` |
| Emoji | `pop:emoji`, `wait-emoji:30`, `emoji-search:coração`, `emoji-tone:2`, `pop:react` |
| Chamada e slides | `tela-cheia:on`, `apresentar:C:\dados\aula.pdf`, `slide:2`, `tecla:PageDown`, `apresentar:fim` |
| Anotações | `anotar:ana`, `ferramenta:rect`, `cor:3`, `arrastar:anotacao`, `desfazer`, `limpar:meus` |
| Arquivos diretos | `direto:on`, `wait-direto-msg:60`, `baixar:C:\dados`, `pausar-direto`, `continuar-direto`, `wait-direto:60` |
| Música | `open:music`, `musica:<link>`, `wait-musica:60`, `volume-todos:65`, `volume-meu:120` |

O relatório inclui o resultado das ações, campos de interface, transferências em `direct` e a fila, os volumes e o nível de áudio em `music`. Para serviços de teste, `BABUTELA_EMOJI_URL`, `BABUTELA_GIPHY_URL` e `BABUTELA_GOOGLE_AUTH_URL` substituem os respectivos endereços. O teste de atualização do instalador utiliza `BABUTELA_SETUP_TEST` para isolar arquivos e registro do Windows. A lista implementada de ações e de campos está no cliente. [Modo de teste](../crates/app/src/smoke.rs) · [Convenções do instalador](../crates/buildinfo/src/lib.rs)
