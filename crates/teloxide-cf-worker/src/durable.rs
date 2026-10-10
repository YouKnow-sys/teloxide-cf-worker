//! Durable Object plumbing for per-key serialized update handling: each
//! distribution key gets one Durable Object instance whose updates are
//! handled one at a time, like [`distribution_function`] does for the native
//! dispatch loop.
//!
//! Updates for one key are handled strictly one at a time, in the order they
//! reach the Durable Object. That can differ from Telegram's send order when
//! Telegram delivers updates concurrently.
//!
//! Only compiled with the `distribution` feature, and only used when the
//! `#[bot]` macro is given a `distribution` argument.
//!
//! [`distribution_function`]:
//! teloxide::dispatching::DispatcherBuilder::distribution_function

use std::cell::Cell;

use teloxide::types::Update;
use worker::wasm_bindgen::JsValue;
use worker::{Env, Method, Request, RequestInit, Response, durable::State};

use crate::runtime;

/// Max updates waiting or running per distribution key; over-cap requests get
/// a 503 so Telegram re-delivers later. Mirrors teloxide's default worker
/// queue size.
pub const QUEUE_CAP: usize = 64;

pub use crate::runtime::DEFAULT_DO_BINDING;

/// Distribution key by chat id (teloxide's default grouping); `None` (no
/// chat) is handled inline in the worker fetch, concurrently.
pub fn default_key(update: &Update) -> Option<String> {
    update.chat().map(|c| c.id.to_string())
}

/// Forwards raw update bytes to the Durable Object instance named `key`. The
/// response arrives only after the update was handled (slow ack), so Telegram
/// retries on failure.
pub(crate) async fn forward(
    env: &Env,
    binding: &str,
    key: &str,
    body: Vec<u8>,
) -> worker::Result<Response> {
    let stub = env.durable_object(binding)?.get_by_name(key)?;
    let mut init = RequestInit::new();
    // Raw bytes, no String/UTF-16 round trip.
    init.with_method(Method::Post).with_body(Some(JsValue::from(
        worker::js_sys::Uint8Array::new_from_slice(&body),
    )));
    // The URL is irrelevant: routing is by DO instance, not by path.
    let req = Request::new_with_init("https://teloxide.invalid/update", &init)?;
    match stub.fetch_with_request(req).await {
        Ok(res) if (200..300).contains(&res.status_code()) => Response::empty(),
        // 503 = queue full; Telegram re-delivers.
        Ok(res) => Response::error("update not accepted", res.status_code()),
        Err(e) => {
            worker::console_error!("teloxide-cf-worker: durable object fetch failed: {e}");
            Response::error("durable object unavailable", 502)
        }
    }
}

struct Slot<'a>(&'a Cell<usize>);

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        self.0.set(self.0.get() - 1);
    }
}

/// Durable Object class named in wrangler.toml (`class_name`/`new_sqlite_classes`):
/// one instance per distribution key, handling updates one at a time.
#[worker::durable_object]
pub struct TeloxideUpdateDistributor {
    env: Env,
    gate: tokio::sync::Mutex<()>,
    pending: Cell<usize>,
}

impl worker::DurableObject for TeloxideUpdateDistributor {
    fn new(_state: State, env: Env) -> Self {
        Self {
            env,
            gate: tokio::sync::Mutex::new(()),
            pending: Cell::new(0),
        }
    }

    async fn fetch(&self, mut req: Request) -> worker::Result<Response> {
        if self.pending.get() >= QUEUE_CAP {
            return Response::error("update queue full", 503);
        }
        self.pending.set(self.pending.get() + 1);
        let _slot = Slot(&self.pending);

        // Take the turn before reading the body, so handlers run in arrival order.
        let _turn = self.gate.lock().await;
        let dp = runtime::get_dp(&self.env).await?;
        let bytes = req.bytes().await?;
        let update = runtime::parse_update(&bytes)?;

        runtime::dispatch_with_timeout(dp, update).await;
        Response::empty()
    }
}
