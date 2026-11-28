use cvbbmso_accoding::AccodingCookies;
use cvbbmso_exam::ExamError;
use cvbbmso_protocol::ExamListResponse;
use reqwest::Certificate;
use thiserror::Error;
use url::{Origin, Url};

// The fixture sets response sizes from the protocol limit independently of the reader.
#[cfg(test)]
pub(crate) const MAX_EXAM_LIST_BYTES: usize = 1024 * 1024;

pub(crate) struct Monitor(cvbbmso_exam::Monitor);

#[derive(Debug, Error)]
pub(crate) enum MonitorError {
    #[error("The monitor HTTPS origin is missing or invalid")]
    Configuration,
    #[error("The monitor HTTP client could not be initialized")]
    Initialization(#[source] ExamError),
    #[error(transparent)]
    Request(#[from] ExamError),
}

impl Monitor {
    pub(crate) fn new(origin: Option<Origin>) -> Result<Self, MonitorError> {
        Self::build(origin, None)
    }

    #[cfg(test)]
    pub(crate) fn with_root_certificate(
        origin: Origin,
        certificate: Certificate,
    ) -> Result<Self, MonitorError> {
        Self::build(Some(origin), Some(certificate))
    }

    fn build(
        origin: Option<Origin>,
        root_certificate: Option<Certificate>,
    ) -> Result<Self, MonitorError> {
        let origin = origin
            .filter(|origin| matches!(origin, Origin::Tuple(scheme, ..) if scheme == "https"))
            .ok_or(MonitorError::Configuration)?;
        let base =
            Url::parse(&origin.ascii_serialization()).map_err(|_| MonitorError::Configuration)?;

        cvbbmso_exam::Monitor::new(base, root_certificate)
            .map(Self)
            .map_err(MonitorError::Initialization)
    }

    pub(crate) fn server_base(&self) -> &Url {
        self.0.server_base()
    }

    pub(crate) async fn list_exams(
        &self,
        cookies: &AccodingCookies,
    ) -> Result<ExamListResponse, MonitorError> {
        self.0.list_exams(cookies).await.map_err(MonitorError::from)
    }
}
