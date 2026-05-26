//! DeepSeek provider 集测：复用 OpenAI-compatible transport，但把
//! DeepSeek 特有的 `/models` 兼容差异收敛在 wrapper 自己。

use std::sync::Arc;

use defect_agent::llm::LlmProvider;
use defect_llm::provider::deepseek::{DeepSeekConfig, DeepSeekProvider};
use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, ResponseTemplate};

mod common;
use common::start_mock_server;

const TEST_API_KEY: &str = "test-deepseek-key";
const TEST_AUTH_HEADER: &str = "Bearer test-deepseek-key";

fn provider_for(server_uri: &str) -> Arc<dyn LlmProvider> {
    let cfg = DeepSeekConfig {
        api_key: Some(TEST_API_KEY.to_string()),
        base_url: Some(server_uri.to_string()),
    };
    Arc::new(DeepSeekProvider::new(cfg).expect("provider")) as Arc<dyn LlmProvider>
}

#[tokio::test]
async fn list_models_falls_back_to_builtin_deepseek_models_when_decode_fails() {
    let server = start_mock_server().await;

    let incompatible_body = json!({
        "object": "list",
        "data": [
            {"id": "deepseek-v4-pro"},
            {"id": "deepseek-v4-flash"}
        ]
    });
    Mock::given(method("GET"))
        .and(path("/models"))
        .and(header("authorization", TEST_AUTH_HEADER))
        .respond_with(ResponseTemplate::new(200).set_body_json(incompatible_body))
        .expect(1)
        .mount(&server)
        .await;

    let provider = provider_for(&server.uri());
    let models = provider.list_models().await.expect("list models");

    assert!(
        models.iter().any(|model| model.id == "deepseek-v4-pro"),
        "expected built-in deepseek-v4-pro fallback"
    );
    assert!(
        models.iter().any(|model| model.id == "deepseek-v4-flash"),
        "expected built-in deepseek-v4-flash fallback"
    );
}
