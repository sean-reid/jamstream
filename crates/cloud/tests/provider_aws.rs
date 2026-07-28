//! Wiremock-backed integration tests for the AWS EC2 provider, plus the
//! generic provider contract run against a small stateful fake EC2 Query
//! API. The provider's base_url override routes every region to one mock
//! server and carries the region as a `?region=...` query parameter, which
//! lets mocks and the fake discriminate regions.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use jamstream_cloud::providers::aws::AwsProvider;
use jamstream_cloud::{
    InstanceClass, LaunchSpec, Provider, ProviderError, ProviderKind, Region, RegionId,
    assert_provider_contract, session_tag,
};
use wiremock::matchers::{body_string_contains, header, method, path, query_param};
use wiremock::{Match, Mock, MockServer, Request, Respond, ResponseTemplate};

const ACCESS_KEY_ID: &str = "AKIDTEST";

fn provider(server: &MockServer) -> AwsProvider {
    AwsProvider::new(ACCESS_KEY_ID.to_owned(), "test-secret-key".to_owned())
        .with_base_url(server.uri())
}

fn region_of(p: &AwsProvider, id: &str) -> Region {
    p.regions()
        .into_iter()
        .find(|r| r.id.as_str() == id)
        .expect("region in catalog")
}

fn error_body(code: &str, message: &str) -> String {
    format!(
        "<Response><Errors><Error><Code>{code}</Code><Message>{message}</Message></Error></Errors><RequestID>test-req</RequestID></Response>"
    )
}

/// Mounts the two calls a launch makes before RunInstances: the session's
/// own security group, and the one port it opens. Tests that care about
/// their shape assert on it themselves; the rest just need them answered.
async fn mount_security_group(server: &MockServer, group_id: &str) {
    Mock::given(method("POST"))
        .and(body_string_contains("Action=CreateSecurityGroup"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            "<CreateSecurityGroupResponse><groupId>{group_id}</groupId>\
             <return>true</return></CreateSecurityGroupResponse>"
        )))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(body_string_contains("Action=AuthorizeSecurityGroupIngress"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "<AuthorizeSecurityGroupIngressResponse><return>true</return>\
             </AuthorizeSecurityGroupIngressResponse>",
        ))
        .mount(server)
        .await;
}

/// Matches requests carrying a well-formed SigV4 authorization for the
/// given region and service, signed with the test access key.
struct SignedFor {
    region: &'static str,
    service: &'static str,
}

impl Match for SignedFor {
    fn matches(&self, request: &Request) -> bool {
        let Some(auth) = request
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
        else {
            return false;
        };
        auth.starts_with(&format!("AWS4-HMAC-SHA256 Credential={ACCESS_KEY_ID}/"))
            && auth.contains(&format!("/{}/{}/aws4_request", self.region, self.service))
            && auth.contains("SignedHeaders=content-type;host;x-amz-date")
            && request.headers.get("x-amz-date").is_some()
    }
}

// ---- RunInstances ----

#[tokio::test]
async fn run_instances_happy_path() {
    let server = MockServer::start().await;
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<RunInstancesResponse xmlns="http://ec2.amazonaws.com/doc/2016-11-15/">
  <requestId>59dbff89-35bd-4eac-99ed-be587EXAMPLE</requestId>
  <reservationId>r-0abc</reservationId>
  <instancesSet>
    <item>
      <instanceId>i-1234567890abcdef0</instanceId>
      <instanceState><code>0</code><name>pending</name></instanceState>
      <tagSet><item><key>jamstream-session</key><value>sess1</value></item></tagSet>
    </item>
  </instancesSet>
</RunInstancesResponse>"#;
    Mock::given(method("POST"))
        .and(path("/"))
        .and(query_param("region", "us-east-1"))
        .and(body_string_contains("Action=RunInstances"))
        .and(body_string_contains("Version=2016-11-15"))
        .and(body_string_contains("ImageId=ami-"))
        .and(body_string_contains("InstanceType=t4g.small"))
        .and(body_string_contains("MinCount=1"))
        .and(body_string_contains("MaxCount=1"))
        .and(body_string_contains(
            "InstanceInitiatedShutdownBehavior=terminate",
        ))
        // The instance is in its own security group, not the VPC default,
        // and its metadata service takes IMDSv2 tokens only.
        .and(body_string_contains("SecurityGroupId.1=sg-jamstream"))
        .and(body_string_contains("MetadataOptions.HttpTokens=required"))
        .and(body_string_contains(
            "MetadataOptions.HttpPutResponseHopLimit=1",
        ))
        // base64("#cloud-config\n") with '=' percent-encoded.
        .and(body_string_contains("UserData=I2Nsb3VkLWNvbmZpZwo%3D"))
        .and(body_string_contains(
            "TagSpecification.1.ResourceType=instance",
        ))
        .and(body_string_contains(
            "TagSpecification.1.Tag.1.Key=jamstream-session",
        ))
        .and(body_string_contains("TagSpecification.1.Tag.1.Value=sess1"))
        .and(SignedFor {
            region: "us-east-1",
            service: "ec2",
        })
        .respond_with(ResponseTemplate::new(200).set_body_string(xml))
        .expect(1)
        .mount(&server)
        .await;

    mount_security_group(&server, "sg-jamstream").await;

    let p = provider(&server);
    let spec = LaunchSpec {
        region: region_of(&p, "us-east-1"),
        instance_class: InstanceClass::Small,
        user_data: "#cloud-config\n".to_owned(),
        tags: vec![session_tag("sess1")],
    };
    let inst = p.launch(spec).await.unwrap();
    assert_eq!(inst.id, "i-1234567890abcdef0");
    assert_eq!(inst.provider, ProviderKind::Aws);
    assert_eq!(inst.region.id.as_str(), "us-east-1");
    assert_eq!(inst.public_ip, None);
    assert_eq!(inst.session_id(), Some("sess1"));
}

