use std::fs;

use tempfile::TempDir;

use crate::session::{BasePromptConfig, PromptConfig, resolve_system_prompt};

fn write(path: &std::path::Path, body: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("parent dirs");
    }
    fs::write(path, body).expect("write file");
}

#[test]
fn resolves_base_prompt_before_append_layers() {
    let tmp = TempDir::new().expect("tmp");
    let repo = tmp.path().join("repo");
    let cwd = repo.join("apps/web");
    fs::create_dir_all(repo.join(".git")).expect("git");
    fs::create_dir_all(&cwd).expect("cwd");
    write(&repo.join("AGENTS.md"), "repo prompt");
    write(&cwd.join("AGENTS.md"), "cwd prompt");
    write(&repo.join("prompts/base.md"), "base file");

    let prompt = PromptConfig {
        file: "AGENTS.md".to_owned(),
        text: Some("user prompt".to_owned()),
        provider_overlays: [("deepseek".to_owned(), "provider overlay".to_owned())].into(),
        model_overlays: [("deepseek-v4-pro".to_owned(), "model overlay".to_owned())].into(),
    };
    let base_prompt = BasePromptConfig {
        file: Some(repo.join("prompts/base.md")),
        text: Some("base text".to_owned()),
    };

    let resolved = resolve_system_prompt(
        &cwd,
        "deepseek",
        "deepseek-v4-pro",
        &base_prompt,
        &prompt,
        Some("session overlay"),
    )
    .expect("resolve")
    .expect("system prompt");

    assert_eq!(
        resolved,
        [
            "base file",
            "base text",
            "user prompt",
            "repo prompt",
            "cwd prompt",
            "provider overlay",
            "model overlay",
            "session overlay",
        ]
        .join("\n\n")
    );
}

#[test]
fn skips_missing_default_agents_file() {
    let tmp = TempDir::new().expect("tmp");
    let repo = tmp.path().join("repo");
    fs::create_dir_all(repo.join(".git")).expect("git");

    let resolved = resolve_system_prompt(
        &repo,
        "openai",
        "gpt-4o-mini",
        &BasePromptConfig::default(),
        &PromptConfig::default(),
        None,
    )
    .expect("resolve");

    assert_eq!(resolved, None);
}
