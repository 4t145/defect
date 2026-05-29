//! 装配进程内 tool registry。
//!
//! 跑在 agent 进程里的工具（bash / fs / fetch / search 等）一次性挂在
//! [`StaticToolRegistry`] 上；MCP 工具走 session-level [`McpToolFactory`]
//! 在 `mcp_servers` 模块里组装。

use std::sync::Arc;

use defect_agent::session::{StaticToolRegistry, ToolRegistry};
use defect_config::LoadedConfig;
use defect_tools::{BashTool, EditFileTool, FetchTool, ReadFileTool, SearchTool, WriteFileTool};

/// 按 `[tools]` 段装配进程内工具集合。
///
/// `fetch` / `search` 通过 `enabled` 字段单独控制；本地 `search` 工具
/// 与 hosted `web_search` capability 完全独立——两者可同时启用。
pub fn build_process_tools(config: &LoadedConfig) -> Arc<dyn ToolRegistry> {
    let mut builder = StaticToolRegistry::builder()
        .insert(Arc::new(BashTool::from_config(
            &config.effective.tools.bash,
        )))
        .insert(Arc::new(ReadFileTool::from_config(
            &config.effective.tools.fs,
        )))
        .insert(Arc::new(WriteFileTool::new()))
        .insert(Arc::new(EditFileTool::new()));
    if config.effective.tools.fetch.enabled {
        builder = builder.insert(Arc::new(FetchTool::from_config(
            &config.effective.tools.fetch,
        )));
    }
    if config.effective.tools.search.enabled {
        builder = builder.insert(Arc::new(SearchTool::from_config(
            &config.effective.tools.search,
        )));
    }
    Arc::new(builder.build())
}