#[tokio::test]
async fn launch_in_unknown_region_is_not_found_without_network() {
    let server = MockServer::start().await;
    let p = provider(&server);
    let spec = LaunchSpec {
        region: Region {
            provider: ProviderKind::Aws,
            id: RegionId::new("nope-central-9"),
            display: String::new(),
            country: String::new(),
        },
        instance_class: InstanceClass::Small,
        user_data: String::new(),
        tags: vec![session_tag("s")],
    };
    assert!(matches!(
        p.launch(spec).await,
        Err(ProviderError::NotFound(_))
    ));
    assert!(server.received_requests().await.unwrap().is_empty());
}

// ---- TerminateInstances ----

#[tokio::test]
async fn terminate_unknown_instance_maps_to_not_found() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_string_contains("Action=TerminateInstances"))
        .and(body_string_contains("InstanceId.1=i-deadbeef"))
        .respond_with(ResponseTemplate::new(400).set_body_string(error_body(
            "InvalidInstanceID.NotFound",
            "The instance ID 'i-deadbeef' does not exist",
        )))
        // The shared http layer carries the 400 body on the error, so one
        // request suffices to map the EC2 error code.
        .expect(1)
        .mount(&server)
        .await;

    let p = provider(&server);
    let err = p
        .destroy(&RegionId::new("us-east-1"), "i-deadbeef")
        .await
        .unwrap_err();
    assert!(matches!(err, ProviderError::NotFound(_)), "got {err:?}");
}

#[tokio::test]
async fn destroy_in_unknown_region_is_not_found_without_network() {
    let server = MockServer::start().await;
    let p = provider(&server);
    let err = p
        .destroy(&RegionId::new("nope-central-9"), "i-123")
        .await
        .unwrap_err();
    assert!(matches!(err, ProviderError::NotFound(_)));
    assert!(server.received_requests().await.unwrap().is_empty());
}

// ---- Retry and auth classification through the shared http layer ----

#[tokio::test]
async fn transient_500_retries_to_success() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_string_contains("Action=TerminateInstances"))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_string_contains("Action=TerminateInstances"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "<TerminateInstancesResponse><instancesSet><item><instanceId>i-123</instanceId></item></instancesSet></TerminateInstancesResponse>",
        ))
        .expect(1)
        .mount(&server)
        .await;

    let p = provider(&server);
    p.destroy(&RegionId::new("us-east-1"), "i-123")
        .await
        .expect("destroy retries the 500 and succeeds");
}

#[tokio::test]
async fn http_401_maps_to_auth_without_retry() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401).set_body_string(error_body(
            "AuthFailure",
            "AWS was not able to validate the provided access credentials",
        )))
        .expect(1)
        .mount(&server)
        .await;

    let p = provider(&server);
    let err = p
        .destroy(&RegionId::new("us-east-1"), "i-123")
        .await
        .unwrap_err();
    assert!(matches!(err, ProviderError::Auth(_)), "got {err:?}");
}

// ---- DescribeInstances / list_tagged ----

const EMPTY_DESCRIBE: &str = "<DescribeInstancesResponse><requestId>r</requestId><reservationSet/></DescribeInstancesResponse>";

const SESSION_DESCRIBE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<DescribeInstancesResponse xmlns="http://ec2.amazonaws.com/doc/2016-11-15/">
  <requestId>req-42</requestId>
  <reservationSet>
    <item>
      <reservationId>r-1</reservationId>
      <instancesSet>
        <item>
          <instanceId>i-aaa11122233344455</instanceId>
          <instanceState><code>16</code><name>running</name></instanceState>
          <privateIpAddress>10.0.0.5</privateIpAddress>
          <ipAddress>3.80.12.34</ipAddress>
          <tagSet>
            <item><key>jamstream-session</key><value>sess1</value></item>
            <item><key>Name</key><value>jam</value></item>
          </tagSet>
        </item>
        <item>
          <instanceId>i-bbb11122233344455</instanceId>
          <instanceState><code>0</code><name>pending</name></instanceState>
          <tagSet>
            <item><key>jamstream-session</key><value>sess1</value></item>
          </tagSet>
        </item>
      </instancesSet>
    </item>
  </reservationSet>
</DescribeInstancesResponse>"#;

#[tokio::test]
async fn list_tagged_by_session_filters_and_parses_instances() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(query_param("region", "us-east-1"))
        .and(body_string_contains("Action=DescribeInstances"))
        .and(body_string_contains("Filter.1.Name=instance-state-name"))
        .and(body_string_contains("Filter.1.Value.1=pending"))
        .and(body_string_contains("Filter.1.Value.2=running"))
        // "tag:jamstream-session" with ':' percent-encoded.
        .and(body_string_contains(
            "Filter.2.Name=tag%3Ajamstream-session",
        ))
        .and(body_string_contains("Filter.2.Value.1=sess1"))
        .respond_with(ResponseTemplate::new(200).set_body_string(SESSION_DESCRIBE))
        .expect(1)
        .mount(&server)
        .await;
    // Every other region answers empty.
    Mock::given(method("POST"))
        .and(body_string_contains("Action=DescribeInstances"))
        .respond_with(ResponseTemplate::new(200).set_body_string(EMPTY_DESCRIBE))
        .expect(7)
        .mount(&server)
        .await;

    let p = provider(&server);
    let instances = p.list_tagged(Some("sess1")).await.unwrap();
    assert_eq!(instances.len(), 2);

    assert_eq!(instances[0].id, "i-aaa11122233344455");
    assert_eq!(instances[0].region.id.as_str(), "us-east-1");
    assert_eq!(instances[0].public_ip, Some("3.80.12.34".parse().unwrap()));
    assert_eq!(instances[0].session_id(), Some("sess1"));
    assert_eq!(instances[0].tags.len(), 2);

    assert_eq!(instances[1].id, "i-bbb11122233344455");
    assert_eq!(instances[1].public_ip, None);
    assert_eq!(instances[1].session_id(), Some("sess1"));
}

