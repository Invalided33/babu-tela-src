//! Uploads to R2. R2 speaks the S3 protocol, so every request is signed with
//! AWS Signature Version 4, using R2 keys: the ones in build/cloudflare.json
//! or, without them, the ones the API token makes (`keys`).

use std::time::Duration;

use hmac::{Hmac, KeyInit, Mac};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::cf::{Config, FILE};
use crate::tools::escape;
use crate::{Result, fail};

type HmacSha256 = Hmac<Sha256>;

fn hmac(key: &[u8], data: &str) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC takes keys of any size");
    mac.update(data.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

pub struct Signer<'a> {
    pub access_key: &'a str,
    pub secret_key: &'a str,
    pub region: &'a str,
    pub service: &'a str,
}

impl Signer<'_> {
    /// The Authorization header for a request. `headers` are the ones to
    /// sign, names in lowercase; `amz_date` is also in them as x-amz-date.
    pub fn authorization(
        &self,
        method: &str,
        path: &str,
        query: &str,
        headers: &[(&str, &str)],
        payload_hash: &str,
        amz_date: &str,
    ) -> String {
        let mut headers: Vec<(&str, &str)> = headers.to_vec();
        headers.sort_by_key(|(name, _)| *name);
        let canonical_headers: String = headers
            .iter()
            .map(|(name, value)| format!("{name}:{}\n", value.trim()))
            .collect();
        let signed = headers
            .iter()
            .map(|(name, _)| *name)
            .collect::<Vec<_>>()
            .join(";");
        let canonical =
            format!("{method}\n{path}\n{query}\n{canonical_headers}\n{signed}\n{payload_hash}");
        let day = &amz_date[..8];
        let scope = format!("{day}/{}/{}/aws4_request", self.region, self.service);
        let to_sign = format!(
            "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
            sha256_hex(canonical.as_bytes())
        );
        let k_date = hmac(format!("AWS4{}", self.secret_key).as_bytes(), day);
        let k_region = hmac(&k_date, self.region);
        let k_service = hmac(&k_region, self.service);
        let k_signing = hmac(&k_service, "aws4_request");
        let signature = hex::encode(hmac(&k_signing, &to_sign));
        format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={signature}",
            self.access_key
        )
    }
}

/// The keys R2 requests are signed with.
pub struct Keys {
    id: String,
    secret: String,
}

impl Keys {
    fn signer(&self) -> Signer<'_> {
        Signer {
            access_key: &self.id,
            secret_key: &self.secret,
            region: "auto",
            service: "s3",
        }
    }
}

/// The R2 keys of build/cloudflare.json or, when both are empty, the API
/// token's own: Cloudflare takes a token with R2 permissions as S3 keys,
/// its id as the access key and the SHA-256 of its value as the secret.
pub async fn keys(http: &reqwest::Client, cfg: &Config) -> Result<Keys> {
    if cfg.bucket.is_empty() {
        fail!("Falta \"bucket\" em {FILE}.");
    }
    match (
        cfg.r2_access_key_id.is_empty(),
        cfg.r2_secret_access_key.is_empty(),
    ) {
        (false, false) => {
            return Ok(Keys {
                id: cfg.r2_access_key_id.clone(),
                secret: cfg.r2_secret_access_key.clone(),
            });
        }
        (true, true) => {}
        _ => fail!(
            "Preencha \"r2AccessKeyId\" e \"r2SecretAccessKey\" em {FILE}, ou deixe os dois \
             vazios para usar o apiToken."
        ),
    }
    // A user's token answers at /user, an account's at /accounts/<id>.
    for url in [
        "https://api.cloudflare.com/client/v4/user/tokens/verify".to_owned(),
        format!(
            "https://api.cloudflare.com/client/v4/accounts/{}/tokens/verify",
            cfg.conta_id
        ),
    ] {
        let res = http
            .get(&url)
            .bearer_auth(&cfg.api_token)
            .timeout(Duration::from_secs(30))
            .send()
            .await?;
        if !res.status().is_success() {
            continue;
        }
        let body: Value = res.json().await.unwrap_or(Value::Null);
        if let Some(id) = body["result"]["id"].as_str().filter(|id| !id.is_empty()) {
            return Ok(Keys {
                id: id.to_owned(),
                secret: sha256_hex(cfg.api_token.as_bytes()),
            });
        }
    }
    fail!(
        "A Cloudflare não confirmou o apiToken de {FILE}, que serve de chave do R2 quando \
         \"r2AccessKeyId\" e \"r2SecretAccessKey\" estão vazios."
    )
}

