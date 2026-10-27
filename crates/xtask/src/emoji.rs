//! `cargo xtask emoji`: copies the official emoji lists into the R2 bucket,
//! which the server offers at <endereco>/emoji/ (crates/worker/src/files.rs).
//! The apps read them there (crates/app/src/emoji.rs), so a person's PC
//! talks only to our server, and ask unicode.org and GitHub only while the
//! server has no copy. `release update` runs it too.
//!
//! The list comes from the address that always has Unicode's newest
//! version, so running this again after a new Unicode release (each
//! September) brings it to the apps; CLDR's names are pinned to one release
//! (babutela_buildinfo::emoji::CLDR).

use babutela_buildinfo::emoji::{SOURCES, Source, mirror};
use serde_json::Value;

use crate::tools::{self, block_on, mb};
use crate::{Result, cf, fail, sigv4};

pub fn run(args: &[String]) -> Result {
    if let Some(extra) = args.first() {
        fail!("opção desconhecida: {extra}");
    }
    let cfg = cf::load()?;
    block_on(async {
        let http = tools::http()?;
        let keys = sigv4::keys(&http, &cfg).await?;
        sync(&http, &cfg, &keys).await
    })?
}

/// Each file from where it is published, checked, and sent to the bucket
/// when the server's copy is not the same; then read back from the server.
pub async fn sync(http: &reqwest::Client, cfg: &cf::Config, keys: &sigv4::Keys) -> Result {
    for src in SOURCES {
        let bytes = download(http, &src).await?;
        let what = check(src.file, std::str::from_utf8(&bytes)?)?;
        let url = mirror(&cfg.endereco, src.file);
        if served(http, &url).await?.as_deref() == Some(&bytes[..]) {
            println!("==> {}: o servidor já tem este ({what})", src.file);
            continue;
        }
        println!(
            "==> Enviando {} ({}, {what})",
            src.file,
            mb(bytes.len() as u64)
        );
        let kind = if src.file.ends_with(".json") {
            "application/json"
        } else {
            "text/plain; charset=utf-8"
        };
        sigv4::put(
            http,
            cfg,
            keys,
            &format!("emoji/{}", src.file),
            bytes.clone(),
            kind,
        )
        .await?;
        match served(http, &url).await? {
            Some(back) if back == bytes => {}
            Some(_) => fail!(
                "O servidor devolveu um {} diferente do enviado ({url})",
                src.file
            ),
            None => fail!(
                "O servidor não oferece {url}. Ele precisa ser a versão que serve /emoji/: \
                 cargo xtask servidor"
            ),
        }
    }
    println!(
        "Lista de emoji no servidor: {}",
        mirror(&cfg.endereco, "emoji-test.txt")
    );
    Ok(())
}

/// The official file.
async fn download(http: &reqwest::Client, src: &Source) -> Result<Vec<u8>> {
    let res = http
        .get(src.url)
        .timeout(std::time::Duration::from_secs(120))
        .send()
        .await?;
    if !res.status().is_success() {
        fail!("{} respondeu {}", src.url, res.status());
    }
    Ok(res.bytes().await?.to_vec())
}

/// What the server offers at `url` now, if anything.
async fn served(http: &reqwest::Client, url: &str) -> Result<Option<Vec<u8>>> {
    let res = http
        .get(url)
        .header("cache-control", "no-cache")
        .timeout(std::time::Duration::from_secs(60))
        .send()
        .await?;
    if res.status().as_u16() == 404 {
        return Ok(None);
    }
    if !res.status().is_success() {
        fail!("{url} respondeu {}", res.status());
    }
    Ok(Some(res.bytes().await?.to_vec()))
}

/// Whether a file is what its name says, and what it holds ("Emoji 18.0,
/// 3963 emoji"): a wrong one would reach every app.
fn check(file: &str, text: &str) -> Result<String> {
    if file.ends_with(".json") {
        let v: Value =
            serde_json::from_str(text).map_err(|e| format!("{file} não é um JSON válido: {e}"))?;
        let body = v
            .get("annotations")
            .or_else(|| v.get("annotationsDerived"))
            .ok_or_else(|| format!("{file} não tem as anotações do CLDR"))?;
        let language = body["identity"]["language"].as_str().unwrap_or_default();
        let names = body["annotations"].as_object().map_or(0, |m| m.len());
        if language != "pt" || names < 1000 {
            fail!(
                "{file}: idioma \"{language}\", {names} nomes; esperava português e mais de 1000"
            );
        }
        return Ok(format!(
            "CLDR {}, {names} nomes",
            babutela_buildinfo::emoji::CLDR
        ));
    }
    let version = text
        .lines()
        .find_map(|l| l.strip_prefix("# Version:"))
        .map(str::trim)
        .unwrap_or_default();
    let emoji = text
        .lines()
        .filter(|l| !l.starts_with('#') && l.contains("; fully-qualified"))
        .count();
    if version.is_empty() || emoji < 1000 || !text.trim_end().ends_with("#EOF") {
        fail!("{file} não parece a lista do Unicode (versão \"{version}\", {emoji} emoji)");
    }
    Ok(format!("Emoji {version}, {emoji} emoji"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_real_lists_go_out() {
        let mut list = String::from("# emoji-test.txt\n# Version: 18.0\n# group: Flags\n");
        for _ in 0..1000 {
            list += "1F1E7 1F1F7 ; fully-qualified # x E2.0 flag: Brazil\n";
        }
        assert_eq!(
            check("emoji-test.txt", &(list.clone() + "#EOF\n")).unwrap(),
            "Emoji 18.0, 1000 emoji"
        );
        // Cut short, or a page that is not it.
        assert!(check("emoji-test.txt", &list).is_err());
        assert!(check("emoji-test.txt", "<html>429 Too Many Requests</html>").is_err());

        let names: serde_json::Map<String, Value> = (0..1000)
            .map(|i| (format!("e{i}"), serde_json::json!({ "tts": ["x"] })))
            .collect();
        let file = |key: &str, language: &str| {
            serde_json::json!({ key: { "identity": { "language": language }, "annotations": names } })
                .to_string()
        };
        assert!(check("annotations.json", &file("annotations", "pt")).is_ok());
        assert!(check("annotationsDerived.json", &file("annotationsDerived", "pt")).is_ok());
        assert!(check("annotations.json", &file("annotations", "en")).is_err());
        assert!(check("annotations.json", "{\"annotations\": ").is_err());
    }
}
