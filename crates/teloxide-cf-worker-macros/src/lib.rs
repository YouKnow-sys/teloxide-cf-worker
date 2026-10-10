//! Attribute macro for the `teloxide-cf-worker` runtime crate. See that
//! crate's docs; it is re-exported there as `#[bot]`.

use proc_macro::TokenStream;
use quote::quote;
use syn::parse::{Parse, Parser};
use syn::punctuated::Punctuated;
use syn::{Expr, ExprLit, ItemFn, Lit, LitBool, ReturnType, Token};

/// One `key = value` argument of the `#[bot]` attribute.
struct BotArg {
    key: syn::Ident,
    value: Expr,
}

impl Parse for BotArg {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let key = input.parse()?;
        input.parse::<Token![=]>()?;
        let value = input.parse()?;
        Ok(BotArg { key, value })
    }
}

/// Generates the worker entrypoint around the fn that builds the bot's
/// dispatcher.
///
/// Put it on an `async fn(Env) -> Dispatcher<...>` that constructs the bot,
/// the handler tree and the dispatcher, and returns `Dispatcher::builder(..).build()`.
/// A `Result<Dispatcher<..>, E>` return works too: a failure is logged and
/// requests answer 500 until the fn succeeds once.
///
/// The fn runs once per isolate (the worker's and each Durable Object's), so
/// it better if its cheap. Its result is cached and every incoming update is
/// dispatched through it with `Dispatcher::dispatch_one`.
///
/// Generated: a startup hook that registers the bot (and installs the panic
/// hook). Everything else already lives in the library: the `fetch`
/// entrypoint, the webhook secret check, update parsing, the per-isolate
/// dispatcher cache and, for `distribution`, the Durable Object class. The
/// crate must depend on `teloxide`.
///
/// # Arguments
///
/// All optional, `key = value`:
/// - `distribution = path`: per-key serialized handling through Durable
///   Objects; needs `features = ["distribution"]` on the teloxide-cf-worker
///   dependency (using it without the feature is a compile error, and so is
///   referencing `teloxide_cf_worker::durable::default_key`).
///   `fn(&Update) -> Option<String>` names the Durable Object instance per
///   update: `Some(key)` is forwarded to instance `key` and handled one at a
///   time (mutual exclusion; arrival order across isolates is best-effort),
///   `None` is handled inline in the worker fetch. Without this argument
///   every update is dispatched directly.
/// - `binding = "NAME"` (default `"TELOXIDE_QUEUE"`, only with
///   `distribution`): the Durable Object binding. wrangler.toml needs:
///
///   ```toml
///   [durable_objects]
///   bindings = [{ name = "TELOXIDE_QUEUE", class_name = "TeloxideUpdateDistributor" }]
///
///   [[migrations]]
///   tag = "v1"
///   new_sqlite_classes = ["TeloxideUpdateDistributor"]
///   ```
///
///   SQLite-backed classes are required on the free plan; the class doesn't
///   need to use storage to be SQLite-backed.
/// - `secret = "NAME"` (default `"WEBHOOK_SECRET"`): binding holding the
///   Telegram `secret_token`. A mismatching header is a 403, and an unset
///   binding is a logged 500; the webhook is registered externally (see the
///   README).
/// - `verify_secret = false`: allow an unset secret binding, skipping the
///   check. Insecure (anyone who finds the URL can post updates); for local
///   development only.
/// - `path = "/webhook"`: the webhook path.
///
/// # Example
///
/// ```ignore
/// use teloxide::adaptors::CacheMe;
/// use teloxide::dispatching::{DefaultKey, Dispatcher};
/// use teloxide::prelude::*;
/// use teloxide_cf_worker::{bot, runtime::console_error_handler, worker::Env};
///
/// type HandlerError = Box<dyn std::error::Error + Send + Sync>;
///
/// #[bot(distribution = teloxide_cf_worker::durable::default_key)]
/// async fn main(env: Env)
/// -> Dispatcher<CacheMe<Bot>, HandlerError, DefaultKey> {
///     let bot: CacheMe<Bot> =
///         Bot::new(env.secret("BOT_TOKEN").expect("BOT_TOKEN").to_string()).cache_me();
///
///     Dispatcher::builder(bot, dptree::entry().endpoint(hello))
///         .error_handler(console_error_handler())
///         .build()
/// }
/// ```
///
/// Handler errors go to the dispatcher's error handler: set one that logs
/// (e.g. `teloxide_cf_worker::runtime::console_error_handler`), or they are
/// invisible on Workers.
#[proc_macro_attribute]
pub fn bot(attr: TokenStream, item: TokenStream) -> TokenStream {
    match try_expand(attr, item) {
        Ok(tokens) => tokens,
        Err(err) => err.to_compile_error().into(),
    }
}

