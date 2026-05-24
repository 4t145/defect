//! MCP 客户端适配层。
//!
//! 把外部 MCP server 暴露的工具/资源包装为 [`defect_agent`] 的 `Tool`
//! 实现，让 agent 主循环以与内置工具一致的接口调用。
