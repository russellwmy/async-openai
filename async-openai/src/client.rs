use std::pin::Pin;

use futures::{stream::StreamExt, Stream};
use reqwest::header::HeaderMap;
use reqwest_eventsource::{Event, EventSource, RequestBuilderExt};
use serde::{de::DeserializeOwned, Serialize};

use crate::{
    edit::Edits,
    error::{OpenAIError, WrappedError},
    file::Files,
    image::Images,
    moderation::Moderations,
    Completions, Embeddings, FineTunes, Models,
};

#[derive(Debug, Clone)]
/// Client is a container for api key, base url, organization id, and backoff
/// configuration used to make API calls.
pub struct Client {
    api_key: String,
    api_base: String,
    org_id: String,
    backoff: backoff::ExponentialBackoff,
}

/// Default v1 API base url
pub const API_BASE: &str = "https://api.openai.com/v1";
/// Name for organization header
pub const ORGANIZATION_HEADER: &str = "OpenAI-Organization";

impl Default for Client {
    /// Create client with default [API_BASE] url and default API key from OPENAI_API_KEY env var
    fn default() -> Self {
        Self {
            api_base: API_BASE.to_string(),
            api_key: std::env::var("OPENAI_API_KEY").unwrap_or_else(|_| "".to_string()),
            org_id: Default::default(),
            backoff: Default::default(),
        }
    }
}

impl Client {
    /// Create client with default [API_BASE] url and default API key from OPENAI_API_KEY env var
    pub fn new() -> Self {
        Default::default()
    }

    /// To use a different API key different from default OPENAI_API_KEY env var
    pub fn with_api_key<S: Into<String>>(mut self, api_key: S) -> Self {
        self.api_key = api_key.into();
        self
    }

    /// To use a different organization id other than default
    pub fn with_org_id<S: Into<String>>(mut self, org_id: S) -> Self {
        self.org_id = org_id.into();
        self
    }

    /// To use a API base url different from default [API_BASE]
    pub fn with_api_base<S: Into<String>>(mut self, api_base: S) -> Self {
        self.api_base = api_base.into();
        self
    }

    /// Exponential backoff for retrying [rate limited](https://help.openai.com/en/articles/5955598-is-api-usage-subject-to-any-rate-limits) requests. Form submissions are not retried.
    pub fn with_backoff(mut self, backoff: backoff::ExponentialBackoff) -> Self {
        self.backoff = backoff;
        self
    }

    pub fn api_base(&self) -> &str {
        &self.api_base
    }

    pub fn api_key(&self) -> &str {
        &self.api_key
    }

    // API groups

    /// To call [Models] group related APIs using this client.
    pub fn models(&self) -> Models {
        Models::new(self)
    }

    /// To call [Completions] group related APIs using this client.
    pub fn completions(&self) -> Completions {
        Completions::new(self)
    }

    /// To call [Edits] group related APIs using this client.
    pub fn edits(&self) -> Edits {
        Edits::new(self)
    }

    /// To call [Images] group related APIs using this client.
    pub fn images(&self) -> Images {
        Images::new(self)
    }

    /// To call [Moderations] group related APIs using this client.
    pub fn moderations(&self) -> Moderations {
        Moderations::new(self)
    }

    /// To call [Files] group related APIs using this client.
    pub fn files(&self) -> Files {
        Files::new(self)
    }

    /// To call [FineTunes] group related APIs using this client.
    pub fn fine_tunes(&self) -> FineTunes {
        FineTunes::new(self)
    }

    /// To call [Embeddings] group related APIs using this client.
    pub fn embeddings(&self) -> Embeddings {
        Embeddings::new(self)
    }

    fn headers(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if !self.org_id.is_empty() {
            headers.insert(ORGANIZATION_HEADER, self.org_id.as_str().parse().unwrap());
        }
        headers
    }

    /// Make a GET request to {path} and deserialize the response body
    pub(crate) async fn get<O>(&self, path: &str) -> Result<O, OpenAIError>
    where
        O: DeserializeOwned,
    {
        let request = reqwest::Client::new()
            .get(format!("{}{path}", self.api_base()))
            .bearer_auth(self.api_key())
            .headers(self.headers())
            .build()?;

        self.execute(request).await
    }

    /// Make a DELETE request to {path} and deserialize the response body
    pub(crate) async fn delete<O>(&self, path: &str) -> Result<O, OpenAIError>
    where
        O: DeserializeOwned,
    {
        let request = reqwest::Client::new()
            .delete(format!("{}{path}", self.api_base()))
            .bearer_auth(self.api_key())
            .headers(self.headers())
            .build()?;

        self.execute(request).await
    }

    /// Make a POST request to {path} and deserialize the response body
    pub(crate) async fn post<I, O>(&self, path: &str, request: I) -> Result<O, OpenAIError>
    where
        I: Serialize,
        O: DeserializeOwned,
    {
        let request = reqwest::Client::new()
            .post(format!("{}{path}", self.api_base()))
            .bearer_auth(self.api_key())
            .headers(self.headers())
            .json(&request)
            .build()?;

        self.execute(request).await
    }

    /// POST a form at {path} and deserialize the response body
    pub(crate) async fn post_form<O>(
        &self,
        path: &str,
        form: reqwest::multipart::Form,
    ) -> Result<O, OpenAIError>
    where
        O: DeserializeOwned,
    {
        let request = reqwest::Client::new()
            .post(format!("{}{path}", self.api_base()))
            .bearer_auth(self.api_key())
            .headers(self.headers())
            .multipart(form)
            .build()?;

        self.execute(request).await
    }

