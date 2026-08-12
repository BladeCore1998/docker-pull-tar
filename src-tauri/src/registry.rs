use crate::domain::{Descriptor, ImageReference, Platform, ResolvedImage};
use crate::error::AppError;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, WWW_AUTHENTICATE};
use reqwest::{Client, Method, Response, StatusCode};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    error::Error,
    fmt,
    time::{Duration, SystemTime},
};
use url::Url;

const ACCEPTED_MANIFESTS: &str = concat!(
    "application/vnd.docker.distribution.manifest.v2+json, ",
    "application/vnd.docker.distribution.manifest.list.v2+json, ",
    "application/vnd.oci.image.manifest.v1+json, ",
    "application/vnd.oci.image.index.v1+json"
);
const MAX_RETRIES: usize = 2;
const MAX_BACKOFF: Duration = Duration::from_secs(5);
const MAX_RETRY_AFTER: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BearerChallenge {
    pub realm: Url,
    pub service: Option<String>,
    pub scope: Option<String>,
}

#[derive(Clone)]
pub struct RegistryClient {
    client: Client,
    #[cfg(feature = "test-utils")]
    allow_loopback_basic: bool,
}

#[derive(Clone)]
pub struct RegistrySession {
    pub image: ResolvedImage,
    blob_authorization: Option<String>,
}

impl RegistrySession {
    #[doc(hidden)]
    pub fn from_parts(image: ResolvedImage, blob_authorization: Option<String>) -> Self {
        Self {
            image,
            blob_authorization,
        }
    }

    pub fn blob_authorization(&self) -> Option<&str> {
        self.blob_authorization.as_deref()
    }
}

