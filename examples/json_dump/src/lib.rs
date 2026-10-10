//! Example bot: replies to every update with the update itself,
//! pretty-printed as JSON and split across messages. Updates are distributed
//! through a Durable Object per chat, so each chat is handled one at a time.

mod split;

use teloxide::adaptors::CacheMe;
use teloxide::dispatching::{DefaultKey, Dispatcher};
use teloxide::prelude::*;
use teloxide::types::{ParseMode, Update};
use teloxide::utils;
use teloxide_cf_workers::{
    bot,
    durable::default_key,
    runtime::console_error_handler,
    worker::{self, Env},
};

type HandlerError = Box<dyn std::error::Error + Send + Sync>;

#[bot(distribution = default_key)]
async fn main(env: Env) -> worker::Result<Dispatcher<CacheMe<Bot>, HandlerError, DefaultKey>> {
    // CacheMe caches `get_me`, which `dispatch_one` otherwise calls on every
    // update.
    let token = env.secret("BOT_TOKEN")?.to_string();
    let bot = Bot::new(token).cache_me();

    Ok(
        Dispatcher::builder(bot, dptree::entry().endpoint(dump_update))
            .error_handler(console_error_handler())
            .build(),
    )
}

async fn dump_update(bot: CacheMe<Bot>, upd: Update) -> Result<(), HandlerError> {
    let Some(chat) = upd.chat() else {
        return Ok(());
    };

    let json = serde_json::to_string_pretty(&upd)?;
    for chunk in split::chunks(&json) {
        bot.send_message(chat.id, utils::html::code_block_with_lang(&chunk, "json"))
            .parse_mode(ParseMode::Html)
            .await?;
    }

    Ok(())
}
