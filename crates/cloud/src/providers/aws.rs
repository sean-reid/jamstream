//! AWS EC2 provider. One short-lived jam-session VM per launch, driven
//! through the EC2 Query API (form-encoded `Action=...` posts, Version
//! 2016-11-15) signed with AWS Signature Version 4. The workspace carries
//! no AWS SDK or hash crates on purpose, so SHA-256, HMAC-SHA256, and the
//! SigV4 dance live in the private `sigv4` module below, pinned by FIPS
//! 180-4, RFC 4231, and AWS test-suite vectors. Responses are small, flat
//! XML documents parsed with targeted tag extraction rather than an XML
//! dependency.
//!
//! Self-destruct story: instances launch with
//! `InstanceInitiatedShutdownBehavior=terminate`, so the cloud-init
//! `SelfDestruct::AwsShutdown` path (`shutdown -h now`) terminates the VM
//! for good with no credentials on the box.
//!
//! Image selection: launch resolves the current Debian 12 arm64 AMI at
//! runtime from the public SSM parameter
//! `/aws/service/debian/release/12/latest/arm64` (JSON protocol, signed
//! with service "ssm"), cached per region; the ids bundled in
//! `data/aws_prices.json` are the fallback when SSM is unreachable.

use std::collections::HashMap;
use std::fmt;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use data_encoding::BASE64;
use serde::Deserialize;

use crate::artifact::ServerArch;
use crate::http;
use crate::provider::{Provider, ProviderError, Result};
use crate::types::{
    ANY_IPV4, ANY_IPV6, DEFAULT_SESSION_PORT, IngressRule, Instance, InstanceClass, LaunchSpec,
    Price, ProviderKind, Region, RegionId, SESSION_TAG_KEY,
};

const EC2_API_VERSION: &str = "2016-11-15";
const CONTENT_TYPE: &str = "application/x-www-form-urlencoded; charset=utf-8";

/// SSM JSON protocol pieces for GetParameter.
const SSM_CONTENT_TYPE: &str = "application/x-amz-json-1.1";
const SSM_TARGET_GET_PARAMETER: &str = "AmazonSSM.GetParameter";
/// Public SSM parameter holding the latest Debian 12 arm64 AMI per region.
const DEBIAN_12_ARM64_PARAM: &str = "/aws/service/debian/release/12/latest/arm64";

/// Static region catalog: (region id, display name, country).
const REGIONS: &[(&str, &str, &str)] = &[
    ("us-east-1", "N. Virginia", "US"),
    ("us-east-2", "Ohio", "US"),
    ("us-west-1", "N. California", "US"),
    ("us-west-2", "Oregon", "US"),
    ("eu-west-1", "Ireland", "IE"),
    ("eu-central-1", "Frankfurt", "DE"),
    ("ca-central-1", "Canada", "CA"),
    ("sa-east-1", "Sao Paulo", "BR"),
];

/// Bundled pricing and image catalog; see data/aws_prices.json for the
/// refresh cadence.
#[derive(Debug, Deserialize)]
struct AwsData {
    egress_microusd_per_gb: u64,
    included_egress_gb: u32,
    regions: HashMap<String, AwsRegionData>,
}

#[derive(Debug, Deserialize)]
struct AwsRegionData {
    small_hourly_microusd: u64,
    medium_hourly_microusd: u64,
    ami: String,
}

/// The slice of the SSM GetParameter response we need.
#[derive(Debug, Deserialize)]
struct SsmGetParameterResponse {
    #[serde(rename = "Parameter")]
    parameter: SsmParameter,
}

#[derive(Debug, Deserialize)]
struct SsmParameter {
    #[serde(rename = "Value")]
    value: String,
}

fn data() -> &'static AwsData {
    static DATA: OnceLock<AwsData> = OnceLock::new();
    DATA.get_or_init(|| {
        serde_json::from_str(include_str!("../../data/aws_prices.json"))
            .expect("bundled data/aws_prices.json must parse")
    })
}

/// Gzips the rendered cloud-init so it fits RunInstances' 16384-byte user
/// data cap; see the call site.
fn gzip(bytes: &[u8]) -> Result<Vec<u8>> {
    use std::io::Write;
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    enc.write_all(bytes)
        .and_then(|()| enc.finish())
        .map_err(|e| ProviderError::Other(format!("gzip user data: {e}")))
}

/// Maps the provider-agnostic size hint to a concrete Graviton type.
fn instance_type(class: InstanceClass) -> &'static str {
    match class {
        InstanceClass::Small => "t4g.small",
        InstanceClass::Standard => "t4g.medium",
    }
}

#[derive(Clone)]
pub struct AwsProvider {
    access_key_id: String,
    secret_access_key: String,
    /// Test override: one base URL used for every region instead of the
    /// per-region `https://{service}.{region}.amazonaws.com` endpoint.
    base_url: Option<String>,
    /// The one port the per-session security group opens.
    session_port: u16,
    http: reqwest::Client,
    /// SSM-resolved AMI id per region, shared across clones so one
    /// resolution serves every launch this provider instance makes.
    ami_cache: Arc<Mutex<HashMap<String, String>>>,
}

