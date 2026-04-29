//! GossipSub topic catalogue.
//!
//! Every topic name carries an explicit `/1` version suffix so a future
//! schema change can be shipped on a new topic (e.g. `/2`) while the old
//! one drains. Node operators are not meant to subscribe to topics
//! directly — the role scheduler in [`arknet-node`] subscribes on their
//! behalf based on the active role bitmap.
//!
//! Topic name format: `arknet/<domain>/<name>/<version>`. Keep them
//! short; GossipSub hashes topic names but the raw string still
//! travels on every subscription.

use libp2p::gossipsub::IdentTopic;

/// Prefix for every arknet topic.
pub const TOPIC_PREFIX: &str = "arknet";

/// Version suffix for Phase 1 topics.
pub const TOPIC_VERSION: u32 = 1;

/// Construct an `IdentTopic` name under the arknet prefix.
pub fn topic(domain: &str, name: &str) -> IdentTopic {
    IdentTopic::new(format!("{TOPIC_PREFIX}/{domain}/{name}/{TOPIC_VERSION}"))
}

// ─── Catalogue ─────────────────────────────────────────────────────────────
//
// Keep this list in sync with PROTOCOL_SPEC §6. Adding a topic is a
// soft-fork (peers without a matching sub are harmless); removing or
// renaming one is a hard fork.

/// Pending-transaction mempool gossip.
pub fn tx_mempool() -> IdentTopic {
    topic("tx", "mempool")
}

/// Newly-proposed block headers + bodies (Phase 1 consensus).
pub fn block_prop() -> IdentTopic {
    topic("block", "prop")
}

/// Validator votes (prevote / precommit) outside the consensus inner loop.
pub fn consensus_vote() -> IdentTopic {
    topic("consensus", "vote")
}

/// Compute-pool offers (advertise free capacity).
pub fn pool_offer() -> IdentTopic {
    topic("pool", "offer")
}

/// Receipt attestations from verifiers.
pub fn receipt_attest() -> IdentTopic {
    topic("receipt", "attest")
}

/// Governance proposal / vote broadcast.
pub fn gov_prop() -> IdentTopic {
    topic("gov", "prop")
}

/// Free-tier quota tick — routers gossip consumption so every peer
/// converges on the same bucket counts within a heartbeat. Added in
/// Week 10 alongside the L2 router/compute roles.
pub fn quota_tick() -> IdentTopic {
    topic("quota", "tick")
}

/// Every topic the node may subscribe to — used by tests and
/// operator-facing CLI commands.
pub fn all_topics() -> Vec<IdentTopic> {
    vec![
        tx_mempool(),
        block_prop(),
        consensus_vote(),
        pool_offer(),
        receipt_attest(),
        gov_prop(),
        quota_tick(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topics_share_prefix_and_version() {
        for t in all_topics() {
            let s = t.to_string();
            assert!(s.starts_with("arknet/"), "topic missing prefix: {s}");
            assert!(s.ends_with("/1"), "topic missing /1 version: {s}");
        }
    }

    #[test]
    fn topics_are_unique() {
        let names: Vec<String> = all_topics().iter().map(|t| t.to_string()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(names.len(), sorted.len(), "duplicate topics: {names:?}");
    }
}
