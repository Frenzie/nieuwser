use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use reqwest::redirect::Policy;

use crate::store::DEFAULT_CACHE_BYTES;
use crate::{Error, Result, Store};

pub const MAX_IMAGE_BYTES: usize = 15 * 1024 * 1024;
pub const MAX_IMAGE_DIMENSION: u32 = 4096;
pub const MAX_IMAGE_PIXELS: u64 = 8 * 1024 * 1024;

static REMOTE_CLIENTS: LazyLock<Mutex<HashMap<String, reqwest::Client>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

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
        if payload.len() > MAX_IMAGE_BYTES.saturating_mul(4) / 3 {
            return Err(Error::Other("image is too large".into()));
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
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(Error::Other("image URL contains credentials".into()));
    }

    let client = remote_client(&parsed).await?;
    let mut response = client.get(parsed).send().await?;
    let status = response.status();
    if !status.is_success() {
        return Err(Error::Http {
            status,
            body: String::new(),
        });
    }
    let data = crate::http_body::bytes(&mut response, MAX_IMAGE_BYTES, "image response").await?;

    store.put_media(url, &data)?;
    let _ = store.enforce_cache_budget(DEFAULT_CACHE_BYTES);
    Ok(data)
}

async fn remote_client(url: &url::Url) -> Result<reqwest::Client> {
    let host = url
        .host_str()
        .ok_or_else(|| Error::Other("image URL has no host".into()))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| Error::Other("image URL has no port".into()))?;
    let (mut addresses, resolve_host) = match url.host() {
        Some(url::Host::Domain(host)) => {
            let addresses: Vec<_> = tokio::time::timeout(
                Duration::from_secs(4),
                tokio::net::lookup_host((host, port)),
            )
            .await
            .map_err(|_| Error::Other("timed out resolving image host".into()))?
            .map_err(|error| Error::Other(format!("could not resolve image host: {error}")))?
            .collect();
            (addresses, Some(host.to_string()))
        }
        Some(url::Host::Ipv4(ip)) => (vec![SocketAddr::new(ip.into(), port)], None),
        Some(url::Host::Ipv6(ip)) => (vec![SocketAddr::new(ip.into(), port)], None),
        None => return Err(Error::Other("image URL has no host".into())),
    };
    if addresses.is_empty()
        || addresses
            .iter()
            .any(|address| !safe_remote_ip(address.ip()))
    {
        return Err(Error::Other(
            "image URL resolves to a private address".into(),
        ));
    }
    addresses.sort_unstable();
    addresses.dedup();
    addresses.truncate(32);

    let key = format!("{host}|{port}|{addresses:?}");
    if let Some(client) = REMOTE_CLIENTS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(&key)
        .cloned()
    {
        return Ok(client);
    }

    let mut builder = reqwest::Client::builder()
        .redirect(Policy::none())
        .no_proxy()
        .connect_timeout(Duration::from_secs(4))
        .timeout(Duration::from_secs(20))
        .user_agent(concat!("Nieuwser/", env!("CARGO_PKG_VERSION")));
    if let Some(host) = resolve_host {
        builder = builder.resolve_to_addrs(&host, &addresses);
    }
    let client = builder.build()?;
    let mut clients = REMOTE_CLIENTS
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if clients.len() >= 32 {
        clients.clear();
    }
    clients.insert(key, client.clone());
    Ok(client)
}

fn safe_remote_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let octets = ip.octets();
            !(ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_multicast()
                || ip.is_unspecified()
                || ip.is_broadcast()
                || ip.is_documentation()
                || octets[0] == 0
                || (octets[0] == 100 && (64..=127).contains(&octets[1]))
                || (octets[0] == 192 && octets[1] == 0 && octets[2] == 0)
                || (octets[0] == 198 && matches!(octets[1], 18 | 19))
                || octets[0] >= 240)
        }
        IpAddr::V6(ip) => {
            !(ip.is_loopback()
                || ip.is_unique_local()
                || ip.is_unicast_link_local()
                || ip.is_multicast()
                || ip.is_unspecified()
                || ip.segments()[..2] == [0x2001, 0x0db8])
                && ip
                    .to_ipv4_mapped()
                    .is_none_or(|mapped| safe_remote_ip(mapped.into()))
        }
    }
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

    #[test]
    fn rejects_private_and_special_addresses() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "169.254.169.254",
            "192.168.0.1",
            "::1",
            "fd00::1",
            "2001:db8::1",
        ] {
            assert!(!safe_remote_ip(ip.parse().unwrap()), "{ip}");
        }
        assert!(safe_remote_ip("93.184.216.34".parse().unwrap()));
    }

    #[tokio::test]
    async fn rejects_loopback_image_urls() {
        let store = Store::open_in_memory().unwrap();
        let error = fetch_image(&store, "http://127.0.0.1/image.png")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("private address"));
    }
}