#[tokio::test]
async fn list_tagged_all_sessions_uses_tag_key_filter() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(query_param("region", "eu-central-1"))
        .and(body_string_contains("Action=DescribeInstances"))
        .and(body_string_contains("Filter.2.Name=tag-key"))
        .and(body_string_contains("Filter.2.Value.1=jamstream-session"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "<DescribeInstancesResponse><reservationSet><item><instancesSet><item>\
             <instanceId>i-orphan1</instanceId>\
             <instanceState><code>16</code><name>running</name></instanceState>\
             <tagSet><item><key>jamstream-session</key><value>lost</value></item></tagSet>\
             </item></instancesSet></item></reservationSet></DescribeInstancesResponse>",
        ))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string(EMPTY_DESCRIBE))
        .expect(7)
        .mount(&server)
        .await;

    let p = provider(&server);
    let instances = p.list_tagged(None).await.unwrap();
    assert_eq!(instances.len(), 1);
    assert_eq!(instances[0].id, "i-orphan1");
    assert_eq!(instances[0].region.id.as_str(), "eu-central-1");
    assert_eq!(instances[0].session_id(), Some("lost"));
}

#[tokio::test]
async fn list_tagged_tolerates_a_broken_region() {
    let server = MockServer::start().await;
    // us-east-1 is persistently on fire; its orphans are unreachable but
    // that must not hide the orphan in eu-west-1.
    Mock::given(method("POST"))
        .and(query_param("region", "us-east-1"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(query_param("region", "eu-west-1"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "<DescribeInstancesResponse><reservationSet><item><instancesSet><item>\
             <instanceId>i-eu1</instanceId>\
             <instanceState><code>16</code><name>running</name></instanceState>\
             <ipAddress>52.16.0.9</ipAddress>\
             <tagSet><item><key>jamstream-session</key><value>sess9</value></item></tagSet>\
             </item></instancesSet></item></reservationSet></DescribeInstancesResponse>",
        ))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string(EMPTY_DESCRIBE))
        .mount(&server)
        .await;

    let p = provider(&server);
    let instances = p.list_tagged(None).await.unwrap();
    assert_eq!(instances.len(), 1);
    assert_eq!(instances[0].id, "i-eu1");
    assert_eq!(instances[0].region.id.as_str(), "eu-west-1");
    assert_eq!(instances[0].public_ip, Some("52.16.0.9".parse().unwrap()));
}

#[tokio::test]
async fn list_tagged_fails_when_every_region_fails() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let p = provider(&server);
    let err = p.list_tagged(None).await.unwrap_err();
    assert!(matches!(err, ProviderError::Transient(_)), "got {err:?}");
}

// ---- refresh ----

#[tokio::test]
async fn refresh_reports_ip_and_not_found_when_terminated() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_string_contains("Action=DescribeInstances"))
        .and(body_string_contains("InstanceId.1=i-live"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "<DescribeInstancesResponse><reservationSet><item><instancesSet><item>\
             <instanceId>i-live</instanceId>\
             <instanceState><code>16</code><name>running</name></instanceState>\
             <ipAddress>54.1.2.3</ipAddress>\
             <tagSet><item><key>jamstream-session</key><value>sess1</value></item></tagSet>\
             </item></instancesSet></item></reservationSet></DescribeInstancesResponse>",
        ))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_string_contains("Action=DescribeInstances"))
        .and(body_string_contains("InstanceId.1=i-dead"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "<DescribeInstancesResponse><reservationSet><item><instancesSet><item>\
             <instanceId>i-dead</instanceId>\
             <instanceState><code>48</code><name>terminated</name></instanceState>\
             </item></instancesSet></item></reservationSet></DescribeInstancesResponse>",
        ))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_string_contains("Action=DescribeInstances"))
        .and(body_string_contains("InstanceId.1=i-gone"))
        .respond_with(ResponseTemplate::new(400).set_body_string(error_body(
            "InvalidInstanceID.NotFound",
            "The instance ID 'i-gone' does not exist",
        )))
        .mount(&server)
        .await;

    let p = provider(&server);
    let region = RegionId::new("us-east-1");

    let live = p.refresh(&region, "i-live").await.unwrap();
    assert_eq!(live.id, "i-live");
    assert_eq!(live.public_ip, Some("54.1.2.3".parse().unwrap()));
    assert_eq!(live.session_id(), Some("sess1"));

    assert!(matches!(
        p.refresh(&region, "i-dead").await,
        Err(ProviderError::NotFound(_))
    ));
    assert!(matches!(
        p.refresh(&region, "i-gone").await,
        Err(ProviderError::NotFound(_))
    ));
}

// ---- SSM AMI resolution ----

const SSM_PARAM: &str = "/aws/service/debian/release/12/latest/arm64";
/// Bundled fallback AMI for us-east-1 in data/aws_prices.json.
const BUNDLED_US_EAST_1_AMI: &str = "ami-0e2c8caa4b6378d8c";

fn ssm_parameter_response(ami: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "Parameter": {
            "ARN": format!("arn:aws:ssm:us-east-1::parameter{SSM_PARAM}"),
            "Name": SSM_PARAM,
            "Type": "String",
            "Value": ami,
            "Version": 7,
        }
    }))
}

fn launch_spec(p: &AwsProvider, region: &str) -> LaunchSpec {
    LaunchSpec {
        region: region_of(p, region),
        instance_class: InstanceClass::Small,
        user_data: "#cloud-config\n".to_owned(),
        tags: vec![session_tag("sess1")],
    }
}

const RUN_INSTANCES_XML: &str = "<RunInstancesResponse><instancesSet><item>\
     <instanceId>i-fromssm</instanceId>\
     <instanceState><code>0</code><name>pending</name></instanceState>\
     </item></instancesSet></RunInstancesResponse>";