impl fmt::Debug for RegistrySession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RegistrySession")
            .field("image", &self.image)
            .field(
                "blob_authorization",
                &self.blob_authorization.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

impl RegistryClient {
    pub fn new() -> Result<Self, AppError> {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(60))
            .https_only(false)
            .build()
            .map_err(map_request_error)?;
        Ok(Self {
            client,
            #[cfg(feature = "test-utils")]
            allow_loopback_basic: false,
        })
    }

    #[doc(hidden)]
    #[cfg(feature = "test-utils")]
    pub fn new_for_loopback_basic_test() -> Result<Self, AppError> {
        let mut client = Self::new()?;
        client.allow_loopback_basic = true;
        Ok(client)
    }

    pub async fn resolve(
        &self,
        image: &ImageReference,
        platform: Platform,
        credentials: Option<(&str, &str)>,
    ) -> Result<ResolvedImage, AppError> {
        self.resolve_session(image, platform, credentials)
            .await
            .map(|session| session.image)
    }

    pub async fn resolve_session(
        &self,
        image: &ImageReference,
        platform: Platform,
        credentials: Option<(&str, &str)>,
    ) -> Result<RegistrySession, AppError> {
        let mut reference = image.reference.clone();
        let (first, first_authorization) =
            self.fetch_manifest(image, &reference, credentials).await?;
        let (first_headers, first_body) = response_parts(first).await?;
        let first_digest = verify_digest(&first_headers, &reference, &first_body)?;
        let first_type = media_type(&first_headers, &first_body)?;

        let (manifest_digest, manifest, blob_authorization) = if is_index(&first_type) {
            let index: ManifestIndex = serde_json::from_slice(&first_body)
                .map_err(|_| AppError::InternalError("invalid registry index".into()))?;
            let selected = select_manifest(&index.manifests, &platform)?;
            reference = selected.digest.clone();
            let (response, authorization) =
                self.fetch_manifest(image, &reference, credentials).await?;
            let (headers, body) = response_parts(response).await?;
            let digest = verify_digest(&headers, &reference, &body)?;
            let content_type = media_type(&headers, &body)?;
            if !is_manifest(&content_type) {
                return Err(AppError::InternalError(
                    "registry returned an unsupported manifest type".into(),
                ));
            }
            (digest, parse_manifest(&body)?, authorization)
        } else if is_manifest(&first_type) {
            (
                first_digest,
                parse_manifest(&first_body)?,
                first_authorization,
            )
        } else {
            return Err(AppError::InternalError(
                "registry returned an unsupported manifest type".into(),
            ));
        };

        Ok(RegistrySession {
            image: ResolvedImage {
                registry: image.registry.clone(),
                repository: image.repository.clone(),
                reference: image.reference.clone(),
                manifest_digest,
                config: manifest.config.into(),
                layers: manifest.layers.into_iter().map(Into::into).collect(),
                platform,
            },
            blob_authorization,
        })
    }

    async fn fetch_manifest(
        &self,
        image: &ImageReference,
        reference: &str,
        credentials: Option<(&str, &str)>,
    ) -> Result<(Response, Option<String>), AppError> {
        let url = manifest_url(image, reference)?;
        let response = self
            .send_with_retries(Method::GET, url.clone(), None, true)
            .await?;
        if response.status() != StatusCode::UNAUTHORIZED {
            return map_status(response).map(|response| (response, None));
        }
        let challenge = response
            .headers()
            .get(WWW_AUTHENTICATE)
            .and_then(|value| value.to_str().ok())
            .ok_or(AppError::RegistryAuthFailed)
            .and_then(parse_bearer_challenge)?;
        let registry_url = registry_base_url(image)?;
        let token = self
            .fetch_token(&registry_url, challenge, credentials)
            .await?;
        let authorization = format!("Bearer {token}");
        let response = map_status(
            self.send_with_retries(Method::GET, url, Some(&authorization), true)
                .await?,
        )?;
        Ok((response, Some(authorization)))
    }

    async fn fetch_token(
        &self,
        registry_url: &Url,
        challenge: BearerChallenge,
        credentials: Option<(&str, &str)>,
    ) -> Result<String, AppError> {
        let mut url = challenge.realm;
        #[cfg(feature = "test-utils")]
        let allow_loopback_basic = self.allow_loopback_basic;
        #[cfg(not(feature = "test-utils"))]
        let allow_loopback_basic = false;
        validate_token_realm_with_policy(
            registry_url,
            &url,
            credentials.is_some(),
            allow_loopback_basic,
        )?;
        {
            let mut query = url.query_pairs_mut();
            if let Some(service) = challenge.service.as_deref() {
                query.append_pair("service", service);
            }
            if let Some(scope) = challenge.scope.as_deref() {
                query.append_pair("scope", scope);
            }
        }
        let authorization =
            credentials.map(|(username, password)| basic_authorization(username, password));
        let response = self
            .send_with_retries(Method::GET, url, authorization.as_deref(), false)
            .await?;
        if response.status() == StatusCode::UNAUTHORIZED
            || response.status() == StatusCode::FORBIDDEN
        {
            return Err(AppError::RegistryAuthFailed);
        }
        if !response.status().is_success() {
            return Err(AppError::RegistryAuthFailed);
        }
        let body: TokenResponse = response
            .json()
            .await
            .map_err(|_| AppError::RegistryAuthFailed)?;
        body.token
            .or(body.access_token)
            .filter(|token| !token.is_empty())
            .ok_or(AppError::RegistryAuthFailed)
    }

    async fn send_with_retries(
        &self,
        method: Method,
        url: Url,
        authorization: Option<&str>,
        accept_manifests: bool,
    ) -> Result<Response, AppError> {
        if method != Method::GET && method != Method::HEAD {
            return Err(AppError::InternalError(
                "non-idempotent registry retry rejected".into(),
            ));
        }
        for attempt in 0..=MAX_RETRIES {
            let mut request = self.client.request(method.clone(), url.clone());
            if accept_manifests {
                request = request.header(ACCEPT, ACCEPTED_MANIFESTS);
            }
            if let Some(value) = authorization {
                request = request.header(AUTHORIZATION, value);
            }
            match request.send().await {
                Ok(response) => {
                    if attempt == MAX_RETRIES
                        || !(response.status() == StatusCode::TOO_MANY_REQUESTS
                            || response.status().is_server_error())
                    {
                        return Ok(response);
                    }
                    tokio::time::sleep(retry_delay(response.headers(), attempt)).await;
                }
                Err(error) => {
                    if attempt == MAX_RETRIES
                        || !is_transient_transport(
                            error.is_timeout(),
                            error.is_connect(),
                            is_tls_certificate_error(&error),
                        )
                    {
                        return Err(map_request_error(error));
                    }
                    tokio::time::sleep(backoff_delay(attempt)).await;
                }
            }
        }
        unreachable!()
    }

    pub async fn head(&self, url: Url) -> Result<StatusCode, AppError> {
        let response = self
            .send_with_retries(Method::HEAD, url, None, false)
            .await?;
        map_status(response).map(|response| response.status())
    }
}