    /// Deserialize response body from either error object or actual response object
    async fn process_response<O>(&self, response: reqwest::Response) -> Result<O, OpenAIError>
    where
        O: DeserializeOwned,
    {
        let status = response.status();
        let bytes = response.bytes().await?;

        if !status.is_success() {
            let wrapped_error: WrappedError =
                serde_json::from_slice(bytes.as_ref()).map_err(OpenAIError::JSONDeserialize)?;

            return Err(OpenAIError::ApiError(wrapped_error.error));
        }

        let response: O =
            serde_json::from_slice(bytes.as_ref()).map_err(OpenAIError::JSONDeserialize)?;
        Ok(response)
    }

    /// Execute any HTTP requests and retry on rate limit, except streaming ones as they cannot be cloned for retrying.
    async fn execute<O>(&self, request: reqwest::Request) -> Result<O, OpenAIError>
    where
        O: DeserializeOwned,
    {
        let client = reqwest::Client::new();

        match request.try_clone() {
            // Only clone-able requests can be retried
            Some(request) => {
                backoff::future::retry(self.backoff.clone(), || async {
                    let response = client
                        .execute(request.try_clone().unwrap())
                        .await
                        .map_err(OpenAIError::Reqwest)
                        .map_err(backoff::Error::Permanent)?;

                    let status = response.status();
                    let bytes = response
                        .bytes()
                        .await
                        .map_err(OpenAIError::Reqwest)
                        .map_err(backoff::Error::Permanent)?;

                    // Deserialize response body from either error object or actual response object
                    if !status.is_success() {
                        let wrapped_error: WrappedError = serde_json::from_slice(bytes.as_ref())
                            .map_err(OpenAIError::JSONDeserialize)
                            .map_err(backoff::Error::Permanent)?;

                        if status.as_u16() == 429
                            // API returns 429 also when:
                            // "You exceeded your current quota, please check your plan and billing details."
                            && wrapped_error.error.r#type != "insufficient_quota"
                        {
                            // Rate limited retry...
                            tracing::warn!("Rate limited: {}", wrapped_error.error.message);
                            return Err(backoff::Error::Transient {
                                err: OpenAIError::ApiError(wrapped_error.error),
                                retry_after: None,
                            });
                        } else {
                            return Err(backoff::Error::Permanent(OpenAIError::ApiError(
                                wrapped_error.error,
                            )));
                        }
                    }

                    let response: O = serde_json::from_slice(bytes.as_ref())
                        .map_err(OpenAIError::JSONDeserialize)
                        .map_err(backoff::Error::Permanent)?;
                    Ok(response)
                })
                .await
            }
            None => {
                let response = client.execute(request).await?;
                self.process_response(response).await
            }
        }
    }

    /// Make HTTP POST request to receive SSE
    pub(crate) async fn post_stream<I, O>(
        &self,
        path: &str,
        request: I,
    ) -> Pin<Box<dyn Stream<Item = Result<O, OpenAIError>> + Send>>
    where
        I: Serialize,
        O: DeserializeOwned + std::marker::Send + 'static,
    {
        let event_source = reqwest::Client::new()
            .post(format!("{}{path}", self.api_base()))
            .headers(self.headers())
            .bearer_auth(self.api_key())
            .json(&request)
            .eventsource()
            .unwrap();

        Client::stream(event_source).await
    }

    /// Make HTTP GET request to receive SSE
    pub(crate) async fn get_stream<Q, O>(
        &self,
        path: &str,
        query: &Q,
    ) -> Pin<Box<dyn Stream<Item = Result<O, OpenAIError>> + Send>>
    where
        Q: Serialize + ?Sized,
        O: DeserializeOwned + std::marker::Send + 'static,
    {
        let event_source = reqwest::Client::new()
            .get(format!("{}{path}", self.api_base()))
            .query(query)
            .headers(self.headers())
            .bearer_auth(self.api_key())
            .eventsource()
            .unwrap();

        Client::stream(event_source).await
    }

    /// Request which responds with SSE.
    /// [server-sent events](https://developer.mozilla.org/en-US/docs/Web/API/Server-sent_events/Using_server-sent_events#event_stream_format)
    pub(crate) async fn stream<O>(
        mut event_source: EventSource,
    ) -> Pin<Box<dyn Stream<Item = Result<O, OpenAIError>> + Send>>
    where
        O: DeserializeOwned + std::marker::Send + 'static,
    {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

        tokio::spawn(async move {
            while let Some(ev) = event_source.next().await {
                match ev {
                    Err(e) => {
                        if let Err(_e) = tx.send(Err(OpenAIError::StreamError(e.to_string()))) {
                            // rx dropped
                            break;
                        }
                    }
                    Ok(event) => match event {
                        Event::Message(message) => {
                            if message.data == "[DONE]" {
                                break;
                            }

                            let response = match serde_json::from_str::<O>(&message.data) {
                                Err(e) => Err(OpenAIError::JSONDeserialize(e)),
                                Ok(output) => Ok(output),
                            };

                            if let Err(_e) = tx.send(response) {
                                // rx dropped
                                break;
                            }
                        }
                        Event::Open => continue,
                    },
                }
            }

            event_source.close();
        });

        Box::pin(tokio_stream::wrappers::UnboundedReceiverStream::new(rx))
    }
}
