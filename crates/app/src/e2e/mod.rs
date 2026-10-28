//! End-to-end encryption: messages, files, reactions and the signalling of
//! calls and direct transfers are encrypted on the sender's PC and opened
//! only on the PCs of whoever can read the conversation. The server (the
//! Worker, its database, R2, the relay) keeps and carries bytes it cannot
//! read, and cannot change, move or make up without that showing.
//!
//! - prim.rs: the algorithms (Ed25519, X-Wing, HKDF, AES-256-GCM).
//! - keys.rs: an account's keys, others' identities and security numbers.
//! - seal.rs: bytes sealed (and signed) for one account.
//! - msg.rs: sending sessions, message envelopes, reactions.
//! - file.rs: files encrypted in blocks before they go up.
//! - provision.rs: a new PC of the same account gets the keys from an old
//!   one, checked by a code the person types.
//! - signals.rs: the signalling of calls and direct transfers, sealed.
//! - dtls.rs: the post-quantum secret each of their connections mixes into
//!   its DTLS handshake, carried in that sealed signalling.
//! - members.rs: who is a member of a server, as its members sign it: only
//!   those the signatures reach from the owner get the keys of its
//!   conversations.
//! - store.rs: the keys on disk, under DPAPI.
//! - record.rs: this PC's copy of the public record of keys (and of the
//!   app's versions), which no key gets past unlisted, and which the apps
//!   compare among themselves.
//!
//! What the server still sees is in README.md ("Privacidade").

pub mod dtls;
mod file;
pub mod keys;
pub mod members;
pub mod msg;
mod prim;
pub mod provision;
pub mod record;
mod seal;
pub mod signals;
mod store;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde_json::{Value, json};

pub use file::{BLOCK, FileKey, SEALED_BLOCK, key_for, open_downloaded, sealed_range};
use keys::{Account, Bundle, Pin, Trust};
use msg::{Body, InSession, OutSession, Refused};
use store::Saved;

use crate::model::{Attachment, Kind, Message, Seal};
use crate::net::{Api, Req};

/// Where this PC stands with the account's keys.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    /// Signed out.
    Off,
    /// Loading them, or asking the server.
    Starting,
    Ready,
    /// The account has keys, on another PC: this one needs them from
    /// there (provision.rs), or a new identity.
    NeedsDevice,
    Failed(String),
}

/// What a message needs before it opens.
#[derive(Default, Debug)]
pub struct Need {
    pub sids: HashSet<[u8; 16]>,
    pub users: HashSet<i64>,
    /// Someone's copy of the public record is ahead of this PC's: the new
    /// entries, to compare.
    pub record: bool,
}

impl Need {
    pub fn is_empty(&self) -> bool {
        self.sids.is_empty() && self.users.is_empty() && !self.record
    }

    pub fn merge(&mut self, other: Need) {
        self.sids.extend(other.sids);
        self.users.extend(other.users);
        self.record |= other.record;
    }
}

/// Something about the public record of keys the person must hear of.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Alert {
    /// The server showed this person another record than mine: someone is
    /// in the middle.
    Fork(i64),
    /// This person's copy has entries the server keeps from me.
    Hidden(i64),
    /// A key was published for my account that this PC never had.
    Foreign,
    /// The server handed out a key for this person that is not their
    /// newest in the record: nothing goes to it.
    Unlisted(i64),
    /// The server changed an entry this PC's copy already had: a record
    /// that only grows does not do that.
    Rewritten,
    /// The server names another owner for this server than the one this
    /// PC pinned (members.rs): its certificates still root in the pinned
    /// one.
    Owner(i64),
}

/// How a message came out.
#[derive(Debug)]
pub enum Opened {
    /// Not encrypted: from before the encryption, or from an old app.
    Plain,
    Open,
    /// Keys still to fetch.
    Need(Need),
    Refused(Refused),
    /// This PC has no keys yet.
    NoKeys,
    /// Its time is up (a conversation whose messages disappear): it goes.
    Expired,
}

/// What to send a message with: my session, or a new one and its key
/// sealed for each reader (to go to the server first).
enum Plan {
    Keep(OutSession),
    New(OutSession, serde_json::Map<String, Value>),
}

/// Who may get a conversation's key, as the app knows it: the readers the
/// server names are checked against it.
#[derive(Clone, Debug)]
pub enum Allowed {
    /// A direct conversation: exactly its two people, or nothing is sent.
    Exactly(HashSet<i64>),
    /// A server's channel: at most the server's members, as its member
    /// list shows them to everyone. A reader the server counts but nobody
    /// sees there gets nothing: a ghost would have to show up in the list.
    Within(HashSet<i64>),
}

/// What the members' certificates say of a server (`E2e::vouched`).
#[derive(Debug, Default)]
pub struct Vouched {
    /// The members the signatures reach from the owner.
    pub reached: HashSet<i64>,
    /// Certificates came, now or before: only those reached get keys.
    pub strict: bool,
    /// Identities still to fetch to check the rest.
    pub need: Need,
    /// Members with a certificate of the owner's own.
    pub by_owner: HashSet<i64>,
}

/// Someone's identity as this PC knows it, for the interface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PinView {
    Unknown,
    Pinned {
        verified: bool,
    },
    /// The server hands out another identity for them: until it is
    /// accepted, nothing is sent to them.
    Changed,
}

