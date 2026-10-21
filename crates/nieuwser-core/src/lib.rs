//! Core for the Nieuwser FreshRSS client: API, storage, and sync.

pub mod api;
pub mod http_body;
pub mod media;
pub mod models;
pub mod reader;
pub mod store;
pub mod sync;

pub use api::Client;
pub use models::{Item, Stream, Subscription};
pub use store::Store;
pub use sync::{SyncEngine, SyncReport};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("HTTP {status}: {body}")]
    Http {
        status: reqwest::StatusCode,
        body: String,
    },
    #[error(transparent)]
    Reqwest(#[from] reqwest::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Db(#[from] rusqlite::Error),
    #[error(transparent)]
    Url(#[from] url::ParseError),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn is_auth_error(&self) -> bool {
        matches!(
            self,
            Error::Http { status, .. }
                if *status == reqwest::StatusCode::UNAUTHORIZED
                    || *status == reqwest::StatusCode::FORBIDDEN
        )
    }
}
