use std::sync::Arc;
use hands::mcp::McpHost;
use serde_json::{json, Value};
use tempfile::TempDir;
#[allow(dead_code)]
pub struct TestHarness {
    pub temp: TempDir,
    pub config_dir: TempDir,
    pub host: Arc<McpHost>,
}

#[allow(dead_code)]

impl TestHarness {
    pub fn new() -> Self {
        let temp = TempDir::new().expect("workspace tempdir");
        Self::new_with_dir(temp)
    }

    pub fn new_with_dir(temp: TempDir) -> Self {
        let config_dir = TempDir::new().expect("config tempdir");
        unsafe {
            std::env::set_var("HANDS_CONFIG_DIR", config_dir.path());
        }
        let workspace_path = dunce::canonicalize(temp.path()).unwrap_or_else(|_| temp.path().to_path_buf());
        let host = McpHost::new(workspace_path);
        Self {
            temp,
            config_dir,
            host,
        }
    }

    pub async fn rpc(&self, method: &str, params: Value) -> Value {
        let req = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params,
        });
        self.host
            .handle_rpc(req)
            .await
            .expect("response expected for request with id")
    }
}
