use std::fs;
use std::path::Path;

use tempfile::TempDir;

use crate::loader::load_config;
use crate::overrides::parse_cli_override;
use crate::types::{
    CliOverrides, ConfigWarning, LoadConfigOptions, PROJECT_LOCAL_CONFIG_RELATIVE, ProviderKind,
};

fn test_options(root: &TempDir) -> LoadConfigOptions {
    LoadConfigOptions {
        cwd: root.path().join("repo"),
        cli: CliOverrides::default(),
        xdg_config_home: Some(root.path().join("xdg")),
        home_dir: None,
    }
}

fn write(path: &Path, body: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("parent dirs");
    }
    fs::write(path, body).expect("write file");
}

#[test]
fn merges_user_project_and_local_by_precedence() {
    let tmp = TempDir::new().expect("tmp");
    let repo = tmp.path().join("repo");
    fs::create_dir_all(repo.join(".git")).expect("git");
    write(
        &tmp.path().join("xdg/defect/config.toml"),
        r#"
[default]
provider = "echo"
model = "user-model"

[turn]
max_llm_retries = 5
"#,
    );
    write(
        &repo.join(".defect/config.toml"),
        r#"
[default]
model = "project-model"
"#,
    );
    write(
        &repo.join(PROJECT_LOCAL_CONFIG_RELATIVE),
        r#"
[default]
model = "local-model"
"#,
    );

    let loaded = load_config(test_options(&tmp)).expect("load config");

    assert_eq!(loaded.effective.provider, ProviderKind::Echo);
    assert_eq!(loaded.effective.model, "local-model");
    assert_eq!(loaded.effective.turn.max_llm_retries, 5);
    assert_eq!(loaded.layers.layers.len(), 4);
}

#[test]
fn cli_overrides_win_over_local_layer() {
    let tmp = TempDir::new().expect("tmp");
    let repo = tmp.path().join("repo");
    fs::create_dir_all(repo.join(".git")).expect("git");
    write(
        &repo.join(PROJECT_LOCAL_CONFIG_RELATIVE),
        r#"
[default]
provider = "openai"
model = "local-model"
"#,
    );

    let mut opts = test_options(&tmp);
    opts.cli.provider = Some(ProviderKind::Anthropic);
    opts.cli.model = Some("cli-model".into());
    let loaded = load_config(opts).expect("load config");

    assert_eq!(loaded.effective.provider, ProviderKind::Anthropic);
    assert_eq!(loaded.effective.model, "cli-model");
}

#[test]
fn shared_project_layer_denylist_warns_and_ignores() {
    let tmp = TempDir::new().expect("tmp");
    let repo = tmp.path().join("repo");
    fs::create_dir_all(repo.join(".git")).expect("git");
    write(
        &repo.join(".defect/config.toml"),
        r#"
[default]
provider = "openai"

[providers.openai]
base_url = "https://example.invalid/v1"
"#,
    );

    let loaded = load_config(test_options(&tmp)).expect("load config");

    assert_eq!(loaded.effective.provider, ProviderKind::Echo);
    assert_eq!(loaded.warnings.len(), 2);
    assert!(loaded.warnings.iter().any(|warning| matches!(
        warning,
        ConfigWarning::IgnoredProjectKey { key, .. } if key == "default.provider"
    )));
    assert_eq!(loaded.effective.providers.openai.base_url, None);
}

#[test]
fn project_local_layer_can_override_endpoint() {
    let tmp = TempDir::new().expect("tmp");
    let repo = tmp.path().join("repo");
    fs::create_dir_all(repo.join(".git")).expect("git");
    write(
        &repo.join(PROJECT_LOCAL_CONFIG_RELATIVE),
        r#"
[default]
provider = "openai"

[providers.openai]
base_url = "https://example.invalid/v1"
"#,
    );

    let loaded = load_config(test_options(&tmp)).expect("load config");

    assert_eq!(loaded.effective.provider, ProviderKind::Openai);
    assert_eq!(
        loaded.effective.providers.openai.base_url.as_deref(),
        Some("https://example.invalid/v1")
    );
}

#[test]
fn parses_dotted_cli_override_values() {
    let tmp = TempDir::new().expect("tmp");
    let repo = tmp.path().join("repo");
    fs::create_dir_all(repo.join(".git")).expect("git");

    let mut opts = test_options(&tmp);
    opts.cli.config_overrides = vec![
        parse_cli_override("turn.max_llm_retries=9").expect("override"),
        parse_cli_override("providers.openai.base_url=\"https://localhost:1234/v1\"")
            .expect("override"),
    ];

    let loaded = load_config(opts).expect("load config");
    assert_eq!(loaded.effective.turn.max_llm_retries, 9);
    assert_eq!(
        loaded.effective.providers.openai.base_url.as_deref(),
        Some("https://localhost:1234/v1")
    );
}
