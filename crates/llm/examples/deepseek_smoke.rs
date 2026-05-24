//! DeepSeek provider 真端点冒烟。
//!
//! 用法：
//!
//! ```bash
//! DEEPSEEK_API_KEY=sk-... \
//!   cargo run -p defect-llm --example deepseek_smoke -- [scenario]
//! ```
//!
//! `[scenario]` ∈ `list-models | text | tool | thinking | all`，默认 `all`。
//!
//! 可选 env：
//! - `DEEPSEEK_BASE_URL`：覆盖默认 `https://api.deepseek.com/v1`
//! - `DEEPSEEK_MODEL`：覆盖默认模型 `deepseek-chat`
//! - `RUST_LOG=defect_llm=debug` 打开协议层调试日志
//!
//! `thinking` 场景默认强制使用 `deepseek-reasoner` 跑——`deepseek-chat`
//! 不发 `reasoning_content`，跑了也是 SKIP。

mod common;

use std::sync::Arc;

use agent_client_protocol::schema::StopReason as AcpStopReason;
use defect_agent::llm::{LlmProvider, SamplingParams};
use defect_llm::provider::deepseek::{DeepSeekConfig, DeepSeekProvider};

use common::{
    EXIT_FAIL, EXIT_OK, build_session, env_string, init_tracing, print_fail, print_pass,
    print_skip, run_turn_and_print, sampling_with_thinking, scenario_from_args,
};

const DEFAULT_MODEL: &str = "deepseek-chat";
const REASONER_MODEL: &str = "deepseek-reasoner";
const THINKING_BUDGET_TOKENS: u32 = 2048;

const TEXT_PROMPT: &str = "Say hello in one short sentence.";
const TOOL_PROMPT: &str = "Please call the `echo` tool with msg=\"hello from smoke\", \
     then briefly summarize what the tool returned.";
const THINKING_PROMPT: &str = "Think step by step: a farmer has 17 sheep and all but 9 die. How many are left? \
     Show your reasoning briefly, then give the final number.";

#[tokio::main]
async fn main() {
    init_tracing();

    let api_key = match env_string("DEEPSEEK_API_KEY").or_else(|| env_string("OPENAI_API_KEY")) {
        Some(k) => k,
        None => {
            eprintln!("DEEPSEEK_API_KEY (or OPENAI_API_KEY) is required for deepseek_smoke");
            std::process::exit(EXIT_FAIL);
        }
    };
    let base_url = env_string("DEEPSEEK_BASE_URL");
    let model = env_string("DEEPSEEK_MODEL").unwrap_or_else(|| DEFAULT_MODEL.to_string());

    let provider: Arc<dyn LlmProvider> = match DeepSeekProvider::new(DeepSeekConfig {
        api_key: Some(api_key),
        base_url,
    }) {
        Ok(p) => Arc::new(p),
        Err(e) => {
            eprintln!("provider init failed: {e}");
            std::process::exit(EXIT_FAIL);
        }
    };

    let scenario = scenario_from_args();
    println!("=== deepseek smoke: scenario={scenario} model={model} ===");

    let mut failed = 0u32;
    let mut ran = 0u32;

    for label in scenarios_for(&scenario) {
        ran += 1;
        let outcome = run_scenario(label, provider.clone(), &model).await;
        match outcome {
            ScenarioOutcome::Pass => print_pass(label),
            ScenarioOutcome::Skip(reason) => print_skip(label, &reason),
            ScenarioOutcome::Fail(err) => {
                failed += 1;
                print_fail(label, &err);
            }
        }
    }

    println!("\n=== deepseek smoke done: ran={ran} failed={failed} ===");
    if failed > 0 {
        std::process::exit(EXIT_FAIL);
    } else {
        std::process::exit(EXIT_OK);
    }
}

fn scenarios_for(name: &str) -> Vec<&'static str> {
    match name {
        "list-models" => vec!["list-models"],
        "text" => vec!["text"],
        "tool" => vec!["tool"],
        "thinking" => vec!["thinking"],
        _ => vec!["list-models", "text", "tool", "thinking"],
    }
}