pub fn blob_url(image: &ImageReference, digest: &str) -> Result<Url, AppError> {
    let mut url = registry_base_url(image)?;
    url.path_segments_mut()
        .map_err(|_| AppError::InternalError("invalid registry endpoint".into()))?
        .extend(["v2"])
        .extend(image.repository.split('/'))
        .extend(["blobs", digest]);
    Ok(url)
}

fn backoff_delay(attempt: usize) -> Duration {
    let multiplier = 1_u64
        .checked_shl(attempt.min(16) as u32)
        .unwrap_or(u64::MAX);
    Duration::from_millis(100_u64.saturating_mul(multiplier)).min(MAX_BACKOFF)
}

fn retry_delay(headers: &HeaderMap, attempt: usize) -> Duration {
    retry_delay_at(headers, attempt, SystemTime::now())
}

fn retry_delay_at(headers: &HeaderMap, attempt: usize, now: SystemTime) -> Duration {
    let Some(value) = headers
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
    else {
        return backoff_delay(attempt);
    };
    if let Ok(seconds) = value.parse::<u64>() {
        return Duration::from_secs(seconds).min(MAX_RETRY_AFTER);
    }
    httpdate::parse_http_date(value)
        .map(|date| {
            date.duration_since(now)
                .unwrap_or(Duration::ZERO)
                .min(MAX_RETRY_AFTER)
        })
        .unwrap_or_else(|_| backoff_delay(attempt))
}

fn is_transient_transport(is_timeout: bool, is_connect: bool, is_tls: bool) -> bool {
    !is_tls && (is_timeout || is_connect)
}

fn basic_authorization(username: &str, password: &str) -> String {
    format!(
        "Basic {}",
        STANDARD.encode(format!("{username}:{password}"))
    )
}

pub fn validate_token_realm(
    registry: &Url,
    realm: &Url,
    has_credentials: bool,
) -> Result<(), AppError> {
    validate_token_realm_with_policy(registry, realm, has_credentials, false)
}

fn validate_token_realm_with_policy(
    registry: &Url,
    realm: &Url,
    has_credentials: bool,
    allow_loopback_basic: bool,
) -> Result<(), AppError> {
    if !realm.username().is_empty() || realm.password().is_some() {
        return Err(AppError::RegistryAuthFailed);
    }
    let same_endpoint = registry.host_str() == realm.host_str()
        && registry.port_or_known_default() == realm.port_or_known_default();
    let docker_hub_realm = matches!(
        registry.host_str(),
        Some("registry-1.docker.io" | "docker.io")
    ) && registry.scheme() == "https"
        && registry.port_or_known_default() == Some(443)
        && registry.username().is_empty()
        && registry.password().is_none()
        && realm.host_str() == Some("auth.docker.io")
        && realm.scheme() == "https"
        && realm.port_or_known_default() == Some(443);
    if !same_endpoint && !docker_hub_realm {
        return Err(AppError::RegistryAuthFailed);
    }
    if realm.scheme() == "https" {
        return Ok(());
    }
    let loopback = matches!(realm.host_str(), Some("localhost" | "127.0.0.1" | "::1"));
    #[cfg(feature = "test-utils")]
    let plaintext_allowed = !has_credentials || allow_loopback_basic;
    #[cfg(not(feature = "test-utils"))]
    let plaintext_allowed = {
        let _ = allow_loopback_basic;
        !has_credentials
    };
    if realm.scheme() == "http" && loopback && plaintext_allowed {
        Ok(())
    } else {
        Err(AppError::RegistryAuthFailed)
    }
}

