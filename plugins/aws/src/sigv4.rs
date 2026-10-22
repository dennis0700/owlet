//! AWS Signature Version 4 signing for the proxied Streamable HTTP endpoint.
//!
//! Reimplements the part of `mcp-proxy-for-aws` the plugin needs: resolve
//! credentials from the environment or the shared profile files and sign each
//! outgoing request.
//!
//! Supported sources, resolved like botocore: environment keys, static profile
//! keys (`aws_access_key_id` / `aws_secret_access_key` / `aws_session_token`),
//! `role_arn` with `source_profile` (chains allowed) or
//! `credential_source = Environment` (STS `AssumeRole`), `role_arn` with
//! `web_identity_token_file` (STS `AssumeRoleWithWebIdentity`) and IAM Identity
//! Center profiles (`sso_session` or legacy `sso_start_url`) using the token
//! cached by `aws sso login`. STS and the SSO portal are called through owlet's
//! http import; the temporary credentials are cached until shortly before they
//! expire. `credential_process` and `mfa_serial` are rejected: a WASI plugin
//! cannot run processes or prompt for an MFA code.

use std::{collections::BTreeMap, path::PathBuf};

use anyhow::{Context, anyhow, bail};
use hmac::{Hmac, KeyInit, Mac};
use serde_json::Value;
use sha2::{Digest, Sha256};
use url::{Url, form_urlencoded};

use crate::owlet::plugin::http::Response;

type HmacSha256 = Hmac<Sha256>;

/// Looks up an environment variable; empty values count as unset.
pub type Env<'a> = &'a dyn Fn(&str) -> Option<String>;

/// Sends `(method, url, headers, body)`: owlet's http import, or a test mock.
pub type Http<'a> =
    &'a mut dyn FnMut(&str, &str, &[(String, String)], &[u8]) -> Result<Response, String>;

/// Temporary credentials by profile name, with their expiration (unix seconds).
pub type Cache = BTreeMap<String, (Credentials, u64)>;

/// Profile keys that need credential providers a WASI plugin cannot implement.
const UNSUPPORTED: &[&str] = &["credential_process"];

/// Cached temporary credentials are refreshed this many seconds before expiry.
const REFRESH_MARGIN: u64 = 300;
/// STS query API version.
const STS_VERSION: &str = "2011-06-15";
const FORM: &str = "application/x-www-form-urlencoded";
/// Longest slice of an error body quoted in an error.
const MAX_ERROR_BODY: usize = 512;

/// Infers `(service, region)` from an endpoint host of the form
/// `service.region.api.aws` or `*.bedrock-agentcore.<region>.amazonaws.com`.
pub fn parse_endpoint(url: &Url) -> (Option<String>, Option<String>) {
    let Some(host) = url.host_str() else {
        return (None, None);
    };
    let labels: Vec<&str> = host.split('.').collect();
    match labels.as_slice() {
        // `*.bedrock-agentcore.<region>.amazonaws.com` (gateway and runtime).
        [.., "bedrock-agentcore", region, "amazonaws", "com"] => (
            Some("bedrock-agentcore".to_owned()),
            Some((*region).to_owned()),
        ),
        // `service.region.api.aws`.
        [service, region, "api", "aws"] => {
            (Some((*service).to_owned()), Some((*region).to_owned()))
        }
        // Fallback: first label is the service, region unknown.
        [service, ..] if !service.is_empty() => (Some((*service).to_owned()), None),
        _ => (None, None),
    }
}

/// AWS credentials. Static ones are read fresh for every request; temporary
/// ones (role, web identity, SSO) are cached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
}

/// Resolves credentials like `boto3.Session(profile_name=...)`: a named
/// profile (`profile`, else `$AWS_PROFILE`) reads only the profile files;
/// otherwise the environment keys win over the `default` profile.
///
/// Role and SSO profiles call STS or the SSO portal through `http`; their
/// temporary credentials stay in `cache` until [`REFRESH_MARGIN`] seconds
/// before they expire. `amz_date` (`YYYYMMDDTHHMMSSZ`) is the current time.
///
/// # Examples
///
/// ```ignore
/// let mut cache = sigv4::Cache::new();
/// let creds = sigv4::load_credentials(Some("dev"), &env, &mut cache, &amz_date, &mut send)?;
/// ```
pub fn load_credentials(
    profile: Option<&str>,
    env: Env,
    cache: &mut Cache,
    amz_date: &str,
    http: Http,
) -> anyhow::Result<Credentials> {
    let profile = match profile.map(str::to_owned).or_else(|| env("AWS_PROFILE")) {
        Some(profile) => profile,
        None => match from_env(env) {
            Some(creds) => return Ok(creds),
            None => "default".to_owned(),
        },
    };
    let now = crate::unix_secs(amz_date).context("sigv4: malformed x-amz-date")?;
    Resolver {
        env,
        cache,
        amz_date,
        now,
        http,
    }
    .profile(&profile, &mut Vec::new())
}

fn from_env(env: Env) -> Option<Credentials> {
    Some(Credentials {
        access_key_id: env("AWS_ACCESS_KEY_ID")?,
        secret_access_key: env("AWS_SECRET_ACCESS_KEY")?,
        session_token: env("AWS_SESSION_TOKEN"),
    })
}

/// State of one [`load_credentials`] call.
struct Resolver<'a> {
    env: Env<'a>,
    cache: &'a mut Cache,
    amz_date: &'a str,
    /// Current time, unix seconds.
    now: u64,
    http: Http<'a>,
}

