//! The admission service's entitlement read API — `GET {admission_url}/
//! premium/{pubkey}` → `{ "premium": bool }` (ADR-0008; *Premium* §1.1) —
//! asked, **fail-closed**, whether a pubkey is premium.
//!
//! The fleet asks it in one rare case: a Direct Challenge that imposes the
//! bot's variant **asymmetrically**, the premium-only form a Sashité client
//! accepts only from a premium challenger (*Premium* §1.3; ADR-0040 §2) — and,
//! at rematch, whether the original imposer of such a configuration still is
//! (§1.3, the re-check). Premium status does not exist apart from the service
//! (its allow-list is the one source of truth), so while the service is down
//! nobody is premium and refusing is the right answer: an unreachable host,
//! a non-200, a malformed body, a timeout, or no URL configured at all, and
//! the answer is `false`. The request is bounded well under the acceptance
//! margin, so a slow service never eats a challenge's life.

use std::time::Duration;

use nostr_sdk::prelude::PublicKey;

/// The bound on one lookup — under `COURT_MARGIN_SECS`, so a hung service
/// costs a few seconds of a challenge's window, never the whole of it.
const TIMEOUT: Duration = Duration::from_secs(5);

/// Whether `pubkey` is premium, per the admission service at `base_url` —
/// `false` on every failure, and without a request when there is no URL.
pub async fn is_premium(base_url: Option<&str>, pubkey: &PublicKey) -> bool {
    let Some(base_url) = base_url else {
        tracing::debug!(%pubkey, "premium lookup: no admission service configured — not premium");
        return false;
    };
    let url = format!("{base_url}/premium/{}", pubkey.to_hex());
    let client = match reqwest::Client::builder().timeout(TIMEOUT).build() {
        Ok(client) => client,
        Err(error) => {
            tracing::warn!(error = %error, "premium lookup: client build failed — not premium");
            return false;
        }
    };
    let response = match client.get(&url).send().await {
        Ok(response) => response,
        Err(error) => {
            tracing::debug!(%pubkey, error = %error, "premium lookup failed — not premium");
            return false;
        }
    };
    if !response.status().is_success() {
        tracing::debug!(%pubkey, status = %response.status(), "premium lookup: non-success — not premium");
        return false;
    }
    let body = match response.bytes().await {
        Ok(body) => body,
        Err(error) => {
            tracing::debug!(%pubkey, error = %error, "premium lookup: body unreadable — not premium");
            return false;
        }
    };
    match serde_json::from_slice::<Answer>(&body) {
        Ok(answer) => {
            tracing::debug!(%pubkey, premium = answer.premium, "premium lookup");
            answer.premium
        }
        Err(error) => {
            tracing::debug!(%pubkey, error = %error, "premium lookup: malformed body — not premium");
            false
        }
    }
}

/// `{ "premium": bool }`.
#[derive(serde::Deserialize)]
struct Answer {
    premium: bool,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use nostr_sdk::prelude::Keys;

    #[tokio::test]
    async fn no_url_means_not_premium_without_a_request() {
        assert!(!is_premium(None, &Keys::generate().public_key()).await);
    }

    #[tokio::test]
    async fn an_unreachable_service_means_not_premium() {
        // A closed port: connection refused, at once.
        assert!(!is_premium(Some("http://127.0.0.1:1"), &Keys::generate().public_key()).await);
    }

    #[tokio::test]
    async fn the_answer_is_read_and_a_wrong_status_or_body_is_false() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        // A one-shot HTTP/1.1 server per case.
        async fn serve(status: &'static str, body: &'static str) -> String {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buf = [0_u8; 1024];
                let _ = socket.read(&mut buf).await;
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
            format!("http://{addr}")
        }
        let key = Keys::generate().public_key();
        assert!(is_premium(Some(&serve("200 OK", r#"{"premium":true}"#).await), &key).await);
        assert!(!is_premium(Some(&serve("200 OK", r#"{"premium":false}"#).await), &key).await);
        assert!(
            !is_premium(
                Some(&serve("500 Internal Server Error", "lookup failed").await),
                &key
            )
            .await
        );
        assert!(!is_premium(Some(&serve("200 OK", "not json").await), &key).await);
    }
}