pub fn parse_bearer_challenge(value: &str) -> Result<BearerChallenge, AppError> {
    let (scheme, parameters) = value
        .trim()
        .split_once(char::is_whitespace)
        .ok_or(AppError::RegistryAuthFailed)?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return Err(AppError::RegistryAuthFailed);
    }
    let mut realm = None;
    let mut service = None;
    let mut scope = None;
    for item in split_parameters(parameters)? {
        let (key, raw) = item.split_once('=').ok_or(AppError::RegistryAuthFailed)?;
        let parsed = unquote(raw.trim())?;
        match key.trim().to_ascii_lowercase().as_str() {
            "realm" => realm = Some(parsed),
            "service" => service = Some(parsed),
            "scope" => scope = Some(parsed),
            _ => {}
        }
    }
    let realm = Url::parse(realm.as_deref().ok_or(AppError::RegistryAuthFailed)?)
        .map_err(|_| AppError::RegistryAuthFailed)?;
    if !matches!(realm.scheme(), "http" | "https") {
        return Err(AppError::RegistryAuthFailed);
    }
    Ok(BearerChallenge {
        realm,
        service,
        scope,
    })
}

fn split_parameters(value: &str) -> Result<Vec<&str>, AppError> {
    let mut parts = Vec::new();
    let mut quoted = false;
    let mut escaped = false;
    let mut start = 0;
    for (index, character) in value.char_indices() {
        if escaped {
            escaped = false;
        } else if character == '\\' && quoted {
            escaped = true;
        } else if character == '"' {
            quoted = !quoted;
        } else if character == ',' && !quoted {
            parts.push(value[start..index].trim());
            start = index + 1;
        }
    }
    if quoted || escaped {
        return Err(AppError::RegistryAuthFailed);
    }
    parts.push(value[start..].trim());
    Ok(parts)
}

