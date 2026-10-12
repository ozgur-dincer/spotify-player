use librespot_core::session::Session;
use maybe_async::maybe_async;
use rspotify::{
    clients::{BaseClient, OAuthClient},
    http::{HttpClient, HttpError, Query},
    sync::Mutex,
    AuthCodePkceSpotify, ClientError, ClientResult, Config, Credentials, OAuth, Token,
};
use serde_json::Value;
use std::{collections::HashSet, fmt, sync::Arc};

use crate::token;

#[derive(Clone, Default)]
/// A custom Spotify client to interact with the official Spotify API server
pub struct Spotify {
    creds: Credentials,
    oauth: OAuth,
    config: Config,
    token: Arc<Mutex<Option<Token>>>,
    http: HttpClient,
    session: Arc<tokio::sync::Mutex<Option<Session>>>,
}

#[allow(clippy::missing_fields_in_debug)] // Seems like not all fields are necessary in debug
impl fmt::Debug for Spotify {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Spotify")
            .field("creds", &self.creds)
            .field("oauth", &self.oauth)
            .field("config", &self.config)
            .field("token", &self.token)
            .finish()
    }
}

impl Spotify {
    /// Create a new Spotify client
    pub fn new() -> Spotify {
        Self {
            creds: Credentials::default(),
            oauth: OAuth::default(),
            config: Config {
                token_refreshing: true,
                ..Default::default()
            },
            token: Arc::new(Mutex::new(None)),
            http: HttpClient::default(),
            session: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }

    pub async fn set_session(&self, session: Session) {
        *self.session.lock().await = Some(session);
    }

    pub async fn session(&self) -> Session {
        self.session
            .lock()
            .await
            .clone()
            .expect("non-empty Spotify session")
    }
}

// TODO: remove the below uses of `maybe_async` crate once
// async trait is fully supported in stable Rust.

#[maybe_async]
impl BaseClient for Spotify {
    fn get_http(&self) -> &HttpClient {
        &self.http
    }

    fn get_token(&self) -> Arc<Mutex<Option<Token>>> {
        Arc::clone(&self.token)
    }

    fn get_creds(&self) -> &Credentials {
        &self.creds
    }

    fn get_config(&self) -> &Config {
        &self.config
    }

    async fn refetch_token(&self) -> ClientResult<Option<Token>> {
        let session = self.session().await;
        let old_token = self.token.lock().await.unwrap().clone();

        if session.is_invalid() {
            tracing::error!("Failed to get a new token: invalid session");
            return Ok(old_token);
        }

        match token::get_token_rspotify(&session).await {
            Ok(token) => Ok(Some(token)),
            Err(err) => {
                tracing::error!("Failed to get a new token: {err:#}");
                Ok(old_token)
            }
        }
    }
}

/// Implement `OAuthClient` trait for `Spotify` struct
/// to allow calling methods that get/modify user's data such as
/// `current_user_playlists`, `playlist_add_items`, etc.
///
/// Because the `Spotify` client interacts with Spotify APIs
/// using an access token that is manually retrieved by
/// the `librespot::get_token` function, implementing
/// `OAuthClient::get_oauth` and `OAuthClient::request_token` is unnecessary
#[maybe_async]
impl OAuthClient for Spotify {
    fn get_oauth(&self) -> &OAuth {
        panic!("`OAuthClient::get_oauth` should never be called!")
    }

    async fn request_token(&self, _code: &str) -> ClientResult<()> {
        panic!("`OAuthClient::request_token` should never be called!")
    }
}

/// Thin wrapper over [`rspotify::AuthCodePkceSpotify`] for the Spotify Web API.
///
/// It exists solely to override `refetch_token` so that a refresh grant which
/// omits `refresh_token` preserves the previously stored refresh token instead
/// of overwriting it with `None`. Spotify stopped rotating (and now expires)
/// refresh tokens, so the refresh response no longer echoes one back.
///
/// See:
/// - <https://developer.spotify.com/blog/2026-06-18-refresh-token-expiration>
/// - <https://github.com/aome510/spotify-player/issues/1040>
#[derive(Clone, Debug, Default)]
pub(crate) struct PkceWebApiClient(AuthCodePkceSpotify);

impl PkceWebApiClient {
    pub(crate) fn new(inner: AuthCodePkceSpotify) -> Self {
        Self(inner)
    }

