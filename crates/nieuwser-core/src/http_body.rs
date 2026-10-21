use crate::{Error, Result};

/// Read a response body in chunks so a server cannot allocate past `max_bytes`.
pub async fn text(mut response: reqwest::Response, max_bytes: usize, what: &str) -> Result<String> {
    let bytes = bytes(&mut response, max_bytes, what).await?;
    String::from_utf8(bytes)
        .map_err(|error| Error::Other(format!("{what} was not valid UTF-8: {error}")))
}

pub async fn bytes(
    response: &mut reqwest::Response,
    max_bytes: usize,
    what: &str,
) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        return Err(too_large(what, max_bytes));
    }

    let capacity = response
        .content_length()
        .and_then(|length| usize::try_from(length).ok())
        .unwrap_or(8 * 1024)
        .min(max_bytes)
        .min(64 * 1024);
    let mut body = Vec::with_capacity(capacity);
    while let Some(chunk) = response.chunk().await? {
        append_bounded(&mut body, &chunk, max_bytes, what)?;
    }
    Ok(body)
}

fn append_bounded(body: &mut Vec<u8>, chunk: &[u8], max_bytes: usize, what: &str) -> Result<()> {
    if chunk.len() > max_bytes.saturating_sub(body.len()) {
        return Err(too_large(what, max_bytes));
    }
    body.extend_from_slice(chunk);
    Ok(())
}

fn too_large(what: &str, max_bytes: usize) -> Error {
    Error::Other(format!(
        "{what} exceeded the {max_bytes}-byte response limit"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_chunks_past_the_limit() {
        let mut body = vec![1, 2, 3];
        append_bounded(&mut body, &[4, 5], 5, "test body").unwrap();
        assert_eq!(body, [1, 2, 3, 4, 5]);
        let error = append_bounded(&mut body, &[6], 5, "test body").unwrap_err();
        assert!(error.to_string().contains("5-byte response limit"));
    }
}