enum ScenarioOutcome {
    Pass,
    Skip(String),
    Fail(String),
}

async fn run_scenario(label: &str, provider: Arc<dyn LlmProvider>, model: &str) -> ScenarioOutcome {
    println!("\n--- running: {label} ---");
    let res = match label {
        "list-models" => scenario_list_models(provider).await,
        "text" => scenario_text(provider, model).await,
        "tool" => scenario_tool(provider, model).await,
        "thinking" => scenario_thinking(provider, model).await,
        other => Err(format!("unknown scenario {other}")),
    };
    match res {
        Ok(None) => ScenarioOutcome::Pass,
        Ok(Some(reason)) => ScenarioOutcome::Skip(reason),
        Err(e) => ScenarioOutcome::Fail(e),
    }
}

async fn scenario_list_models(provider: Arc<dyn LlmProvider>) -> Result<Option<String>, String> {
    let models = provider.list_models().await.map_err(|e| e.to_string())?;
    if models.is_empty() {
        return Err("list_models returned empty".to_string());
    }
    println!("got {} models, first 5:", models.len());
    for m in models.iter().take(5) {
        println!(
            "  - {} ({})",
            m.id,
            m.display_name.as_deref().unwrap_or("-")
        );
    }
    Ok(None)
}

async fn scenario_text(
    provider: Arc<dyn LlmProvider>,
    model: &str,
) -> Result<Option<String>, String> {
    let session = build_session(provider, model, SamplingParams::default()).await;
    let (stop, text, _hits) = run_turn_and_print(session, TEXT_PROMPT)
        .await
        .map_err(|e| e.to_string())?;
    if !matches!(stop, AcpStopReason::EndTurn) {
        return Err(format!("unexpected stop reason: {stop:?}"));
    }
    if text.trim().is_empty() {
        return Err("empty assistant text".to_string());
    }
    Ok(None)
}

async fn scenario_tool(
    provider: Arc<dyn LlmProvider>,
    model: &str,
) -> Result<Option<String>, String> {
    // deepseek-reasoner 暂不支持 function calling；落到 deepseek-chat 上跑。
    let tool_model = if model == REASONER_MODEL {
        DEFAULT_MODEL
    } else {
        model
    };
    let session = build_session(provider, tool_model, SamplingParams::default()).await;
    let (stop, _text, hits) = run_turn_and_print(session, TOOL_PROMPT)
        .await
        .map_err(|e| e.to_string())?;
    if !matches!(stop, AcpStopReason::EndTurn) {
        return Err(format!("unexpected stop reason: {stop:?}"));
    }
    if hits.started == 0 || hits.finished == 0 {
        return Err(format!(
            "expected at least one tool call (started={}, finished={})",
            hits.started, hits.finished
        ));
    }
    Ok(None)
}

async fn scenario_thinking(
    provider: Arc<dyn LlmProvider>,
    model: &str,
) -> Result<Option<String>, String> {
    // 强制用 reasoner——deepseek-chat 不发 reasoning_content，跑了不验证什么。
    let thinking_model = REASONER_MODEL;
    if model != REASONER_MODEL {
        println!(
            "(scenario thinking forces model={REASONER_MODEL}; \
             configured model={model} ignored)"
        );
    }
    let sampling = sampling_with_thinking(Some(THINKING_BUDGET_TOKENS));
    let session = build_session(provider, thinking_model, sampling).await;
    let (stop, text, hits) = run_turn_and_print(session, THINKING_PROMPT)
        .await
        .map_err(|e| e.to_string())?;
    if !matches!(stop, AcpStopReason::EndTurn) {
        return Err(format!("unexpected stop reason: {stop:?}"));
    }
    if text.trim().is_empty() {
        return Err("empty assistant text".to_string());
    }
    if hits.thought_text.trim().is_empty() {
        return Ok(Some(format!(
            "no reasoning_content emitted by {thinking_model}; \
             check upstream changed shape"
        )));
    }
    Ok(None)
}
