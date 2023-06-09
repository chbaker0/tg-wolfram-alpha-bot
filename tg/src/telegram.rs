//! Bindings for the Telegram bot API

use crate::http_service::{Body, FormPart, Method, Request, Url};

use std::sync::Arc;

use bytes::Bytes;
use futures::ready;
use pin_project::pin_project;
use serde::de::{DeserializeOwned, IgnoredAny};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tower::{Service, ServiceExt};
use tracing::Instrument;

#[derive(Debug, Error)]
pub enum ApiError<Inner> {
    /// Retry the request after the specified time.
    #[error("rate limited; telegram api asked us to retry after {0:?}")]
    RetryAfter(std::time::Duration),
    /// Telegram returned an error for the request.
    #[error("telegram api returned error: \"{0}\"")]
    TelegramError(String),
    /// Successfully received the response, but it wasn't well-formed.
    #[error("api response is malformed: \"{0}\"")]
    MalformedResponse(String),
    /// The underlying transport (e.g. network or HTTP processing) failed.
    #[error("error sending api request")]
    ServiceError {
        #[from]
        source: Inner,
    },
}

/// A valid Telegram Bot API query. Consumed on use.
pub trait Query: Clone + std::fmt::Debug {
    /// The response type.
    type Response: DeserializeOwned;

    /// For internal use. The internal body that will be serialized to send.
    type Body: Serialize;

    /// For internal use. The endpoint URL for this query.
    const ENDPOINT: &'static str;

    /// For internal use. Construct the actual message to be sent.
    fn construct(self) -> (Self::Body, Option<FormPart>);
}

/// Telegram API client for a particular bot. Runs queries and returns the
/// result.
///
/// This is generic so code using the Telegram API can be tested easier. In
/// practice, this will be constructed from `Bot::on` with a real HTTP service.
pub trait Client {
    /// The underlying service's error type.
    type Error;
    type Future<Q: Query>: std::future::Future<Output = Result<Q::Response, ApiError<Self::Error>>>;

    /// Call `query` and get the result.
    fn call<Q: Query>(&self, query: Q) -> Self::Future<Q>;
}

/// A Telegram bot. Constructs a `Client` instance for calling API methods.
#[derive(Clone)]
pub struct Bot {
    url: Arc<Url>,
}

impl Bot {
    pub fn new(bot_token: &str) -> Self {
        const API_URL: &str = "https://api.telegram.org/bot";
        Bot {
            url: Arc::new(Url::parse(&format!("{API_URL}{bot_token}/")).unwrap()),
        }
    }

    /// Given a service we can send HTTP requests on, construct the service to
    /// send Telegram queries. `client` should be cheaply cloneable (e.g.
    /// reqwest::Client, which is a simple `Arc<_>` internally).
    ///
    /// The returned value implements both `Client`, to send any Telegram query,
    /// and `tower::Service<Q>` for all query types `Q`, for interop with tower.
    pub fn on<S: Service<Request, Response = Bytes> + Clone>(&self, client: S) -> Instance<S> {
        Instance {
            url: self.url.clone(),
            client,
        }
    }
}

/// Concrete API session on a particular HTTP service. Meant to be transient,
/// since all fields are cheaply constructed and cloned.
#[derive(Clone)]
pub struct Instance<S> {
    url: Arc<Url>,
    client: S,
}

impl<S, E> Client for Instance<S>
where
    S: Service<Request, Response = Bytes, Error = E> + Clone,
{
    type Error = S::Error;
    type Future<Q: Query> = DoCall<S, Q::Response>;

    fn call<Q: Query>(&self, query: Q) -> Self::Future<Q> {
        let url = method_url(&self.url, Q::ENDPOINT);
        build_call(self.client.clone(), query, url)
    }
}

impl<Q: Query, S> Service<Q> for Instance<S>
where
    S: Service<Request, Response = Bytes> + Clone,
{
    type Response = Q::Response;
    type Error = ApiError<S::Error>;
    type Future = DoCall<S, Q::Response>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.client.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, query: Q) -> Self::Future {
        <Self as Client>::call(self, query)
    }
}

fn method_url(base: &Url, method: &str) -> Url {
    base.join(method).unwrap()
}

#[tracing::instrument(skip(client))]
fn build_call<Q: Query, S: Service<Request, Response = Bytes>>(
    client: S,
    query: Q,
    mut url: Url,
) -> DoCall<S, Q::Response> {
    let (body, extra_part) = query.construct();
    body.serialize(serde_urlencoded::Serializer::new(
        &mut url.query_pairs_mut(),
    ))
    .unwrap();

    let body = if let Some(part) = extra_part {
        Body::Multipart(vec![part])
    } else {
        Body::Normal(Bytes::new())
    };

    let req = Request {
        method: Method::POST,
        url,
        body,
    };

    DoCall {
        fut: client
            .oneshot(req)
            .instrument(tracing::trace_span!("calling method")),
        _phantom: std::marker::PhantomData,
    }
}

