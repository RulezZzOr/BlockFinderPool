use anyhow::{anyhow, Context};
use reqwest::Client;
use std::time::Duration;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::json;

#[derive(Clone)]
pub struct RpcClient {
    url:            String,
    user:           String,
    pass:           String,
    /// Fast client: 8s timeout for all normal RPC calls (submit, getinfo, etc.)
    client:         Client,
    /// Slow client: 130s timeout exclusively for getblocktemplate with longpollid.
    /// Bitcoin Core holds the connection open for ~90s until the template changes.
    /// Using the fast client for longpoll causes "RPC request failed" every 8s.
    client_longpoll: Client,
}

#[derive(Debug, Deserialize)]
struct RpcResponse<T> {
    result: Option<T>,
    error: Option<RpcError>,
}

#[derive(Debug, Deserialize)]
struct RpcError {
    code: i64,
    message: String,
}

// ── Unit tests ────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;

    // Helpers: deserialise the JSON the way call_with_client / call_optional do.
    fn decode_response<T: serde::de::DeserializeOwned>(json: &str) -> anyhow::Result<T> {
        let body: RpcResponse<T> = serde_json::from_str(json).expect("parse RpcResponse");
        if let Some(err) = body.error {
            return Err(anyhow::anyhow!("RPC error: {} ({})", err.message, err.code));
        }
        body.result.ok_or_else(|| anyhow::anyhow!("empty result"))
    }

    fn decode_optional<T: serde::de::DeserializeOwned>(json: &str) -> anyhow::Result<Option<T>> {
        let body: RpcResponse<T> = serde_json::from_str(json).expect("parse RpcResponse");
        if let Some(err) = body.error {
            return Err(anyhow::anyhow!("RPC error: {} ({})", err.message, err.code));
        }
        Ok(body.result)
    }

    /// submitblock success: Bitcoin Core returns {"result": null, "error": null}.
    /// call::<Option<String>> (broken): null result → ok_or_else → Err.
    /// call_optional::<String> (fixed):  null result → Ok(None).
    #[test]
    fn test_submitblock_null_result_is_ok_with_call_optional() {
        let json = r#"{"result":null,"error":null,"id":"BlockFinder"}"#;
        // Old behaviour: call::<Option<String>> → body.result is None → Err
        let old: anyhow::Result<Option<String>> = decode_response(json);
        assert!(old.is_err(), "call::<Option<String>> must fail on null (demonstrates the bug)");

        // New behaviour: call_optional::<String> → Ok(None)
        let new: anyhow::Result<Option<String>> = decode_optional(json);
        assert!(new.is_ok(),  "call_optional::<String> must succeed on null");
        assert!(new.unwrap().is_none(), "null result must be Ok(None)");
    }

    /// submitblock rejection: Bitcoin Core returns {"result": "duplicate", ...}.
    /// Both approaches should return the reason string.
    #[test]
    fn test_submitblock_rejection_reason_parsed() {
        let json = r#"{"result":"duplicate","error":null,"id":"BlockFinder"}"#;
        let result: anyhow::Result<Option<String>> = decode_optional(json);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), Some("duplicate".to_string()));
    }

    /// submitblock with a different rejection reason.
    #[test]
    fn test_submitblock_rejected_reason() {
        let json = r#"{"result":"rejected","error":null,"id":"BlockFinder"}"#;
        let result: anyhow::Result<Option<String>> = decode_optional(json);
        assert_eq!(result.unwrap(), Some("rejected".to_string()));
    }

    /// RPC error body (e.g. invalid params): returns Err regardless of approach.
    #[test]
    fn test_rpc_error_body_returns_err() {
        let json = r#"{"result":null,"error":{"code":-25,"message":"Block not found"},"id":"BlockFinder"}"#;
        let result: anyhow::Result<Option<String>> = decode_optional(json);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("Block not found"), "error message must propagate: {msg}");
        assert!(msg.contains("-25"), "error code must propagate: {msg}");
    }

    /// Successful non-null call (e.g. getblockheader) still works normally.
    #[test]
    fn test_normal_call_with_value_result() {
        let json = r#"{"result":{"hash":"abc123"},"error":null,"id":"BlockFinder"}"#;
        let result: anyhow::Result<serde_json::Value> = decode_response(json);
        assert!(result.is_ok());
        assert_eq!(result.unwrap()["hash"], "abc123");
    }
}

