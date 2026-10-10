# teloxide-cf-workers

Run a [teloxide] bot as a [Cloudflare Worker]. The `#[bot]` attribute macro
generates the webhook entrypoint (secret-token check, update parsing,
dispatcher cache) around the function where you build your bot. A complete,
deployable bot is in [`examples/json_dump`](examples/json_dump).

Optional cargo features: `distribution` (per-chat serialization with Durable
Objects) and `storage` (dialogue storage in D1).

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

- The `#[bot]` fn runs once per isolate, so keep it cheap. It may return the
  dispatcher directly or a `Result` of it; an `Err` is logged and
  requests answer 500 until the fn succeeds.
- `CacheMe` saves a `get_me` call per update; passing `Me` through
  `.dependencies(..)` works too.

## Registering the webhook

Register the webhook once:

```sh
curl -X POST "https://api.telegram.org/bot$BOT_TOKEN/setWebhook" \
  -d url="https://<your-worker>.workers.dev/webhook" \
  -d secret_token="$WEBHOOK_SECRET" \
  -d drop_pending_updates=true
```

- `secret_token` (1-256 chars of `A-Za-z0-9_-`) must equal the worker's
  `WEBHOOK_SECRET` secret. A wrong token gets a 403, an unset secret a 500.
- The URL path must match `#[bot(path = ...)]` (default `/webhook`).
- Teloxide's own listeners derive `allowed_updates` from your handlers; here
  you should pass it yourself, for example
  `-d allowed_updates='["message","callback_query","chat_member"]'`.

## Per-chat serialization (Durable Objects)

Updates are handled concurrently by default. To handle one chat's updates one
at a time (dialogues, per-chat state), enable the `distribution` feature, name
a key function on `#[bot]`, and add the Durable Object binding.

```toml
teloxide-cf-workers = { git = "https://github.com/YouKnow-sys/teloxide-cf-workers", features = ["distribution"] }
```

```rust
#[bot(distribution = teloxide_cf_workers::durable::default_key)]
```

```toml
# wrangler.toml
[durable_objects]
bindings = [{ name = "TELOXIDE_QUEUE", class_name = "TeloxideUpdateDistributor" }]

[[migrations]]
tag = "v1"
new_sqlite_classes = ["TeloxideUpdateDistributor"]
```

Use `new_sqlite_classes` on every plan (required on free, recommended on
paid). `default_key` groups by chat id; write your own
`fn(&Update) -> Option<String>` to group differently, where `None` handles
that update inline.

- Handlers for one key never overlap. Order is the order updates reach the
  Durable Object, which can differ from Telegram's when it delivers
  concurrently.
- At most 64 updates wait per key; more get a 503 and Telegram retries later.
- Each update costs one extra Durable Object request.
- Without `distribution` there is no Durable Object, binding or migration. The
  alternative is `-d max_connections=1` in `setWebhook`, which makes Telegram
  send one update at a time, but then a slow handler stalls the whole bot.

## Dialogue storage (D1)

`D1Storage` (feature `storage`) stores teloxide dialogues in D1 and implements
teloxide's `Storage` trait. Bind the database:

```toml
[[d1_databases]]
binding = "DB"
database_name = "<db-name>"
database_id = "<id>"
```

The library never creates the table at runtime. Create the database, then
paste the SQL below into the migration file that `migrations create`
generates, before applying it:

```console
npx wrangler d1 create <db-name>
npx wrangler d1 migrations create <db-name> add_dialogues
npx wrangler d1 migrations apply <db-name> --local
npx wrangler d1 migrations apply <db-name> --remote
```

```sql
CREATE TABLE teloxide_dialogues (
    chat_id  INTEGER PRIMARY KEY,
    dialogue BLOB NOT NULL
) STRICT;
```

Then build the storage from the binding and a serializer:

```rust
use teloxide::dispatching::dialogue::serializer::Json;
use teloxide_cf_workers::storage::D1Storage;

let storage = D1Storage::new(env.d1("DB")?, Json);

Dispatcher::builder(
    bot,
    dptree::entry()
        .branch(dialogue::enter::<Update, D1Storage<Json>, State, _>().endpoint(handler)),
)
.dependencies(dptree::deps![storage])
.build()
```

- Combine it with `distribution`: without per-chat serialization, concurrent
  updates for one chat can overwrite each other's dialogue state.
- `no such table: teloxide_dialogues` means the migration was not applied.

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
  D1 dialogue storage for dialogues, and KV, D1 or Durable Object storage for
  other state.
- Updates are acknowledged after handling, under a 25s handler timeout.
  A handler that is cancelled by the timeout still has its update
  acknowledged, and one slower than Telegram's webhook timeout can cause
  duplicate deliveries. Keep handlers short and idempotent, and move long jobs
  to a Cloudflare Queue or Workflow.
- Handler errors go to the dispatcher's error handler. Teloxide's default
  logger has no output on Workers, so use `console_error_handler()`.
- One `#[bot]` per crate.

[teloxide]: https://github.com/YouKnow-sys/teloxide/tree/wasm
[Cloudflare Worker]: https://developers.cloudflare.com/workers/