/// Manual Debug: the secret key (and anything derived from it, like an
/// Authorization header) must never reach logs.
impl fmt::Debug for AwsProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AwsProvider")
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &"<redacted>")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl AwsProvider {
    pub fn new(access_key_id: String, secret_access_key: String) -> Self {
        AwsProvider {
            access_key_id,
            secret_access_key,
            base_url: None,
            session_port: DEFAULT_SESSION_PORT,
            http: http::client(),
            ami_cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Session UDP port the security group opens; see
    /// [`Provider::session_port`].
    pub fn with_session_port(mut self, port: u16) -> Self {
        self.session_port = port;
        self
    }

    /// Credentials from AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY.
    pub fn from_env() -> Result<Self> {
        Self::from_env_lookup(|key| std::env::var(key).ok())
    }

    /// Injectable lookup so the env path is testable without mutating
    /// process-global state (unsafe in edition 2024, racy under the
    /// parallel test runner).
    fn from_env_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let read =
            |key: &str| get(key).ok_or_else(|| ProviderError::Auth(format!("{key} is not set")));
        Ok(Self::new(
            read("AWS_ACCESS_KEY_ID")?,
            read("AWS_SECRET_ACCESS_KEY")?,
        ))
    }

    /// Overrides the per-region endpoint with a single base URL (tests).
    /// The region then rides along as a `?region=...` query parameter so
    /// one mock server can still tell regions apart.
    pub fn with_base_url(mut self, url: String) -> Self {
        self.base_url = Some(url.trim_end_matches('/').to_owned());
        self
    }

    fn catalog_region(&self, id: &RegionId) -> Result<Region> {
        REGIONS
            .iter()
            .find(|(rid, _, _)| *rid == id.as_str())
            .map(|(rid, display, country)| Region {
                provider: ProviderKind::Aws,
                id: RegionId::new(*rid),
                display: (*display).to_owned(),
                country: (*country).to_owned(),
            })
            .ok_or_else(|| ProviderError::NotFound(format!("aws region {id}")))
    }

    /// Endpoint URL and the query string appended to it for one AWS
    /// service. The real per-region endpoint takes no query; under a
    /// base_url override the region is carried as a query parameter (and
    /// signed as such).
    fn endpoint(&self, service: &str, region: &str) -> (String, String) {
        match &self.base_url {
            Some(base) => (base.clone(), format!("region={}", aws_encode(region))),
            None => (
                format!("https://{service}.{region}.amazonaws.com"),
                String::new(),
            ),
        }
    }

    /// Full request URL, host header value, and canonical query for one
    /// signed call against `service` in `region`.
    fn signed_url(&self, service: &str, region: &str) -> Result<(String, String, String)> {
        let (endpoint, query) = self.endpoint(service, region);
        let url = if query.is_empty() {
            format!("{endpoint}/")
        } else {
            format!("{endpoint}/?{query}")
        };
        let parsed = reqwest::Url::parse(&url)
            .map_err(|e| ProviderError::Other(format!("bad endpoint {endpoint}: {e}")))?;
        let host = match (parsed.host_str(), parsed.port()) {
            (Some(h), Some(p)) => format!("{h}:{p}"),
            (Some(h), None) => h.to_owned(),
            (None, _) => {
                return Err(ProviderError::Other(format!(
                    "endpoint {endpoint} has no host"
                )));
            }
        };
        Ok((url, host, query))
    }

    /// One signed EC2 Query API call. Every request goes through
    /// `http::send_retrying`; the request is re-signed per attempt so
    /// x-amz-date stays fresh across backoff.
    async fn ec2_call(
        &self,
        region: &str,
        action: &str,
        params: &[(String, String)],
    ) -> Result<String> {
        let (url, host, query) = self.signed_url("ec2", region)?;

        let mut body = format!("Action={}&Version={EC2_API_VERSION}", aws_encode(action));
        for (key, value) in params {
            body.push('&');
            body.push_str(&aws_encode(key));
            body.push('=');
            body.push_str(&aws_encode(value));
        }

        let build = || {
            let amz_date = amz_date_now();
            let authorization = sigv4::authorization(&sigv4::RequestToSign {
                access_key_id: &self.access_key_id,
                secret_access_key: &self.secret_access_key,
                region,
                service: "ec2",
                amz_date: &amz_date,
                host: &host,
                query: &query,
                content_type: CONTENT_TYPE,
                amz_target: None,
                body: &body,
            });
            self.http
                .post(url.clone())
                .header("content-type", CONTENT_TYPE)
                .header("x-amz-date", amz_date)
                .header("authorization", authorization)
                .body(body.clone())
        };

        match http::send_retrying(build).await {
            Ok(resp) => resp
                .text()
                .await
                .map_err(|e| ProviderError::Other(format!("reading {action} response: {e}"))),
            // EC2 signals expected failures as an XML error code in the
            // body, which the shared http layer carries on the error: bad
            // parameters and unknown ids arrive as 400 (classified Other),
            // a missing IAM permission as 403 (classified Auth). Both are
            // fatal and both carry a code worth naming, so both get
            // remapped. 5xx keeps its Transient classification: the code in
            // a server-error body says nothing about whether to retry.
            Err(err) => {
                let remapped = match http::error_body(&err) {
                    Some(body)
                        if matches!(err, ProviderError::Other(_) | ProviderError::Auth(_))
                            && body.contains("<Code>") =>
                    {
                        Some(map_error_body(body))
                    }
                    _ => None,
                };
                Err(remapped.unwrap_or(err))
            }
        }
    }

    /// Resolves the current Debian 12 arm64 AMI for `region` from the
    /// public SSM parameter `/aws/service/debian/release/12/latest/arm64`,
    /// caching the result per region for the provider's lifetime. `launch`
    /// prefers this over the bundled snapshot id.
    pub async fn resolve_ami(&self, region: &RegionId) -> Result<String> {
        self.catalog_region(region)?;
        let region = region.as_str();
        if let Some(ami) = self.ami_cache.lock().expect("ami cache lock").get(region) {
            return Ok(ami.clone());
        }

        let (url, host, query) = self.signed_url("ssm", region)?;
        let body = format!("{{\"Name\":\"{DEBIAN_12_ARM64_PARAM}\"}}");
        let build = || {
            let amz_date = amz_date_now();
            let authorization = sigv4::authorization(&sigv4::RequestToSign {
                access_key_id: &self.access_key_id,
                secret_access_key: &self.secret_access_key,
                region,
                service: "ssm",
                amz_date: &amz_date,
                host: &host,
                query: &query,
                content_type: SSM_CONTENT_TYPE,
                amz_target: Some(SSM_TARGET_GET_PARAMETER),
                body: &body,
            });
            self.http
                .post(url.clone())
                .header("content-type", SSM_CONTENT_TYPE)
                .header("x-amz-target", SSM_TARGET_GET_PARAMETER)
                .header("x-amz-date", amz_date)
                .header("authorization", authorization)
                .body(body.clone())
        };

        let resp = http::send_retrying(build).await?;
        let text = resp
            .text()
            .await
            .map_err(|e| ProviderError::Other(format!("reading GetParameter response: {e}")))?;
        let parsed: SsmGetParameterResponse = serde_json::from_str(&text)
            .map_err(|e| ProviderError::Other(format!("GetParameter response parse: {e}")))?;
        let ami = parsed.parameter.value;
        if !ami.starts_with("ami-") {
            return Err(ProviderError::Other(format!(
                "GetParameter for {DEBIAN_12_ARM64_PARAM} returned a non-AMI value {ami:?}"
            )));
        }
        self.ami_cache
            .lock()
            .expect("ami cache lock")
            .insert(region.to_owned(), ami.clone());
        Ok(ami)
    }

    /// Name of the session's security group. Up to 255 characters, and the
    /// EC2 charset is wide enough that a session id never needs escaping.
    fn group_name(session: &str) -> String {
        format!("jamstream-{session}")
    }

    /// Group id of the session's security group, or None when it does not
    /// exist. Looked up by name rather than by tag so a group created by a
    /// launch that failed halfway is found and reused.
    async fn find_group(&self, region: &str, session: &str) -> Result<Option<String>> {
        let params = vec![
            ("Filter.1.Name".to_owned(), "group-name".to_owned()),
            ("Filter.1.Value.1".to_owned(), Self::group_name(session)),
        ];
        let body = self
            .ec2_call(region, "DescribeSecurityGroups", &params)
            .await?;
        Ok(xml_value(&body, "groupId").map(xml_unescape))
    }

    /// Creates the session's security group and opens exactly one port in
    /// it. Returns the group id, which `launch` passes to RunInstances so
    /// the instance never exists without it.
    ///
    /// Nothing ambient is relied on: without this the instance lands in the
    /// VPC default security group, which permits inbound only from itself,
    /// so the session port is unreachable unless the account holder had
    /// previously widened that group for something else.
    async fn ensure_group(
        &self,
        region: &str,
        session: &str,
        tags: &[(String, String)],
    ) -> Result<String> {
        let name = Self::group_name(session);
        let mut params = vec![
            ("GroupName".to_owned(), name.clone()),
            (
                "GroupDescription".to_owned(),
                format!("JamStream session {session}"),
            ),
            (
                "TagSpecification.1.ResourceType".to_owned(),
                "security-group".to_owned(),
            ),
        ];
        for (i, (key, value)) in tags.iter().enumerate() {
            params.push((format!("TagSpecification.1.Tag.{}.Key", i + 1), key.clone()));
            params.push((
                format!("TagSpecification.1.Tag.{}.Value", i + 1),
                value.clone(),
            ));
        }
        let group_id = match self.ec2_call(region, "CreateSecurityGroup", &params).await {
            Ok(body) => xml_value(&body, "groupId")
                .map(xml_unescape)
                .ok_or_else(|| {
                    ProviderError::Other(
                        "CreateSecurityGroup response contained no groupId".to_owned(),
                    )
                })?,
            // A retried launch finds its own group from the previous
            // attempt. EC2 calls that InvalidGroup.Duplicate.
            Err(err) if error_code(&err).as_deref() == Some("InvalidGroup.Duplicate") => {
                self.find_group(region, session).await?.ok_or_else(|| {
                    ProviderError::Other(format!(
                        "security group {name} exists but cannot be found"
                    ))
                })?
            }
            Err(err) => return Err(err),
        };

        let port = self.session_port.to_string();
        let authorize = vec![
            ("GroupId".to_owned(), group_id.clone()),
            ("IpPermissions.1.IpProtocol".to_owned(), "udp".to_owned()),
            ("IpPermissions.1.FromPort".to_owned(), port.clone()),
            ("IpPermissions.1.ToPort".to_owned(), port),
            (
                "IpPermissions.1.IpRanges.1.CidrIp".to_owned(),
                ANY_IPV4.to_owned(),
            ),
            (
                "IpPermissions.1.IpRanges.1.Description".to_owned(),
                "JamStream session traffic".to_owned(),
            ),
            // A dual-stack subnet hands out a v6 address too, and a rule
            // that covers only v4 is not the rule the session needs.
            (
                "IpPermissions.1.Ipv6Ranges.1.CidrIpv6".to_owned(),
                ANY_IPV6.to_owned(),
            ),
            (
                "IpPermissions.1.Ipv6Ranges.1.Description".to_owned(),
                "JamStream session traffic".to_owned(),
            ),
        ];
        match self
            .ec2_call(region, "AuthorizeSecurityGroupIngress", &authorize)
            .await
        {
            Ok(_) => {}
            // Already open, which is the reused-group case.
            Err(err) if error_code(&err).as_deref() == Some("InvalidPermission.Duplicate") => {}
            Err(err) => return Err(err),
        }
        Ok(group_id)
    }

    /// DescribeInstances with nextToken pagination, parsed into instances.
    async fn describe_instances(
        &self,
        region: &Region,
        base_params: &[(String, String)],
    ) -> Result<Vec<ParsedInstance>> {
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut params = base_params.to_vec();
            if let Some(t) = &token {
                params.push(("NextToken".to_owned(), t.clone()));
            }
            let body = self
                .ec2_call(region.id.as_str(), "DescribeInstances", &params)
                .await?;
            out.extend(parse_instances(region, &body));
            token = xml_value(&body, "nextToken")
                .map(xml_unescape)
                .filter(|t| !t.is_empty());
            if token.is_none() {
                break;
            }
        }
        Ok(out)
    }

    /// Hourly price for a specific instance class in a region.
    pub fn price_for(&self, region: &RegionId, class: InstanceClass) -> Result<Price> {
        let region = self.catalog_region(region)?;
        let d = data();
        let rd = d.regions.get(region.id.as_str()).ok_or_else(|| {
            ProviderError::NotFound(format!("no aws pricing for region {}", region.id))
        })?;
        let hourly_microusd = match class {
            InstanceClass::Small => rd.small_hourly_microusd,
            InstanceClass::Standard => rd.medium_hourly_microusd,
        };
        Ok(Price {
            hourly_microusd,
            egress_microusd_per_gb: d.egress_microusd_per_gb,
            included_egress_gb: d.included_egress_gb,
        })
    }

    /// Fetches the current state of one instance so the CLI can poll for
    /// the public IP after launch. NotFound once the instance is
    /// terminated or gone entirely.
    pub async fn refresh(&self, region: &RegionId, id: &str) -> Result<Instance> {
        let region = self.catalog_region(region)?;
        let params = vec![("InstanceId.1".to_owned(), id.to_owned())];
        let found = self
            .describe_instances(&region, &params)
            .await?
            .into_iter()
            .find(|p| p.instance.id == id)
            .ok_or_else(|| {
                ProviderError::NotFound(format!("instance {id} in region {}", region.id))
            })?;
        if found.state.as_deref() == Some("terminated") {
            return Err(ProviderError::NotFound(format!(
                "instance {id} is terminated"
            )));
        }
        Ok(found.instance)
    }
}

