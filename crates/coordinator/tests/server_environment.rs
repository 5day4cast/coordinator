use std::{
    process::{Command, Stdio},
    time::{Duration, Instant},
};

// Exercise the actual server entry point: calling only the generic file loader
// used to skip these checks as well as all the valid runtime overrides.
#[test]
fn server_rejects_invalid_runtime_environment_before_starting() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("settings.toml");
    std::fs::write(
        &config,
        toml::to_string(&coordinator::config::Settings::default()).unwrap(),
    )
    .unwrap();
    for name in [
        "COORDINATOR_METRICS_LISTEN_ADDR",
        "COORDINATOR_TELEMETRY_ENABLED",
        "COORDINATOR_FEEDBACK_ENABLED",
    ] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_coordinator"))
            .args(["--config", config.to_str().unwrap()])
            .env_clear()
            .env(name, "invalid-test-value")
            .current_dir(dir.path())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("server ignored invalid {name} and did not exit");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let output = child.wait_with_output().unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(name), "server skipped {name}: {stderr}");
    }
}
