//! Sign-in against a local stand-in for the Microsoft sign-in service.

use drivecraft::auth::{scopes_read_only, AppConfig, Authenticator, TokenSource};
use serde_json::json;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn app() -> AppConfig {
    AppConfig {
        client_id: "client-123".into(),
        tenant: "organizations".into(),
    }
}

#[tokio::test]
async fn code_sign_in_is_saved_and_renewed() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/devicecode"))
        .and(body_string_contains("client_id=client-123"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_code": "DEV", "user_code": "ABCD-1234",
            "verification_uri": "https://example.test/device", "expires_in": 60, "interval": 1
        })))
        .mount(&server)
        .await;
    // Not approved yet, then approved, then a renewal.
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("device_code=DEV"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({ "error": "authorization_pending" })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("device_code=DEV"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "ACCESS-1", "refresh_token": "REFRESH-1", "expires_in": 3600,
            "scope": "Files.Read.All Sites.Read.All User.Read"
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .and(body_string_contains("refresh_token=REFRESH-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "access_token": "ACCESS-2", "expires_in": 3600 })))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("signin.bin");
    let auth = Authenticator::new(app(), store.clone()).unwrap().with_authority(server.uri());
    let said = std::sync::Mutex::new(Vec::new());
    let tokens = auth
        .login_device_code(&scopes_read_only(), |s| said.lock().unwrap().push(s.to_string()))
        .await
        .unwrap();
    assert!(said.lock().unwrap()[0].contains("ABCD-1234"));
    assert!(!tokens.can_write(), "a read-only sign-in is recognised as such");
    assert_eq!(auth.token(false).await.unwrap(), "ACCESS-1");

    // A new process picks the sign-in up from disk, and can renew it.
    let again = Authenticator::new(app(), store).unwrap().with_authority(server.uri());
    assert_eq!(again.token(false).await.unwrap(), "ACCESS-1");
    assert_eq!(again.token(true).await.unwrap(), "ACCESS-2");
    let kept = again.current().await.unwrap();
    assert_eq!(kept.refresh_token.as_deref(), Some("REFRESH-1"), "kept when the server sends no new one");
    assert!(!kept.can_write());

    again.logout().unwrap();
    let gone = Authenticator::new(app(), dir.path().join("signin.bin")).unwrap();
    assert!(gone.token(false).await.is_err());
}