fn unquote(value: &str) -> Result<String, AppError> {
    if !value.starts_with('"') || !value.ends_with('"') || value.len() < 2 {
        return Ok(value.to_owned());
    }
    let mut result = String::new();
    let mut escaped = false;
    for character in value[1..value.len() - 1].chars() {
        if escaped {
            result.push(character);
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else {
            result.push(character);
        }
    }
    if escaped {
        return Err(AppError::RegistryAuthFailed);
    }
    Ok(result)
}

fn manifest_url(image: &ImageReference, reference: &str) -> Result<Url, AppError> {
    let mut url = registry_base_url(image)?;
    url.path_segments_mut()
        .map_err(|_| AppError::InternalError("invalid registry endpoint".into()))?
        .extend(["v2"])
        .extend(image.repository.split('/'))
        .extend(["manifests", reference]);
    Ok(url)
}

fn registry_base_url(image: &ImageReference) -> Result<Url, AppError> {
    let scheme = if image.registry.starts_with("localhost:")
        || image.registry.starts_with("127.0.0.1:")
        || image.registry.starts_with("[::1]:")
    {
        "http"
    } else {
        "https"
    };
    Url::parse(&format!("{scheme}://{}/", image.registry))
        .map_err(|_| AppError::InternalError("invalid registry endpoint".into()))
}

async fn response_parts(response: Response) -> Result<(HeaderMap, Vec<u8>), AppError> {
    let headers = response.headers().clone();
    let body = response.bytes().await.map_err(map_request_error)?.to_vec();
    Ok((headers, body))
}

fn map_status(response: Response) -> Result<Response, AppError> {
    match response.status() {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(AppError::RegistryAuthFailed),
        StatusCode::NOT_FOUND => Err(AppError::RegistryNotFound),
        status if status.is_success() => Ok(response),
        _ => Err(AppError::InternalError("registry request failed".into())),
    }
}

fn map_request_error(error: reqwest::Error) -> AppError {
    if error.is_timeout() {
        AppError::NetworkTimeout
    } else if is_tls_certificate_error(&error) {
        AppError::TlsValidationFailed
    } else {
        AppError::InternalError("registry network request failed".into())
    }
}

fn is_tls_certificate_error(error: &(dyn Error + 'static)) -> bool {
    let mut current = Some(error);
    while let Some(cause) = current {
        let message = cause.to_string().to_ascii_lowercase();
        if [
            "certificate",
            "cert error",
            "unknown issuer",
            "invalid peer certificate",
            "self signed",
        ]
        .iter()
        .any(|needle| message.contains(needle))
        {
            return true;
        }
        current = cause.source();
    }
    false
}

fn media_type(headers: &HeaderMap, body: &[u8]) -> Result<String, AppError> {
    if let Some(value) = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
    {
        return Ok(value
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_owned());
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct MediaType {
        media_type: String,
    }
    serde_json::from_slice::<MediaType>(body)
        .map(|value| value.media_type)
        .map_err(|_| AppError::InternalError("registry response has no media type".into()))
}

fn verify_digest(headers: &HeaderMap, requested: &str, body: &[u8]) -> Result<String, AppError> {
    let expected = if let Some(digest) = headers
        .get("docker-content-digest")
        .and_then(|value| value.to_str().ok())
    {
        if !valid_digest(digest) || (valid_digest(requested) && requested != digest) {
            return Err(AppError::DigestMismatch);
        }
        digest.to_owned()
    } else {
        valid_digest(requested)
            .then(|| requested.to_owned())
            .ok_or(AppError::DigestMismatch)?
    };
    let actual = format!("sha256:{:x}", Sha256::digest(body));
    if actual == expected {
        Ok(expected)
    } else {
        Err(AppError::DigestMismatch)
    }
}

fn valid_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|payload| {
        payload.len() == 64 && payload.bytes().all(|byte| byte.is_ascii_hexdigit())
    })
}

fn is_index(media_type: &str) -> bool {
    matches!(
        media_type,
        "application/vnd.docker.distribution.manifest.list.v2+json"
            | "application/vnd.oci.image.index.v1+json"
    )
}

fn is_manifest(media_type: &str) -> bool {
    matches!(
        media_type,
        "application/vnd.docker.distribution.manifest.v2+json"
            | "application/vnd.oci.image.manifest.v1+json"
    )
}

fn select_manifest<'a>(
    manifests: &'a [IndexDescriptor],
    requested: &Platform,
) -> Result<&'a IndexDescriptor, AppError> {
    manifests
        .iter()
        .find(|manifest| {
            manifest.platform.as_ref().is_some_and(|candidate| {
                candidate.os == requested.os
                    && candidate.architecture == requested.architecture
                    && candidate.variant == requested.variant
            })
        })
        .ok_or(AppError::PlatformNotFound)
}

fn parse_manifest(body: &[u8]) -> Result<ImageManifest, AppError> {
    serde_json::from_slice(body)
        .map_err(|_| AppError::InternalError("invalid registry manifest".into()))
}

#[derive(Deserialize)]
struct TokenResponse {
    token: Option<String>,
    access_token: Option<String>,
}

#[derive(Deserialize)]
struct ManifestIndex {
    manifests: Vec<IndexDescriptor>,
}

#[derive(Deserialize)]
struct IndexDescriptor {
    digest: String,
    platform: Option<Platform>,
}

#[derive(Deserialize)]
struct ImageManifest {
    config: WireDescriptor,
    layers: Vec<WireDescriptor>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireDescriptor {
    media_type: String,
    digest: String,
    size: u64,
}

impl From<WireDescriptor> for Descriptor {
    fn from(value: WireDescriptor) -> Self {
        Self {
            media_type: value.media_type,
            digest: value.digest,
            size: value.size,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt;

    #[derive(Debug)]
    struct NestedError {
        message: &'static str,
        source: Option<Box<NestedError>>,
    }

    impl fmt::Display for NestedError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(self.message)
        }
    }

    impl Error for NestedError {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            self.source.as_deref().map(|value| value as _)
        }
    }

