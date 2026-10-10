# teloxide-cf-workers

Run a [teloxide] bot as a [Cloudflare Worker]. One attribute macro, `#[bot]`,
generates the webhook entrypoint (secret-token check, update parsing,
dispatcher cache) around the function where you build your bot and dispatcher.
See [`examples/json_dump`](examples/json_dump) for a complete, deployable
bot.

## Quickstart

`Cargo.toml`:

```toml
[lib]
crate-type = ["cdylib"]

[dependencies]
teloxide = { git = "https://github.com/YouKnow-sys/teloxide", branch = "wasm", default-features = false, features = ["cache-me"] }
teloxide-cf-workers = { git = "https://github.com/YouKnow-sys/teloxide-cf-workers" }
```

`src/lib.rs`:

```rust
use teloxide::adaptors::CacheMe;
use teloxide::dispatching::{DefaultKey, Dispatcher};
use teloxide::prelude::*;
use teloxide_cf_workers::{
    bot,
    runtime::console_error_handler,
    worker::{self, Env},
};

type HandlerError = Box<dyn std::error::Error + Send + Sync>;

#[bot]
async fn main(env: Env) -> worker::Result<Dispatcher<CacheMe<Bot>, HandlerError, DefaultKey>> {
    let bot = Bot::new(env.secret("BOT_TOKEN")?.to_string()).cache_me();

    Ok(Dispatcher::builder(bot, dptree::entry().endpoint(hello))
        .error_handler(console_error_handler())
        .build())
}

async fn hello(bot: CacheMe<Bot>, upd: Update) -> Result<(), HandlerError> {
    if let Some(chat) = upd.chat() {
        bot.send_message(chat.id, "hello from the edge").await?;
    }
    Ok(())
}
```

`wrangler.toml`:

```toml
name = "my-bot"
main = "build/index.js"
compatibility_date = "2026-09-01"

[build]
command = "cargo install worker-build && worker-build --release"
```

Deploy:

```console
rustup target add wasm32-unknown-unknown
npx wrangler secret put BOT_TOKEN
npx wrangler secret put WEBHOOK_SECRET
npx wrangler deploy
```

The `#[bot]` fn runs once per isolate, so keep it cheap. Returning a `Result`
is optional; an `Err` is logged and requests answer 500 until the fn succeeds.
`CacheMe` avoids a `get_me` call on every update; you can pass `Me` through
`.dependencies(..)` instead. A handler running longer than 25 s is cancelled
and its update acknowledged, both inline and in a Durable Object.

## Registering the webhook

Register the webhook once.

```sh
curl -X POST "https://api.telegram.org/bot$BOT_TOKEN/setWebhook" \
  -d url="https://<your-worker>.workers.dev/webhook" \
  -d secret_token="$WEBHOOK_SECRET" \
  -d drop_pending_updates=true
```

- `secret_token` (1-256 chars of `A-Za-z0-9_-`) must equal the worker's
  `WEBHOOK_SECRET` secret. A wrong token gets a 403; an unset secret gets a 500.
- The URL path must match `#[bot(path = ...)]` (default `/webhook`).
- `wrangler dev` needs `WEBHOOK_SECRET` in `.dev.vars`.
- `-d max_connections=1` makes Telegram send one update at a time, with no
  Durable Objects needed. A slow handler then stalls the whole bot.

## Per-chat serialization (Durable Objects)

By default updates are handled concurrently. To handle one chat's updates one
at a time (dialogues, per-chat state), enable the `distribution` feature, pass
a key function, and add the Durable Object binding.

```toml
teloxide-cf-workers = { git = "https://github.com/YouKnow-sys/teloxide-cf-workers", features = ["distribution"] }
```

```toml
# wrangler.toml
[durable_objects]
bindings = [{ name = "TELOXIDE_QUEUE", class_name = "TeloxideUpdateDistributor" }]

[[migrations]]
tag = "v1"
new_sqlite_classes = ["TeloxideUpdateDistributor"]
```

Use `new_sqlite_classes` on every plan (required on free, recommended on paid).

```rust
fn user_key(update: &Update) -> Option<String> {
    update.from().map(|u| u.id.to_string())
}

#[bot(distribution = user_key)]
async fn main(env: Env) -> worker::Result<Dispatcher<CacheMe<Bot>, HandlerError, DefaultKey>> {
    /* ... */
}
```

Updates for which the function returns `None` are handled inline, concurrently.
`teloxide_cf_workers::durable::default_key` groups by chat id.

- Handlers for one key never overlap. Order is the order updates reach the
  Durable Object, which can differ from Telegram's when it delivers
  concurrently.
- The webhook response is sent after the update is handled, so Telegram
  re-delivers if the worker dies mid-handling.
- The same 25 s handler timeout applies, so a hung handler can't stall the
  key's queue.
- At most 64 updates wait per key; more get a 503 and Telegram retries later.
- Each update costs one extra Durable Object request.

Without `distribution` no Durable Object is used: no binding, no migration.

## `#[bot]` arguments

| argument        | default            | description |
|-----------------|--------------------|-------------|
| `distribution`  | none               | `fn(&Update) -> Option<String>` naming the Durable Object per update. Needs the `distribution` feature. |
| `binding`       | `"TELOXIDE_QUEUE"` | Durable Object binding name. Only with `distribution`. |
| `secret`        | `"WEBHOOK_SECRET"` | Binding holding the Telegram `secret_token`. |
| `verify_secret` | `true`             | `false` allows an unset secret binding. Insecure; local development only. |
| `path`          | `"/webhook"`       | Webhook path. `GET /` answers a 200 health check. |

## Caveats

- `InMemStorage` and other in-process state are per isolate and ephemeral. Use
  KV, D1 or Durable Object storage for dialogues and other state.
- Updates are acknowledged only after handling, so a handler slower than
  Telegram's webhook timeout can cause duplicate deliveries.
- Handler errors go to the dispatcher's error handler. Teloxide's default
  logger has no output on Workers, so use `console_error_handler()`.
- One `#[bot]` per crate.

[teloxide]: https://github.com/YouKnow-sys/teloxide/tree/wasm
[Cloudflare Worker]: https://developers.cloudflare.com/workers/