    pub(crate) fn get_authorize_url(
        &mut self,
        verifier_bytes: Option<usize>,
    ) -> ClientResult<String> {
        self.0.get_authorize_url(verifier_bytes)
    }
}

#[maybe_async]
impl BaseClient for PkceWebApiClient {
    fn get_http(&self) -> &HttpClient {
        self.0.get_http()
    }

    fn get_token(&self) -> Arc<Mutex<Option<Token>>> {
        self.0.get_token()
    }

    fn get_creds(&self) -> &Credentials {
        self.0.get_creds()
    }

    fn get_config(&self) -> &Config {
        self.0.get_config()
    }

    async fn refetch_token(&self) -> ClientResult<Option<Token>> {
        // Capture the current refresh token before refreshing
        let previous_refresh_token = self
            .0
            .get_token()
            .lock()
            .await
            .unwrap()
            .as_ref()
            .and_then(|token| token.refresh_token.clone());

        // Spotify's refresh response no longer includes `refresh_token`; carry the
        // previous one forward so it is not lost on the round-trip and nulled in the
        // token cache.
        let refreshed = self.0.refetch_token().await?;
        Ok(refreshed.map(|mut token| {
            if token.refresh_token.is_none() {
                token.refresh_token = previous_refresh_token;
            }
            token
        }))
    }
}

#[maybe_async]
impl OAuthClient for PkceWebApiClient {
    fn get_oauth(&self) -> &OAuth {
        self.0.get_oauth()
    }

    async fn request_token(&self, code: &str) -> ClientResult<()> {
        self.0.request_token(code).await
    }
}

/// Spotify Web API client with an optional fallback identity.
#[derive(Clone, Debug)]
pub struct WebApiClient {
    primary: PkceWebApiClient,
    fallback: Option<PkceWebApiClient>,
    ncspot_only_get_endpoints: Vec<String>,
    /// Playlists whose contents the primary client cannot read; their GETs go straight to ncspot.
    restricted_playlists: Arc<std::sync::Mutex<HashSet<String>>>,
}

impl Default for WebApiClient {
    fn default() -> Self {
        Self::new(AuthCodePkceSpotify::default(), None)
    }
}

impl WebApiClient {
    pub fn new(primary: AuthCodePkceSpotify, fallback: Option<AuthCodePkceSpotify>) -> Self {
        Self {
            primary: PkceWebApiClient::new(primary),
            fallback: fallback.map(PkceWebApiClient::new),
            ncspot_only_get_endpoints: crate::config::DEFAULT_NCSPOT_ONLY_GET_ENDPOINTS
                .iter()
                .map(ToString::to_string)
                .collect(),
            restricted_playlists: Arc::default(),
        }
    }

    pub fn with_ncspot_only_get_endpoints(
        mut self,
        ncspot_only_get_endpoints: Vec<String>,
    ) -> Self {
        self.ncspot_only_get_endpoints = ncspot_only_get_endpoints;
        self
    }

    pub(crate) fn primary_mut(&mut self) -> &mut PkceWebApiClient {
        &mut self.primary
    }

    pub(crate) fn fallback_mut(&mut self) -> Option<&mut PkceWebApiClient> {
        self.fallback.as_mut()
    }

    fn ncspot(&self) -> &PkceWebApiClient {
        self.fallback.as_ref().unwrap_or(&self.primary)
    }

    fn is_ncspot_only_get(&self, url: &str) -> bool {
        self.ncspot_only_get_endpoints
            .iter()
            .any(|endpoint| url.starts_with(endpoint))
    }

    fn is_restricted_playlist_get(&self, url: &str) -> bool {
        let Some(id) = playlist_id_from_url(url) else {
            return false;
        };
        self.fallback.is_some()
            && self
                .restricted_playlists
                .lock()
                .is_ok_and(|playlists| playlists.contains(id))
    }

    fn mark_restricted_playlist(&self, url: &str) {
        let Some(id) = playlist_id_from_url(url) else {
            return;
        };
        if let Ok(mut playlists) = self.restricted_playlists.lock() {
            if playlists.insert(id.to_string()) {
                tracing::info!(
                    playlist_id = id,
                    "Routing restricted playlist reads to ncspot"
                );
            }
        }
    }

    fn routes_to_ncspot(&self, url: &str) -> bool {
        self.is_ncspot_only_get(url) || self.is_restricted_playlist_get(url)
    }