    #[test]
    fn selects_each_supported_linux_platform() {
        let index: ManifestIndex = serde_json::from_value(serde_json::json!({"manifests":[
            {"digest":"amd", "platform":{"os":"linux","architecture":"amd64"}},
            {"digest":"arm64", "platform":{"os":"linux","architecture":"arm64"}},
            {"digest":"arm7", "platform":{"os":"linux","architecture":"arm","variant":"v7"}}
        ]}))
        .unwrap();
        for (architecture, variant, digest) in [
            ("amd64", None, "amd"),
            ("arm64", None, "arm64"),
            ("arm", Some("v7"), "arm7"),
        ] {
            let selected = select_manifest(
                &index.manifests,
                &Platform {
                    os: "linux".into(),
                    architecture: architecture.into(),
                    variant: variant.map(str::to_owned),
                },
            )
            .unwrap();
            assert_eq!(selected.digest, digest);
        }
    }

    #[test]
    fn rejects_missing_platform() {
        let index = ManifestIndex { manifests: vec![] };
        assert!(matches!(
            select_manifest(
                &index.manifests,
                &Platform {
                    os: "linux".into(),
                    architecture: "amd64".into(),
                    variant: None
                }
            ),
            Err(AppError::PlatformNotFound)
        ));
    }

    #[test]
    fn client_configuration_never_disables_certificate_validation() {
        let source = include_str!("registry.rs");
        assert!(!source.contains(&["danger_accept_invalid_", "certs"].concat()));
        assert!(!source.contains(&["danger_accept_invalid_", "hostnames"].concat()));
    }

    #[test]
    fn tls_certificate_classifier_walks_the_error_source_chain() {
        let error = NestedError {
            message: "request failed",
            source: Some(Box::new(NestedError {
                message: "invalid peer certificate: unknown issuer",
                source: None,
            })),
        };
        assert!(is_tls_certificate_error(&error));
    }

    #[test]
    fn basic_authorization_is_constructed_without_entering_errors() {
        assert_eq!(
            basic_authorization("alice", "secret"),
            "Basic YWxpY2U6c2VjcmV0"
        );
        assert!(!AppError::RegistryAuthFailed.to_string().contains("secret"));
    }

    #[test]
    fn exponential_retry_delay_is_bounded_and_deterministic() {
        assert_eq!(backoff_delay(0), Duration::from_millis(100));
        assert_eq!(backoff_delay(1), Duration::from_millis(200));
        assert_eq!(backoff_delay(8), Duration::from_secs(5));
    }

    #[test]
    fn numeric_retry_after_is_honored_and_capped() {
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", "7".parse().unwrap());
        assert_eq!(retry_delay(&headers, 0), Duration::from_secs(7));
        headers.insert("retry-after", "90".parse().unwrap());
        assert_eq!(retry_delay(&headers, 0), Duration::from_secs(30));
        headers.insert("retry-after", "not-a-number".parse().unwrap());
        assert_eq!(retry_delay(&headers, 1), Duration::from_millis(200));
    }

    #[test]
    fn future_http_date_retry_after_is_bounded() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut headers = HeaderMap::new();
        headers.insert(
            "retry-after",
            httpdate::fmt_http_date(now + Duration::from_secs(90))
                .parse()
                .unwrap(),
        );
        assert_eq!(retry_delay_at(&headers, 0, now), Duration::from_secs(30));
    }

    #[test]
    fn past_http_date_retry_after_is_zero() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut headers = HeaderMap::new();
        headers.insert(
            "retry-after",
            httpdate::fmt_http_date(now - Duration::from_secs(1))
                .parse()
                .unwrap(),
        );
        assert_eq!(retry_delay_at(&headers, 2, now), Duration::ZERO);
    }

    #[test]
    fn malformed_http_date_falls_back_to_exponential_delay() {
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", "Tuesday eventually".parse().unwrap());
        assert_eq!(
            retry_delay_at(&headers, 2, SystemTime::UNIX_EPOCH),
            Duration::from_millis(400)
        );
    }

    #[test]
    fn transport_retry_classification_is_transient_only() {
        assert!(is_transient_transport(true, false, false));
        assert!(is_transient_transport(false, true, false));
        assert!(!is_transient_transport(false, false, false));
        assert!(!is_transient_transport(false, true, true));
    }
}
