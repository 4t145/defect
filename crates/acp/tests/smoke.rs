//! E2E smoke test：在进程内用 [`Channel::duplex`] 把 ACP 客户端 / 服务端对接起来，
//! 跑一遍 initialize → session/new → session/prompt 的最小路径。
//!
//! 校验三件事：
//! 1. `serve_on` 正确处理 `initialize` / `session/new` / `session/prompt`
//! 2. [`EchoProvider`] 通过 [`crate::project`] 投射出 `AgentMessageChunk`
//! 3. `PromptResponse` 拿到 `EndTurn` stop reason

use std::sync::Arc;
use std::sync::Mutex;

use agent_client_protocol::schema::{
    ContentBlock, InitializeRequest, LoadSessionRequest, ModelId, NewSessionRequest, PromptRequest,
    ProtocolVersion, SessionNotification, SessionUpdate, StopReason as AcpStopReason, TextContent,
};
use agent_client_protocol::{Agent, Channel, Client, ConnectTo, Role};
use defect_acp::{EchoProvider, serve_on};
use defect_agent::llm::{
    Capabilities, CompletionRequest, FeatureSupport, LlmProvider, ModelInfo, ProtocolId,
    ProviderChunk, ProviderError, ProviderErrorKind, ProviderInfo, ProviderStream,
    StopReason as LlmStopReason, ThinkingEcho,
};
use defect_agent::session::{AgentCore, DefaultAgentCore, TurnConfig};
use defect_storage::StorageObserver;
use futures::future::BoxFuture;
use futures::stream;
use tokio_util::sync::CancellationToken;

/// `Channel` 实现的是 `ConnectTo<R>` for 任意 R，但 `serve_on` 需要
/// `T: ConnectTo<Agent>`。这里的 wrapper 仅是显式声明 role，方便类型推导。
struct ChannelTransport<R: Role> {
    inner: Channel,
    _marker: std::marker::PhantomData<R>,
}

struct SwitchableProvider;

impl LlmProvider for SwitchableProvider {
    fn info(&self) -> ProviderInfo {
        ProviderInfo {
            vendor: "switchable".to_string(),
            protocol: ProtocolId::AnthropicMessages,
            display_name: "Switchable Test Provider".to_string(),
        }
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tool_calls: FeatureSupport::Unsupported,
            parallel_tool_calls: FeatureSupport::Unsupported,
            thinking: FeatureSupport::Unsupported,
            vision: FeatureSupport::Unsupported,
            prompt_cache: FeatureSupport::Unsupported,
            thinking_echo: ThinkingEcho::Forbidden,
        }
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, ProviderError>> {
        Box::pin(async {
            Ok(vec![
                ModelInfo {
                    id: "alpha".to_string(),
                    display_name: Some("Alpha".to_string()),
                    context_window: None,
                    max_output_tokens: None,
                    deprecated: false,
                    capabilities_overrides: Default::default(),
                },
                ModelInfo {
                    id: "beta".to_string(),
                    display_name: Some("Beta".to_string()),
                    context_window: None,
                    max_output_tokens: None,
                    deprecated: false,
                    capabilities_overrides: Default::default(),
                },
            ])
        })
    }

    fn model_info(&self, model_id: &str) -> Option<ModelInfo> {
        match model_id {
            "alpha" => Some(ModelInfo {
                id: "alpha".to_string(),
                display_name: Some("Alpha".to_string()),
                context_window: None,
                max_output_tokens: None,
                deprecated: false,
                capabilities_overrides: Default::default(),
            }),
            "beta" => Some(ModelInfo {
                id: "beta".to_string(),
                display_name: Some("Beta".to_string()),
                context_window: None,
                max_output_tokens: None,
                deprecated: false,
                capabilities_overrides: Default::default(),
            }),
            _ => None,
        }
    }

    fn complete(
        &self,
        req: CompletionRequest,
        _cancel: CancellationToken,
    ) -> BoxFuture<'_, Result<ProviderStream, ProviderError>> {
        let model = req.model.clone();
        Box::pin(async move {
            let chunks: Vec<Result<ProviderChunk, ProviderError>> = vec![
                Ok(ProviderChunk::MessageStart {
                    id: "switchable-0".to_string(),
                    model: model.clone(),
                }),
                Ok(ProviderChunk::TextDelta {
                    text: format!("model={model}"),
                }),
                Ok(ProviderChunk::Stop {
                    reason: LlmStopReason::EndTurn,
                }),
            ];
            let s: ProviderStream = Box::pin(stream::iter(chunks));
            Ok(s)
        })
    }
}

struct FlakyModelProvider;

