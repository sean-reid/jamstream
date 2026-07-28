use std::collections::HashMap;
use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Mutex;

use async_trait::async_trait;

use crate::artifact::ServerArch;
use crate::provider::{Provider, ProviderError, Result};
use crate::types::{
    ANY_IPV4, ANY_IPV6, DEFAULT_SESSION_PORT, IngressRule, Instance, LaunchSpec, Price,
    ProviderKind, Region, RegionId, session_id_from_tags,
};

/// Every call made against the mock, in order, for test assertions.
#[derive(Debug, Clone, PartialEq)]
pub enum Call {
    Regions,
    Price(RegionId),
    Launch {
        region: RegionId,
        session_id: Option<String>,
    },
    Destroy {
        region: RegionId,
        id: String,
    },
    ListTagged(Option<String>),
    /// A per-session firewall created by `launch`.
    CreateFirewall {
        session_id: String,
        port: u16,
    },
    DestroyOrphanFirewalls,
}

#[derive(Default)]
struct State {
    instances: Vec<Instance>,
    /// Session id to the ingress the session's firewall holds, mirroring
    /// what the real providers create at launch.
    firewalls: HashMap<String, Vec<IngressRule>>,
    next_id: u64,
    next_ip: u32,
    price_failures: VecDeque<ProviderError>,
    launch_failures: VecDeque<ProviderError>,
    destroy_failures: VecDeque<ProviderError>,
    list_failures: VecDeque<ProviderError>,
    calls: Vec<Call>,
}

/// In-memory Provider for server, CLI, and E2E tests.
pub struct MockProvider {
    kind: ProviderKind,
    regions: Vec<Region>,
    prices: HashMap<RegionId, Price>,
    session_port: u16,
    state: Mutex<State>,
}

impl MockProvider {
    pub fn new(kind: ProviderKind) -> Self {
        MockProvider {
            kind,
            regions: Vec::new(),
            prices: HashMap::new(),
            session_port: DEFAULT_SESSION_PORT,
            state: Mutex::new(State::default()),
        }
    }

    /// Session UDP port the mock's firewalls open, like the real providers.
    pub fn with_session_port(mut self, port: u16) -> Self {
        self.session_port = port;
        self
    }

    /// A mock with two regions at distinct prices, enough for most tests.
    pub fn with_default_regions(kind: ProviderKind) -> Self {
        let mut p = Self::new(kind);
        p = p.with_region(
            Region {
                provider: kind,
                id: RegionId::new("mock-east"),
                display: "Mock East".to_owned(),
                country: "US".to_owned(),
            },
            Price {
                hourly_microusd: 16_800,
                egress_microusd_per_gb: 90_000,
                included_egress_gb: 100,
            },
        );
        p.with_region(
            Region {
                provider: kind,
                id: RegionId::new("mock-west"),
                display: "Mock West".to_owned(),
                country: "US".to_owned(),
            },
            Price {
                hourly_microusd: 24_000,
                egress_microusd_per_gb: 10_000,
                included_egress_gb: 1000,
            },
        )
    }

    pub fn with_region(mut self, region: Region, price: Price) -> Self {
        self.prices.insert(region.id.clone(), price);
        self.regions.push(region);
        self
    }

    /// A region the provider offers but has no price for, which is how the
    /// real ones report "this size is not sold here": DigitalOcean's atl1
    /// does not carry `s-2vcpu-2gb`, and `price` there is a NotFound.
    pub fn with_unpriced_region(mut self, region: Region) -> Self {
        self.regions.push(region);
        self
    }

    /// The next `n` price calls fail with a clone of `err`, whatever the
    /// region. For telling a region that cannot run our size apart from a
    /// credential or network failure, which must not be mistaken for one.
    pub fn fail_next_prices(&self, n: usize, err: ProviderError) {
        let mut s = self.state.lock().unwrap();
        for _ in 0..n {
            s.price_failures.push_back(err.clone());
        }
    }

    /// The next `n` launch calls fail with a clone of `err`.
    pub fn fail_next_launches(&self, n: usize, err: ProviderError) {
        let mut s = self.state.lock().unwrap();
        for _ in 0..n {
            s.launch_failures.push_back(err.clone());
        }
    }

