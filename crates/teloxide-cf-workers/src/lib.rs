//! Run a [teloxide] bot as a [Cloudflare Worker].
//!
//! Put [`#[bot]`] on an async function that builds your dispatcher (bot with
//! whichever adaptors you want, handler tree, dependencies, error handler)
//! and returns it, or `Result<Dispatcher<..>, E>` for fallible setup. The
//! macro generates the whole worker entrypoint around it: the `fetch`
//! handler, the Telegram webhook secret check, update parsing and a
//! per-isolate cache that constructs your dispatcher once and feeds every
//! update through it with [`Dispatcher::dispatch_one`].
//!
//! ```ignore
//! use teloxide::adaptors::CacheMe;
//! use teloxide::dispatching::{DefaultKey, Dispatcher};
//! use teloxide::prelude::*;
//! use teloxide_cf_workers::{bot, runtime::console_error_handler, worker::{self, Env}};
//!
//! type HandlerError = Box<dyn std::error::Error + Send + Sync>;
//!
//! #[bot]
//! async fn main(env: Env) -> worker::Result<Dispatcher<CacheMe<Bot>, HandlerError, DefaultKey>> {
//!     let token = env.secret("BOT_TOKEN")?.to_string();
//!     let bot: CacheMe<Bot> = Bot::new(token).cache_me();
//!
//!     Ok(Dispatcher::builder(bot, dptree::entry().endpoint(my_endpoint))
//!         .error_handler(console_error_handler())
//!         .build())
//! }
//! ```
//!
//! The fn runs once per isolate (the worker's and each Durable Object's), so
//! keep it cheap. An `Err` is logged and requests answer 500 until the fn
//! succeeds once.
//!
//! With `distribution = key_fn` (and the `distribution` feature) updates are
//! routed to the Durable Object instance named by the key fn, serializing
//! updates per key; see [`bot`] and the README.
//!
//! With the `storage` feature, `storage::D1Storage` keeps teloxide dialogues
//! in a D1 database; the table comes from a migration, see the README.
//!
//! [teloxide]: https://github.com/YouKnow-sys/teloxide
//! [Cloudflare Worker]: https://developers.cloudflare.com/workers/
//! [`Dispatcher::dispatch_one`]:
//! teloxide::dispatching::Dispatcher::dispatch_one

pub use teloxide_cf_workers_macros::bot;

pub use worker;

#[cfg(feature = "distribution")]
pub mod durable;
pub mod runtime;
#[cfg(feature = "storage")]
pub mod storage;

use std::future::Future;
use std::pin::Pin;
use std::sync::OnceLock;

use teloxide::dispatching::Dispatcher;
use teloxide::prelude::Requester;
use teloxide::types::Update;

#[doc(hidden)]
pub trait DispatchDyn {
    fn dispatch_update(&self, update: Update) -> Pin<Box<dyn Future<Output = ()> + '_>>;
}

impl<R, Err, Key> DispatchDyn for Dispatcher<R, Err, Key>
where
    R: Requester + Clone + Send + Sync + 'static,
    Err: Send + Sync + 'static,
    Key: std::hash::Hash + Eq + Clone + Send,
{
    fn dispatch_update(&self, update: Update) -> Pin<Box<dyn Future<Output = ()> + '_>> {
        Box::pin(self.dispatch_one(update))
    }
}

/// Accepts the `#[bot]` fn's return value: a bare dispatcher or `Result`.
#[doc(hidden)]
pub trait IntoDispatcher {
    fn into_dispatcher(self) -> worker::Result<Box<dyn DispatchDyn>>;
}

impl<R, Err, Key> IntoDispatcher for Dispatcher<R, Err, Key>
where
    R: Requester + Clone + Send + Sync + 'static,
    Err: Send + Sync + 'static,
    Key: std::hash::Hash + Eq + Clone + Send + 'static,
{
    fn into_dispatcher(self) -> worker::Result<Box<dyn DispatchDyn>> {
        Ok(Box::new(self))
    }
}

impl<R, Err, Key, E> IntoDispatcher for Result<Dispatcher<R, Err, Key>, E>
where
    R: Requester + Clone + Send + Sync + 'static,
    Err: Send + Sync + 'static,
    Key: std::hash::Hash + Eq + Clone + Send + 'static,
    E: std::fmt::Display,
{
    fn into_dispatcher(self) -> worker::Result<Box<dyn DispatchDyn>> {
        self.map(|dp| Box::new(dp) as Box<dyn DispatchDyn>)
            .map_err(|e| worker::Error::from(e.to_string()))
    }
}

#[doc(hidden)]
pub type BuildFuture =
    Pin<Box<dyn Future<Output = worker::Result<Box<dyn DispatchDyn>>> + 'static>>;

#[doc(hidden)]
pub type DistributionKey = fn(&Update) -> Option<String>;

#[doc(hidden)]
pub struct BotDef {
    pub path: &'static str,
    pub secret_binding: &'static str,
    pub distribution: Option<(&'static str, DistributionKey)>,
    pub verify_secret: bool,
    pub build: fn(worker::Env) -> BuildFuture,
}

static DEF: OnceLock<BotDef> = OnceLock::new();

#[doc(hidden)]
pub const DISTRIBUTION_ENABLED: bool = cfg!(feature = "distribution");

#[doc(hidden)]
pub fn __register(def: BotDef) {
    console_error_panic_hook::set_once();
    let _ = DEF.set(def);
}
