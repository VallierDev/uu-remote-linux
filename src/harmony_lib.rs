//! HarmonyOS static library linked into the application N-API module.
#![cfg(target_env = "ohos")]
#![allow(dead_code)]
// Shared exported i18n macros are defined through the included server root.
#![allow(macro_expanded_macro_exports_accessed_by_absolute_paths)]
include!("main.rs");