fn try_expand(attr: TokenStream, item: TokenStream) -> syn::Result<TokenStream> {
    let args = Punctuated::<BotArg, Token![,]>::parse_terminated.parse(attr)?;
    let item_fn = syn::parse::<ItemFn>(item)?;

    if item_fn.sig.inputs.len() != 1 {
        return Err(syn::Error::new_spanned(
            &item_fn.sig.inputs,
            "the #[bot] fn must take exactly one argument, `env: Env`",
        ));
    }
    if item_fn.sig.asyncness.is_none() {
        return Err(syn::Error::new_spanned(
            item_fn.sig.fn_token,
            "the #[bot] fn must be async",
        ));
    }
    if !item_fn.sig.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &item_fn.sig.generics,
            "the #[bot] fn must not be generic",
        ));
    }
    if matches!(&item_fn.sig.output, ReturnType::Default) {
        return Err(syn::Error::new_spanned(
            &item_fn.sig,
            "the #[bot] fn must return the Dispatcher (`Dispatcher::builder(bot, handler).build()`) or a `Result` of it",
        ));
    }

    let mut secret = None;
    let mut path = None;
    let mut binding = None;
    let mut distribution = None;
    let mut verify_secret = None;

    for BotArg { key, value } in args {
        match key.to_string().as_str() {
            "secret" => set_arg(&mut secret, "secret", value, expr_to_str)?,
            "path" => set_arg(&mut path, "path", value, expr_to_str)?,
            "distribution" => set_arg(&mut distribution, "distribution", value, expr_to_path)?,
            "binding" => set_arg(&mut binding, "binding", value, expr_to_str)?,
            "verify_secret" => set_arg(&mut verify_secret, "verify_secret", value, expr_to_bool)?,
            unknown => {
                return Err(syn::Error::new_spanned(
                    key,
                    format!("unknown `#[bot]` argument `{unknown}`"),
                ));
            }
        }
    }

    if let (Some(binding), None) = (&binding, &distribution) {
        return Err(syn::Error::new_spanned(
            binding,
            "`binding` requires `distribution`: without it updates are dispatched directly",
        ));
    }

    let secret = match secret {
        Some(secret) => quote! { #secret },
        None => quote! { ::teloxide_cf_worker::runtime::DEFAULT_SECRET_BINDING },
    };
    let path = match path {
        Some(path) => quote! { #path },
        None => quote! { ::teloxide_cf_worker::runtime::DEFAULT_WEBHOOK_PATH },
    };
    let verify_secret = match verify_secret {
        Some(verify_secret) => quote! { #verify_secret },
        None => quote! { true },
    };
    let distribution_arg = match &distribution {
        Some(distribution) => {
            let binding = match binding {
                Some(binding) => quote! { #binding },
                None => quote! { ::teloxide_cf_worker::runtime::DEFAULT_DO_BINDING },
            };
            quote! { ::std::option::Option::Some((#binding, #distribution)) }
        }
        None => quote! { ::std::option::Option::None },
    };

    let item_ident = &item_fn.sig.ident;

    let feature_check = match &distribution {
        Some(_) => quote! {
            const _: () = ::std::assert!(
                ::teloxide_cf_worker::DISTRIBUTION_ENABLED,
                "`#[bot(distribution = ...)]` needs the `distribution` feature of teloxide-cf-worker",
            );
        },
        None => quote! {},
    };

    let start = quote! {
        // Through the prelude: the bare re-export path lacks the attribute macro.
        #[::teloxide_cf_worker::worker::wasm_bindgen::prelude::wasm_bindgen(
            start,
            wasm_bindgen = ::teloxide_cf_worker::worker::wasm_bindgen,
        )]
        fn __teloxide_cf_worker_start() {
            ::teloxide_cf_worker::__register(::teloxide_cf_worker::BotDef {
                path: #path,
                secret_binding: #secret,
                distribution: #distribution_arg,
                verify_secret: #verify_secret,
                build: |env| {
                    ::std::boxed::Box::pin(async move {
                        ::teloxide_cf_worker::IntoDispatcher::into_dispatcher(
                            #item_ident(env).await,
                        )
                    })
                },
            });
        }
    };

    Ok(quote! {
        #item_fn

        #feature_check

        #start
    }
    .into())
}

fn set_arg<T>(
    slot: &mut Option<T>,
    key: &str,
    value: Expr,
    cast: fn(Expr, &str) -> syn::Result<T>,
) -> syn::Result<()> {
    if slot.is_some() {
        return Err(syn::Error::new_spanned(
            value,
            format!("`{key}` given more than once"),
        ));
    }
    *slot = Some(cast(value, key)?);
    Ok(())
}

fn expr_to_path(value: Expr, key: &str) -> syn::Result<syn::Path> {
    match &value {
        Expr::Path(path) if path.attrs.is_empty() && path.qself.is_none() => Ok(path.path.clone()),
        _ => Err(syn::Error::new_spanned(
            value,
            format!("`{key}` expects a function path"),
        )),
    }
}

fn expr_to_str(value: Expr, key: &str) -> syn::Result<syn::LitStr> {
    match &value {
        Expr::Lit(ExprLit {
            lit: Lit::Str(s), ..
        }) => Ok(s.clone()),
        _ => Err(syn::Error::new_spanned(
            value,
            format!("`{key}` expects a string literal"),
        )),
    }
}

fn expr_to_bool(value: Expr, key: &str) -> syn::Result<LitBool> {
    match &value {
        Expr::Lit(ExprLit {
            lit: Lit::Bool(b), ..
        }) => Ok(b.clone()),
        _ => Err(syn::Error::new_spanned(
            value,
            format!("`{key}` expects a bool literal"),
        )),
    }
}