impl Resolver<'_> {
    /// Credentials of profile `name`. `chain` lists the profiles whose
    /// `source_profile` led here, to detect cycles.
    fn profile(&mut self, name: &str, chain: &mut Vec<String>) -> anyhow::Result<Credentials> {
        if let Some((creds, expires)) = self.cache.get(name)
            && self.now.saturating_add(REFRESH_MARGIN) < *expires
        {
            return Ok(creds.clone());
        }
        let section = profile_section(name, self.env)?;
        let temporary = match value(&section, "role_arn") {
            Some(role_arn) => self.role(name, &section, role_arn, chain)?,
            None if section.contains_key("sso_session")
                || section.contains_key("sso_start_url") =>
            {
                self.sso(name, &section)?
            }
            None => return credentials_from_section(name, &section),
        };
        self.cache.insert(name.to_owned(), temporary.clone());
        Ok(temporary.0)
    }

    /// Assumes `role_arn` with the credentials of `source_profile`, of the
    /// environment (`credential_source = Environment`) or of a web identity.
    fn role(
        &mut self,
        name: &str,
        section: &BTreeMap<String, String>,
        role_arn: &str,
        chain: &mut Vec<String>,
    ) -> anyhow::Result<(Credentials, u64)> {
        if section.contains_key("mfa_serial") {
            bail!(
                "sigv4: profile `{name}` uses `mfa_serial`, which needs an interactive \
                 MFA prompt that a WASI plugin cannot show"
            );
        }
        if let Some(source) = value(section, "source_profile") {
            let creds = self.source(name, source, chain)?;
            return self.assume_role(name, section, role_arn, &creds);
        }
        if let Some(source) = value(section, "credential_source") {
            if source != "Environment" {
                bail!(
                    "sigv4: profile `{name}` uses credential_source `{source}`; \
                     only `Environment` is supported"
                );
            }
            let creds = from_env(self.env).with_context(|| {
                format!(
                    "sigv4: profile `{name}` uses credential_source = Environment, \
                     but AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY are not set"
                )
            })?;
            return self.assume_role(name, section, role_arn, &creds);
        }
        if let Some(file) = value(section, "web_identity_token_file") {
            return self.web_identity(name, section, role_arn, file);
        }
        bail!(
            "sigv4: profile `{name}` has role_arn but no source_profile, \
             credential_source or web_identity_token_file"
        )
    }

    /// Credentials of `source`, the `source_profile` of `name`. Like botocore,
    /// static keys of the source win over its own role (so a profile may name
    /// itself); otherwise it is resolved recursively.
    fn source(
        &mut self,
        name: &str,
        source: &str,
        chain: &mut Vec<String>,
    ) -> anyhow::Result<Credentials> {
        let section = profile_section(source, self.env)?;
        if value(&section, "aws_access_key_id").is_some()
            && value(&section, "aws_secret_access_key").is_some()
        {
            return credentials_from_section(source, &section);
        }
        chain.push(name.to_owned());
        if chain.iter().any(|p| p == source) {
            bail!(
                "sigv4: source_profile cycle: {} -> {source}",
                chain.join(" -> ")
            );
        }
        self.profile(source, chain)
    }

    /// STS `AssumeRole`, signed with `creds`.
    fn assume_role(
        &mut self,
        name: &str,
        section: &BTreeMap<String, String>,
        role_arn: &str,
        creds: &Credentials,
    ) -> anyhow::Result<(Credentials, u64)> {
        let region = sts_region(section, self.env);
        let url = sts_url(&region)?;
        let mut form = self.role_form(section, "AssumeRole", role_arn);
        if let Some(id) = value(section, "external_id") {
            form.append_pair("ExternalId", id);
        }
        if let Some(secs) = value(section, "duration_seconds") {
            form.append_pair("DurationSeconds", secs);
        }
        let body = form.finish();
        let signed = sign(
            creds,
            "POST",
            FORM,
            &region,
            "sts",
            &url,
            body.as_bytes(),
            self.amz_date,
        )?;
        let mut headers = vec![
            ("content-type".to_owned(), FORM.to_owned()),
            ("x-amz-date".to_owned(), signed.amz_date),
            ("authorization".to_owned(), signed.authorization),
        ];
        if let Some(token) = signed.security_token {
            headers.push(("x-amz-security-token".to_owned(), token));
        }
        self.sts(name, "AssumeRole", &url, &headers, &body)
    }

    /// Unsigned STS `AssumeRoleWithWebIdentity` with the token in `file`.
    fn web_identity(
        &mut self,
        name: &str,
        section: &BTreeMap<String, String>,
        role_arn: &str,
        file: &str,
    ) -> anyhow::Result<(Credentials, u64)> {
        let token = std::fs::read_to_string(file).with_context(|| {
            format!("sigv4: profile `{name}`: cannot read web_identity_token_file `{file}`")
        })?;
        let url = sts_url(&sts_region(section, self.env))?;
        let mut form = self.role_form(section, "AssumeRoleWithWebIdentity", role_arn);
        form.append_pair("WebIdentityToken", token.trim());
        let body = form.finish();
        let headers = [("content-type".to_owned(), FORM.to_owned())];
        self.sts(name, "AssumeRoleWithWebIdentity", &url, &headers, &body)
    }

    /// The form fields every STS role action takes.
    fn role_form(
        &self,
        section: &BTreeMap<String, String>,
        action: &str,
        role_arn: &str,
    ) -> form_urlencoded::Serializer<'static, String> {
        let session_name = value(section, "role_session_name")
            .map_or_else(|| format!("owlet-aws-mcp-{}", self.now), str::to_owned);
        let mut form = form_urlencoded::Serializer::new(String::new());
        form.append_pair("Action", action)
            .append_pair("Version", STS_VERSION)
            .append_pair("RoleArn", role_arn)
            .append_pair("RoleSessionName", &session_name);
        form
    }

    /// POSTs an STS query `body` and parses the credentials from the XML reply.
    fn sts(
        &mut self,
        name: &str,
        action: &str,
        url: &Url,
        headers: &[(String, String)],
        body: &str,
    ) -> anyhow::Result<(Credentials, u64)> {
        let resp = (self.http)("POST", url.as_str(), headers, body.as_bytes())
            .map_err(|e| anyhow!("sigv4: STS {action} for profile `{name}`: {e}"))?;
        let text = String::from_utf8_lossy(&resp.body);
        if !(200..300).contains(&resp.status) {
            let message = xml_text(&text, "Message")
                .unwrap_or_else(|| text.trim().chars().take(MAX_ERROR_BODY).collect());
            bail!(
                "sigv4: STS {action} for profile `{name}` failed: HTTP {}: {message}",
                resp.status
            );
        }
        let field = |tag: &str| {
            xml_text(&text, tag)
                .filter(|v| !v.is_empty())
                .with_context(|| format!("sigv4: STS {action} reply has no {tag}"))
        };
        let expiration = field("Expiration")?;
        let expires = crate::unix_secs(&expiration).with_context(|| {
            format!("sigv4: STS {action} reply has a malformed Expiration `{expiration}`")
        })?;
        let creds = Credentials {
            access_key_id: field("AccessKeyId")?,
            secret_access_key: field("SecretAccessKey")?,
            session_token: Some(field("SessionToken")?),
        };
        Ok((creds, expires))
    }

    /// IAM Identity Center: exchanges the token cached by `aws sso login` for
    /// role credentials at the SSO portal (`GetRoleCredentials`).
    fn sso(
        &mut self,
        name: &str,
        section: &BTreeMap<String, String>,
    ) -> anyhow::Result<(Credentials, u64)> {
        let place = format!("profile `{name}`");
        // The token cache is keyed by the session name, or the legacy start URL.
        let (region, cache_key) = match value(section, "sso_session") {
            Some(session) => {
                let header = format!("sso-session {session}");
                let sso = aws_path(self.env, "AWS_CONFIG_FILE", ".aws/config")
                    .and_then(|path| std::fs::read_to_string(path).ok())
                    .and_then(|text| read_section(&text, &header, false))
                    .with_context(|| {
                        format!("sigv4: {place}: no [{header}] section in the config file")
                    })?;
                (
                    required(&sso, "sso_region", &format!("[{header}]"))?,
                    session.to_owned(),
                )
            }
            None => (
                required(section, "sso_region", &place)?,
                required(section, "sso_start_url", &place)?,
            ),
        };
        let account = required(section, "sso_account_id", &place)?;
        let role = required(section, "sso_role_name", &place)?;
        let token = self.sso_token(name, &cache_key)?;

        let mut url = Url::parse(&format!(
            "https://portal.sso.{}.amazonaws.com/federation/credentials",
            checked_region(&region)?
        ))?;
        url.query_pairs_mut()
            .append_pair("account_id", &account)
            .append_pair("role_name", &role);
        let headers = [("x-amz-sso_bearer_token".to_owned(), token)];
        let resp = (self.http)("GET", url.as_str(), &headers, &[])
            .map_err(|e| anyhow!("sigv4: SSO GetRoleCredentials for {place}: {e}"))?;
        if !(200..300).contains(&resp.status) {
            let text = String::from_utf8_lossy(&resp.body);
            let text: String = text.trim().chars().take(MAX_ERROR_BODY).collect();
            bail!(
                "sigv4: SSO GetRoleCredentials for {place} failed: HTTP {}: {text}; \
                 if the SSO session expired, run `aws sso login --profile {name}`",
                resp.status
            );
        }
        let reply: Value = serde_json::from_slice(&resp.body)
            .with_context(|| format!("sigv4: SSO GetRoleCredentials for {place}: not JSON"))?;
        let field = |key: &str| {
            reply
                .pointer(&format!("/roleCredentials/{key}"))
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty())
                .map(str::to_owned)
                .with_context(|| format!("sigv4: SSO GetRoleCredentials reply has no {key}"))
        };
        let expires_ms = reply
            .pointer("/roleCredentials/expiration")
            .and_then(Value::as_u64)
            .context("sigv4: SSO GetRoleCredentials reply has no expiration")?;
        let creds = Credentials {
            access_key_id: field("accessKeyId")?,
            secret_access_key: field("secretAccessKey")?,
            session_token: Some(field("sessionToken")?),
        };
        Ok((creds, expires_ms / 1000))
    }

    /// The unexpired access token in `~/.aws/sso/cache/<sha1(key)>.json`. The
    /// plugin's directories are read-only, so an expired token is not refreshed.
    fn sso_token(&self, name: &str, key: &str) -> anyhow::Result<String> {
        let login = format!("run `aws sso login --profile {name}`");
        let home = (self.env)("HOME").with_context(|| {
            format!("sigv4: profile `{name}`: HOME is not set, so the SSO token cache is unknown")
        })?;
        let path = PathBuf::from(home)
            .join(".aws/sso/cache")
            .join(format!("{}.json", sha1_smol::Sha1::from(key).digest()));
        let cached: Value = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .with_context(|| {
                format!(
                    "sigv4: profile `{name}`: no cached SSO token at {}; {login}",
                    path.display()
                )
            })?;
        let token = cached
            .get("accessToken")
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty());
        let expires = cached
            .get("expiresAt")
            .and_then(Value::as_str)
            .and_then(crate::unix_secs);
        match (token, expires) {
            (Some(token), Some(expires)) if self.now < expires => Ok(token.to_owned()),
            (Some(_), Some(_)) => {
                bail!("sigv4: profile `{name}`: the cached SSO token has expired; {login}")
            }
            _ => bail!(
                "sigv4: profile `{name}`: malformed SSO token cache {}; {login}",
                path.display()
            ),
        }
    }
}

