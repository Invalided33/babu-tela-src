//! Who is a member of a server, as its members sign it: the server keeps
//! the list, but an account it adds on its own gets no key, because no
//! member signed it in.
//!
//! Each member carries certificates (`Cert`), each signed by someone who
//! was a member:
//!
//! - "dono": the owner's own, which roots the server;
//! - "convite": an invite's. Whoever creates one picks 32 random bytes, which
//!   only the link carries (`link`), and signs the server, the code and the
//!   public half of a key made from them (`InviteCert`); whoever comes in
//!   with the link signs their entry with that key. The server sees the
//!   code, never the bytes, so it cannot use the invite itself;
//! - "aceito": the approver's, on a private server's request;
//! - "garantia": a member vouching for someone (the owner, for the members
//!   there were before certificates, and for those whose inviter left;
//!   anyone, by hand, for someone who came in with an old invite).
//!
//! `vouched` follows the signatures from the owner on: only those it
//! reaches read the server's encrypted conversations from then on. The
//! owner is pinned (mod.rs, `Saved::owners`): from the signed invite one
//! came in with, or else the first time this PC sees the server, as
//! identities are; an owner the server names later roots nothing. What it
//! cannot do: tell the owner apart from an account the server made one, on
//! that first sight without an invite, or see someone the server should
//! have removed (a removal is not signed).

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use super::prim::{self, Identity};

const DS_MEMBER: &[u8] = b"babutela/e2e/v1/membro";
const DS_INVITE: &[u8] = b"babutela/e2e/v1/convite";
const DS_ENTRY: &[u8] = b"babutela/e2e/v1/entrada";
const DS_INVITE_KEY: &[u8] = b"babutela/e2e/v1/chave-convite";

/// A member's certificate, as the server keeps and hands it out.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Cert {
    /// The server, the member, and who signed them in.
    pub s: i64,
    pub u: i64,
    pub by: i64,
    /// When (ms).
    pub t: i64,
    /// "dono", "convite", "aceito" or "garantia".
    pub how: String,
    /// `by`'s identity's signature; for an invite, the invite key's.
    pub sig: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inv: Option<InviteCert>,
}

/// An invite, signed by whoever created it.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct InviteCert {
    pub s: i64,
    pub code: String,
    pub by: i64,
    pub t: i64,
    /// The public half of the key the link's secret makes.
    pub ik: String,
    /// The server's owner, as its creator's app knows them: whoever comes
    /// in takes the owner from here, not from the server.
    #[serde(default)]
    pub o: i64,
    pub sig: String,
}

fn statement(how: &str, s: i64, u: i64, by: i64, t: i64) -> Vec<u8> {
    [
        DS_MEMBER,
        &(how.len() as u32).to_be_bytes(),
        how.as_bytes(),
        &s.to_be_bytes(),
        &u.to_be_bytes(),
        &by.to_be_bytes(),
        &t.to_be_bytes(),
    ]
    .concat()
}

fn invite_statement(inv: &InviteCert, ik: &[u8; 32]) -> Vec<u8> {
    [
        DS_INVITE,
        &inv.s.to_be_bytes(),
        &(inv.code.len() as u32).to_be_bytes(),
        inv.code.as_bytes(),
        &inv.by.to_be_bytes(),
        &inv.t.to_be_bytes(),
        ik,
        &inv.o.to_be_bytes(),
    ]
    .concat()
}

fn entry_statement(s: i64, code: &str, u: i64, t: i64) -> Vec<u8> {
    [
        DS_ENTRY,
        &s.to_be_bytes(),
        &(code.len() as u32).to_be_bytes(),
        code.as_bytes(),
        &u.to_be_bytes(),
        &t.to_be_bytes(),
    ]
    .concat()
}

/// The key a link's secret makes.
fn invite_key(secret: &[u8; 32]) -> Identity {
    Identity::from_seed(prim::key32(&[], secret, &[DS_INVITE_KEY]))
}

/// An invite made here.
pub struct NewInvite {
    pub code: String,
    pub secret: [u8; 32],
    pub cert: InviteCert,
}

/// A new invite to server `s`, whose owner is `owner`, signed by `me`
/// (`uid`).
pub fn new_invite(me: &Identity, uid: i64, s: i64, owner: i64, now: i64) -> NewInvite {
    let code = prim::b64(&prim::random::<6>());
    let secret: [u8; 32] = prim::random();
    let ik = invite_key(&secret).public();
    let mut cert = InviteCert {
        s,
        code: code.clone(),
        by: uid,
        t: now,
        ik: prim::b64(&ik),
        o: owner,
        sig: String::new(),
    };
    cert.sig = prim::b64(&me.sign(&invite_statement(&cert, &ik)));
    NewInvite { cert, code, secret }
}

