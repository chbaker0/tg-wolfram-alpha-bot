mod telegram;

#[tokio::main]
async fn main() -> eyre::Result<()> {
    color_eyre::install()?;

    let tg = telegram::Api::new(TELEGRAM_KEY.trim_end());

    let reqw = reqwest::Client::new();

    let me = tg.get_me(&reqw).await?;
    println!("ID: {}", me.id);
    eyre::ensure!(me.is_bot, "we're not a bot?");

    let mut offset = 0;
    loop {
        let updates = tg.get_updates(&reqw, offset, 30).await?;
        for u in updates {
            println!("{u:?}");
            offset = std::cmp::max(offset, u.update_id + 1);
        }
    }

    Ok(())
}

static TELEGRAM_KEY: &str = include_str!("../.keys/telegram");
// static WOLFRAM_KEY: &str = include_str!("../.keys/wolframalpha");