#[tokio::test]
async fn launch_resolves_ami_via_ssm_get_parameter() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(query_param("region", "us-east-1"))
        .and(header("x-amz-target", "AmazonSSM.GetParameter"))
        .and(header("content-type", "application/x-amz-json-1.1"))
        .and(body_string_contains(SSM_PARAM))
        .and(SignedFor {
            region: "us-east-1",
            service: "ssm",
        })
        .respond_with(ssm_parameter_response("ami-0123resolved456789"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_string_contains("Action=RunInstances"))
        .and(body_string_contains("ImageId=ami-0123resolved456789"))
        .and(SignedFor {
            region: "us-east-1",
            service: "ec2",
        })
        .respond_with(ResponseTemplate::new(200).set_body_string(RUN_INSTANCES_XML))
        .expect(1)
        .mount(&server)
        .await;

    mount_security_group(&server, "sg-jamstream").await;

    let p = provider(&server);
    let inst = p.launch(launch_spec(&p, "us-east-1")).await.unwrap();
    assert_eq!(inst.id, "i-fromssm");
}

#[tokio::test]
async fn launch_falls_back_to_bundled_ami_when_ssm_fails() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(header("x-amz-target", "AmazonSSM.GetParameter"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_string_contains("Action=RunInstances"))
        .and(body_string_contains(format!(
            "ImageId={BUNDLED_US_EAST_1_AMI}"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_string(RUN_INSTANCES_XML))
        .expect(1)
        .mount(&server)
        .await;

    mount_security_group(&server, "sg-jamstream").await;

    let p = provider(&server);
    let inst = p.launch(launch_spec(&p, "us-east-1")).await.unwrap();
    assert_eq!(inst.id, "i-fromssm");
}

#[tokio::test]
async fn ssm_ami_resolution_is_cached_per_region() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(header("x-amz-target", "AmazonSSM.GetParameter"))
        .respond_with(ssm_parameter_response("ami-0cachedvalue000001"))
        // The whole point: one SSM round trip serves both launches.
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_string_contains("Action=RunInstances"))
        .and(body_string_contains("ImageId=ami-0cachedvalue000001"))
        .respond_with(ResponseTemplate::new(200).set_body_string(RUN_INSTANCES_XML))
        .expect(2)
        .mount(&server)
        .await;

    mount_security_group(&server, "sg-jamstream").await;

    let p = provider(&server);
    p.launch(launch_spec(&p, "us-east-1")).await.unwrap();
    p.launch(launch_spec(&p, "us-east-1")).await.unwrap();
    server.verify().await;
}

// ---- Generic provider contract against a stateful fake EC2 ----

#[derive(Clone)]
struct FakeInstance {
    id: String,
    tags: Vec<(String, String)>,
}

/// One authorized permission, as the strings the Query API sends and the
/// XML sends back.
#[derive(Clone, PartialEq)]
struct FakePermission {
    protocol: String,
    from_port: String,
    to_port: String,
    v4: Vec<String>,
    v6: Vec<String>,
}

#[derive(Clone)]
struct FakeGroup {
    id: String,
    name: String,
    tags: Vec<(String, String)>,
    ingress: Vec<FakePermission>,
}

/// A tiny stateful EC2 Query API: per-region instance and security group
/// stores keyed by the `?region=...` query parameter the provider adds under
/// a base_url override. Just enough of RunInstances, TerminateInstances,
/// DescribeInstances, and the four security group calls for the behavioral
/// contract suite.
#[derive(Default)]
struct FakeEc2 {
    state: Mutex<HashMap<String, Vec<FakeInstance>>>,
    groups: Mutex<HashMap<String, Vec<FakeGroup>>>,
    next_id: AtomicU64,
}

fn form_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_form(body: &str) -> HashMap<String, String> {
    body.split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(k, v)| (form_decode(k), form_decode(v)))
        .collect()
}

fn xml_error(code: &str, message: &str) -> ResponseTemplate {
    ResponseTemplate::new(400).set_body_string(error_body(code, message))
}

impl FakeEc2 {
    fn run_instances(&self, region: &str, params: &HashMap<String, String>) -> ResponseTemplate {
        let mut tags = Vec::new();
        let mut n = 1;
        while let (Some(key), Some(value)) = (
            params.get(&format!("TagSpecification.1.Tag.{n}.Key")),
            params.get(&format!("TagSpecification.1.Tag.{n}.Value")),
        ) {
            tags.push((key.clone(), value.clone()));
            n += 1;
        }
        let id = format!("i-fake{:012x}", self.next_id.fetch_add(1, Ordering::SeqCst));
        self.state
            .lock()
            .unwrap()
            .entry(region.to_owned())
            .or_default()
            .push(FakeInstance {
                id: id.clone(),
                tags,
            });
        ResponseTemplate::new(200).set_body_string(format!(
            "<RunInstancesResponse><instancesSet><item>\
             <instanceId>{id}</instanceId>\
             <instanceState><code>0</code><name>pending</name></instanceState>\
             </item></instancesSet></RunInstancesResponse>"
        ))
    }

    fn terminate_instances(
        &self,
        region: &str,
        params: &HashMap<String, String>,
    ) -> ResponseTemplate {
        let Some(id) = params.get("InstanceId.1") else {
            return xml_error("MissingParameter", "InstanceId is required");
        };
        let mut state = self.state.lock().unwrap();
        let instances = state.entry(region.to_owned()).or_default();
        let before = instances.len();
        instances.retain(|i| &i.id != id);
        if instances.len() == before {
            return xml_error(
                "InvalidInstanceID.NotFound",
                &format!("The instance ID '{id}' does not exist"),
            );
        }
        ResponseTemplate::new(200).set_body_string(format!(
            "<TerminateInstancesResponse><instancesSet><item>\
             <instanceId>{id}</instanceId>\
             </item></instancesSet></TerminateInstancesResponse>"
        ))
    }

