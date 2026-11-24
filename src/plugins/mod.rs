//! 服务插件：唯一能力目录、编译期装配与按服务隔离的宿主接口。
//!
//! 协议实现沿用 backend 中的传输模块；公共应用代码只通过此目录发现能力。

pub mod auth;
pub mod builtin;
pub mod contract;
pub mod control;
pub mod device_auth;
pub mod host;
pub mod registry;
pub(crate) mod service;

pub mod autoglm;
pub mod minimax_code;
mod modelscope;
pub mod monkeycode;
mod openai_compat;
pub mod qoder;
pub mod raccoon;
pub mod trae_personal;
pub mod workbuddy;

pub use contract::{PluginDescriptor, ProviderPlugin, ProviderRuntime};
pub use host::ScopedHostServices;
pub use registry::PluginRegistry;