pub struct E2e {
    api: Arc<Api>,
    dir: PathBuf,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    state: Option<State>,
    saved: Option<Saved>,
    account: Option<Account>,
    /// Bundles fetched this session, checked against the pins.
    bundles: HashMap<i64, Bundle>,
    /// Each envelope's random id and the server's id of the message it came
    /// in: the same envelope under another message is the server repeating
    /// or moving it.
    seen: HashMap<[u8; 16], i64>,
    /// The first encrypted message seen in each conversation: a message in
    /// plain text after it did not come from this app.
    first_sealed: HashMap<i64, i64>,
    /// Something changed that is not on disk yet (pins, counts).
    dirty: bool,
    /// This PC's copy of the public record (record.rs).
    record: Option<record::Log>,
    /// Copies of others that are ahead of mine, to compare once mine
    /// catches up: whose, and how far.
    ahead: Vec<(i64, record::Head)>,
    /// What the person must hear of, until they dismiss it.
    alert: Option<Alert>,
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn sid_key(sid: &[u8; 16]) -> String {
    prim::b64(sid)
}

/// My account's current key as an entry of the record.
fn own_leaf(account: &Account) -> [u8; 32] {
    let k = account.current();
    record::key_leaf(account.uid, &account.identity.public(), &k.public, k.at)
}

/// One entry as the server sends it: [seq, kind, account, leaf, time].
fn parse_entry(v: &Value) -> Option<record::Entry> {
    let a = v.as_array()?;
    let kind = match a.get(1)?.as_str()? {
        "k" => b'k',
        "r" => b'r',
        _ => return None,
    };
    Some(record::Entry {
        seq: a.first()?.as_u64()?,
        kind,
        uid: a.get(2)?.as_i64()?,
        leaf: prim::unb64_n(a.get(3)?.as_str()?)?,
        at: a.get(4)?.as_i64()?,
    })
}

/// The most the keys sent to a new PC may take before sealing: sealed and
/// in base64 they must fit the server's provisioning message (192 KB).
const EXPORT_MAX: usize = 128 * 1024;

/// Why something cannot be sent encrypted.
pub fn no_keys_text() -> String {
    crate::t!("e2e.needs_keys").into()
}

impl E2e {
    pub fn new(api: Arc<Api>, dir: PathBuf) -> Arc<E2e> {
        Arc::new(E2e {
            api,
            dir,
            inner: Mutex::new(Inner::default()),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn state(&self) -> State {
        self.lock().state.clone().unwrap_or(State::Off)
    }

    pub fn me(&self) -> Option<i64> {
        self.lock().account.as_ref().map(|a| a.uid)
    }

    fn persist(&self, inner: &mut Inner) {
        if let Some(saved) = &inner.saved {
            if let Err(e) = store::save(&self.dir, saved) {
                crate::logln!("e2e: não deu para guardar as chaves: {e}");
            }
            inner.dirty = false;
        }
    }

    fn install(&self, inner: &mut Inner, mut saved: Saved) {
        // The keys of the day move on (or start), and the old ones go.
        if saved.roll_eph(keys::day_of(now_ms())) {
            inner.dirty = true;
        }
        let account = saved.account();
        // The keys this PC has are mine, whatever the record says.
        let leaf = prim::b64(&own_leaf(&account));
        if !saved.mine.contains(&leaf) {
            saved.mine.push(leaf);
            inner.dirty = true;
        }
        inner.record = Some(record::Log::load(&record::path(&self.dir, saved.uid)));
        inner.account = Some(account);
        inner.saved = Some(saved);
        inner.state = Some(State::Ready);
    }

    // ------------------------------------------------------------ the record

    /// Brings this PC's copy of the public record up to the server's, and
    /// checks what came: entries under my account that are not my keys,
    /// and the copies of others that were ahead of mine.
    pub async fn sync_record(&self) -> Result<(), String> {
        loop {
            let from = {
                let inner = self.lock();
                let Some(log) = inner.record.as_ref() else {
                    return Ok(());
                };
                log.size()
            };
            let v = self
                .api
                .call(&Req::get(format!("/e2e/log?from={from}")))
                .await
                .map_err(|e| e.message)?;
            let size = v.get("size").and_then(Value::as_u64).unwrap_or(0);
            let entries: Vec<record::Entry> = v
                .get("entries")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(parse_entry)
                .collect();
            let got = entries.len();
            let mut inner = self.lock();
            let me = inner.account.as_ref().map(|a| a.uid);
            let Some(log) = inner.record.as_mut() else {
                return Ok(());
            };
            let taken = match log.extend(entries) {
                Ok(taken) => taken,
                Err(e) => {
                    if matches!(e, record::Broken::Changed(_)) {
                        inner.alert = Some(Alert::Rewritten);
                    }
                    return Err(e.to_string());
                }
            };
            let now = log.size();
            // Entries under my account that this PC did not publish.
            let mine = inner
                .saved
                .as_ref()
                .map(|s| s.mine.clone())
                .unwrap_or_default();
            if taken
                .iter()
                .any(|e| e.kind == b'k' && Some(e.uid) == me && !mine.contains(&prim::b64(&e.leaf)))
            {
                crate::logln!(
                    "e2e: uma chave que este PC nunca teve foi publicada para a minha conta"
                );
                inner.alert = Some(Alert::Foreign);
            }
            if now >= size || got == 0 {
                Self::settle_ahead(&mut inner, size);
                return Ok(());
            }
        }
    }

    /// The copies of others that were ahead of mine, now that mine is as
    /// far as the server goes (`size`): each must agree with it.
    fn settle_ahead(inner: &mut Inner, size: u64) {
        let Some(log) = inner.record.as_ref() else {
            return;
        };
        let mut alert = None;
        for (uid, head) in std::mem::take(&mut inner.ahead) {
            match log.agrees(&head) {
                Some(true) => {}
                Some(false) => alert = Some(Alert::Fork(uid)),
                // They have more than the server gives me.
                None if head.n > size => alert = Some(Alert::Hidden(uid)),
                None => {}
            }
        }
        if let Some(a) = alert {
            crate::logln!("e2e: o registro público de chaves não confere: {a:?}");
            inner.alert = Some(a);
        }
    }

    /// Someone's copy of the record, as a message or a signal of theirs
    /// says it is: compared with mine now, or once mine catches up (true:
    /// mine must catch up).
    fn compare_head(inner: &mut Inner, from: i64, head: &record::Head) -> bool {
        let Some(log) = inner.record.as_ref() else {
            return false;
        };
        match log.agrees(head) {
            Some(true) => false,
            Some(false) => {
                crate::logln!("e2e: o registro público de {from} não é o meu");
                inner.alert = Some(Alert::Fork(from));
                false
            }
            None => {
                if !inner.ahead.iter().any(|(u, h)| *u == from && h == head) {
                    inner.ahead.push((from, head.clone()));
                }
                true
            }
        }
    }

    /// My copy of the record as it is now, for a message or a signal.
    fn my_head(inner: &Inner) -> Option<record::Head> {
        inner.record.as_ref().map(record::Log::head)
    }

    /// Whether `b` is its account's newest key in the record.
    fn listed(inner: &Inner, b: &Bundle) -> bool {
        let leaf = record::key_leaf(b.uid, &b.identity, &b.kem, b.at);
        inner
            .record
            .as_ref()
            .and_then(|log| log.newest_key(b.uid))
            .is_some_and(|e| prim::same(&e.leaf, &leaf))
    }

    /// The record brought up to date when one of `bundles` is not in this
    /// PC's copy yet (it may have been published a moment ago).
    async fn ensure_listed(&self, bundles: &[&Bundle]) {
        let missing = {
            let inner = self.lock();
            bundles.iter().any(|b| !Self::listed(&inner, b))
        };
        if missing && let Err(e) = self.sync_record().await {
            crate::logln!("e2e: não deu para ler o registro público: {e}");
        }
    }

    // ------------------------------------------------------------ messages that disappear

    /// A new day (and the start): the keys of the day move on, the ones
    /// older than `EPH_KEEP_DAYS` are dropped, and so are the sessions of
    /// conversations whose messages disappear once nothing they carried
    /// can still be shown; then today's and tomorrow's keys are published.
    /// Whatever was sealed for what went cannot be opened again, not even
    /// with this PC and the server's database together.
    pub async fn day(&self) -> Result<(), String> {
        let today = keys::day_of(now_ms());
        let prekeys = {
            let mut inner = self.lock();
            let Some(saved) = inner.saved.as_mut() else {
                return Ok(());
            };
            let mut changed = saved.roll_eph(today);
            let now = now_ms();
            // A session there is sent with for a day, and its messages last
            // their time after that: mine, the time it was made for; one
            // of someone else's, the longest there is (7 days), as this PC
            // cannot tell which it was.
            let gone = |at: i64, ttl: i64| now > at + 2 * 86_400_000 + ttl * 1000;
            let before = (saved.inb.len(), saved.out.len());
            saved.inb.retain(|_, s| !(s.eph && gone(s.at, 7 * 86_400)));
            saved.out.retain(|s| !(s.ttl > 0 && gone(s.at, s.ttl)));
            changed |= before != (saved.inb.len(), saved.out.len());
            let account = saved.account();
            let prekeys = account.eph_bundle(&[today, today + 1]);
            inner.account = Some(account);
            if changed {
                self.persist(&mut inner);
            }
            prekeys
        };
        self.api
            .call(&Req::put("/e2e/prekeys").with(json!({ "prekeys": prekeys })))
            .await
            .map(|_| ())
            .map_err(|e| e.message)
    }

    // ------------------------------------------------------------ members

    /// What the members' certificates say of `server` (members.rs): who
    /// they vouch for, whether it is past the start of certificates (from
    /// then on only those read its conversations), and the identities to
    /// fetch to check the certificates not checked yet.
    pub fn vouched(&self, server: &crate::model::Server) -> Vouched {
        let mut inner = self.lock();
        let members: Vec<(i64, Vec<members::Cert>)> = server
            .members
            .iter()
            .map(|u| {
                let certs = u
                    .certs
                    .iter()
                    .filter_map(|c| serde_json::from_value(c.clone()).ok())
                    .collect();
                (u.id, certs)
            })
            .collect();
        let any = members.iter().any(|(_, c)| !c.is_empty());
        if let Some(saved) = inner.saved.as_mut()
            && any
            && !saved.strict.contains(&server.id)
        {
            saved.strict.push(server.id);
            inner.dirty = true;
        }
        let strict = inner
            .saved
            .as_ref()
            .is_some_and(|s| s.strict.contains(&server.id));
        let owner = Self::owner_of(&mut inner, server);
        let ids = |u: i64| Self::identities(&inner, u);
        let reached = members::vouched(server.id, owner, &members, &ids);
        let mut need = Need::default();
        for (_, certs) in &members {
            for c in certs {
                if Self::identities(&inner, c.by).is_empty() {
                    need.users.insert(c.by);
                }
            }
        }
        // Whom the owner vouches for directly (so their entry does not hang
        // on an inviter who may leave).
        let by_owner = members
            .iter()
            .filter(|(u, certs)| {
                certs
                    .iter()
                    .any(|c| c.by == owner && members::holds(c, server.id, *u, &ids))
            })
            .map(|(u, _)| *u)
            .collect();
        Vouched {
            reached,
            strict,
            need,
            by_owner,
        }
    }

    /// A certificate signed by me: "dono", "aceito" or "garantia".
    pub fn sign_member(&self, how: &str, sid: i64, uid: i64) -> Option<members::Cert> {
        let inner = self.lock();
        let me = inner.account.as_ref()?;
        Some(members::signed(
            &me.identity,
            me.uid,
            how,
            sid,
            uid,
            now_ms(),
        ))
    }

    /// The owner of `server` as this PC pinned it, pinning the one the
    /// server names when there is none yet. One it names later that is not
    /// the pinned one is the server lying (an owner never changes: there is
    /// no transfer, and the owner cannot leave), and the person hears of it.
    fn owner_of(inner: &mut Inner, server: &crate::model::Server) -> i64 {
        let key = server.id.to_string();
        let Some(saved) = inner.saved.as_mut() else {
            return server.owner_id;
        };
        if let Some(&pinned) = saved.owners.get(&key) {
            if pinned != server.owner_id && inner.alert != Some(Alert::Owner(server.id)) {
                crate::logln!(
                    "e2e: o servidor {} diz que o dono é {}, mas este PC fixou {pinned}",
                    server.id,
                    server.owner_id
                );
                inner.alert = Some(Alert::Owner(server.id));
            }
            return pinned;
        }
        saved.owners.insert(key, server.owner_id);
        inner.dirty = true;
        server.owner_id
    }

    /// The owner of `server`, as this PC pinned it.
    pub fn owner(&self, server: &crate::model::Server) -> i64 {
        Self::owner_of(&mut self.lock(), server)
    }

    /// A new invite to `server`, signed by me, with its owner as this PC
    /// pinned it.
    pub fn new_invite(&self, server: &crate::model::Server) -> Option<members::NewInvite> {
        let mut inner = self.lock();
        let owner = Self::owner_of(&mut inner, server);
        let me = inner.account.as_ref()?;
        Some(members::new_invite(
            &me.identity,
            me.uid,
            server.id,
            owner,
            now_ms(),
        ))
    }

    /// The owner the invite `inv` names, pinned, when the invite holds (the
    /// link's, signed by its creator, whose identity this PC must have
    /// fetched first): theirs is the server's from now on, whatever the
    /// server showed before.
    pub fn pin_owner(&self, secret: &[u8; 32], inv: &members::InviteCert) {
        let mut inner = self.lock();
        let by = Self::identities(&inner, inv.by);
        if inv.o == 0 || !members::invite_holds(inv, secret, &by) {
            return;
        }
        let Some(saved) = inner.saved.as_mut() else {
            return;
        };
        let had = saved.owners.insert(inv.s.to_string(), inv.o);
        if had.is_some_and(|o| o != inv.o) {
            crate::logln!(
                "e2e: o convite diz que o dono do servidor {} é {}, não {had:?}",
                inv.s,
                inv.o
            );
            inner.alert = Some(Alert::Owner(inv.s));
        }
        inner.dirty = true;
    }

    /// My entry through the invite `inv`, with the link's secret (and the
    /// owner it names, pinned).
    pub fn entry(&self, secret: &[u8; 32], inv: &members::InviteCert) -> Option<members::Cert> {
        self.pin_owner(secret, inv);
        let me = self.me()?;
        members::entry(secret, inv, me, now_ms())
    }

    /// Whether this PC has the account's keys and its copy of the record.
    pub fn ready(&self) -> bool {
        let inner = self.lock();
        inner.state == Some(State::Ready) && inner.record.is_some()
    }

    /// Whether this version of the app is in the public record (taking what
    /// came since, if this PC's copy does not have it yet). Nothing is
    /// installed that is not: an update made for one person alone would
    /// have to show to everyone.
    pub async fn release_listed(
        &self,
        version: &str,
        file: &str,
        sha256: &str,
    ) -> Result<(), String> {
        let leaf = record::release_leaf(version, file, sha256);
        let has = |me: &Self| {
            me.lock()
                .record
                .as_ref()
                .is_some_and(|log| log.has_release(&leaf))
        };
        if has(self) {
            return Ok(());
        }
        self.sync_record().await?;
        if has(self) {
            Ok(())
        } else {
            Err(format!("a versão {version} não está no registro público"))
        }
    }

    /// What the person must hear of about the record, if anything.
    pub fn alert(&self) -> Option<Alert> {
        self.lock().alert.clone()
    }

    pub fn dismiss_alert(&self) {
        self.lock().alert = None;
    }

    /// Whether `uid`'s key, as this PC pinned it, is their newest in the
    /// record (for the security number's dialog).
    pub fn in_record(&self, uid: i64) -> bool {
        let inner = self.lock();
        inner
            .bundles
            .get(&uid)
            .is_some_and(|b| Self::listed(&inner, b))
    }

    /// Signed in as `uid`: the keys from disk, or new ones when the account
    /// has none anywhere, or the wait for another PC of the account.
    pub async fn start(self: &Arc<Self>, uid: i64) -> State {
        {
            let mut inner = self.lock();
            if inner.account.as_ref().is_some_and(|a| a.uid == uid) {
                return State::Ready;
            }
            *inner = Inner {
                state: Some(State::Starting),
                ..Inner::default()
            };
        }
        let on_server = match self.fetch_bundles(&[uid]).await {
            Ok(mut b) => b.remove(&uid),
            Err(e) => {
                let mut inner = self.lock();
                inner.state = Some(State::Failed(e.clone()));
                return State::Failed(e);
            }
        };
        if let Some(saved) = store::load(&self.dir, uid) {
            let mine = saved.account();
            // The server lost my keys, or hands out others under my name:
            // mine go back up (the others notice a change, if anyone took
            // the false ones).
            let differs = on_server.as_ref().is_none_or(|b| {
                b.identity != mine.identity.public() || b.kem != mine.current().public
            });
            if differs {
                if on_server.is_some() {
                    crate::logln!(
                        "e2e: o servidor tinha outra chave para mim; publico a minha de novo"
                    );
                }
                if let Err(e) = self.publish(&mine).await {
                    crate::logln!("e2e: não deu para publicar minhas chaves: {e}");
                }
            }
            {
                let mut inner = self.lock();
                self.install(&mut inner, saved);
                if inner.dirty {
                    self.persist(&mut inner);
                }
            }
            // The record as it is now: what came since, my account's
            // entries among it.
            if let Err(e) = self.sync_record().await {
                crate::logln!("e2e: não deu para ler o registro público: {e}");
            }
            return State::Ready;
        }
        if on_server.is_some() {
            self.lock().state = Some(State::NeedsDevice);
            return State::NeedsDevice;
        }
        self.create(uid).await
    }

    /// A new identity for my account, published: what this PC does the
    /// first time, or when the person gives up on the old keys (lost PC).
    pub async fn create(self: &Arc<Self>, uid: i64) -> State {
        let account = Account::generate(uid, now_ms());
        if let Err(e) = self.publish(&account).await {
            let mut inner = self.lock();
            inner.state = Some(State::Failed(e.clone()));
            return State::Failed(e);
        }
        let saved = Saved {
            v: 1,
            uid,
            identity: *account.identity.seed(),
            kem: account.kem.clone(),
            ..Saved::default()
        };
        {
            let mut inner = self.lock();
            self.install(&mut inner, saved);
            self.persist(&mut inner);
        }
        if let Err(e) = self.sync_record().await {
            crate::logln!("e2e: não deu para ler o registro público: {e}");
        }
        State::Ready
    }

    async fn publish(&self, account: &Account) -> Result<(), String> {
        self.api
            .call(&Req::put("/e2e/keys").with(json!({ "keys": account.bundle() })))
            .await
            .map(|_| ())
            .map_err(|e| e.message)
    }

    /// Keys that came from another PC of mine (provision.rs), with that
    /// PC's copy of the record, which mine is compared with as it catches
    /// up (a server that showed the new PC another record shows there).
    pub(crate) fn adopt(&self, mut saved: Saved) {
        let head = saved.head.take();
        let mut inner = self.lock();
        let me = saved.uid;
        self.install(&mut inner, saved);
        if let Some(head) = head {
            inner.ahead.push((me, head));
        }
        self.persist(&mut inner);
    }

    /// What goes to a new PC of mine: the account's keys and the people this
    /// PC pinned. Not the sessions: the new PC asks the server for the ones
    /// sealed for the account when a message needs them, and starts its own
    /// to send.
    pub(crate) fn export(&self) -> Result<Vec<u8>, String> {
        let mut keys = {
            let inner = self.lock();
            let saved = inner.saved.as_ref().ok_or_else(no_keys_text)?;
            Saved {
                v: saved.v,
                uid: saved.uid,
                identity: saved.identity,
                kem: saved.kem.clone(),
                pins: saved.pins.clone(),
                older: saved.older.clone(),
                mine: saved.mine.clone(),
                strict: saved.strict.clone(),
                owners: saved.owners.clone(),
                eph_seed: saved.eph_seed,
                eph_day: saved.eph_day,
                head: Self::my_head(&inner),
                ..Saved::default()
            }
        };
        let mut out = serde_json::to_vec(&keys).map_err(|e| e.to_string())?;
        if out.len() > EXPORT_MAX {
            // Too many people for one message: some pins stay behind, the
            // unverified ones first, and the new PC pins those people again
            // when it first sees them. An identity change still to be
            // accepted always goes.
            let mut spare: Vec<(bool, String)> = keys
                .pins
                .iter()
                .filter(|(_, p)| p.changed.is_none())
                .map(|(k, p)| (p.verified, k.clone()))
                .collect();
            spare.sort();
            let before = keys.pins.len();
            let mut excess = out.len() - EXPORT_MAX;
            for (_, k) in spare {
                if excess == 0 {
                    break;
                }
                if let Some(p) = keys.pins.remove(&k) {
                    // `"k":{...},` left the map.
                    let cost = k.len() + 4 + serde_json::to_vec(&p).map_or(0, |v| v.len());
                    excess = excess.saturating_sub(cost);
                }
            }
            out = serde_json::to_vec(&keys).map_err(|e| e.to_string())?;
            crate::logln!(
                "e2e: {} pessoas ficaram fora das chaves enviadas ao PC novo (ele as fixa quando as vir)",
                before - keys.pins.len()
            );
        }
        if out.len() > EXPORT_MAX {
            return Err(crate::t!("e2e.too_many_keys").into());
        }
        Ok(out)
    }

    pub fn stop(&self) {
        let mut inner = self.lock();
        if inner.dirty {
            self.persist(&mut inner);
        }
        *inner = Inner::default();
        file::forget_all();
    }

    // ------------------------------------------------------------ identities

    async fn fetch_bundles(&self, ids: &[i64]) -> Result<HashMap<i64, Bundle>, String> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let list = ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
        let v = self
            .api
            .call(&Req::get(format!("/e2e/keys?ids={list}")))
            .await
            .map_err(|e| e.message)?;
        let mut out = HashMap::new();
        for id in ids {
            if let Some(b) = v.get("keys").and_then(|k| k.get(id.to_string())) {
                match Bundle::parse(*id, b) {
                    Some(b) => {
                        out.insert(*id, b);
                    }
                    None if !b.is_null() => {
                        crate::logln!(
                            "e2e: as chaves de {id} vieram inválidas (assinatura não confere)"
                        );
                    }
                    None => {}
                }
            }
        }
        Ok(out)
    }

    /// Takes a bundle the server handed out for someone, against what this
    /// PC pinned for them.
    fn take_bundle(&self, inner: &mut Inner, b: Bundle) -> Trust {
        if !Self::listed(inner, &b) {
            crate::logln!(
                "e2e: a chave de {} não é a mais nova dele no registro público",
                b.uid
            );
            inner.alert = Some(Alert::Unlisted(b.uid));
            return Trust::Unlisted;
        }
        let Some(saved) = inner.saved.as_mut() else {
            return Trust::Changed;
        };
        let key = b.uid.to_string();
        let trust = match saved.pins.get_mut(&key) {
            None => {
                saved.pins.insert(
                    key,
                    Pin {
                        identity: b.identity,
                        verified: false,
                        changed: None,
                    },
                );
                inner.dirty = true;
                Trust::New
            }
            Some(pin) if pin.identity == b.identity => {
                if pin.changed.take().is_some() {
                    inner.dirty = true;
                }
                Trust::Same
            }
            Some(pin) => {
                if pin.changed != Some(b.identity) {
                    pin.changed = Some(b.identity);
                    inner.dirty = true;
                }
                Trust::Changed
            }
        };
        if trust != Trust::Changed {
            inner.bundles.insert(b.uid, b);
        }
        trust
    }

    pub fn pin_view(&self, uid: i64) -> PinView {
        let inner = self.lock();
        match inner
            .saved
            .as_ref()
            .and_then(|s| s.pins.get(&uid.to_string()))
        {
            None => PinView::Unknown,
            Some(p) if p.changed.is_some() => PinView::Changed,
            Some(p) => PinView::Pinned {
                verified: p.verified,
            },
        }
    }

    /// The security number between me and `uid`, with the identity the
    /// server hands out now when it differs from the pinned one.
    pub fn security_number(&self, uid: i64) -> Option<(String, Option<String>)> {
        let inner = self.lock();
        let me = inner.account.as_ref()?;
        let mine = (me.uid, &me.identity.public());
        let pin = inner.saved.as_ref()?.pins.get(&uid.to_string())?;
        let now = keys::security_number(mine, (uid, &pin.identity));
        let new = pin
            .changed
            .as_ref()
            .map(|c| keys::security_number(mine, (uid, c)));
        Some((now, new))
    }

    /// The person compared the number with `uid` and it matched.
    pub fn mark_verified(&self, uid: i64, verified: bool) {
        let mut inner = self.lock();
        if let Some(p) = inner
            .saved
            .as_mut()
            .and_then(|s| s.pins.get_mut(&uid.to_string()))
        {
            p.verified = verified && p.changed.is_none();
        }
        self.persist(&mut inner);
    }

    /// Accepts the new identity the server hands out for `uid`: the old one
    /// still checks what they signed with it. Not verified any more.
    pub fn accept_change(&self, uid: i64) {
        let mut inner = self.lock();
        let Some(saved) = inner.saved.as_mut() else {
            return;
        };
        let key = uid.to_string();
        let Some(pin) = saved.pins.get_mut(&key) else {
            return;
        };
        let Some(new) = pin.changed.take() else {
            return;
        };
        let old = std::mem::replace(&mut pin.identity, new);
        pin.verified = false;
        saved.older.entry(key).or_default().push(prim::b64(&old));
        // Sessions sealed for the old key go: the next message makes one
        // for the new key.
        saved
            .out
            .retain(|s| !s.readers.iter().any(|(r, _)| *r == uid));
        inner.bundles.remove(&uid);
        self.persist(&mut inner);
    }

    /// The identities that may have signed what `uid` sent: the pinned one
    /// first, then the ones accepted before it.
    fn identities(inner: &Inner, uid: i64) -> Vec<[u8; 32]> {
        let Some(saved) = inner.saved.as_ref() else {
            return Vec::new();
        };
        let key = uid.to_string();
        if Some(uid) == inner.account.as_ref().map(|a| a.uid) {
            return vec![
                inner
                    .account
                    .as_ref()
                    .map(|a| a.identity.public())
                    .unwrap_or_default(),
            ];
        }
        let mut out: Vec<[u8; 32]> = saved
            .pins
            .get(&key)
            .map(|p| p.identity)
            .into_iter()
            .collect();
        if let Some(older) = saved.older.get(&key) {
            out.extend(older.iter().rev().filter_map(|s| prim::unb64_n(s)));
        }
        out
    }

    // ------------------------------------------------------------ files

    /// Someone published other keys: the bundle kept for them goes, and
    /// the next one is checked against their pin.
    pub fn forget(&self, uid: i64) {
        self.lock().bundles.remove(&uid);
    }

    fn remember_file(url: &str, key: FileKey) {
        if !url.is_empty() {
            file::remember(url, key);
        }
    }

    // ------------------------------------------------------------ opening

    /// The session `sid` as this PC can read it: one of its own, or one it
    /// was sealed for.
    fn in_session(inner: &Inner, sid: &[u8; 16]) -> Option<InSession> {
        let saved = inner.saved.as_ref()?;
        if let Some(s) = saved.inb.get(&sid_key(sid)) {
            return Some(s.clone());
        }
        let me = inner.account.as_ref()?.uid;
        saved.out.iter().find(|s| &s.sid == sid).map(|s| InSession {
            key: s.key,
            cid: s.cid,
            sender: me,
            at: s.at,
            eph: s.ttl > 0,
        })
    }

    /// Opens `m` in place: its text, files, sticker, quote and reactions
    /// come from the envelope, checked. A message without one is left as
    /// the server sent it, marked as not encrypted.
    pub fn open(&self, m: &mut Message) -> Opened {
        let Some(env) = m.e2e.clone() else {
            // In plain text after the conversation went encrypted: not from
            // this app, so nothing it says is taken (no going back).
            let after = self
                .lock()
                .first_sealed
                .get(&m.channel_id)
                .is_some_and(|first| m.id > *first);
            if after && !m.pending {
                m.seal = Seal::Refused("sem criptografia numa conversa cifrada");
                m.content.clear();
                m.attachments.clear();
                m.sticker = None;
                m.reply = None;
                return Opened::Refused(Refused::Malformed);
            }
            m.seal = Seal::Plain;
            return Opened::Plain;
        };
        if m.seal == Seal::Open {
            return Opened::Open;
        }
        let mut inner = self.lock();
        let first = inner.first_sealed.entry(m.channel_id).or_insert(m.id);
        *first = (*first).min(m.id);
        if inner.account.is_none() {
            m.seal = Seal::Locked;
            return Opened::NoKeys;
        }
        let Some((sid, mid)) = msg::envelope_ids(&env) else {
            m.seal = Seal::Refused(crate::t!("e2e.refused_envelope"));
            return Opened::Refused(Refused::Malformed);
        };
        if *inner.seen.entry(mid).or_insert(m.id) != m.id {
            m.seal = Seal::Refused(crate::t!("e2e.refused_replay"));
            m.content.clear();
            m.attachments.clear();
            return Opened::Refused(Refused::Replayed);
        }
        drop(inner);
        let mut inner = self.lock();
        let author = m.author_id();
        let session = Self::in_session(&inner, &sid);
        let ids = Self::identities(&inner, author);
        let mut need = Need::default();
        if session.is_none() {
            need.sids.insert(sid);
        }
        if ids.is_empty() {
            need.users.insert(author);
        }
        for r in &m.reactions {
            for (u, _) in &r.data {
                if Self::identities(&inner, *u).is_empty() {
                    need.users.insert(*u);
                }
            }
        }
        let Some(session) = session.filter(|_| !ids.is_empty()) else {
            m.seal = Seal::Locked;
            return Opened::Need(need);
        };
        let mut last = Refused::NotSigned;
        let mut body = None;
        for id in &ids {
            match msg::open_message(&env, m.channel_id, author, &session, id) {
                Ok(b) => {
                    body = Some(b);
                    break;
                }
                Err(e) => last = e,
            }
        }
        let Some(body) = body else {
            m.seal = Seal::Refused(refused_text(&last));
            m.content.clear();
            m.attachments.clear();
            m.sticker = None;
            return Opened::Refused(last);
        };
        // Its time is up: it is gone, whatever the server still keeps.
        if body.x.is_some_and(|x| x <= now_ms()) {
            m.content.clear();
            m.attachments.clear();
            m.sticker = None;
            m.seal = Seal::Gone;
            return Opened::Expired;
        }
        // An edit says which message it edits: one put on another message
        // is refused.
        if let Some(ed) = &body.ed
            && ed.id != m.id
        {
            m.seal = Seal::Refused(crate::t!("e2e.refused_edit"));
            m.content.clear();
            m.attachments.clear();
            return Opened::Refused(Refused::WrongSession);
        }
        m.content = body.t.clone();
        m.sticker = body.st.clone();
        // The time the author signed (an edit keeps the message's own).
        if body.ed.is_none() {
            m.created = body.ts;
        }
        m.attachments = body
            .a
            .iter()
            .map(|f| {
                if let Some(key) = FileKey::from_parts(&f.fk, f.z.max(0) as u64) {
                    Self::remember_file(&f.u, key);
                }
                Attachment {
                    url: f.u.clone(),
                    name: f.n.clone(),
                    size: f.z,
                    kind: if f.d.is_some() {
                        Kind::Direct
                    } else {
                        Kind::from(f.k.clone())
                    },
                    direct: f.d.clone(),
                }
            })
            .collect();
        m.reply = body.r.as_ref().map(|q| crate::model::ReplyRef {
            author: None,
            author_id: q.u,
            content: q.x.clone(),
            sticker: q.st.clone(),
        });
        // Reactions: each person's must be theirs, for this message.
        let mut reactions = Vec::new();
        for r in std::mem::take(&mut m.reactions) {
            // As the server sends it, a reaction's "emoji" is its mark.
            let token = r.emoji;
            let mut emoji: Option<String> = None;
            let mut users = Vec::new();
            for (u, data) in &r.data {
                let opened = Self::identities(&inner, *u).iter().find_map(|id| {
                    msg::open_reaction(&session.key, m.channel_id, &env, *u, id, &token, data)
                });
                // Everyone in a group must have reacted with the same one.
                match (&emoji, opened) {
                    (None, Some(e)) => {
                        emoji = Some(e);
                        users.push(*u);
                    }
                    (Some(had), Some(e)) if *had == e => users.push(*u),
                    _ => {}
                }
            }
            if let Some(emoji) = emoji {
                reactions.push(crate::model::Reaction {
                    users,
                    emoji,
                    data: r.data,
                });
            }
        }
        m.reactions = reactions;
        // The author's copy of the record, against mine (one ahead is
        // compared once mine catches up: `record_behind`).
        if let Some(h) = &body.h {
            Self::compare_head(&mut inner, author, h);
        }
        m.body = Some(Box::new(body));
        m.seal = Seal::Open;
        Opened::Open
    }

    /// Fetches what messages need to open: session keys sealed for me, and
    /// the identities of their authors.
    pub async fn fetch(&self, need: Need) -> Result<(), String> {
        if !need.sids.is_empty() {
            let list = need.sids.iter().map(sid_key).collect::<Vec<_>>().join(",");
            let v = self
                .api
                .call(&Req::get(format!("/e2e/sessions?ids={list}")))
                .await
                .map_err(|e| e.message)?;
            let mut inner = self.lock();
            let Some(me) = inner.account.take() else {
                return Ok(());
            };
            for s in v
                .get("sessions")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let opened = (|| {
                    let sid: [u8; 16] = prim::unb64_n(s.get("sid")?.as_str()?)?;
                    if !need.sids.contains(&sid) {
                        return None;
                    }
                    let cid = s.get("cid")?.as_i64()?;
                    let sender = s.get("sender")?.as_i64()?;
                    let sealed = prim::unb64(s.get("key")?.as_str()?)?;
                    let key: [u8; 32] =
                        seal::open_from(&me, sender, &msg::session_context(cid, &sid), &sealed)?
                            .try_into()
                            .ok()?;
                    Some((
                        sid,
                        InSession {
                            key,
                            cid,
                            sender,
                            at: now_ms(),
                            eph: seal::key_id(&sealed).is_some_and(|id| me.is_eph(&id)),
                        },
                    ))
                })();
                if let Some((sid, session)) = opened
                    && let Some(saved) = inner.saved.as_mut()
                {
                    saved.inb.insert(sid_key(&sid), session);
                    inner.dirty = true;
                }
            }
            inner.account = Some(me);
            if inner.dirty {
                self.persist(&mut inner);
            }
        }
        if need.record {
            self.sync_record().await?;
        }
        let users: Vec<i64> = need.users.into_iter().collect();
        if !users.is_empty() {
            let bundles = self.fetch_bundles(&users).await?;
            self.ensure_listed(&bundles.values().collect::<Vec<_>>())
                .await;
            let mut inner = self.lock();
            for (_, b) in bundles {
                self.take_bundle(&mut inner, b);
            }
            if inner.dirty {
                self.persist(&mut inner);
            }
        }
        Ok(())
    }

    /// Whether some copy of the record that came in a message is ahead of
    /// this PC's (`Need::record` brings it up to date and compares).
    pub fn record_behind(&self) -> bool {
        !self.lock().ahead.is_empty()
    }

    // ------------------------------------------------------------ sending

    /// My session for `cid`, fit for whoever reads it now: the one I have,
    /// or a new one sealed for each of them. `allowed` is who may be a
    /// reader, as the app knows it ([`Allowed`]).
    async fn session_for(
        &self,
        cid: i64,
        allowed: Option<&Allowed>,
        ttl: i64,
    ) -> Result<(OutSession, Vec<i64>), String> {
        let v = self
            .api
            .call(&Req::get(format!("/channels/{cid}/e2e/readers")))
            .await
            .map_err(|e| e.message)?;
        let me = self.me().ok_or_else(no_keys_text)?;
        let today = keys::day_of(now_ms());
        let mut bundles = Vec::new();
        let mut missing = Vec::new();
        let mut ids = HashSet::new();
        // In a conversation whose messages disappear, each reader's key of
        // the day (signed by their identity): the session goes to it, so it
        // can no longer be opened once they drop it.
        let mut eph: HashMap<i64, Vec<u8>> = HashMap::new();
        for r in v
            .get("readers")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(uid) = r.get("id").and_then(Value::as_i64) else {
                continue;
            };
            ids.insert(uid);
            match r.get("keys").and_then(|k| Bundle::parse(uid, k)) {
                Some(b) => {
                    if ttl > 0 && uid != me {
                        match r
                            .get("eph")
                            .and_then(|e| keys::parse_eph(uid, &b.identity, e))
                        {
                            Some((day, kem))
                                if day <= today + 1 && day >= today - msg::SESSION_DAYS =>
                            {
                                eph.insert(uid, kem);
                            }
                            _ => {
                                missing.push(uid);
                                continue;
                            }
                        }
                    }
                    bundles.push(b);
                }
                None => missing.push(uid),
            }
        }
        if !ids.contains(&me) {
            return Err(crate::t!("e2e.not_reader").into());
        }
        // Every key must be its account's newest in the public record: one
        // that is not gets nothing (and the person hears of it).
        self.ensure_listed(&bundles.iter().filter(|b| b.uid != me).collect::<Vec<_>>())
            .await;
        {
            let mut inner = self.lock();
            let (listed, unlisted): (Vec<Bundle>, Vec<Bundle>) = bundles
                .into_iter()
                .partition(|b| b.uid == me || Self::listed(&inner, b));
            for b in &unlisted {
                crate::logln!(
                    "e2e: o servidor entregou para {} uma chave fora do registro público",
                    b.uid
                );
                inner.alert = Some(Alert::Unlisted(b.uid));
                missing.push(b.uid);
            }
            bundles = listed;
        }
        match allowed {
            Some(Allowed::Exactly(only)) if ids != *only => {
                return Err(
                    "O servidor disse que esta conversa tem outros leitores: nada foi enviado."
                        .into(),
                );
            }
            Some(Allowed::Within(members)) if !ids.is_subset(members) => {
                crate::logln!(
                    "e2e: o servidor conta {} leitor(es) fora da lista de membros na conversa {cid}: ficam sem a chave",
                    ids.difference(members).count()
                );
                bundles.retain(|b| members.contains(&b.uid));
                missing.retain(|u| members.contains(u));
            }
            _ => {}
        }
        let (session, sealed) = match self.plan_session(cid, me, bundles, ttl, &eph)? {
            Plan::Keep(s) => return Ok((s, missing)),
            Plan::New(s, sealed) => (s, sealed),
        };
        self.api
            .call(
                &Req::post(format!("/channels/{cid}/e2e/sessions"))
                    .with(json!({ "sid": sid_key(&session.sid), "keys": sealed })),
            )
            .await
            .map_err(|e| e.message)?;
        let mut inner = self.lock();
        if let Some(saved) = inner.saved.as_mut() {
            saved.out.retain(|s| s.cid != cid);
            saved.out.push(session.clone());
        }
        self.persist(&mut inner);
        Ok((session, missing))
    }