    fn describe_instances(
        &self,
        region: &str,
        params: &HashMap<String, String>,
    ) -> ResponseTemplate {
        let mut filters: Vec<(String, Vec<String>)> = Vec::new();
        let mut n = 1;
        while let Some(name) = params.get(&format!("Filter.{n}.Name")) {
            let mut values = Vec::new();
            let mut m = 1;
            while let Some(value) = params.get(&format!("Filter.{n}.Value.{m}")) {
                values.push(value.clone());
                m += 1;
            }
            filters.push((name.clone(), values));
            n += 1;
        }

        let state = self.state.lock().unwrap();
        let empty = Vec::new();
        let instances = state.get(region).unwrap_or(&empty);
        let mut items = String::new();
        for inst in instances.iter().filter(|inst| {
            filters.iter().all(|(name, values)| match name.as_str() {
                // Every fake instance is considered running.
                "instance-state-name" => values.iter().any(|v| v == "running"),
                "tag-key" => values.iter().any(|v| inst.tags.iter().any(|(k, _)| k == v)),
                name => match name.strip_prefix("tag:") {
                    Some(key) => inst
                        .tags
                        .iter()
                        .any(|(k, v)| k == key && values.contains(v)),
                    None => true,
                },
            })
        }) {
            let tags_xml: String = inst
                .tags
                .iter()
                .map(|(k, v)| format!("<item><key>{k}</key><value>{v}</value></item>"))
                .collect();
            let _ = write!(
                items,
                "<item><instancesSet><item>\
                 <instanceId>{}</instanceId>\
                 <instanceState><code>16</code><name>running</name></instanceState>\
                 <ipAddress>198.51.100.7</ipAddress>\
                 <tagSet>{tags_xml}</tagSet>\
                 </item></instancesSet></item>",
                inst.id
            );
        }
        ResponseTemplate::new(200).set_body_string(format!(
            "<DescribeInstancesResponse><reservationSet>{items}</reservationSet></DescribeInstancesResponse>"
        ))
    }
}

impl FakeEc2 {
    fn create_security_group(
        &self,
        region: &str,
        params: &HashMap<String, String>,
    ) -> ResponseTemplate {
        let Some(name) = params.get("GroupName") else {
            return xml_error("MissingParameter", "GroupName is required");
        };
        if params.get("GroupDescription").is_none() {
            return xml_error("MissingParameter", "GroupDescription is required");
        }
        let mut groups = self.groups.lock().unwrap();
        let in_region = groups.entry(region.to_owned()).or_default();
        if in_region.iter().any(|g| &g.name == name) {
            return xml_error(
                "InvalidGroup.Duplicate",
                &format!("The security group '{name}' already exists"),
            );
        }
        let mut tags = Vec::new();
        let mut n = 1;
        while let (Some(key), Some(value)) = (
            params.get(&format!("TagSpecification.1.Tag.{n}.Key")),
            params.get(&format!("TagSpecification.1.Tag.{n}.Value")),
        ) {
            tags.push((key.clone(), value.clone()));
            n += 1;
        }
        let id = format!(
            "sg-fake{:012x}",
            self.next_id.fetch_add(1, Ordering::SeqCst)
        );
        in_region.push(FakeGroup {
            id: id.clone(),
            name: name.clone(),
            tags,
            ingress: Vec::new(),
        });
        ResponseTemplate::new(200).set_body_string(format!(
            "<CreateSecurityGroupResponse><groupId>{id}</groupId>\
             <return>true</return></CreateSecurityGroupResponse>"
        ))
    }

    fn authorize_ingress(
        &self,
        region: &str,
        params: &HashMap<String, String>,
    ) -> ResponseTemplate {
        let Some(group_id) = params.get("GroupId") else {
            return xml_error("MissingParameter", "GroupId is required");
        };
        let mut groups = self.groups.lock().unwrap();
        let Some(group) = groups
            .get_mut(region)
            .and_then(|gs| gs.iter_mut().find(|g| &g.id == group_id))
        else {
            return xml_error(
                "InvalidGroup.NotFound",
                &format!("The security group '{group_id}' does not exist"),
            );
        };
        let get = |key: &str| params.get(key).cloned().unwrap_or_default();
        let list = |key: &str| {
            let mut out = Vec::new();
            let mut n = 1;
            while let Some(v) = params.get(&format!(
                "IpPermissions.1.{key}.{n}.{key2}",
                key2 = if key == "IpRanges" {
                    "CidrIp"
                } else {
                    "CidrIpv6"
                }
            )) {
                out.push(v.clone());
                n += 1;
            }
            out
        };
        let rule = FakePermission {
            protocol: get("IpPermissions.1.IpProtocol"),
            from_port: get("IpPermissions.1.FromPort"),
            to_port: get("IpPermissions.1.ToPort"),
            v4: list("IpRanges"),
            v6: list("Ipv6Ranges"),
        };
        if group.ingress.contains(&rule) {
            return xml_error(
                "InvalidPermission.Duplicate",
                "the specified rule already exists",
            );
        }
        group.ingress.push(rule);
        ResponseTemplate::new(200).set_body_string(
            "<AuthorizeSecurityGroupIngressResponse><return>true</return>\
             </AuthorizeSecurityGroupIngressResponse>",
        )
    }

