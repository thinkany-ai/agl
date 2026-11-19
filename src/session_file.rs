//! Deliver a file explicitly selected from a session's working directory.

use std::path::{Component, Path};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use futures_util::stream;
use reqwest::{header, Client, Url};
use serde_json::{json, Value};
use tokio::fs::{self, File, OpenOptions};
use tokio::io::AsyncReadExt;

const MAX_BYTES: u64 = 400 * 1024 * 1024;

fn visible_relative_path(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path.components().all(|component| {
            matches!(component, Component::Normal(name) if !name.to_string_lossy().starts_with('.'))
        })
}

async fn open_file(cwd: &Path, path: &Path) -> Result<(File, u64)> {
    if !visible_relative_path(path) {
        bail!("file must be inside the session workspace");
    }
    let root = fs::canonicalize(cwd)
        .await
        .context("workspace unavailable")?;
    let resolved = fs::canonicalize(root.join(path))
        .await
        .context("file unavailable")?;
    if !resolved
        .strip_prefix(&root)
        .is_ok_and(visible_relative_path)
    {
        bail!("file must be inside the session workspace");
    }
    let mut options = OpenOptions::new();
    options.read(true);
    // Prevent final-component symlink substitution and blocking on a FIFO.
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let file = options.open(resolved).await.context("cannot open file")?;
    let meta = file.metadata().await.context("cannot read file metadata")?;
    if !meta.is_file() || meta.len() == 0 || meta.len() > MAX_BYTES {
        bail!("expected a nonempty regular file up to 400 MiB");
    }
    Ok((file, meta.len()))
}

pub(crate) async fn upload(cwd: &Path, path: &str, upload_url: &str) -> Result<Value> {
    let url = Url::parse(upload_url).context("invalid upload URL")?;
    if url.scheme() != "https"
        || url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        bail!("expected an HTTPS upload URL");
    }
    let (file, size) = open_file(cwd, Path::new(path)).await?;
    let client = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(29 * 60))
        .build()?;
    put(&client, url, file, size).await?;
    Ok(json!({"size": size}))
}

async fn put(client: &Client, url: Url, file: File, size: u64) -> Result<()> {
    let chunks = stream::try_unfold(file.take(size), |mut file| async move {
        if file.limit() == 0 {
            return Ok(None);
        }
        let mut bytes = vec![0; 64 * 1024];
        let read = file.read(&mut bytes).await?;
        if read == 0 {
            return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof));
        }
        bytes.truncate(read);
        Ok(Some((bytes, file)))
    });
    let response = client
        .put(url)
        .header(header::CONTENT_LENGTH, size)
        .body(reqwest::Body::wrap_stream(chunks))
        .send()
        .await
        .map_err(reqwest::Error::without_url)
        .context("file upload failed")?;
    if !response.status().is_success() {
        bail!("file upload failed (HTTP {})", response.status().as_u16());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Bytes, http::HeaderMap, routing::put as put_route, Router};
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn streams_the_selected_file_with_no_credentials() -> Result<()> {
        let root = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        fs::create_dir(&root).await?;
        let file = root.join("promo video.mp4");
        let sample = vec![0xa5; 128 * 1024 + 7];
        fs::write(&file, &sample).await?;

        let alternate = root.join(".credentials");
        fs::write(&alternate, b"private").await?;
        let link = root.join("outside");
        #[cfg(unix)]
        std::os::unix::fs::symlink(std::env::temp_dir(), &link)?;
        assert!(open_file(&root, Path::new("../outside")).await.is_err());
        assert!(open_file(&root, Path::new(".credentials")).await.is_err());
        assert!(open_file(&root, &file).await.is_err());
        assert!(open_file(&root, Path::new("outside")).await.is_err());

        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = Url::parse(&format!("http://{}/upload", listener.local_addr()?))?;
        let (sender, received) = tokio::sync::oneshot::channel();
        let sender = std::sync::Arc::new(std::sync::Mutex::new(Some(sender)));
        let server = tokio::spawn(async move {
            let app = Router::new().route(
                "/upload",
                put_route(move |headers: HeaderMap, body: Bytes| {
                    let sender = sender.clone();
                    async move {
                        sender
                            .lock()
                            .unwrap()
                            .take()
                            .unwrap()
                            .send((headers, body))
                            .unwrap();
                    }
                }),
            );
            axum::serve(listener, app).await.unwrap();
        });

        let (body, size) = open_file(&root, Path::new("promo video.mp4")).await?;
        assert!(upload(&root, "promo video.mp4", endpoint.as_str())
            .await
            .is_err());
        let client = Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()?;
        put(&client, endpoint, body, size).await?;
        let (headers, body) = received.await?;
        assert_eq!(headers[header::CONTENT_LENGTH], size.to_string());
        assert!(!headers.contains_key(header::AUTHORIZATION));
        assert_eq!(body.as_ref(), &sample);
        server.abort();
        fs::remove_dir_all(root).await?;
        Ok(())
    }
}
