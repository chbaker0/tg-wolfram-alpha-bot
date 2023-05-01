//! Bindings for the Telegram bot API

use crate::http_service::{Body, FormPart, Method, Request, Url};

use std::sync::Arc;

use bytes::Bytes;
use serde::de::{DeserializeOwned, IgnoredAny};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tower::Service;
use tracing::Instrument;

#[derive(Debug, Error)]
pub enum ApiError<Inner> {
    #[error("rate limited; telegram api asked us to retry after {0:?}")]
    RetryAfter(std::time::Duration),
    #[error("telegram api returned error: \"{0}\"")]
    TelegramError(String),
    #[error("api response is malformed: \"{0}\"")]
    MalformedResponse(String),
    #[error("error sending api request")]
    ServiceError {
        #[from]
        source: Inner,
    },
}

#[derive(Clone)]
pub struct Api {
    url: Arc<Url>,
}

impl Api {
    pub fn new(bot_token: &str) -> Self {
        const API_URL: &str = "https://api.telegram.org/bot";
        Api {
            url: Arc::new(Url::parse(&format!("{API_URL}{bot_token}/")).unwrap()),
        }
    }

    pub fn on<S: Service<Request, Response = Bytes> + Send + Clone>(&self, client: &S) -> ApiOn<S> {
        ApiOn {
            url: self.url.clone(),
            client: client.clone(),
            // client: Some(client.clone()),
        }
    }
}

pub trait GenericApi<S, E> {
    type Handler<Q: Query + 'static>: Service<Q>;
    type Future<Q: Query + 'static>: std::future::Future<Output = Result<Q::Response, ApiError<E>>>;
    fn call<Q: Query + 'static>(&self, client: &S, query: Q) -> Self::Future<Q>;
}

impl<S> GenericApi<S, S::Error> for Api
where
    S: Service<Request, Response = Bytes> + Send + Clone + 'static,
    <S as Service<Request>>::Future: Send,
    <S as Service<Request>>::Error: Send,
{
    type Handler<Q: Query + 'static> = ApiOn<S>;
    type Future<Q: Query + 'static> = <ApiOn<S> as Service<Q>>::Future;
    fn call<Q: Query + 'static>(&self, client: &S, query: Q) -> Self::Future<Q> {
        self.on(client).call(query)
    }
}

fn method_url(base: &Url, method: &str) -> Url {
    base.join(method).unwrap()
}

pub trait GenericApiOn {
    type Error: std::error::Error + Send + Sync + 'static;
    type Future<Q: Query + 'static>: std::future::Future<
        Output = Result<Q::Response, ApiError<Self::Error>>,
    >;
    fn do_call<Q: Query + 'static>(&self, query: Q) -> Self::Future<Q>;
}

impl<S, E> GenericApiOn for ApiOn<S>
where
    S: Service<Request, Response = Bytes, Error = E> + Send + Clone + 'static,
    <S as Service<Request>>::Future: Send,
    E: std::error::Error + Send + Sync + 'static,
{
    type Error = S::Error;
    type Future<Q: Query + 'static> = <ApiOn<S> as Service<Q>>::Future;

    fn do_call<Q: Query + 'static>(&self, query: Q) -> Self::Future<Q> {
        self.clone().call(query)
    }
}

#[derive(Clone)]
pub struct ApiOn<S> {
    url: Arc<Url>,
    client: S,
}

impl<Q: Query, S> Service<Q> for ApiOn<S>
where
    S: Service<Request, Response = Bytes> + Send + Clone + 'static,
    S::Future: Send,
    S::Error: Send,
    Q: 'static,
{
    type Response = Q::Response;
    type Error = ApiError<S::Error>;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
    >;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        // assert!(self.client);
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, query: Q) -> Self::Future {
        let url = method_url(&self.url, Q::ENDPOINT);
        Box::pin(do_call(self.client.clone(), query, url))
    }
}