/// Whether invite `inv` is the one the link with `secret` was made for,
/// signed by its creator (whose identities this PC knows as `ids`): then
/// the owner it names is the one its creator's app knew, whatever the
/// server says.
pub fn invite_holds(inv: &InviteCert, secret: &[u8; 32], ids: &[[u8; 32]]) -> bool {
    let ik = invite_key(secret).public();
    let Some(sig) = prim::unb64(&inv.sig) else {
        return false;
    };
    prim::b64(&ik) == inv.ik
        && ids
            .iter()
            .any(|id| prim::verify(id, &invite_statement(inv, &ik), &sig))
}

/// What people share: the code, then the secret (which never goes to the
/// server).
pub fn link(code: &str, secret: &[u8; 32]) -> String {
    format!("{code}.{}", prim::b64(secret))
}

/// What someone pasted: the code for the server, and the secret if the
/// link had one (an older app's invite is the code alone).
pub fn parse_link(typed: &str) -> (String, Option<[u8; 32]>) {
    let typed = typed.trim();
    let last = typed.rsplit('/').next().unwrap_or(typed);
    match last.split_once('.') {
        Some((code, secret)) => (code.to_owned(), prim::unb64_n(secret.trim())),
        None => (last.to_owned(), None),
    }
}

/// My entry through invite `inv` (the server gave it back as I joined),
/// signed with the key the link's secret makes: None when the link is not
/// this invite's.
pub fn entry(secret: &[u8; 32], inv: &InviteCert, me: i64, now: i64) -> Option<Cert> {
    let ik = invite_key(secret);
    if prim::b64(&ik.public()) != inv.ik {
        return None;
    }
    let sig = ik.sign(&entry_statement(inv.s, &inv.code, me, now));
    Some(Cert {
        s: inv.s,
        u: me,
        by: inv.by,
        t: now,
        how: "convite".into(),
        sig: prim::b64(&sig),
        inv: Some(inv.clone()),
    })
}

/// A certificate `me` (`by`) signs: mine as the owner ("dono", for myself),
/// someone I let in ("aceito"), someone I vouch for ("garantia").
pub fn signed(me: &Identity, by: i64, how: &str, s: i64, u: i64, now: i64) -> Cert {
    let sig = me.sign(&statement(how, s, u, by, now));
    Cert {
        s,
        u,
        by,
        t: now,
        how: how.into(),
        sig: prim::b64(&sig),
        inv: None,
    }
}

/// Whether `c` says that `u` is in `s`, signed by its signer, whose
/// identities this PC knows by `ids` (none known: it does not hold yet).
pub fn holds(c: &Cert, s: i64, u: i64, ids: &dyn Fn(i64) -> Vec<[u8; 32]>) -> bool {
    if c.s != s || c.u != u {
        return false;
    }
    let Some(sig) = prim::unb64(&c.sig) else {
        return false;
    };
    let by_signer = |msg: &[u8], sig: &[u8]| ids(c.by).iter().any(|id| prim::verify(id, msg, sig));
    match c.how.as_str() {
        "dono" => c.by == u && by_signer(&statement("dono", s, u, c.by, c.t), &sig),
        "aceito" | "garantia" => by_signer(&statement(&c.how, s, u, c.by, c.t), &sig),
        "convite" => {
            let Some(inv) = &c.inv else {
                return false;
            };
            let (Some(ik), Some(isig)) = (prim::unb64_n::<32>(&inv.ik), prim::unb64(&inv.sig))
            else {
                return false;
            };
            inv.s == s
                && inv.by == c.by
                && by_signer(&invite_statement(inv, &ik), &isig)
                && prim::verify(&ik, &entry_statement(s, &inv.code, u, c.t), &sig)
        }
        _ => false,
    }
}

/// The members of `s` the signatures reach, from its owner on: the owner
/// by their own certificate, then everyone signed in by someone already
/// reached. `members` are the server's, with their certificates.
pub fn vouched(
    s: i64,
    owner: i64,
    members: &[(i64, Vec<Cert>)],
    ids: &dyn Fn(i64) -> Vec<[u8; 32]>,
) -> HashSet<i64> {
    let mut ok = HashSet::new();
    let rooted = members.iter().any(|(u, certs)| {
        *u == owner
            && certs
                .iter()
                .any(|c| c.how == "dono" && holds(c, s, owner, ids))
    });
    if !rooted {
        return ok;
    }
    ok.insert(owner);
    loop {
        let before = ok.len();
        for (u, certs) in members {
            if !ok.contains(u)
                && certs
                    .iter()
                    .any(|c| c.how != "dono" && ok.contains(&c.by) && holds(c, s, *u, ids))
            {
                ok.insert(*u);
            }
        }
        if ok.len() == before {
            return ok;
        }
    }
}
