//! A new PC of the same account gets the keys from one that has them.
//!
//! The new PC makes a throwaway X-Wing key and shows a code: 16 characters
//! of its hash. Its public key goes, through the server, to the account's
//! other PCs; on one of them the person types the code, and that PC checks
//! the key against it before sealing everything for it. The server carries
//! both steps and can neither read the keys nor put its own key in the
//! new PC's place: that key's code would not be the one on the new PC's
//! screen (finding another key with the same 80-bit code is out of reach).

use super::keys::{Account, Bundle, KemKey};
use super::prim::{self, Identity};
use super::seal;
use super::store::Saved;

const DS_PROVISION: &[u8] = b"babutela/e2e/v1/provisao";

/// Crockford's base32: no I, L, O or U, so nothing reads as something else.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// The code of a new PC's key, as typed: 16 characters, without the dashes.
pub fn code_of(public: &[u8]) -> String {
    let h = prim::sha256(&[DS_PROVISION, public]);
    let bits = u128::from_be_bytes(h[..16].try_into().unwrap_or_default());
    (0..16)
        .map(|i| ALPHABET[((bits >> (123 - 5 * i)) & 31) as usize] as char)
        .collect()
}

/// The code as shown: four groups of four.
pub fn shown(code: &str) -> String {
    code.as_bytes()
        .chunks(4)
        .map(|c| String::from_utf8_lossy(c).into_owned())
        .collect::<Vec<_>>()
        .join("-")
}

/// What the person typed, as a code: case, spaces and dashes do not
/// matter, and the letters that look like digits are taken as them.
pub fn normalize(typed: &str) -> String {
    typed
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| match c.to_ascii_uppercase() {
            'O' => '0',
            'I' | 'L' => '1',
            c => c,
        })
        .collect()
}

/// The new PC's side: its throwaway key, until the keys come.
pub struct Request {
    secret: [u8; 32],
    pub public: Vec<u8>,
    pub code: String,
}

impl Default for Request {
    fn default() -> Self {
        Self::new()
    }
}

impl Request {
    pub fn new() -> Request {
        let pair = prim::kem_generate();
        let code = code_of(&pair.public);
        Request {
            secret: pair.secret,
            public: pair.public,
            code,
        }
    }

    pub fn public_b64(&self) -> String {
        prim::b64(&self.public)
    }

    /// The keys an old PC of `uid` sealed for this request.
    pub fn open(&self, uid: i64, sealed_b64: &str) -> Option<Saved> {
        let sealed = prim::unb64(sealed_b64)?;
        let me = Account {
            uid,
            identity: Identity::from_seed([0; 32]),
            kem: vec![KemKey {
                secret: self.secret,
                public: self.public.clone(),
                at: 0,
                sig: Vec::new(),
            }],
            eph: Vec::new(),
        };
        let plain = seal::open_from(&me, uid, DS_PROVISION, &sealed)?;
        let saved: Saved = serde_json::from_slice(&plain).ok()?;
        (saved.v == 1 && saved.uid == uid).then_some(saved)
    }
}

/// The old PC's side: the account's keys (`export`, store.rs) sealed for
/// the new PC's key `public_b64`, if the code typed is that key's.
pub fn approve(uid: i64, export: &[u8], public_b64: &str, typed: &str) -> Result<String, String> {
    let public = prim::unb64(public_b64)
        .filter(|p| p.len() == prim::KEM_PUBLIC)
        .ok_or("o pedido veio com uma chave inválida")?;
    if !prim::same(normalize(typed).as_bytes(), code_of(&public).as_bytes()) {
        return Err("O código não é o que aparece no PC novo. Nada foi enviado.".into());
    }
    let to = Bundle {
        uid,
        identity: [0; 32],
        kem: public,
        at: 0,
    };
    let sealed =
        seal::seal_to(uid, &to, DS_PROVISION, export).ok_or("não deu para selar as chaves")?;
    Ok(prim::b64(&sealed))
}