    /// The part of `session_for` that needs the keys: the readers' bundles
    /// checked against the pins, and the session that fits them (a new one
    /// with its key sealed for each, when mine does not).
    fn plan_session(
        &self,
        cid: i64,
        me: i64,
        bundles: Vec<Bundle>,
        ttl: i64,
        eph: &HashMap<i64, Vec<u8>>,
    ) -> Result<Plan, String> {
        let mut inner = self.lock();
        let mut readers = Vec::new();
        let mut changed = Vec::new();
        let mut usable = Vec::new();
        let today = keys::day_of(now_ms());
        for b in bundles {
            if b.uid == me {
                // Mine is the one this PC has.
                let ok = inner.account.as_ref().is_some_and(|a| {
                    a.identity.public() == b.identity && a.current().public == b.kem
                });
                if !ok {
                    return Err(
                        "O servidor entregou outra chave em meu nome: nada foi enviado.".into(),
                    );
                }
                // My other PCs: to today's key of mine too, when the
                // messages disappear (they make the same keys of the day).
                let b = match inner.account.as_ref().and_then(|a| a.eph_of(today)) {
                    Some(k) if ttl > 0 => Bundle {
                        kem: k.public.clone(),
                        ..b
                    },
                    _ if ttl > 0 => return Err(no_keys_text()),
                    _ => b,
                };
                readers.push((me, prim::b64(&b.kid())));
                usable.push(b);
                continue;
            }
            match self.take_bundle(&mut inner, b.clone()) {
                Trust::Changed => changed.push(b.uid),
                // Gets nothing (session_for left them out already).
                Trust::Unlisted => {}
                Trust::New | Trust::Same => {
                    // To their key of the day, when the messages disappear.
                    let b = match eph.get(&b.uid) {
                        Some(kem) if ttl > 0 => Bundle {
                            kem: kem.clone(),
                            ..b
                        },
                        _ if ttl > 0 => continue,
                        _ => b,
                    };
                    readers.push((b.uid, prim::b64(&b.kid())));
                    usable.push(b);
                }
            }
        }
        if inner.dirty {
            self.persist(&mut inner);
        }
        if !changed.is_empty() {
            return Err(changed_text(&changed));
        }
        readers.sort();
        let now = now_ms();
        let saved = inner.saved.as_ref().ok_or_else(no_keys_text)?;
        if let Some(s) = saved.out.iter().find(|s| s.cid == cid)
            && s.fits(&readers, now, ttl)
        {
            return Ok(Plan::Keep(s.clone()));
        }
        let session = OutSession {
            ttl,
            ..OutSession::new(cid, readers, now)
        };
        let account = inner.account.as_ref().ok_or_else(no_keys_text)?;
        let context = msg::session_context(cid, &session.sid);
        let mut sealed = serde_json::Map::new();
        for b in &usable {
            let s = seal::seal_to(account.uid, b, &context, &session.key)
                .ok_or("não deu para selar a chave da sessão")?;
            sealed.insert(b.uid.to_string(), json!(prim::b64(&s)));
        }
        Ok(Plan::New(session, sealed))
    }

