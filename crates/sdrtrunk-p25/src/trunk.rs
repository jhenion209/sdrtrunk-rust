//! Minimal trunking tracker: turns parsed control-channel TSBKs into "start recording this call" events.
//! Learns the band plan, resolves channels to Hz, and suppresses repeat grants for a call already active.
use crate::tsbk_msg::{is_emergency, is_encrypted, BandPlans, Channel, Tsbk};
use std::collections::HashMap;

/// A call to follow. `source` is 0 when the message did not carry it (grant updates).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallGrant { pub group: u16, pub source: u32, pub channel: Channel, pub hz: u64, pub encrypted: bool, pub emergency: bool }

pub struct TrunkTracker {
    plans: BandPlans,
    /// group -> (frequency, last time a grant for it was seen)
    active: HashMap<u16, (u64, i64)>,
    /// A call is new again after this long without a grant (ms).
    pub hold_ms: i64,
}

impl Default for TrunkTracker {
    fn default() -> Self { Self { plans: BandPlans::default(), active: HashMap::new(), hold_ms: 5_000 } }
}

impl TrunkTracker {
    /// Feed one valid TSBK. Returns a grant when it starts a new call, or moves a known group to another channel.
    /// Grants whose band plan is not known yet are dropped (the plan repeats every few seconds).
    pub fn handle(&mut self, now_ms: i64, msg: &Tsbk) -> Vec<CallGrant> {
        let mut raw: Vec<(u16, Channel, u32, u8)> = Vec::new();
        match msg {
            Tsbk::IdentifierUpdate(p) => self.plans.learn(*p),
            Tsbk::GroupVoiceGrant { service_options, channel, group, source } => raw.push((*group, *channel, *source, *service_options)),
            Tsbk::GroupVoiceGrantUpdate { grants } => raw.extend(grants.iter().map(|(c, g)| (*g, *c, 0, 0))),
            Tsbk::GroupVoiceGrantUpdateExplicit { service_options, downlink, group, .. } => raw.push((*group, *downlink, 0, *service_options)),
            _ => {}
        }
        let mut out = Vec::new();
        for (group, channel, source, opts) in raw {
            let Some(hz) = self.plans.downlink_hz(channel) else { continue };
            let seen = self.active.insert(group, (hz, now_ms));
            let fresh = seen.map_or(true, |(f, t)| f != hz || now_ms - t > self.hold_ms);
            if fresh { out.push(CallGrant { group, source, channel, hz, encrypted: is_encrypted(opts), emergency: is_emergency(opts) }); }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tsbk_msg::BandPlan;

    #[test]
    fn dedups_repeats_and_waits_for_band_plan() {
        let mut t = TrunkTracker::default();
        let grant = Tsbk::GroupVoiceGrant { service_options: 0x40, channel: Channel { band: 1, number: 8 }, group: 85, source: 7 };
        assert!(t.handle(0, &grant).is_empty(), "no band plan yet");
        t.handle(10, &Tsbk::IdentifierUpdate(BandPlan { id: 1, base: 851_000_000, spacing: 12_500, tx_offset: 0, slots: 1 }));
        let g = t.handle(20, &grant);
        assert_eq!(g, vec![CallGrant { group: 85, source: 7, channel: Channel { band: 1, number: 8 }, hz: 851_100_000, encrypted: true, emergency: false }]);
        assert!(t.handle(500, &grant).is_empty(), "repeat inside hold window");
        assert_eq!(t.handle(10_000, &grant).len(), 1, "new call after silence");
    }
}
