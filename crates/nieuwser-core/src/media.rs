use std::sync::LazyLock;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;

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

    if let Some(encoded) = url.strip_prefix("data:") {
        let (meta, payload) = encoded
            .split_once(',')
            .ok_or_else(|| Error::Other("malformed data image".into()))?;
        if !meta.starts_with("image/") || !meta.ends_with(";base64") {
            return Err(Error::Other("unsupported data image".into()));
        }
        let data = BASE64
            .decode(payload)
            .map_err(|error| Error::Other(format!("invalid data image: {error}")))?;
        if data.len() > MAX_IMAGE_BYTES {
            return Err(Error::Other("image is too large".into()));
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn decodes_embedded_data_images() {
        let store = Store::open_in_memory().unwrap();
        let url = "data:image/png;base64,aGVsbG8=";
        assert_eq!(fetch_image(&store, url).await.unwrap(), b"hello");
        assert!(store.cached_media(url).unwrap().is_none());
    }
}
