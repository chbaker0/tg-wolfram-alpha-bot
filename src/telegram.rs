//! Bindings for the Telegram bot API

use bytes::Bytes;
use reqwest::multipart::{Form, Part};
use reqwest::Client;
use serde::de::{DeserializeOwned, IgnoredAny};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tower::Service;

pub type Result<T> = std::result::Result<T, ApiError>;

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("rate limited; telegram api asked us to retry after {0:?}")]
    RetryAfter(std::time::Duration),
    #[error("telegram api returned error: \"{0}\"")]
    TelegramError(String),
    #[error("api response is malformed: \"{0}\"")]
    MalformedResponse(String),
    #[error("error sending api request")]
    ServiceError {
        #[from]
        source: reqwest::Error,
    },
}

pub struct Api {
    client: Client,
    url: String,
}

pub trait Query {
    type Body: Serialize;
    type Response: DeserializeOwned;
    const ENDPOINT: &'static str;

    fn construct(self) -> (Self::Body, Option<(String, Part)>);
}

/// Represents an empty (or ignored) response. `()` doesn't work on its own
/// since a JSON dict won't deserialize into that, even if we want to ignore the
/// value.
#[derive(Deserialize)]
pub struct Empty(IgnoredAny);

impl Api {
    pub fn new(client: Client, bot_token: &str) -> Api {
        const API_URL: &str = "https://api.telegram.org/bot";
        Api {
            client,
            url: format!("{API_URL}{bot_token}"),
        }
    }

    pub async fn call<Q: Query>(&self, query: Q) -> Result<Q::Response> {
        let (body, extra_part) = query.construct();
        let mut builder = self.client.post(self.method_url(Q::ENDPOINT));
        builder = if let Some((name, part)) = extra_part {
            let form = Form::new()
                .part(
                    "tg_query",
                    Part::text(serde_urlencoded::to_string(&body).unwrap())
                        .mime_str("application/x-www-form-urlencoded")?,
                )
                .part(name, part);
            builder.multipart(form)
        } else {
            builder.json(&body)
        };

        let resp = builder.send().await?.text().await?;

        let resp: Reply<Q::Response> =
            serde_json::from_str(&resp).map_err(|_| ApiError::MalformedResponse(resp))?;
        resp.into_result()
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

macro_rules! default_query_impl {
    ($t:ty, $resp:ty, $endpoint:literal) => {
        impl Query for $t {
            type Body = Self;
            type Response = $resp;
            const ENDPOINT: &'static str = $endpoint;

            fn construct(self) -> (Self, Option<(String, Part)>) {
                (self, None)
            }
        }
    };
}

#[derive(Debug, Serialize)]
pub struct GetMe;

default_query_impl!(GetMe, User, "getMe");

#[derive(Debug, Serialize)]
pub struct GetUpdates {
    pub offset: Option<i64>,
    pub timeout: u64,
}

default_query_impl!(GetUpdates, Vec<Update>, "getUpdates");

#[derive(Debug, Serialize)]
pub struct SendMessage {
    pub chat_id: i64,
    pub text: String,
    pub reply_to_message_id: i64,
}

default_query_impl!(SendMessage, Empty, "sendMessage");

pub struct SendPhoto {
    body: SendPhotoBody,
    part: Part,
}

impl SendPhoto {
    /// Construct the query. Fails if `content_type` is not a valid mime type.
    pub fn new(
        chat_id: i64,
        reply_to_message_id: i64,
        data: Bytes,
        content_type: String,
    ) -> Option<Self> {
        let body = SendPhotoBody {
            chat_id,
            photo: "attach://photo".to_string(),
            reply_to_message_id,
        };

        let part = Part::bytes(data.to_vec())
            .file_name("photo")
            .mime_str(&content_type)
            .ok()?;
        Some(SendPhoto { body, part })
    }
}

impl Query for SendPhoto {
    type Body = SendPhotoBody;
    type Response = Empty;
    const ENDPOINT: &'static str = "sendPhoto";

    fn construct(self) -> (Self::Body, Option<(String, Part)>) {
        (self.body, Some(("photo".to_string(), self.part)))
    }
}

#[derive(Debug, Serialize)]
pub struct SendPhotoBody {
    chat_id: i64,
    photo: String,
    reply_to_message_id: i64,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Reply<T> {
    Success {
        result: T,
    },
    Fail {
        description: String,
        parameters: Option<ResponseParameters>,
    },
}

#[derive(Debug, Deserialize)]
struct ResponseParameters {
    retry_after: Option<u64>,
}

impl<T> Reply<T> {
    fn into_result(self) -> Result<T> {
        match self {
            Reply::Success { result } => Ok(result),
            Reply::Fail {
                parameters:
                    Some(ResponseParameters {
                        retry_after: Some(seconds),
                    }),
                ..
            } => Err(ApiError::RetryAfter(std::time::Duration::from_secs(
                seconds,
            ))),
            Reply::Fail { description, .. } => Err(ApiError::TelegramError(description)),
        }
    }
}
