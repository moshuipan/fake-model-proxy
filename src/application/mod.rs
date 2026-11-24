//! 管理入口共用的应用用例；配置提交与远程操作保持独立。

pub mod access;
pub mod api_schema;
pub mod config_service;
pub mod config_types;
mod legacy_config;
pub mod management;
pub mod operation_service;
