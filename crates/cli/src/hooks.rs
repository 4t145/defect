//! 把 `defect-config` 的 hook 配置翻译成 agent 的 [`DefaultHookEngine`]。
//!
//! 详见 `docs/internal/hooks.md` §5.3 / §10——agent crate 不依赖 config，
//! 翻译动作放 CLI 装配期；这里也是 fail-fast 报"未知 builtin 名"的位置。
//!
//! v0 仅支持 `HookHandlerSpec::Builtin`；`Command` / `Prompt` 走
//! [`HookError::Configuration`] 占位，真正的子进程 / LLM handler 实现
//! 留在 hooks Phase E / F。

use std::sync::Arc;
use std::time::Duration;

use defect_agent::hooks::{
    DefaultHookEngine, HandlerEntry, HandlerTable, HookEventKind, HookMatcher as AgentHookMatcher,
};
use defect_agent::hooks::builtin::BuiltinRegistry;
use defect_config::{
    HookCommandSpec, HookEntry, HookHandlerSpec, HookMatcher as ConfigHookMatcher, HooksConfig,
};

/// 装配错误：v0 阶段大多是"配置引用了未实现的 handler 形态"或"builtin name
/// 拼错"。CLI 入口直接 `anyhow::bail!` 即可。
#[derive(Debug, thiserror::Error)]
pub enum HookEngineBuildError {
    #[error("unknown builtin hook handler `{name}` (available: {available})")]
    UnknownBuiltin { name: String, available: String },

    #[error("hook handler form `{form}` is not yet implemented (Phase E/F)")]
    Unimplemented { form: &'static str },
}

/// 用 `[hooks]` 段 + builtin 注册表构造一个 [`DefaultHookEngine`]。
///
/// - 空 `HooksConfig` → 返回空 engine（caller 可以选择直接走 `NoopHookEngine`）
/// - `Builtin { name }` → registry.lookup；找不到时 fail-fast，避免用户在 turn
///   跑到一半才发现拼错（hooks.md §4.1）
/// - `Command(_)` / `Prompt(_)` → 返回 [`HookEngineBuildError::Unimplemented`]，
///   让 CLI 在启动期报错而不是默默 noop——后者会让"我配了个 hook 但它没生效"
///   的状态变成隐藏 bug
pub fn build_hook_engine(
    hooks: &HooksConfig,
    builtins: &BuiltinRegistry,
) -> Result<DefaultHookEngine, HookEngineBuildError> {
    let mut table = HandlerTable::empty();

    push_bucket(
        &mut table,
        HookEventKind::SessionStart,
        &hooks.session_start,
        builtins,
    )?;
    push_bucket(
        &mut table,
        HookEventKind::UserPromptSubmit,
        &hooks.user_prompt_submit,
        builtins,
    )?;
    push_bucket(
        &mut table,
        HookEventKind::PreToolUse,
        &hooks.pre_tool_use,
        builtins,
    )?;
    push_bucket(
        &mut table,
        HookEventKind::PostToolUse,
        &hooks.post_tool_use,
        builtins,
    )?;
    push_bucket(
        &mut table,
        HookEventKind::PostToolUseFailure,
        &hooks.post_tool_use_failure,
        builtins,
    )?;

    let engine = DefaultHookEngine::new();
    engine.reload(table);
    Ok(engine)
}

fn push_bucket(
    table: &mut HandlerTable,
    kind: HookEventKind,
    entries: &[HookEntry],
    builtins: &BuiltinRegistry,
) -> Result<(), HookEngineBuildError> {
    for entry in entries {
        let matcher = translate_matcher(&entry.matcher);
        let (handler, timeout) = match &entry.handler {
            HookHandlerSpec::Builtin { name } => {
                let handler = builtins.lookup(name).ok_or_else(|| {
                    let available = builtins
                        .names()
                        .collect::<Vec<_>>()
                        .join(", ");
                    HookEngineBuildError::UnknownBuiltin {
                        name: name.clone(),
                        available,
                    }
                })?;
                (handler, None)
            }
            HookHandlerSpec::Command(spec) => {
                let _timeout = command_timeout(spec);
                return Err(HookEngineBuildError::Unimplemented { form: "command" });
            }
            HookHandlerSpec::Prompt(_spec) => {
                return Err(HookEngineBuildError::Unimplemented { form: "prompt" });
            }
            // `HookHandlerSpec` 是 non_exhaustive 的；CLI 装配阶段不认识的形态
            // 直接 fail-fast，避免新增 handler 类型后默默 noop。
            other => {
                let _ = other;
                return Err(HookEngineBuildError::Unimplemented {
                    form: "<unrecognized>",
                });
            }
        };
        let mut hook = HandlerEntry::new(matcher, handler);
        if let Some(t) = timeout {
            hook = hook.with_timeout(t);
        }
        table.push(kind, hook);
    }
    Ok(())
}

fn translate_matcher(m: &ConfigHookMatcher) -> AgentHookMatcher {
    let mut out = AgentHookMatcher::default();
    out.tool = m.tool.clone();
    out.tool_glob = m.tool_glob.clone();
    out.safety = m.safety.clone();
    out
}

/// 留给后续 Phase E：从 `HookCommandSpec` 上抽 `timeout_sec` 字段。
fn command_timeout(spec: &HookCommandSpec) -> Option<Duration> {
    let secs = match spec {
        HookCommandSpec::Argv { timeout_sec, .. } | HookCommandSpec::Shell { timeout_sec, .. } => {
            *timeout_sec
        }
        // non_exhaustive: 未知形态保守不带 timeout，由引擎默认值兜底。
        _ => None,
    };
    secs.map(Duration::from_secs)
}

/// 在 [`Arc`] 里封装一份 hook engine——session/turn 主循环统一拿
/// `Arc<dyn HookEngine>`。`HooksConfig::is_empty` 时用
/// [`defect_agent::hooks::NoopHookEngine`] 走零开销路径。
pub fn build_engine_arc(
    hooks: &HooksConfig,
    builtins: &BuiltinRegistry,
) -> Result<Arc<dyn defect_agent::hooks::HookEngine>, HookEngineBuildError> {
    if hooks.is_empty() {
        return Ok(Arc::new(defect_agent::hooks::NoopHookEngine));
    }
    let engine = build_hook_engine(hooks, builtins)?;
    Ok(Arc::new(engine))
}

#[cfg(test)]
mod test {
    use super::*;
    use defect_agent::tool::SafetyClass;
    use defect_config::ConfigSource;