    /// `body` sealed for conversation `cid`: the envelope to send, and the
    /// readers who will not be able to read it (no keys yet: an old app).
    pub async fn seal_body(
        &self,
        cid: i64,
        body: &Body,
        allowed: Option<&Allowed>,
        ttl: i64,
    ) -> Result<(Value, Vec<i64>), String> {
        let (session, missing) = self.session_for(cid, allowed, ttl).await?;
        let mut inner = self.lock();
        // My copy of the record goes in it, for the readers to compare; and,
        // in a conversation whose messages disappear, when this one does
        // (signed with the rest: the readers' apps drop it then).
        let body = Body {
            h: Self::my_head(&inner),
            // (An edit keeps the time of the message it edits.)
            x: body.x.or_else(|| (ttl > 0).then(|| now_ms() + ttl * 1000)),
            ..body.clone()
        };
        let account = inner.account.as_ref().ok_or_else(no_keys_text)?;
        let (env, _) = msg::seal_message(account, &session, &body);
        if let Some(s) = inner
            .saved
            .as_mut()
            .and_then(|s| s.out.iter_mut().find(|s| s.sid == session.sid))
        {
            s.sent += 1;
            inner.dirty = true;
        }
        Ok((env, missing))
    }

    /// My reaction `emoji` to `m` (opened): its mark, and what goes with it.
    pub fn reaction(&self, m: &Message, emoji: &str) -> Option<(String, String)> {
        let env = m.e2e.as_ref()?;
        let (sid, _) = msg::envelope_ids(env)?;
        let inner = self.lock();
        let session = Self::in_session(&inner, &sid)?;
        let account = inner.account.as_ref()?;
        let token = msg::reaction_token(&session.key, m.channel_id, env, emoji)?;
        let data = msg::seal_reaction(account, &session.key, m.channel_id, env, emoji)?;
        Some((token, data))
    }

