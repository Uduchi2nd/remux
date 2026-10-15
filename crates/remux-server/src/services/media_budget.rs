//! Admission control for server-side paid media reads. Device playback stays direct.
use std::{path::PathBuf, time::Duration};
use uuid::Uuid;

use crate::{AppContext, db};

#[derive(Clone, Copy, serde::Serialize, strum::EnumString, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub(crate) enum Provider {
    Premiumize,
    Torbox,
    Other,
}

pub(crate) fn provider(info: &crate::stream::StreamInfo) -> Provider {
    let service = info
        .service_id
        .as_deref()
        .unwrap_or("")
        .to_ascii_lowercase();
    match service.as_str() {
        "torbox" | "tb" => return Provider::Torbox,
        "premiumize" | "premiumize.me" | "premiumizeme" | "pm" => {
            return Provider::Premiumize;
        }
        _ => {}
    }
    let Some(url) = info
        .descriptor
        .as_http_url()
    else {
        return Provider::Other;
    };
    if let Ok(url) = url::Url::parse(url) {
        let host = url
            .host_str()
            .unwrap_or("")
            .to_ascii_lowercase();
        if host == "torbox.app" || host.ends_with(".torbox.app") {
            return Provider::Torbox;
        }
        if ["vnphim", "nzbdav", "usenetstreamer"]
            .iter()
            .any(|word| host.contains(word))
        {
            return Provider::Other;
        }
    }
    let filename = info
        .filename
        .as_deref()
        .unwrap_or("")
        .to_ascii_lowercase();
    if filename.contains("vietnamese.thuyetminh")
        || filename.contains("vietnamese.longtieng")
        || filename.contains("proxiedvn")
    {
        return Provider::Other;
    }
    // An opaque addon URL with no trustworthy provider metadata gets the
    // stricter allowance rather than silently bypassing paid-read limits.
    Provider::Premiumize
}

#[derive(Clone)]
pub(crate) struct ReadBudget {
    endpoint: Option<String>,
    token_file: Option<PathBuf>,
    pub provider: Provider,
    bytes: u64,
    kind: &'static str,
}

impl ReadBudget {
    pub async fn for_media(
        ctx: &AppContext,
        media: &db::Media,
        kind: &'static str,
    ) -> Self {
        let hq = if let Some(id) = super::dubmux::hq_id_of(media) {
            db::Media::get_by_id(&ctx.db, &id)
                .await
                .ok()
                .flatten()
        } else {
            None
        };
        let media = hq
            .as_ref()
            .unwrap_or(media);
        let info = media
            .stream_info
            .as_ref();
        Self {
            endpoint: ctx
                .config
                .media_transfer_budget_url
                .clone(),
            token_file: ctx
                .config
                .media_transfer_budget_token_file
                .clone(),
            provider: info
                .map(provider)
                .unwrap_or(Provider::Premiumize),
            bytes: info
                .and_then(|info| info.size)
                .filter(|n| *n > 0)
                .map(|n| n as u64)
                .unwrap_or(if kind == "probe" {
                    128 * 1024 * 1024
                } else {
                    0
                }),
            kind,
        }
    }

    pub async fn acquire(&self) -> anyhow::Result<Permit> {
        if self.provider == Provider::Other {
            return Ok(Permit { release: None });
        }
        let endpoint = self
            .endpoint
            .as_deref()
            .ok_or_else(|| {
                anyhow::anyhow!("media transfer budget is not configured")
            })?;
        let token = tokio::fs::read_to_string(
            self.token_file
                .as_ref()
                .ok_or_else(|| {
                    anyhow::anyhow!("media transfer budget token is not configured")
                })?,
        )
        .await?;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_millis(500))
            .timeout(Duration::from_secs(2))
            .build()?;
        let response = client.post(format!("{}/transfer-budget/reserve", endpoint.trim_end_matches('/')))
            .bearer_auth(token.trim()).json(&serde_json::json!({"provider": self.provider, "bytes": self.bytes, "kind": self.kind}))
            .send().await.map_err(|_| anyhow::anyhow!("media transfer budget unavailable"))?;
        if response.status() != reqwest::StatusCode::OK {
            anyhow::bail!("media transfer budget deferred this read");
        }
        #[derive(serde::Deserialize)]
        struct Reply {
            allowed: bool,
            lease: Option<Uuid>,
        }
        let reply: Reply = response
            .json()
            .await?;
        anyhow::ensure!(
            reply.allowed
                && reply
                    .lease
                    .is_some(),
            "media transfer budget deferred this read"
        );
        Ok(Permit {
            release: reply
                .lease
                .map(|id| {
                    (
                        client,
                        format!(
                            "{}/transfer-budget/release/{id}",
                            endpoint.trim_end_matches('/')
                        ),
                        token,
                    )
                }),
        })
    }
}

