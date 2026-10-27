//! The shapes the server sends and receives, mirroring
//! crates/worker/src/model.rs.
//!
//! Every field is `#[serde(default)]`: an older app talking to a newer server
//! should show what it understands instead of refusing to start.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------- permissions

pub const P_VIEW: i64 = 1;
pub const P_SEND: i64 = 2;
pub const P_MANAGE_MESSAGES: i64 = 4;
pub const P_MANAGE_CHANNELS: i64 = 8;
pub const P_MANAGE_ROLES: i64 = 16;
pub const P_KICK: i64 = 32;
pub const P_BAN: i64 = 64;
pub const P_ADMIN: i64 = 128;
pub const P_INVITE: i64 = 256;
pub const P_ATTACH: i64 = 512;
pub const P_CONNECT: i64 = 1024;
pub const P_MANAGE_SERVER: i64 = 2048;

/// Bit, name and explanation, in the order the role editor lists them.
pub const PERM_LABELS: &[(i64, &str, &str)] = &[
    (P_ADMIN, "Administrador", "Concede todas as permissões"),
    (P_MANAGE_SERVER, "Gerenciar servidor", "Renomear o servidor"),
    (
        P_MANAGE_CHANNELS,
        "Gerenciar canais",
        "Criar, editar e excluir canais",
    ),
    (
        P_MANAGE_ROLES,
        "Gerenciar cargos",
        "Criar cargos e atribuí-los a membros",
    ),
    (
        P_MANAGE_MESSAGES,
        "Gerenciar mensagens",
        "Apagar mensagens de outras pessoas",
    ),
    (
        P_KICK,
        "Expulsar membros",
        "E aceitar ou recusar os pedidos de entrada de um servidor privado",
    ),
    (P_BAN, "Banir membros", ""),
    (P_INVITE, "Criar convites", ""),
    (P_VIEW, "Ver canais", ""),
    (P_SEND, "Enviar mensagens", ""),
    (P_ATTACH, "Anexar arquivos", ""),
    (P_CONNECT, "Conectar aos canais de voz", ""),
];

// ---------------------------------------------------------------- enums
// All of these accept anything: a value the app does not know becomes the
// safest option instead of a parse error.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Online,
    Idle,
    Dnd,
    Invisible,
    #[default]
    Offline,
}

impl Status {
    /// What the status picker shows.
    pub const PICKABLE: [Status; 4] =
        [Status::Online, Status::Idle, Status::Dnd, Status::Invisible];

    pub fn label(self) -> &'static str {
        match self {
            Status::Online => "Disponível",
            Status::Idle => "Ausente",
            Status::Dnd => "Não perturbe",
            Status::Invisible => "Invisível",
            Status::Offline => "Offline",
        }
    }

    pub fn wire(self) -> &'static str {
        match self {
            Status::Online => "online",
            Status::Idle => "idle",
            Status::Dnd => "dnd",
            Status::Invisible => "invisible",
            Status::Offline => "offline",
        }
    }

    /// What other people see: "invisible" is not something to show around.
    pub fn public(self) -> Status {
        if self == Status::Invisible {
            Status::Offline
        } else {
            self
        }
    }
}

impl From<String> for Status {
    fn from(s: String) -> Status {
        match s.as_str() {
            "online" => Status::Online,
            "idle" => Status::Idle,
            "dnd" => Status::Dnd,
            "invisible" => Status::Invisible,
            _ => Status::Offline,
        }
    }
}

impl<'de> Deserialize<'de> for Status {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Status, D::Error> {
        Ok(Option::<String>::deserialize(d)?.unwrap_or_default().into())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ChannelKind {
    #[default]
    Text,
    Voice,
    Dm,
}

impl From<String> for ChannelKind {
    fn from(s: String) -> ChannelKind {
        match s.as_str() {
            "voice" => ChannelKind::Voice,
            "dm" => ChannelKind::Dm,
            _ => ChannelKind::Text,
        }
    }
}

impl<'de> Deserialize<'de> for ChannelKind {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<ChannelKind, D::Error> {
        Ok(Option::<String>::deserialize(d)?.unwrap_or_default().into())
    }
}

impl ChannelKind {
    pub fn wire(self) -> &'static str {
        match self {
            ChannelKind::Text => "text",
            ChannelKind::Voice => "voice",
            ChannelKind::Dm => "dm",
        }
    }
}

/// What someone is sharing: nothing, their camera or their screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Video {
    Camera,
    Screen,
}