    // ------------------------------------------------------------ signalling

    /// `data` signed and sealed for `to`, in `context` (a call, a transfer).
    pub async fn seal_signal(
        &self,
        to: i64,
        context: &[u8],
        data: &Value,
    ) -> Result<Value, String> {
        let bundle = {
            let inner = self.lock();
            inner.bundles.get(&to).cloned()
        };
        let bundle = match bundle {
            Some(b) => b,
            None => {
                let mut got = self.fetch_bundles(&[to]).await?;
                let b = got
                    .remove(&to)
                    .ok_or("a pessoa ainda não tem chaves (app antigo)")?;
                self.ensure_listed(&[&b]).await;
                let mut inner = self.lock();
                let trust = self.take_bundle(&mut inner, b.clone());
                if trust == Trust::Changed || trust == Trust::Unlisted {
                    if inner.dirty {
                        self.persist(&mut inner);
                    }
                    return Err(if trust == Trust::Unlisted {
                        "A chave dessa pessoa não está no registro público: nada foi enviado."
                            .into()
                    } else {
                        changed_text(&[to])
                    });
                }
                if inner.dirty {
                    self.persist(&mut inner);
                }
                b
            }
        };
        let inner = self.lock();
        // My copy of the record goes along, for them to compare.
        let body = json!({ "ts": now_ms(), "d": data, "h": Self::my_head(&inner) }).to_string();
        let me = inner.account.as_ref().ok_or_else(no_keys_text)?;
        let sealed =
            seal::seal_signed(me, &bundle, context, body.as_bytes()).ok_or("não deu para selar")?;
        Ok(json!({ "e2e": prim::b64(&sealed) }))
    }

