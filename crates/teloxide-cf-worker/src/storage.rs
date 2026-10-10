use std::{
    fmt::{Debug, Display},
    sync::Arc,
};

use serde::{Serialize, de::DeserializeOwned};
use teloxide::{
    dispatching::dialogue::{Storage, serializer::Serializer},
    send::MaybeSendBoxFuture,
    types::ChatId,
};
use worker::{d1::D1Database, js_sys::Uint8Array, wasm_bindgen::JsValue};

#[derive(Debug, thiserror::Error)]
pub enum D1StorageError<SE>
where
    SE: Debug + Display,
{
    #[error("dialogue serialization error: {0}")]
    SerdeError(SE),

    #[error("worker error: {0}")]
    WorkerError(#[from] worker::Error),

    #[error("row not found")]
    DialogueNotFound,
}

/// A persistent dialogue storage based on [Cloudflare D1].
///
/// The `teloxide_dialogues` table must exist; create it with a D1 migration.
///
/// [Cloudflare D1]: https://developers.cloudflare.com/d1
pub struct D1Storage<S> {
    db: D1Database,
    serializer: S,
}

impl<S> D1Storage<S> {
    /// Creates the storage on top of a D1 binding.
    ///
    /// Parameters:
    /// - db: the D1 binding, for example `env.d1("DB")?`
    /// - serializer: what [`Serializer`] will be used to encode the dialogue
    ///   data, for example [`Json`]
    ///
    /// [`Serializer`]: teloxide::dispatching::dialogue::serializer::Serializer
    /// [`Json`]: teloxide::dispatching::dialogue::serializer::Json
    pub fn new(db: D1Database, serializer: S) -> Arc<Self> {
        Arc::new(Self { db, serializer })
    }
}

// telegram ids fit in 52 bits, so the f64 is exact.
fn chat_id_js(ChatId(id): ChatId) -> JsValue {
    JsValue::from_f64(id as f64)
}

impl<S, D> Storage<D> for D1Storage<S>
where
    S: Send + Sync + Serializer<D> + 'static,
    D: Send + Serialize + DeserializeOwned + 'static,
    <S as Serializer<D>>::Error: Debug + Display,
{
    type Error = D1StorageError<<S as Serializer<D>>::Error>;

    fn remove_dialogue(
        self: Arc<Self>,
        chat_id: ChatId,
    ) -> MaybeSendBoxFuture<'static, Result<(), Self::Error>>
    where
        D: Send + 'static,
    {
        Box::pin(async move {
            let deleted = self
                .db
                .prepare("DELETE FROM teloxide_dialogues WHERE chat_id = ?1 RETURNING chat_id")
                .bind(&[chat_id_js(chat_id)])?
                .first::<f64>(Some("chat_id"))
                .await?;

            deleted.map(|_| ()).ok_or(D1StorageError::DialogueNotFound)
        })
    }

    fn update_dialogue(
        self: Arc<Self>,
        chat_id: ChatId,
        dialogue: D,
    ) -> MaybeSendBoxFuture<'static, Result<(), Self::Error>>
    where
        D: Send + 'static,
    {
        Box::pin(async move {
            let bytes = self
                .serializer
                .serialize(&dialogue)
                .map_err(D1StorageError::SerdeError)?;

            self.db
                .prepare(
                    "INSERT INTO teloxide_dialogues (chat_id, dialogue) VALUES (?1, ?2) \
                     ON CONFLICT (chat_id) DO UPDATE SET dialogue = excluded.dialogue",
                )
                .bind(&[
                    chat_id_js(chat_id),
                    Uint8Array::new_from_slice(&bytes).into(),
                ])?
                .run()
                .await?;

            Ok(())
        })
    }

    fn get_dialogue(
        self: Arc<Self>,
        chat_id: ChatId,
    ) -> MaybeSendBoxFuture<'static, Result<Option<D>, Self::Error>> {
        Box::pin(async move {
            let bytes = self
                .db
                .prepare("SELECT dialogue FROM teloxide_dialogues WHERE chat_id = ?1")
                .bind(&[chat_id_js(chat_id)])?
                .first::<Vec<u8>>(Some("dialogue"))
                .await?;

            bytes
                .map(|b| {
                    self.serializer
                        .deserialize(&b)
                        .map_err(D1StorageError::SerdeError)
                })
                .transpose()
        })
    }
}