#[tracing::instrument(skip(client))]
async fn do_call<Q: Query, S: Service<Request, Response = Bytes>>(
    mut client: S,
    query: Q,
    url: Url,
) -> Result<Q::Response, ApiError<S::Error>> {
    let (body, extra_part) = query.construct();
    let body = if let Some(part) = extra_part {
        Body::Multipart(vec![
            FormPart {
                name: "tg_query".to_string(),
                file_name: None,
                mime_str: Some("application/x-www-form-urlencoded".to_string()),
                value: serde_urlencoded::to_string(&body).unwrap().into(),
            },
            part,
        ])
    } else {
        Body::Normal(serde_json::to_string(&body).unwrap().into())
    };

    use tower::ServiceExt;

    let client = client
        .ready()
        .instrument(tracing::info_span!("awaiting client"))
        .await?;

    let resp = client
        .call(Request {
            method: Method::POST,
            url,
            body,
        })
        .instrument(tracing::info_span!("calling method"))
        .await?;

    let resp: Reply<Q::Response> = serde_json::from_slice(&resp)
        .map_err(|_| ApiError::MalformedResponse(String::from_utf8_lossy(&resp).into_owned()))?;
    resp.into_result()
}

pub trait Query: std::fmt::Debug + Send {
    type Body: Serialize + Send;
    type Response: DeserializeOwned;
    const ENDPOINT: &'static str;

    fn construct(self) -> (Self::Body, Option<FormPart>);
}

/// Represents an empty (or ignored) response. `()` doesn't work on its own
/// since a JSON dict won't deserialize into that, even if we want to ignore the
/// value.
#[derive(Deserialize)]
pub struct Empty(IgnoredAny);

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

#[derive(Debug, Deserialize, Serialize)]
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

            fn construct(self) -> (Self, Option<FormPart>) {
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

#[derive(Debug)]
pub struct SendPhoto {
    body: SendPhotoBody,
    part: FormPart,
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

        let part = FormPart {
            name: "photo".to_string(),
            file_name: Some("photo".to_string()),
            mime_str: Some(content_type),
            value: data,
        };
        Some(SendPhoto { body, part })
    }
}

impl Query for SendPhoto {
    type Body = SendPhotoBody;
    type Response = Empty;
    const ENDPOINT: &'static str = "sendPhoto";

    fn construct(self) -> (Self::Body, Option<FormPart>) {
        (self.body, Some(self.part))
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
    fn into_result<E>(self) -> Result<T, ApiError<E>> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use eyre::*;
    use tower::ServiceExt;
    use tower_test::*;

    use crate::http_service as hs;
    use crate::test_util::*;

    #[tokio::test]
    async fn test() -> eyre::Result<()> {
        crate::setup_tracing().unwrap();

        let api = Api::new("fake");
        let (m, mut handle) = mock::pair::<hs::Request, Bytes>();
        let task = tokio::spawn(async move {
            api.on(&m.map_err(|e| EyreWrapper::from(eyre!(e))))
                .call(GetMe)
                .await
        });

        let (req, h) = handle.next_request().await.unwrap();
        ensure!(req.method == Method::POST, "incorrect method");
        ensure!(
            req.url == Url::parse("https://api.telegram.org/botfake/getMe").unwrap(),
            "incorrect url"
        );
        let Body::Normal(bytes) = req.body else { bail!("incorrect body type: {:?}", req.body)};

        use serde_json::{json, Value};
        let value: Value = serde_json::from_slice(&bytes)?;
        ensure!(value == json!(null), "incorrect body: {value:?}");
        h.send_response(
            json!({
                "ok": true,
                "result": {
                    "id": 42,
                    "is_bot": true
                }
            })
            .to_string()
            .into(),
        );

        let resp = task.await??;
        ensure!(
            matches!(
                resp,
                User {
                    id: 42,
                    is_bot: true
                }
            ),
            "service did not reply correctly"
        );
        Ok(())
    }
}
