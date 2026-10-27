//! `cargo xtask <comando>`: the project's tooling, in Rust like the rest of
//! it. The alias lives in .cargo/config.toml.

mod call;
mod cf;
mod direto;
mod e2e;
mod emoji;
mod giphy_fake;
mod icons;
mod medir;
mod modelo;
mod onnx;
mod publish;
mod release;
mod server;
mod signing;
mod sigv4;
mod site;
mod tools;

use std::process::ExitCode;

pub type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Returns early with an error message, formatted like `format!`.
macro_rules! fail {
    ($($arg:tt)*) => {
        return Err(format!($($arg)*).into())
    };
}
pub(crate) use fail;

const HELP: &str = "\
Uso: cargo xtask <comando>

  dev [argumentos]          abre o app em modo de desenvolvimento
  servidor                  compila o servidor e publica na Cloudflare
  release [--endereco URL]  gera build/release/Babu Tela Setup.exe
  release update            publica uma versão nova de uma vez: gera o
                            instalador, publica o servidor (com a lista de
                            emoji), a atualização automática dos apps e a
                            página de download (aumente a versão no
                            Cargo.toml antes)
  publicar                  envia esse instalador para a atualização automática,
                            assinado com a chave deste PC e as que vierem em
                            build/assinaturas/<versão>.txt, e o põe no
                            registro público
  chave                     cria (uma vez) a chave que assina as atualizações,
                            fora da Cloudflare, e mostra a parte pública
  assinar VERSÃO SHA256     a assinatura deste PC para um instalador, para
                            quem publica (com mais de um assinante,
                            assinantes.txt)
  pagina                    põe esse instalador na página de download
                            (GitHub Pages, pelo gh conectado neste PC)
  emoji                     copia a lista oficial de emoji (Unicode) e os
                            nomes em português (CLDR) para o servidor, de
                            onde os apps a leem
  modelo [--atualizar]      diz se a Ceva publicou um modelo novo da
                            supressão de ruído (DPDFNet, no Hugging Face);
                            --atualizar o traz para o app, depois de
                            conferir e testar
  testar [--so-servidor] [URL]
                            testes em Rust e, depois, de ponta a ponta contra o
                            servidor publicado (num banco de teste separado)
  chamada [--pessoas N] [--calado] [--so-tcp]
          [--tela | --janela TÍTULO] [--segundos N]
                            uma chamada de verdade entre 2 a 6 apps neste PC,
                            direto entre as pessoas (--calado: a última
                            fica em silêncio, e quase nada sai dela;
                            --so-tcp: só pelo relé TCP/TLS, como numa rede
                            sem UDP)
  direto [--mb N] [--so-tcp] [--exe CAMINHO]
                            um arquivo grande \"Enviar direto\" entre dois
                            apps neste PC, sem passar pelo servidor: o Beto
                            baixa, pausa na metade, continua e confere o
                            arquivo byte a byte (--so-tcp: pelo relé)
  medir [mensagens | ocioso | voz | tela | midia | arquivos]
        [--segundos N] [--rotulo NOME] [--comparar NOME] [--env NOME=VALOR]
        [--ruido ia|padrao|desligada]
                            mede atraso, processador e memória do app
                            otimizado neste PC, contra o servidor publicado,
                            e guarda os números em build/medidas/
  icones                    redesenha crates/app/assets/icons/*.svg

As chaves da Cloudflare ficam em build/cloudflare.json (veja o README).
";

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let command = args.next().unwrap_or_default();
    let rest: Vec<String> = args.collect();
    let outcome = match command.as_str() {
        "dev" => tools::dev(&rest),
        "servidor" => server::run(&rest),
        "release" => release::run(&rest),
        "publicar" => publish::run(&rest),
        "chave" => signing::run(&rest),
        "assinar" => signing::cosign(&rest),
        "pagina" => site::run(&rest),
        "emoji" => emoji::run(&rest),
        "modelo" => modelo::run(&rest),
        "testar" => e2e::run(&rest),
        "chamada" => call::run(&rest),
        "direto" => direto::run(&rest),
        "medir" => medir::run(&rest),
        "icones" => icons::run(&rest),
        "" | "ajuda" | "help" | "-h" | "--help" => {
            print!("{HELP}");
            Ok(())
        }
        other => Err(format!("comando desconhecido: {other}\n\n{HELP}").into()),
    };
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("\nerro: {e}");
            ExitCode::FAILURE
        }
    }
}