    fn describe_security_groups(
        &self,
        region: &str,
        params: &HashMap<String, String>,
    ) -> ResponseTemplate {
        let name_filter = (params.get("Filter.1.Name").map(String::as_str) == Some("group-name"))
            .then(|| params.get("Filter.1.Value.1").cloned().unwrap_or_default());
        let tag_key_filter = (params.get("Filter.1.Name").map(String::as_str) == Some("tag-key"))
            .then(|| params.get("Filter.1.Value.1").cloned().unwrap_or_default());
        let groups = self.groups.lock().unwrap();
        let empty = Vec::new();
        let mut items = String::new();
        for group in groups.get(region).unwrap_or(&empty) {
            if let Some(name) = &name_filter
                && &group.name != name
            {
                continue;
            }
            if let Some(key) = &tag_key_filter
                && !group.tags.iter().any(|(k, _)| k == key)
            {
                continue;
            }
            let permissions: String = group
                .ingress
                .iter()
                .map(|rule| {
                    let v4: String = rule
                        .v4
                        .iter()
                        .map(|c| format!("<item><cidrIp>{c}</cidrIp></item>"))
                        .collect();
                    let v6: String = rule
                        .v6
                        .iter()
                        .map(|c| format!("<item><cidrIpv6>{c}</cidrIpv6></item>"))
                        .collect();
                    format!(
                        "<item><ipProtocol>{}</ipProtocol>\
                         <fromPort>{}</fromPort><toPort>{}</toPort>\
                         <ipRanges>{v4}</ipRanges><ipv6Ranges>{v6}</ipv6Ranges></item>",
                        rule.protocol, rule.from_port, rule.to_port
                    )
                })
                .collect();
            let _ = write!(
                items,
                "<item><groupId>{}</groupId><groupName>{}</groupName>\
                 <ipPermissions>{permissions}</ipPermissions>\
                 <ipPermissionsEgress><item><ipProtocol>-1</ipProtocol>\
                 <ipRanges><item><cidrIp>0.0.0.0/0</cidrIp></item></ipRanges>\
                 </item></ipPermissionsEgress></item>",
                group.id, group.name
            );
        }
        ResponseTemplate::new(200).set_body_string(format!(
            "<DescribeSecurityGroupsResponse><securityGroupInfo>{items}</securityGroupInfo>\
             </DescribeSecurityGroupsResponse>"
        ))
    }

    fn delete_security_group(
        &self,
        region: &str,
        params: &HashMap<String, String>,
    ) -> ResponseTemplate {
        let Some(group_id) = params.get("GroupId") else {
            return xml_error("MissingParameter", "GroupId is required");
        };
        let mut groups = self.groups.lock().unwrap();
        let in_region = groups.entry(region.to_owned()).or_default();
        let Some(index) = in_region.iter().position(|g| &g.id == group_id) else {
            return xml_error(
                "InvalidGroup.NotFound",
                &format!("The security group '{group_id}' does not exist"),
            );
        };
        // Real EC2 refuses while an instance still holds the group, which is
        // what makes cleanup a retry rather than part of destroy.
        let attached = self
            .state
            .lock()
            .unwrap()
            .get(region)
            .is_some_and(|instances| {
                instances.iter().any(|i| {
                    i.tags.iter().any(|(k, v)| {
                        k == "jamstream-session" && in_region[index].name.ends_with(v.as_str())
                    })
                })
            });
        if attached {
            return xml_error("DependencyViolation", "resource sg has a dependent object");
        }
        in_region.remove(index);
        ResponseTemplate::new(200).set_body_string(
            "<DeleteSecurityGroupResponse><return>true</return></DeleteSecurityGroupResponse>",
        )
    }
}

impl Respond for FakeEc2 {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let region = request
            .url
            .query_pairs()
            .find(|(k, _)| k == "region")
            .map(|(_, v)| v.into_owned())
            .unwrap_or_default();
        let body = String::from_utf8_lossy(&request.body);
        let params = parse_form(&body);
        match params.get("Action").map(String::as_str) {
            Some("RunInstances") => self.run_instances(&region, &params),
            Some("TerminateInstances") => self.terminate_instances(&region, &params),
            Some("DescribeInstances") => self.describe_instances(&region, &params),
            Some("CreateSecurityGroup") => self.create_security_group(&region, &params),
            Some("AuthorizeSecurityGroupIngress") => self.authorize_ingress(&region, &params),
            Some("DescribeSecurityGroups") => self.describe_security_groups(&region, &params),
            Some("DeleteSecurityGroup") => self.delete_security_group(&region, &params),
            _ => xml_error("InvalidAction", "unsupported action"),
        }
    }
}

#[tokio::test]
async fn aws_provider_passes_the_generic_contract() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(FakeEc2::default())
        .mount(&server)
        .await;
    let p = provider(&server);
    assert_provider_contract(&p).await;
}

// ---- Security groups ----

#[tokio::test]
async fn launch_creates_a_group_that_opens_one_udp_port_and_nothing_else() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(query_param("region", "us-east-1"))
        .and(body_string_contains("Action=CreateSecurityGroup"))
        .and(body_string_contains("GroupName=jamstream-sess1"))
        .and(body_string_contains(
            "GroupDescription=JamStream%20session%20sess1",
        ))
        .and(body_string_contains(
            "TagSpecification.1.ResourceType=security-group",
        ))
        .and(body_string_contains(
            "TagSpecification.1.Tag.1.Key=jamstream-session",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "<CreateSecurityGroupResponse><groupId>sg-0new</groupId>\
             </CreateSecurityGroupResponse>",
        ))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_string_contains("Action=AuthorizeSecurityGroupIngress"))
        .and(body_string_contains("GroupId=sg-0new"))
        .and(body_string_contains("IpPermissions.1.IpProtocol=udp"))
        .and(body_string_contains("IpPermissions.1.FromPort=45000"))
        .and(body_string_contains("IpPermissions.1.ToPort=45000"))
        .and(body_string_contains(
            "IpPermissions.1.IpRanges.1.CidrIp=0.0.0.0%2F0",
        ))
        .and(body_string_contains(
            "IpPermissions.1.Ipv6Ranges.1.CidrIpv6=%3A%3A%2F0",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "<AuthorizeSecurityGroupIngressResponse><return>true</return>\
             </AuthorizeSecurityGroupIngressResponse>",
        ))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_string_contains("Action=RunInstances"))
        .and(body_string_contains("SecurityGroupId.1=sg-0new"))
        .respond_with(ResponseTemplate::new(200).set_body_string(RUN_INSTANCES_XML))
        .expect(1)
        .mount(&server)
        .await;

    let p = provider(&server).with_session_port(45000);
    assert_eq!(p.session_port(), 45000);
    p.launch(launch_spec(&p, "us-east-1")).await.unwrap();
    server.verify().await;
}