#[pin_project]
pub struct DoCall<S: Service<Request>, Resp> {
    #[pin]
    fut: tracing::instrument::Instrumented<tower::util::Oneshot<S, Request>>,
    _phantom: std::marker::PhantomData<Resp>,
}

impl<S, Resp> std::future::Future for DoCall<S, Resp>
where
    S: Service<Request, Response = Bytes>,
    Resp: DeserializeOwned,
{
    type Output = Result<Resp, ApiError<S::Error>>;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        use std::task::Poll::*;

        let this = self.project();
        let bytes: Bytes = match ready!(this.fut.poll(cx)) {
            Ok(b) => b,
            Err(e) => return Ready(Err(e.into())),
        };

        let resp: Result<Resp, _> = serde_json::from_slice(&bytes)
            .map_err(|_| ApiError::MalformedResponse(String::from_utf8_lossy(&bytes).into_owned()))
            .and_then(|r: Reply<Resp>| r.into_result());
        Ready(resp)
    }
}

/// Represents an empty (or ignored) response. `()` doesn't work on its own
/// since a JSON dict won't deserialize into that, even if we want to ignore the
/// value.
#[derive(Deserialize)]
pub struct Empty(IgnoredAny);

/// <https://core.telegram.org/bots/api#update>
///
/// The API docs specify each update has at most one update variant, so the
/// inner update is an `enum`.
///
/// `Update` is parametrized on `Detail` so tests don't have to worry about the
/// update contents.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Update<Detail = UpdateDetail> {
    pub update_id: i64,
    #[serde(flatten)]
    pub detail: Option<Detail>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateDetail {
    Message(Message),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Message {
    pub message_id: i64,
    pub text: Option<String>,
    pub chat: Chat,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Chat {
    pub id: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
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

#[derive(Clone, Debug, Serialize)]
pub struct GetMe;

default_query_impl!(GetMe, User, "getMe");

#[derive(Clone, Debug, Serialize)]
pub struct GetUpdates {
    pub offset: Option<i64>,
    pub timeout: u64,
}

default_query_impl!(GetUpdates, Vec<Update>, "getUpdates");

#[derive(Clone, Debug, Serialize)]
pub struct SendMessage {
    pub chat_id: i64,
    pub text: String,
    pub reply_to_message_id: i64,
}

default_query_impl!(SendMessage, Empty, "sendMessage");

#[derive(Clone, Debug)]
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

#[derive(Clone, Debug, Serialize)]
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

    use test_util::*;

    use eyre::*;
    use serde_json::{json, Value};
    use tower::ServiceExt;
    use tower_test::*;

    use crate::http_service as hs;

    #[tokio::test]
    async fn test_api() -> eyre::Result<()> {
        setup_tracing_for_test();

        let api = Bot::new("fake");
        let (m, mut handle) = mock::pair::<hs::Request, Bytes>();
        let client = api.on(m.map_err(|e| EyreWrapper::from(eyre!(e))));
        let task = tokio::spawn(client.call(GetMe));

        let (req, h) = handle.next_request().await.unwrap();
        ensure!(req.method == Method::POST, "incorrect method");
        ensure!(
            req.url.path() == "/botfake/getMe",
            "incorrect url {}",
            req.url.path()
        );
        let Body::Normal(bytes) = req.body else { bail!("incorrect body type: {:?}", req.body)};
        ensure!(bytes.is_empty(), "{bytes:?}");

        let value = Value::deserialize(serde_urlencoded::Deserializer::new(req.url.query_pairs()))?;
        ensure!(
            value == json!({}),
            "incorrect query: {value:?} from {}",
            req.url
        );
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

    #[test]
    fn deserialize_update() -> eyre::Result<()> {
        setup_tracing_for_test();

        let update: Update = serde_json::from_value(json!({
            "update_id": 123,
        }))?;

        ensure!(
            matches!(
                update,
                Update {
                    update_id: 123,
                    detail: None,
                }
            ),
            "{update:?}"
        );

        let update: Update = serde_json::from_value(json!({
            "update_id": 124,
            "unrecognized_variant": {"foo": 42},
        }))?;

        ensure!(
            matches!(
                update,
                Update {
                    update_id: 124,
                    detail: None,
                }
            ),
            "{update:?}"
        );

        let update: Update = serde_json::from_value(json!({
            "update_id": 124,
            "message": {"message_id": 42, "text": "hello world", "chat": { "id": 1}},
        }))?;

        ensure!(
            matches!(
                update,
                Update {
                    update_id: 124,
                    detail: Some(UpdateDetail::Message(Message {
                        message_id: 42,
                        text: Some(_),
                        chat: Chat { id: 1 },
                    })),
                }
            ),
            "{update:?}"
        );

        Ok(())
    }
}
