use crate::runtime::Config;

pub(super) fn args(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

/// A configuration that sets every flag, so no environment default leaks in.
pub(super) fn full_config() -> Config {
    Config::from_args(&args(&[
        "--data-dir",
        "/srv/sidevoice/core",
        "--socket",
        "/run/sidevoice/local.sock",
        "--ready-file",
        "/run/sidevoice/ready.json",
        "--host",
        "0.0.0.0",
        "--port",
        "9000",
        "--launch-id",
        "launch",
        "--log-file",
        "/var/log/sidevoice/core.log",
        "--room-credential",
        "/srv/sidevoice/room.json",
        "--idle-exit",
        "0",
    ]))
    .expect("valid flags")
}
