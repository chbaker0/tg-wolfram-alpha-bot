//! Bindings for the Wolfram Alpha API

use crate::http_service;

use bytes::Bytes;
use serde::Serialize;
use thiserror::Error;
use tower::Service;

#[derive(Debug, Error)]
pub enum ApiError<Inner> {
    #[error("wolfram alpha could not interpret this query")]
    InvalidQuery,
    #[error("api response is malformed")]
    MalformedResponse,
    #[error("received an unexpected HTTP status code")]
    BadStatus,
    #[error("error sending api request")]
    ServiceError {
        #[from]
        source: Inner,
    },
}

#[derive(Clone)]
pub struct Api<S> {
    client: S,
    app_id: String,
}

impl<
        'a,
        S: Service<http_service::Request, Response = http_service::Response> + Clone + 'static,
    > Service<String> for &'a Api<S>
{
    type Response = SimpleResponse;
    type Error = ApiError<S::Error>;
    type Future =
        std::pin::Pin<Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>>>>;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: String) -> Self::Future {
        Box::pin(self.query(req))
    }
}

impl<S: Service<http_service::Request, Response = http_service::Response> + Clone + 'static>
    Service<String> for Api<S>
{
    type Response = SimpleResponse;
    type Error = ApiError<S::Error>;
    type Future =
        std::pin::Pin<Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>>>>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        (&mut &*self).poll_ready(cx)
    }

    fn call(&mut self, req: String) -> Self::Future {
        (&mut &*self).call(req)
    }
}

async fn do_query<S: Service<http_service::Request, Response = http_service::Response>>(
    client: S,
    req: http_service::Request,
) -> Result<SimpleResponse, ApiError<S::Error>> {
    use tower::util::ServiceExt;

    let resp = client.oneshot(req).await?;
    match resp.status {
        reqwest::StatusCode::OK => (),
        reqwest::StatusCode::NOT_IMPLEMENTED => {
            return Err(ApiError::InvalidQuery);
        }
        _ => {
            return Err(ApiError::BadStatus);
        }
    }

    Ok(SimpleResponse {
        content_type: resp
            .headers
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|s| s.to_str().ok())
            .ok_or(ApiError::MalformedResponse)?
            .to_string(),
        image_data: resp.bytes,
    })
}

impl<S: Service<http_service::Request, Response = http_service::Response> + Clone> Api<S> {
    pub fn new(client: S, app_id: &str) -> Self {
        Api {
            client,
            app_id: app_id.to_string(),
        }
    }

    pub fn query(
        &self,
        text: String,
    ) -> impl std::future::Future<Output = Result<SimpleResponse, ApiError<S::Error>>> {
        let q = Query {
            app_id: self.app_id.clone(),
            text,
        };

        let mut url = api_url().clone();
        q.serialize(serde_urlencoded::Serializer::new(
            &mut url.query_pairs_mut(),
        ))
        .unwrap();

        let req = http_service::Request {
            method: http_service::Method::GET,
            url,
            body: http_service::Body::Normal(Bytes::new()),
        };

        do_query(self.client.clone(), req)
    }
}

pub struct SimpleResponse {
    pub image_data: Bytes,
    pub content_type: String,
}

#[derive(Serialize)]
struct Query {
    #[serde(rename = "appid")]
    app_id: String,
    #[serde(rename = "i")]
    text: String,
}

fn api_url() -> &'static http_service::Url {
    const STR: &str = "http://api.wolframalpha.com/v1/simple";
    static CELL: once_cell::sync::OnceCell<http_service::Url> = once_cell::sync::OnceCell::new();
    CELL.get_or_init(|| http_service::Url::parse(STR).unwrap())
}
