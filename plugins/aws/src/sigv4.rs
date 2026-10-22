//! AWS credentials and Signature Version 4 signing for the proxied Streamable
//! HTTP endpoint, on the AWS SDK: `aws-config` resolves credentials and
//! `aws-sigv4` signs, like `boto3.Session(profile).get_credentials()` plus
//! `SigV4Auth.add_auth` in `mcp-proxy-for-aws`.
//!
//! A named profile reads only the shared profile files (static keys,
//! `role_arn` with `source_profile` / `credential_source`, web identity);
//! otherwise the environment keys come first, then the `default` profile,
//! `$AWS_WEB_IDENTITY_TOKEN_FILE` and the ECS container endpoint. The SDK's
//! HTTP calls (STS, ECS) go through owlet's http import. `credential_process`
//! is unsupported because a WASI plugin cannot run processes; IAM Identity
//! Center (SSO) because its SDK feature pulls in `ring`, a C build; EC2
//! instance metadata is left out of the chain because the blocking import
//! cannot time out its probe off EC2.

use std::{
    collections::BTreeMap,
    future::Future,
    path::PathBuf,
    pin::pin,
    task::{Context, Poll, Waker},
    time::{Duration, SystemTime},
};

use anyhow::{anyhow, bail};
use aws_config::{
    Region,
    ecs::EcsCredentialsProvider,
    environment::EnvironmentVariableCredentialsProvider,
    meta::{credentials::CredentialsProviderChain, region::ProvideRegion},
    profile::{ProfileFileCredentialsProvider, ProfileFileRegionProvider},
    provider_config::ProviderConfig,
    web_identity_token::WebIdentityTokenCredentialsProvider,
};
pub use aws_credential_types::Credentials;
use aws_credential_types::provider::{ProvideCredentials, error::CredentialsError};
use aws_runtime::env_config::file::{EnvConfigFileKind, EnvConfigFiles};
use aws_sigv4::{
    http_request::{SignableBody, SignableRequest, SigningSettings},
    sign::v4,
};
use aws_smithy_async::{
    rt::sleep::{AsyncSleep, Sleep},
    time::StaticTimeSource,
};
use aws_smithy_runtime_api::{
    client::{
        http::{
            HttpClient, HttpConnector, HttpConnectorFuture, HttpConnectorSettings,
            SharedHttpConnector,
        },
        orchestrator::{HttpRequest, HttpResponse},
        result::ConnectorError,
        runtime_components::RuntimeComponents,
    },
    http::StatusCode,
};
use aws_smithy_types::{body::SdkBody, error::display::DisplayErrorContext};
use url::Url;

use crate::owlet::plugin::http::Response;

/// Looks up an environment variable; empty values count as unset.
pub type Env<'a> = &'a dyn Fn(&str) -> Option<String>;

/// Sends `(method, url, headers, body)`: owlet's http import, or a test mock.
pub type Http = fn(&str, &str, &[(String, String)], &[u8]) -> Result<Response, String>;

/// Cached temporary credentials are refreshed this long before they expire.
const REFRESH_MARGIN: Duration = Duration::from_secs(300);
/// Polls of an SDK future before giving up; they all complete synchronously.
const MAX_POLLS: usize = 1000;

/// Temporary credentials by profile (`None`: the default chain), kept with the
/// profile files they were resolved from, plus the HTTP route of the SDK.
#[derive(Debug)]
pub struct Cache {
    http: Http,
    entries: BTreeMap<Option<String>, (Credentials, ProfileFiles)>,
}

impl Cache {
    /// An empty cache whose SDK calls (STS, ECS) go through `http`.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// static CREDENTIALS: Mutex<sigv4::Cache> = Mutex::new(sigv4::Cache::new(owlet::plugin::http::send));
    /// ```
    pub const fn new(http: Http) -> Self {
        Self {
            http,
            entries: BTreeMap::new(),
        }
    }
}

/// Contents of the shared config and credentials files (`None`: unreadable).
type ProfileFiles = [Option<String>; 2];

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