impl RpcClient {
    pub fn new(url: String, user: String, pass: String) -> Self {
        // Fast client: tight timeouts to fail quickly on network issues.
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(8))
            .tcp_nodelay(true)
            .pool_max_idle_per_host(8)
            .build()
            .expect("build reqwest client");

        // Longpoll client: 130s response timeout (Core holds ~90s, +40s buffer).
        // connect_timeout stays short so a dead Core is detected quickly.
        let client_longpoll = Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(130))
            .tcp_nodelay(true)
            .pool_max_idle_per_host(2)
            .build()
            .expect("build longpoll reqwest client");

        Self { url, user, pass, client, client_longpoll }
    }

    /// Normal RPC call — 8s timeout. Use for everything except longpoll GBT.
    pub async fn call<T: DeserializeOwned>(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> anyhow::Result<T> {
        self.call_with_client(&self.client, method, params).await
    }

    /// RPC call where a `null` result is valid and distinct from an error.
    ///
    /// Bitcoin Core's `submitblock` returns:
    ///   - `null`        → block accepted (success)
    ///   - `"duplicate"` → already known (treat as success)
    ///   - `"rejected"`  → consensus/validation failure
    ///
    /// Using `call::<Option<String>>` does NOT work for this: serde maps both a
    /// missing field and a JSON `null` to `None`, so `ok_or_else` turns every
    /// successful `submitblock` into an error ("empty result").
    ///
    /// This method returns `Ok(None)` for null and `Ok(Some(T))` for a real value.
    pub async fn call_optional<T: DeserializeOwned>(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> anyhow::Result<Option<T>> {
        let payload = serde_json::json!({
            "jsonrpc": "1.0",
            "id": "BlockFinder",
            "method": method,
            "params": params,
        });

        let response = self.client
            .post(&self.url)
            .basic_auth(&self.user, Some(&self.pass))
            .json(&payload)
            .send()
            .await
            .context("RPC request failed")?;

        let body = response.json::<RpcResponse<T>>().await?;

        if let Some(err) = body.error {
            return Err(anyhow!("RPC error {method}: {} ({})", err.message, err.code));
        }

        // `body.result` is `None` for a JSON null and `Some(T)` for a real value.
        // Returning Ok(None) here correctly signals "null = success" to the caller.
        Ok(body.result)
    }

    /// Longpoll-aware GBT call — 130s timeout.
    /// Blocks until Bitcoin Core returns a changed template (up to ~90s).
    /// Eliminates the "longpoll GBT call failed" WARN spam.
    pub async fn call_longpoll<T: DeserializeOwned>(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> anyhow::Result<T> {
        self.call_with_client(&self.client_longpoll, method, params).await
    }

    async fn call_with_client<T: DeserializeOwned>(
        &self,
        client: &Client,
        method: &str,
        params: serde_json::Value,
    ) -> anyhow::Result<T> {
        let payload = json!({
            "jsonrpc": "1.0",
            "id": "BlockFinder",
            "method": method,
            "params": params,
        });

        let response = client
            .post(&self.url)
            .basic_auth(&self.user, Some(&self.pass))
            .json(&payload)
            .send()
            .await
            .context("RPC request failed")?;

        let status = response.status();
        let body = response.json::<RpcResponse<T>>().await?;

        if let Some(err) = body.error {
            return Err(anyhow!("RPC error {method}: {} ({})", err.message, err.code));
        }

        body.result.ok_or_else(|| anyhow!("RPC {method} returned empty result (status {status})"))
    }
}