/// The section of `profile` in the config file merged with its section in the
/// shared credentials file, whose keys win (as in botocore).
fn profile_section(profile: &str, env: Env) -> anyhow::Result<BTreeMap<String, String>> {
    let shared = aws_path(env, "AWS_SHARED_CREDENTIALS_FILE", ".aws/credentials");
    let config = aws_path(env, "AWS_CONFIG_FILE", ".aws/config");
    let read = |path: Option<PathBuf>| path.and_then(|p| std::fs::read_to_string(p).ok());

    let mut section = read(config).and_then(|text| read_section(&text, profile, true));
    if let Some(keys) = read(shared).and_then(|text| read_section(&text, profile, false)) {
        section.get_or_insert_default().extend(keys);
    }
    section.with_context(|| format!("sigv4: no credentials for profile `{profile}`"))
}

fn credentials_from_section(
    profile: &str,
    section: &BTreeMap<String, String>,
) -> anyhow::Result<Credentials> {
    if let Some(key) = UNSUPPORTED.iter().find(|k| section.contains_key(**k)) {
        bail!(
            "sigv4: profile `{profile}` uses `{key}`, which this plugin cannot resolve: \
             a WASI plugin cannot run processes; use static keys, role_arn, \
             web_identity_token_file or SSO instead"
        );
    }
    let get = |key: &str| {
        section
            .get(key)
            .filter(|v| !v.is_empty())
            .cloned()
            .with_context(|| format!("sigv4: profile `{profile}` has no {key}"))
    };
    Ok(Credentials {
        access_key_id: get("aws_access_key_id")?,
        secret_access_key: get("aws_secret_access_key")?,
        session_token: section
            .get("aws_session_token")
            .filter(|v| !v.is_empty())
            .cloned(),
    })
}

/// A non-empty value of `section`.
fn value<'a>(section: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    section
        .get(key)
        .map(String::as_str)
        .filter(|v| !v.is_empty())
}

/// [`value`], or an error saying `place` lacks `key`.
fn required(section: &BTreeMap<String, String>, key: &str, place: &str) -> anyhow::Result<String> {
    value(section, key)
        .map(str::to_owned)
        .with_context(|| format!("sigv4: {place} has no {key}"))
}

/// The profile's `region`, else `$AWS_REGION` / `$AWS_DEFAULT_REGION`, else `us-east-1`.
fn sts_region(section: &BTreeMap<String, String>, env: Env) -> String {
    value(section, "region")
        .map(str::to_owned)
        .or_else(|| env("AWS_REGION"))
        .or_else(|| env("AWS_DEFAULT_REGION"))
        .unwrap_or_else(|| "us-east-1".to_owned())
}

fn sts_url(region: &str) -> anyhow::Result<Url> {
    Ok(Url::parse(&format!(
        "https://sts.{}.amazonaws.com/",
        checked_region(region)?
    ))?)
}

/// Rejects regions that would change the host they are spliced into.
fn checked_region(region: &str) -> anyhow::Result<&str> {
    if !region.is_empty()
        && region
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        Ok(region)
    } else {
        bail!("sigv4: invalid region `{region}`")
    }
}

/// The unescaped text of the first `<tag>` element in `xml`.
fn xml_text(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let start = xml.find(&open)? + open.len();
    let rest = xml.get(start..)?;
    let end = rest.find(&format!("</{tag}>"))?;
    Some(
        rest.get(..end)?
            .trim()
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
            .replace("&amp;", "&"),
    )
}

/// The `region` key of `profile` (`None`: `default`) in the shared config file.
pub fn profile_region(profile: Option<&str>, env: Env) -> Option<String> {
    let path = aws_path(env, "AWS_CONFIG_FILE", ".aws/config")?;
    let text = std::fs::read_to_string(path).ok()?;
    read_section(&text, profile.unwrap_or("default"), true)?
        .remove("region")
        .filter(|v| !v.is_empty())
}

