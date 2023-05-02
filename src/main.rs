#![feature(never_type)]

mod http_service;
mod telegram;
mod wolfram;

use telegram::Client;
use tracing::Instrument;

use std::sync::Arc;

use eyre::{bail, Context};
use futures::FutureExt;
use futures::StreamExt;
use futures::TryStreamExt;
use itertools::Itertools;
use tower::util::ServiceExt;
use tower::Service;
use tracing::{error, instrument};

use tokio::sync::mpsc as chan;

fn setup_tracing() -> eyre::Result<()> {
    color_eyre::install()?;

    {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;
        tracing_subscriber::fmt::fmt()
            .finish()
            .with(tracing_error::ErrorLayer::default())
            .init();
    }

    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> eyre::Result<()> {
    setup_tracing()?;

    let _guard = tokio::task::LocalSet::new().enter();

    let client = http_service::Client::new(reqwest::Client::new());
    let wolf = Arc::new(wolfram::Api::new(client.clone(), WOLFRAM_KEY.trim_end()));
    let tg =
        Arc::new(telegram::Bot::new(TELEGRAM_KEY.trim_end()).on(client.map_response(|r| r.bytes)));

    let me = tg.call(telegram::GetMe).await?;
    println!("ID: {}", me.id);
    eyre::ensure!(me.is_bot, "we're not a bot?");

    let (sender, mut receiver) = chan::channel(100);
    let tg_update_task = tokio::task::spawn({
        let tg = tg.clone();
        async move { update_streamer(tg.as_ref(), sender).await }
    });

    while let Some(u) = receiver.recv().await {
        let Some(msg) = u.message else {continue};

        // Spawn and ignore the handle, since the task doesn't return anything and
        // logs any errors.
        handle_request(tg.as_ref(), wolf.as_ref(), msg).await;
    }

    tg_update_task.await?
}

#[instrument(skip(tg, wolfram))]
async fn handle_request<E, Tg, Wolf>(tg: &Tg, wolfram: Wolf, msg: telegram::Message)
where
    Tg: telegram::Client,
    Tg::Error: std::error::Error + Send + Sync + 'static,
    E: std::error::Error + Send + Sync + 'static,
    Wolf: Service<String, Response = wolfram::SimpleResponse, Error = wolfram::ApiError<E>>,
{
    match handle_request_impl(tg, wolfram, &msg).await {
        Ok(()) => (),
        Err(report) => {
            error!(
                msg.text, msg.chat.id,
                report = ?report, "error handling Telegram query"
            );
        }
    }
}

async fn handle_request_impl<E, Tg, Wolf>(
    tg: &Tg,
    mut wolfram: Wolf,
    msg: &telegram::Message,
) -> eyre::Result<()>
where
    Tg: telegram::Client,
    Tg::Error: std::error::Error + Send + Sync + 'static,
    E: std::error::Error + Send + Sync + 'static,
    Wolf: Service<String, Response = wolfram::SimpleResponse, Error = wolfram::ApiError<E>>,
{
    tracing::info!(msg.text);
    let Some(mut text) = msg.text.as_deref() else {return Ok(())};

    if text.starts_with(['@', '/']) {
        match text.split_once(' ') {
            Some((_, r)) => text = r,
            None => return Ok(()),
        }
    }

    if text.is_empty() {
        return Ok(());
    }

    let send_message = |text| {
        tg.call(telegram::SendMessage {
            chat_id: msg.chat.id,
            reply_to_message_id: msg.message_id,
            text,
        })
    };

    match wolfram.call(text.to_string()).await {
        Ok(resp) => {
            if let Some(q) = telegram::SendPhoto::new(
                msg.chat.id,
                msg.message_id,
                resp.image_data,
                resp.content_type,
            ) {
                tg.call(q).await
            } else {
                send_message("Wolfram Alpha sent a bad image".to_string()).await
            }
        }
        Err(wolfram::ApiError::InvalidQuery) => {
            send_message("Wolfram Alpha could not process this query".to_string()).await
        }
        Err(_) => send_message("Could not contact Wolfram Alpha...try again?".to_string()).await,
    }
    .map(|_| ())
    .wrap_err("telegram api request failed")
}

fn update_stream<'a, S>(
    mut tg: S,
) -> impl futures::Stream<Item = Result<telegram::Update, S::Error>> + 'a
where
    S: Service<telegram::GetUpdates, Response = Vec<telegram::Update>> + 'a,
    S::Future: 'a,
{
    let timeout = 30;

    futures::stream::unfold(None, move |offset| {
        tg.call(telegram::GetUpdates { offset, timeout })
            .map(move |res| {
                let next_offset = match &res {
                    Ok(batch) => batch.iter().map(|u| u.update_id + 1).max(),
                    Err(_) => offset,
                };

                Some((res, next_offset))
            })
    })
    .map(|res| futures::stream::iter(std::iter::once(res).flatten_ok()))
    .flatten()
}

async fn update_streamer<'a, E, S>(tg: S, sink: chan::Sender<telegram::Update>) -> eyre::Result<()>
where
    S: Service<
            telegram::GetUpdates,
            Response = Vec<telegram::Update>,
            Error = telegram::ApiError<E>,
        > + 'a,
    S::Future: 'a,
    E: std::error::Error + Send + Sync + 'static,
{
    // Keep track of the number of consecutive failed requests. Retry until
    // max_errs.
    let max_errs = 3;
    let mut err_count = 0;

    let stream = update_stream(tg);
    futures::pin_mut!(stream);

    loop {
        async {
            let u = stream.next().await.unwrap();

            match u {
                Ok(u) => {
                    err_count = 0;
                    sink.send(u).await?;
                    Ok(())
                }
                Err(telegram::ApiError::RetryAfter(d)) => {
                    // Don't count this as an error. Telegram gave us a well-formed
                    // response asking us to wait.
                    err_count = 0;
                    tokio::time::sleep(d).await;
                    Ok(())
                }
                Err(e) => {
                    // Got an error that can't be handled. Log an retry, until the
                    // max number of errors.
                    error!(e = ?e, "Telegram API error");
                    err_count += 1;
                    if err_count == max_errs {
                        error!("Reached max number of retries");
                        Err(eyre::eyre!(e))
                    } else {
                        Ok(())
                    }
                }
            }
        }
        .instrument(tracing::info_span!("processing update"))
        .await?
    }
}

static TELEGRAM_KEY: &str = include_str!("../.keys/telegram");
static WOLFRAM_KEY: &str = include_str!("../.keys/wolframalpha");

#[cfg(test)]
mod test_util;
