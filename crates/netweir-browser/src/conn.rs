//! The protocol client: one pipe to Chrome, carrying every session.
//!
//! Each message is a JSON object followed by a NUL byte. A reader thread
//! routes replies to the command waiting for their `id` and events to the
//! handler of the session they name; a writer thread does the writing, so
//! no async task waits on the pipe.

use std::collections::{BTreeSet, HashMap};
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::sync::oneshot;

use crate::{Error, Result};

/// Methods whose side effects page scripts can see. Never sent.
const FORBIDDEN: &[&str] = &["Runtime.enable", "Console.enable", "Log.enable"];

/// Called on the reader thread for each event of a session.
pub(crate) type Handler = Arc<dyn Fn(&str, &Value) + Send + Sync>;

/// Sent to a session's handler, after which it gets nothing more.
pub(crate) const GONE: &str = "netweir.gone";

#[derive(Clone)]
pub(crate) struct Connection {
    shared: Arc<Shared>,
}

struct Shared {
    out: Mutex<Option<mpsc::Sender<Vec<u8>>>>,
    next_id: AtomicU64,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    closed: bool,
    pending: HashMap<u64, (String, oneshot::Sender<Result<Value>>)>,
    /// By session id; "" is the browser's own.
    handlers: HashMap<String, Handler>,
    /// Every method sent, for the test that nothing forbidden was.
    sent: BTreeSet<String>,
}

impl Connection {
    pub(crate) fn new(
        input: impl Read + Send + 'static,
        output: impl Write + Send + 'static,
    ) -> Connection {
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let shared = Arc::new(Shared {
            out: Mutex::new(Some(tx)),
            next_id: AtomicU64::new(1),
            state: Mutex::new(State::default()),
        });
        std::thread::Builder::new()
            .name("netweir-cdp-write".into())
            .spawn(move || {
                let mut output = output;
                for message in rx {
                    if output.write_all(&message).is_err() {
                        break;
                    }
                }
            })
            .expect("can't start the CDP writer thread");
        let reader = shared.clone();
        std::thread::Builder::new()
            .name("netweir-cdp-read".into())
            .spawn(move || read_loop(BufReader::new(input), &reader))
            .expect("can't start the CDP reader thread");
        Connection { shared }
    }

    /// Sends `method` to `session` ("" for the browser) and waits for the
    /// result.
    pub(crate) async fn call(&self, session: &str, method: &str, params: Value) -> Result<Value> {
        if FORBIDDEN.contains(&method) {
            return Err(Error::Invalid(format!(
                "{method} is never sent: pages can detect it"
            )));
        }
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        let mut message = json!({"id": id, "method": method, "params": params});
        if !session.is_empty() {
            message["sessionId"] = Value::from(session);
        }
        let mut bytes = serde_json::to_vec(&message).expect("a JSON value serialises");
        bytes.push(0);
        let (tx, rx) = oneshot::channel();
        {
            let mut state = self.shared.lock();
            if state.closed {
                return Err(Error::Closed);
            }
            state.sent.insert(method.to_string());
            state.pending.insert(id, (method.to_string(), tx));
        }
        let sent = match &*self.shared.out.lock().unwrap_or_else(|e| e.into_inner()) {
            Some(out) => out.send(bytes).is_ok(),
            None => false,
        };
        if !sent {
            self.shared.lock().pending.remove(&id);
            return Err(Error::Closed);
        }
        rx.await.unwrap_or(Err(Error::Closed))
    }

    pub(crate) fn on(&self, session: &str, handler: Handler) {
        let mut state = self.shared.lock();
        if state.closed {
            drop(state);
            handler(GONE, &Value::Null);
            return;
        }
        state.handlers.insert(session.to_string(), handler);
    }

    /// Stops events reaching the session's handler, which hears GONE.
    pub(crate) fn off(&self, session: &str) {
        let handler = self.shared.lock().handlers.remove(session);
        if let Some(handler) = handler {
            handler(GONE, &Value::Null);
        }
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.shared.lock().closed
    }

    pub(crate) fn methods_sent(&self) -> Vec<String> {
        self.shared.lock().sent.iter().cloned().collect()
    }

    /// Stops writing; the reader ends when Chrome closes its end.
    pub(crate) fn shut(&self) {
        self.shared
            .out
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
    }
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn deliver(&self, message: Value) {
        if let Some(id) = message.get("id").and_then(Value::as_u64) {
            let Some((method, tx)) = self.lock().pending.remove(&id) else {
                return;
            };
            let result = match message.get("error") {
                Some(e) => Err(Error::Protocol {
                    method,
                    message: e
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("failed")
                        .to_string(),
                }),
                None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
            };
            let _ = tx.send(result);
            return;
        }
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            return;
        };
        let params = message.get("params").unwrap_or(&Value::Null);
        let session = message
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or("");
        // A detached target's session hears it from the browser's.
        if method == "Target.detachedFromTarget"
            && let Some(gone) = params.get("sessionId").and_then(Value::as_str)
        {
            let handler = self.lock().handlers.remove(gone);
            if let Some(handler) = handler {
                handler(GONE, &Value::Null);
            }
        }
        let handler = self.lock().handlers.get(session).cloned();
        if let Some(handler) = handler {
            handler(method, params);
        }
    }

    fn close(&self) {
        let (pending, handlers) = {
            let mut state = self.lock();
            state.closed = true;
            (
                std::mem::take(&mut state.pending),
                std::mem::take(&mut state.handlers),
            )
        };
        for (_, (_, tx)) in pending {
            let _ = tx.send(Err(Error::Closed));
        }
        for handler in handlers.into_values() {
            handler(GONE, &Value::Null);
        }
        self.out.lock().unwrap_or_else(|e| e.into_inner()).take();
    }
}

fn read_loop(mut input: impl BufRead, shared: &Shared) {
    let mut message = Vec::new();
    loop {
        message.clear();
        match input.read_until(0, &mut message) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if message.last() == Some(&0) {
            message.pop();
        }
        // ponytail: a message that isn't JSON is dropped; Chrome only sends
        // JSON, and one bad message shouldn't end the session.
        if let Ok(value) = serde_json::from_slice::<Value>(&message) {
            shared.deliver(value);
        }
    }
    shared.close();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn forbidden_methods_are_refused_without_being_sent() {
        let (input, _keep) = std::io::pipe().unwrap();
        let conn = Connection::new(input, std::io::sink());
        let err = conn
            .call("", "Runtime.enable", json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Invalid(_)), "{err}");
        assert!(conn.methods_sent().is_empty());
    }

    #[tokio::test]
    async fn a_closed_pipe_fails_what_was_waiting() {
        let (input, writer) = std::io::pipe().unwrap();
        let conn = Connection::new(input, std::io::sink());
        let waiting = tokio::spawn({
            let conn = conn.clone();
            async move { conn.call("", "Browser.getVersion", json!({})).await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(writer);
        assert_eq!(waiting.await.unwrap(), Err(Error::Closed));
        assert!(conn.is_closed());
        assert_eq!(
            conn.call("", "Browser.getVersion", json!({})).await,
            Err(Error::Closed)
        );
    }
}
