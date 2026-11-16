//! Outbound (control channel -> radios) TSBK message parsing for the standard vendor: the messages a recorder needs
//! to follow calls. Field layouts are from sdrtrunk `tsbk.standard.osp.*`. Everything else is `Other`.
use std::collections::HashMap;
use sdrtrunk_edac::bits::get_uint;

/// Channel reference: band-plan identifier plus channel number.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Channel { pub band: u8, pub number: u16 }

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Tsbk {
    /// Opcode 0x00.
    GroupVoiceGrant { service_options: u8, channel: Channel, group: u16, source: u32 },
    /// Opcode 0x02: one or two grants.
    GroupVoiceGrantUpdate { grants: Vec<(Channel, u16)> },
    /// Opcode 0x03.
    GroupVoiceGrantUpdateExplicit { service_options: u8, downlink: Channel, uplink: Channel, group: u16 },
    /// Opcode 0x3D (FDMA) and 0x33 (TDMA, `slots` > 1 possible).
    IdentifierUpdate(BandPlan),
    /// Opcode 0x3B.
    NetworkStatus { area: u8, wacn: u32, system: u16, channel: Channel, service_class: u8 },
    Other { opcode: u8 },
}

/// One frequency band entry (Hz).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BandPlan { pub id: u8, pub base: u64, pub spacing: u64, pub tx_offset: i64, pub slots: u8 }

impl BandPlan {
    /// Downlink frequency of a channel number; TDMA channels share a carrier across their slots.
    pub fn downlink(&self, channel: u16) -> u64 { self.base + self.spacing * (channel as u64 / self.slots as u64) }
}

/// Emergency flag of voice service options.
pub fn is_emergency(service_options: u8) -> bool { service_options & 0x80 != 0 }
/// Encryption ("protected") flag of voice service options.
pub fn is_encrypted(service_options: u8) -> bool { service_options & 0x40 != 0 }

fn chan(bits: &[bool], at: usize) -> Channel {
    Channel { band: get_uint(bits, at, at + 3) as u8, number: get_uint(bits, at + 4, at + 15) as u16 }
}

/// Parses a CRC-valid 96-bit TSBK. Non-standard vendors come back as `Other`.
pub fn parse(bits: &[bool]) -> Tsbk {
    let opcode = get_uint(bits, 2, 7) as u8;
    let vendor = get_uint(bits, 8, 15);
    if bits.len() < 80 || vendor > 1 { return Tsbk::Other { opcode }; }
    match opcode {
        0x00 => Tsbk::GroupVoiceGrant {
            service_options: get_uint(bits, 16, 23) as u8,
            channel: chan(bits, 24),
            group: get_uint(bits, 40, 55) as u16,
            source: get_uint(bits, 56, 79) as u32,
        },
        0x02 => {
            let mut grants = vec![(chan(bits, 16), get_uint(bits, 32, 47) as u16)];
            let b = get_uint(bits, 64, 79) as u16;
            if b != 0 && b != grants[0].1 { grants.push((chan(bits, 48), b)); }
            Tsbk::GroupVoiceGrantUpdate { grants }
        }
        0x03 => Tsbk::GroupVoiceGrantUpdateExplicit {
            service_options: get_uint(bits, 16, 23) as u8,
            downlink: chan(bits, 32),
            uplink: chan(bits, 48),
            group: get_uint(bits, 64, 79) as u16,
        },
        0x3D => {
            let sign = bits[29];
            let raw = get_uint(bits, 30, 37) as i64;
            let tx_offset = if raw == 0x80 { 0 } else if sign { raw * 250_000 } else { -raw * 250_000 };
            Tsbk::IdentifierUpdate(BandPlan {
                id: get_uint(bits, 16, 19) as u8,
                base: get_uint(bits, 48, 79) * 5,
                spacing: get_uint(bits, 38, 47) * 125,
                tx_offset,
                slots: 1,
            })
        }
        0x33 => {
            let spacing = get_uint(bits, 38, 47) * 125;
            let slots = match get_uint(bits, 20, 23) { 3 | 5 => 2, 4 => 4, _ => 1 };
            let raw = get_uint(bits, 25, 37) as i64;
            let tx_offset = if raw == 0x80 { 0 } else if bits[24] { raw * spacing as i64 } else { -raw * spacing as i64 };
            Tsbk::IdentifierUpdate(BandPlan { id: get_uint(bits, 16, 19) as u8, base: get_uint(bits, 48, 79) * 5, spacing, tx_offset, slots })
        }
        0x3B => Tsbk::NetworkStatus {
            area: get_uint(bits, 16, 23) as u8,
            wacn: get_uint(bits, 24, 43) as u32,
            system: get_uint(bits, 44, 55) as u16,
            channel: chan(bits, 56),
            service_class: get_uint(bits, 72, 79) as u8,
        },
        _ => Tsbk::Other { opcode },
    }
}

/// Accumulates identifier updates so channel references resolve to frequencies.
#[derive(Default)]
pub struct BandPlans(HashMap<u8, BandPlan>);

impl BandPlans {
    pub fn learn(&mut self, plan: BandPlan) { self.0.insert(plan.id, plan); }
    pub fn downlink_hz(&self, c: Channel) -> Option<u64> { self.0.get(&c.band).map(|p| p.downlink(c.number)) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sdrtrunk_edac::bits::set_uint;

    fn block(opcode: u64) -> Vec<bool> {
        let mut b = vec![false; 96];
        set_uint(&mut b, 2, 6, opcode);
        b
    }

    #[test]
    fn voice_grant_and_band_plan_resolve_frequency() {
        let mut g = block(0x00);
        set_uint(&mut g, 16, 8, 0x40);
        set_uint(&mut g, 24, 4, 3);
        set_uint(&mut g, 28, 12, 0x0A0);
        set_uint(&mut g, 40, 16, 1234);
        set_uint(&mut g, 56, 24, 987_654);
        let mut i = block(0x3D);
        set_uint(&mut i, 16, 4, 3);
        set_uint(&mut i, 38, 10, 100); // 12.5 kHz
        set_uint(&mut i, 48, 32, 851_000_000 / 5);
        let mut plans = BandPlans::default();
        let Tsbk::IdentifierUpdate(p) = parse(&i) else { panic!() };
        plans.learn(p);
        let Tsbk::GroupVoiceGrant { service_options, channel, group, source } = parse(&g) else { panic!() };
        assert_eq!((group, source), (1234, 987_654));
        assert!(is_encrypted(service_options) && !is_emergency(service_options));
        assert_eq!(plans.downlink_hz(channel), Some(851_000_000 + 160 * 12_500));
    }

    #[test]
    fn grant_update_drops_duplicate_b() {
        let mut u = block(0x02);
        set_uint(&mut u, 32, 16, 77);
        set_uint(&mut u, 64, 16, 77);
        let Tsbk::GroupVoiceGrantUpdate { grants } = parse(&u) else { panic!() };
        assert_eq!(grants.len(), 1);
    }
}