/// Resolves credentials like `boto3.Session(profile_name=...)`: a named
/// profile (`profile`, else `$AWS_PROFILE`) reads only the profile files;
/// otherwise the SDK chain starts with the environment keys.
///
/// The profile files are re-read on every call, so an edited profile applies
/// at once. Temporary credentials stay in `cache` until
/// [`REFRESH_MARGIN`] before they expire or until the profile files change;
/// static ones are resolved afresh. `now` is the current time.
///
/// # Examples
///
/// ```ignore
/// let mut cache = sigv4::Cache::new(owlet::plugin::http::send);
/// let creds = sigv4::load_credentials(Some("dev"), &env, &mut cache, SystemTime::now())?;
/// ```
pub fn load_credentials(
    profile: Option<&str>,
    env: Env,
    cache: &mut Cache,
    now: SystemTime,
) -> anyhow::Result<Credentials> {
    let profile = profile.map(str::to_owned).or_else(|| env("AWS_PROFILE"));
    let files = read_profile_files(env);
    if let Some((creds, seen)) = cache.entries.get(&profile)
        && *seen == files
        && creds.expiry().is_some_and(|t| now + REFRESH_MARGIN < t)
    {
        return Ok(creds.clone());
    }

    let name = profile.as_deref().unwrap_or("default");
    // Region of the STS calls made by role profiles.
    let region = env("AWS_REGION")
        .or_else(|| env("AWS_DEFAULT_REGION"))
        .or_else(|| region_of(name, &files))
        .unwrap_or_else(|| "us-east-1".to_owned());
    let conf = ProviderConfig::empty()
        .with_http_client(Connector(cache.http))
        .with_sleep_impl(NoSleep)
        .with_time_source(StaticTimeSource::new(now))
        .with_region(Some(Region::new(region)));
    let from_files = ProfileFileCredentialsProvider::builder()
        .configure(&conf)
        .profile_files(env_config_files(&files))
        .profile_name(name)
        .build();
    let result = match profile {
        Some(_) => block_on(from_files.provide_credentials())?,
        None => {
            let chain = CredentialsProviderChain::first_try(
                "Environment",
                EnvironmentVariableCredentialsProvider::new(),
            )
            .or_else("Profile", from_files)
            .or_else(
                "WebIdentityToken",
                WebIdentityTokenCredentialsProvider::builder()
                    .configure(&conf)
                    .build(),
            )
            .or_else(
                "EcsContainer",
                EcsCredentialsProvider::builder().configure(&conf).build(),
            );
            block_on(chain.provide_credentials())?
        }
    };
    let creds = result.map_err(|e| {
        let what = match e {
            CredentialsError::CredentialsNotLoaded(_) => "no credentials",
            _ => "cannot load credentials",
        };
        // Without the `sso` feature the SDK reports a missing cargo feature,
        // which means nothing to the user.
        if needs_sso(&e) {
            return anyhow!(
                "sigv4: {what} for profile `{name}`: IAM Identity Center (SSO) profiles are unsupported"
            );
        }
        anyhow!(
            "sigv4: {what} for profile `{name}`: {}",
            DisplayErrorContext(&e)
        )
    })?;
    if creds.expiry().is_some() {
        cache.entries.insert(profile, (creds.clone(), files));
    }
    Ok(creds)
}

/// The `region` of `profile` (`None`: `default`) in the shared config file.
pub fn profile_region(profile: Option<&str>, env: Env) -> Option<String> {
    region_of(profile.unwrap_or("default"), &read_profile_files(env))
}

fn region_of(profile: &str, files: &ProfileFiles) -> Option<String> {
    let provider = ProfileFileRegionProvider::builder()
        .profile_files(env_config_files(files))
        .profile_name(profile)
        .build();
    block_on(provider.region())
        .ok()
        .flatten()
        .map(|r| r.to_string())
}

/// Whether the SDK failed because a profile in the chain is an IAM Identity
/// Center one, which needs the disabled `sso` feature. The SDK keeps only the
/// text of its `ProfileFileError`, so this matches the message.
fn needs_sso(error: &CredentialsError) -> bool {
    DisplayErrorContext(error)
        .to_string()
        .contains("cargo feature(s) enabled: sso.")
}

/// Reads the config and credentials files: `$AWS_CONFIG_FILE` /
/// `$AWS_SHARED_CREDENTIALS_FILE`, else under `$HOME/.aws`.
fn read_profile_files(env: Env) -> ProfileFiles {
    let read = |var: &str, default: &str| {
        let path = match env(var) {
            Some(path) => PathBuf::from(path),
            None => PathBuf::from(env("HOME")?).join(default),
        };
        std::fs::read_to_string(path).ok()
    };
    [
        read("AWS_CONFIG_FILE", ".aws/config"),
        read("AWS_SHARED_CREDENTIALS_FILE", ".aws/credentials"),
    ]
}

