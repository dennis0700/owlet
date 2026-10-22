//! Server-form parsing and validation, ported from the Tauri UI's `form.ts`.
//!
//! The functions here turn the raw text of the add/edit server dialog into a
//! validated [`ServerConfig`], and back, reproducing every rule and every
//! user-facing error message of the original TypeScript verbatim.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Result, bail};
use owlet::{RemoteTransport, Scope, ServerConfig};

/// Which of the three mutually exclusive server shapes the form describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Command,
    Url,
    Wasm,
}

/// The raw text of every field in the server dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormValues {
    pub name: String,
    pub kind: Kind,
    pub enabled: bool,
    pub command: String,
    pub args: String,
    pub env: String,
    pub cwd: String,
    pub wasm: String,
    pub inherit_env: String,
    pub dirs: String,
    pub url: String,
    /// `None` means "default (Streamable HTTP)".
    pub transport: Option<RemoteTransport>,
    pub headers: String,
    pub scope: Scope,
    pub idle_timeout: String,
    pub cache_lists: bool,
}

impl Default for FormValues {
    fn default() -> Self {
        Self {
            name: String::new(),
            kind: Kind::Command,
            enabled: true,
            command: String::new(),
            args: String::new(),
            env: String::new(),
            cwd: String::new(),
            wasm: String::new(),
            inherit_env: String::new(),
            dirs: String::new(),
            url: String::new(),
            transport: None,
            headers: String::new(),
            scope: Scope::Shared,
            idle_timeout: String::new(),
            cache_lists: true,
        }
    }
}

/// Splits text into trimmed, non-empty lines (mirrors `form.ts`'s `lines`).
fn lines(text: &str) -> Vec<String> {
    text.lines()
        .map(|l| l.trim().to_owned())
        .filter(|l| !l.is_empty())
        .collect()
}

