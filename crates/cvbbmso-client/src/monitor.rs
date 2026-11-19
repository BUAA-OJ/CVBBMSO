use std::time::Duration;

use cvbbmso_accoding::AccodingCookies;
use cvbbmso_protocol::{ExamListResponse, ExamSummary};
use reqwest::{Client, ClientBuilder, StatusCode, header, redirect};
use thiserror::Error;
use url::{Origin, Url};

pub(crate) struct Monitor {
    endpoint: Url,
    client: Client,
}

#[derive(Debug, Error)]
pub(crate) enum MonitorError {
    #[error("The monitor HTTPS origin is missing or invalid")]
    Configuration,
    #[error("The monitor HTTP client could not be initialized")]
    Initialization(#[source] reqwest::Error),
    #[error("The Accoding session is missing or expired")]
    SignInRequired,
    #[error("The identity service is temporarily unavailable")]
    Unavailable,
    #[error("The monitor request could not complete")]
    Connection(#[source] reqwest::Error),
    #[error("The monitor returned HTTP {0}")]
    UnexpectedStatus(StatusCode),
    #[error("The monitor returned an invalid exam list")]
    InvalidResponse(#[source] reqwest::Error),
}

impl Monitor {
    pub(crate) fn new(origin: Option<Origin>) -> Result<Self, MonitorError> {
        Self::build(origin, Client::builder())
    }

    #[cfg(test)]
    pub(crate) fn with_root_certificate(
        origin: Origin,
        certificate: reqwest::Certificate,
    ) -> Result<Self, MonitorError> {
        Self::build(
            Some(origin),
            Client::builder().add_root_certificate(certificate),
        )
    }

    fn build(origin: Option<Origin>, builder: ClientBuilder) -> Result<Self, MonitorError> {
        let origin = origin
            .filter(|origin| matches!(origin, Origin::Tuple(scheme, ..) if scheme == "https"))
            .ok_or(MonitorError::Configuration)?;
        let mut endpoint =
            Url::parse(&origin.ascii_serialization()).map_err(|_| MonitorError::Configuration)?;
        endpoint.set_path("/api/exams");

        let client = builder
            .https_only(true)
            .no_proxy()
            .redirect(redirect::Policy::none())
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(MonitorError::Initialization)?;
        Ok(Self { endpoint, client })
    }

    pub(crate) async fn list_exams(
        &self,
        cookies: &AccodingCookies,
    ) -> Result<Vec<ExamSummary>, MonitorError> {
        let response = self
            .client
            .get(self.endpoint.clone())
            .header(header::ACCEPT, "application/json")
            .header(header::COOKIE, cookies.header_value())
            .send()
            .await
            .map_err(MonitorError::Connection)?;

        match response.status() {
            StatusCode::OK => response
                .json::<ExamListResponse>()
                .await
                .map(|list| list.items)
                .map_err(|error| {
                    if error.is_decode() {
                        MonitorError::InvalidResponse(error)
                    } else {
                        MonitorError::Connection(error)
                    }
                }),
            StatusCode::UNAUTHORIZED => Err(MonitorError::SignInRequired),
            StatusCode::SERVICE_UNAVAILABLE => Err(MonitorError::Unavailable),
            status => Err(MonitorError::UnexpectedStatus(status)),
        }
    }
}
