//! ACP（Agent Client Protocol）服务端实现。
//!
//! 桥接 [`defect_agent`] 暴露的事件流与 ACP 线上协议；不参与业务逻辑，
//! 仅做协议适配与传输（stdio / socket）。
