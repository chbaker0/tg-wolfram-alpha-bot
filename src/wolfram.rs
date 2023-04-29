//! Bindings for the Wolfram Alpha API

use bytes::Bytes;
use reqwest::Client;
use serde::Serialize;
use thiserror::Error;

pub type Result<T> = std::result::Result<T, ApiError>;

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("wolfram alpha could not interpret this query")]
    InvalidQuery,
    #[error("api response is malformed")]
    MalformedResponse,
    #[error("error sending api request")]
    ServiceError {
        #[from]
        source: reqwest::Error,
    },
}

pub struct Api {
    app_id: String,
}

impl Api {
    pub fn new(app_id: &str) -> Api {
        Api {
            app_id: app_id.to_string(),
        }
    }

    pub async fn query(&self, client: &Client, text: String) -> Result<SimpleResponse> {
        let q = Query {
            app_id: self.app_id.clone(),
            text,
        };

        let resp = client.get(API_URL).query(&q).send().await?;
        match resp.status() {
            reqwest::StatusCode::OK => (),
            reqwest::StatusCode::NOT_IMPLEMENTED => {
                return Err(ApiError::InvalidQuery);
            }
            _ => {
                resp.error_for_status()?;
                unreachable!("received non-OK status code, but no error?")
            }
        }

        Ok(SimpleResponse {
            content_type: resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|s| s.to_str().ok())
                .ok_or(ApiError::MalformedResponse)?
                .to_string(),
            image_data: resp.bytes().await?,
        })
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

const API_URL: &str = "http://api.wolframalpha.com/v1/simple";