/// Hands already-read profile files to the SDK; missing ones count as empty
/// (the builder panics on an empty file set, so both are always given).
fn env_config_files(files: &ProfileFiles) -> EnvConfigFiles {
    let [config, credentials] = files;
    EnvConfigFiles::builder()
        .include_default_config_file(false)
        .include_default_credentials_file(false)
        .with_contents(
            EnvConfigFileKind::Config,
            config.as_deref().unwrap_or_default(),
        )
        .with_contents(
            EnvConfigFileKind::Credentials,
            credentials.as_deref().unwrap_or_default(),
        )
        .build()
}

/// Signs a request like botocore's `SigV4Auth.add_auth`: `host` and every
/// header in `headers` except `connection` are signed. Returns the headers to
/// add: `x-amz-date`, `authorization` and, with a session token,
/// `x-amz-security-token`.
///
/// # Examples
///
/// ```ignore
/// let extra = sigv4::sign(&creds, "POST", &url, &headers, body, "us-east-1", "aws-mcp", SystemTime::now())?;
/// headers.extend(extra);
/// ```
#[allow(clippy::too_many_arguments)]
pub fn sign(
    creds: &Credentials,
    method: &str,
    url: &Url,
    headers: &[(String, String)],
    body: &[u8],
    region: &str,
    service: &str,
    now: SystemTime,
) -> anyhow::Result<Vec<(String, String)>> {
    let identity = creds.clone().into();
    let params = v4::SigningParams::builder()
        .identity(&identity)
        .region(region)
        .name(service)
        .time(now)
        .settings(SigningSettings::default())
        .build()?
        .into();
    let request = SignableRequest::new(
        method,
        url.as_str(),
        headers
            .iter()
            .filter(|(k, _)| !k.eq_ignore_ascii_case("connection"))
            .map(|(k, v)| (k.as_str(), v.as_str())),
        SignableBody::Bytes(body),
    )?;
    let (instructions, _) = aws_sigv4::http_request::sign(request, &params)?.into_parts();
    Ok(instructions
        .headers()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect())
}

/// Runs an SDK future to completion. Every one completes without waiting:
/// HTTP goes through the blocking import and [`NoSleep`] returns at once.
fn block_on<F: Future>(future: F) -> anyhow::Result<F::Output> {
    let mut future = pin!(future);
    let mut cx = Context::from_waker(Waker::noop());
    for _ in 0..MAX_POLLS {
        if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
            return Ok(output);
        }
    }
    bail!("sigv4: an AWS SDK call did not complete")
}

/// SDK sleeps (retry backoff) return at once: WASI has no timer to wait on.
#[derive(Debug)]
struct NoSleep;

impl AsyncSleep for NoSleep {
    fn sleep(&self, _: Duration) -> Sleep {
        Sleep::new(std::future::ready(()))
    }
}

/// The SDK's HTTP client: owlet's http import, which is synchronous.
#[derive(Debug, Clone, Copy)]
struct Connector(Http);

impl HttpClient for Connector {
    fn http_connector(
        &self,
        _: &HttpConnectorSettings,
        _: &RuntimeComponents,
    ) -> SharedHttpConnector {
        SharedHttpConnector::new(*self)
    }
}

impl HttpConnector for Connector {
    fn call(&self, request: HttpRequest) -> HttpConnectorFuture {
        HttpConnectorFuture::ready(self.send(&request))
    }
}

