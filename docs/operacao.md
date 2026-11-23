# Operação do Babu Tela

Para abrir o app de desenvolvimento com o servidor do projeto, siga as instruções do [README](../README.md#o-que-você-precisa). Para manter uma instância própria ou distribuir uma versão, execute os comandos abaixo a partir da raiz do repositório.

## Configurar uma instância

Crie `build/cloudflare.json` no computador que fará o deploy. Substitua os valores de exemplo e guarde uma cópia privada. O cliente instalado recebe o endereço do servidor. As credenciais administrativas ficam no computador de publicação.

```json
{
  "contaId": "<id da conta Cloudflare>",
  "apiToken": "<token de API da Cloudflare>",
  "worker": "babutela",
  "endereco": "https://babutela.seu-subdominio.workers.dev",
  "bucket": "babutela",
  "armazenamentoGb": 10,
  "r2AccessKeyId": "",
  "r2SecretAccessKey": "",
  "pagina": "",
  "segredos": {
    "PEPPER": "<chave aleatória privada>",
    "GOOGLE_CLIENT_ID": "<id do cliente OAuth de desktop>",
    "GOOGLE_CLIENT_SECRET": "<segredo do cliente OAuth>",
    "TEST_HUB": "",
    "TURN_KEY_ID": "",
    "TURN_KEY_TOKEN": "",
    "GIPHY_API_KEY": ""
  }
}
```

1. **Worker.** Indique a conta, o nome do Worker e a URL HTTPS que ele atenderá. Conceda ao `apiToken` acesso para editar Workers e para ler e gravar no R2 quando usar o bucket. Em `PEPPER`, guarde um valor aleatório próprio da instância. O Worker o usa para autenticar operações de arquivos e o registro de versões.
2. **Login.** No projeto do Google Cloud, configure o nome e o público da tela de consentimento e crie um cliente OAuth do tipo **Desktop app**. Preencha `GOOGLE_CLIENT_ID` e `GOOGLE_CLIENT_SECRET`. O app usa os escopos `openid`, `email` e `profile`. Enquanto o projeto externo estiver em teste, cadastre os usuários autorizados no Google. Para acesso geral, publique o app OAuth. O comando de deploy exige os dois campos e o `PEPPER`. [Google Cloud Console](https://console.cloud.google.com/)
3. **Arquivos e atualizações.** Para armazenar anexos, listas de emoji e instaladores, ative o R2 na conta e defina `bucket`. O deploy cria o bucket informado se necessário. Uma instância sem o vínculo ao R2 ainda serve o chat de texto. `armazenamentoGb` limita a soma dos anexos contabilizados pelo app, em GB decimais. O padrão é 10 e o valor 0 desativa o teto. A cobrança da conta depende também dos demais recursos utilizados.
4. **Acesso ao R2 pelo publicador.** Preencha `r2AccessKeyId` e `r2SecretAccessKey` em conjunto ou deixe ambos vazios. No segundo caso, o `xtask` consulta o ID do `apiToken` e deriva dele a chave usada para assinar as requisições ao R2. Esse acesso é usado por `publicar`, `emoji` e `release update`.
5. **Testes.** Para executar os testes de ponta a ponta, chamadas, transferências e medições, crie outro segredo aleatório em `TEST_HUB`. Ele permite que o Worker encaminhe cada execução para um banco de teste isolado com contas próprias.
6. **Relé das chamadas.** Preencha `TURN_KEY_ID` e `TURN_KEY_TOKEN` com as credenciais TURN da Cloudflare para atender redes em que a conexão direta falha. Para usar seu próprio TURN, o Worker aceita `TURN_URLS` (URLs separadas por vírgula) e `TURN_SECRET` em `segredos`. Com TURN configurado, `TURN_RELAY_ONLY` igual a `"1"` solicita o uso exclusivo do relé. Com apenas STUN, a chamada depende da possibilidade de conexão direta.
7. **Figurinhas.** Em `GIPHY_API_KEY`, use a chave de um aplicativo do tipo **API**. O Worker a fornece aos clientes para que eles façam as buscas diretamente no GIPHY. Sem uma chave configurada no Worker, o app oculta o seletor. Para obter uma captura do seletor usando o serviço real, execute `cargo xtask medir figurinhas --giphy` e consulte `build/giphy/`. [Portal do GIPHY](https://developers.giphy.com/)
8. **Downloads.** Para distribuir seu instalador no GitHub, defina `pagina` como `dono/repositorio` de um repositório destinado à página de download. Com o campo vazio, o destino é `babu-tela` na conta autenticada no GitHub CLI.

### Primeiro deploy e atualizações do Worker

```powershell
cargo xtask servidor
```

O comando instala o alvo `wasm32-unknown-unknown` e a versão de `worker-build` esperada pelo projeto, compila `crates/worker`, configura o Durable Object SQLite e vincula o R2 quando disponível. Se o Worker ainda não existir, a ferramenta pede que você digite `criar` antes de criar o banco vazio. Ao fim, ela consulta `/api/version` no endereço configurado.

Para atualizar o código mantendo o banco, repita o comando sobre o mesmo Worker. Excluir o Worker no painel causa perda das contas e mensagens armazenadas em seu Durable Object. O deploy grava os segredos com valor preenchido e preserva os já publicados quando o campo local fica vazio. Para desativar uma integração, remova também o segredo correspondente no Worker. O deploy remove automaticamente as antigas chaves `REALTIME_APP_ID` e `REALTIME_APP_SECRET` do SFU.

## Gerar o instalador

1. No PC de cada assinante, execute `cargo xtask chave`. A chave privada fica em `%USERPROFILE%\.babutela\chave-atualizacoes.txt` ou no arquivo indicado por `BABUTELA_CHAVE_ATUALIZACOES`. O comando exibe a chave pública. Guarde uma cópia da chave privada fora desse computador. A perda das chaves exigidas para o quórum obriga os usuários a reinstalar o app manualmente para trocar de assinantes.
2. Execute `cargo xtask release` para compilar o app com o `endereco` da configuração. Para gerar um instalador local apontado para outra instância, use `cargo xtask release --endereco https://chat.exemplo.com`. Com essa opção, o comando obtém a URL pela linha de comando e dispensa o arquivo de credenciais da Cloudflare.
3. O resultado fica em `build/release/Babu Tela Setup.exe`. O arquivo `gerado.json` ao lado registra a versão, a URL e a data usadas pelo publicador. Para trabalhar apenas na interface do instalador, `cargo xtask release --so-instalador` reaproveita o app compactado de um build anterior, quando presente. Use o build completo ao trocar a URL, a versão ou os assinantes.

Para exigir mais de uma assinatura, crie `assinantes.txt` na raiz antes de compilar o app. Use uma linha `minimo = N` e uma chave pública hexadecimal por linha. Sem esse arquivo, o app confia na chave do PC de compilação. Cada assinante adicional confere o instalador, compara o SHA-256 com o próprio build e executa `cargo xtask assinar VERSÃO SHA256`. Quem publica reúne as linhas JSON recebidas em `build/assinaturas/<versão>.txt`.

## Distribuir uma versão

Para lançar uma nova versão do aplicativo, aumente `version` em `[workspace.package]` no [Cargo.toml](../Cargo.toml). Configure o Worker e o bucket de destino, prepare a assinatura e autentique o GitHub CLI na conta que tem acesso ao repositório de downloads.

```powershell
gh auth status
cargo xtask release update
```

Se necessário, use `gh auth login` para entrar. Antes de compilar, o comando de release verifica o acesso ao R2, as credenciais do Worker, a sessão do GitHub CLI e a versão que os apps já recebem. Também consulta se há uma revisão nova do modelo de supressão de ruído.

A publicação gera o instalador, atualiza o Worker e sincroniza a lista de emoji. Em seguida, `publicar` envia o instalador para o R2, registra a versão no servidor e disponibiliza o manifesto assinado em `/updates/latest.json`. Os clientes usam esse manifesto na atualização automática. Se a sincronização de emoji falhar, a publicação prossegue e é possível repetir `cargo xtask emoji`.

Por último, `pagina` anexa o instalador a um GitHub Release no repositório de **downloads**, preenche o [modelo HTML](../crates/xtask/pagina/index.html) com a versão, o tamanho, a data e o SHA-256 e publica no GitHub Pages. Na primeira execução, pode criar o repositório público e ativar o Pages. Se somente essa etapa falhar, repita `cargo xtask pagina` usando o instalador já gerado.