    fn fallback_status(error: &ClientError) -> Option<reqwest::StatusCode> {
        match error {
            ClientError::Http(error) => match error.as_ref() {
                HttpError::StatusCode(response) if response.status().is_client_error() => {
                    Some(response.status())
                }
                HttpError::Client(_) | HttpError::StatusCode(_) => None,
            },
            _ => None,
        }
    }

    fn log_fallback(method: &str, url: &str, status: reqwest::StatusCode) {
        tracing::warn!(
            method,
            url,
            %status,
            "Custom Spotify Web API request failed; retrying with ncspot client"
        );
    }

    async fn get_with_fallback(&self, url: &str, payload: &Query<'_>) -> ClientResult<String> {
        if self.routes_to_ncspot(url) {
            return self.ncspot().api_get(url, payload).await;
        }
        match self.primary.api_get(url, payload).await {
            Err(error) => match (Self::fallback_status(&error), &self.fallback) {
                (Some(status), Some(fallback)) => {
                    Self::log_fallback("GET", url, status);
                    let result = fallback.api_get(url, payload).await;
                    if status == reqwest::StatusCode::FORBIDDEN && result.is_ok() {
                        self.mark_restricted_playlist(url);
                    }
                    result
                }
                _ => Err(error),
            },
            result => result,
        }
    }

    pub(crate) async fn playlist_metadata(
        &self,
        id: &str,
    ) -> anyhow::Result<rspotify::model::FullPlaylist> {
        let url = format!("playlists/{id}");
        let params = Query::from([("market", "from_token")]);
        let text = self.api_get(&url, &params).await?;
        let mut response: Value = serde_json::from_str(&text)?;
        if !has_playlist_items(&response) {
            if !self.routes_to_ncspot(&url) {
                if let Some(fallback) = &self.fallback {
                    tracing::warn!(%url, "Custom client returned restricted playlist metadata without items/tracks; retrying with ncspot");
                    self.mark_restricted_playlist(&url);
                    let text = fallback.api_get(&url, &params).await?;
                    response = serde_json::from_str(&text)?;
                }
            }
            anyhow::ensure!(
                has_playlist_items(&response),
                "Spotify withheld playlist contents (missing items/tracks); the client may not have access to this subscribed playlist"
            );
        }
        Ok(serde_json::from_value(response)?)
    }
}

fn has_playlist_items(response: &Value) -> bool {
    response.get("items").is_some_and(Value::is_object)
        || response.get("tracks").is_some_and(Value::is_object)
}

/// Extracts the playlist ID from `playlists/{id}` and `playlists/{id}/...` URLs.
fn playlist_id_from_url(url: &str) -> Option<&str> {
    let id = url.strip_prefix("playlists/")?.split('/').next()?;
    (!id.is_empty()).then_some(id)
}

#[maybe_async]
impl BaseClient for WebApiClient {
    fn get_http(&self) -> &HttpClient {
        self.primary.get_http()
    }

    fn get_token(&self) -> Arc<Mutex<Option<Token>>> {
        self.primary.get_token()
    }

    fn get_creds(&self) -> &Credentials {
        self.primary.get_creds()
    }

    fn get_config(&self) -> &Config {
        self.primary.get_config()
    }

    async fn refetch_token(&self) -> ClientResult<Option<Token>> {
        self.primary.refetch_token().await
    }

    async fn api_get(&self, url: &str, payload: &Query<'_>) -> ClientResult<String> {
        let started = std::time::Instant::now();
        tracing::info!(%url, ?payload, ncspot_only = self.routes_to_ncspot(url), "Starting Spotify Web API GET");
        let result = self.get_with_fallback(url, payload).await;
        match &result {
            Ok(_) => {
                tracing::info!(%url, elapsed_ms = started.elapsed().as_millis(), "Completed Spotify Web API GET");
            }
            Err(err) => {
                tracing::error!(%url, elapsed_ms = started.elapsed().as_millis(), "Spotify Web API GET failed: {err:#}");
            }
        }
        result
    }

    async fn api_post(&self, url: &str, payload: &Value) -> ClientResult<String> {
        match self.primary.api_post(url, payload).await {
            Err(error) => match (Self::fallback_status(&error), &self.fallback) {
                (Some(status), Some(fallback)) => {
                    Self::log_fallback("POST", url, status);
                    fallback.api_post(url, payload).await
                }
                _ => Err(error),
            },
            result => result,
        }
    }

