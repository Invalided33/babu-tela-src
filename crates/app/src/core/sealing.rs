//! The client's side of the end-to-end encryption (e2e/): messages open as
//! they arrive, and leave sealed.
//!
//! A message whose keys this PC does not have yet shows as locked and opens
//! when they arrive (`Msg::Keys`); a fresh one is only announced then (its
//! sound, notification and unread count), since what it says, a mention for
//! one, is only known then.

use std::collections::HashSet;

use serde_json::{Value, json};

use super::{Core, ErrTo, Modal, Msg, Outgoing, Then};
use crate::e2e::msg::{Body, FileRef, Quote};
use crate::e2e::{self, Allowed, Need, Opened, State};
use crate::model::{Attachment, Kind, Message, Seal};
use crate::net::Req;

/// How much of the message answered a reply quotes.
const QUOTE_LEN: usize = 140;

impl Core {
    /// The account's keys, once signed in (READY).
    pub(super) fn start_e2e(&mut self) {
        let Some(uid) = self.me.as_ref().map(|u| u.id) else {
            return;
        };
        let e2e = self.e2e.clone();
        self.spawn(async move { Msg::E2e(e2e.start(uid).await) });
    }

    pub(super) fn e2e_state(&mut self, state: State) {
        match state {
            State::Ready => {
                self.provision = None;
                // The keys of the day (conversations whose messages
                // disappear), and the wait for the next.
                self.new_day();
                self.reopen_all(true);
                // An update is only taken once it is found in the public
                // record (update.rs), which this PC can read from now on.
                self.check_update(super::Check::Later);
                // The servers' members, signed (e2e/members.rs).
                let ids: Vec<i64> = self.servers.iter().map(|s| s.id).collect();
                for sid in ids {
                    self.members_check(sid);
                }
            }
            State::NeedsDevice => {
                self.ask_for_keys();
                self.open_modal(Modal::NewDevice);
            }
            State::Failed(e) => {
                crate::logln!("e2e: {e}");
                self.toast(crate::tf!("core.e2e_unavailable", e = e));
            }
            State::Off | State::Starting => {}
        }
    }

    /// Opens a message from the server: at once when this PC has what it
    /// needs, otherwise once the keys arrive.
    pub(super) fn open_msg(&mut self, m: &mut Message) {
        let opened = self.e2e.open(m);
        if matches!(opened, Opened::Open) {
            self.schedule_expiry(m);
        }
        match opened {
            Opened::Need(need) => self.fetch_keys(need),
            // The author's copy of the public record is ahead of mine:
            // mine catches up, and they are compared.
            Opened::Open if self.e2e.record_behind() => self.fetch_keys(Need {
                record: true,
                ..Need::default()
            }),
            // Kept in the log: someone (the server, say) handed out a
            // message that does not hold together.
            Opened::Refused(why) => {
                crate::logln!(
                    "e2e: mensagem {} de {} recusada: {why:?}",
                    m.id,
                    m.author_id()
                );
            }
            _ => {}
        }
        self.fill_quote(m);
    }

    /// Opens a page of messages, asking once for everything it lacks.
    pub(super) fn open_page(&mut self, list: &mut [Message]) {
        let mut need = e2e::need_of(&self.e2e, list);
        for m in list.iter_mut() {
            self.fill_quote(m);
            if m.seal == Seal::Open {
                self.schedule_expiry(m);
            }
        }
        need.record |= self.e2e.record_behind();
        self.fetch_keys(need);
    }

    fn fill_quote(&self, m: &mut Message) {
        if let Some(r) = &mut m.reply
            && r.author.is_none()
            && r.author_id != 0
        {
            r.author = Some(self.user(r.author_id));
        }
    }

    pub(super) fn fetch_keys(&mut self, need: Need) {
        if need.is_empty() {
            return;
        }
        let e2e = self.e2e.clone();
        self.spawn(async move {
            if let Err(e) = e2e.fetch(need).await {
                crate::logln!("e2e: buscando chaves: {e}");
            }
            Msg::Keys
        });
    }