#[tokio::test]
async fn launch_reuses_the_group_a_previous_attempt_created() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_string_contains("Action=CreateSecurityGroup"))
        .respond_with(ResponseTemplate::new(400).set_body_string(error_body(
            "InvalidGroup.Duplicate",
            "The security group 'jamstream-sess1' already exists",
        )))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_string_contains("Action=DescribeSecurityGroups"))
        .and(body_string_contains("Filter.1.Name=group-name"))
        .and(body_string_contains("Filter.1.Value.1=jamstream-sess1"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "<DescribeSecurityGroupsResponse><securityGroupInfo><item>\
             <groupId>sg-0existing</groupId><groupName>jamstream-sess1</groupName>\
             </item></securityGroupInfo></DescribeSecurityGroupsResponse>",
        ))
        .expect(1)
        .mount(&server)
        .await;
    // The rule is already there too, which EC2 reports as a duplicate.
    Mock::given(method("POST"))
        .and(body_string_contains("Action=AuthorizeSecurityGroupIngress"))
        .respond_with(ResponseTemplate::new(400).set_body_string(error_body(
            "InvalidPermission.Duplicate",
            "the specified rule already exists",
        )))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_string_contains("Action=RunInstances"))
        .and(body_string_contains("SecurityGroupId.1=sg-0existing"))
        .respond_with(ResponseTemplate::new(200).set_body_string(RUN_INSTANCES_XML))
        .expect(1)
        .mount(&server)
        .await;

    let p = provider(&server);
    p.launch(launch_spec(&p, "us-east-1"))
        .await
        .expect("a relaunch of the same session reuses its group");
    server.verify().await;
}

#[tokio::test]
async fn session_ingress_parses_ports_and_both_families() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_string_contains("Action=DescribeSecurityGroups"))
        .and(query_param("region", "us-east-1"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "<DescribeSecurityGroupsResponse><securityGroupInfo><item>\
             <groupId>sg-0abc</groupId><groupName>jamstream-sess1</groupName>\
             <ipPermissions><item>\
             <ipProtocol>udp</ipProtocol><fromPort>43210</fromPort><toPort>43210</toPort>\
             <ipRanges><item><cidrIp>0.0.0.0/0</cidrIp></item></ipRanges>\
             <ipv6Ranges><item><cidrIpv6>::/0</cidrIpv6></item></ipv6Ranges>\
             </item></ipPermissions>\
             <ipPermissionsEgress><item><ipProtocol>-1</ipProtocol>\
             <ipRanges><item><cidrIp>0.0.0.0/0</cidrIp></item></ipRanges>\
             </item></ipPermissionsEgress>\
             </item></securityGroupInfo></DescribeSecurityGroupsResponse>",
        ))
        .mount(&server)
        .await;
    // Every other region answers with nothing.
    Mock::given(method("POST"))
        .and(body_string_contains("Action=DescribeSecurityGroups"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "<DescribeSecurityGroupsResponse><securityGroupInfo/>\
             </DescribeSecurityGroupsResponse>",
        ))
        .mount(&server)
        .await;

    let p = provider(&server);
    let rules = p.session_ingress("sess1").await.expect("ingress");
    assert_eq!(rules.len(), 1, "the allow-all egress rule is not ingress");
    assert_eq!(rules[0].protocol, "udp");
    assert!(rules[0].is_only_port(43210));
    assert_eq!(
        rules[0].cidrs,
        vec!["0.0.0.0/0".to_owned(), "::/0".to_owned()]
    );
    assert!(rules[0].is_open_to_the_internet());
}

#[tokio::test]
async fn orphan_cleanup_leaves_a_group_that_is_still_attached() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_string_contains("Action=DescribeSecurityGroups"))
        .and(body_string_contains("Filter.1.Name=tag-key"))
        .and(body_string_contains("Filter.1.Value.1=jamstream-session"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "<DescribeSecurityGroupsResponse><securityGroupInfo>\
             <item><groupId>sg-0busy</groupId><groupName>jamstream-live</groupName></item>\
             <item><groupId>sg-0free</groupId><groupName>jamstream-gone</groupName></item>\
             </securityGroupInfo></DescribeSecurityGroupsResponse>",
        ))
        .mount(&server)
        .await;
    // A group whose instance has not finished terminating is not an orphan;
    // the next sweep collects it.
    Mock::given(method("POST"))
        .and(body_string_contains("Action=DeleteSecurityGroup"))
        .and(body_string_contains("GroupId=sg-0busy"))
        .respond_with(ResponseTemplate::new(400).set_body_string(error_body(
            "DependencyViolation",
            "resource sg-0busy has a dependent object",
        )))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_string_contains("Action=DeleteSecurityGroup"))
        .and(body_string_contains("GroupId=sg-0free"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "<DeleteSecurityGroupResponse><return>true</return></DeleteSecurityGroupResponse>",
        ))
        .mount(&server)
        .await;

    let p = provider(&server);
    let deleted = p.destroy_orphan_firewalls().await.expect("cleanup");
    // One per region in the catalog, since the same fake answers all of them.
    assert!(
        deleted.iter().all(|name| name == "jamstream-gone"),
        "only the unattached group is deleted, got {deleted:?}"
    );
    assert_eq!(deleted.len(), p.regions().len());
}

// ---- missing IAM permissions ----

