//! X (Twitter) publisher — X API v2 `POST /2/tweets` with OAuth 1.0a User Context.
//!
//! Credentials come from the environment (never from the repository):
//! `X_API_KEY`, `X_API_SECRET`, `X_ACCESS_TOKEN` and `X_ACCESS_TOKEN_SECRET`.
//!
//! The OAuth 1.0a request signing (HMAC-SHA1 over the RFC 5849 signature base
//! string) is implemented here in pure Rust and is unit-tested against the
//! published reference vectors, so its correctness is provable **without any
//! network access** (see the tests at the bottom of this file).

use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine;
use hmac::{Hmac, Mac};
use sha1::Sha1;

/// Hosted X API entry point.
pub const DEFAULT_X_API_BASE_URL: &str = "https://api.x.com";

/// Endpoint that creates a post (X API v2).
pub const CREATE_POST_PATH: &str = "/2/tweets";

/// OAuth 1.0a signature method carried by the `Authorization` header.
const SIGNATURE_METHOD: &str = "HMAC-SHA1";

/// OAuth protocol version carried by the `Authorization` header.
const OAUTH_VERSION: &str = "1.0";

/// Environment variables required for OAuth 1.0a user-context signing.
pub const X_ENV_VARS: [&str; 4] = [
    "X_API_KEY",
    "X_API_SECRET",
    "X_ACCESS_TOKEN",
    "X_ACCESS_TOKEN_SECRET",
];

/// Credentials read from the environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XCredentials {
    pub api_key: String,
    pub api_secret: String,
    pub access_token: String,
    pub access_token_secret: String,
}

impl XCredentials {
    /// Read the four OAuth 1.0a variables from the environment.
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            api_key: crate::env_var("X_API_KEY")?,
            api_secret: crate::env_var("X_API_SECRET")?,
            access_token: crate::env_var("X_ACCESS_TOKEN")?,
            access_token_secret: crate::env_var("X_ACCESS_TOKEN_SECRET")?,
        })
    }
}

/// Names of the required variables that are missing or empty.
pub fn missing_env_vars() -> Vec<&'static str> {
    X_ENV_VARS
        .iter()
        .copied()
        .filter(|name| {
            std::env::var(name)
                .map(|value| value.trim().is_empty())
                .unwrap_or(true)
        })
        .collect()
}

/// Percent-encode a string per RFC 5849 §3.6 / RFC 3986 §2.3.
///
/// Only the unreserved characters (`A-Z a-z 0-9 - . _ ~`) are left untouched;
/// every other byte becomes `%XX` with uppercase hexadecimal digits.
pub fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        if is_unreserved(byte) {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push(hex_digit(byte >> 4));
            out.push(hex_digit(byte));
        }
    }
    out
}

fn is_unreserved(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~')
}

fn hex_digit(nibble: u8) -> char {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    HEX[(nibble & 0x0f) as usize] as char
}

/// Normalize parameters into the OAuth parameter string (RFC 5849 §3.4.1.3.2):
/// encode every name and value, sort by name then value, join with `&`.
pub fn normalized_parameter_string(params: &[(String, String)]) -> String {
    let mut encoded: Vec<(String, String)> = params
        .iter()
        .map(|(name, value)| (percent_encode(name), percent_encode(value)))
        .collect();
    encoded.sort();

    encoded
        .into_iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// Build the signature base string (RFC 5849 §3.4.1).
pub fn signature_base_string(method: &str, base_url: &str, params: &[(String, String)]) -> String {
    format!(
        "{}&{}&{}",
        method.to_ascii_uppercase(),
        percent_encode(base_url),
        percent_encode(&normalized_parameter_string(params)),
    )
}

/// The HMAC-SHA1 signing key: `pct(consumer_secret) & pct(token_secret)`.
pub fn signing_key(consumer_secret: &str, token_secret: &str) -> String {
    format!(
        "{}&{}",
        percent_encode(consumer_secret),
        percent_encode(token_secret),
    )
}

/// Base64 HMAC-SHA1 of `message` under `key`.
///
/// HMAC accepts keys of any length, so the `Result` only exists to keep the
/// failure path explicit: production code never panics.
pub fn hmac_sha1_base64(key: &str, message: &str) -> Result<String> {
    let mut mac =
        Hmac::<Sha1>::new_from_slice(key.as_bytes()).context("invalid HMAC-SHA1 key length")?;
    mac.update(message.as_bytes());
    Ok(base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes()))
}