    /// Keys arrived: what was locked is tried again. What still does not
    /// open has no key for this PC (sent before I could read the
    /// conversation, say), and says so. `again` asks for missing keys once
    /// more (the keys of this PC just came).
    pub(super) fn reopen_all(&mut self, again: bool) {
        let e2e = self.e2e.clone();
        let mut need = Need::default();
        let mut fresh = Vec::new();
        let mut expiring = Vec::new();
        let mut gone = Vec::new();
        for st in self.msgs.values_mut() {
            for m in &mut st.list {
                if m.e2e.is_none() || matches!(m.seal, Seal::Open | Seal::Refused(_)) {
                    continue;
                }
                match e2e.open(m) {
                    Opened::Open => {
                        if self.fresh_locked.remove(&m.id) {
                            fresh.push(m.clone());
                        }
                        if m.body.as_ref().is_some_and(|b| b.x.is_some()) {
                            expiring.push(m.clone());
                        }
                    }
                    Opened::Need(n) if again => need.merge(n),
                    Opened::Need(_) => m.seal = Seal::Missing,
                    // Its time came while it was locked.
                    Opened::Expired => gone.push((m.channel_id, m.id)),
                    _ => {}
                }
            }
        }
        for s in &mut self.servers {
            for r in &mut s.reports {
                if r.message.e2e.is_some() && !matches!(r.message.seal, Seal::Open) {
                    e2e.open(&mut r.message);
                }
            }
        }
        let users = self.users.clone();
        for st in self.msgs.values_mut() {
            for m in &mut st.list {
                if let Some(r) = &mut m.reply
                    && r.author.is_none()
                    && let Some(u) = users.get(&r.author_id)
                {
                    r.author = Some(u.clone());
                }
            }
        }
        for (cid, id) in gone {
            self.message_gone(cid, id);
        }
        for m in fresh {
            self.alert(&m);
        }
        for m in expiring {
            self.schedule_expiry(&m);
        }
        need.record |= self.e2e.record_behind();
        self.fetch_keys(need);
    }

    /// Who may read conversation `cid`, as this app knows it, so the
    /// server is not taken at its word (e2e::Allowed): exactly the two
    /// people of a direct one, or at most the members of the channel's
    /// server, as its member list shows them.
    pub(super) fn known_readers(&self, cid: i64) -> Option<Allowed> {
        if let Some(dm) = self.dms.iter().find(|d| d.id == cid) {
            return Some(Allowed::Exactly(
                [self.me_id(), dm.user.id].into_iter().collect(),
            ));
        }
        let server = self
            .servers
            .iter()
            .find(|s| s.channels.iter().any(|c| c.id == cid))?;
        // Once its members sign who is in (e2e/members.rs), only those
        // their signatures reach; before that, its list.
        let v = self.e2e.vouched(server);
        let mut members: HashSet<i64> = if v.strict {
            v.reached
        } else {
            server.members.iter().map(|u| u.id).collect()
        };
        members.insert(self.me_id());
        Some(Allowed::Within(members))
    }

    /// The members' certificates of server `sid` (e2e/members.rs), once it
    /// arrives: the identities they need are fetched; and, as its owner,
    /// this PC signs its own certificate, the members there were before
    /// certificates (once: from then on a member signs each one in), and
    /// those someone else let in, so their entry outlives whoever did.
    pub(super) fn members_check(&mut self, sid: i64) {
        if self.e2e.state() != State::Ready {
            return;
        }
        let Some(s) = self.server(sid) else {
            return;
        };
        let v = self.e2e.vouched(s);
        let me = self.me_id();
        let mut certs = Vec::new();
        if self.e2e.owner(s) == me {
            if !v.reached.contains(&me) {
                certs.extend(self.e2e.sign_member("dono", sid, me));
            }
            for m in s.members.iter().filter(|m| m.id != me) {
                let vouch = if v.strict {
                    v.reached.contains(&m.id) && !v.by_owner.contains(&m.id)
                } else {
                    true
                };
                if vouch {
                    certs.extend(self.e2e.sign_member("garantia", sid, m.id));
                }
            }
        }
        self.fetch_keys(v.need);
        if !certs.is_empty() {
            crate::logln!(
                "e2e: assinando {} certificado(s) de membro do servidor {sid}",
                certs.len()
            );
            self.call(
                Req::post(format!("/servers/{sid}/e2e/certs")).with(json!({ "certs": certs })),
                Then::Nothing,
                ErrTo::Toast,
            );
        }
    }

    /// How long the messages of conversation `cid` last (s); 0 for good.
    pub fn ttl_of(&self, cid: i64) -> i64 {
        if let Some(d) = self.dms.iter().find(|d| d.id == cid) {
            return d.ttl;
        }
        self.servers
            .iter()
            .find_map(|s| s.channel(cid))
            .map_or(0, |c| c.ttl)
    }

