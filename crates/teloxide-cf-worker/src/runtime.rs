//! Webhook plumbing: the worker's `fetch` entrypoint, shared by plain and
//! distributed mode.

use std::sync::Arc;

use futures_util::future::{Either, select};
use teloxide::error_handlers::ErrorHandler;
use teloxide::types::Update;
use worker::send::SendWrapper;
use worker::{Env, Method, Request, Response};

use crate::{DEF, DispatchDyn};

#[cfg(feature = "distribution")]
use crate::durable;

/// Header Telegram sends with every update when the webhook was registered
/// with a `secret_token`.
pub const SECRET_HEADER: &str = "x-telegram-bot-api-secret-token";

/// Default name of the binding holding the Telegram `secret_token`
/// (`#[bot(secret = ...)]`).
pub const DEFAULT_SECRET_BINDING: &str = "WEBHOOK_SECRET";

/// Default webhook path (`#[bot(path = ...)]`).
pub const DEFAULT_WEBHOOK_PATH: &str = "/webhook";

/// Default Durable Object binding name (`#[bot(binding = ...)]`); must match
/// wrangler.toml.
pub const DEFAULT_DO_BINDING: &str = "TELOXIDE_QUEUE";

/// How long a handler may run before it is cancelled. The update is
/// acknowledged anyway, since a retry would likely time out again
pub const HANDLER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(25);

/// An [`ErrorHandler`] that logs handler errors to the Workers console, for
/// `DispatcherBuilder::error_handler`; teloxide's default writes through the
/// `log` crate, which has no backend on Workers.
pub fn console_error_handler<E>() -> Arc<dyn ErrorHandler<E>>
where
    E: std::fmt::Display + Send + 'static,
{
    Arc::new(|err: E| async move {
        worker::console_error!("teloxide-cf-worker: update handler error: {err}");
    })
}

// The dispatcher isn't `Sync` on wasm, so it can't live in a `static` bare;
// `SendWrapper` is sound because a Workers isolate is single-threaded.
static DP: tokio::sync::OnceCell<SendWrapper<Box<dyn DispatchDyn>>> =
    tokio::sync::OnceCell::const_new();

/// The isolate's dispatcher, built from `DEF.build` on first use. A build
/// failure is logged and retried on the next request.
pub(crate) async fn get_dp(env: &Env) -> worker::Result<&'static dyn DispatchDyn> {
    let def = DEF
        .get()
        .ok_or_else(|| worker::Error::from("no #[bot] fn registered"))?;

    Ok(&***DP
        .get_or_try_init(|| async {
            let dp = (def.build)(env.clone()).await.inspect_err(|e| {
                worker::console_error!("teloxide-cf-worker: failed to build the bot: {e}");
            })?;
            Ok::<_, worker::Error>(SendWrapper::new(dp))
        })
        .await?)
}

/// Serves the health check and the webhook: verifies the secret, then
/// dispatches the update inline or forwards it to its Durable Object.
#[::worker::event(fetch)]
async fn worker_fetch(
    mut req: Request,
    env: Env,
    _ctx: worker::Context,
) -> worker::Result<Response> {
    let Some(def) = DEF.get() else {
        return Response::error("no #[bot] fn registered", 500);
    };
    match (req.method(), req.path().as_str()) {
        (Method::Get, "/") => Response::ok("ok"),
        (Method::Post, p) if p == def.path => {
            if let Some((status, message)) =
                secret_check(&env, def.secret_binding, &req, def.verify_secret)
            {
                return Response::error(message, status);
            }
            let bytes = req.bytes().await?;
            // Unparseable bodies are logged by `parse_update`; acknowledge
            // them anyway, since a retry can never succeed.
            let Ok(update) = parse_update(&bytes) else {
                return Response::empty();
            };
            #[cfg(feature = "distribution")]
            if let Some((binding, key)) = def
                .distribution
                .and_then(|(b, key)| key(&update).map(|k| (b, k)))
            {
                return durable::forward(&env, binding, &key, bytes).await;
            }
            let dp = get_dp(&env).await?;
            dispatch_with_timeout(dp, update).await;
            Response::empty()
        }
        _ => Response::error("not found", 404),
    }
}

/// None when the request may pass; Some((status, message)) to reject it: the
/// binding is unset (fail closed) or the header doesn't match.
fn secret_check(
    env: &Env,
    binding: &str,
    req: &Request,
    required: bool,
) -> Option<(u16, &'static str)> {
    let secret = env
        .var(binding)
        .map(|s| s.to_string())
        .ok()
        .filter(|s| !s.is_empty());

    let Some(secret) = secret else {
        if required {
            worker::console_error!(
                "teloxide-cf-worker: secret binding `{binding}` is not set or empty; pass `verify_secret = false` (insecure, local dev only) to allow it"
            );
            return Some((500, "webhook secret not configured"));
        }
        return None;
    };

    match req.headers().get(SECRET_HEADER) {
        Ok(Some(header)) if header == secret => None,
        _ => Some((403, "invalid secret token")),
    }
}

/// Parses a webhook body into a [`teloxide::types::Update`].
///
/// Deliberately not `Request::json()`: worker's JSON path (serde_wasm_bindgen)
/// silently deserializes teloxide updates into `UpdateKind::Error`.
pub fn parse_update(bytes: &[u8]) -> worker::Result<teloxide::types::Update> {
    serde_json::from_slice(bytes).map_err(|e| {
        worker::console_error!("teloxide-cf-worker: failed to parse update: {e}");
        worker::Error::from(format!("failed to parse update: {e}"))
    })
}

pub(crate) async fn dispatch_with_timeout(dp: &dyn DispatchDyn, update: Update) {
    let timer = std::pin::pin!(worker::Delay::from(HANDLER_TIMEOUT));
    if let Either::Right(_) = select(dp.dispatch_update(update), timer).await {
        worker::console_error!(
            "teloxide-cf-worker: handler exceeded {}s and was cancelled; the update was acknowledged",
            HANDLER_TIMEOUT.as_secs()
        );
    }
}