    async fn api_put(&self, url: &str, payload: &Value) -> ClientResult<String> {
        match self.primary.api_put(url, payload).await {
            Err(error) => match (Self::fallback_status(&error), &self.fallback) {
                (Some(status), Some(fallback)) => {
                    Self::log_fallback("PUT", url, status);
                    fallback.api_put(url, payload).await
                }
                _ => Err(error),
            },
            result => result,
        }
    }

    async fn api_delete(&self, url: &str, payload: &Value) -> ClientResult<String> {
        match self.primary.api_delete(url, payload).await {
            Err(error) => match (Self::fallback_status(&error), &self.fallback) {
                (Some(status), Some(fallback)) => {
                    Self::log_fallback("DELETE", url, status);
                    fallback.api_delete(url, payload).await
                }
                _ => Err(error),
            },
            result => result,
        }
    }
}

#[maybe_async]
impl OAuthClient for WebApiClient {
    fn get_oauth(&self) -> &OAuth {
        self.primary.get_oauth()
    }

    async fn request_token(&self, code: &str) -> ClientResult<()> {
        self.primary.request_token(code).await
    }
}

#[cfg(test)]
mod tests {
    use super::WebApiClient;
    use crate::client::middleware::SpotifyApiMiddleware;
    use rspotify::{
        clients::BaseClient, http::Query, AuthCodePkceSpotify, Config, Credentials, OAuth, Token,
    };
    use std::{collections::HashSet, sync::Arc};
    use wiremock::{
        matchers::{header, method, path},
        Mock, MockServer, Request, Respond, ResponseTemplate,
    };

    fn playlist_response() -> serde_json::Value {
        serde_json::json!({
            "id": "playlist123", "name": "Playlist", "type": "playlist",
            "uri": "spotify:playlist:playlist123", "href": "https://example.com/playlist",
            "collaborative": false, "public": true, "description": "",
            "snapshot_id": "snapshot", "external_urls": {}, "images": [],
            "followers": {"href": null, "total": 1},
            "owner": {"id": "owner", "display_name": "Owner", "type": "user",
                "uri": "spotify:user:owner", "href": "", "external_urls": {}},
            "items": {"href": "", "items": [], "limit": 50, "next": null,
                "offset": 0, "previous": null, "total": 0}
        })
    }

