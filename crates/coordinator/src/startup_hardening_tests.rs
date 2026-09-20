use super::*;
use tower::ServiceExt;

#[tokio::test]
async fn forwarded_headers_do_not_bypass_a_peer_limit() {
    let router = limited(
        Router::new().route("/test", get(|| async { StatusCode::OK })),
        &RateLimitSettings::default(),
        1,
        1,
    );
    let request = |peer: &str, claimed: &str| {
        Request::builder()
            .uri("/test")
            .extension(ConnectInfo(peer.parse::<SocketAddr>().unwrap()))
            .header("x-forwarded-for", claimed)
            .header("x-real-ip", claimed)
            .header("forwarded", format!("for={claimed}"))
            .body(Body::empty())
            .unwrap()
    };
    assert_eq!(
        router
            .clone()
            .oneshot(request("192.0.2.1:1000", "198.51.100.1"))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    for claimed in ["198.51.100.2", "198.51.100.3", "198.51.100.4"] {
        assert_eq!(
            router
                .clone()
                .oneshot(request("192.0.2.1:2000", claimed))
                .await
                .unwrap()
                .status(),
            StatusCode::TOO_MANY_REQUESTS
        );
    }
    assert_eq!(
        router
            .clone()
            .oneshot(request("192.0.2.2:1000", "198.51.100.1"))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    let missing_peer = Request::builder()
        .uri("/test")
        .header("x-forwarded-for", "198.51.100.99")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        router.oneshot(missing_peer).await.unwrap().status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );
}

#[tokio::test]
async fn stopped_or_panicked_workers_cancel_the_service() {
    for mode in ["completed", "failed", "panicked"] {
        let tracker = TaskTracker::new();
        let cancel = CancellationToken::new();
        let worker = spawn_supervised(&tracker, "test worker", cancel.clone(), async move {
            match mode {
                "completed" => Ok(()),
                "failed" => Err(anyhow!("worker failure")),
                _ => panic!("worker panic"),
            }
        });
        tokio::time::timeout(Duration::from_secs(2), cancel.cancelled())
            .await
            .unwrap();
        worker.await.unwrap();
        tracker.close();
        tracker.wait().await;
    }
}

#[tokio::test]
async fn aborted_workers_also_cancel_the_service() {
    let tracker = TaskTracker::new();
    let cancel = CancellationToken::new();
    let worker = spawn_supervised(
        &tracker,
        "aborted worker",
        cancel.clone(),
        std::future::pending(),
    );
    worker.abort();
    assert!(worker.await.unwrap_err().is_cancelled());
    assert!(cancel.is_cancelled());
}

#[tokio::test]
async fn requested_shutdown_remains_a_clean_worker_exit() {
    let tracker = TaskTracker::new();
    let cancel = CancellationToken::new();
    let observer = cancel.clone();
    let worker = spawn_supervised(&tracker, "shutdown worker", cancel.clone(), async move {
        observer.cancelled().await;
        Ok(())
    });
    cancel.cancel();
    worker.await.unwrap();
}

#[test]
fn request_config_preserves_defaults_and_rejects_unusable_limits() {
    let old = r#"domain = "127.0.0.1"
port = "9990"
origins = ["http://localhost:9990"]"#;
    let mut settings: APISettings = toml::from_str(old).unwrap();
    assert_eq!(settings.rate_limit.auth_burst, 10);
    settings
        .validate(&crate::config::UISettings::default())
        .unwrap();
    settings.rate_limit.auth_per_second = 0;
    assert!(settings
        .validate(&crate::config::UISettings::default())
        .is_err());
    settings.rate_limit.enabled = false;
    settings
        .validate(&crate::config::UISettings::default())
        .unwrap();
    settings.replay_capacity = 0;
    assert!(settings
        .validate(&crate::config::UISettings::default())
        .is_err());
}
