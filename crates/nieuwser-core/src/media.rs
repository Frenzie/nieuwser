use std::sync::LazyLock;
use std::time::Duration;

use crate::store::DEFAULT_CACHE_BYTES;
use crate::{Error, Result, Store};

pub const MAX_IMAGE_BYTES: usize = 15 * 1024 * 1024;

static IMAGE_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent(concat!("Nieuwser/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("image HTTP client")
});

/// Returns an image from the local cache, downloading and caching it if needed.
pub async fn fetch_image(store: &Store, url: &str) -> Result<Vec<u8>> {
    if let Some(data) = store.cached_media(url)? {
        return Ok(data);
    }

    let parsed = url::Url::parse(url)
        .map_err(|error| Error::Other(format!("invalid image URL {url}: {error}")))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(Error::Other(format!("unsupported image URL: {url}")));
    }

    let response = IMAGE_CLIENT.get(parsed).send().await?;
    let status = response.status();
    if !status.is_success() {
        return Err(Error::Http {
            status,
            body: String::new(),
        });
    }
    if response.content_length().unwrap_or(0) as usize > MAX_IMAGE_BYTES {
        return Err(Error::Other("image is too large".into()));
    }
    let data = response.bytes().await?;
    if data.len() > MAX_IMAGE_BYTES {
        return Err(Error::Other("image is too large".into()));
    }

    store.put_media(url, &data)?;
    let _ = store.enforce_cache_budget(DEFAULT_CACHE_BYTES);
    Ok(data.to_vec())
}
