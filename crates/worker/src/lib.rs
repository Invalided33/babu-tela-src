//! The Babu Tela server, running on Cloudflare.
//!
//! The Worker itself holds nothing: every request about the chat goes to one
//! Durable Object ([`Hub`]), which owns the SQLite database and the open
//! WebSockets. That keeps the whole chat consistent, the same way the
//! single-process server did, and the database lives with the code instead
//! of on a PC. Files are the exception: reading them and taking the parts of
//! an upload go straight between the Worker and R2 (see files.rs).

use std::cell::RefCell;

use worker::*;

mod db;
mod direct;
mod e2e;
mod files;
mod gateway;
mod google;
mod http;
mod ice;
mod model;
mod router;
mod routes;
mod state;
mod util;

/// The Durable Object binding declared at deploy time.
const HUB: &str = "HUB";

/// Everything lives in one object, so it is always the same one.
const HUB_NAME: &str = "main";

#[event(start)]
fn start() {
    console_error_panic_hook::set_once();
}

#[event(fetch)]
async fn fetch(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    // Reading files and taking the parts of an upload need nothing from the
    // chat, so they never wake it (and never hold it while bytes flow).
    if let Some(answer) = files::stateless(&req, &env).await {
        let mut res = match answer {
            Ok(res) => res,
            Err(e) => {
                console_error!("arquivos: {e}");
                return internal();
            }
        };
        http::harden(&mut res);
        return Ok(res);
    }
    let hub = env
        .durable_object(HUB)?
        .id_from_name(&hub_name(&req, &env))?
        .get_stub()?;
    match hub.fetch_with_request(req).await {
        Ok(res) => Ok(res),
        Err(e) => {
            console_error!("hub: {e}");
            internal()
        }
    }
}

/// The same body and headers a handler would send for a failure.
fn internal() -> Result<Response> {
    let mut res = http::json_error(500, util::INTERNAL)?;
    http::harden(&mut res);
    Ok(res)
}

/// Normally the one object. The end-to-end test asks for a throwaway one
/// (its own empty database) by sending `x-babu-test: <segredo>:<nome>`,
/// which only works while the TEST_HUB secret exists.
fn hub_name(req: &Request, env: &Env) -> String {
    let Ok(secret) = env.secret("TEST_HUB") else {
        return HUB_NAME.into();
    };
    let header = req
        .headers()
        .get("x-babu-test")
        .ok()
        .flatten()
        .unwrap_or_default();
    match header.split_once(':') {
        Some((given, name))
            if given == secret.to_string()
                && !name.is_empty()
                && name.len() <= 40
                && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') =>
        {
            format!("teste-{name}")
        }
        _ => HUB_NAME.into(),
    }
}

/// Whether `req` goes to one of the tests' throwaway databases. The object
/// asks the same question the Worker answered when it picked it.
pub(crate) fn on_test_hub(req: &Request, env: &Env) -> bool {
    hub_name(req, env) != HUB_NAME
}

/// The chat: the database and the gateway's open connections.
#[durable_object]
pub struct Hub {
    /// Kept outside the cell so file reads and writes, which have to wait on
    /// the network, never hold the chat's state while another request lands.
    pub(crate) env: Env,
    pub(crate) st: RefCell<state::State>,
}

impl DurableObject for Hub {
    fn new(ctx: State, env: Env) -> Self {
        let st = state::State::new(ctx, env.clone());
        // A failure here would leave the object unusable, and the error says
        // what went wrong better than a later confusing one.
        st.db.migrate().expect("preparando o banco");
        // The app pings every few seconds; answering those in the runtime
        // lets the object stay asleep.
        if let Ok(pair) =
            worker_sys::WebSocketRequestResponsePair::new(gateway::PING, gateway::PONG)
        {
            st.do_state().set_websocket_auto_response(&pair);
        }
        Self {
            env,
            st: RefCell::new(st),
        }
    }

    async fn fetch(&self, req: Request) -> Result<Response> {
        match router::serve(self, req).await {
            Ok(res) => Ok(res),
            Err(e) => {
                console_error!("pedido: {e}");
                internal()
            }
        }
    }

    async fn websocket_message(
        &self,
        ws: WebSocket,
        message: WebSocketIncomingMessage,
    ) -> Result<()> {
        let Ok(mut st) = self.st.try_borrow_mut() else {
            return Ok(());
        };
        st.refresh();
        match message {
            WebSocketIncomingMessage::String(text) => {
                gateway::message(&mut st, &ws, text.as_bytes())
            }
            WebSocketIncomingMessage::Binary(bytes) => gateway::message(&mut st, &ws, &bytes),
        }
        Ok(())
    }

    async fn websocket_close(
        &self,
        ws: WebSocket,
        _code: usize,
        _reason: String,
        _was_clean: bool,
    ) -> Result<()> {
        self.closed(&ws).await;
        Ok(())
    }

    async fn websocket_error(&self, ws: WebSocket, _error: Error) -> Result<()> {
        self.closed(&ws).await;
        Ok(())
    }

    /// Places held in calls whose time ran out (gateway::alarm).
    async fn alarm(&self) -> Result<Response> {
        let next = match self.st.try_borrow_mut() {
            Ok(mut st) => gateway::alarm(&mut st),
            // Busy with something else right now: again in a moment.
            Err(_) => Some(util::now() + 1000),
        };
        if let Some(at) = next {
            self.wake_at(at).await;
        }
        Response::empty()
    }
}

impl Hub {
    async fn closed(&self, ws: &WebSocket) {
        let held = match self.st.try_borrow_mut() {
            Ok(mut st) => gateway::closed(&mut st, ws),
            Err(_) => None,
        };
        if let Some(expires) = held {
            self.wake_at(expires).await;
        }
    }

    /// Has the object woken up at `at` (ms since 1970) for the places held
    /// in calls, unless it already will be by then. One alarm serves them
    /// all: each run sets it again for the next one (gateway::alarm).
    async fn wake_at(&self, at: i64) {
        // Taken out of the cell first: the waits below must not hold it.
        let Some(storage) = self.st.try_borrow().ok().map(|st| st.do_state().storage()) else {
            return;
        };
        if let Ok(Some(set)) = storage.get_alarm().await
            && set <= at
        {
            return;
        }
        if let Err(e) = storage.set_alarm(at - util::now()).await {
            console_error!("alarme: {e}");
        }
    }
}
