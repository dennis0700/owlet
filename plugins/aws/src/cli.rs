//! Plugin configuration from WASI argv, mirroring the `mcp-proxy-for-aws`
//! options a WASI plugin can support:
//! `<endpoint> [--service S] [--region R] [--profile P ...] [--metadata K=V ...]
//! [--read-only]`. With two or more profiles (`--profile a b` or
//! `$AWS_MCP_PROXY_PROFILES`), auth tools accept an `aws_profile` argument.

use serde_json::{Map, Value};
use url::Url;

use crate::sigv4::{self, Env, parse_endpoint};

/// Resolved proxy settings.
#[derive(Debug, Clone, PartialEq)]
pub struct Options {
    pub endpoint: Url,
    /// Service name in the SigV4 credential scope.
    pub service: String,
    /// Region in the SigV4 credential scope.
    pub region: String,
    /// The default profile, used for every request without `aws_profile`.
    pub profile: Profile,
    /// The other allowed profiles; non-empty enables profile switching.
    pub extra_profiles: Vec<Profile>,
    /// Only expose and allow tools annotated with `readOnlyHint: true`.
    pub read_only: bool,
}

/// One AWS identity requests can be signed with.
#[derive(Debug, Clone, PartialEq)]
pub struct Profile {
    /// Named profile for shared-file credentials; `None` is the default chain.
    pub name: Option<String>,
    /// Injected into `params._meta` of every request: `--metadata` plus this
    /// profile's `AWS_REGION` default. Keys already in the request win.
    pub metadata: Map<String, Value>,
}

/// Parses the arguments after the program name.
///
/// Profiles: `$AWS_MCP_PROXY_PROFILES` (whitespace-separated), else the last
/// `--profile`, else `$AWS_PROFILE`; duplicates are dropped and the first is
/// the default. Region resolution for signing: `--region`, else the endpoint
/// host, else `$AWS_REGION`. Each profile's `AWS_REGION` metadata defaults to
/// `$AWS_REGION` / `$AWS_DEFAULT_REGION`, else the profile's `region` in the
/// config file, else the endpoint region, else the signing region.
pub fn parse(args: &[String], env: Env) -> Result<Options, String> {
    let mut endpoint = None;
    let mut service = None;
    let mut region = None;
    let mut profiles = Vec::new();
    let mut metadata = Map::new();
    let mut read_only = false;

    let mut iter = args.iter().peekable();
    while let Some(arg) = iter.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => (flag, Some(value.to_owned())),
            _ => (arg.as_str(), None),
        };
        let mut value = |name: &str| {
            inline
                .clone()
                .or_else(|| iter.next_if(|a| !a.starts_with("--")).cloned())
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match flag {
            "--service" => service = Some(value("--service")?),
            "--region" => region = Some(value("--region")?),
            "--profile" => {
                // Like argparse: a repeated flag replaces the earlier list.
                profiles.clear();
                profiles.extend(inline.clone());
                // A trailing endpoint is not a profile (`--profile a b https://...`).
                while let Some(item) = iter.next_if(|a| !a.starts_with("--") && !a.contains("://"))
                {
                    profiles.push(item.clone());
                }
                if profiles.is_empty() {
                    return Err("--profile needs a value".to_owned());
                }
            }
            "--read-only" if inline.is_none() => read_only = true,
            "--read-only" => return Err("--read-only takes no value".to_owned()),
            "--metadata" => {
                let mut items: Vec<String> = inline.clone().into_iter().collect();
                while let Some(item) = iter.next_if(|a| !a.starts_with("--")) {
                    items.push(item.clone());
                }
                for item in items {
                    let (k, v) = item
                        .split_once('=')
                        .ok_or_else(|| format!("--metadata must be key=value, got `{item}`"))?;
                    metadata.insert(k.to_owned(), Value::String(v.to_owned()));
                }
            }
            f if f.starts_with("--") => return Err(format!("unsupported option `{f}`")),
            _ if endpoint.is_none() => endpoint = Some(arg.clone()),
            _ => return Err(format!("unexpected argument `{arg}`")),
        }
    }

    let endpoint = endpoint.ok_or("missing <endpoint> argument")?;
    let endpoint = validate_endpoint(&endpoint)?;
    let (host_service, host_region) = parse_endpoint(&endpoint);
    let service = service
        .or(host_service)
        .ok_or("could not infer the service from the endpoint; pass --service")?;
    let region = region
        .or_else(|| host_region.clone())
        .or_else(|| env("AWS_REGION"))
        .ok_or("could not infer the region from the endpoint or AWS_REGION; pass --region")?;

    let profiles = match env("AWS_MCP_PROXY_PROFILES") {
        Some(list) => list.split_whitespace().map(str::to_owned).collect(),
        None if !profiles.is_empty() => profiles,
        None => env("AWS_PROFILE").into_iter().collect(),
    };
    let mut names: Vec<Option<String>> = Vec::new();
    for name in profiles {
        if !names.iter().any(|n| n.as_deref() == Some(name.as_str())) {
            names.push(Some(name));
        }
    }
    if names.is_empty() {
        names.push(None);
    }
    let mut profiles = names.into_iter().map(|name| {
        let mut metadata = metadata.clone();
        if !metadata.contains_key("AWS_REGION") {
            let backend = env("AWS_REGION")
                .or_else(|| env("AWS_DEFAULT_REGION"))
                .or_else(|| sigv4::profile_region(name.as_deref(), env))
                .or_else(|| host_region.clone())
                .unwrap_or_else(|| region.clone());
            metadata.insert("AWS_REGION".to_owned(), Value::String(backend));
        }
        Profile { name, metadata }
    });
    let profile = profiles.next().ok_or("no profile")?;
    let extra_profiles = profiles.collect();
    Ok(Options {
        endpoint,
        service,
        region,
        profile,
        extra_profiles,
        read_only,
    })
}