#[async_trait]
impl Provider for AwsProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Aws
    }

    /// t4g is Graviton and the AMI is the Debian 12 arm64 build, so the
    /// server binary must be aarch64.
    fn server_arch(&self) -> ServerArch {
        ServerArch::Aarch64
    }

    fn regions(&self) -> Vec<Region> {
        REGIONS
            .iter()
            .map(|(id, display, country)| Region {
                provider: ProviderKind::Aws,
                id: RegionId::new(*id),
                display: (*display).to_owned(),
                country: (*country).to_owned(),
            })
            .collect()
    }

    /// Prices the Standard class (t4g.medium), the size `launch` actually
    /// creates for a Standard spec, so the preview never understates the
    /// bill. `price_for` covers both classes.
    async fn price(&self, region: &RegionId) -> Result<Price> {
        self.price_for(region, InstanceClass::Standard)
    }

    async fn launch(&self, spec: LaunchSpec) -> Result<Instance> {
        let region = self.catalog_region(&spec.region.id)?;
        let Some(session) = spec.session_id().map(str::to_owned) else {
            return Err(ProviderError::Other(format!(
                "launch spec has no {SESSION_TAG_KEY} tag; refusing to create an instance the sweeper cannot find"
            )));
        };
        let rd = data().regions.get(region.id.as_str()).ok_or_else(|| {
            ProviderError::NotFound(format!("no aws image data for region {}", region.id))
        })?;
        // Prefer the live SSM-resolved Debian AMI; the bundled id is a
        // pinned fallback so launches survive an SSM outage.
        let ami = match self.resolve_ami(&region.id).await {
            Ok(ami) => ami,
            Err(err) => {
                tracing::warn!(
                    region = %region.id,
                    error = %err,
                    "ssm ami resolution failed; falling back to bundled ami"
                );
                rd.ami.clone()
            }
        };

        // The security group exists before the instance does, so there is no
        // window in which the instance is up on VPC defaults.
        let group_id = self
            .ensure_group(region.id.as_str(), &session, &spec.tags)
            .await?;

        let mut params = vec![
            ("ImageId".to_owned(), ami),
            (
                "InstanceType".to_owned(),
                instance_type(spec.instance_class).to_owned(),
            ),
            ("MinCount".to_owned(), "1".to_owned()),
            ("MaxCount".to_owned(), "1".to_owned()),
            ("SecurityGroupId.1".to_owned(), group_id),
            // Plain `shutdown -h now` in the guest terminates the VM, so
            // the cloud-init self-destruct path needs no credentials.
            (
                "InstanceInitiatedShutdownBehavior".to_owned(),
                "terminate".to_owned(),
            ),
            // IMDSv2 only. user-data carries the server private key, and
            // IMDSv1 hands it to anything that can make an outbound GET,
            // including through an SSRF in something else on the box.
            // HttpEndpoint stays enabled: cloud-init reads user-data from
            // IMDS and nowhere else, so disabling it would boot a VM with
            // no configuration at all.
            (
                "MetadataOptions.HttpTokens".to_owned(),
                "required".to_owned(),
            ),
            (
                "MetadataOptions.HttpPutResponseHopLimit".to_owned(),
                "1".to_owned(),
            ),
            // Gzipped, not for transfer size: RunInstances caps user data at
            // 16384 bytes and the rendered cloud-init crossed it, which
            // rejected every launch. cloud-init detects the compression from
            // the magic bytes on the machine. The other providers take user
            // data as a plain JSON string, so they could not carry gzip and
            // do not need it: their caps are 64 KB and 256 KB.
            (
                "UserData".to_owned(),
                BASE64.encode(&gzip(spec.user_data.as_bytes())?),
            ),
            (
                "TagSpecification.1.ResourceType".to_owned(),
                "instance".to_owned(),
            ),
        ];
        for (i, (key, value)) in spec.tags.iter().enumerate() {
            params.push((format!("TagSpecification.1.Tag.{}.Key", i + 1), key.clone()));
            params.push((
                format!("TagSpecification.1.Tag.{}.Value", i + 1),
                value.clone(),
            ));
        }

        let body = self
            .ec2_call(region.id.as_str(), "RunInstances", &params)
            .await?;
        let first = parse_instances(&region, &body)
            .into_iter()
            .next()
            .ok_or_else(|| {
                ProviderError::Other("RunInstances response contained no instanceId".to_owned())
            })?;
        // The public IP is usually not assigned yet; callers poll
        // `refresh` until it appears. Tags echo the spec rather than the
        // response so the result is complete even on terse responses.
        Ok(Instance {
            tags: spec.tags,
            ..first.instance
        })
    }

    async fn destroy(&self, region: &RegionId, id: &str) -> Result<()> {
        let region = self.catalog_region(region)?;
        let params = vec![("InstanceId.1".to_owned(), id.to_owned())];
        self.ec2_call(region.id.as_str(), "TerminateInstances", &params)
            .await?;
        Ok(())
    }

    /// One security-group describe: the launch's first EC2 call and the
    /// exact permission a real launch found missing from a hand-written
    /// policy (#118). Failing it here puts the action name on screen while
    /// the host is still pasting keys.
    async fn preflight(&self) -> Result<()> {
        let region = self
            .regions()
            .into_iter()
            .next()
            .ok_or_else(|| ProviderError::Other("aws catalog is empty".to_owned()))?;
        self.find_group(region.id.as_str(), "preflight").await?;
        Ok(())
    }

    async fn list_tagged(&self, session_tag: Option<&str>) -> Result<Vec<Instance>> {
        let mut base = vec![
            ("Filter.1.Name".to_owned(), "instance-state-name".to_owned()),
            ("Filter.1.Value.1".to_owned(), "pending".to_owned()),
            ("Filter.1.Value.2".to_owned(), "running".to_owned()),
        ];
        match session_tag {
            Some(session) => {
                base.push(("Filter.2.Name".to_owned(), format!("tag:{SESSION_TAG_KEY}")));
                base.push(("Filter.2.Value.1".to_owned(), session.to_owned()));
            }
            None => {
                base.push(("Filter.2.Name".to_owned(), "tag-key".to_owned()));
                base.push(("Filter.2.Value.1".to_owned(), SESSION_TAG_KEY.to_owned()));
            }
        }

        let mut set = tokio::task::JoinSet::new();
        for region in self.regions() {
            let provider = self.clone();
            let params = base.clone();
            set.spawn(async move {
                let res = provider.describe_instances(&region, &params).await;
                (region, res)
            });
        }

        let mut out = Vec::new();
        let mut first_err = None;
        let mut ok_regions = 0usize;
        while let Some(joined) = set.join_next().await {
            match joined {
                Ok((_, Ok(parsed))) => {
                    ok_regions += 1;
                    out.extend(parsed.into_iter().map(|p| p.instance));
                }
                // One broken region must not hide orphans elsewhere: warn
                // and keep going with the regions that answered.
                Ok((region, Err(err))) => {
                    tracing::warn!(region = %region.id, error = %err, "list_tagged: skipping unreachable region");
                    first_err.get_or_insert(err);
                }
                Err(err) => {
                    tracing::warn!(error = %err, "list_tagged: region task failed");
                    first_err.get_or_insert(ProviderError::Other(err.to_string()));
                }
            }
        }
        // If every region failed there is nothing trustworthy to report;
        // an empty Ok here would look like "no orphans" to the sweeper.
        if ok_regions == 0
            && let Some(err) = first_err
        {
            return Err(err);
        }
        out.sort_by(|a, b| {
            (a.region.id.as_str(), a.id.as_str()).cmp(&(b.region.id.as_str(), b.id.as_str()))
        });
        Ok(out)
    }

    fn session_port(&self) -> u16 {
        self.session_port
    }

    async fn session_ingress(&self, session: &str) -> Result<Vec<IngressRule>> {
        let params = vec![
            ("Filter.1.Name".to_owned(), "group-name".to_owned()),
            ("Filter.1.Value.1".to_owned(), Self::group_name(session)),
        ];
        let mut set = tokio::task::JoinSet::new();
        for region in self.regions() {
            let provider = self.clone();
            let params = params.clone();
            set.spawn(async move {
                provider
                    .ec2_call(region.id.as_str(), "DescribeSecurityGroups", &params)
                    .await
            });
        }
        let mut out = Vec::new();
        while let Some(joined) = set.join_next().await {
            if let Ok(Ok(body)) = joined {
                out.extend(parse_ingress(&body));
            }
        }
        out.sort();
        out.dedup();
        Ok(out)
    }

    async fn destroy_orphan_firewalls(&self) -> Result<Vec<String>> {
        // Every group JamStream created carries the session tag, so this
        // finds them without depending on the naming convention.
        let params = vec![
            ("Filter.1.Name".to_owned(), "tag-key".to_owned()),
            ("Filter.1.Value.1".to_owned(), SESSION_TAG_KEY.to_owned()),
        ];
        let mut set = tokio::task::JoinSet::new();
        for region in self.regions() {
            let provider = self.clone();
            let params = params.clone();
            set.spawn(async move {
                let listed = provider
                    .ec2_call(region.id.as_str(), "DescribeSecurityGroups", &params)
                    .await;
                (region, listed)
            });
        }

        let mut deleted = Vec::new();
        while let Some(joined) = set.join_next().await {
            let Ok((region, Ok(body))) = joined else {
                continue;
            };
            for (group_id, group_name) in parse_security_groups(&body) {
                let params = vec![("GroupId".to_owned(), group_id.clone())];
                match self
                    .ec2_call(region.id.as_str(), "DeleteSecurityGroup", &params)
                    .await
                {
                    Ok(_) => deleted.push(group_name),
                    // The group still has a network interface behind it,
                    // which is either a live session or an instance that is
                    // still terminating. Either way it is not an orphan yet;
                    // the next sweep gets it.
                    Err(err) if error_code(&err).as_deref() == Some("DependencyViolation") => {}
                    Err(err) => {
                        tracing::warn!(
                            region = %region.id,
                            group = group_name,
                            error = %err,
                            "could not delete security group"
                        );
                    }
                }
            }
        }
        deleted.sort();
        Ok(deleted)
    }
}

