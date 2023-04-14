mod telegram;
mod wolfram;

use std::sync::Arc;

use tokio::sync::mpsc as chan;

#[tokio::main]
async fn main() -> eyre::Result<()> {
    color_eyre::install()?;

    let tg = Arc::new(telegram::Api::new(TELEGRAM_KEY.trim_end()));
    let wolf = wolfram::Api::new(WOLFRAM_KEY.trim_end());

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
        let Some(msg) = &u.message else {continue};
        let Some(mut text) = msg.text.as_deref() else {continue};
        if text.starts_with("@") || text.starts_with("/") {
            match text.split_once(' ') {
                Some((_, r)) => text = r,
                None => continue,
            }
        }

        if text.is_empty() {
            continue;
        }

        let resp = wolf.query(&reqw, text.to_string()).await?;
        tg.send_photo(
            &reqw,
            msg.chat.id,
            msg.message_id,
            resp.image_data,
            resp.content_type,
        )
        .await?;
    }

    tg_update_task.await?
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