/// Build the `Authorization: OAuth ...` header for a request.
///
/// `extra_params` are request parameters that participate in the signature
/// (query-string parameters or an `application/x-www-form-urlencoded` body).
/// The X JSON body is **not** part of the signature, so the live call passes an
/// empty slice here.
pub fn authorization_header(
    credentials: &XCredentials,
    method: &str,
    base_url: &str,
    extra_params: &[(&str, &str)],
    nonce: &str,
    timestamp: u64,
) -> Result<String> {
    let oauth_params: Vec<(String, String)> = vec![
        ("oauth_consumer_key".into(), credentials.api_key.clone()),
        ("oauth_nonce".into(), nonce.to_string()),
        (
            "oauth_signature_method".into(),
            SIGNATURE_METHOD.to_string(),
        ),
        ("oauth_timestamp".into(), timestamp.to_string()),
        ("oauth_token".into(), credentials.access_token.clone()),
        ("oauth_version".into(), OAUTH_VERSION.to_string()),
    ];

    // The signature covers the OAuth parameters plus the request parameters.
    let mut signing_params = oauth_params.clone();
    signing_params.extend(
        extra_params
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string())),
    );

    let base_string = signature_base_string(method, base_url, &signing_params);
    let signature = hmac_sha1_base64(
        &signing_key(&credentials.api_secret, &credentials.access_token_secret),
        &base_string,
    )?;

    // The header carries only the oauth_* parameters, sorted by name.
    let mut header_params = oauth_params;
    header_params.push(("oauth_signature".into(), signature));
    header_params.sort();

    let rendered = header_params
        .iter()
        .map(|(name, value)| format!("{}=\"{}\"", percent_encode(name), percent_encode(value)))
        .collect::<Vec<_>>()
        .join(", ");
    Ok(format!("OAuth {rendered}"))
}

/// A random nonce (32 lowercase hex characters) for a single request.
pub fn generate_nonce() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes)
        .map_err(|err| anyhow::anyhow!("generating an OAuth nonce: {err}"))?;

    let mut nonce = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        nonce.push(hex_digit(byte >> 4));
        nonce.push(hex_digit(byte));
    }
    Ok(nonce)
}

/// Current Unix time in seconds, for `oauth_timestamp`.
pub fn now_unix_seconds() -> u64 {
    u64::try_from(chrono::Utc::now().timestamp()).unwrap_or_default()
}

/// Full URL of the create-post endpoint for `base_url`.
pub fn post_endpoint(base_url: &str) -> String {
    format!("{}{CREATE_POST_PATH}", base_url.trim_end_matches('/'))
}

/// Canonical human-readable URL for a post id.
pub fn post_url(id: &str) -> String {
    format!("https://x.com/i/web/status/{id}")
}

/// Build the JSON body the X API expects (`{"text": "..."}`).
pub fn build_tweet_body(text: &str) -> serde_json::Value {
    serde_json::json!({ "text": text })
}

/// A post created by the API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedPost {
    pub id: String,
    pub url: String,
}

/// Parse a `POST /2/tweets` response into the created post.
pub fn parse_create_response(raw: &str) -> Result<PublishedPost> {
    let payload: serde_json::Value =
        serde_json::from_str(raw).context("invalid X create-post JSON")?;

    if let Some(id) = payload
        .get("data")
        .and_then(|data| data.get("id"))
        .and_then(serde_json::Value::as_str)
        .filter(|id| !id.is_empty())
    {
        return Ok(PublishedPost {
            id: id.to_string(),
            url: post_url(id),
        });
    }

    // Error shapes: `{"errors":[{"detail":"..."}]}` or `{"detail":"..."}`.
    let message = payload
        .get("errors")
        .and_then(serde_json::Value::as_array)
        .and_then(|errors| errors.first())
        .and_then(|error| error.get("detail"))
        .and_then(serde_json::Value::as_str)
        .or_else(|| payload.get("detail").and_then(serde_json::Value::as_str))
        .unwrap_or("unknown error");
    bail!("X rejected the post: {message}");
}

/// Create a post and return it (id + URL).
pub async fn create_post(
    base_url: &str,
    credentials: &XCredentials,
    text: &str,
    timeout: Duration,
) -> Result<PublishedPost> {
    let url = post_endpoint(base_url);
    let authorization =
        credentials.authorization_header("POST", &url, &generate_nonce()?, now_unix_seconds())?;
    let body = build_tweet_body(text);

    let response = tokio::time::timeout(
        timeout,
        reqwest::Client::new()
            .post(&url)
            .header(reqwest::header::AUTHORIZATION, authorization)
            .json(&body)
            .send(),
    )
    .await
    .context("X post request timed out")?
    .context("X post request failed")?;

    let status = response.status();
    let raw = response.text().await.context("invalid X response body")?;

    if status.is_success() {
        return parse_create_response(&raw);
    }
    match parse_create_response(&raw) {
        Ok(post) => bail!(
            "X returned HTTP {status} with an unexpected post id {}",
            post.id
        ),
        Err(err) => Err(err.context(format!("X post failed (HTTP {status})"))),
    }
}

