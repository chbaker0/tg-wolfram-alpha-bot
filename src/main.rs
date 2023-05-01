#![feature(never_type)]

mod http_service;
mod telegram;
mod wolfram;

use telegram::Client;

use std::sync::Arc;

use eyre::{bail, Context};
// use tower::Service;
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

    let reqw = reqwest::Client::new();
    let wolf = Arc::new(wolfram::Api::new(reqw.clone(), WOLFRAM_KEY.trim_end()));

    let client = http_service::Client::new(reqw);
    let tg = Arc::new(telegram::Bot::new(TELEGRAM_KEY.trim_end()).on(client.clone()));

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
        handle_request(tg.as_ref(), &wolf, msg).await;
    }

    tg_update_task.await?
}

#[instrument(skip(tg, wolfram))]
async fn handle_request<Tg: telegram::Client>(
    tg: &Tg,
    wolfram: &wolfram::Api,
    msg: telegram::Message,
) where
    Tg::Error: std::error::Error + Send + Sync + 'static,
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

async fn handle_request_impl<Tg: telegram::Client>(
    tg: &Tg,
    wolfram: &wolfram::Api,
    msg: &telegram::Message,
) -> eyre::Result<()>
where
    Tg::Error: std::error::Error + Send + Sync + 'static,
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

    match wolfram.query(text.to_string()).await {
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

async fn update_streamer<Tg: telegram::Client>(
    tg: &Tg,
    sink: chan::Sender<telegram::Update>,
) -> eyre::Result<()>
where
    Tg::Error: std::error::Error + Send + Sync + 'static,
{
    let timeout = 30;

    // Keep track of the number of consecutive failed requests. Retry until
    // max_errs.
    let max_errs = 3;
    let mut err_count = 0;

    // The first getUpdates call technically should have no `offset` arg.
    let mut offset = None;

    loop {
        let batch = match async {
            tracing::info!(offset);
            tg.call(telegram::GetUpdates { offset, timeout }).await
        }
        .await
        {
            Ok(b) => {
                err_count = 0;
                b
            }
            Err(telegram::ApiError::RetryAfter(d)) => {
                // Don't count this as an error. Telegram gave us a well-formed
                // response asking us to wait.
                err_count = 0;
                tokio::time::sleep(d).await;
                continue;
            }
            Err(e) => {
                // Got an error that can't be handled. Log an retry, until the
                // max number of errors.
                error!(e = ?e, "Telegram API error");
                err_count += 1;
                if err_count == max_errs {
                    error!("Reached max number of retries");
                    bail!(e);
                }
                continue;
            }
        };

        for u in batch.into_iter() {
            // Bump the offset we call getUpdates with. This implicitly
            // acknowledges the updates received upon the next call.
            // Conveniently, `None` compares less than `Some(_)`.
            offset = std::cmp::max(offset, Some(u.update_id + 1));
            tracing::info!(offset, u = ?u);

            sink.send(u).await?;
        }
    }
}

static TELEGRAM_KEY: &str = include_str!("../.keys/telegram");
static WOLFRAM_KEY: &str = include_str!("../.keys/wolframalpha");

#[cfg(test)]
mod test_util;