    /// The day's round of the keys of conversations whose messages
    /// disappear (e2e/mod.rs `day`), now and at the start of every next
    /// day (UTC): nothing runs in between. A new round makes the wait of
    /// the one before it moot (`Msg::NewDay`).
    pub(super) fn new_day(&mut self) {
        self.day_round += 1;
        let round = self.day_round;
        let now = chrono::Utc::now().timestamp_millis();
        self.day_done = e2e::keys::day_of(now);
        let e2e = self.e2e.clone();
        self.spawn(async move {
            if let Err(e) = e2e.day().await {
                crate::logln!("e2e: as chaves do dia: {e}");
            }
            // A minute into the next day, so clocks a little apart agree.
            let now = chrono::Utc::now().timestamp_millis();
            let next = (e2e::keys::day_of(now) + 1) * 86_400_000 + 60_000;
            tokio::time::sleep(std::time::Duration::from_millis((next - now).max(0) as u64)).await;
            Msg::NewDay(round)
        });
    }

    /// The round, if the day turned since the last one (the wait for it
    /// may have slept with the PC).
    pub(super) fn new_day_if_due(&mut self) {
        if e2e::keys::day_of(chrono::Utc::now().timestamp_millis()) != self.day_done {
            self.new_day();
        }
    }

    /// A message of a conversation whose messages disappear goes from the
    /// screen at its time (the server deletes it too, in its round).
    pub(super) fn schedule_expiry(&mut self, m: &Message) {
        let Some(x) = m.body.as_ref().and_then(|b| b.x) else {
            return;
        };
        if self.expiring.insert(m.id) {
            self.expire_at(m.channel_id, m.id, x);
        }
    }