/// Credentials travel in signed headers, so only https is accepted, plus http
/// to the local host for development.
fn validate_endpoint(endpoint: &str) -> Result<Url, String> {
    let url = Url::parse(endpoint).map_err(|e| format!("invalid endpoint `{endpoint}`: {e}"))?;
    let local = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    match url.scheme() {
        "https" => Ok(url),
        "http" if local => Ok(url),
        "http" => Err(format!(
            "endpoint `{endpoint}` must use https; http is only allowed for localhost"
        )),
        other => Err(format!(
            "endpoint `{endpoint}` has unsupported scheme `{other}`"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn parses_the_uvx_style_command_line() {
        let opts = parse(
            &args(&[
                "https://aws-mcp.us-east-1.api.aws/mcp",
                "--metadata",
                "AWS_REGION=us-west-2",
                "K=V",
            ]),
            &no_env,
        )
        .expect("parse");
        assert_eq!(opts.service, "aws-mcp");
        assert_eq!(opts.region, "us-east-1");
        assert_eq!(opts.profile.metadata["AWS_REGION"], "us-west-2");
        assert_eq!(opts.profile.metadata["K"], "V");
        assert_eq!(opts.profile.name, None);
        assert!(opts.extra_profiles.is_empty());
        assert!(!opts.read_only);
    }

    #[test]
    fn explicit_flags_and_repeated_metadata() {
        let opts = parse(
            &args(&[
                "--region=eu-west-1",
                "--service",
                "bedrock",
                "--metadata=A=1",
                "--metadata",
                "B=x=y",
                "--profile",
                "dev",
                "--read-only",
                "https://aws-mcp.us-east-1.api.aws/mcp",
            ]),
            &no_env,
        )
        .expect("parse");
        assert_eq!(
            (opts.region.as_str(), opts.service.as_str()),
            ("eu-west-1", "bedrock")
        );
        assert_eq!(opts.profile.metadata["A"], "1");
        assert_eq!(opts.profile.metadata["B"], "x=y");
        assert_eq!(opts.profile.name.as_deref(), Some("dev"));
        assert!(opts.read_only);
    }

    #[test]
    fn injects_aws_region_metadata() {
        let endpoint = args(&["https://aws-mcp.us-east-1.api.aws/mcp"]);
        let opts = parse(&endpoint, &no_env).expect("parse");
        assert_eq!(opts.profile.metadata["AWS_REGION"], "us-east-1");

        let env = |k: &str| match k {
            "AWS_REGION" => Some("ap-south-1".to_owned()),
            "AWS_PROFILE" => Some("p".to_owned()),
            _ => None,
        };
        let opts = parse(&endpoint, &env).expect("parse");
        assert_eq!(opts.profile.metadata["AWS_REGION"], "ap-south-1");
        // The signing region still follows the endpoint.
        assert_eq!(opts.region, "us-east-1");
        assert_eq!(opts.profile.name.as_deref(), Some("p"));

        // No endpoint region: signing falls back to AWS_REGION.
        let opts = parse(&args(&["https://example.com/mcp"]), &env).expect("parse");
        assert_eq!(
            (opts.service.as_str(), opts.region.as_str()),
            ("example", "ap-south-1")
        );
    }

    #[test]
    fn rejects_bad_input() {
        let local = "http://127.0.0.1:9/mcp";
        assert!(parse(&args(&["http://aws-mcp.us-east-1.api.aws/mcp"]), &no_env).is_err());
        assert!(parse(&args(&["ftp://h/mcp"]), &no_env).is_err());
        assert!(parse(&args(&[]), &no_env).is_err());
        assert!(
            parse(&args(&[local, "--service", "s"]), &no_env).is_err(),
            "no region"
        );
        assert!(parse(&args(&[local, "--region", "r"]), &no_env).is_ok());
        assert!(parse(&args(&["http://localhost/mcp", "--region", "r"]), &no_env).is_ok());
        assert!(parse(&args(&[local, "--region"]), &no_env).is_err());
        assert!(parse(&args(&[local, "--region", "r", "--metadata", "x"]), &no_env).is_err());
        assert!(parse(&args(&[local, "--region", "r", "--read-only=1"]), &no_env).is_err());
        assert!(parse(&args(&[local, "--region", "r", "extra"]), &no_env).is_err());
        assert!(parse(&args(&[local, "--region", "r", "--profile"]), &no_env).is_err());
    }

    fn names(opts: &Options) -> Vec<&str> {
        std::iter::once(&opts.profile)
            .chain(&opts.extra_profiles)
            .filter_map(|p| p.name.as_deref())
            .collect()
    }

    #[test]
    fn multiple_profiles() {
        let url = "https://aws-mcp.us-east-1.api.aws/mcp";
        let opts = parse(&args(&["--profile", "a", "b", "a", url]), &no_env).expect("parse");
        assert_eq!(opts.endpoint.as_str(), url);
        assert_eq!(names(&opts), ["a", "b"], "deduplicated, first is default");
        let opts = parse(&args(&[url, "--profile=a", "b", "--read-only"]), &no_env).expect("parse");
        assert_eq!(names(&opts), ["a", "b"]);
        assert!(opts.read_only);
        let opts =
            parse(&args(&["--profile", "a", "--profile", "b", url]), &no_env).expect("parse");
        assert_eq!(
            names(&opts),
            ["b"],
            "the last --profile wins, as in argparse"
        );
        assert!(opts.extra_profiles.is_empty());

        let env = |k: &str| match k {
            "AWS_MCP_PROXY_PROFILES" => Some(" x  y\tx ".to_owned()),
            "AWS_PROFILE" => Some("p".to_owned()),
            _ => None,
        };
        let opts = parse(&args(&[url, "--profile", "a", "b"]), &env).expect("parse");
        assert_eq!(names(&opts), ["x", "y"], "the env list wins over --profile");

        let env = |k: &str| (k == "AWS_PROFILE").then(|| "p".to_owned());
        assert_eq!(names(&parse(&args(&[url]), &env).expect("parse")), ["p"]);
        let opts = parse(&args(&[url, "--profile", "a"]), &env).expect("parse");
        assert_eq!(names(&opts), ["a"], "--profile wins over AWS_PROFILE");
        assert!(opts.extra_profiles.is_empty());
    }

    #[test]
    fn per_profile_region_metadata() {
        let dir = std::env::temp_dir().join(format!("owlet-aws-cli-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let config = dir.join("config");
        std::fs::write(
            &config,
            "[default]\nregion = eu-west-1\n[profile b]\nregion = ap-east-1\n[profile c]\n",
        )
        .expect("write");
        let config = config.to_str().expect("utf-8").to_owned();
        let env = |k: &str| (k == "AWS_CONFIG_FILE").then(|| config.clone());
        let url = "https://aws-mcp.us-east-1.api.aws/mcp";
        let region = |p: &Profile| p.metadata["AWS_REGION"].clone();

        let opts = parse(&args(&[url, "--profile", "default", "b", "c"]), &env).expect("parse");
        assert_eq!(region(&opts.profile), "eu-west-1");
        assert_eq!(region(&opts.extra_profiles[0]), "ap-east-1");
        assert_eq!(
            region(&opts.extra_profiles[1]),
            "us-east-1",
            "endpoint fallback"
        );
        // The default chain reads `[default]` too.
        assert_eq!(
            region(&parse(&args(&[url]), &env).expect("parse").profile),
            "eu-west-1"
        );

        let env_region = |k: &str| match k {
            "AWS_DEFAULT_REGION" => Some("sa-east-1".to_owned()),
            _ => env(k),
        };
        let opts = parse(&args(&[url, "--profile", "b"]), &env_region).expect("parse");
        assert_eq!(region(&opts.profile), "sa-east-1", "environment wins");
        let opts = parse(
            &args(&[url, "--metadata", "AWS_REGION=x", "--profile", "b", "c"]),
            &env,
        )
        .expect("parse");
        assert_eq!(region(&opts.profile), "x", "--metadata wins");
        assert_eq!(region(&opts.extra_profiles[0]), "x");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
