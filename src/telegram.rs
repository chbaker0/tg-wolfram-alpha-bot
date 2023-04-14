//! Bindings for the Telegram bot API

use bytes::Bytes;
use eyre::{Context, Result};
use reqwest::multipart::{Form, Part};
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
        offset: Option<i64>,
        timeout: u64,
    ) -> Result<Vec<Update>> {
        let args = GetUpdatesArgs { offset, timeout };
        self.call_method(client, "getUpdates", Some(args)).await
    }

    pub async fn send_photo(
        &self,
        client: &Client,
        chat_id: i64,
        reply_to_message_id: i64,
        data: Bytes,
        content_type: String,
    ) -> Result<()> {
        let args = SendPhotoArgs {
            chat_id,
            photo: "attach://photo".to_string(),
            reply_to_message_id,
        };

        let photo_part = Part::bytes(data.to_vec())
            .file_name("photo")
            .mime_str(&content_type)?;
        let form = Form::new().part("photo", photo_part);

        let req = client
            .post(self.method_url("sendPhoto"))
            .query(&args)
            .multipart(form)
            .build()?;

        println!("{req:?}");

        let body: String = client.execute(req).await?.text().await?;
        let resp: Reply<Message> = serde_json::from_str(&body).wrap_err_with(|| body.clone())?;
        resp.into_result().map_err(eyre::Report::msg)?;
        Ok(())
    }

    async fn call_method<'a, T: Serialize, U: DeserializeOwned>(
        &self,
        client: &Client,
        method_name: &str,
        body: Option<T>,
    ) -> Result<U> {
        let mut builder = client.get(self.method_url(method_name));
        if let Some(body) = body {
            builder = builder.json(&body);
        }
        let req = builder.build()?;
        let resp: Reply<U> = client.execute(req).await?.json().await?;
        resp.into_result()
            .map_err(eyre::Report::msg)
            .wrap_err("telegram API returned error")
    }

    fn method_url(&self, method_name: &str) -> String {
        format!("{}/{}", self.url, method_name)
    }
}

#[derive(Debug, Deserialize)]
pub struct Update {
    pub update_id: i64,
    pub message: Option<Message>,
}

#[derive(Debug, Deserialize)]
pub struct Message {
    pub message_id: i64,
    pub text: Option<String>,
    pub chat: Chat,
}

#[derive(Debug, Deserialize)]
pub struct Chat {
    pub id: i64,
}

#[derive(Debug, Deserialize)]
pub struct User {
    pub id: i64,
    pub is_bot: bool,
}

#[derive(Debug, Serialize)]
struct GetUpdatesArgs {
    offset: Option<i64>,
    timeout: u64,
}

#[derive(Debug, Serialize)]
struct SendPhotoArgs {
    chat_id: i64,
    photo: String,
    reply_to_message_id: i64,
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
