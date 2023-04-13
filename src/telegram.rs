//! Bindings for the Telegram bot API

use eyre::{Context, Result};
use reqwest::Client;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

pub struct Api {
    url: String,
}

impl Api {
    pub fn new(bot_token: &str) -> Api {
        const API_URL: &str = "https://api.telegram.org/bot";
        Api {
            url: format!("{API_URL}{bot_token}"),
        }
    }

    pub async fn get_me(&self, client: &Client) -> Result<User> {
        self.call_method::<(), _>(client, "getMe", None).await
    }

    pub async fn get_updates(
        &self,
        client: &Client,
        offset: i64,
        timeout: u64,
    ) -> Result<Vec<Update>> {
        let args = GetUpdatesArgs { offset, timeout };
        self.call_method(client, "getUpdates", Some(args)).await
    }

    async fn call_method<'a, T: Serialize, U: DeserializeOwned>(
        &self,
        client: &Client,
        method_name: &str,
        body: Option<T>,
    ) -> Result<U> {
        let mut builder = client.get(format!("{}/{}", self.url, method_name));
        if let Some(body) = body {
            builder = builder.json(&body);
        }
        let req = builder.build()?;
        let resp: Reply<U> = client.execute(req).await?.json().await?;
        resp.into_result()
            .map_err(eyre::Report::msg)
            .wrap_err("telegram API returned error")
    }
}

#[derive(Debug, Deserialize)]
pub struct Update {
    pub update_id: i64,
    pub message: Option<Message>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Message {
    pub message_id: i64,
    pub text: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct User {
    pub id: i64,
    pub is_bot: bool,
}

#[derive(Debug, Serialize)]
struct GetUpdatesArgs {
    offset: i64,
    timeout: u64,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Reply<T> {
    Success { result: T },
    Fail { description: String },
}

impl<T> Reply<T> {
    fn into_result(self) -> Result<T, String> {
        match self {
            Reply::Success { result } => Ok(result),
            Reply::Fail { description } => Err(description),
        }
    }
}