impl LlmProvider for FlakyModelProvider {
    fn info(&self) -> ProviderInfo {
        ProviderInfo {
            vendor: "flaky".to_string(),
            protocol: ProtocolId::AnthropicMessages,
            display_name: "Flaky Model Provider".to_string(),
        }
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tool_calls: FeatureSupport::Unsupported,
            parallel_tool_calls: FeatureSupport::Unsupported,
            thinking: FeatureSupport::Unsupported,
            vision: FeatureSupport::Unsupported,
            prompt_cache: FeatureSupport::Unsupported,
            thinking_echo: ThinkingEcho::Forbidden,
        }
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, ProviderError>> {
        Box::pin(async {
            Err(ProviderError::new(ProviderErrorKind::Other(
                defect_agent::error::BoxError::new(std::io::Error::other(
                    "models endpoint unavailable",
                )),
            )))
        })
    }

    fn model_info(&self, model_id: &str) -> Option<ModelInfo> {
        Some(ModelInfo {
            id: model_id.to_string(),
            display_name: Some(model_id.to_string()),
            context_window: None,
            max_output_tokens: None,
            deprecated: false,
            capabilities_overrides: Default::default(),
        })
    }

    fn complete(
        &self,
        req: CompletionRequest,
        _cancel: CancellationToken,
    ) -> BoxFuture<'_, Result<ProviderStream, ProviderError>> {
        let model = req.model.clone();
        Box::pin(async move {
            let chunks: Vec<Result<ProviderChunk, ProviderError>> = vec![
                Ok(ProviderChunk::MessageStart {
                    id: "flaky-0".to_string(),
                    model: model.clone(),
                }),
                Ok(ProviderChunk::TextDelta {
                    text: format!("model={model}"),
                }),
                Ok(ProviderChunk::Stop {
                    reason: LlmStopReason::EndTurn,
                }),
            ];
            let s: ProviderStream = Box::pin(stream::iter(chunks));
            Ok(s)
        })
    }
}

impl<R: Role> ChannelTransport<R> {
    fn new(inner: Channel) -> Self {
        Self {
            inner,
            _marker: std::marker::PhantomData,
        }
    }
}

impl<R: Role> ConnectTo<R> for ChannelTransport<R> {
    async fn connect_to(
        self,
        client: impl ConnectTo<R::Counterpart>,
    ) -> Result<(), agent_client_protocol::Error> {
        <Channel as ConnectTo<R>>::connect_to(self.inner, client).await
    }

    fn into_channel_and_future(
        self,
    ) -> (
        Channel,
        agent_client_protocol::BoxFuture<'static, Result<(), agent_client_protocol::Error>>,
    ) {
        <Channel as ConnectTo<R>>::into_channel_and_future(self.inner)
    }
}