impl XCredentials {
    /// OAuth 1.0a `Authorization` header for a JSON POST with no query params.
    pub fn authorization_header(
        &self,
        method: &str,
        base_url: &str,
        nonce: &str,
        timestamp: u64,
    ) -> Result<String> {
        authorization_header(self, method, base_url, &[], nonce, timestamp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The classic Twitter "Creating a signature" credentials.
    fn twitter_credentials() -> XCredentials {
        XCredentials {
            api_key: "xvz1evFS4wEEPTGEFPHBog".into(),
            api_secret: "kAcSOqF21Fu85e7zjz7ZN2U4ZRhfV3WpwPAoE3Z7kBw".into(),
            access_token: "370773112-GmHxMAgYyLbNEtIKZeRNFsMKPR9EyMZeS9weJAEb".into(),
            access_token_secret: "LswwdoUaIvS8ltyTt5jkRh4J50vUPVVHtR2YPi5kE".into(),
        }
    }

    /// Signature base string of the Twitter example.
    const TWITTER_BASE_STRING: &str = "POST&https%3A%2F%2Fapi.twitter.com%2F1%2Fstatuses%2Fupdate.json&include_entities%3Dtrue%26oauth_consumer_key%3Dxvz1evFS4wEEPTGEFPHBog%26oauth_nonce%3DkYjzVBB8Y0ZFabxSWbWovY3uYSQ2pTgmZeNu2VS4cg%26oauth_signature_method%3DHMAC-SHA1%26oauth_timestamp%3D1318622958%26oauth_token%3D370773112-GmHxMAgYyLbNEtIKZeRNFsMKPR9EyMZeS9weJAEb%26oauth_version%3D1.0%26status%3DHello%2520Ladies%2520%252B%2520Gentlemen%252C%2520a%2520signed%2520OAuth%2520request%2521";

    #[test]
    fn percent_encode_follows_rfc3986() {
        assert_eq!(percent_encode("abcXYZ019-._~"), "abcXYZ019-._~");
        assert_eq!(percent_encode("=%3D"), "%3D%253D");
        assert_eq!(
            percent_encode("Hello Ladies + Gentlemen, a signed OAuth request!"),
            "Hello%20Ladies%20%2B%20Gentlemen%2C%20a%20signed%20OAuth%20request%21"
        );
    }

    #[test]
    fn signature_base_string_matches_twitter_vector() {
        // Source: the classic Twitter OAuth 1.0a example, reproduced in
        // ddo/oauth-1.0a `test/twitter_sample_data.js`
        // (https://github.com/ddo/oauth-1.0a/blob/master/test/twitter_sample_data.js),
        // which mirrors the former Twitter "Creating a signature" documentation.
        let params = vec![
            ("include_entities".to_string(), "true".to_string()),
            (
                "oauth_consumer_key".to_string(),
                "xvz1evFS4wEEPTGEFPHBog".to_string(),
            ),
            (
                "oauth_nonce".to_string(),
                "kYjzVBB8Y0ZFabxSWbWovY3uYSQ2pTgmZeNu2VS4cg".to_string(),
            ),
            (
                "oauth_signature_method".to_string(),
                "HMAC-SHA1".to_string(),
            ),
            ("oauth_timestamp".to_string(), "1318622958".to_string()),
            (
                "oauth_token".to_string(),
                "370773112-GmHxMAgYyLbNEtIKZeRNFsMKPR9EyMZeS9weJAEb".to_string(),
            ),
            ("oauth_version".to_string(), "1.0".to_string()),
            (
                "status".to_string(),
                "Hello Ladies + Gentlemen, a signed OAuth request!".to_string(),
            ),
        ];
        assert_eq!(
            signature_base_string(
                "POST",
                "https://api.twitter.com/1/statuses/update.json",
                &params
            ),
            TWITTER_BASE_STRING
        );
    }

    #[test]
    fn signing_key_matches_twitter_vector() {
        assert_eq!(
            signing_key(
                "kAcSOqF21Fu85e7zjz7ZN2U4ZRhfV3WpwPAoE3Z7kBw",
                "LswwdoUaIvS8ltyTt5jkRh4J50vUPVVHtR2YPi5kE"
            ),
            "kAcSOqF21Fu85e7zjz7ZN2U4ZRhfV3WpwPAoE3Z7kBw&LswwdoUaIvS8ltyTt5jkRh4J50vUPVVHtR2YPi5kE"
        );
    }

    #[test]
    fn hmac_sha1_matches_twitter_vector() {
        assert_eq!(
            hmac_sha1_base64(
                "kAcSOqF21Fu85e7zjz7ZN2U4ZRhfV3WpwPAoE3Z7kBw&LswwdoUaIvS8ltyTt5jkRh4J50vUPVVHtR2YPi5kE",
                TWITTER_BASE_STRING
            )
            .expect("HMAC-SHA1 signs"),
            "tnnArxj06cWHq44gCs1OSKk/jLY="
        );
    }

    #[test]
    fn authorization_header_matches_twitter_vector() {
        let header = authorization_header(
            &twitter_credentials(),
            "POST",
            "https://api.twitter.com/1/statuses/update.json",
            &[
                ("include_entities", "true"),
                (
                    "status",
                    "Hello Ladies + Gentlemen, a signed OAuth request!",
                ),
            ],
            "kYjzVBB8Y0ZFabxSWbWovY3uYSQ2pTgmZeNu2VS4cg",
            1318622958,
        )
        .expect("header builds");

        assert_eq!(
            header,
            concat!(
                "OAuth oauth_consumer_key=\"xvz1evFS4wEEPTGEFPHBog\", ",
                "oauth_nonce=\"kYjzVBB8Y0ZFabxSWbWovY3uYSQ2pTgmZeNu2VS4cg\", ",
                "oauth_signature=\"tnnArxj06cWHq44gCs1OSKk%2FjLY%3D\", ",
                "oauth_signature_method=\"HMAC-SHA1\", ",
                "oauth_timestamp=\"1318622958\", ",
                "oauth_token=\"370773112-GmHxMAgYyLbNEtIKZeRNFsMKPR9EyMZeS9weJAEb\", ",
                "oauth_version=\"1.0\""
            )
        );
    }

    #[test]
    fn signature_base_string_matches_rfc5849_vector() {
        // RFC 5849 §3.4.1.1, with the POST signature from verified erratum 2550
        // (the signature printed in the RFC was computed as if the method were GET).
        let params = vec![
            ("b5".to_string(), "=%3D".to_string()),
            ("a3".to_string(), "a".to_string()),
            ("c@".to_string(), String::new()),
            ("a2".to_string(), "r b".to_string()),
            (
                "oauth_consumer_key".to_string(),
                "9djdj82h48djs9d2".to_string(),
            ),
            ("oauth_token".to_string(), "kkk9d7dh3k39sjv7".to_string()),
            (
                "oauth_signature_method".to_string(),
                "HMAC-SHA1".to_string(),
            ),
            ("oauth_timestamp".to_string(), "137131201".to_string()),
            ("oauth_nonce".to_string(), "7d8f3e4a".to_string()),
            ("c2".to_string(), String::new()),
            ("a3".to_string(), "2 q".to_string()),
        ];
        let base = signature_base_string("POST", "http://example.com/request", &params);

        assert_eq!(
            base,
            concat!(
                "POST&http%3A%2F%2Fexample.com%2Frequest&a2%3Dr%2520b%26a3%3D2%2520q",
                "%26a3%3Da%26b5%3D%253D%25253D%26c%2540%3D%26c2%3D%26oauth_consumer_",
                "key%3D9djdj82h48djs9d2%26oauth_nonce%3D7d8f3e4a%26oauth_signature_m",
                "ethod%3DHMAC-SHA1%26oauth_timestamp%3D137131201%26oauth_token%3Dkkk",
                "9d7dh3k39sjv7"
            )
        );
        assert_eq!(
            hmac_sha1_base64("j49sk3j29djd&dh893hdasih9", &base).expect("HMAC-SHA1 signs"),
            "r6/TJjbCOr97/+UU0NsvSne7s5g="
        );
    }

    #[test]
    fn build_tweet_body_carries_text() {
        assert_eq!(
            build_tweet_body("olá, ciência"),
            serde_json::json!({ "text": "olá, ciência" })
        );
    }

    #[test]
    fn parse_create_response_reads_id_and_url() {
        let post = parse_create_response(
            r#"{"data":{"id":"1580000000000000000","text":"O cérebro gasta 20%."}}"#,
        )
        .expect("fixture parses");

        assert_eq!(post.id, "1580000000000000000");
        assert_eq!(post.url, "https://x.com/i/web/status/1580000000000000000");
    }

    #[test]
    fn parse_create_response_reports_api_error() {
        let err = parse_create_response(
            r#"{"errors":[{"title":"Unauthorized","detail":"Unauthorized"}]}"#,
        )
        .expect_err("an error payload is rejected");
        assert!(
            err.to_string().contains("Unauthorized"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn parse_create_response_handles_flat_error() {
        let err = parse_create_response(r#"{"title":"Forbidden","detail":"Forbidden"}"#)
            .expect_err("a flat error payload is rejected");
        assert!(err.to_string().contains("Forbidden"), "unexpected: {err}");
    }

    #[test]
    fn post_endpoint_trims_trailing_slash() {
        assert_eq!(
            post_endpoint("https://api.x.com"),
            "https://api.x.com/2/tweets"
        );
        assert_eq!(
            post_endpoint("https://api.x.com/"),
            "https://api.x.com/2/tweets"
        );
    }
}
