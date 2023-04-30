//! Wraps the basic HTTP service functionality we need.
//!
//! `reqwest::Client` provides a `tower::Service<reqwest::Request>` impl, but we
//! need multipart forms and there's no way to construct such a request without
//! `Client` itself. So, we must re-wrap what we need.

use bytes::Bytes;
use tower::Service;

pub use http::method::Method;
pub use reqwest::{Error, Response, Result, Url};

#[derive(Debug)]
pub struct Request {
    pub method: Method,
    pub url: Url,
    pub body: Body,
}

#[derive(Debug)]
pub enum Body {
    Normal(Bytes),
    Multipart(Vec<FormPart>),
}

#[derive(Debug)]
pub struct FormPart {
    pub name: String,
    pub file_name: Option<String>,
    pub mime_str: Option<String>,
    pub value: Bytes,
}

#[derive(Clone)]
pub struct Client {
    inner: reqwest::Client,
}

impl Client {
    pub fn new(inner: reqwest::Client) -> Self {
        Self { inner }
    }
}

impl Service<Request> for Client {
    type Response = Response;
    type Error = Error;
    type Future = std::pin::Pin<Box<dyn std::future::Future<Output = Result<Response>> + Send>>;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::result::Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: Request) -> Self::Future {
        let builder = self.inner.request(request.method, request.url);
        let builder = (|| {
            Ok(match request.body {
                Body::Normal(data) => builder.body(data),
                Body::Multipart(parts) => {
                    use reqwest::multipart::{Form, Part};
                    builder.multipart(parts.into_iter().try_fold(Form::new(), |form, p| {
                        let mut part = Part::bytes(p.value.to_vec());
                        if let Some(s) = p.file_name {
                            part = part.file_name(s);
                        }
                        if let Some(s) = p.mime_str {
                            part = part.mime_str(&s)?;
                        }
                        Ok(form.part(p.name, part))
                    })?)
                }
            })
        })();
        let client = self.inner.clone();
        Box::pin(async move {
            let req = builder?.build()?;
            client.execute(req).await
        })
    }
}
