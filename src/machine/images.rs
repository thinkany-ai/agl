//! Images in built-in tool results (computer use, host-registered tools):
//! how they go back to the caller. Only callers that ask (`images: true`)
//! get the bytes; a screenshot the same as the last one sent for that
//! place is marked `unchanged`; with an `image_upload` URL the bytes are
//! PUT there instead of returned (falling back to inline with a warning).

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

/// Largest image handed back (base64 bytes); bigger ones are `omitted`.
pub(crate) const MAX_IMAGE_B64: usize = 4 * 1024 * 1024 * 4 / 3;
/// Longest a screenshot upload (`image_upload`) may take.
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(30);
/// Screenshot hashes remembered for `unchanged`.
const MAX_SHOTS: usize = 4096;

/// Where a screenshot was taken, for `unchanged`: (link, session, and two
/// parts the tool chooses, e.g. app and window).
pub(crate) type ShotKey = (String, String, String, String);

/// Images go back only to callers that asked for them (`images: true`);
/// others, and images over 4 MB, get `omitted` placeholders.
pub fn shape_images(r: &mut Value, wanted: bool) {
    let Some(list) = r.get_mut("images").and_then(Value::as_array_mut) else { return };
    for img in list {
        let too_big = img["data"].as_str().is_some_and(|d| d.len() > MAX_IMAGE_B64);
        if !wanted || too_big {
            if let Some(o) = img.as_object_mut() {
                o.remove("data");
                o.insert("omitted".into(), json!(true));
            }
        }
    }
    if !wanted {
        r["images_note"] = json!("pass images: true to tools.call to receive screenshots");
    }
}

/// The last screenshot sent per place.
#[derive(Default)]
pub(crate) struct Shots(std::sync::Mutex<HashMap<ShotKey, String>>);

impl Shots {
    /// Sends each image of `r` the way the caller asked (see the module).
    pub(crate) async fn deliver(&self, key: ShotKey, r: &mut Value, upload: Option<&str>) {
        let last = self.0.lock().unwrap().get(&key).cloned();
        let sent = deliver_images(r, last.as_deref(), |bytes| async move {
            match upload {
                Some(url) => put_image(url, bytes).await.map(Some),
                None => Ok(None),
            }
        })
        .await;
        if let Some(hash) = sent {
            let mut shots = self.0.lock().unwrap();
            if shots.len() >= MAX_SHOTS {
                shots.clear();
            }
            shots.insert(key, hash);
        }
    }

    /// The next screenshot of every place in this session is sent again.
    pub(crate) fn forget(&self, hub: &str, session: &str) {
        self.0.lock().unwrap().retain(|k, _| !(k.0 == hub && k.1 == session));
    }
}

pub(crate) fn hash(data: &str) -> String {
    use sha2::Digest;
    format!("{:x}", sha2::Sha256::digest(data.as_bytes()))
}

/// The images of a result as they go to the caller. `upload` PUTs the
/// image bytes and says whether it did (`Some`), was not asked to (`None`),
/// or failed (`Err`). Returns the hash of the image sent, if one was.
pub(crate) async fn deliver_images<F, Fut>(r: &mut Value, last: Option<&str>, upload: F) -> Option<String>
where
    F: Fn(Vec<u8>) -> Fut,
    Fut: std::future::Future<Output = Result<Option<()>>>,
{
    use base64::Engine;
    let mut sent = None;
    let mut warnings = Vec::new();
    if let Some(list) = r.get_mut("images").and_then(Value::as_array_mut) {
        for img in list {
            let Some(data) = img["data"].as_str().map(str::to_string) else { continue };
            let h = hash(&data);
            let Some(o) = img.as_object_mut() else { continue };
            if last == Some(h.as_str()) {
                o.remove("data");
                o.insert("unchanged".into(), json!(true));
                continue;
            }
            let bytes = match base64::engine::general_purpose::STANDARD.decode(&data) {
                Ok(b) => b,
                Err(_) => continue,
            };
            match upload(bytes).await {
                Ok(Some(())) => {
                    o.remove("data");
                    o.insert("uploaded".into(), json!(true));
                    o.insert("bytes".into(), json!(data.len()));
                }
                Ok(None) => {}
                Err(e) => warnings.push(format!("image_upload_failed: {e:#}")),
            }
            sent = Some(h);
        }
    }
    if !warnings.is_empty() {
        if !r["warnings"].is_array() {
            r["warnings"] = json!([]);
        }
        if let Some(w) = r["warnings"].as_array_mut() {
            w.extend(warnings.into_iter().map(Value::from));
        }
    }
    sent
}

/// Where an image may be uploaded: HTTPS, or HTTP to this machine (a
/// development hub's storage). No credentials or fragment in the URL.
pub(crate) fn upload_url(raw: &str) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(raw).map_err(|_| anyhow!("invalid upload URL"))?;
    let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    let ok = (url.scheme() == "https" || (url.scheme() == "http" && loopback))
        && url.host().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none();
    if !ok {
        bail!("expected an HTTPS upload URL");
    }
    Ok(url)
}