    /// The next `n` destroy calls fail with a clone of `err`.
    pub fn fail_next_destroys(&self, n: usize, err: ProviderError) {
        let mut s = self.state.lock().unwrap();
        for _ in 0..n {
            s.destroy_failures.push_back(err.clone());
        }
    }

    /// The next `n` list calls fail with a clone of `err`. A provider that
    /// cannot be listed is one the sweeper never searched, which is a
    /// different outcome from finding nothing.
    pub fn fail_next_lists(&self, n: usize, err: ProviderError) {
        let mut s = self.state.lock().unwrap();
        for _ in 0..n {
            s.list_failures.push_back(err.clone());
        }
    }

    /// Seeds an already-running instance, e.g. a leaked orphan for sweeper
    /// tests. Returns the instance as stored.
    pub fn seed_instance(&self, region: &Region, tags: Vec<(String, String)>) -> Instance {
        let mut s = self.state.lock().unwrap();
        let inst = Self::build_instance(self.kind, region, tags, &mut s);
        s.instances.push(inst.clone());
        inst
    }

    pub fn calls(&self) -> Vec<Call> {
        self.state.lock().unwrap().calls.clone()
    }

    pub fn running_instances(&self) -> Vec<Instance> {
        self.state.lock().unwrap().instances.clone()
    }

    fn region(&self, id: &RegionId) -> Result<&Region> {
        self.regions
            .iter()
            .find(|r| &r.id == id)
            .ok_or_else(|| ProviderError::NotFound(format!("region {id}")))
    }

    fn build_instance(
        kind: ProviderKind,
        region: &Region,
        tags: Vec<(String, String)>,
        s: &mut State,
    ) -> Instance {
        s.next_id += 1;
        s.next_ip += 1;
        let ip = s.next_ip;
        Instance {
            provider: kind,
            region: region.clone(),
            id: format!("mock-{:06}", s.next_id),
            public_ip: Some(IpAddr::V4(Ipv4Addr::new(
                10,
                0,
                (ip >> 8) as u8,
                (ip & 0xff) as u8,
            ))),
            tags,
        }
    }
}

#[async_trait]
impl Provider for MockProvider {
    fn kind(&self) -> ProviderKind {
        self.kind
    }

    /// Mirrors the real provider of this kind, so artifact selection under
    /// test picks what a real launch would.
    fn server_arch(&self) -> ServerArch {
        match self.kind {
            ProviderKind::Aws => ServerArch::Aarch64,
            _ => ServerArch::X86_64,
        }
    }

    fn regions(&self) -> Vec<Region> {
        self.state.lock().unwrap().calls.push(Call::Regions);
        self.regions.clone()
    }

    async fn price(&self, region: &RegionId) -> Result<Price> {
        {
            let mut s = self.state.lock().unwrap();
            s.calls.push(Call::Price(region.clone()));
            if let Some(err) = s.price_failures.pop_front() {
                return Err(err);
            }
        }
        self.region(region)?;
        self.prices
            .get(region)
            .copied()
            .ok_or_else(|| ProviderError::NotFound(format!("price for region {region}")))
    }

    async fn launch(&self, spec: LaunchSpec) -> Result<Instance> {
        let mut s = self.state.lock().unwrap();
        s.calls.push(Call::Launch {
            region: spec.region.id.clone(),
            session_id: spec.session_id().map(str::to_owned),
        });
        if let Some(err) = s.launch_failures.pop_front() {
            return Err(err);
        }
        drop(s);
        let region = self.region(&spec.region.id)?.clone();
        let mut s = self.state.lock().unwrap();
        // The firewall goes in before the instance, as it does on every real
        // provider: an instance that exists before its ingress rule does is
        // an instance sitting on provider defaults.
        if let Some(session) = spec.session_id() {
            s.calls.push(Call::CreateFirewall {
                session_id: session.to_owned(),
                port: self.session_port,
            });
            s.firewalls.insert(
                session.to_owned(),
                vec![IngressRule::session_udp(
                    self.session_port,
                    vec![ANY_IPV4.to_owned(), ANY_IPV6.to_owned()],
                )],
            );
        }
        let inst = Self::build_instance(self.kind, &region, spec.tags, &mut s);
        s.instances.push(inst.clone());
        Ok(inst)
    }