    /// What `from` signalled in `context`: None when it is not sealed for me
    /// by them (or is stale). Fetches their identity first when this PC
    /// does not know it.
    pub async fn open_signal(&self, from: i64, context: &[u8], sealed: &Value) -> Option<Value> {
        let raw = prim::unb64(sealed.get("e2e")?.as_str()?)?;
        let known = !Self::identities(&self.lock(), from).is_empty();
        if !known {
            let _ = self
                .fetch(Need {
                    users: [from].into_iter().collect(),
                    ..Need::default()
                })
                .await;
        }
        let behind = {
            let mut inner = self.lock();
            let me = inner.account.as_ref()?;
            let body = Self::identities(&inner, from)
                .iter()
                .find_map(|id| seal::open_signed(me, from, id, context, &raw))?;
            let v: Value = serde_json::from_slice(&body).ok()?;
            // A signal from long ago is someone replaying it.
            let ts = v.get("ts")?.as_i64()?;
            if (now_ms() - ts).abs() > 10 * 60_000 {
                crate::logln!("e2e: sinal de {from} fora do horário recusado");
                return None;
            }
            // Their copy of the record, against mine.
            let behind = v
                .get("h")
                .cloned()
                .and_then(|h| serde_json::from_value::<record::Head>(h).ok())
                .is_some_and(|h| Self::compare_head(&mut inner, from, &h));
            (behind, v.get("d").cloned())
        };
        if behind.0
            && let Err(e) = self.sync_record().await
        {
            crate::logln!("e2e: não deu para ler o registro público: {e}");
        }
        behind.1
    }
}

fn refused_text(r: &Refused) -> &'static str {
    match r {
        Refused::Malformed => "envelope inválido",
        Refused::WrongSession => "chave de outra conversa ou de outra pessoa",
        Refused::Tampered => "conteúdo alterado no caminho",
        Refused::NotSigned => "assinatura não é do autor",
        Refused::Replayed => "repetida pelo servidor",
    }
}

fn changed_text(uids: &[i64]) -> String {
    let _ = uids;
    "A chave de segurança de alguém nesta conversa mudou. Confira e aceite a nova chave antes de enviar."
        .into()
}

/// The ids in a list of opened messages that still need keys, merged.
pub fn need_of(e2e: &E2e, list: &mut [Message]) -> Need {
    let mut need = Need::default();
    for m in list {
        if let Opened::Need(n) = e2e.open(m) {
            need.merge(n);
        }
    }
    need
}

/// The message `env` (an edit of `m`) would carry for `content`: the same
/// files, sticker and quote, the new text.
pub fn edit_body(m: &Message, content: &str) -> Option<Body> {
    let old = m.body.as_deref()?;
    let n = msg::envelope_ids(m.e2e.as_ref()?)?.1;
    Some(Body {
        ts: now_ms(),
        t: content.to_owned(),
        ed: Some(msg::EditOf {
            id: m.id,
            n: prim::b64(&n),
        }),
        ..old.clone()
    })
}
