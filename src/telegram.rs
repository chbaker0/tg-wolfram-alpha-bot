//! Bindings for the Telegram bot API

use bytes::Bytes;
use eyre::{Context, Result};
use reqwest::multipart::{Form, Part};
use reqwest::Client;
use serde::de::{DeserializeOwned, IgnoredAny};
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
        self.call_method::<(), _>(client, "getMe", None, None).await
    }

    pub async fn get_updates(
        &self,
        client: &Client,
        offset: Option<i64>,
        timeout: u64,
    ) -> Result<Vec<Update>> {
        let args = GetUpdatesArgs { offset, timeout };
        self.call_method(client, "getUpdates", Some(args), None)
            .await
    }

    pub async fn send_message(
        &self,
        client: &Client,
        chat_id: i64,
        reply_to_message_id: i64,
        text: String,
    ) -> Result<()> {
        let args = SendMessageArgs {
            chat_id,
            text,
            reply_to_message_id,
        };

        let _: IgnoredAny = self
            .call_method(client, "sendMessage", Some(args), None)
            .await?;
        Ok(())
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

        let _: IgnoredAny = self
            .call_method(
                client,
                "sendPhoto",
                Some(args),
                Some(("photo".to_string(), photo_part)),
            )
            .await?;
        Ok(())
    }

    async fn call_method<'a, T: Serialize, U: DeserializeOwned>(
        &self,
        client: &Client,
        method_name: &str,
        body: Option<T>,
        extra_part: Option<(String, Part)>,
    ) -> Result<U> {
        let mut builder = client.post(self.method_url(method_name));
        builder = match (body, extra_part) {
            (None, None) => builder,
            (Some(body), None) => builder.json(&body),
            (Some(body), Some((name, part))) => {
                let form = Form::new()
                    .part(
                        "tg_query",
                        Part::text(serde_urlencoded::to_string(&body)?)
                            .mime_str("application/x-www-form-urlencoded")?,
                    )
                    .part(name, part);
                builder.multipart(form)
            }
            _ => unimplemented!(
                "api does not accept extra multipart/form-data parts without a query body"
            ),
        };

        let resp: Reply<U> = builder.send().await?.json().await?;
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
struct SendMessageArgs {
    chat_id: i64,
    text: String,
    reply_to_message_id: i64,
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