/// Joins a map back into `KEY<sep>VALUE` lines, sorted for stability.
fn join_pairs(map: &HashMap<String, String>, sep: &str) -> String {
    let mut pairs: Vec<_> = map.iter().collect();
    pairs.sort();
    pairs
        .into_iter()
        .map(|(k, v)| format!("{k}{sep}{v}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Parses `KEY<sep>VALUE` lines into a map; `what` names the field in errors.
///
/// Matches `form.ts`'s `parsePairs`: the separator is found by its first
/// occurrence, the key is everything before it (trimmed) and must be
/// non-empty, and the value is everything after it (trimmed).
fn parse_pairs(text: &str, sep: char, what: &str) -> Result<HashMap<String, String>> {
    let mut out = HashMap::new();
    for line in lines(text) {
        let key = match line.find(sep) {
            Some(i) => line[..i].trim().to_owned(),
            None => String::new(),
        };
        if key.is_empty() {
            bail!("{what}: expected \"KEY{sep}VALUE\", got \"{line}\"");
        }
        let value = match line.find(sep) {
            Some(i) => line[i + sep.len_utf8()..].trim().to_owned(),
            None => String::new(),
        };
        out.insert(key, value);
    }
    Ok(out)
}

/// Converts an existing entry to form field text (mirrors `toForm`).
///
/// # Examples
///
/// ```ignore
/// let values = to_form("fetch", &server);
/// ```
pub fn to_form(name: &str, s: &ServerConfig) -> FormValues {
    let kind = if s.wasm.is_some() {
        Kind::Wasm
    } else if s.url.is_some() {
        Kind::Url
    } else {
        Kind::Command
    };
    FormValues {
        name: name.to_owned(),
        kind,
        enabled: s.enabled,
        command: s.command.clone().unwrap_or_default(),
        args: s.args.join("\n"),
        env: join_pairs(&s.env, "="),
        cwd: s
            .cwd
            .as_deref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
        wasm: s
            .wasm
            .as_deref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
        inherit_env: s.inherit_env.join("\n"),
        dirs: s
            .dirs
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("\n"),
        url: s.url.clone().unwrap_or_default(),
        transport: s.transport,
        headers: join_pairs(&s.headers, ": "),
        scope: s.scope,
        idle_timeout: s.idle_timeout.map(|v| v.to_string()).unwrap_or_default(),
        cache_lists: s.cache_lists,
    }
}

/// Converts form input into a validated config entry (mirrors `fromForm`).
///
/// Returns the trimmed name and the server, or an error whose message is the
/// exact user-facing string from the original form.
///
/// # Examples
///
/// ```ignore
/// let (name, server) = from_form(&values)?;
/// ```
pub fn from_form(f: &FormValues) -> Result<(String, ServerConfig)> {
    let name = f.name.trim().to_owned();
    if name.is_empty() {
        bail!("Name is required");
    }
    if name.contains('/') {
        bail!("Name must not contain \"/\"");
    }

    let idle_timeout = if f.idle_timeout.trim().is_empty() {
        None
    } else {
        match f.idle_timeout.trim().parse::<u64>() {
            Ok(v) => Some(v),
            Err(_) => bail!("Idle timeout must be a non-negative integer (seconds)"),
        }
    };

    let mut server = ServerConfig {
        enabled: f.enabled,
        command: None,
        wasm: None,
        url: None,
        transport: None,
        headers: HashMap::new(),
        args: Vec::new(),
        env: HashMap::new(),
        inherit_env: Vec::new(),
        dirs: Vec::new(),
        cwd: None,
        scope: f.scope,
        idle_timeout,
        cache_lists: f.cache_lists,
    };

    match f.kind {
        Kind::Command => {
            if f.command.trim().is_empty() {
                bail!("Command is required");
            }
            server.command = Some(f.command.trim().to_owned());
            server.args = lines(&f.args);
            server.env = parse_pairs(&f.env, '=', "Environment")?;
            server.cwd = non_empty(&f.cwd).map(PathBuf::from);
        }
        Kind::Wasm => {
            if f.scope == Scope::Project {
                bail!("Scope \"project\" needs a local process; use a command server");
            }
            if f.wasm.trim().is_empty() {
                bail!("WASM path is required");
            }
            server.wasm = Some(PathBuf::from(f.wasm.trim()));
            server.args = lines(&f.args);
            server.env = parse_pairs(&f.env, '=', "Environment")?;
            server.inherit_env = lines(&f.inherit_env);
            server.dirs = lines(&f.dirs).into_iter().map(PathBuf::from).collect();
        }
        Kind::Url => {
            if f.scope == Scope::Project {
                bail!("Scope \"project\" needs a local process; use a command server");
            }
            if f.url.trim().is_empty() {
                bail!("URL is required");
            }
            server.url = Some(f.url.trim().to_owned());
            server.transport = f.transport;
            server.headers = parse_pairs(&f.headers, ':', "Headers")?;
        }
    }

    Ok((name, server))
}

fn non_empty(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form() -> FormValues {
        FormValues::default()
    }

    #[test]
    fn name_is_required() {
        let f = FormValues {
            command: "x".into(),
            ..form()
        };
        assert_eq!(from_form(&f).unwrap_err().to_string(), "Name is required");
    }

    #[test]
    fn name_must_not_contain_slash() {
        let f = FormValues {
            name: "a/b".into(),
            command: "x".into(),
            ..form()
        };
        assert_eq!(
            from_form(&f).unwrap_err().to_string(),
            "Name must not contain \"/\""
        );
    }

    #[test]
    fn idle_timeout_must_be_non_negative_integer() {
        let f = FormValues {
            name: "a".into(),
            command: "x".into(),
            idle_timeout: "-1".into(),
            ..form()
        };
        assert_eq!(
            from_form(&f).unwrap_err().to_string(),
            "Idle timeout must be a non-negative integer (seconds)"
        );
        let f = FormValues {
            name: "a".into(),
            command: "x".into(),
            idle_timeout: "1.5".into(),
            ..form()
        };
        assert!(from_form(&f).is_err());
        // Blank stays None, a valid integer parses.
        let f = FormValues {
            name: "a".into(),
            command: "x".into(),
            idle_timeout: "  ".into(),
            ..form()
        };
        assert_eq!(from_form(&f).expect("ok").1.idle_timeout, None);
        let f = FormValues {
            name: "a".into(),
            command: "x".into(),
            idle_timeout: "30".into(),
            ..form()
        };
        assert_eq!(from_form(&f).expect("ok").1.idle_timeout, Some(30));
    }

    #[test]
    fn command_is_required() {
        let f = FormValues {
            name: "a".into(),
            kind: Kind::Command,
            ..form()
        };
        assert_eq!(from_form(&f).unwrap_err().to_string(), "Command is required");
    }

    #[test]
    fn command_parses_args_env_cwd() {
        let f = FormValues {
            name: "a".into(),
            kind: Kind::Command,
            command: "uvx".into(),
            args: "mcp-server-fetch\n\n--flag\n".into(),
            env: "K=v\nFOO=bar baz".into(),
            cwd: " /tmp ".into(),
            ..form()
        };
        let (name, s) = from_form(&f).expect("ok");
        assert_eq!(name, "a");
        assert_eq!(s.command.as_deref(), Some("uvx"));
        assert_eq!(s.args, vec!["mcp-server-fetch", "--flag"]);
        assert_eq!(s.env["K"], "v");
        assert_eq!(s.env["FOO"], "bar baz");
        assert_eq!(s.cwd, Some(PathBuf::from("/tmp")));
    }

    #[test]
    fn env_parse_error_message() {
        let f = FormValues {
            name: "a".into(),
            kind: Kind::Command,
            command: "x".into(),
            env: "NOEQUALS".into(),
            ..form()
        };
        assert_eq!(
            from_form(&f).unwrap_err().to_string(),
            "Environment: expected \"KEY=VALUE\", got \"NOEQUALS\""
        );
    }

    #[test]
    fn url_is_required_and_project_scope_rejected() {
        let f = FormValues {
            name: "a".into(),
            kind: Kind::Url,
            ..form()
        };
        assert_eq!(from_form(&f).unwrap_err().to_string(), "URL is required");

        let f = FormValues {
            name: "a".into(),
            kind: Kind::Url,
            url: "https://h/mcp".into(),
            scope: Scope::Project,
            ..form()
        };
        assert_eq!(
            from_form(&f).unwrap_err().to_string(),
            "Scope \"project\" needs a local process; use a command server"
        );
    }

    #[test]
    fn url_parses_transport_and_headers() {
        let f = FormValues {
            name: "a".into(),
            kind: Kind::Url,
            url: " https://h/mcp ".into(),
            transport: Some(RemoteTransport::Sse),
            headers: "Authorization: Bearer x\nX-Trace: 1".into(),
            ..form()
        };
        let (_, s) = from_form(&f).expect("ok");
        assert_eq!(s.url.as_deref(), Some("https://h/mcp"));
        assert_eq!(s.transport, Some(RemoteTransport::Sse));
        assert_eq!(s.headers["Authorization"], "Bearer x");
        assert_eq!(s.headers["X-Trace"], "1");
    }

    #[test]
    fn headers_parse_error_message() {
        let f = FormValues {
            name: "a".into(),
            kind: Kind::Url,
            url: "https://h/mcp".into(),
            headers: "nocolon".into(),
            ..form()
        };
        assert_eq!(
            from_form(&f).unwrap_err().to_string(),
            "Headers: expected \"KEY:VALUE\", got \"nocolon\""
        );
    }

    #[test]
    fn wasm_requires_path_and_rejects_project_scope() {
        let f = FormValues {
            name: "a".into(),
            kind: Kind::Wasm,
            ..form()
        };
        assert_eq!(from_form(&f).unwrap_err().to_string(), "WASM path is required");

        let f = FormValues {
            name: "a".into(),
            kind: Kind::Wasm,
            wasm: "/p.wasm".into(),
            scope: Scope::Project,
            ..form()
        };
        assert_eq!(
            from_form(&f).unwrap_err().to_string(),
            "Scope \"project\" needs a local process; use a command server"
        );
    }

    #[test]
    fn wasm_parses_capabilities() {
        let f = FormValues {
            name: "aws".into(),
            kind: Kind::Wasm,
            wasm: "/p.wasm".into(),
            args: "https://h/mcp".into(),
            env: "AWS_PROFILE=dev".into(),
            inherit_env: "AWS_*\nHOME".into(),
            dirs: "~/.aws\n/etc/x".into(),
            ..form()
        };
        let (_, s) = from_form(&f).expect("ok");
        assert_eq!(s.wasm, Some(PathBuf::from("/p.wasm")));
        assert_eq!(s.args, vec!["https://h/mcp"]);
        assert_eq!(s.env["AWS_PROFILE"], "dev");
        assert_eq!(s.inherit_env, vec!["AWS_*", "HOME"]);
        assert_eq!(s.dirs, vec![PathBuf::from("~/.aws"), PathBuf::from("/etc/x")]);
    }

    #[test]
    fn round_trips_through_to_form() {
        let original = FormValues {
            name: "aws".into(),
            kind: Kind::Wasm,
            wasm: "/p.wasm".into(),
            args: "https://h/mcp".into(),
            env: "K=v".into(),
            inherit_env: "AWS_*".into(),
            dirs: "~/.aws".into(),
            idle_timeout: "30".into(),
            cache_lists: false,
            enabled: false,
            ..form()
        };
        let (name, server) = from_form(&original).expect("ok");
        let back = to_form(&name, &server);
        // The parsed-and-reserialized form matches the normalized input.
        assert_eq!(back.kind, Kind::Wasm);
        assert_eq!(back.wasm, "/p.wasm");
        assert_eq!(back.args, "https://h/mcp");
        assert_eq!(back.env, "K=v");
        assert_eq!(back.inherit_env, "AWS_*");
        assert_eq!(back.dirs, "~/.aws");
        assert_eq!(back.idle_timeout, "30");
        assert!(!back.cache_lists);
        assert!(!back.enabled);
    }
}