/// Stand-in for the base64 blob AWS staples onto every UnauthorizedOperation.
/// The real one is around 700 characters and is only readable with
/// sts:DecodeAuthorizationMessage, which the documented jamstream policy
/// does not grant.
const ENCODED_BLOB: &str = "kV9Pz2Nz3wKQ7dJ8mYb1hV0sN5aQ2eR6tU8iO0pL4kJ3hG2fD1sA9zX8cV7bN6mM5lK4jH3gF2dS1aQ0wE9rT8yU7iO6pP5oI4uY3tR2eW1qZ0xC9vB8nM7mL6kJ5hG4fD3sA2zX1cV0bN9mM8lK7jH6gF5dS4aQ3wE2rT1yU0iO9pP8oI7uY6tR5eW4qZ3xC2vB1nM0mL9kJ8hG7fD6sA5zX4cV3bN2mM1lK0jH9gF8dS7aQ6wE5rT4yU3iO2pP1oI0uY9tR8eW7qZ6xC5vB4nM3mL2kJ1hG0fD9sA8zX7cV6bN5mM4lK3jH2gF1dS0aQ9wE8rT7yU6iO5pP4oI3uY2tR1eW0qZ9xC8vB7nM6mL5kJ4hG3fD2sA1zX0cV9bN8mM7lK6jH5gF4dS3aQ2wE1rT0yU9iO8pP7oI6uY5tR4eW3qZ2xC1vB0nM9mL8kJ7hG6fD5sA4zX3cV2bN1mM0lK9jH8gF7dS6aQ5wE4rT3yU2iO1pP0oI9uY8tR7eW6qZ5xC4vB3nM2mL1kJ0hG9fD8sA7zX6cV5bN4mM3lK2jH1gF0dS9aQ8wE7rT6yU5iO4pP3oI2uY1tR0eW9qZ8xC7vB6nM5mL4kJ3hG2fD1sA0zX";

fn unauthorized_body(action: &str) -> String {
    error_body(
        "UnauthorizedOperation",
        &format!(
            "You are not authorized to perform this operation. \
             User: arn:aws:iam::123456789012:user/jamstream is not authorized to perform: \
             {action} on resource: arn:aws:ec2:us-west-2:123456789012:security-group/* \
             because no identity-based policy allows the {action} action. \
             Encoded authorization failure message: {ENCODED_BLOB}"
        ),
    )
}

/// The defect behind issue 114's second half: a host whose policy predates
/// the per-session security group saw the whole 403 body, encoded blob and
/// all, and nothing that said which action to add.
#[tokio::test]
async fn a_missing_permission_names_the_action_and_the_setup_page() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(header("x-amz-target", "AmazonSSM.GetParameter"))
        .respond_with(ssm_parameter_response("ami-0123resolved456789"))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_string_contains("Action=CreateSecurityGroup"))
        .respond_with(
            ResponseTemplate::new(403)
                .set_body_string(unauthorized_body("ec2:CreateSecurityGroup")),
        )
        .mount(&server)
        .await;

    let p = provider(&server);
    let err = p.launch(launch_spec(&p, "us-west-2")).await.unwrap_err();
    assert!(matches!(err, ProviderError::Auth(_)), "got {err:?}");
    let rendered = err.to_string();
    assert!(
        rendered.contains("the IAM policy is missing ec2:CreateSecurityGroup"),
        "{rendered}"
    );
    assert!(
        rendered.contains("guides/providers/aws.html"),
        "the message must point at the page with the policy: {rendered}"
    );
    // The underlying failure survives; only the undecodable blob is dropped.
    assert!(
        rendered.contains("arn:aws:iam::123456789012:user/jamstream"),
        "{rendered}"
    );
    assert!(!rendered.contains(ENCODED_BLOB), "{rendered}");
    assert!(
        !rendered.contains("<Code>"),
        "the raw XML must not reach the wizard: {rendered}"
    );
    // The body this came from renders at 1133 characters, most of it blob.
    assert!(
        rendered.len() < 600,
        "the message is {} characters: {rendered}",
        rendered.len()
    );
}

/// Every EC2 action the provider sends can come back denied; whichever one
/// does, the message names it rather than making the host decode a blob.
#[tokio::test]
async fn a_denial_on_any_action_names_that_action() {
    for action in [
        "ec2:RunInstances",
        "ec2:DescribeInstances",
        "ec2:TerminateInstances",
        "ec2:CreateSecurityGroup",
        "ec2:AuthorizeSecurityGroupIngress",
        "ec2:DescribeSecurityGroups",
        "ec2:DeleteSecurityGroup",
        "ec2:CreateTags",
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(403).set_body_string(unauthorized_body(action)))
            .mount(&server)
            .await;
        let p = provider(&server);
        let err = p
            .destroy(&RegionId::new("us-west-2"), "i-123")
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains(&format!("the IAM policy is missing {action}")),
            "{action}: {err}"
        );
    }
}

/// A denial AWS phrases without an action name still says what it is and
/// where to look, and still carries what AWS said.
#[tokio::test]
async fn an_unattributed_denial_still_points_at_the_policy() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(403).set_body_string(error_body(
            "UnauthorizedOperation",
            "You are not authorized to perform this operation.",
        )))
        .mount(&server)
        .await;
    let p = provider(&server);
    let err = p
        .destroy(&RegionId::new("us-west-2"), "i-123")
        .await
        .unwrap_err();
    let rendered = err.to_string();
    assert!(
        rendered.contains("the IAM policy is missing a permission this call needs"),
        "{rendered}"
    );
    assert!(rendered.contains("guides/providers/aws.html"), "{rendered}");
    assert!(
        rendered.contains("You are not authorized to perform this operation."),
        "{rendered}"
    );
}

/// A 403 is fatal, so the remap must not cost the caller a retry: the
/// shared http layer never retries auth failures and the remap runs after
/// it, not instead of it.
#[tokio::test]
async fn a_denied_call_is_not_retried() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(403).set_body_string(unauthorized_body("ec2:TerminateInstances")),
        )
        .expect(1)
        .mount(&server)
        .await;
    let p = provider(&server);
    p.destroy(&RegionId::new("us-west-2"), "i-123")
        .await
        .unwrap_err();
}