    async fn destroy(&self, region: &RegionId, id: &str) -> Result<()> {
        let mut s = self.state.lock().unwrap();
        s.calls.push(Call::Destroy {
            region: region.clone(),
            id: id.to_owned(),
        });
        if let Some(err) = s.destroy_failures.pop_front() {
            return Err(err);
        }
        let before = s.instances.len();
        s.instances
            .retain(|i| !(i.id == id && &i.region.id == region));
        if s.instances.len() == before {
            return Err(ProviderError::NotFound(format!(
                "instance {id} in region {region}"
            )));
        }
        Ok(())
    }

    async fn list_tagged(&self, session_tag: Option<&str>) -> Result<Vec<Instance>> {
        let mut s = self.state.lock().unwrap();
        s.calls
            .push(Call::ListTagged(session_tag.map(str::to_owned)));
        if let Some(err) = s.list_failures.pop_front() {
            return Err(err);
        }
        Ok(s.instances
            .iter()
            .filter(|i| match (session_id_from_tags(&i.tags), session_tag) {
                (Some(sid), Some(want)) => sid == want,
                (Some(_), None) => true,
                (None, _) => false,
            })
            .cloned()
            .collect())
    }

    fn session_port(&self) -> u16 {
        self.session_port
    }

    async fn session_ingress(&self, session: &str) -> Result<Vec<IngressRule>> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .firewalls
            .get(session)
            .cloned()
            .unwrap_or_default())
    }

    async fn destroy_orphan_firewalls(&self) -> Result<Vec<String>> {
        let mut s = self.state.lock().unwrap();
        s.calls.push(Call::DestroyOrphanFirewalls);
        let live: Vec<String> = s
            .instances
            .iter()
            .filter_map(|i| i.session_id().map(str::to_owned))
            .collect();
        let orphans: Vec<String> = s
            .firewalls
            .keys()
            .filter(|session| !live.contains(session))
            .cloned()
            .collect();
        for session in &orphans {
            s.firewalls.remove(session);
        }
        Ok(orphans)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{InstanceClass, session_tag};

    fn spec(p: &MockProvider, region: &str, session: &str) -> LaunchSpec {
        let region = p
            .regions
            .iter()
            .find(|r| r.id.as_str() == region)
            .unwrap()
            .clone();
        LaunchSpec {
            region,
            instance_class: InstanceClass::Small,
            user_data: String::new(),
            tags: vec![session_tag(session)],
        }
    }

    #[tokio::test]
    async fn launch_list_destroy_cycle() {
        let p = MockProvider::with_default_regions(ProviderKind::DigitalOcean);
        let inst = p.launch(spec(&p, "mock-east", "s1")).await.unwrap();
        assert!(inst.public_ip.is_some());
        assert_eq!(inst.session_id(), Some("s1"));

        let listed = p.list_tagged(Some("s1")).await.unwrap();
        assert_eq!(listed, vec![inst.clone()]);

        p.destroy(&inst.region.id, &inst.id).await.unwrap();
        assert!(p.list_tagged(None).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn scripted_launch_failures() {
        let p = MockProvider::with_default_regions(ProviderKind::Aws);
        p.fail_next_launches(2, ProviderError::QuotaExceeded("vcpu".into()));
        let s = spec(&p, "mock-east", "s1");
        assert!(matches!(
            p.launch(s.clone()).await,
            Err(ProviderError::QuotaExceeded(_))
        ));
        assert!(matches!(
            p.launch(s.clone()).await,
            Err(ProviderError::QuotaExceeded(_))
        ));
        assert!(p.launch(s).await.is_ok());
    }

    #[tokio::test]
    async fn untagged_instances_are_invisible_to_list() {
        let p = MockProvider::with_default_regions(ProviderKind::Gcp);
        let region = p.regions[0].clone();
        p.seed_instance(&region, vec![("unrelated".into(), "tag".into())]);
        assert!(p.list_tagged(None).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn records_calls_in_order() {
        let p = MockProvider::with_default_regions(ProviderKind::Aws);
        let _ = p.price(&RegionId::new("mock-east")).await;
        let _ = p.list_tagged(None).await;
        assert_eq!(
            p.calls(),
            vec![
                Call::Price(RegionId::new("mock-east")),
                Call::ListTagged(None),
            ]
        );
    }
}
