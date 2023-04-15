mod telegram;
mod wolfram;

use std::sync::Arc;

use tracing::{error, instrument, warn};

use tokio::sync::mpsc as chan;

#[tokio::main]
async fn main() -> eyre::Result<()> {
    color_eyre::install()?;

    tracing_subscriber::fmt::init();

    let tg = Arc::new(telegram::Api::new(TELEGRAM_KEY.trim_end()));
    let wolf = Arc::new(wolfram::Api::new(WOLFRAM_KEY.trim_end()));

    let reqw = reqwest::Client::new();

    let me = tg.get_me(&reqw).await?;
    println!("ID: {}", me.id);
    eyre::ensure!(me.is_bot, "we're not a bot?");

    let (sender, mut receiver) = chan::channel(100);
    let tg_update_task = tokio::spawn({
        let tg = tg.clone();
        let reqw = reqw.clone();
        async move { update_streamer(&tg, reqw, sender).await }
    });

    while let Some(u) = receiver.recv().await {
        let Some(msg) = u.message else {continue};

        // Spawn and ignore the handle, since the task doesn't return anything and
        // logs any errors.
        tokio::spawn({
            let reqw = reqw.clone();
            let tg = tg.clone();
            let wolf = wolf.clone();
            async move { handle_request(reqw, &tg, &wolf, msg).await }
        });
    }

    tg_update_task.await?
}

#[instrument(skip(reqw, tg, wolfram))]
async fn handle_request(
    reqw: reqwest::Client,
    tg: &telegram::Api,
    wolfram: &wolfram::Api,
    msg: telegram::Message,
) {
    match handle_request_impl(reqw, tg, wolfram, &msg).await {
        Ok(()) => (),
        Err(report) => {
            error!(
                msg.text, msg.chat.id,
                report = ?report, "error handling Telegram query"
            );
        }
    }
}

async fn handle_request_impl(
    reqw: reqwest::Client,
    tg: &telegram::Api,
    wolfram: &wolfram::Api,
    msg: &telegram::Message,
) -> eyre::Result<()> {
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

    match wolfram.query(&reqw, text.to_string()).await {
        Ok(resp) => {
            tg.send_photo(
                &reqw,
                msg.chat.id,
                msg.message_id,
                resp.image_data,
                resp.content_type,
            )
            .await
        }
        Err(report) => {
            warn!(report = ?report, "Wolfram Alpha request failed (maybe it couldn't handle the query?)");
            tg.send_message(
                &reqw,
                msg.chat.id,
                msg.message_id,
                "Wolfram Alpha could not process this query".to_string(),
            )
            .await
        }
    }
}

async fn update_streamer(
    api: &telegram::Api,
    client: reqwest::Client,
    sink: chan::Sender<telegram::Update>,
) -> eyre::Result<()> {
    let poll_timeout = 30;

    // The first getUpdates call technically should have no `offset` arg.
    let mut offset = None;

    loop {
        for u in api
            .get_updates(&client, offset, poll_timeout)
            .await?
            .into_iter()
        {
            // Bump the offset we call getUpdates with. This implicitly
            // acknowledges the updates received upon the next call.
            // Conveniently, `None` compares less than `Some(_)`.
            offset = std::cmp::max(offset, Some(u.update_id + 1));
            println!("{u:?}");

            sink.send(u).await?;
        }
    }
}

static TELEGRAM_KEY: &str = include_str!("../.keys/telegram");
static WOLFRAM_KEY: &str = include_str!("../.keys/wolframalpha");