impl Connector {
    /// Errors are `other`, not `io`: the SDK retries io errors at once under
    /// [`NoSleep`], each attempt blocking up to the host's HTTP timeout.
    fn send(&self, request: &HttpRequest) -> Result<HttpResponse, ConnectorError> {
        let other = |e: String| ConnectorError::other(e.into(), None);
        let headers: Vec<(String, String)> = request
            .headers()
            .iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        let body = request
            .body()
            .bytes()
            .ok_or_else(|| other("sigv4: streaming request bodies are unsupported".to_owned()))?;
        let resp = (self.0)(request.method(), request.uri(), &headers, body).map_err(other)?;
        let status = StatusCode::try_from(resp.status).map_err(|e| other(e.to_string()))?;
        let mut out = HttpResponse::new(status, SdkBody::from(resp.body));
        for (k, v) in resp.headers {
            out.headers_mut()
                .try_append(k, v)
                .map_err(|e| other(e.to_string()))?;
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        sync::atomic::{AtomicUsize, Ordering},
        time::UNIX_EPOCH,
    };

    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).expect("url")
    }

    fn offline(_: &str, url: &str, _: &[(String, String)], _: &[u8]) -> Result<Response, String> {
        Err(format!("unexpected request to {url}"))
    }

    /// 2024-01-01T00:00:00Z.
    fn now() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_704_067_200)
    }

    /// A temp dir with `credentials` and `config`, and an env pointing at them.
    struct Files {
        dir: PathBuf,
        vars: HashMap<&'static str, String>,
    }

    impl Files {
        fn new(name: &str, credentials: &str, config: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("owlet-aws-{name}-{}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("mkdir");
            let mut vars = HashMap::new();
            for (var, file, text) in [
                ("AWS_SHARED_CREDENTIALS_FILE", "credentials", credentials),
                ("AWS_CONFIG_FILE", "config", config),
            ] {
                let path = dir.join(file);
                std::fs::write(&path, text).expect("write");
                vars.insert(var, path.to_str().expect("utf-8").to_owned());
            }
            Self { dir, vars }
        }

        fn env(&self, k: &str) -> Option<String> {
            self.vars.get(k).cloned()
        }
    }

    impl Drop for Files {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
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
    fn static_profiles_and_regions() {
        let files = Files::new(
            "static",
            "[default]\naws_access_key_id = AKIADEFAULT\naws_secret_access_key = d\n\
             [split]\naws_access_key_id = AKIASPLIT\naws_secret_access_key = s\n",
            "[default]\nregion = eu-west-1\n\
             [profile staging]\naws_access_key_id = AKIASTAGE\naws_secret_access_key = s\n\
             aws_session_token = t\nregion = ap-east-1\n\
             [profile proc]\ncredential_process = /bin/creds\n",
        );
        let env = |k: &str| files.env(k);
        let mut cache = Cache::new(offline);
        let mut load = |profile| load_credentials(profile, &env, &mut cache, now());

        // The default chain reads the environment keys from the real process
        // environment (the SDK offers no override), so only check it without.
        if std::env::var_os("AWS_ACCESS_KEY_ID").is_none() {
            assert_eq!(load(None).expect("default").access_key_id(), "AKIADEFAULT");
        }
        let default = load(Some("default")).expect("default profile");
        assert_eq!(default.access_key_id(), "AKIADEFAULT");
        let staging = load(Some("staging")).expect("config profile");
        assert_eq!(staging.access_key_id(), "AKIASTAGE");
        assert_eq!(staging.session_token(), Some("t"));
        assert_eq!(
            load(Some("split")).expect("split").access_key_id(),
            "AKIASPLIT"
        );
        let err = format!("{:#}", load(Some("missing")).expect_err("no such profile"));
        assert!(err.contains("profile `missing`"), "{err}");
        assert!(
            load(Some("proc")).is_err(),
            "credential_process is unsupported"
        );

        let profile_env = |k: &str| match k {
            "AWS_PROFILE" => Some("staging".to_owned()),
            _ => files.env(k),
        };
        let creds = load_credentials(None, &profile_env, &mut Cache::new(offline), now());
        assert_eq!(creds.expect("AWS_PROFILE").access_key_id(), "AKIASTAGE");

        assert_eq!(profile_region(None, &env).as_deref(), Some("eu-west-1"));
        assert_eq!(
            profile_region(Some("staging"), &env).as_deref(),
            Some("ap-east-1")
        );
        assert_eq!(profile_region(Some("split"), &env), None);
        assert_eq!(profile_region(None, &|_| None), None);
    }

    #[test]
    fn sso_profiles_are_unsupported() {
        let files = Files::new(
            "sso",
            "",
            "[profile sso]\nsso_session = s\nsso_account_id = 123456789012\n\
             sso_role_name = Admin\nregion = eu-west-1\n\
             [profile legacy]\nsso_start_url = https://example.awsapps.com/start\n\
             sso_region = eu-west-1\nsso_account_id = 123456789012\nsso_role_name = Admin\n\
             [profile admin]\nrole_arn = arn:aws:iam::123456789012:role/Admin\n\
             source_profile = sso\n\
             [sso-session s]\nsso_start_url = https://example.awsapps.com/start\n\
             sso_region = eu-west-1\n\
             [profile proc]\ncredential_process = /bin/creds\n",
        );
        let env = |k: &str| files.env(k);
        let mut cache = Cache::new(offline);
        for profile in ["sso", "legacy", "admin"] {
            let err = format!(
                "{:#}",
                load_credentials(Some(profile), &env, &mut cache, now()).expect_err("sso")
            );
            assert!(
                err.contains("IAM Identity Center (SSO) profiles are unsupported"),
                "{err}"
            );
        }
        let err = format!(
            "{:#}",
            load_credentials(Some("proc"), &env, &mut cache, now()).expect_err("unsupported")
        );
        assert!(!err.contains("SSO"), "{err}");
    }

    static STS_CALLS: AtomicUsize = AtomicUsize::new(0);

    /// STS: `AssumeRole` signed by the source profile, issuing `ASIAROLE`.
    fn sts(
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: &[u8],
    ) -> Result<Response, String> {
        STS_CALLS.fetch_add(1, Ordering::SeqCst);
        let auth = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
            .map(|(_, v)| v.as_str())
            .unwrap_or_default();
        let body = String::from_utf8_lossy(body);
        assert_eq!(method, "POST");
        assert_eq!(url, "https://sts.eu-west-1.amazonaws.com/");
        assert!(auth.contains("Credential=AKIASOURCE/"), "{auth}");
        assert!(body.contains("Action=AssumeRole"), "{body}");
        assert!(body.contains("RoleSessionName=owlet"), "{body}");
        let xml = "<AssumeRoleResponse xmlns=\"https://sts.amazonaws.com/doc/2011-06-15/\">\
             <AssumeRoleResult><AssumedRoleUser><AssumedRoleId>AROA:owlet</AssumedRoleId>\
             <Arn>arn:aws:sts::123456789012:assumed-role/Admin/owlet</Arn></AssumedRoleUser>\
             <Credentials><AccessKeyId>ASIAROLE</AccessKeyId>\
             <SecretAccessKey>role-secret</SecretAccessKey><SessionToken>role-token</SessionToken>\
             <Expiration>2024-01-01T01:00:00Z</Expiration></Credentials></AssumeRoleResult>\
             <ResponseMetadata><RequestId>r</RequestId></ResponseMetadata></AssumeRoleResponse>";
        Ok(Response {
            status: 200,
            headers: vec![("content-type".to_owned(), "text/xml".to_owned())],
            body: xml.as_bytes().to_vec(),
        })
    }

    #[test]
    fn assume_role_goes_through_http_and_is_cached() {
        let config = "[profile admin]\nrole_arn = arn:aws:iam::123456789012:role/Admin\n\
                      source_profile = source\nrole_session_name = owlet\nregion = eu-west-1\n";
        let files = Files::new(
            "assume",
            "[source]\naws_access_key_id = AKIASOURCE\naws_secret_access_key = s\n",
            config,
        );
        let env = |k: &str| files.env(k);
        let mut cache = Cache::new(sts);

        let creds = load_credentials(Some("admin"), &env, &mut cache, now()).expect("role");
        assert_eq!(creds.access_key_id(), "ASIAROLE");
        assert_eq!(creds.session_token(), Some("role-token"));
        assert_eq!(STS_CALLS.load(Ordering::SeqCst), 1);

        // Cached while valid and the files are unchanged.
        let later = now() + Duration::from_secs(600);
        load_credentials(Some("admin"), &env, &mut cache, later).expect("cached");
        assert_eq!(STS_CALLS.load(Ordering::SeqCst), 1);
        // Refreshed shortly before expiry.
        let late = now() + Duration::from_secs(3600) - REFRESH_MARGIN;
        load_credentials(Some("admin"), &env, &mut cache, late).expect("refreshed");
        assert_eq!(STS_CALLS.load(Ordering::SeqCst), 2);
        // An edited profile applies at once.
        std::fs::write(
            &files.vars["AWS_CONFIG_FILE"],
            format!("{config}# edited\n"),
        )
        .expect("write");
        load_credentials(Some("admin"), &env, &mut cache, later).expect("re-resolved");
        assert_eq!(STS_CALLS.load(Ordering::SeqCst), 3);
    }

    static FAILED_CALLS: AtomicUsize = AtomicUsize::new(0);

    fn unreachable(_: &str, _: &str, _: &[(String, String)], _: &[u8]) -> Result<Response, String> {
        FAILED_CALLS.fetch_add(1, Ordering::SeqCst);
        Err("timed out".to_owned())
    }

    #[test]
    fn sdk_http_errors_are_not_retried() {
        let files = Files::new(
            "retry",
            "[source]\naws_access_key_id = AKIASOURCE\naws_secret_access_key = s\n",
            "[profile admin]\nrole_arn = arn:aws:iam::123456789012:role/Admin\n\
             source_profile = source\n",
        );
        let env = |k: &str| files.env(k);
        let err = load_credentials(Some("admin"), &env, &mut Cache::new(unreachable), now())
            .expect_err("STS unreachable");
        assert!(format!("{err:#}").contains("timed out"), "{err:#}");
        assert_eq!(FAILED_CALLS.load(Ordering::SeqCst), 1);
    }

    fn example_creds() -> Credentials {
        Credentials::new(
            "AKIDEXAMPLE",
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            None,
            None,
            "test",
        )
    }

    fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
        headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// The `get-vanilla` case of the AWS SigV4 test suite.
    #[test]
    fn signs_the_aws_test_suite_vector() {
        let time = UNIX_EPOCH + Duration::from_secs(1_440_938_160); // 20150830T123600Z
        let signed = sign(
            &example_creds(),
            "GET",
            &url("https://example.amazonaws.com/"),
            &[],
            b"",
            "us-east-1",
            "service",
            time,
        )
        .expect("sign");
        assert_eq!(header(&signed, "x-amz-date"), Some("20150830T123600Z"));
        assert_eq!(
            header(&signed, "authorization"),
            Some(
                "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, \
                 SignedHeaders=host;x-amz-date, \
                 Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
            )
        );
        assert_eq!(header(&signed, "x-amz-security-token"), None);
    }

    /// The same signature the previous hand-written signer produced.
    #[test]
    fn signs_an_mcp_post() {
        let signed = sign(
            &example_creds(),
            "POST",
            &url("https://aws-mcp.us-east-1.api.aws/mcp"),
            &[("content-type".to_owned(), "application/json".to_owned())],
            b"{}",
            "us-east-1",
            "aws-mcp",
            now(),
        )
        .expect("sign");
        assert_eq!(
            header(&signed, "authorization"),
            Some(
                "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20240101/us-east-1/aws-mcp/aws4_request, \
                 SignedHeaders=content-type;host;x-amz-date, \
                 Signature=2a5ec64b784002f4194bd832ff4094eb7c35b7881b825596ef03de397285a685"
            )
        );
    }

    #[test]
    fn signs_headers_and_session_token() {
        let creds = Credentials::new("AKID", "secret", Some("token-value".to_owned()), None, "t");
        let headers = [
            ("content-type".to_owned(), "application/json".to_owned()),
            ("connection".to_owned(), "keep-alive".to_owned()),
            ("mcp-session-id".to_owned(), "s1".to_owned()),
        ];
        let u = url("http://127.0.0.1:8080/mcp?b=2&a=1");
        let signed = sign(
            &creds,
            "POST",
            &u,
            &headers,
            b"{}",
            "us-east-1",
            "aws-mcp",
            now(),
        )
        .expect("sign");
        assert_eq!(header(&signed, "x-amz-security-token"), Some("token-value"));
        let auth = header(&signed, "authorization").expect("authorization");
        assert!(
            auth.contains(
                "SignedHeaders=content-type;host;mcp-session-id;x-amz-date;x-amz-security-token,"
            ),
            "{auth}"
        );
        let other = sign(
            &creds,
            "POST",
            &u,
            &headers,
            b"{\"a\":1}",
            "us-east-1",
            "aws-mcp",
            now(),
        )
        .expect("sign");
        assert_ne!(signed, other, "the body is signed");
    }
}
