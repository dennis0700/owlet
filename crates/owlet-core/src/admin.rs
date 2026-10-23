//! `owlet status|enable|disable`: talks to a running owlet over its admin API.

use std::{net::SocketAddr, path::Path, time::Duration};

use anyhow::{Context, bail};
use reqwest::{Client, Method, header::AUTHORIZATION};
use serde_json::Value;

use crate::config::Config;

/// Admin client bound to the `listen` address and token from the config.
pub struct AdminClient {
    client: Client,
    base: String,
    token: Option<String>,
}

impl AdminClient {
    /// Builds a client from the same config file the server uses.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let admin = AdminClient::from_config(Path::new("owlet.toml"))?;
    /// ```
    pub fn from_config(path: &Path) -> anyhow::Result<Self> {
        let cfg = Config::load(path)?;
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("build HTTP client")?;
        Ok(Self {
            client,
            base: format!("http://{}", connect_addr(cfg.listen)),
            token: cfg.token,
        })
    }

    async fn call(&self, method: Method, path: &str) -> anyhow::Result<Value> {
        let url = format!("{}{path}", self.base);
        let mut req = self.client.request(method, &url);
        if let Some(token) = &self.token {
            req = req.header(AUTHORIZATION, format!("Bearer {token}"));
        }
        let resp = req
            .send()
            .await
            .with_context(|| format!("cannot reach owlet at {} (is it running?)", self.base))?;
        let status = resp.status();
        let body = resp.text().await.context("read response")?;
        let value: Value = serde_json::from_str(&body).unwrap_or(Value::String(body));
        if !status.is_success() {
            let msg = value
                .get("error")
                .and_then(Value::as_str)
                .map_or_else(|| value.to_string(), str::to_owned);
            bail!("{status}: {msg}");
        }
        Ok(value)
    }

    /// Prints configured servers and running instances.
    pub async fn status(&self, json: bool) -> anyhow::Result<()> {
        let status = self.call(Method::GET, "/status").await?;
        if json {
            println!("{}", serde_json::to_string_pretty(&status)?);
        } else {
            print!("{}", render_status(&status));
        }
        Ok(())
    }

    /// Enables or disables a server; `persist` also rewrites the config file.
    pub async fn toggle(&self, server: &str, enabled: bool, persist: bool) -> anyhow::Result<()> {
        let action = if enabled { "enable" } else { "disable" };
        let path = format!(
            "/admin/servers/{}/{action}?persist={persist}",
            encode_segment(server)
        );
        let resp = self.call(Method::POST, &path).await?;
        let changed = resp.get("changed").and_then(Value::as_bool) == Some(true);
        let verb = if enabled { "enabled" } else { "disabled" };
        let mut line = if changed {
            format!("{server}: {verb}")
        } else {
            format!("{server}: already {verb}")
        };
        if persist {
            line.push_str(" (saved to config)");
        }
        println!("{line}");
        Ok(())
    }
}

/// A wildcard listen address is not connectable; use loopback instead.
fn connect_addr(listen: SocketAddr) -> SocketAddr {
    let mut addr = listen;
    if addr.ip().is_unspecified() {
        addr.set_ip(match addr {
            SocketAddr::V4(_) => [127, 0, 0, 1].into(),
            SocketAddr::V6(_) => std::net::Ipv6Addr::LOCALHOST.into(),
        });
    }
    addr
}

fn encode_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn render_status(status: &Value) -> String {
    let empty = Vec::new();
    let servers = status["servers"].as_array().unwrap_or(&empty);
    let instances = status["instances"].as_array().unwrap_or(&empty);
    let width = servers
        .iter()
        .filter_map(|s| s["name"].as_str())
        .map(str::len)
        .max()
        .unwrap_or(4)
        .max(4);

    let mut out = format!(
        "{:<width$}  {:<8}  {:<9}  {:<7}  {}\n",
        "NAME", "STATE", "TRANSPORT", "SCOPE", "INSTANCES"
    );
    for s in servers {
        let name = s["name"].as_str().unwrap_or_default();
        let state = if s["enabled"].as_bool() == Some(true) {
            "enabled"
        } else {
            "disabled"
        };
        let mine: Vec<&Value> = instances
            .iter()
            .filter(|i| i["server"].as_str() == Some(name))
            .collect();
        let running = mine.iter().filter(|i| i["running"] == true).count();
        let summary = if mine.is_empty() {
            "-".to_owned()
        } else {
            let pids: Vec<String> = mine
                .iter()
                .filter_map(|i| i["pid"].as_u64())
                .map(|p| p.to_string())
                .collect();
            let mut s = format!("{running}/{} running", mine.len());
            if !pids.is_empty() {
                s.push_str(&format!(" (pid {})", pids.join(",")));
            }
            s
        };
        out.push_str(&format!(
            "{:<width$}  {:<8}  {:<9}  {:<7}  {}\n",
            name,
            state,
            s["transport"].as_str().unwrap_or("?"),
            s["scope"].as_str().unwrap_or("?"),
            summary
        ));
    }
    out.push_str(&format!(
        "sessions: {}\n",
        status["sessions"].as_u64().unwrap_or(0)
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn renders_table() {
        let status = json!({
            "servers": [
                {"name": "fetch", "enabled": true, "transport": "stdio", "scope": "shared"},
                {"name": "off", "enabled": false, "transport": "http", "scope": "session"},
            ],
            "instances": [{"server": "fetch", "running": true, "pid": 42}],
            "sessions": 3,
        });
        let out = render_status(&status);
        assert!(out.contains("fetch  enabled   stdio      shared   1/1 running (pid 42)"));
        assert!(out.contains("off    disabled  http       session  -"));
        assert!(out.ends_with("sessions: 3\n"));
    }

    #[test]
    fn addresses_and_paths() {
        let any: SocketAddr = "0.0.0.0:8808".parse().expect("addr");
        assert_eq!(connect_addr(any).to_string(), "127.0.0.1:8808");
        let v6: SocketAddr = "[::]:1".parse().expect("addr");
        assert_eq!(connect_addr(v6).to_string(), "[::1]:1");
        assert_eq!(encode_segment("a b/c"), "a%20b%2Fc");
    }
}
