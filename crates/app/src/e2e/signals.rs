//! Signalling sealed end to end: what calls and direct transfers say to set
//! up their connections (a description with the DTLS certificate's
//! fingerprint, a candidate with an address) goes signed by my identity and
//! sealed for the person (seal.rs).
//!
//! That fingerprint is what DTLS checks the other side by: a server that
//! could swap it would sit in the middle of the call, or of the file, with
//! both sides none the wiser. Sealed and signed, the server cannot read it,
//! swap it or make one up; one that comes unsealed (an old app, or someone
//! trying) is dropped, so there is no way back to plain signalling.
//!
//! Sealing and opening take a moment the first time (the person's keys may
//! have to be fetched), and the order matters (an offer before its
//! candidates): each person has a queue of their own, each way.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::Value;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

use super::E2e;

/// What to do with the result of one step.
pub type Then = Box<dyn FnOnce(Value) + Send>;

/// The waits after each failed try to seal (the person's keys not published
/// yet): about 7 s in all, which a connection being set up can wait.
const RETRIES: [std::time::Duration; 5] = [
    std::time::Duration::from_millis(250),
    std::time::Duration::from_millis(500),
    std::time::Duration::from_secs(1),
    std::time::Duration::from_secs(2),
    std::time::Duration::from_secs(3),
];

enum Job {
    Seal {
        context: Vec<u8>,
        data: Value,
        then: Then,
    },
    Open {
        context: Vec<u8>,
        sealed: Value,
        then: Then,
    },
}

pub struct Signals {
    e2e: Arc<E2e>,
    rt: tokio::runtime::Handle,
    /// One queue per person and way (true: to them).
    queues: Mutex<HashMap<(i64, bool), UnboundedSender<Job>>>,
}

/// What a call's signalling is sealed in: the voice channel, on both sides.
pub fn call_context(channel: i64) -> Vec<u8> {
    [
        b"babutela/e2e/v1/chamada".as_slice(),
        &channel.to_be_bytes(),
    ]
    .concat()
}

/// What a direct transfer's is sealed in: the message, the file and the
/// transfer's own number.
pub fn direct_context(msg: i64, file: &str, sid: &str) -> Vec<u8> {
    [
        b"babutela/e2e/v1/direto".as_slice(),
        &msg.to_be_bytes(),
        &(file.len() as u32).to_be_bytes(),
        file.as_bytes(),
        sid.as_bytes(),
    ]
    .concat()
}

impl Signals {
    pub fn new(e2e: Arc<E2e>, rt: tokio::runtime::Handle) -> Arc<Signals> {
        Arc::new(Signals {
            e2e,
            rt,
            queues: Mutex::new(HashMap::new()),
        })
    }

    fn queue(&self, peer: i64, out: bool) -> UnboundedSender<Job> {
        let mut queues = self.queues.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(tx) = queues.get(&(peer, out))
            && !tx.is_closed()
        {
            return tx.clone();
        }
        let (tx, mut rx) = unbounded_channel::<Job>();
        let e2e = self.e2e.clone();
        self.rt.spawn(async move {
            while let Some(job) = rx.recv().await {
                match job {
                    Job::Seal {
                        context,
                        data,
                        then,
                    } => {
                        // Someone who just signed in may not have published
                        // their keys yet: a few more tries, a second or so
                        // apart, before the step is given up on.
                        let mut last = String::new();
                        let mut sealed = None;
                        for wait in RETRIES {
                            match e2e.seal_signal(peer, &context, &data).await {
                                Ok(s) => {
                                    sealed = Some(s);
                                    break;
                                }
                                Err(e) => last = e,
                            }
                            tokio::time::sleep(wait).await;
                        }
                        match sealed {
                            Some(s) => then(s),
                            None => crate::logln!("e2e: sinal para {peer} não selado: {last}"),
                        }
                    }
                    Job::Open {
                        context,
                        sealed,
                        then,
                    } => match e2e.open_signal(peer, &context, &sealed).await {
                        Some(data) => then(data),
                        None => crate::logln!(
                            "e2e: sinal de {peer} recusado (sem selo, de outra conversa, ou não assinado por essa pessoa)"
                        ),
                    },
                }
            }
        });
        queues.insert((peer, out), tx.clone());
        tx
    }

    /// `data` for `to`, signed and sealed in `context`, in its turn: `then`
    /// gets what goes to the server.
    pub fn seal(&self, to: i64, context: Vec<u8>, data: Value, then: Then) {
        let _ = self.queue(to, true).send(Job::Seal {
            context,
            data,
            then,
        });
    }

    /// What `from` sealed for me in `context`, in its turn: `then` gets it
    /// only if it is theirs.
    pub fn open(&self, from: i64, context: Vec<u8>, sealed: Value, then: Then) {
        let _ = self.queue(from, false).send(Job::Open {
            context,
            sealed,
            then,
        });
    }

    /// Signed out: the queues go (their tasks end with them).
    pub fn clear(&self) {
        self.queues
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }
}