    fn expire_at(&mut self, cid: i64, id: i64, x: i64) {
        let wait = (x - chrono::Utc::now().timestamp_millis()).max(0) as u64;
        self.spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
            Msg::Expire { cid, id }
        });
    }

    /// The wait for message `id` is over: it goes, unless the clock says
    /// otherwise (it moved meanwhile), when it waits again.
    pub(super) fn expire(&mut self, cid: i64, id: i64) {
        let x = self
            .msgs
            .get(&cid)
            .and_then(|st| st.list.iter().find(|m| m.id == id))
            .and_then(|m| m.body.as_ref()?.x);
        match x {
            Some(x) if x > chrono::Utc::now().timestamp_millis() => self.expire_at(cid, id, x),
            Some(_) => {
                self.expiring.remove(&id);
                self.message_gone(cid, id);
            }
            // Not on screen now: when it comes again, it is opened (and
            // dropped, its time being up) like any other.
            None => {
                self.expiring.remove(&id);
            }
        }
    }

    /// Sets how long the messages of `cid` last (`ttl` seconds; 0 for good).
    pub fn set_ttl(&mut self, cid: i64, ttl: i64) {
        self.modal = None;
        self.call(
            Req::put(format!("/channels/{cid}/ttl")).with(json!({ "ttl": ttl })),
            Then::Toast(if ttl > 0 {
                crate::t!("core.ttl_warn").into()
            } else {
                "As mensagens novas desta conversa ficam.".into()
            }),
            ErrTo::Toast,
        );
    }

    /// Whether `uid` gets the keys of `sid`'s conversations: always before
    /// certificates; after, only if they reach them.
    pub fn member_vouched(&self, sid: i64, uid: i64) -> bool {
        self.server(sid).is_none_or(|s| {
            let v = self.e2e.vouched(s);
            !v.strict || v.reached.contains(&uid)
        })
    }

    /// The quote of the message being answered, as the reply carries it.
    pub(super) fn quote_of(m: &Message) -> Quote {
        Quote {
            id: m.id,
            u: m.author_id(),
            x: m.content.chars().take(QUOTE_LEN).collect(),
            st: m.sticker.clone(),
        }
    }

    /// Seals a message that has no envelope yet, then sends it.
    pub(super) fn seal_out(&mut self, out: Outgoing) {
        let e2e = self.e2e.clone();
        let only = self.known_readers(out.cid);
        let body = Body {
            ts: chrono::Utc::now().timestamp_millis(),
            t: out.content.clone(),
            st: out.sticker.clone(),
            r: out.quote.clone(),
            ..Body::default()
        };
        let (send, cid) = (out.send, out.cid);
        let ttl = self.ttl_of(cid);
        self.sealing.insert(send, out);
        self.spawn(async move {
            match e2e.seal_body(cid, &body, only.as_ref(), ttl).await {
                Ok((env, missing)) => Msg::Sealed { send, env, missing },
                Err(error) => Msg::SealFailed { send, error },
            }
        });
    }

    pub(super) fn sealed(&mut self, send: u64, env: Value, missing: Vec<i64>) {
        let Some(mut out) = self.sealing.remove(&send) else {
            return;
        };
        out.env = Some(env);
        self.warn_missing(&missing);
        self.dispatch(out);
    }

    pub(super) fn seal_failed(&mut self, send: u64, error: String) {
        if let Some(out) = self.sealing.remove(&send) {
            self.unsent(out.send, out.cid, out.content, Vec::new(), error);
        }
    }

    /// Readers whose app has no keys yet do not get what was sent: said
    /// once per person.
    pub(super) fn warn_missing(&mut self, missing: &[i64]) {
        let new: Vec<i64> = missing
            .iter()
            .copied()
            .filter(|u| self.warned_old.insert(*u))
            .collect();
        let names: Vec<String> = new
            .iter()
            .map(|u| self.user(*u).name().to_owned())
            .collect();
        if !names.is_empty() {
            self.toast(format!(
                "{} não vai conseguir ler: o app ainda não tem as chaves para isso (uma versão \
                 antiga ou, numa conversa de mensagens temporárias, mais de uma semana fechado).",
                names.join(", ")
            ));
        }
    }

    /// The files of a message as its envelope lists them, and what the
    /// server keeps of each (which upload, which direct file).
    pub(super) fn file_refs(
        attachments: &[(Attachment, Option<e2e::FileKey>)],
    ) -> (Vec<FileRef>, Vec<Value>) {
        let mut refs = Vec::new();
        let mut server = Vec::new();
        for (a, key) in attachments {
            let kind = match a.kind {
                Kind::Image => "image",
                Kind::Video => "video",
                Kind::Audio => "audio",
                Kind::Direct => "direct",
                Kind::File => "file",
            };
            refs.push(FileRef {
                u: a.url.clone(),
                n: a.name.clone(),
                z: a.size,
                k: kind.to_owned(),
                fk: key.as_ref().map(e2e::FileKey::encoded).unwrap_or_default(),
                d: a.direct.clone(),
            });
            match &a.direct {
                Some(d) => server.push(json!({ "direct": d.id })),
                None => server.push(json!({ "url": a.url })),
            }
        }
        (refs, server)
    }

    /// Enter in the edit box: the new text, sealed as an edit of that
    /// message.
    pub(super) fn seal_edit(&mut self, id: i64, content: String) {
        let Some(m) = self.find_msg(id).cloned() else {
            return;
        };
        let Some(body) = e2e::edit_body(&m, &content) else {
            self.toast(crate::t!("core.only_encrypted_edit"));
            return;
        };
        let e2e = self.e2e.clone();
        let only = self.known_readers(m.channel_id);
        let api = self.api.clone();
        let ttl = self.ttl_of(m.channel_id);
        self.spawn(async move {
            let res = match e2e.seal_body(m.channel_id, &body, only.as_ref(), ttl).await {
                Ok((env, _)) => {
                    api.call(&Req::patch(format!("/messages/{id}")).with(json!({ "e2e": env })))
                        .await
                }
                Err(e) => Err(crate::net::ApiError::msg(e)),
            };
            Msg::Done {
                then: Then::Nothing,
                err: ErrTo::Toast,
                res,
            }
        });
    }

    /// A reaction to `m`: its mark in the path, the sealed emoji in the body.
    pub(super) fn react_sealed(&mut self, m: &Message, emoji: &str, mine: bool) {
        let Some((token, data)) = self.e2e.reaction(m, emoji) else {
            self.toast(crate::t!("core.react_failed"));
            return;
        };
        let path = format!("/messages/{}/reactions/{token}", m.id);
        let req = if mine {
            Req::delete(path)
        } else {
            Req::put(path).with(json!({ "d": data }))
        };
        self.call(req, Then::Nothing, ErrTo::Toast);
    }

    /// A message's file opened in its program: it comes down to a folder of
    /// the app's in %TEMP%, decrypted on the way (net.rs `save_to`).
    pub fn open_attachment(&mut self, a: Attachment) {
        let api = self.api.clone();
        self.toast(format!("Abrindo {}…", a.name));
        self.spawn(async move {
            let dir = std::env::temp_dir().join("babutela-arquivos");
            let _ = tokio::fs::create_dir_all(&dir).await;
            // Only the name's last part, and nothing Windows refuses.
            let name: String = a
                .name
                .rsplit(['/', '\\'])
                .next()
                .unwrap_or("arquivo")
                .chars()
                .map(|c| {
                    if "<>:\"|?*".contains(c) || c.is_control() {
                        '_'
                    } else {
                        c
                    }
                })
                .collect();
            let path = dir.join(if name.trim().is_empty() {
                "arquivo"
            } else {
                &name
            });
            match api.save_to(&a.url, &path).await {
                Ok(()) => match open::that_detached(&path) {
                    Ok(()) => Msg::Toast(String::new()),
                    Err(e) => Msg::Toast(crate::tf!("core.open_failed", name = a.name, e = e)),
                },
                Err(e) => Msg::Toast(crate::tf!("core.download_failed", name = a.name, e = e)),
            }
        });
    }

    // ------------------------------------------------------------ another PC

    /// This PC has no keys and the account has some, on another PC: it asks
    /// for them (and shows the code to type there).
    pub(super) fn ask_for_keys(&mut self) {
        let req = self
            .provision
            .get_or_insert_with(e2e::provision::Request::new);
        let pk = req.public_b64();
        self.gw
            .send(&json!({ "op": "provision", "data": { "pk": pk } }));
    }

    /// Something from another PC of my account.
    pub(super) fn provision_event(&mut self, d: Value) {
        let from = d.get("from").and_then(Value::as_u64).unwrap_or(0);
        let data = d.get("data").cloned().unwrap_or(Value::Null);
        // A new PC asking: the person types its code here.
        if let Some(pk) = data.get("pk").and_then(Value::as_str) {
            if self.e2e.state() == State::Ready {
                self.open_modal(Modal::ApproveDevice {
                    conn: from,
                    pk: pk.to_owned(),
                    code: String::new(),
                    error: None,
                });
            }
            return;
        }
        // The keys, from an old PC.
        if let (Some(sealed), Some(req), Some(uid)) = (
            data.get("keys").and_then(Value::as_str),
            self.provision.as_ref(),
            self.me.as_ref().map(|u| u.id),
        ) {
            match req.open(uid, sealed) {
                Some(saved) => {
                    self.e2e.adopt(saved);
                    self.provision = None;
                    self.toast("Este PC recebeu as chaves da sua conta.");
                    self.e2e_state(State::Ready);
                    // This PC's copy of the record, compared with the old
                    // PC's as it catches up.
                    self.fetch_keys(Need {
                        record: true,
                        ..Need::default()
                    });
                }
                None => crate::logln!("e2e: chaves de outro PC que não abrem aqui"),
            }
        }
    }

    /// "Aprovar" on the old PC: the code typed must be the new PC's.
    pub fn approve_device(&mut self) {
        let Some(Modal::ApproveDevice {
            conn,
            pk,
            code,
            error,
        }) = &mut self.modal
        else {
            return;
        };
        let Some(uid) = self.e2e.me() else {
            *error = Some(e2e::no_keys_text());
            return;
        };
        let sealed = self
            .e2e
            .export()
            .and_then(|export| e2e::provision::approve(uid, &export, pk, code));
        match sealed {
            Ok(sealed) => {
                let to = *conn;
                self.gw.send(&json!({
                    "op": "provision", "to": to, "data": { "keys": sealed },
                }));
                self.modal = None;
                self.toast("Chaves enviadas para o PC novo.");
            }
            Err(e) => *error = Some(e),
        }
    }

    /// "Começar do zero" on a new PC: a new identity, everyone is told it
    /// changed, and what was sent before cannot be read here.
    pub fn reset_identity(&mut self) {
        let Some(uid) = self.me.as_ref().map(|u| u.id) else {
            return;
        };
        let e2e = self.e2e.clone();
        self.provision = None;
        self.spawn(async move { Msg::E2e(e2e.create(uid).await) });
    }

    /// The security number with `uid`, for the dialog.
    pub fn security_number(&self, uid: i64) -> Option<(String, Option<String>)> {
        self.e2e.security_number(uid)
    }

    pub fn mark_verified(&mut self, uid: i64, verified: bool) {
        self.e2e.mark_verified(uid, verified);
    }

    /// "Aceitar a nova chave": what they signed with it opens now.
    pub fn accept_key_change(&mut self, uid: i64) {
        self.e2e.accept_change(uid);
        for st in self.msgs.values_mut() {
            for m in &mut st.list {
                if m.author_id() == uid && matches!(m.seal, Seal::Refused(_) | Seal::Missing) {
                    m.seal = Seal::Locked;
                }
            }
        }
        self.reopen_all(true);
    }

    /// "Pedir de novo" on a new PC: the request goes again to the account's
    /// other PCs (one may have opened since).
    pub fn ask_for_keys_again(&mut self) {
        self.ask_for_keys();
        self.toast("Pedido enviado aos seus outros PCs.");
    }
}