    #[tokio::test]
    async fn restricted_playlist_metadata_uses_fallback_before_deserialization() {
        let primary_server = MockServer::start().await;
        let fallback_server = MockServer::start().await;
        let mut restricted = playlist_response();
        restricted.as_object_mut().unwrap().remove("items");
        Mock::given(method("GET"))
            .and(path("/v1/playlists/playlist123"))
            .respond_with(ResponseTemplate::new(200).set_body_json(restricted))
            .expect(1)
            .mount(&primary_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/playlists/playlist123"))
            .respond_with(ResponseTemplate::new(200).set_body_json(playlist_response()))
            .expect(1)
            .mount(&fallback_server)
            .await;
        let api = WebApiClient::new(
            client(&primary_server, false).await,
            Some(client(&fallback_server, false).await),
        )
        .with_ncspot_only_get_endpoints(vec![]);
        assert_eq!(
            api.playlist_metadata("playlist123").await.unwrap().name,
            "Playlist"
        );
    }

    #[tokio::test]
    async fn restricted_playlist_without_fallback_returns_error_instead_of_panicking() {
        let server = MockServer::start().await;
        let mut restricted = playlist_response();
        restricted.as_object_mut().unwrap().remove("items");
        Mock::given(method("GET"))
            .and(path("/v1/playlists/playlist123"))
            .respond_with(ResponseTemplate::new(200).set_body_json(restricted))
            .expect(1)
            .mount(&server)
            .await;
        let api = WebApiClient::new(client(&server, false).await, None)
            .with_ncspot_only_get_endpoints(vec![]);
        let err = api.playlist_metadata("playlist123").await.unwrap_err();
        assert!(err.to_string().contains("withheld playlist contents"));
    }

    #[tokio::test]
    async fn accessible_playlist_does_not_use_fallback() {
        let primary_server = MockServer::start().await;
        let fallback_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/playlists/playlist123"))
            .respond_with(ResponseTemplate::new(200).set_body_json(playlist_response()))
            .expect(1)
            .mount(&primary_server)
            .await;
        let api = WebApiClient::new(
            client(&primary_server, false).await,
            Some(client(&fallback_server, false).await),
        )
        .with_ncspot_only_get_endpoints(vec![]);
        assert!(api.playlist_metadata("playlist123").await.is_ok());
        assert!(fallback_server
            .received_requests()
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn restricted_fallback_metadata_also_returns_error() {
        let server = MockServer::start().await;
        let mut restricted = playlist_response();
        restricted["items"] = serde_json::Value::Null;
        Mock::given(method("GET"))
            .and(path("/v1/playlists/playlist123"))
            .respond_with(ResponseTemplate::new(200).set_body_json(restricted))
            .expect(1)
            .mount(&server)
            .await;
        let api = WebApiClient::new(client(&server, false).await, None);
        assert!(api.playlist_metadata("playlist123").await.is_err());
    }

    #[tokio::test]
    async fn restricted_playlist_routes_later_reads_to_ncspot() {
        let primary_server = MockServer::start().await;
        let fallback_server = MockServer::start().await;
        let mut restricted = playlist_response();
        restricted.as_object_mut().unwrap().remove("items");
        Mock::given(method("GET"))
            .and(path("/v1/playlists/playlist123"))
            .respond_with(ResponseTemplate::new(200).set_body_json(restricted))
            .expect(1)
            .mount(&primary_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/playlists/playlist123"))
            .respond_with(ResponseTemplate::new(200).set_body_json(playlist_response()))
            .expect(2)
            .mount(&fallback_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/playlists/playlist123/items"))
            .respond_with(ResponseTemplate::new(200).set_body_string("items"))
            .expect(1)
            .mount(&fallback_server)
            .await;
        let api = WebApiClient::new(
            client(&primary_server, false).await,
            Some(client(&fallback_server, false).await),
        )
        .with_ncspot_only_get_endpoints(vec![]);

        api.playlist_metadata("playlist123").await.unwrap();
        api.playlist_metadata("playlist123").await.unwrap();
        api.api_get("playlists/playlist123/items", &Query::new())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn forbidden_playlist_items_route_later_pages_to_ncspot() {
        let primary_server = MockServer::start().await;
        let fallback_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/playlists/playlist123/items"))
            .respond_with(ResponseTemplate::new(403))
            .expect(1)
            .mount(&primary_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/playlists/playlist123/items"))
            .respond_with(ResponseTemplate::new(200).set_body_string("items"))
            .expect(2)
            .mount(&fallback_server)
            .await;
        let api = WebApiClient::new(
            client(&primary_server, false).await,
            Some(client(&fallback_server, false).await),
        )
        .with_ncspot_only_get_endpoints(vec![]);

        for _ in 0..2 {
            api.api_get("playlists/playlist123/items", &Query::new())
                .await
                .unwrap();
        }
    }

    #[derive(Clone)]
    struct RateLimitThenOk {
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Respond for RateLimitThenOk {
        fn respond(&self, _request: &Request) -> ResponseTemplate {
            if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                ResponseTemplate::new(429).insert_header("retry-after", "0")
            } else {
                ResponseTemplate::new(200).set_body_string("fallback")
            }
        }
    }

    async fn client_with_token(
        server: &MockServer,
        with_middleware: bool,
        access_token: &str,
    ) -> AuthCodePkceSpotify {
        let config = Config {
            api_base_url: format!("{}/v1", server.uri()),
            ..Default::default()
        };
        let client = AuthCodePkceSpotify::with_config(
            Credentials::new_pkce("client-id"),
            OAuth::default(),
            config.clone(),
        );
        let client = if with_middleware {
            client.with_middleware(SpotifyApiMiddleware::new(&config.api_base_url, 1).unwrap())
        } else {
            client
        };
        *client.get_token().lock().await.unwrap() = Some(Token {
            access_token: access_token.to_string(),
            expires_in: chrono::Duration::hours(1),
            expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
            scopes: HashSet::new(),
            refresh_token: Some("refresh-token".to_string()),
        });
        client
    }

    async fn client(server: &MockServer, with_middleware: bool) -> AuthCodePkceSpotify {
        client_with_token(server, with_middleware, "access-token").await
    }

    #[tokio::test]
    async fn successful_primary_request_does_not_use_fallback() {
        let primary_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/test"))
            .respond_with(ResponseTemplate::new(200).set_body_string("primary"))
            .expect(1)
            .mount(&primary_server)
            .await;
        let fallback_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/test"))
            .respond_with(ResponseTemplate::new(200).set_body_string("fallback"))
            .expect(0)
            .mount(&fallback_server)
            .await;

        let client = WebApiClient::new(
            client(&primary_server, false).await,
            Some(client(&fallback_server, true).await),
        );

        assert_eq!(
            client.api_get("test", &Query::new()).await.unwrap(),
            "primary"
        );
    }

    #[tokio::test]
    async fn server_error_does_not_use_fallback() {
        let primary_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/test"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&primary_server)
            .await;
        let fallback_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/test"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&fallback_server)
            .await;

        let client = WebApiClient::new(
            client(&primary_server, false).await,
            Some(client(&fallback_server, true).await),
        );

        assert!(client.api_get("test", &Query::new()).await.is_err());
    }

    #[tokio::test]
    async fn custom_rate_limit_falls_back_before_ncspot_retries() {
        let primary_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/test"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "0"))
            .expect(1)
            .mount(&primary_server)
            .await;
        let fallback_server = MockServer::start().await;
        let fallback_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        Mock::given(method("GET"))
            .and(path("/v1/test"))
            .respond_with(RateLimitThenOk {
                calls: Arc::clone(&fallback_calls),
            })
            .mount(&fallback_server)
            .await;

        let client = WebApiClient::new(
            client(&primary_server, false).await,
            Some(client(&fallback_server, true).await),
        );

        assert_eq!(
            client.api_get("test", &Query::new()).await.unwrap(),
            "fallback"
        );
        assert_eq!(fallback_calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn client_error_on_mutation_uses_fallback_once() {
        let primary_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/test"))
            .respond_with(ResponseTemplate::new(403))
            .expect(1)
            .mount(&primary_server)
            .await;
        let fallback_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/test"))
            .respond_with(ResponseTemplate::new(200).set_body_string("fallback"))
            .expect(1)
            .mount(&fallback_server)
            .await;

        let client = WebApiClient::new(
            client(&primary_server, false).await,
            Some(client(&fallback_server, true).await),
        );

        assert_eq!(
            client
                .api_post("test", &serde_json::json!({ "name": "playlist" }))
                .await
                .unwrap(),
            "fallback"
        );
    }

    #[tokio::test]
    async fn api_get_uses_fallback_token() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/test"))
            .and(header("authorization", "Bearer primary-token"))
            .respond_with(ResponseTemplate::new(403))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/test"))
            .and(header("authorization", "Bearer fallback-token"))
            .respond_with(ResponseTemplate::new(200).set_body_string("fallback"))
            .expect(1)
            .mount(&server)
            .await;

        let client = WebApiClient::new(
            client_with_token(&server, false, "primary-token").await,
            Some(client_with_token(&server, true, "fallback-token").await),
        );

        assert_eq!(
            client.api_get("test", &Query::new()).await.unwrap(),
            "fallback"
        );
    }

    #[tokio::test]
    async fn configured_endpoint_prefix_uses_ncspot_without_calling_primary() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/artists/artist-id"))
            .and(header("authorization", "Bearer primary-token"))
            .respond_with(ResponseTemplate::new(200).set_body_string("primary"))
            .expect(0)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/artists/artist-id"))
            .and(header("authorization", "Bearer fallback-token"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ncspot"))
            .expect(1)
            .mount(&server)
            .await;

        let client = WebApiClient::new(
            client_with_token(&server, false, "primary-token").await,
            Some(client_with_token(&server, true, "fallback-token").await),
        )
        .with_ncspot_only_get_endpoints(vec!["artists/".to_string()]);

        assert_eq!(
            client
                .api_get("artists/artist-id", &Query::new())
                .await
                .unwrap(),
            "ncspot"
        );
    }

    #[tokio::test]
    async fn current_user_playlists_uses_ncspot_without_calling_primary() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/me/playlists"))
            .and(header("authorization", "Bearer primary-token"))
            .respond_with(ResponseTemplate::new(200).set_body_string("primary"))
            .expect(0)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/me/playlists"))
            .and(header("authorization", "Bearer fallback-token"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ncspot"))
            .expect(1)
            .mount(&server)
            .await;

        let client = WebApiClient::new(
            client_with_token(&server, false, "primary-token").await,
            Some(client_with_token(&server, true, "fallback-token").await),
        );

        assert_eq!(
            client.api_get("me/playlists", &Query::new()).await.unwrap(),
            "ncspot"
        );
    }
}