#[tokio::test]
async fn echo_round_trip() {
    let provider = Arc::new(EchoProvider::new());
    let config = TurnConfig {
        model: "echo".to_string(),
        ..TurnConfig::default()
    };
    let agent_core = DefaultAgentCore::builder()
        .provider(provider)
        .config(config)
        .build();
    let agent_core: Arc<dyn AgentCore> = Arc::new(agent_core);

    // server 用 channel_b（agent 视角），client 用 channel_a（client 视角）。
    let (channel_a, channel_b) = Channel::duplex();

    let server_handle = tokio::spawn(serve_on(
        agent_core,
        ChannelTransport::<Agent>::new(channel_b),
    ));

    let updates: Arc<Mutex<Vec<SessionUpdate>>> = Arc::new(Mutex::new(Vec::new()));
    let updates_for_handler = updates.clone();

    let cwd = std::env::current_dir().expect("cwd available");
    let prompt_text = "hello echo";

    let client_result = Client
        .builder()
        .name("smoke-client")
        .on_receive_notification(
            async move |notif: SessionNotification, _cx| {
                updates_for_handler
                    .lock()
                    .expect("updates mutex")
                    .push(notif.update);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(
            ChannelTransport::<Client>::new(channel_a),
            async move |cx| {
                cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;

                let new_session = cx
                    .send_request(NewSessionRequest::new(cwd))
                    .block_task()
                    .await?;
                let models = new_session
                    .models
                    .expect("agent should advertise session model candidates");
                assert_eq!(models.current_model_id.0.as_ref(), "echo");
                assert_eq!(models.available_models.len(), 1);
                assert_eq!(models.available_models[0].model_id.0.as_ref(), "echo");

                let prompt_resp = cx
                    .send_request(PromptRequest::new(
                        new_session.session_id,
                        vec![ContentBlock::Text(TextContent::new(
                            prompt_text.to_string(),
                        ))],
                    ))
                    .block_task()
                    .await?;

                Ok(prompt_resp.stop_reason)
            },
        )
        .await
        .expect("client connection completed");

    assert_eq!(
        client_result,
        AcpStopReason::EndTurn,
        "echo provider should drive a clean EndTurn"
    );

    // serve 用的是 `connect_to`（内部 `future::pending`），不会因为 main_fn
    // 结束自动退出；测试里直接 abort 即可。
    server_handle.abort();
    let _ = server_handle.await;

    let updates = updates.lock().expect("updates mutex");
    let assistant_text: String = updates
        .iter()
        .filter_map(|u| match u {
            SessionUpdate::AgentMessageChunk(chunk) => Some(&chunk.content),
            _ => None,
        })
        .filter_map(|content| match content {
            ContentBlock::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .collect();
    assert!(
        assistant_text.contains(prompt_text),
        "echo response should include user's prompt; got {assistant_text:?}; updates {updates:?}",
    );
}

#[tokio::test]
async fn load_session_round_trip() {
    let provider = Arc::new(EchoProvider::new());
    let config = TurnConfig {
        model: "echo".to_string(),
        ..TurnConfig::default()
    };
    let sessions_dir = tempfile::tempdir().expect("tempdir");
    let storage = Arc::new(StorageObserver::new(sessions_dir.path().to_path_buf()));
    let agent_core = DefaultAgentCore::builder()
        .provider(provider)
        .observe_session(storage.clone())
        .session_loader(storage)
        .config(config)
        .build();
    let agent_core: Arc<dyn AgentCore> = Arc::new(agent_core);

    let (channel_a, channel_b) = Channel::duplex();
    let server_handle = tokio::spawn(serve_on(
        agent_core,
        ChannelTransport::<Agent>::new(channel_b),
    ));

    let cwd = std::env::current_dir().expect("cwd available");
    let prompt_text = "resume me";
    let updates: Arc<Mutex<Vec<SessionUpdate>>> = Arc::new(Mutex::new(Vec::new()));
    let updates_for_handler = updates.clone();

    let client_result = Client
        .builder()
        .name("load-session-client")
        .on_receive_notification(
            async move |notif: SessionNotification, _cx| {
                updates_for_handler
                    .lock()
                    .expect("updates mutex")
                    .push(notif.update);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(
            ChannelTransport::<Client>::new(channel_a),
            async move |cx| {
                let init = cx
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                assert!(
                    init.agent_capabilities.load_session,
                    "agent should advertise load_session capability"
                );

                let new_session = cx
                    .send_request(NewSessionRequest::new(cwd.clone()))
                    .block_task()
                    .await?;
                let new_models = new_session
                    .models
                    .expect("new session should include model candidates");
                assert_eq!(new_models.current_model_id.0.as_ref(), "echo");

                let first = cx
                    .send_request(PromptRequest::new(
                        new_session.session_id.clone(),
                        vec![ContentBlock::Text(TextContent::new(
                            prompt_text.to_string(),
                        ))],
                    ))
                    .block_task()
                    .await?;
                assert_eq!(first.stop_reason, AcpStopReason::EndTurn);

                cx.send_request(LoadSessionRequest::new(
                    new_session.session_id.clone(),
                    cwd.clone(),
                ))
                .block_task()
                .await?
                .models
                .expect("loaded session should include model candidates");

                let replayed_user_text = updates
                    .lock()
                    .expect("updates mutex")
                    .iter()
                    .filter_map(|update| match update {
                        SessionUpdate::UserMessageChunk(chunk) => Some(&chunk.content),
                        _ => None,
                    })
                    .filter_map(|content| match content {
                        ContentBlock::Text(text) => Some(text.text.as_str()),
                        _ => None,
                    })
                    .any(|text| text == prompt_text);
                assert!(
                    replayed_user_text,
                    "session/load should replay previous user transcript"
                );

                let second = cx
                    .send_request(PromptRequest::new(
                        new_session.session_id,
                        vec![ContentBlock::Text(TextContent::new(
                            "after load".to_string(),
                        ))],
                    ))
                    .block_task()
                    .await?;

                Ok(second.stop_reason)
            },
        )
        .await
        .expect("client connection completed");

    assert_eq!(client_result, AcpStopReason::EndTurn);

    server_handle.abort();
    let _ = server_handle.await;
}

#[tokio::test]
async fn set_model_updates_next_turn_model() {
    let provider = Arc::new(SwitchableProvider);
    let config = TurnConfig {
        model: "alpha".to_string(),
        allowed_models: Some(vec!["alpha".to_string(), "beta".to_string()]),
        ..TurnConfig::default()
    };
    let agent_core = DefaultAgentCore::builder()
        .provider(provider)
        .config(config)
        .build();
    let agent_core: Arc<dyn AgentCore> = Arc::new(agent_core);

    let (channel_a, channel_b) = Channel::duplex();
    let server_handle = tokio::spawn(serve_on(
        agent_core,
        ChannelTransport::<Agent>::new(channel_b),
    ));

    let cwd = std::env::current_dir().expect("cwd available");
    let client_result = Client
        .builder()
        .name("set-model-client")
        .connect_with(
            ChannelTransport::<Client>::new(channel_a),
            async move |cx| {
                cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;

                let new_session = cx
                    .send_request(NewSessionRequest::new(cwd))
                    .block_task()
                    .await?;
                let models = new_session
                    .models
                    .expect("agent should advertise session model candidates");
                assert_eq!(models.current_model_id.0.as_ref(), "alpha");
                assert_eq!(models.available_models.len(), 2);

                cx.send_request(agent_client_protocol::schema::SetSessionModelRequest::new(
                    new_session.session_id.clone(),
                    ModelId::new("beta"),
                ))
                .block_task()
                .await?;

                let prompt_resp = cx
                    .send_request(PromptRequest::new(
                        new_session.session_id,
                        vec![ContentBlock::Text(TextContent::new("switch".to_string()))],
                    ))
                    .block_task()
                    .await?;

                Ok(prompt_resp.stop_reason)
            },
        )
        .await
        .expect("client connection completed");

    assert_eq!(client_result, AcpStopReason::EndTurn);

    server_handle.abort();
    let _ = server_handle.await;
}

#[tokio::test]
async fn set_model_rejects_model_outside_configured_candidates() {
    let provider = Arc::new(SwitchableProvider);
    let config = TurnConfig {
        model: "alpha".to_string(),
        allowed_models: Some(vec!["alpha".to_string()]),
        ..TurnConfig::default()
    };
    let agent_core = DefaultAgentCore::builder()
        .provider(provider)
        .config(config)
        .build();
    let agent_core: Arc<dyn AgentCore> = Arc::new(agent_core);

    let (channel_a, channel_b) = Channel::duplex();
    let server_handle = tokio::spawn(serve_on(
        agent_core,
        ChannelTransport::<Agent>::new(channel_b),
    ));

    let cwd = std::env::current_dir().expect("cwd available");
    let client_result = Client
        .builder()
        .name("set-model-reject-client")
        .connect_with(
            ChannelTransport::<Client>::new(channel_a),
            async move |cx| {
                cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;

                let new_session = cx
                    .send_request(NewSessionRequest::new(cwd))
                    .block_task()
                    .await?;
                let models = new_session
                    .models
                    .expect("agent should advertise session model candidates");
                assert_eq!(models.available_models.len(), 1);
                assert_eq!(models.available_models[0].model_id.0.as_ref(), "alpha");

                let err = cx
                    .send_request(agent_client_protocol::schema::SetSessionModelRequest::new(
                        new_session.session_id,
                        ModelId::new("beta"),
                    ))
                    .block_task()
                    .await
                    .expect_err("beta should be rejected by configured candidate filter");

                Ok(err.message)
            },
        )
        .await
        .expect("client connection completed");

    assert!(
        client_result.contains("model not found") && client_result.contains("beta"),
        "expected set_model rejection for filtered model, got {client_result:?}"
    );

    server_handle.abort();
    let _ = server_handle.await;
}

#[tokio::test]
async fn model_candidates_fall_back_to_configured_whitelist_when_provider_list_fails() {
    let provider = Arc::new(FlakyModelProvider);
    let config = TurnConfig {
        model: "deepseek-v4-pro".to_string(),
        allowed_models: Some(vec![
            "deepseek-v4-pro".to_string(),
            "deepseek-v4-flash".to_string(),
        ]),
        ..TurnConfig::default()
    };
    let agent_core = DefaultAgentCore::builder()
        .provider(provider)
        .config(config)
        .build();
    let agent_core: Arc<dyn AgentCore> = Arc::new(agent_core);

    let (channel_a, channel_b) = Channel::duplex();
    let server_handle = tokio::spawn(serve_on(
        agent_core,
        ChannelTransport::<Agent>::new(channel_b),
    ));

    let cwd = std::env::current_dir().expect("cwd available");
    let client_result = Client
        .builder()
        .name("flaky-model-client")
        .connect_with(
            ChannelTransport::<Client>::new(channel_a),
            async move |cx| {
                cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;

                let loaded = cx
                    .send_request(NewSessionRequest::new(cwd))
                    .block_task()
                    .await?;
                let models = loaded
                    .models
                    .expect("session should still advertise configured models");
                Ok(models.available_models.len() as u64)
            },
        )
        .await
        .expect("client connection completed");

    assert_eq!(client_result, 2);

    server_handle.abort();
    let _ = server_handle.await;
}