impl Video {
    pub fn wire(self) -> &'static str {
        match self {
            Video::Camera => "camera",
            Video::Screen => "screen",
        }
    }
}

/// `null`, "camera" or "screen"; anything else is `null`.
pub fn video_from(s: Option<String>) -> Option<Video> {
    match s.as_deref() {
        Some("camera") => Some(Video::Camera),
        Some("screen") => Some(Video::Screen),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Image,
    Video,
    Audio,
    #[default]
    File,
    /// Sent directly: not on the server, downloaded from the sender's app
    /// while both are online (direct/).
    Direct,
}

impl From<String> for Kind {
    fn from(s: String) -> Kind {
        match s.as_str() {
            "image" => Kind::Image,
            "video" => Kind::Video,
            "audio" => Kind::Audio,
            "direct" => Kind::Direct,
            _ => Kind::File,
        }
    }
}

impl<'de> Deserialize<'de> for Kind {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Kind, D::Error> {
        Ok(Option::<String>::deserialize(d)?.unwrap_or_default().into())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FriendState {
    Accepted,
    Incoming,
    Outgoing,
}

impl From<String> for FriendState {
    fn from(s: String) -> FriendState {
        match s.as_str() {
            "accepted" => FriendState::Accepted,
            "outgoing" => FriendState::Outgoing,
            _ => FriendState::Incoming,
        }
    }
}

impl<'de> Deserialize<'de> for FriendState {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<FriendState, D::Error> {
        Ok(Option::<String>::deserialize(d)?.unwrap_or_default().into())
    }
}

// ---------------------------------------------------------------- people

/// One struct for all three shapes the server sends: the public user, the
/// user with a status, and a server member (which adds roles). The missing
/// fields simply stay at their defaults, the way the interface treated these
/// as one kind of thing all along.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct User {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub color: String,
    #[serde(default)]
    pub avatar: Option<String>,
    #[serde(default)]
    pub status: Status,
    #[serde(default)]
    pub roles: Vec<i64>,
    /// As a server's member: who signed them in (e2e/members.rs).
    #[serde(default)]
    pub certs: Vec<serde_json::Value>,
}

impl User {
    /// A placeholder for someone the app has never heard of.
    pub fn unknown(id: i64) -> User {
        User {
            id,
            username: "?".into(),
            display_name: "Usuário".into(),
            ..Default::default()
        }
    }

    pub fn name(&self) -> &str {
        if !self.display_name.is_empty() {
            &self.display_name
        } else if !self.username.is_empty() {
            &self.username
        } else {
            "Desconhecido"
        }
    }

    /// The letter shown when there is no picture (a whole flag or emoji, if
    /// the name starts with one).
    pub fn initial(&self) -> String {
        use unicode_segmentation::UnicodeSegmentation as _;
        self.name()
            .trim()
            .graphemes(true)
            .next()
            .map(str::to_uppercase)
            .unwrap_or_else(|| "?".into())
    }

    /// Copies over the fields a USER_UPDATE carries, leaving status and
    /// roles alone (that event does not include them).
    pub fn apply_profile(&mut self, from: &User) {
        self.username.clone_from(&from.username);
        self.display_name.clone_from(&from.display_name);
        self.color.clone_from(&from.color);
        self.avatar.clone_from(&from.avatar);
    }
}

// ---------------------------------------------------------------- servers

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Role {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub perms: i64,
    #[serde(default)]
    pub is_everyone: i64,
}

impl Role {
    pub fn is_everyone(&self) -> bool {
        self.is_everyone != 0
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
pub struct Override {
    #[serde(default)]
    pub role_id: i64,
    #[serde(default)]
    pub allow: i64,
    #[serde(default)]
    pub deny: i64,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Channel {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(rename = "type", default)]
    pub kind: ChannelKind,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub topic: Option<String>,
    #[serde(default)]
    pub perms: i64,
    #[serde(default)]
    pub overrides: Vec<Override>,
    /// Set by the app for direct conversations, which have no name of their
    /// own: the person on the other side.
    #[serde(default)]
    pub user: Option<User>,
    /// How long its messages last (s); 0 for good.
    #[serde(default)]
    pub ttl: i64,
}

impl Channel {
    pub fn label(&self) -> &str {
        match (&self.user, &self.name) {
            (Some(u), _) => u.name(),
            (None, Some(n)) => n,
            (None, None) => "",
        }
    }

    pub fn category_name(&self) -> &str {
        match self.category.as_deref() {
            Some(c) if !c.is_empty() => c,
            _ => "Canais",
        }
    }

    pub fn can(&self, bit: i64) -> bool {
        self.perms & bit != 0
    }

    pub fn is_voice(&self) -> bool {
        self.kind == ChannelKind::Voice
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct VoiceUser {
    #[serde(rename = "userId", default)]
    pub user_id: i64,
    #[serde(default)]
    pub mute: bool,
    #[serde(default)]
    pub deaf: bool,
    #[serde(default, deserialize_with = "opt_video")]
    pub video: Option<Video>,
}

fn opt_video<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Video>, D::Error> {
    Ok(video_from(Option::<String>::deserialize(d)?))
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Server {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub owner_id: i64,
    #[serde(default)]
    pub color: String,
    #[serde(default)]
    pub perms: i64,
    #[serde(default)]
    pub roles: Vec<Role>,
    #[serde(default)]
    pub members: Vec<User>,
    #[serde(default)]
    pub channels: Vec<Channel>,
    /// Keyed by channel id as a string, the way the server sends it.
    #[serde(default)]
    pub voice: HashMap<String, Vec<VoiceUser>>,
    #[serde(default)]
    pub bans: Vec<User>,
    #[serde(default)]
    pub reports: Vec<Report>,
    /// Its picture ("/uploads/..."); none shows the initials.
    #[serde(default)]
    pub icon: Option<String>,
    /// Joining takes a request that someone here (with P_KICK) accepts.
    #[serde(default)]
    pub private: bool,
    /// Who is waiting to come in: only for those who may let them.
    #[serde(default)]
    pub requests: Vec<JoinRequest>,
}

/// Someone who asked to join a private server.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct JoinRequest {
    #[serde(default)]
    pub user: User,
    /// When (ms since 1970).
    #[serde(default)]
    pub created: i64,
}

/// A server as someone outside it sees it: what they asked to join.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct ServerCard {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub color: String,
    #[serde(default)]
    pub icon: Option<String>,
}

/// One of my requests to join a private server, still waiting.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct MyRequest {
    #[serde(default)]
    pub server: ServerCard,
    #[serde(default)]
    pub created: i64,
}

impl Server {
    pub fn can(&self, bit: i64) -> bool {
        self.perms & bit != 0
    }

    pub fn channel(&self, id: i64) -> Option<&Channel> {
        self.channels.iter().find(|c| c.id == id)
    }

    pub fn member(&self, id: i64) -> Option<&User> {
        self.members.iter().find(|m| m.id == id)
    }

    pub fn role(&self, id: i64) -> Option<&Role> {
        self.roles.iter().find(|r| r.id == id)
    }

    pub fn in_voice(&self, channel_id: i64) -> &[VoiceUser] {
        self.voice
            .get(&channel_id.to_string())
            .map_or(&[], Vec::as_slice)
    }

    pub fn set_voice(&mut self, channel_id: i64, users: Vec<VoiceUser>) {
        self.voice.insert(channel_id.to_string(), users);
    }

    /// The colour of the member's highest role that has one, for their name.
    pub fn role_color(&self, member: &User) -> Option<&str> {
        self.roles
            .iter()
            .filter(|r| !r.is_everyone() && member.roles.contains(&r.id))
            .find_map(|r| match r.color.as_deref() {
                Some(c) if !c.is_empty() => Some(c),
                _ => None,
            })
    }

    /// The letters shown on the server button: the first character, as a
    /// person sees it, of each of the first two words. A flag or an emoji
    /// with a skin tone is several code points and must not be cut in half.
    pub fn initials(&self) -> String {
        initials(&self.name)
    }
}

impl ServerCard {
    pub fn initials(&self) -> String {
        initials(&self.name)
    }
}

/// The letters a server button shows (`Server::initials`).
fn initials(name: &str) -> String {
    use unicode_segmentation::UnicodeSegmentation as _;
    name.split_whitespace()
        .take(2)
        .filter_map(|w| w.graphemes(true).next())
        .map(str::to_uppercase)
        .collect()
}

// ---------------------------------------------------------------- messages

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Attachment {
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub size: i64,
    #[serde(default)]
    pub kind: Kind,
    /// A file sent directly (Kind::Direct): what to ask its sender for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direct: Option<DirectInfo>,
}

/// What a file sent directly is: its id on the sender's PC, how it is cut
/// in pieces, and the hash of those pieces' hashes (direct/hash.rs).
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct DirectInfo {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub sha256: String,
    #[serde(default)]
    pub piece: u64,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Reaction {
    #[serde(default)]
    pub users: Vec<i64>,
    /// The emoji. As the server sends an encrypted message's reactions,
    /// their mark (e2e/msg.rs), until the message opens.
    #[serde(default)]
    pub emoji: String,
    /// Each person's encrypted and signed reaction: (who, what).
    #[serde(default)]
    pub data: Vec<(i64, String)>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct ReplyRef {
    #[serde(default)]
    pub author: Option<User>,
    #[serde(default)]
    pub content: String,
    /// The message answered is a sticker (its GIPHY id).
    #[serde(default)]
    pub sticker: Option<String>,
    /// As an encrypted message quotes it: the author's id (the app fills
    /// in `author` from it).
    #[serde(skip)]
    pub author_id: i64,
}

/// What became of a message's encryption.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Seal {
    /// Not encrypted: from before the encryption (or an old app). Nobody
    /// vouches for who wrote it.
    #[default]
    Plain,
    /// Encrypted, its keys still on their way.
    Locked,
    /// Opened, and signed by its author.
    Open,
    /// Encrypted, and this PC has no key for it: sent before I could read
    /// the conversation, or to an identity of mine from before a reset.
    Missing,
    /// Encrypted, and it does not hold together: why.
    Refused(&'static str),
    /// Its time is up (a conversation whose messages disappear): about to
    /// be taken away, never shown.
    Gone,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Message {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub channel_id: i64,
    #[serde(default)]
    pub author: Option<User>,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub created: i64,
    #[serde(default)]
    pub edited: Option<i64>,
    #[serde(default)]
    pub reply: Option<ReplyRef>,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    #[serde(default)]
    pub reactions: Vec<Reaction>,
    /// A GIPHY sticker's id: the message is that sticker, and nothing else.
    #[serde(default)]
    pub sticker: Option<String>,
    /// The mark the sender's app put on it, echoed back by the server, so
    /// the copy it showed at once can be matched to the real one.
    #[serde(default)]
    pub nonce: Option<String>,
    /// The encrypted envelope (e2e/msg.rs), as the server keeps it.
    #[serde(default)]
    pub e2e: Option<serde_json::Value>,
    /// Shown here before the server has it (the sender's own copy).
    #[serde(skip)]
    pub pending: bool,
    #[serde(skip)]
    pub seal: Seal,
    /// What the envelope said, once opened (an edit starts from it).
    #[serde(skip)]
    pub body: Option<Box<crate::e2e::msg::Body>>,
}

impl Message {
    pub fn author_id(&self) -> i64 {
        self.author.as_ref().map_or(0, |a| a.id)
    }

    /// Whether this message and the one before it belong to the same block:
    /// same person, within five minutes, and not a reply.
    pub fn groups_with(&self, prev: &Message) -> bool {
        prev.author_id() == self.author_id()
            && self.created - prev.created < 300_000
            && self.reply.is_none()
    }

    pub fn reacted(&self, emoji: &str, uid: i64) -> bool {
        self.reactions
            .iter()
            .any(|r| r.emoji == emoji && r.users.contains(&uid))
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Report {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub reporter: Option<User>,
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(default)]
    pub message: Message,
}

/// The reasons a message can be reported for, as the dialog lists them.
pub const REPORT_REASONS: &[(&str, &str)] = &[
    ("spam", "Spam"),
    ("assedio", "Assédio ou ódio"),
    ("improprio", "Conteúdo impróprio"),
    ("outro", "Outro motivo"),
];

/// Turns a stored reason back into its label.
pub fn reason_label(reason: &str) -> &str {
    REPORT_REASONS
        .iter()
        .find(|(k, _)| *k == reason)
        .map_or(reason, |(_, l)| *l)
}

// ---------------------------------------------------------------- top level

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Dm {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub user: User,
    /// How long its messages last (s); 0 for good.
    #[serde(default)]
    pub ttl: i64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Friend {
    #[serde(default)]
    pub user: User,
    pub state: FriendState,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Ready {
    #[serde(default)]
    pub user: User,
    #[serde(default)]
    pub servers: Vec<Server>,
    #[serde(default)]
    pub dms: Vec<Dm>,
    #[serde(default)]
    pub friends: Vec<Friend>,
    #[serde(default)]
    pub blocks: Vec<i64>,
    #[serde(default)]
    pub version: String,
    /// My favourite stickers (GIPHY ids), newest first.
    #[serde(default)]
    pub stickers: Vec<String>,
    /// The GIPHY key the apps search with; empty hides the stickers.
    #[serde(default)]
    pub giphy: String,
    /// What only I see of my account (crate::google::Account).
    #[serde(default)]
    pub account: serde_json::Value,
    /// The private servers I asked to join and wait on.
    #[serde(default)]
    pub requests: Vec<MyRequest>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Messages {
    #[serde(default)]
    pub more: bool,
    #[serde(default)]
    pub messages: Vec<Message>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct IceServer {
    #[serde(default, deserialize_with = "one_or_many")]
    pub urls: Vec<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub credential: Option<String>,
}

/// `urls` is allowed to be a single address or a list of them.
fn one_or_many<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
        Null,
    }
    Ok(match OneOrMany::deserialize(d)? {
        OneOrMany::One(s) => vec![s],
        OneOrMany::Many(v) => v,
        OneOrMany::Null => Vec::new(),
    })
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct IceConfig {
    #[serde(rename = "iceServers", default)]
    pub ice_servers: Vec<IceServer>,
    #[serde(rename = "relayOnly", default)]
    pub relay_only: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_enum_values_fall_back() {
        assert_eq!(Status::from("gremlin".to_owned()), Status::Offline);
        assert_eq!(ChannelKind::from("gremlin".to_owned()), ChannelKind::Text);
        assert_eq!(Kind::from("gremlin".to_owned()), Kind::File);
        assert_eq!(video_from(Some("gremlin".into())), None);
        assert_eq!(Status::Invisible.public(), Status::Offline);
        assert_eq!(Status::Dnd.public(), Status::Dnd);
    }

    #[test]
    fn a_member_parses_as_one_user_shape() {
        // Two hashes: the colour value itself contains a quote-hash pair.
        let m: User = serde_json::from_str(
            r##"{"id":7,"username":"ana","display_name":"Ana","color":"#fff",
                 "avatar":null,"status":"idle","roles":[1,2]}"##,
        )
        .unwrap();
        assert_eq!((m.id, m.status, m.roles.len()), (7, Status::Idle, 2));
        // And so does an author, which carries neither status nor roles.
        let a: User =
            serde_json::from_str(r#"{"id":7,"username":"ana","display_name":"","color":""}"#)
                .unwrap();
        assert_eq!((a.status, a.name()), (Status::Offline, "ana"));
    }

    #[test]
    fn ready_survives_missing_and_extra_fields() {
        let r: Ready = serde_json::from_str(r#"{"user":{"id":1},"surpresa":true}"#).unwrap();
        assert_eq!(r.user.id, 1);
        assert!(r.servers.is_empty() && r.blocks.is_empty());
        // An older server says nothing of stickers: they stay hidden.
        assert!(r.stickers.is_empty() && r.giphy.is_empty());
        let r: Ready =
            serde_json::from_str(r#"{"user":{"id":1},"stickers":["b2","a1"],"giphy":"chave"}"#)
                .unwrap();
        assert_eq!(
            (r.stickers, r.giphy.as_str()),
            (vec!["b2".into(), "a1".into()], "chave")
        );
    }

    #[test]
    fn stickers_come_as_ids_or_null() {
        let m: Message = serde_json::from_str(
            r#"{"id":5,"content":"","sticker":"xT4uQulxzV39haRFjG",
                "reply":{"id":4,"content":"","sticker":"abc"}}"#,
        )
        .unwrap();
        assert_eq!(m.sticker.as_deref(), Some("xT4uQulxzV39haRFjG"));
        assert_eq!(m.reply.and_then(|r| r.sticker).as_deref(), Some("abc"));
        let m: Message = serde_json::from_str(r#"{"id":6,"content":"oi","sticker":null}"#).unwrap();
        assert_eq!(m.sticker, None);
    }

    #[test]
    fn a_private_server_brings_its_picture_and_who_asked() {
        let s: Server = serde_json::from_str(
            r#"{"id":1,"name":"Casa Fechada","icon":"/uploads/ab.png","private":true,
                "requests":[{"user":{"id":7,"username":"caio","display_name":"Caio"},"created":5}]}"#,
        )
        .unwrap();
        assert_eq!(s.icon.as_deref(), Some("/uploads/ab.png"));
        assert!(s.private);
        assert_eq!(s.requests.len(), 1);
        assert_eq!(s.requests[0].user.username, "caio");
        // From a server of before: open, no picture, nobody waiting.
        let old: Server = serde_json::from_str(r#"{"id":2,"name":"Velho"}"#).unwrap();
        assert!(!old.private && old.icon.is_none() && old.requests.is_empty());
        let ready: Ready = serde_json::from_str(
            r#"{"requests":[{"server":{"id":3,"name":"Clube do Livro","icon":null},"created":9}]}"#,
        )
        .unwrap();
        assert_eq!(ready.requests[0].server.id, 3);
        assert_eq!(ready.requests[0].server.initials(), "CD");
    }

    #[test]
    fn voice_keys_are_strings_on_the_wire() {
        let s: Server = serde_json::from_str(
            r#"{"id":1,"name":"Casa","voice":{"42":[{"userId":9,"mute":true,"video":"screen"}]}}"#,
        )
        .unwrap();
        let users = s.in_voice(42);
        assert_eq!(users.len(), 1);
        assert_eq!(users[0].user_id, 9);
        assert!(users[0].mute && !users[0].deaf);
        assert_eq!(users[0].video, Some(Video::Screen));
        assert!(s.in_voice(43).is_empty());
    }

    #[test]
    fn ice_urls_accept_one_or_many() {
        let c: IceConfig = serde_json::from_str(
            r#"{"iceServers":[{"urls":"stun:a"},{"urls":["turn:b","turns:c"],"username":"u","credential":"p"}],"relayOnly":true}"#,
        )
        .unwrap();
        assert_eq!(c.ice_servers[0].urls, ["stun:a"]);
        assert_eq!(c.ice_servers[1].urls.len(), 2);
        assert_eq!(c.ice_servers[1].username.as_deref(), Some("u"));
        assert!(c.relay_only);
    }

    #[test]
    fn grouping_needs_same_author_and_five_minutes() {
        let msg = |id: i64, author: i64, created: i64| Message {
            id,
            author: Some(User {
                id: author,
                ..Default::default()
            }),
            created,
            ..Default::default()
        };
        let first = msg(1, 10, 1_000_000);
        assert!(msg(2, 10, 1_060_000).groups_with(&first));
        assert!(!msg(3, 11, 1_060_000).groups_with(&first));
        assert!(!msg(4, 10, 1_400_000).groups_with(&first));
        let mut reply = msg(5, 10, 1_060_000);
        reply.reply = Some(ReplyRef::default());
        assert!(!reply.groups_with(&first));
    }

    #[test]
    fn initials_and_role_colour() {
        let s = Server {
            name: "  casa   dos  amigos ".into(),
            roles: vec![
                Role {
                    id: 1,
                    is_everyone: 1,
                    color: Some("#111111".into()),
                    ..Default::default()
                },
                Role {
                    id: 2,
                    color: Some(String::new()),
                    ..Default::default()
                },
                Role {
                    id: 3,
                    color: Some("#abcdef".into()),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        assert_eq!(s.initials(), "CD");
        let member = User {
            roles: vec![1, 2, 3],
            ..Default::default()
        };
        // @everyone is skipped, so is a role with no colour set.
        assert_eq!(s.role_color(&member), Some("#abcdef"));
    }

    #[test]
    fn initials_keep_flags_and_emoji_whole() {
        let named = |name: &str| Server {
            name: name.into(),
            ..Default::default()
        };
        assert_eq!(named("Bandeiras 🇧🇷").initials(), "B🇧🇷");
        assert_eq!(named("👍🏽 time").initials(), "👍🏽T");
        assert_eq!(named(" éramos").initials(), "É");
        let person = |name: &str| User {
            display_name: name.into(),
            ..Default::default()
        };
        assert_eq!(person("🇧🇷 Ana").initial(), "🇧🇷");
        assert_eq!(person("  bia").initial(), "B");
    }
}