/// (groupId, groupName) for every group in a DescribeSecurityGroups body.
fn parse_security_groups(xml: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some((id, after_id)) = take_tag(rest, "groupId") {
        let Some((name, after_name)) = take_tag(after_id, "groupName") else {
            break;
        };
        out.push((xml_unescape(id), xml_unescape(name)));
        rest = after_name;
    }
    out
}

/// The ingress permissions of a DescribeSecurityGroups body. Only the
/// `ipPermissions` block counts: `ipPermissionsEgress` is the allow-all
/// outbound rule every new group starts with, which is what lets the VM
/// download jamstreamd.
fn parse_ingress(xml: &str) -> Vec<IngressRule> {
    let Some(block) = xml_value(xml, "ipPermissions") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut rest = block;
    while let Some((protocol, after)) = take_tag(rest, "ipProtocol") {
        // A permission ends where the next one's protocol begins.
        let end = after.find("<ipProtocol>").unwrap_or(after.len());
        let item = &after[..end];
        let port = |tag: &str| {
            xml_value(item, tag)
                .and_then(|v| v.parse::<u16>().ok())
                .unwrap_or(0)
        };
        let mut cidrs = Vec::new();
        for tag in ["cidrIp", "cidrIpv6"] {
            let mut ranges = item;
            while let Some((cidr, after_cidr)) = take_tag(ranges, tag) {
                cidrs.push(xml_unescape(cidr));
                ranges = after_cidr;
            }
        }
        cidrs.sort();
        out.push(IngressRule {
            protocol: xml_unescape(protocol),
            from_port: port("fromPort"),
            to_port: port("toPort"),
            cidrs,
        });
        rest = &after[end..];
    }
    out
}

/// The AWS setup page in the published user reference. A missing action is
/// a policy the host has to edit, not a bug, so the error points at the
/// page that has the policy to paste.
const AWS_SETUP_URL: &str = "https://sean-reid.github.io/jamstream/guides/providers/aws.html";

/// AWS appends `Encoded authorization failure message: <~700 characters of
/// base64>` to UnauthorizedOperation. Decoding it takes
/// sts:DecodeAuthorizationMessage, which a host following our setup does
/// not have, so in the wizard it is 700 characters that say nothing; the
/// action name earlier in the same message is the part that says what to
/// fix.
const ENCODED_AUTHORIZATION_MESSAGE: &str = "Encoded authorization failure message:";

/// The action an UnauthorizedOperation message names, if it names one. AWS
/// phrases it "is not authorized to perform: ec2:CreateSecurityGroup on
/// resource: ...".
fn denied_action(message: &str) -> Option<&str> {
    let action = message
        .split_once("is not authorized to perform: ")?
        .1
        .split([' ', ',', '.'])
        .next()?;
    (!action.is_empty()).then_some(action)
}