    #[test]
    fn empty_config_yields_noop_engine() {
        let builtins = BuiltinRegistry::defaults();
        let arc = build_engine_arc(&HooksConfig::default(), &builtins).expect("ok");
        // 不能直接断言类型，但断言 fire 返回 Pass 即可
        let session_id = agent_client_protocol::schema::SessionId::new("s");
        let cwd = std::path::Path::new("/");
        let ctx = defect_agent::hooks::HookCtx::new(
            &session_id,
            cwd,
            tokio_util::sync::CancellationToken::new(),
        );
        let ev = defect_agent::hooks::HookEvent::SessionStart {
            source: defect_agent::hooks::SessionSource::New,
            cwd,
        };
        let outcome = futures::executor::block_on(arc.fire(ev, ctx));
        assert!(outcome.block.is_none());
        assert!(outcome.append.is_empty());
    }

    #[test]
    fn unknown_builtin_fails_fast() {
        let builtins = BuiltinRegistry::defaults();
        let mut hooks = HooksConfig::default();
        hooks.session_start.push(HookEntry {
            matcher: ConfigHookMatcher::default(),
            handler: HookHandlerSpec::Builtin {
                name: "does-not-exist".into(),
            },
            source: ConfigSource::User,
        });
        let err = match build_engine_arc(&hooks, &builtins) {
            Ok(_) => panic!("should fail"),
            Err(e) => e,
        };
        assert!(matches!(
            err,
            HookEngineBuildError::UnknownBuiltin { ref name, .. } if name == "does-not-exist"
        ));
    }

    #[test]
    fn known_builtin_loads() {
        let builtins = BuiltinRegistry::defaults();
        let mut hooks = HooksConfig::default();
        hooks.pre_tool_use.push(HookEntry {
            matcher: ConfigHookMatcher {
                tool: Some("login".into()),
                ..Default::default()
            },
            handler: HookHandlerSpec::Builtin {
                name: "redact-secrets".into(),
            },
            source: ConfigSource::Project,
        });
        let _arc = build_engine_arc(&hooks, &builtins).expect("ok");
    }

    #[test]
    fn command_handler_unimplemented() {
        use std::collections::BTreeMap;
        let builtins = BuiltinRegistry::defaults();
        let mut hooks = HooksConfig::default();
        hooks.pre_tool_use.push(HookEntry {
            matcher: ConfigHookMatcher::default(),
            handler: HookHandlerSpec::Command(HookCommandSpec::Argv {
                argv: vec!["true".into()],
                argv_windows: None,
                cwd: None,
                env: BTreeMap::new(),
                timeout_sec: Some(5),
            }),
            source: ConfigSource::User,
        });
        let err = match build_engine_arc(&hooks, &builtins) {
            Ok(_) => panic!("should fail"),
            Err(e) => e,
        };
        assert!(matches!(
            err,
            HookEngineBuildError::Unimplemented { form: "command" }
        ));
    }

    #[test]
    fn matcher_translation_preserves_fields() {
        let cm = ConfigHookMatcher {
            tool: Some("bash".into()),
            tool_glob: Some("mcp.*".into()),
            safety: vec![SafetyClass::Destructive, SafetyClass::Network],
        };
        let am = translate_matcher(&cm);
        assert_eq!(am.tool.as_deref(), Some("bash"));
        assert_eq!(am.tool_glob.as_deref(), Some("mcp.*"));
        assert_eq!(am.safety.len(), 2);
    }
}