fn aws_path(env: Env, var: &str, default: &str) -> Option<PathBuf> {
    if let Some(path) = env(var) {
        return Some(PathBuf::from(path));
    }
    env("HOME").map(|home| PathBuf::from(home).join(default))
}

/// Reads one INI section. In the shared credentials file the header is
/// `[name]`; in the config file (`config_style`) it is `[profile name]`, except
/// for `default`, which may be written either way.
fn read_section(text: &str, profile: &str, config_style: bool) -> Option<BTreeMap<String, String>> {
    let matches = |header: &str| {
        if !config_style {
            return header == profile;
        }
        let prefixed = header
            .strip_prefix("profile ")
            .map(str::trim)
            .is_some_and(|name| name == profile);
        prefixed || (profile == "default" && header == "default")
    };

    let mut section = BTreeMap::new();
    let mut found = false;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(header) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            if found {
                break;
            }
            found = matches(header.trim());
            continue;
        }
        if found && let Some((key, value)) = line.split_once('=') {
            section.insert(key.trim().to_lowercase(), value.trim().to_owned());
        }
    }
    found.then_some(section)
}

/// The headers SigV4 signing adds to a request.
#[derive(Debug, PartialEq, Eq)]
pub struct SignedHeaders {
    pub authorization: String,
    pub amz_date: String,
    pub security_token: Option<String>,
}

/// Signs a `method` request to `url` with `body` and returns the headers to attach.
///
/// `content-type` (`content_type`), `host` and `x-amz-date` (plus
/// `x-amz-security-token` with a session token) are signed; `amz_date` is the
/// `YYYYMMDDTHHMMSSZ` timestamp sent as `X-Amz-Date`.
///
/// # Examples
///
/// ```ignore
/// let signed = sigv4::sign(&creds, "POST", "application/json", "us-east-1", "aws-mcp", &url, body, &amz_date)?;
/// ```
#[allow(clippy::too_many_arguments)]
pub fn sign(
    creds: &Credentials,
    method: &str,
    content_type: &str,
    region: &str,
    service: &str,
    url: &Url,
    body: &[u8],
    amz_date: &str,
) -> anyhow::Result<SignedHeaders> {
    let date = amz_date.get(..8).context("sigv4: malformed x-amz-date")?;
    let host = url.host_str().context("sigv4: endpoint has no host")?;
    let host_header = match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    };

    // Canonical request. Headers must be sorted by lowercase name.
    let mut headers: Vec<(&str, &str)> = vec![
        ("content-type", content_type),
        ("host", &host_header),
        ("x-amz-date", amz_date),
    ];
    if let Some(token) = &creds.session_token {
        headers.push(("x-amz-security-token", token));
    }
    headers.sort_by(|a, b| a.0.cmp(b.0));

    let signed_header_names = headers
        .iter()
        .map(|(name, _)| *name)
        .collect::<Vec<_>>()
        .join(";");
    let canonical_headers = headers
        .iter()
        .map(|(name, value)| format!("{name}:{}\n", value.trim()))
        .collect::<String>();
    let canonical_request = format!(
        "{method}\n{}\n{}\n{canonical_headers}\n{signed_header_names}\n{}",
        uri_encode(url.path(), false),
        canonical_query(url),
        hex(&Sha256::digest(body)),
    );

    let scope = format!("{date}/{region}/{service}/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        hex(&Sha256::digest(canonical_request.as_bytes())),
    );
    let key = signing_key(&creds.secret_access_key, date, region, service)?;
    let signature = hex(&hmac(&key, string_to_sign.as_bytes())?);

    Ok(SignedHeaders {
        authorization: format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_header_names}, Signature={signature}",
            creds.access_key_id,
        ),
        amz_date: amz_date.to_owned(),
        security_token: creds.session_token.clone(),
    })
}

/// Builds the canonical query string (keys sorted, values URI-encoded).
fn canonical_query(url: &Url) -> String {
    let mut pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(k, v)| (uri_encode(&k, true), uri_encode(&v, true)))
        .collect();
    pairs.sort();
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// Derives the SigV4 signing key: HMAC chain over date, region, service, suffix.
fn signing_key(secret: &str, date: &str, region: &str, service: &str) -> anyhow::Result<Vec<u8>> {
    let k_date = hmac(format!("AWS4{secret}").as_bytes(), date.as_bytes())?;
    let k_region = hmac(&k_date, region.as_bytes())?;
    let k_service = hmac(&k_region, service.as_bytes())?;
    hmac(&k_service, b"aws4_request")
}