/// Maps an EC2 error body (`<Response><Errors><Error><Code>...`) to a
/// ProviderError. Reached for the fatal statuses whose body the shared http
/// layer carried on the error, 400 and 403; other statuses keep the shared
/// classification.
fn map_error_body(body: &str) -> ProviderError {
    let code = xml_value(body, "Code")
        .map(xml_unescape)
        .unwrap_or_default();
    let message = xml_value(body, "Message")
        .map(xml_unescape)
        .unwrap_or_default();
    let detail = format!("{code}: {message}");
    // The one failure a host who followed the setup guide can actually act
    // on: name the action their policy is missing and where to add it,
    // ahead of what AWS said rather than instead of it.
    if code == "UnauthorizedOperation" {
        let missing = match denied_action(&message) {
            Some(action) => format!("the IAM policy is missing {action}"),
            None => "the IAM policy is missing a permission this call needs".to_owned(),
        };
        let said = message
            .split(ENCODED_AUTHORIZATION_MESSAGE)
            .next()
            .unwrap_or(&message)
            .trim();
        return ProviderError::Auth(format!(
            "{code}: {missing}. Update the jamstream-host policy at {AWS_SETUP_URL} \
             (AWS said: {said})"
        ));
    }
    if code == "RequestLimitExceeded" || code == "Throttling" {
        ProviderError::RateLimited { retry_after: None }
    } else if code.contains("NotFound") || code == "InvalidInstanceID.Malformed" {
        ProviderError::NotFound(detail)
    } else if code == "AuthFailure" || code == "SignatureDoesNotMatch" {
        ProviderError::Auth(detail)
    } else if code.contains("LimitExceeded") || code == "InsufficientInstanceCapacity" {
        ProviderError::QuotaExceeded(detail)
    } else {
        ProviderError::Other(detail)
    }
}

/// EC2 error code behind an error returned by [`AwsProvider::ec2_call`].
/// Codes that call names, `InvalidGroup.Duplicate` and
/// `DependencyViolation`, are the difference between a retryable launch and
/// a real failure, so they have to survive the trip through ProviderError.
/// `map_error_body` renders what it remapped as "{code}: {message}"; an
/// error it left alone still carries the raw XML.
fn error_code(err: &ProviderError) -> Option<String> {
    if let Some(code) = http::error_body(err)
        .and_then(|body| xml_value(body, "Code"))
        .map(xml_unescape)
    {
        return Some(code);
    }
    let msg = match err {
        ProviderError::Auth(m)
        | ProviderError::QuotaExceeded(m)
        | ProviderError::NotFound(m)
        | ProviderError::Transient(m)
        | ProviderError::Other(m) => m,
        ProviderError::RateLimited { .. } => return None,
    };
    let (code, _) = msg.split_once(": ")?;
    // Codes are dotted CamelCase with no spaces; anything else is prose.
    (!code.contains(' ') && !code.is_empty()).then(|| code.to_owned())
}

/// RFC 3986 percent-encoding with AWS's unreserved set. Everything
/// outside [A-Za-z0-9._~-], including space and '+', becomes %XX with
/// uppercase hex, exactly as SigV4 expects.
pub(crate) fn aws_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

pub(crate) fn amz_date_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_secs();
    amz_date(secs)
}

/// `YYYYMMDDTHHMMSSZ` from unix seconds, no chrono needed.
fn amz_date(epoch_secs: u64) -> String {
    let days = (epoch_secs / 86_400) as i64;
    let rem = epoch_secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Days since 1970-01-01 to (year, month, day); Howard Hinnant's
/// civil-from-days algorithm.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Text of the first `<tag>...</tag>` element, if present. The EC2 Query
/// responses are small and predictable enough that targeted extraction
/// beats an XML dependency; nested structures are handled by slicing the
/// enclosing block first (see `instance_state`, `parse_tags`).
pub(crate) fn xml_value<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    take_tag(xml, tag).map(|(value, _)| value)
}

/// Like `xml_value` but also returns the remainder after the close tag,
/// for iterating repeated elements.
pub(crate) fn take_tag<'a>(xml: &'a str, tag: &str) -> Option<(&'a str, &'a str)> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some((&xml[start..end], &xml[end + close.len()..]))
}

pub(crate) fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

struct ParsedInstance {
    instance: Instance,
    state: Option<String>,
}

/// Splits a RunInstances/DescribeInstances body into per-instance chunks,
/// each starting at its `<instanceId>`, and extracts id, public IP,
/// state, and tags. Within an instancesSet item the fields we need always
/// follow the id, and reservation-level trailer elements between
/// instances carry none of the tags we look for.
fn parse_instances(region: &Region, xml: &str) -> Vec<ParsedInstance> {
    let mut starts: Vec<usize> = xml.match_indices("<instanceId>").map(|(i, _)| i).collect();
    starts.push(xml.len());
    let mut out = Vec::new();
    for w in starts.windows(2) {
        let chunk = &xml[w[0]..w[1]];
        let Some(id) = xml_value(chunk, "instanceId") else {
            continue;
        };
        out.push(ParsedInstance {
            instance: Instance {
                provider: ProviderKind::Aws,
                region: region.clone(),
                id: xml_unescape(id),
                public_ip: xml_value(chunk, "ipAddress").and_then(|ip| ip.parse().ok()),
                tags: parse_tags(chunk),
            },
            state: instance_state(chunk).map(str::to_owned),
        });
    }
    out
}

fn instance_state(chunk: &str) -> Option<&str> {
    xml_value(chunk, "instanceState").and_then(|block| xml_value(block, "name"))
}

fn parse_tags(chunk: &str) -> Vec<(String, String)> {
    let mut tags = Vec::new();
    let Some(mut rest) = xml_value(chunk, "tagSet") else {
        return tags;
    };
    while let Some((key, after_key)) = take_tag(rest, "key") {
        let Some((value, after_value)) = take_tag(after_key, "value") else {
            break;
        };
        tags.push((xml_unescape(key), xml_unescape(value)));
        rest = after_value;
    }
    tags
}

/// SHA-256, HMAC-SHA256, and AWS Signature Version 4, implemented by hand
/// because the workspace deliberately carries no AWS or hash crates.
/// Verified in the unit tests against FIPS 180-4 SHA-256 vectors, RFC
/// 4231 HMAC vectors, and the AWS SigV4 test-suite "get-vanilla" case.
///
/// Two entry points, deliberately layered so the EC2 and SSM callers cannot
/// be broken by S3's extra requirements:
///
/// * [`authorization`] signs the fixed-shape `POST /` this module's own
///   provider makes (EC2 Query form posts and SSM JSON posts). Its signed
///   header set and canonical request are unchanged.
/// * [`authorization_for`] signs an arbitrary method, canonical URI, query
///   string, and header set against a caller-supplied payload hash. S3
///   needs all four: it addresses objects by path, uses PUT/HEAD/DELETE,
///   passes options in the query string (`?uploads`, `?uploadId=`,
///   `?lifecycle`), and requires the payload hash to travel in a signed
///   `x-amz-content-sha256` header. [`authorization`] is implemented in
///   terms of it, so the exact-string tests below cover both.
pub(crate) mod sigv4 {
    use data_encoding::HEXLOWER;

    const BLOCK: usize = 64;