pub(crate) struct Permit {
    release: Option<(reqwest::Client, String, String)>,
}

impl Drop for Permit {
    fn drop(&mut self) {
        if let Some((client, url, token)) = self
            .release
            .take()
        {
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    let _ = client
                        .post(url)
                        .bearer_auth(token.trim())
                        .send()
                        .await;
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::StreamDescriptor;
    #[tokio::test]
    async fn authenticated_reservation_is_released_and_denials_do_not_read_media() {
        use std::sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        };
        let events = Arc::new(Mutex::new(Vec::new()));
        let recorded = events.clone();
        let deny = Arc::new(AtomicBool::new(false));
        let server_deny = deny.clone();
        let lease = Uuid::new_v4();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let address = listener
            .local_addr()
            .unwrap();
        let app = axum::Router::new().fallback(axum::routing::post(
            move |uri: axum::http::Uri,
                  headers: axum::http::HeaderMap,
                  body: axum::body::Bytes| {
                assert_eq!(
                    headers
                        .get("authorization")
                        .and_then(|h| h
                            .to_str()
                            .ok()),
                    Some("Bearer local-fixture-token")
                );
                let body = serde_json::from_slice::<serde_json::Value>(&body)
                    .unwrap_or(serde_json::Value::Null);
                recorded
                    .lock()
                    .unwrap()
                    .push((
                        uri.path()
                            .to_string(),
                        body,
                    ));
                let denied = server_deny.load(Ordering::SeqCst);
                async move {
                    (
                        if denied {
                            axum::http::StatusCode::TOO_MANY_REQUESTS
                        } else {
                            axum::http::StatusCode::OK
                        },
                        axum::Json(
                            serde_json::json!({"allowed":!denied,"lease":lease}),
                        ),
                    )
                }
            },
        ));
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .unwrap()
        });
        let temp = tempfile::tempdir().unwrap();
        let token_path = temp
            .path()
            .join("token");
        tokio::fs::write(&token_path, "local-fixture-token")
            .await
            .unwrap();
        let budget = ReadBudget {
            endpoint: Some(format!("http://{address}")),
            token_file: Some(token_path),
            provider: Provider::Premiumize,
            bytes: 1000,
            kind: "full",
        };
        let permit = budget
            .acquire()
            .await
            .unwrap();
        assert_eq!(
            events
                .lock()
                .unwrap()[0]
                .1,
            serde_json::json!({"provider":"premiumize","bytes":1000,"kind":"full"})
        );
        drop(permit);
        let end = tokio::time::Instant::now() + Duration::from_secs(1);
        while events
            .lock()
            .unwrap()
            .len()
            < 2
            && tokio::time::Instant::now() < end
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            events
                .lock()
                .unwrap()[1]
                .0,
            format!("/transfer-budget/release/{lease}")
        );
        deny.store(true, Ordering::SeqCst);
        assert!(
            budget
                .acquire()
                .await
                .is_err()
        );
        server.abort();
    }
    #[test]
    fn classification_uses_service_metadata_and_keeps_unknown_sources_strict() {
        let mut info = crate::stream::StreamInfo {
            descriptor: StreamDescriptor::http("https://opaque.example/video"),
            ..Default::default()
        };
        assert_eq!(provider(&info), Provider::Premiumize);
        info.service_id = Some("torbox".into());
        assert_eq!(provider(&info), Provider::Torbox);
        info.service_id = Some("premiumize".into());
        assert_eq!(provider(&info), Provider::Premiumize);
        info.service_id = None;
        info.descriptor = StreamDescriptor::http("https://nzbdav.example/video");
        assert_eq!(provider(&info), Provider::Other);
        info.descriptor = StreamDescriptor::Local(PathBuf::from("/media/file.mkv"));
        assert_eq!(provider(&info), Provider::Other);
    }
}
