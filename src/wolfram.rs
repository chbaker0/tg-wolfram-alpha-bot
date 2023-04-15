//! Bindings for the Wolfram Alpha API

use bytes::Bytes;
use eyre::{Context, Result};
use reqwest::Client;
use serde::Serialize;

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

        let resp = client
            .get(API_URL)
            .query(&q)
            .send()
            .await
            .wrap_err("while sending Wolfram API query")?
            .error_for_status()
            .wrap_err("wolfram API could not handle query")?;

        Ok(SimpleResponse {
            content_type: resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .unwrap()
                .to_str()?
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