    /// Hex SHA-256 of the empty string: the payload hash for every request
    /// with no body (HEAD, GET, DELETE).
    pub const EMPTY_PAYLOAD_SHA256: &str =
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    #[rustfmt::skip]
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5,
        0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
        0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3,
        0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
        0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc,
        0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
        0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
        0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
        0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3,
        0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5,
        0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208,
        0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];

    pub fn sha256(data: &[u8]) -> [u8; 32] {
        let mut h: [u32; 8] = [
            0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
            0x5be0cd19,
        ];
        let mut msg = data.to_vec();
        let bit_len = (data.len() as u64).wrapping_mul(8);
        msg.push(0x80);
        while msg.len() % BLOCK != 56 {
            msg.push(0);
        }
        msg.extend_from_slice(&bit_len.to_be_bytes());
        for chunk in msg.chunks_exact(BLOCK) {
            let mut w = [0u32; 64];
            for (i, word) in chunk.chunks_exact(4).enumerate() {
                w[i] = u32::from_be_bytes(word.try_into().expect("4-byte chunk"));
            }
            for i in 16..64 {
                let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
                let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
                w[i] = w[i - 16]
                    .wrapping_add(s0)
                    .wrapping_add(w[i - 7])
                    .wrapping_add(s1);
            }
            let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
            for (&ki, &wi) in K.iter().zip(w.iter()) {
                let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
                let ch = (e & f) ^ (!e & g);
                let t1 = hh
                    .wrapping_add(s1)
                    .wrapping_add(ch)
                    .wrapping_add(ki)
                    .wrapping_add(wi);
                let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
                let maj = (a & b) ^ (a & c) ^ (b & c);
                let t2 = s0.wrapping_add(maj);
                hh = g;
                g = f;
                f = e;
                e = d.wrapping_add(t1);
                d = c;
                c = b;
                b = a;
                a = t1.wrapping_add(t2);
            }
            for (hi, v) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
                *hi = hi.wrapping_add(v);
            }
        }
        let mut out = [0u8; 32];
        for (i, word) in h.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
        out
    }

    pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
        let mut k = [0u8; BLOCK];
        if key.len() > BLOCK {
            k[..32].copy_from_slice(&sha256(key));
        } else {
            k[..key.len()].copy_from_slice(key);
        }
        let mut inner = Vec::with_capacity(BLOCK + msg.len());
        inner.extend(k.iter().map(|b| b ^ 0x36));
        inner.extend_from_slice(msg);
        let inner_hash = sha256(&inner);
        let mut outer = Vec::with_capacity(BLOCK + 32);
        outer.extend(k.iter().map(|b| b ^ 0x5c));
        outer.extend_from_slice(&inner_hash);
        sha256(&outer)
    }

    pub fn hex_sha256(data: &[u8]) -> String {
        HEXLOWER.encode(&sha256(data))
    }

    /// Canonical request plus the signed-headers list. `headers` must be
    /// lowercase names in ascending order.
    pub fn canonical_request(
        method: &str,
        path: &str,
        query: &str,
        headers: &[(&str, &str)],
        payload: &[u8],
    ) -> (String, String) {
        canonical_request_with_hash(method, path, query, headers, &hex_sha256(payload))
    }

    /// [`canonical_request`] against a payload hash the caller already has.
    /// S3 always has one, because the same hex digest has to be sent in the
    /// `x-amz-content-sha256` header, and hashing a 16 MiB part twice for
    /// every retry would be wasteful.
    pub fn canonical_request_with_hash(
        method: &str,
        path: &str,
        query: &str,
        headers: &[(&str, &str)],
        payload_sha256: &str,
    ) -> (String, String) {
        let signed = headers
            .iter()
            .map(|(name, _)| *name)
            .collect::<Vec<_>>()
            .join(";");
        let mut canonical_headers = String::new();
        for (name, value) in headers {
            canonical_headers.push_str(name);
            canonical_headers.push(':');
            canonical_headers.push_str(value.trim());
            canonical_headers.push('\n');
        }
        let request =
            format!("{method}\n{path}\n{query}\n{canonical_headers}\n{signed}\n{payload_sha256}");
        (request, signed)
    }

    pub fn string_to_sign(amz_date: &str, scope: &str, canonical_request: &str) -> String {
        format!(
            "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
            hex_sha256(canonical_request.as_bytes())
        )
    }

    pub fn signature(
        secret_access_key: &str,
        date: &str,
        region: &str,
        service: &str,
        string_to_sign: &str,
    ) -> String {
        let k = hmac_sha256(
            format!("AWS4{secret_access_key}").as_bytes(),
            date.as_bytes(),
        );
        let k = hmac_sha256(&k, region.as_bytes());
        let k = hmac_sha256(&k, service.as_bytes());
        let k = hmac_sha256(&k, b"aws4_request");
        HEXLOWER.encode(&hmac_sha256(&k, string_to_sign.as_bytes()))
    }

    /// Everything needed to sign one jamstream AWS POST (EC2 Query form
    /// posts and SSM JSON-protocol posts share this shape).
    pub struct RequestToSign<'a> {
        pub access_key_id: &'a str,
        pub secret_access_key: &'a str,
        pub region: &'a str,
        /// Signing service name: "ec2" or "ssm".
        pub service: &'a str,
        /// `YYYYMMDDTHHMMSSZ`.
        pub amz_date: &'a str,
        pub host: &'a str,
        /// Canonical query string, already percent-encoded; empty for the
        /// real per-region endpoint.
        pub query: &'a str,
        pub content_type: &'a str,
        /// X-Amz-Target header for JSON-protocol services (signed when
        /// present); None for the EC2 Query API.
        pub amz_target: Option<&'a str>,
        pub body: &'a str,
    }

    /// Authorization header value for the POST described by `req`. Signed
    /// headers are content-type, host, x-amz-date, plus x-amz-target when
    /// present (already in ascending order as SigV4 requires).
    pub fn authorization(req: &RequestToSign<'_>) -> String {
        let mut headers = vec![
            ("content-type", req.content_type),
            ("host", req.host),
            ("x-amz-date", req.amz_date),
        ];
        if let Some(target) = req.amz_target {
            headers.push(("x-amz-target", target));
        }
        let (canonical, signed) =
            canonical_request("POST", "/", req.query, &headers, req.body.as_bytes());
        header_value(
            req.access_key_id,
            req.secret_access_key,
            req.region,
            req.service,
            req.amz_date,
            &canonical,
            &signed,
        )
    }

    /// Everything needed to sign an arbitrary AWS request. Unlike
    /// [`RequestToSign`] nothing here is fixed: the S3 object store needs
    /// PUT/HEAD/DELETE/POST, keyed canonical URIs, sub-resource query
    /// strings, and a header set that varies per operation.
    pub struct SignedRequest<'a> {
        pub access_key_id: &'a str,
        pub secret_access_key: &'a str,
        pub region: &'a str,
        /// Signing service name: "ec2", "ssm", or "s3".
        pub service: &'a str,
        /// Uppercase HTTP method.
        pub method: &'a str,
        /// Canonical URI: absolute path, already percent-encoded, `/` for
        /// the service root. S3 signs the path exactly as it appears in the
        /// request line and, unlike other services, must not be
        /// double-encoded, so the caller owns the encoding.
        pub canonical_uri: &'a str,
        /// Canonical query string: `key=value` pairs sorted by key, each
        /// side percent-encoded, joined with `&`. Empty when there is none.
        pub query: &'a str,
        /// `YYYYMMDDTHHMMSSZ`.
        pub amz_date: &'a str,
        /// The complete signed header set: lowercase names in ascending
        /// order, and it must include `host` and `x-amz-date`. Every header
        /// listed here has to be sent on the wire with the same value.
        pub headers: &'a [(&'a str, &'a str)],
        /// Hex SHA-256 of the request body ([`EMPTY_PAYLOAD_SHA256`] when
        /// there is none). S3 additionally requires this exact string in a
        /// signed `x-amz-content-sha256` header.
        pub payload_sha256: &'a str,
    }

    /// Authorization header value for the request described by `req`.
    pub fn authorization_for(req: &SignedRequest<'_>) -> String {
        let (canonical, signed) = canonical_request_with_hash(
            req.method,
            req.canonical_uri,
            req.query,
            req.headers,
            req.payload_sha256,
        );
        header_value(
            req.access_key_id,
            req.secret_access_key,
            req.region,
            req.service,
            req.amz_date,
            &canonical,
            &signed,
        )
    }

    /// The shared tail of both entry points: derive the scope, build the
    /// string to sign, sign it, and render the header. Keeping it in one
    /// place is what makes the two entry points provably equivalent for a
    /// request they can both express.
    fn header_value(
        access_key_id: &str,
        secret_access_key: &str,
        region: &str,
        service: &str,
        amz_date: &str,
        canonical_request: &str,
        signed_headers: &str,
    ) -> String {
        let date = &amz_date[..8];
        let scope = format!("{date}/{region}/{service}/aws4_request");
        let to_sign = string_to_sign(amz_date, &scope, canonical_request);
        let sig = signature(secret_access_key, date, region, service, &to_sign);
        format!(
            "AWS4-HMAC-SHA256 Credential={access_key_id}/{scope}, \
             SignedHeaders={signed_headers}, Signature={sig}"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use data_encoding::HEXLOWER;

    // ---- SHA-256: FIPS 180-4 vectors ----

    #[test]
    fn sha256_fips_vectors() {
        let cases: [(&[u8], &str); 3] = [
            (
                b"abc",
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            ),
            (
                b"",
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            ),
            (
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq",
                "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1",
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(HEXLOWER.encode(&sigv4::sha256(input)), expected);
        }
    }

    // ---- HMAC-SHA256: RFC 4231 vectors ----

    #[test]
    fn hmac_sha256_rfc4231_vectors() {
        // Case 1.
        assert_eq!(
            HEXLOWER.encode(&sigv4::hmac_sha256(&[0x0b; 20], b"Hi There")),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        // Case 2.
        assert_eq!(
            HEXLOWER.encode(&sigv4::hmac_sha256(
                b"Jefe",
                b"what do ya want for nothing?"
            )),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // Case 3.
        assert_eq!(
            HEXLOWER.encode(&sigv4::hmac_sha256(&[0xaa; 20], &[0xdd; 50])),
            "773ea91e36800e46854db8ebd09181a72959098b3ef8c122d9635514ced565fe"
        );
        // Case 6: key larger than the block size gets hashed first.
        assert_eq!(
            HEXLOWER.encode(&sigv4::hmac_sha256(
                &[0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    // ---- SigV4: the AWS test-suite "get-vanilla" case ----

    #[test]
    fn sigv4_get_vanilla_exact_strings() {
        let headers = [
            ("host", "example.amazonaws.com"),
            ("x-amz-date", "20150830T123600Z"),
        ];
        let (canonical, signed) = sigv4::canonical_request("GET", "/", "", &headers, b"");
        assert_eq!(signed, "host;x-amz-date");
        assert_eq!(
            canonical,
            "GET\n/\n\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\n\nhost;x-amz-date\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );

        let scope = "20150830/us-east-1/service/aws4_request";
        let to_sign = sigv4::string_to_sign("20150830T123600Z", scope, &canonical);
        assert_eq!(
            to_sign,
            "AWS4-HMAC-SHA256\n20150830T123600Z\n20150830/us-east-1/service/aws4_request\nbb579772317eb040ac9ed261061d46c1f17a8133879d6129b6e1c25292927e63"
        );

        let sig = sigv4::signature(
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            "20150830",
            "us-east-1",
            "service",
            &to_sign,
        );
        assert_eq!(
            sig,
            "5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    #[test]
    fn authorization_header_shape() {
        let auth = sigv4::authorization(&sigv4::RequestToSign {
            access_key_id: "AKIDEXAMPLE",
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            region: "us-east-1",
            service: "ec2",
            amz_date: "20150830T123600Z",
            host: "ec2.us-east-1.amazonaws.com",
            query: "",
            content_type: CONTENT_TYPE,
            amz_target: None,
            body: "Action=DescribeInstances&Version=2016-11-15",
        });
        assert!(auth.starts_with(
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/ec2/aws4_request, "
        ));
        assert!(auth.contains("SignedHeaders=content-type;host;x-amz-date, Signature="));
    }

    #[test]
    fn authorization_header_shape_for_ssm_signs_the_target() {
        let auth = sigv4::authorization(&sigv4::RequestToSign {
            access_key_id: "AKIDEXAMPLE",
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            region: "eu-west-1",
            service: "ssm",
            amz_date: "20150830T123600Z",
            host: "ssm.eu-west-1.amazonaws.com",
            query: "",
            content_type: SSM_CONTENT_TYPE,
            amz_target: Some(SSM_TARGET_GET_PARAMETER),
            body: r#"{"Name":"/aws/service/debian/release/12/latest/arm64"}"#,
        });
        assert!(auth.starts_with(
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/eu-west-1/ssm/aws4_request, "
        ));
        assert!(
            auth.contains("SignedHeaders=content-type;host;x-amz-date;x-amz-target, Signature=")
        );
    }

    /// The AWS SigV4 test-suite "get-vanilla" case driven through the
    /// generalized entry point, all the way to the published Authorization
    /// header. This is the guard on the S3 extension: if `authorization_for`
    /// ever drifts, this published vector breaks.
    #[test]
    fn authorization_for_matches_the_published_get_vanilla_vector() {
        let auth = sigv4::authorization_for(&sigv4::SignedRequest {
            access_key_id: "AKIDEXAMPLE",
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            region: "us-east-1",
            service: "service",
            method: "GET",
            canonical_uri: "/",
            query: "",
            amz_date: "20150830T123600Z",
            headers: &[
                ("host", "example.amazonaws.com"),
                ("x-amz-date", "20150830T123600Z"),
            ],
            payload_sha256: sigv4::EMPTY_PAYLOAD_SHA256,
        });
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, \
             SignedHeaders=host;x-amz-date, \
             Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    #[test]
    fn empty_payload_constant_matches_the_hash_of_nothing() {
        assert_eq!(sigv4::hex_sha256(b""), sigv4::EMPTY_PAYLOAD_SHA256);
    }

    /// The EC2/SSM entry point is now a thin wrapper; signing the same
    /// request both ways must produce byte-identical headers, which is what
    /// keeps the S3 extension from touching those callers.
    #[test]
    fn authorization_delegates_without_changing_its_output() {
        let body = "Action=DescribeInstances&Version=2016-11-15";
        let via_wrapper = sigv4::authorization(&sigv4::RequestToSign {
            access_key_id: "AKIDEXAMPLE",
            secret_access_key: "secret",
            region: "us-east-1",
            service: "ec2",
            amz_date: "20150830T123600Z",
            host: "ec2.us-east-1.amazonaws.com",
            query: "region=us-east-1",
            content_type: CONTENT_TYPE,
            amz_target: None,
            body,
        });
        let via_general = sigv4::authorization_for(&sigv4::SignedRequest {
            access_key_id: "AKIDEXAMPLE",
            secret_access_key: "secret",
            region: "us-east-1",
            service: "ec2",
            method: "POST",
            canonical_uri: "/",
            query: "region=us-east-1",
            amz_date: "20150830T123600Z",
            headers: &[
                ("content-type", CONTENT_TYPE),
                ("host", "ec2.us-east-1.amazonaws.com"),
                ("x-amz-date", "20150830T123600Z"),
            ],
            payload_sha256: &sigv4::hex_sha256(body.as_bytes()),
        });
        assert_eq!(via_wrapper, via_general);
    }

    /// An S3 PUT: keyed canonical URI, a sub-resource query string, and the
    /// mandatory signed `x-amz-content-sha256` header.
    #[test]
    fn authorization_for_signs_the_s3_header_set() {
        let payload = sigv4::hex_sha256(b"part bytes");
        let auth = sigv4::authorization_for(&sigv4::SignedRequest {
            access_key_id: "AKIDEXAMPLE",
            secret_access_key: "secret",
            region: "eu-west-1",
            service: "s3",
            method: "PUT",
            canonical_uri: "/jamstream/recordings/abc/mix.wav",
            query: "partNumber=2&uploadId=up-1",
            amz_date: "20150830T123600Z",
            headers: &[
                ("host", "bucket.s3.eu-west-1.amazonaws.com"),
                ("x-amz-content-sha256", &payload),
                ("x-amz-date", "20150830T123600Z"),
            ],
            payload_sha256: &payload,
        });
        assert!(auth.starts_with(
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/eu-west-1/s3/aws4_request, "
        ));
        assert!(auth.contains("SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature="));
    }

    // ---- Encoding and dates ----

    #[test]
    fn aws_encode_unreserved_set() {
        assert_eq!(aws_encode("AZaz09-_.~"), "AZaz09-_.~");
        assert_eq!(aws_encode("a b+c=d:e/f&g"), "a%20b%2Bc%3Dd%3Ae%2Ff%26g");
        assert_eq!(aws_encode("I2Nsb3VkLWNvbmZpZwo="), "I2Nsb3VkLWNvbmZpZwo%3D");
    }

    #[test]
    fn amz_date_formatting() {
        assert_eq!(amz_date(0), "19700101T000000Z");
        // 2015-08-30 12:36:00 UTC, the SigV4 test-suite timestamp.
        assert_eq!(amz_date(1_440_938_160), "20150830T123600Z");
        // Leap day.
        assert_eq!(amz_date(951_782_400), "20000229T000000Z");
    }

    // ---- XML extraction ----

    const DESCRIBE_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<DescribeInstancesResponse xmlns="http://ec2.amazonaws.com/doc/2016-11-15/">
  <requestId>req-1</requestId>
  <reservationSet>
    <item>
      <reservationId>r-1</reservationId>
      <instancesSet>
        <item>
          <instanceId>i-aaa</instanceId>
          <instanceState><code>16</code><name>running</name></instanceState>
          <privateIpAddress>10.0.0.5</privateIpAddress>
          <ipAddress>3.80.12.34</ipAddress>
          <tagSet>
            <item><key>jamstream-session</key><value>s&amp;1</value></item>
            <item><key>Name</key><value>jam</value></item>
          </tagSet>
        </item>
        <item>
          <instanceId>i-bbb</instanceId>
          <instanceState><code>0</code><name>pending</name></instanceState>
          <tagSet>
            <item><key>jamstream-session</key><value>s2</value></item>
          </tagSet>
        </item>
      </instancesSet>
    </item>
  </reservationSet>
</DescribeInstancesResponse>"#;

    fn test_region() -> Region {
        Region {
            provider: ProviderKind::Aws,
            id: RegionId::new("us-east-1"),
            display: "N. Virginia".to_owned(),
            country: "US".to_owned(),
        }
    }

    #[test]
    fn parse_instances_extracts_ids_ips_states_tags() {
        let parsed = parse_instances(&test_region(), DESCRIBE_XML);
        assert_eq!(parsed.len(), 2);

        assert_eq!(parsed[0].instance.id, "i-aaa");
        assert_eq!(parsed[0].state.as_deref(), Some("running"));
        assert_eq!(
            parsed[0].instance.public_ip,
            Some("3.80.12.34".parse().unwrap())
        );
        assert_eq!(
            parsed[0].instance.tags,
            vec![
                ("jamstream-session".to_owned(), "s&1".to_owned()),
                ("Name".to_owned(), "jam".to_owned()),
            ]
        );

        assert_eq!(parsed[1].instance.id, "i-bbb");
        assert_eq!(parsed[1].state.as_deref(), Some("pending"));
        assert_eq!(parsed[1].instance.public_ip, None);
        assert_eq!(parsed[1].instance.tags.len(), 1);
    }

    #[test]
    fn parse_instances_handles_empty_and_missing_sets() {
        let empty = "<DescribeInstancesResponse><reservationSet/></DescribeInstancesResponse>";
        assert!(parse_instances(&test_region(), empty).is_empty());
        assert!(parse_instances(&test_region(), "").is_empty());
    }

    #[test]
    fn xml_value_ignores_prefixed_tags() {
        // <privateIpAddress> must not satisfy a lookup for <ipAddress>.
        let xml = "<privateIpAddress>10.0.0.5</privateIpAddress>";
        assert_eq!(xml_value(xml, "ipAddress"), None);
    }

    // ---- Error body mapping ----

    fn error_xml(code: &str) -> String {
        format!(
            "<Response><Errors><Error><Code>{code}</Code><Message>msg</Message></Error></Errors><RequestID>rid</RequestID></Response>"
        )
    }

    #[test]
    fn map_error_body_codes() {
        assert!(matches!(
            map_error_body(&error_xml("InvalidInstanceID.NotFound")),
            ProviderError::NotFound(_)
        ));
        assert!(matches!(
            map_error_body(&error_xml("InvalidInstanceID.Malformed")),
            ProviderError::NotFound(_)
        ));
        assert!(matches!(
            map_error_body(&error_xml("UnauthorizedOperation")),
            ProviderError::Auth(_)
        ));
        assert!(matches!(
            map_error_body(&error_xml("RequestLimitExceeded")),
            ProviderError::RateLimited { .. }
        ));
        assert!(matches!(
            map_error_body(&error_xml("InstanceLimitExceeded")),
            ProviderError::QuotaExceeded(_)
        ));
        assert!(matches!(
            map_error_body(&error_xml("InsufficientInstanceCapacity")),
            ProviderError::QuotaExceeded(_)
        ));
        assert!(matches!(
            map_error_body(&error_xml("ValidationError")),
            ProviderError::Other(_)
        ));
        assert!(matches!(
            map_error_body("not xml at all"),
            ProviderError::Other(_)
        ));
    }

    // ---- Catalog, data file, constructors ----

    #[test]
    fn data_file_covers_exactly_the_region_catalog() {
        let d = data();
        assert_eq!(d.regions.len(), REGIONS.len());
        for (id, _, _) in REGIONS {
            let rd = d
                .regions
                .get(*id)
                .unwrap_or_else(|| panic!("data file missing region {id}"));
            assert!(rd.small_hourly_microusd > 0, "zero price for {id}");
            assert!(
                rd.medium_hourly_microusd > rd.small_hourly_microusd,
                "medium must cost more than small for {id}"
            );
            assert!(rd.ami.starts_with("ami-"), "bad ami id for {id}");
        }
        assert_eq!(d.egress_microusd_per_gb, 90_000);
        assert_eq!(d.included_egress_gb, 100);
    }

    #[tokio::test]
    async fn price_and_regions_are_static_and_consistent() {
        let p = AwsProvider::new("id".into(), "secret".into());
        assert_eq!(p.kind(), ProviderKind::Aws);
        let regions = p.regions();
        assert_eq!(regions.len(), 8);
        assert_eq!(regions[0].id.as_str(), "us-east-1");
        for r in &regions {
            assert_eq!(r.provider, ProviderKind::Aws);
            let price = p.price(&r.id).await.unwrap();
            assert!(price.hourly_microusd > 0);
            assert_eq!(price.egress_microusd_per_gb, 90_000);
            assert_eq!(price.included_egress_gb, 100);
        }
        assert_eq!(
            p.price(&RegionId::new("us-east-1"))
                .await
                .unwrap()
                .hourly_microusd,
            33_600
        );
        assert!(matches!(
            p.price(&RegionId::new("mars-north-1")).await,
            Err(ProviderError::NotFound(_))
        ));
    }

    #[test]
    fn from_env_lookup_requires_both_variables() {
        let ok = AwsProvider::from_env_lookup(|key| Some(format!("{key}-value")));
        assert!(ok.is_ok());
        for missing in ["AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY"] {
            let err =
                AwsProvider::from_env_lookup(|key| (key != missing).then(|| "value".to_owned()))
                    .unwrap_err();
            match err {
                ProviderError::Auth(msg) => assert!(msg.contains(missing)),
                other => panic!("expected Auth, got {other:?}"),
            }
        }
    }

    #[test]
    fn debug_redacts_the_secret() {
        let p = AwsProvider::new("AKIDVISIBLE".into(), "super-secret-value".into());
        let dbg = format!("{p:?}");
        assert!(dbg.contains("AKIDVISIBLE"));
        assert!(!dbg.contains("super-secret-value"));
        assert!(dbg.contains("<redacted>"));
    }

    #[test]
    fn endpoint_override_carries_region_as_query() {
        let p = AwsProvider::new("id".into(), "secret".into());
        assert_eq!(
            p.endpoint("ec2", "eu-west-1"),
            (
                "https://ec2.eu-west-1.amazonaws.com".to_owned(),
                String::new()
            )
        );
        assert_eq!(
            p.endpoint("ssm", "eu-west-1"),
            (
                "https://ssm.eu-west-1.amazonaws.com".to_owned(),
                String::new()
            )
        );
        let p = p.with_base_url("http://127.0.0.1:9/".to_owned());
        assert_eq!(
            p.endpoint("ec2", "eu-west-1"),
            (
                "http://127.0.0.1:9".to_owned(),
                "region=eu-west-1".to_owned()
            )
        );
    }

    #[test]
    fn instance_type_mapping() {
        assert_eq!(instance_type(InstanceClass::Small), "t4g.small");
        assert_eq!(instance_type(InstanceClass::Standard), "t4g.medium");
    }

    /// #139: every instance type here is Graviton and the AMI parameter is
    /// the arm64 build, so the artifact this provider selects must be the
    /// aarch64 one; the x86_64 binary dies at exec on these machines.
    #[test]
    fn launches_arm64_machines_and_says_so() {
        let provider = AwsProvider::new("AKIATEST".to_owned(), "secret".to_owned());
        assert_eq!(provider.server_arch(), ServerArch::Aarch64);
        assert!(DEBIAN_12_ARM64_PARAM.ends_with("arm64"));
        for class in [InstanceClass::Small, InstanceClass::Standard] {
            assert!(instance_type(class).starts_with("t4g."));
        }
    }
}