/// PUTs a JPEG image. Errors never contain the URL: it is a signed
/// credential.
async fn put_image(raw: &str, bytes: Vec<u8>) -> Result<()> {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    let url = upload_url(raw)?;
    let client = match CLIENT.get() {
        Some(c) => c,
        None => {
            let c = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(10))
                .timeout(UPLOAD_TIMEOUT)
                .build()
                .map_err(|_| anyhow!("no HTTP client"))?;
            CLIENT.get_or_init(|| c)
        }
    };
    let response = client
        .put(url)
        .header(reqwest::header::CONTENT_TYPE, "image/jpeg")
        .body(bytes)
        .send()
        .await
        .map_err(|e| {
            if e.is_timeout() {
                anyhow!("timed out")
            } else if e.is_connect() {
                anyhow!("could not connect")
            } else {
                anyhow!("request failed")
            }
        })?;
    if !response.status().is_success() {
        bail!("HTTP {}", response.status().as_u16());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn images_are_opt_in() {
        let mut r = json!({"images": [{"mime_type": "image/jpeg", "data": "abcd"}]});
        shape_images(&mut r, false);
        assert_eq!(r["images"][0], json!({"mime_type": "image/jpeg", "omitted": true}));
        let mut r = json!({"images": [{"mime_type": "image/jpeg", "data": "abcd"}]});
        shape_images(&mut r, true);
        assert_eq!(r["images"][0]["data"], "abcd");
    }

    fn shot() -> Value {
        json!({"images": [{"mime_type": "image/jpeg", "data": "aGVsbG8="}]})
    }

    #[tokio::test]
    async fn screenshots_upload_or_stay_inline() {
        // Uploaded: no data, the base64 length as bytes.
        let mut r = shot();
        let seen = std::sync::Mutex::new(Vec::new());
        let sent = deliver_images(&mut r, None, |b| {
            seen.lock().unwrap().push(b);
            async { Ok(Some(())) }
        })
        .await;
        assert_eq!(r["images"][0], json!({"mime_type": "image/jpeg", "uploaded": true, "bytes": 8}));
        assert_eq!(seen.lock().unwrap().as_slice(), &[b"hello".to_vec()]);
        assert_eq!(sent, Some(hash("aGVsbG8=")));
        // A failed upload keeps the data and says why.
        let mut r = shot();
        let sent = deliver_images(&mut r, None, |_| async { Err(anyhow!("HTTP 403")) }).await;
        assert_eq!(r["images"][0]["data"], "aGVsbG8=");
        assert_eq!(r["warnings"], json!(["image_upload_failed: HTTP 403"]));
        assert!(sent.is_some());
        // No upload asked: inline.
        let mut r = shot();
        deliver_images(&mut r, None, |_| async { Ok(None) }).await;
        assert_eq!(r["images"][0]["data"], "aGVsbG8=");
    }

    #[tokio::test]
    async fn the_same_screenshot_is_not_sent_twice() {
        let mut r = shot();
        let sent = deliver_images(&mut r, Some(&hash("aGVsbG8=")), |_| async { panic!("no upload of an unchanged screenshot") }).await;
        assert_eq!(r["images"][0], json!({"mime_type": "image/jpeg", "unchanged": true}));
        assert_eq!(sent, None);
    }

    #[tokio::test]
    async fn shots_are_per_place_and_forget_clears_a_session() {
        let s = Shots::default();
        let key = |session: &str, place: &str| ("h".to_string(), session.to_string(), place.to_string(), String::new());
        let mut r = shot();
        s.deliver(key("s", "a"), &mut r, None).await;
        assert!(r["images"][0]["data"].is_string());
        let mut r = shot();
        s.deliver(key("s", "a"), &mut r, None).await;
        assert_eq!(r["images"][0]["unchanged"], true);
        let mut r = shot();
        s.deliver(key("s", "b"), &mut r, None).await;
        assert!(r["images"][0]["data"].is_string());
        s.forget("h", "s");
        let mut r = shot();
        s.deliver(key("s", "a"), &mut r, None).await;
        assert!(r["images"][0]["data"].is_string());
    }

    #[test]
    fn upload_urls() {
        assert!(upload_url("https://bucket.example/k?X-Sig=1").is_ok());
        assert!(upload_url("http://localhost:9000/k").is_ok());
        assert!(upload_url("http://bucket.example/k").is_err());
        assert!(upload_url("https://u:p@bucket.example/k").is_err());
        assert!(upload_url("ftp://bucket.example/k").is_err());
        assert!(upload_url("not a url").is_err());
        // The error never repeats the URL.
        assert!(!format!("{:#}", upload_url("http://secret.example/sig").unwrap_err()).contains("secret"));
    }
}