fn host(cfg: &Config) -> String {
    format!("{}.r2.cloudflarestorage.com", cfg.conta_id)
}

fn amz_date() -> String {
    chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string()
}

/// Whether `keys` open the bucket: a listing of one name, which changes
/// nothing. `release update` asks first, so it never stops halfway, with
/// the server out and no update for the apps.
pub async fn check(http: &reqwest::Client, cfg: &Config, keys: &Keys) -> Result {
    let host = host(cfg);
    let path = format!("/{}", escape(&cfg.bucket));
    let query = "list-type=2&max-keys=1";
    let hash = sha256_hex(b"");
    let amz_date = amz_date();
    let auth = keys.signer().authorization(
        "GET",
        &path,
        query,
        &[
            ("host", &host),
            ("x-amz-content-sha256", &hash),
            ("x-amz-date", &amz_date),
        ],
        &hash,
        &amz_date,
    );
    let res = http
        .get(format!("https://{host}{path}?{query}"))
        .header("x-amz-content-sha256", &hash)
        .header("x-amz-date", &amz_date)
        .header("authorization", auth)
        .timeout(Duration::from_secs(60))
        .send()
        .await?;
    if !res.status().is_success() {
        let status = res.status().as_u16();
        let text = res.text().await.unwrap_or_default();
        fail!(
            "O R2 não abriu o bucket \"{}\" (HTTP {status}): {}\nO apiToken precisa poder ler e \
             escrever no R2, ou preencha \"r2AccessKeyId\" e \"r2SecretAccessKey\" em {FILE}.",
            cfg.bucket,
            text.trim()
        );
    }
    Ok(())
}

/// Puts one object in the bucket from build/cloudflare.json.
pub async fn put(
    http: &reqwest::Client,
    cfg: &Config,
    keys: &Keys,
    key: &str,
    body: Vec<u8>,
    content_type: &str,
) -> Result {
    let host = host(cfg);
    let path = format!(
        "/{}/{}",
        escape(&cfg.bucket),
        key.split('/').map(escape).collect::<Vec<_>>().join("/")
    );
    let hash = sha256_hex(&body);
    let amz_date = amz_date();
    let auth = keys.signer().authorization(
        "PUT",
        &path,
        "",
        &[
            ("content-type", content_type),
            ("host", &host),
            ("x-amz-content-sha256", &hash),
            ("x-amz-date", &amz_date),
        ],
        &hash,
        &amz_date,
    );
    let res = http
        .put(format!("https://{host}{path}"))
        .header("content-type", content_type)
        .header("x-amz-content-sha256", &hash)
        .header("x-amz-date", &amz_date)
        .header("authorization", auth)
        .timeout(std::time::Duration::from_secs(600))
        .body(body)
        .send()
        .await?;
    if !res.status().is_success() {
        let status = res.status().as_u16();
        let text = res.text().await.unwrap_or_default();
        fail!("O R2 recusou {key} (HTTP {status}): {}", text.trim());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// "Example: GET Object" from Amazon's Signature Version 4 docs for S3.
    #[test]
    fn signs_the_documented_example() {
        let signer = Signer {
            access_key: "AKIAIOSFODNN7EXAMPLE",
            secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            region: "us-east-1",
            service: "s3",
        };
        let empty = sha256_hex(b"");
        assert_eq!(
            empty,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        let auth = signer.authorization(
            "GET",
            "/test.txt",
            "",
            &[
                ("x-amz-date", "20130524T000000Z"),
                ("host", "examplebucket.s3.amazonaws.com"),
                ("range", "bytes=0-9"),
                ("x-amz-content-sha256", &empty),
            ],
            &empty,
            "20130524T000000Z",
        );
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, \
             SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, \
             Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
    }
}