fn hmac(key: &[u8], data: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut mac = HmacSha256::new_from_slice(key)
        .map_err(|_| anyhow::anyhow!("sigv4: invalid HMAC key length"))?;
    mac.update(data);
    Ok(mac.finalize().into_bytes().to_vec())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// RFC 3986 encoding as required by SigV4 (`/` is kept unless `encode_slash`).
fn uri_encode(input: &str, encode_slash: bool) -> String {
    let mut out = String::with_capacity(input.len());
    for b in input.bytes() {
        let keep = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~');
        if keep || (b == b'/' && !encode_slash) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::HashMap};

    use super::*;

    fn env_of(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let map: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |k| map.get(k).filter(|v| !v.is_empty()).cloned()
    }

    fn url(s: &str) -> Url {
        Url::parse(s).expect("url")
    }

    /// 2024-01-01T00:00:00Z.
    const DATE: &str = "20240101T000000Z";
    const NOW: u64 = 1_704_067_200;

    /// Resolves without a cache; any HTTP request fails.
    fn load(profile: Option<&str>, env: Env) -> anyhow::Result<Credentials> {
        load_credentials(
            profile,
            env,
            &mut Cache::new(),
            DATE,
            &mut |_, url, _, _| Err(format!("unexpected request to {url}")),
        )
    }

    /// Requests seen by a mock `http`: (method, url, headers, body).
    type Seen = RefCell<Vec<(String, String, Vec<(String, String)>, Vec<u8>)>>;

    fn record(seen: &Seen, method: &str, url: &str, headers: &[(String, String)], body: &[u8]) {
        seen.borrow_mut().push((
            method.to_owned(),
            url.to_owned(),
            headers.to_vec(),
            body.to_vec(),
        ));
    }

    fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
        headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    fn form(body: &[u8]) -> BTreeMap<String, String> {
        form_urlencoded::parse(body).into_owned().collect()
    }

    /// The access key a request was signed with.
    fn signer(headers: &[(String, String)]) -> Option<String> {
        let auth = header(headers, "authorization")?;
        let credential = auth.split("Credential=").nth(1)?;
        credential.split('/').next().map(str::to_owned)
    }

    /// An STS `action` reply issuing `key` until `expiration`.
    fn sts_reply(action: &str, key: &str, expiration: &str) -> Response {
        let xml = format!(
            "<{action}Response xmlns=\"https://sts.amazonaws.com/doc/2011-06-15/\">\
             <{action}Result><Credentials><AccessKeyId>{key}</AccessKeyId>\
             <SecretAccessKey>secret-{key}</SecretAccessKey>\
             <SessionToken>token-{key}</SessionToken>\
             <Expiration>{expiration}</Expiration></Credentials></{action}Result>\
             </{action}Response>"
        );
        Response {
            status: 200,
            headers: vec![("content-type".to_owned(), "text/xml".to_owned())],
            body: xml.into_bytes(),
        }
    }

    /// A temp `$HOME` with `~/.aws/credentials` and `~/.aws/config`.
    fn aws_home(name: &str, credentials: &str, config: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("owlet-aws-{name}-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".aws/sso/cache")).expect("mkdir");
        std::fs::write(dir.join(".aws/credentials"), credentials).expect("write");
        std::fs::write(dir.join(".aws/config"), config).expect("write");
        dir
    }

    const BASE: &str =
        "[base]\naws_access_key_id = AKIDBASE\naws_secret_access_key = base-secret\n";

    #[test]
    fn assume_role_from_static_source_is_cached() {
        let dir = aws_home(
            "assume",
            BASE,
            "[profile admin]\nrole_arn = arn:aws:iam::123456789012:role/Admin\n\
             source_profile = base\nregion = eu-west-1\nexternal_id = ext-1\n\
             duration_seconds = 900\n",
        );
        let home = dir.to_str().expect("utf-8");
        let env = env_of(&[("HOME", home)]);
        let seen = Seen::default();
        let mut http = |m: &str, u: &str, h: &[(String, String)], b: &[u8]| {
            record(&seen, m, u, h, b);
            Ok(sts_reply("AssumeRole", "ASIAROLE", "2024-01-01T01:00:00Z"))
        };
        let mut cache = Cache::new();
        let creds =
            load_credentials(Some("admin"), &env, &mut cache, DATE, &mut http).expect("role");
        assert_eq!(
            creds,
            Credentials {
                access_key_id: "ASIAROLE".to_owned(),
                secret_access_key: "secret-ASIAROLE".to_owned(),
                session_token: Some("token-ASIAROLE".to_owned()),
            }
        );
        {
            let seen = seen.borrow();
            let (method, url, headers, body) = &seen[0];
            assert_eq!(
                (method.as_str(), url.as_str()),
                ("POST", "https://sts.eu-west-1.amazonaws.com/")
            );
            assert_eq!(header(headers, "content-type"), Some(FORM));
            assert_eq!(header(headers, "x-amz-date"), Some(DATE));
            let auth = header(headers, "authorization").expect("signed");
            assert!(
                auth.starts_with(
                    "AWS4-HMAC-SHA256 Credential=AKIDBASE/20240101/eu-west-1/sts/aws4_request, \
                     SignedHeaders=content-type;host;x-amz-date, Signature="
                ),
                "{auth}"
            );
            assert_eq!(header(headers, "x-amz-security-token"), None);
            let fields = form(body);
            let expected = [
                ("Action", "AssumeRole"),
                ("Version", "2011-06-15"),
                ("RoleArn", "arn:aws:iam::123456789012:role/Admin"),
                ("RoleSessionName", "owlet-aws-mcp-1704067200"),
                ("ExternalId", "ext-1"),
                ("DurationSeconds", "900"),
            ];
            assert_eq!(
                fields,
                expected
                    .iter()
                    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                    .collect()
            );
        }
        assert_eq!(cache["admin"].1, NOW + 3600);
        assert!(
            !cache.contains_key("base"),
            "static credentials are not cached"
        );

        // 30 minutes later the cached credentials are still fresh.
        load_credentials(
            Some("admin"),
            &env,
            &mut cache,
            "20240101T003000Z",
            &mut http,
        )
        .expect("cached");
        assert_eq!(seen.borrow().len(), 1);
        // 4 minutes before expiry they are refreshed.
        load_credentials(
            Some("admin"),
            &env,
            &mut cache,
            "20240101T005600Z",
            &mut http,
        )
        .expect("refreshed");
        assert_eq!(seen.borrow().len(), 2);

        // STS errors carry the service message.
        let mut denied = |_: &str, _: &str, _: &[(String, String)], _: &[u8]| {
            Ok(Response {
                status: 403,
                headers: Vec::new(),
                body: b"<ErrorResponse><Error><Type>Sender</Type><Code>AccessDenied</Code>\
                        <Message>User is not authorized &amp; denied</Message></Error>\
                        </ErrorResponse>"
                    .to_vec(),
            })
        };
        let err = load_credentials(Some("admin"), &env, &mut Cache::new(), DATE, &mut denied)
            .expect_err("denied");
        assert_eq!(
            format!("{err:#}"),
            "sigv4: STS AssumeRole for profile `admin` failed: HTTP 403: \
             User is not authorized & denied"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn profile_split_across_files_is_merged() {
        // `aws configure --profile admin` left keys in credentials; the role is in config.
        let dir = aws_home(
            "split",
            &format!("{BASE}[admin]\naws_access_key_id = AKIDSTALE\naws_secret_access_key = s\n"),
            "[profile admin]\nrole_arn = arn:aws:iam::1:role/Admin\nsource_profile = base\n\
             region = eu-west-1\n",
        );
        let home = dir.to_str().expect("utf-8");
        let env = env_of(&[("HOME", home)]);
        let seen = Seen::default();
        let mut http = |m: &str, u: &str, h: &[(String, String)], b: &[u8]| {
            record(&seen, m, u, h, b);
            Ok(sts_reply("AssumeRole", "ASIAROLE", "2024-01-01T01:00:00Z"))
        };
        let creds = load_credentials(Some("admin"), &env, &mut Cache::new(), DATE, &mut http)
            .expect("role");
        assert_eq!(creds.access_key_id, "ASIAROLE");
        let seen = seen.borrow();
        assert_eq!(seen.len(), 1);
        let (_, url, headers, _) = &seen[0];
        assert_eq!(url, "https://sts.eu-west-1.amazonaws.com/", "config region");
        assert_eq!(signer(headers).as_deref(), Some("AKIDBASE"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn role_chains_and_cycles() {
        let dir = aws_home(
            "chain",
            BASE,
            "[profile first]\nrole_arn = arn:aws:iam::1:role/First\nsource_profile = base\n\
             [profile second]\nrole_arn = arn:aws:iam::1:role/Second\nsource_profile = first\n\
             role_session_name = chained\n\
             [profile loop-a]\nrole_arn = arn:aws:iam::1:role/A\nsource_profile = loop-b\n\
             [profile loop-b]\nrole_arn = arn:aws:iam::1:role/B\nsource_profile = loop-a\n\
             [profile self]\nrole_arn = arn:aws:iam::1:role/Self\nsource_profile = self\n\
             aws_access_key_id = AKIDSELF\naws_secret_access_key = s\n",
        );
        let home = dir.to_str().expect("utf-8");
        let env = env_of(&[("HOME", home)]);
        let seen = Seen::default();
        let mut http = |m: &str, u: &str, h: &[(String, String)], b: &[u8]| {
            record(&seen, m, u, h, b);
            let role = form(b)["RoleArn"]
                .rsplit('/')
                .next()
                .unwrap_or_default()
                .to_uppercase();
            Ok(sts_reply(
                "AssumeRole",
                &format!("ASIA{role}"),
                "2024-01-01T01:00:00Z",
            ))
        };
        let mut cache = Cache::new();
        let creds =
            load_credentials(Some("second"), &env, &mut cache, DATE, &mut http).expect("chain");
        assert_eq!(creds.access_key_id, "ASIASECOND");
        {
            let seen = seen.borrow();
            assert_eq!(seen.len(), 2);
            assert_eq!(seen[0].1, "https://sts.us-east-1.amazonaws.com/");
            assert_eq!(signer(&seen[0].2).as_deref(), Some("AKIDBASE"));
            assert_eq!(form(&seen[0].3)["RoleArn"], "arn:aws:iam::1:role/First");
            assert_eq!(signer(&seen[1].2).as_deref(), Some("ASIAFIRST"));
            assert_eq!(
                header(&seen[1].2, "x-amz-security-token"),
                Some("token-ASIAFIRST")
            );
            assert_eq!(form(&seen[1].3)["RoleSessionName"], "chained");
        }
        assert!(cache.contains_key("first") && cache.contains_key("second"));

        // The cached intermediate role is reused.
        seen.borrow_mut().clear();
        cache.remove("second");
        load_credentials(Some("second"), &env, &mut cache, DATE, &mut http).expect("chain");
        assert_eq!(seen.borrow().len(), 1);

        let err =
            load_credentials(Some("loop-a"), &env, &mut cache, DATE, &mut http).expect_err("cycle");
        assert_eq!(
            format!("{err:#}"),
            "sigv4: source_profile cycle: loop-a -> loop-b -> loop-a"
        );
        // A profile naming itself uses its own static keys.
        seen.borrow_mut().clear();
        let creds =
            load_credentials(Some("self"), &env, &mut cache, DATE, &mut http).expect("self");
        assert_eq!(creds.access_key_id, "ASIASELF");
        assert_eq!(signer(&seen.borrow()[0].2).as_deref(), Some("AKIDSELF"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn role_from_environment_and_unsupported_sources() {
        let dir = aws_home(
            "envrole",
            BASE,
            "[profile envrole]\nrole_arn = arn:aws:iam::1:role/Env\ncredential_source = Environment\n\
             [profile ec2]\nrole_arn = arn:aws:iam::1:role/E\ncredential_source = Ec2InstanceMetadata\n\
             [profile mfa]\nrole_arn = arn:aws:iam::1:role/M\nsource_profile = base\nmfa_serial = arn:mfa\n\
             [profile bare]\nrole_arn = arn:aws:iam::1:role/Bare\n",
        );
        let home = dir.to_str().expect("utf-8");
        let env = env_of(&[
            ("HOME", home),
            ("AWS_ACCESS_KEY_ID", "AKIDENV"),
            ("AWS_SECRET_ACCESS_KEY", "env-secret"),
            ("AWS_REGION", "ap-south-1"),
        ]);
        let seen = Seen::default();
        let mut http = |m: &str, u: &str, h: &[(String, String)], b: &[u8]| {
            record(&seen, m, u, h, b);
            Ok(sts_reply("AssumeRole", "ASIAENV", "2024-01-01T01:00:00Z"))
        };
        let mut cache = Cache::new();
        let creds =
            load_credentials(Some("envrole"), &env, &mut cache, DATE, &mut http).expect("env");
        assert_eq!(creds.access_key_id, "ASIAENV");
        {
            let seen = seen.borrow();
            assert_eq!(seen[0].1, "https://sts.ap-south-1.amazonaws.com/");
            assert_eq!(signer(&seen[0].2).as_deref(), Some("AKIDENV"));
        }
        let mut fail = |profile: &str, env: Env| {
            let err = load_credentials(Some(profile), env, &mut Cache::new(), DATE, &mut http)
                .expect_err(profile);
            format!("{err:#}")
        };
        assert!(fail("ec2", &env).contains("credential_source `Ec2InstanceMetadata`"));
        assert!(fail("mfa", &env).contains("`mfa_serial`"));
        assert!(fail("bare", &env).contains("has role_arn but no source_profile"));
        let no_keys = env_of(&[("HOME", home)]);
        assert!(fail("envrole", &no_keys).contains("AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY"));
        assert_eq!(seen.borrow().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn web_identity_is_unsigned_with_the_token_file() {
        let dir = std::env::temp_dir().join(format!("owlet-aws-webid-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".aws")).expect("mkdir");
        let token = dir.join("token");
        std::fs::write(&token, "jwt-token\n").expect("write");
        let dir = aws_home(
            "webid",
            "",
            &format!(
                "[profile web]\nrole_arn = arn:aws:iam::1:role/Web\nregion = us-west-2\n\
                 web_identity_token_file = {}\n\
                 [profile gone]\nrole_arn = arn:aws:iam::1:role/Web\n\
                 web_identity_token_file = {}\n",
                token.display(),
                dir.join("missing").display()
            ),
        );
        let home = dir.to_str().expect("utf-8");
        let env = env_of(&[("HOME", home)]);
        let seen = Seen::default();
        let mut http = |m: &str, u: &str, h: &[(String, String)], b: &[u8]| {
            record(&seen, m, u, h, b);
            Ok(sts_reply(
                "AssumeRoleWithWebIdentity",
                "ASIAWEB",
                "2024-01-01T01:00:00Z",
            ))
        };
        let mut cache = Cache::new();
        let creds = load_credentials(Some("web"), &env, &mut cache, DATE, &mut http).expect("web");
        assert_eq!(creds.access_key_id, "ASIAWEB");
        assert_eq!(creds.session_token.as_deref(), Some("token-ASIAWEB"));
        {
            let seen = seen.borrow();
            let (method, url, headers, body) = &seen[0];
            assert_eq!(
                (method.as_str(), url.as_str()),
                ("POST", "https://sts.us-west-2.amazonaws.com/")
            );
            assert_eq!(header(headers, "authorization"), None, "unsigned");
            assert_eq!(header(headers, "content-type"), Some(FORM));
            let fields = form(body);
            assert_eq!(fields["Action"], "AssumeRoleWithWebIdentity");
            assert_eq!(fields["Version"], "2011-06-15");
            assert_eq!(fields["RoleArn"], "arn:aws:iam::1:role/Web");
            assert_eq!(fields["RoleSessionName"], "owlet-aws-mcp-1704067200");
            assert_eq!(fields["WebIdentityToken"], "jwt-token");
        }
        load_credentials(Some("web"), &env, &mut cache, DATE, &mut http).expect("cached");
        assert_eq!(seen.borrow().len(), 1);
        let err = load_credentials(Some("gone"), &env, &mut cache, DATE, &mut http)
            .expect_err("missing token");
        assert!(format!("{err:#}").contains("cannot read web_identity_token_file"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sso_uses_the_cached_login_token() {
        let dir = aws_home(
            "sso",
            "",
            "[profile sso-new]\nsso_session = corp\nsso_account_id = 111122223333\n\
             sso_role_name = ReadOnly\n\
             [sso-session corp]\nsso_start_url = https://corp.awsapps.com/start\n\
             sso_region = us-west-2\n\
             [profile sso-legacy]\nsso_start_url = https://legacy.awsapps.com/start\n\
             sso_region = eu-central-1\nsso_account_id = 444455556666\nsso_role_name = Admin\n\
             [profile sso-stale]\nsso_session = stale\nsso_account_id = 1\nsso_role_name = R\n\
             [sso-session stale]\nsso_region = us-east-1\n\
             [profile sso-nologin]\nsso_session = nologin\nsso_account_id = 1\nsso_role_name = R\n\
             [sso-session nologin]\nsso_region = us-east-1\n",
        );
        // File names are the SHA-1 of the session name / legacy start URL.
        let cache_dir = dir.join(".aws/sso/cache");
        for (sha1, token, expires) in [
            (
                "ee0bfd2552fbd840c02cc48b6e823320543c450f",
                "tok-corp",
                "2024-01-01T08:00:00Z",
            ),
            (
                "79e435d7a515078e81c9dffc35f38d5687ebd3a7",
                "tok-legacy",
                "2024-01-01T08:00:00Z",
            ),
            (
                "a4e976a3d3fc6d0b40453de216fbbe2e9aaecdc4",
                "tok-old",
                "2023-12-31T23:00:00Z",
            ),
        ] {
            let body = format!(
                "{{\"startUrl\": \"https://x\", \"region\": \"us-east-1\", \
                 \"accessToken\": \"{token}\", \"expiresAt\": \"{expires}\"}}"
            );
            std::fs::write(cache_dir.join(format!("{sha1}.json")), body).expect("write");
        }
        let home = dir.to_str().expect("utf-8");
        let env = env_of(&[("HOME", home)]);
        let seen = Seen::default();
        let mut http = |m: &str, u: &str, h: &[(String, String)], b: &[u8]| {
            record(&seen, m, u, h, b);
            let reply = serde_json::json!({"roleCredentials": {
                "accessKeyId": "ASIASSO", "secretAccessKey": "sso-secret",
                "sessionToken": "sso-token", "expiration": (NOW + 3600) * 1000}});
            Ok(Response {
                status: 200,
                headers: Vec::new(),
                body: reply.to_string().into_bytes(),
            })
        };
        let mut cache = Cache::new();
        let creds =
            load_credentials(Some("sso-new"), &env, &mut cache, DATE, &mut http).expect("sso");
        assert_eq!(
            creds,
            Credentials {
                access_key_id: "ASIASSO".to_owned(),
                secret_access_key: "sso-secret".to_owned(),
                session_token: Some("sso-token".to_owned()),
            }
        );
        assert_eq!(cache["sso-new"].1, NOW + 3600);
        load_credentials(Some("sso-legacy"), &env, &mut cache, DATE, &mut http).expect("legacy");
        {
            let seen = seen.borrow();
            let requests: Vec<(&str, &str, Option<&str>, usize)> = seen
                .iter()
                .map(|(m, u, h, b)| {
                    (
                        m.as_str(),
                        u.as_str(),
                        header(h, "x-amz-sso_bearer_token"),
                        b.len(),
                    )
                })
                .collect();
            assert_eq!(
                requests,
                [
                    (
                        "GET",
                        "https://portal.sso.us-west-2.amazonaws.com/federation/credentials\
                         ?account_id=111122223333&role_name=ReadOnly",
                        Some("tok-corp"),
                        0
                    ),
                    (
                        "GET",
                        "https://portal.sso.eu-central-1.amazonaws.com/federation/credentials\
                         ?account_id=444455556666&role_name=Admin",
                        Some("tok-legacy"),
                        0
                    ),
                ]
            );
            assert!(
                seen.iter()
                    .all(|(_, _, h, _)| header(h, "authorization").is_none())
            );
        }
        load_credentials(Some("sso-new"), &env, &mut cache, DATE, &mut http).expect("cached");
        assert_eq!(seen.borrow().len(), 2);

        let mut fail = |profile: &str| {
            let err = load_credentials(Some(profile), &env, &mut Cache::new(), DATE, &mut http)
                .expect_err(profile);
            format!("{err:#}")
        };
        let stale = fail("sso-stale");
        assert!(stale.contains("cached SSO token has expired"), "{stale}");
        assert!(
            stale.contains("run `aws sso login --profile sso-stale`"),
            "{stale}"
        );
        assert!(fail("sso-nologin").contains("no cached SSO token"));
        assert_eq!(seen.borrow().len(), 2);

        let mut unauthorized = |_: &str, _: &str, _: &[(String, String)], _: &[u8]| {
            Ok(Response {
                status: 401,
                headers: Vec::new(),
                body: b"{\"message\":\"Session token not found or invalid\"}".to_vec(),
            })
        };
        let err = load_credentials(
            Some("sso-new"),
            &env,
            &mut Cache::new(),
            DATE,
            &mut unauthorized,
        )
        .expect_err("401");
        let err = format!("{err:#}");
        assert!(err.contains("HTTP 401: {\"message\""), "{err}");
        assert!(err.contains("aws sso login --profile sso-new"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn xml_text_extracts_and_unescapes() {
        let xml = "<a><Message> x &lt;y&gt; &quot;z&quot; &amp;amp; </Message></a>";
        assert_eq!(
            xml_text(xml, "Message").as_deref(),
            Some("x <y> \"z\" &amp;")
        );
        assert_eq!(xml_text(xml, "Code"), None);
        assert_eq!(xml_text("<Message>open", "Message"), None);
        assert!(checked_region("eu-west-1").is_ok());
        assert!(checked_region("evil.com/#").is_err());
        assert!(checked_region("").is_err());
    }

    #[test]
    fn parses_endpoint_hosts() {
        assert_eq!(
            parse_endpoint(&url("https://aws-mcp.us-east-1.api.aws/mcp")),
            (Some("aws-mcp".to_owned()), Some("us-east-1".to_owned()))
        );
        assert_eq!(
            parse_endpoint(&url(
                "https://my-gw.gateway.bedrock-agentcore.us-west-2.amazonaws.com/mcp"
            )),
            (
                Some("bedrock-agentcore".to_owned()),
                Some("us-west-2".to_owned())
            )
        );
        assert_eq!(
            parse_endpoint(&url("https://example.com/mcp")),
            (Some("example".to_owned()), None)
        );
    }

    #[test]
    fn credentials_prefer_env_then_profile_files() {
        let env = env_of(&[
            ("AWS_ACCESS_KEY_ID", "AKIDENV"),
            ("AWS_SECRET_ACCESS_KEY", "s"),
            ("AWS_SESSION_TOKEN", "t"),
        ]);
        let creds = load(None, &env).expect("env creds");
        assert_eq!(creds.access_key_id, "AKIDENV");
        assert_eq!(creds.session_token.as_deref(), Some("t"));

        let dir = std::env::temp_dir().join(format!("owlet-aws-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".aws")).expect("mkdir");
        std::fs::write(
            dir.join(".aws/credentials"),
            "[default]\naws_access_key_id = AKIADEFAULT\naws_secret_access_key = d\n",
        )
        .expect("write");
        std::fs::write(
            dir.join(".aws/config"),
            "[profile proc]\ncredential_process = /bin/creds\n\n\
             [profile staging]\naws_access_key_id = AKIASTAGE\naws_secret_access_key = s\n",
        )
        .expect("write");
        let home = dir.to_str().expect("utf-8");

        let env = env_of(&[("HOME", home)]);
        let creds = load(None, &env).expect("default profile");
        assert_eq!(creds.access_key_id, "AKIADEFAULT");
        assert_eq!(creds.session_token, None);
        let creds = load(Some("staging"), &env).expect("config profile");
        assert_eq!(creds.access_key_id, "AKIASTAGE");
        let env_profile = env_of(&[("HOME", home), ("AWS_PROFILE", "staging")]);
        let creds = load(None, &env_profile).expect("AWS_PROFILE");
        assert_eq!(creds.access_key_id, "AKIASTAGE");
        // A named profile ignores environment keys, as botocore does.
        let env_keys = env_of(&[
            ("HOME", home),
            ("AWS_ACCESS_KEY_ID", "AKIDENV"),
            ("AWS_SECRET_ACCESS_KEY", "s"),
        ]);
        let creds = load(Some("staging"), &env_keys).expect("explicit profile");
        assert_eq!(creds.access_key_id, "AKIASTAGE");
        let creds = load(None, &env_keys).expect("env over default");
        assert_eq!(creds.access_key_id, "AKIDENV");

        let err = load(Some("proc"), &env).expect_err("credential_process unsupported");
        let err = format!("{err:#}");
        assert!(err.contains("`credential_process`"), "{err}");
        assert!(err.contains("cannot run processes"), "{err}");
        assert!(load(Some("missing"), &env).is_err());

        // An explicit credentials file overrides `$HOME/.aws/credentials`.
        let file = dir.join("alt");
        std::fs::write(
            &file,
            "[default]\naws_access_key_id = AKIAALT\naws_secret_access_key = a\n",
        )
        .expect("write");
        let env = env_of(&[
            ("HOME", home),
            ("AWS_SHARED_CREDENTIALS_FILE", file.to_str().expect("utf-8")),
        ]);
        assert_eq!(load(None, &env).expect("alt").access_key_id, "AKIAALT");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ini_profile_headers() {
        let creds = "[default]\naws_access_key_id = A\n\n[prod]\nAWS_Session_Token = tok\n\
                     [profile prod]\naws_access_key_id = WRONG\n";
        assert_eq!(
            read_section(creds, "default", false).expect("default")["aws_access_key_id"],
            "A"
        );
        assert_eq!(
            read_section(creds, "prod", false).expect("prod")["aws_session_token"],
            "tok"
        );

        let config = "# c\n[default]\nregion = r\n[profile staging]\naws_access_key_id = S\n\
                      [staging]\naws_access_key_id = WRONG\n";
        assert_eq!(
            read_section(config, "default", true).expect("default")["region"],
            "r"
        );
        assert_eq!(
            read_section(config, "staging", true).expect("staging")["aws_access_key_id"],
            "S"
        );
        assert!(read_section("[profile default]\nk = v\n", "default", true).is_some());
        // `[profile x]` is not a credentials-file header.
        assert!(read_section("[profile staging]\nk = v\n", "staging", false).is_none());
        assert!(read_section(config, "missing", true).is_none());
    }

    /// Known-answer test for the key derivation from the AWS SigV4 docs.
    #[test]
    fn signing_key_matches_aws_docs() {
        let key = signing_key(
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            "20120215",
            "us-east-1",
            "iam",
        )
        .expect("key");
        assert_eq!(
            hex(&key),
            "f4780e2d9f65fa895f9c67b32ce1baf0b0d8a43505a000a1a9e090d414db404d"
        );
    }

    /// Full signature cross-checked against an independent Python
    /// (hashlib/hmac) implementation of the same canonical request.
    #[test]
    fn signature_known_answer() {
        let creds = Credentials {
            access_key_id: "AKIDEXAMPLE".to_owned(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".to_owned(),
            session_token: None,
        };
        let u = url("https://aws-mcp.us-east-1.api.aws/mcp");
        let a = sign(
            &creds,
            "POST",
            "application/json",
            "us-east-1",
            "aws-mcp",
            &u,
            b"{}",
            "20240101T000000Z",
        )
        .expect("sign");
        assert_eq!(
            a.authorization,
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20240101/us-east-1/aws-mcp/aws4_request, \
             SignedHeaders=content-type;host;x-amz-date, Signature=2a5ec64b784002f4194bd832ff4094eb7c35b7881b825596ef03de397285a685"
        );
        let c = sign(
            &creds,
            "POST",
            "application/json",
            "us-east-1",
            "aws-mcp",
            &u,
            b"{\"a\":1}",
            "20240101T000000Z",
        )
        .expect("sign");
        assert_ne!(a.authorization, c.authorization);
        let delete = sign(
            &creds,
            "DELETE",
            "application/json",
            "us-east-1",
            "aws-mcp",
            &u,
            b"{}",
            "20240101T000000Z",
        )
        .expect("sign");
        assert_ne!(a.authorization, delete.authorization);
        assert!(
            sign(
                &creds,
                "POST",
                "application/json",
                "us-east-1",
                "aws-mcp",
                &u,
                b"{}",
                "2024"
            )
            .is_err()
        );
    }

    #[test]
    fn session_token_and_port_are_signed() {
        let creds = Credentials {
            access_key_id: "AKID".to_owned(),
            secret_access_key: "secret".to_owned(),
            session_token: Some("token-value".to_owned()),
        };
        let u = url("http://127.0.0.1:8080/mcp?b=2&a=1");
        let signed = sign(
            &creds,
            "POST",
            "application/json",
            "us-east-1",
            "aws-mcp",
            &u,
            b"{}",
            "20240101T000000Z",
        )
        .expect("sign");
        assert_eq!(signed.security_token.as_deref(), Some("token-value"));
        assert!(
            signed
                .authorization
                .contains("SignedHeaders=content-type;host;x-amz-date;x-amz-security-token")
        );
        assert_eq!(canonical_query(&u), "a=1&b=2");
    }

    #[test]
    fn uri_encode_matches_sigv4_rules() {
        assert_eq!(uri_encode("a b", true), "a%20b");
        assert_eq!(uri_encode("a/b", false), "a/b");
        assert_eq!(uri_encode("a/b", true), "a%2Fb");
        assert_eq!(uri_encode("-_.~", true), "-_.~");
    }
}
