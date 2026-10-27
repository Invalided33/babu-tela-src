//! The secret mixed into the DTLS handshake of each connection a call or a
//! direct transfer makes (vendor/rtc-dtls), so that their media and data
//! stay closed even to someone who records them today and breaks the ECDHE
//! later (with a quantum computer, say).
//!
//! Each side makes 32 random bytes for the connection and puts them in its
//! descriptions (`pq`). Those go signed by the sender's identity and sealed
//! for the other person with X-Wing (signals.rs), so only the two of them
//! know the bytes, and a recording of the signalling opens only if X25519
//! and ML-KEM-768 both fall. Both then mix the same secret, made of the two
//! halves, into the handshake, the way RFC 5489's ECDHE_PSK suites mix a
//! pre-shared key: every key of the connection depends on it, and the
//! handshake fails (at Finished) unless both sides had the same. A
//! description that comes without its half is refused, so a connection
//! never goes without.

use std::sync::{Arc, Mutex, PoisonError};

use rtc::peer_connection::configuration::setting_engine::DtlsExtraSecret;
use serde_json::Value;

use super::prim;

const DS_DTLS: &[u8] = b"babutela/e2e/v1/dtls";

/// Where a description carries the sender's half.
const FIELD: &str = "pq";

pub struct DtlsSecret {
    mine: [u8; 32],
    theirs: Mutex<Option<[u8; 32]>>,
}

impl DtlsSecret {
    /// A new connection's: my half, theirs still to come.
    pub fn new() -> Arc<DtlsSecret> {
        Arc::new(DtlsSecret {
            mine: prim::random(),
            theirs: Mutex::new(None),
        })
    }

    /// `data`, a signal about to go out, with my half if it is a
    /// description (every one of a connection carries the same).
    pub fn attach(&self, data: &mut Value) {
        if data.get("description").is_some()
            && let Value::Object(m) = data
        {
            m.insert(FIELD.into(), Value::String(prim::b64(&self.mine)));
        }
    }

    /// Their half, from a description of theirs: false when it has none. A
    /// connection's half never changes, so the first one counts.
    pub fn take(&self, data: &Value) -> bool {
        let Some(theirs) = data
            .get(FIELD)
            .and_then(Value::as_str)
            .and_then(prim::unb64_n::<32>)
        else {
            return false;
        };
        let mut slot = self.theirs.lock().unwrap_or_else(PoisonError::into_inner);
        match *slot {
            None => *slot = Some(theirs),
            Some(had) if had != theirs => {
                crate::logln!(
                    "e2e: uma descrição veio com outra metade do segredo do DTLS; fica a primeira"
                );
            }
            Some(_) => {}
        }
        true
    }

    /// The secret both sides mix in, once they each have the other's half:
    /// the two halves in an order both agree on.
    pub fn secret(&self) -> Option<Vec<u8>> {
        let theirs = (*self.theirs.lock().unwrap_or_else(PoisonError::into_inner))?;
        let (a, b) = if self.mine <= theirs {
            (self.mine, theirs)
        } else {
            (theirs, self.mine)
        };
        Some(prim::sha256(&[DS_DTLS, &a, &b]).to_vec())
    }

    /// What the connection's settings ask when its DTLS starts.
    pub fn hook(self: &Arc<Self>) -> DtlsExtraSecret {
        let me = self.clone();
        Arc::new(move || me.secret())
    }
}
