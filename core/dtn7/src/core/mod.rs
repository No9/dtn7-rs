pub mod application_agent;
pub mod bundlepack;
pub mod helpers;
pub mod peer;
pub mod processing;
pub mod store;

pub use crate::core::peer::{DtnPeer, PeerType};
use crate::core::store::BundleStore;
use crate::routing::RoutingAgent;
use crate::routing::RoutingAgentsEnum;
use crate::{store_get_bundle, store_get_metadata};
pub use crate::{store_has_item, store_push_bundle};
use crate::{PEERS, STORE};
use application_agent::ApplicationAgent;
use bp7::EndpointID;
use log::debug;
use log::trace;
use log::{error, info};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Instant;

use crate::core::application_agent::ApplicationAgentEnum;

use self::bundlepack::BundlePack;
use self::processing::forward;

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct DtnStatistics {
    pub incoming: u64,
    pub dups: u64,
    pub outgoing: u64,
    pub delivered: u64,
    pub broken: u64,
}

impl DtnStatistics {
    pub fn new() -> DtnStatistics {
        DtnStatistics {
            incoming: 0,
            dups: 0,
            outgoing: 0,
            delivered: 0,
            broken: 0,
        }
    }
}
#[derive(Debug)]
pub struct DtnCore {
    pub endpoints: Vec<ApplicationAgentEnum>,
    pub service_list: HashMap<u8, String>,
    pub routing_agent: RoutingAgentsEnum,
}

impl Default for DtnCore {
    fn default() -> Self {
        Self::new()
    }
}

impl DtnCore {
    pub fn new() -> DtnCore {
        DtnCore {
            endpoints: Vec::new(),
            service_list: HashMap::new(),
            //routing_agent: crate::routing::flooding::FloodingRoutingAgent::new().into(),
            routing_agent: crate::routing::epidemic::EpidemicRoutingAgent::new().into(),
        }
    }

    pub fn register_application_agent(&mut self, aa: ApplicationAgentEnum) {
        if self.is_in_endpoints(aa.eid()) {
            info!("Application agent already registered for EID: {}", aa.eid());
        } else {
            info!("Registered new application agent for EID: {}", aa.eid());
            self.endpoints.push(aa);
        }
    }
    pub fn unregister_application_agent(&mut self, aa: ApplicationAgentEnum) {
        info!("Unregistered application agent for EID: {}", aa.eid());
        self.endpoints
            .iter()
            .position(|n| n.eid() == aa.eid())
            .map(|e| self.endpoints.remove(e));
    }
    pub fn eids(&self) -> Vec<String> {
        self.endpoints.iter().map(|e| e.eid().to_string()).collect()
    }
    pub fn bundle_ids(&self) -> Vec<String> {
        (*STORE.lock()).all_ids()
    }
    pub fn bundle_count(&self) -> usize {
        (*STORE.lock()).count() as usize
    }
    pub fn bundle_names(&self) -> Vec<String> {
        (*STORE.lock()).all_ids()
    }
    pub fn is_in_endpoints(&self, eid: &EndpointID) -> bool {
        for aa in self.endpoints.iter() {
            if eid == aa.eid() {
                return true;
            }
        }
        false
    }
    pub fn get_endpoint_mut(&mut self, eid: &EndpointID) -> Option<&mut ApplicationAgentEnum> {
        for aa in self.endpoints.iter_mut() {
            if eid == aa.eid() {
                return Some(aa);
            }
        }
        None
    }
    pub fn get_endpoint(&self, eid: &EndpointID) -> Option<&ApplicationAgentEnum> {
        for aa in self.endpoints.iter() {
            if eid == aa.eid() {
                return Some(aa);
            }
        }
        None
    }
}

/// Removes peers from global peer list that haven't been seen in a while.
pub fn process_peers() {
    (*PEERS.lock()).retain(|_k, v| {
        let val = v.still_valid();
        if !val {
            info!(
                "Have not seen {} @ {} in a while, removing it from list of known peers",
                v.eid, v.addr
            );
        }
        v.con_type == PeerType::Static || val
    });
}

/// Reprocess bundles in store
pub async fn process_bundles() {
    let now_total = Instant::now();

    let forwarding_bids: Vec<String> = (*STORE.lock()).forwarding();

    // avoid transmission attempts if peers are the same as last time and bundles are also the same.
    // only process bundles if peers have changed or if we have already skipped retransmission a few times.
    //
    // only check if number of peers and number of bundles have changed.
    // the chances that the exact same number of peers and bundles are removed and new ones added is very low.
    // proper hashing would be much more expensive.

    const MAX_SKIPS_BEFORE_RETRANSMISSION: usize = 5;

    static LAST_NUM_PEERS: AtomicUsize = AtomicUsize::new(0);
    static LAST_NUM_BUNDLES: AtomicUsize = AtomicUsize::new(0);
    static RETRY_COUNTDOWN: AtomicUsize = AtomicUsize::new(MAX_SKIPS_BEFORE_RETRANSMISSION);

    let num_peers: usize = (*PEERS.lock()).len();
    let num_bundles = forwarding_bids.len();

    if num_peers == LAST_NUM_PEERS.load(Ordering::Relaxed)
        && num_bundles == LAST_NUM_BUNDLES.load(Ordering::Relaxed)
    {
        if RETRY_COUNTDOWN.fetch_sub(1, std::sync::atomic::Ordering::Relaxed) <= 1 {
            RETRY_COUNTDOWN.store(MAX_SKIPS_BEFORE_RETRANSMISSION, Ordering::Relaxed);
            debug!("Retrying bundle processing, even though neighbors haven't changed");
        } else {
            debug!("Neighbors and bundles haven't changed, skipping bundle processing");
            LAST_NUM_PEERS.store(num_peers, Ordering::Relaxed);
            LAST_NUM_BUNDLES.store(num_bundles, Ordering::Relaxed);
            return;
        }
    }
    LAST_NUM_PEERS.store(num_peers, Ordering::Relaxed);
    LAST_NUM_BUNDLES.store(num_bundles, Ordering::Relaxed);

    // start actual bundle processing

    let mut forwarding_bundles: Vec<BundlePack> = forwarding_bids
        .iter()
        .filter_map(|bid| store_get_metadata(bid))
        .collect();
    forwarding_bundles.sort_unstable_by(|a, b| a.creation_time.cmp(&b.creation_time));

    let num_bundles = forwarding_bundles.len();
    for bp in forwarding_bundles {
        let bpid = bp.id().to_string();
        let now = Instant::now();
        if let Err(err) = forward(bp).await {
            error!("Error forwarding bundle: {}", err);
        }
        trace!("Forwarding time: {:?} for {}", now.elapsed(), bpid);
    }
    debug!(
        "time to process {} bundles: {:?}",
        num_bundles,
        now_total.elapsed()
    );
    //forwarding_bundle_ids.iter().for_each(|bpid| {});
}
