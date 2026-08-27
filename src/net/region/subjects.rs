//! The subjects the region-to-edge protocol uses, and how to read one back.
//!
//! Four shapes, all rooted at `umwelt`:
//!
//! | subject | direction |
//! |---|---|
//! | `umwelt.{region}.info` | request and reply |
//! | `umwelt.{region}.edge.{edge}.payload` | region to edge |
//! | `umwelt.{region}.edge.{edge}.reply` | region to edge |
//! | `umwelt.{region}.edge.{edge}.command` | edge to region |
//!
//! An edge subscribes to `umwelt.*.edge.{edge}.payload` once and never again:
//! payloads from a region it has never heard of match a subscription it already
//! holds. That is why migrating an entity between regions costs the edge
//! nothing. See `docs/adr/0001`.

use crate::net::error::NetError;
use crate::net::region::edges::EdgeName;
use crate::net::region::protocol::RegionId;

pub fn info(region: RegionId) -> String {
    format!("umwelt.{}.info", region.raw())
}

pub fn payload(region: RegionId, edge: &EdgeName) -> String {
    format!("umwelt.{}.edge.{edge}.payload", region.raw())
}

pub fn reply(region: RegionId, edge: &EdgeName) -> String {
    format!("umwelt.{}.edge.{edge}.reply", region.raw())
}

pub fn command(region: RegionId, edge: &EdgeName) -> String {
    format!("umwelt.{}.edge.{edge}.command", region.raw())
}

/// Every command sent to one region, whichever edge sent it.
pub fn commands_to(region: RegionId) -> String {
    format!("umwelt.{}.edge.*.command", region.raw())
}

/// Everything addressed to one edge with the given leaf, from any region.
pub fn to_edge(edge: &EdgeName, leaf: &str) -> String {
    format!("umwelt.*.edge.{edge}.{leaf}")
}

/// The edge that sent a command, read out of the subject it arrived on.
pub fn sender(subject: &str) -> Result<EdgeName, NetError> {
    match subject.split('.').collect::<Vec<_>>()[..] {
        ["umwelt", _, "edge", edge, "command"] => EdgeName::new(edge),
        _ => Err(NetError::BadSubject),
    }
}

/// The region a payload or reply came from, read out of its subject.
pub fn origin(subject: &str) -> Result<RegionId, NetError> {
    match subject.split('.').collect::<Vec<_>>()[..] {
        ["umwelt", region, "edge", _, "payload" | "reply"] => {
            region.parse().map(RegionId::from_raw).map_err(|_| NetError::BadSubject)
        }
        _ => Err(NetError::BadSubject),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edge() -> EdgeName {
        EdgeName::new("edge-3").expect("valid")
    }

    #[test]
    fn subjects_have_the_shape_the_wildcards_expect() {
        let r = RegionId::from_raw(7);
        assert_eq!(info(r), "umwelt.7.info");
        assert_eq!(payload(r, &edge()), "umwelt.7.edge.edge-3.payload");
        assert_eq!(command(r, &edge()), "umwelt.7.edge.edge-3.command");
        assert_eq!(commands_to(r), "umwelt.7.edge.*.command");
        assert_eq!(to_edge(&edge(), "payload"), "umwelt.*.edge.edge-3.payload");
    }

    #[test]
    fn an_edges_subscription_matches_every_region() {
        // The property the whole migration story rests on: one subscription,
        // taken at startup, matches regions that did not exist then.
        let pattern = to_edge(&edge(), "payload");
        for region in [1u32, 7, 4_000_000] {
            let concrete = payload(RegionId::from_raw(region), &edge());
            let (p, c): (Vec<_>, Vec<_>) =
                (pattern.split('.').collect(), concrete.split('.').collect());
            assert_eq!(p.len(), c.len());
            assert!(p.iter().zip(&c).all(|(a, b)| *a == "*" || a == b), "{concrete}");
        }
    }

    #[test]
    fn a_command_subject_names_its_sender() {
        assert_eq!(sender("umwelt.7.edge.edge-3.command").expect("parses").as_str(), "edge-3");
        assert!(sender("umwelt.7.edge.edge-3.payload").is_err());
        assert!(sender("umwelt.7.info").is_err());
        assert!(sender("nonsense").is_err());
    }

    #[test]
    fn a_payload_subject_names_its_region() {
        assert_eq!(origin("umwelt.7.edge.e.payload").expect("parses"), RegionId::from_raw(7));
        assert_eq!(origin("umwelt.12.edge.e.reply").expect("parses"), RegionId::from_raw(12));
        assert!(origin("umwelt.seven.edge.e.payload").is_err());
        assert!(origin("umwelt.7.edge.e.command").is_err());
    }
}
