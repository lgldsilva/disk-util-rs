//! Snapshot do TOML default (schema da config versionada).
//!
//! Garante que `config.example.toml` só muda por decisão explícita
//! (snapshot review), igual ao fluxo de `docker-top-rs`.

use disk_util_rs::config::AppConfig;

#[test]
fn default_config_snapshot() {
    let toml = toml::to_string_pretty(&AppConfig::default()).expect("serialize default");
    insta::assert_snapshot!("default_config", toml);
}
