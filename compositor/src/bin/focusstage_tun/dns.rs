use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::{Duration, Instant};

pub const DNS_RECORD_TTL_SECS: u32 = 60;
pub const DNS_MAPPING_TTL: Duration = Duration::from_secs(300);

/// IPv6 Fake-IP pool (`fd98:18::/48`) sharing slot IDs with `198.18.0.0/15`.
pub const FAKE_IPV6_NET: Ipv6Addr = Ipv6Addr::new(0xfd98, 0x0018, 0, 0, 0, 0, 0, 0);
pub const FAKE_IPV6_PREFIX: u8 = 48;

// Slot IDs 2..=130_558 map to 198.18.0.2..=198.19.253.254, keeping 198.19.254.0/30
// (TUN link 198.19.254.1 / 198.19.254.2) completely disjoint.
const FIRST_SLOT_ID: u32 = 2;
const LAST_SLOT_ID: u32 = 130_558;
const IPV4_POOL_BASE: u32 = 0xC612_0000; // 198.18.0.0

const QTYPE_A: u16 = 1;
const QTYPE_AAAA: u16 = 28;
const QCLASS_IN: u16 = 1;

#[derive(Debug, Clone)]
struct DnsEntry {
    name: String,
    expires_at: Instant,
}

#[derive(Debug)]
pub struct VirtualDns {
    ipv6_enabled: bool,
    next_id: u32,
    name_to_id: HashMap<String, u32>,
    id_to_entry: HashMap<u32, DnsEntry>,
}

impl VirtualDns {
    pub fn new(ipv6_enabled: bool) -> Self {
        Self {
            ipv6_enabled,
            next_id: FIRST_SLOT_ID,
            name_to_id: HashMap::new(),
            id_to_entry: HashMap::new(),
        }
    }

    pub fn allocate_for_name(&mut self, raw_name: &str, now: Instant) -> Option<(Ipv4Addr, Ipv6Addr)> {
        let name = canonicalize_domain(raw_name);
        if name.is_empty() {
            return None;
        }

        if let Some(&id) = self.name_to_id.get(&name) {
            if let Some(entry) = self.id_to_entry.get_mut(&id) {
                entry.expires_at = now + DNS_MAPPING_TTL;
                return Some((id_to_ipv4(id), id_to_ipv6(id)));
            }
            self.name_to_id.remove(&name);
        }

        let total_slots = LAST_SLOT_ID - FIRST_SLOT_ID + 1;
        for _ in 0..total_slots {
            let candidate = self.next_id;
            self.next_id = if self.next_id >= LAST_SLOT_ID {
                FIRST_SLOT_ID
            } else {
                self.next_id + 1
            };

            let can_use = match self.id_to_entry.get(&candidate) {
                None => true,
                Some(existing) => now >= existing.expires_at,
            };

            if can_use {
                if let Some(old) = self.id_to_entry.remove(&candidate) {
                    if self.name_to_id.get(&old.name) == Some(&candidate) {
                        self.name_to_id.remove(&old.name);
                    }
                }
                self.name_to_id.insert(name.clone(), candidate);
                self.id_to_entry.insert(
                    candidate,
                    DnsEntry {
                        name,
                        expires_at: now + DNS_MAPPING_TTL,
                    },
                );
                return Some((id_to_ipv4(candidate), id_to_ipv6(candidate)));
            }
        }

        None
    }

    pub fn resolve_ip(&mut self, ip: IpAddr, now: Instant) -> Option<String> {
        let id = ip_to_id(ip)?;
        let entry = self.id_to_entry.get_mut(&id)?;
        entry.expires_at = now + DNS_MAPPING_TTL;
        Some(entry.name.clone())
    }

    pub fn handle_query(&mut self, query: &[u8]) -> Option<Vec<u8>> {
        let parsed = parse_dns_query(query)?;
        let now = Instant::now();

        let (ancount, answer_rdata): (u16, Option<&[u8]>) = match (parsed.qclass, parsed.qtype) {
            (QCLASS_IN, QTYPE_A) => match self.allocate_for_name(&parsed.qname, now) {
                Some((v4, _)) => {
                    return Some(build_dns_response(
                        query,
                        parsed.question_end,
                        QTYPE_A,
                        &v4.octets(),
                    ));
                }
                None => (0, None),
            },
            (QCLASS_IN, QTYPE_AAAA) if self.ipv6_enabled => {
                match self.allocate_for_name(&parsed.qname, now) {
                    Some((_, v6)) => {
                        return Some(build_dns_response(
                            query,
                            parsed.question_end,
                            QTYPE_AAAA,
                            &v6.octets(),
                        ));
                    }
                    None => (0, None),
                }
            }
            _ => (0, None),
        };

        let _ = answer_rdata;
        Some(build_empty_noerror_response(
            query,
            parsed.question_end,
            ancount,
        ))
    }
}

struct ParsedQuery {
    qname: String,
    qtype: u16,
    qclass: u16,
    question_end: usize,
}

fn canonicalize_domain(raw: &str) -> String {
    raw.trim_end_matches('.').to_ascii_lowercase()
}

#[inline]
pub fn id_to_ipv4(id: u32) -> Ipv4Addr {
    Ipv4Addr::from(IPV4_POOL_BASE + id)
}

#[inline]
pub fn ipv4_to_id(ip: Ipv4Addr) -> Option<u32> {
    let raw = u32::from(ip);
    let min = IPV4_POOL_BASE + FIRST_SLOT_ID;
    let max = IPV4_POOL_BASE + LAST_SLOT_ID;
    if (min..=max).contains(&raw) {
        Some(raw - IPV4_POOL_BASE)
    } else {
        None
    }
}

#[inline]
pub fn id_to_ipv6(id: u32) -> Ipv6Addr {
    Ipv6Addr::new(
        0xfd98,
        0x0018,
        0,
        0,
        0,
        0,
        (id >> 16) as u16,
        (id & 0xFFFF) as u16,
    )
}

#[inline]
pub fn ipv6_to_id(ip: Ipv6Addr) -> Option<u32> {
    let segs = ip.segments();
    if segs[0] != 0xfd98 || segs[1] != 0x0018 || segs[2] != 0 || segs[3] != 0 || segs[4] != 0 || segs[5] != 0 {
        return None;
    }
    let id = ((segs[6] as u32) << 16) | (segs[7] as u32);
    (FIRST_SLOT_ID..=LAST_SLOT_ID).contains(&id).then_some(id)
}

#[inline]
pub fn ip_to_id(ip: IpAddr) -> Option<u32> {
    match ip {
        IpAddr::V4(v4) => ipv4_to_id(v4),
        IpAddr::V6(v6) => ipv6_to_id(v6),
    }
}

fn parse_dns_query(packet: &[u8]) -> Option<ParsedQuery> {
    if packet.len() < 12 {
        return None;
    }
    // Must be a query (QR == 0) with at least one question.
    if (packet[2] & 0x80) != 0 {
        return None;
    }
    let qdcount = u16::from_be_bytes([packet[4], packet[5]]);
    if qdcount == 0 {
        return None;
    }

    let mut pos = 12usize;
    let mut name = String::new();
    let mut total_wire_len = 0usize;

    loop {
        let len = *packet.get(pos)?;
        pos += 1;
        if len == 0 {
            if total_wire_len >= 255 {
                return None;
            }
            break;
        }
        // Reject compressed pointers or reserved label types in question QNAME.
        if (len & 0xC0) != 0 {
            return None;
        }
        let label_len = len as usize;
        if label_len > 63 {
            return None;
        }
        total_wire_len += 1 + label_len;
        if total_wire_len > 255 {
            return None;
        }
        let label_bytes = packet.get(pos..pos + label_len)?;
        let label = std::str::from_utf8(label_bytes).ok()?;
        if !name.is_empty() {
            name.push('.');
        }
        for ch in label.chars() {
            name.push(ch.to_ascii_lowercase());
        }
        pos += label_len;
    }

    if pos + 4 > packet.len() {
        return None;
    }
    let qtype = u16::from_be_bytes([packet[pos], packet[pos + 1]]);
    let qclass = u16::from_be_bytes([packet[pos + 2], packet[pos + 3]]);
    let question_end = pos + 4;

    Some(ParsedQuery {
        qname: name,
        qtype,
        qclass,
        question_end,
    })
}

fn build_empty_noerror_response(query: &[u8], question_end: usize, ancount: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(question_end);
    out.extend_from_slice(&query[..2]);
    // QR=1, preserve OPCODE + RD, clear AA/TC.
    out.push(0x80 | (query[2] & 0x79));
    // RA=1, Z=0, RCODE=0 (NOERROR).
    out.push(0x80);
    out.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT=1
    out.extend_from_slice(&ancount.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes()); // NSCOUNT=0
    out.extend_from_slice(&0u16.to_be_bytes()); // ARCOUNT=0
    out.extend_from_slice(&query[12..question_end]);
    out
}

fn build_dns_response(query: &[u8], question_end: usize, rr_type: u16, rdata: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(question_end + 12 + rdata.len());
    out.extend_from_slice(&query[..2]);
    out.push(0x80 | (query[2] & 0x79));
    out.push(0x80);
    out.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT=1
    out.extend_from_slice(&1u16.to_be_bytes()); // ANCOUNT=1
    out.extend_from_slice(&0u16.to_be_bytes()); // NSCOUNT=0
    out.extend_from_slice(&0u16.to_be_bytes()); // ARCOUNT=0
    out.extend_from_slice(&query[12..question_end]);

    // Answer RR: pointer to QNAME at offset 12 (0xC00C)
    out.extend_from_slice(&[0xC0, 0x0C]);
    out.extend_from_slice(&rr_type.to_be_bytes());
    out.extend_from_slice(&QCLASS_IN.to_be_bytes());
    out.extend_from_slice(&DNS_RECORD_TTL_SECS.to_be_bytes());
    out.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
    out.extend_from_slice(rdata);
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn encode_query_wire(id: u16, domain: &str, qtype: u16, edns0: bool) -> Vec<u8> {
        let mut pkt = Vec::new();
        pkt.extend_from_slice(&id.to_be_bytes());
        pkt.extend_from_slice(&[0x01, 0x00]); // RD=1
        pkt.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT=1
        pkt.extend_from_slice(&0u16.to_be_bytes()); // ANCOUNT=0
        pkt.extend_from_slice(&0u16.to_be_bytes()); // NSCOUNT=0
        pkt.extend_from_slice(&(if edns0 { 1u16 } else { 0u16 }).to_be_bytes()); // ARCOUNT
        for label in domain.trim_end_matches('.').split('.') {
            pkt.push(label.len() as u8);
            pkt.extend_from_slice(label.as_bytes());
        }
        pkt.push(0);
        pkt.extend_from_slice(&qtype.to_be_bytes());
        pkt.extend_from_slice(&QCLASS_IN.to_be_bytes());
        if edns0 {
            // Minimal OPT pseudo-RR
            pkt.push(0); // root
            pkt.extend_from_slice(&41u16.to_be_bytes()); // TYPE=OPT
            pkt.extend_from_slice(&4096u16.to_be_bytes()); // UDP payload size
            pkt.extend_from_slice(&[0, 0, 0, 0]); // ext RCODE + flags
            pkt.extend_from_slice(&0u16.to_be_bytes()); // RDLEN=0
        }
        pkt
    }

    #[test]
    fn a_and_aaaa_queries_share_slot_and_resolve_back() {
        let mut dns = VirtualDns::new(true);
        let q_a = encode_query_wire(0x1234, "Example.COM.", QTYPE_A, true);
        let r_a = dns.handle_query(&q_a).expect("A response");
        assert_eq!(&r_a[..2], &[0x12, 0x34]);
        assert_eq!(u16::from_be_bytes([r_a[6], r_a[7]]), 1); // ANCOUNT=1
        assert_eq!(u16::from_be_bytes([r_a[10], r_a[11]]), 0); // ARCOUNT=0
        let v4 = Ipv4Addr::new(r_a[r_a.len() - 4], r_a[r_a.len() - 3], r_a[r_a.len() - 2], r_a[r_a.len() - 1]);
        assert_eq!(v4, Ipv4Addr::new(198, 18, 0, 2));
        let ttl_off = r_a.len() - 4 - 2 - 4;
        assert_eq!(u32::from_be_bytes(r_a[ttl_off..ttl_off + 4].try_into().unwrap()), 60);

        let q_aaaa = encode_query_wire(0x5678, "example.com", QTYPE_AAAA, false);
        let r_aaaa = dns.handle_query(&q_aaaa).expect("AAAA response");
        assert_eq!(u16::from_be_bytes([r_aaaa[6], r_aaaa[7]]), 1);
        let v6_bytes: [u8; 16] = r_aaaa[r_aaaa.len() - 16..].try_into().unwrap();
        let v6 = Ipv6Addr::from(v6_bytes);
        assert_eq!(v6, Ipv6Addr::new(0xfd98, 0x0018, 0, 0, 0, 0, 0, 2));

        let now = Instant::now();
        assert_eq!(dns.resolve_ip(IpAddr::V4(v4), now).as_deref(), Some("example.com"));
        assert_eq!(dns.resolve_ip(IpAddr::V6(v6), now).as_deref(), Some("example.com"));
    }

    #[test]
    fn unsupported_qtypes_and_disabled_ipv6_return_empty_noerror() {
        let mut dns = VirtualDns::new(false);
        let q_aaaa = encode_query_wire(0x0001, "example.org", QTYPE_AAAA, false);
        let r_aaaa = dns.handle_query(&q_aaaa).expect("empty AAAA response");
        assert_eq!(r_aaaa[3] & 0x0F, 0); // RCODE=NOERROR
        assert_eq!(u16::from_be_bytes([r_aaaa[6], r_aaaa[7]]), 0); // ANCOUNT=0

        let q_https = encode_query_wire(0x0002, "example.org", 65, false);
        let r_https = dns.handle_query(&q_https).expect("empty HTTPS response");
        assert_eq!(r_https[3] & 0x0F, 0);
        assert_eq!(u16::from_be_bytes([r_https[6], r_https[7]]), 0);
    }

    #[test]
    fn pool_bounds_never_collide_with_tun_link_and_reclaim_expired() {
        assert_eq!(id_to_ipv4(FIRST_SLOT_ID), Ipv4Addr::new(198, 18, 0, 2));
        assert_eq!(id_to_ipv4(LAST_SLOT_ID), Ipv4Addr::new(198, 19, 253, 254));
        assert!(ipv4_to_id(Ipv4Addr::new(198, 19, 254, 1)).is_none());
        assert!(ipv4_to_id(Ipv4Addr::new(198, 19, 254, 2)).is_none());
        assert!(ipv4_to_id(Ipv4Addr::new(93, 184, 216, 34)).is_none());
        assert!(ipv4_to_id(Ipv4Addr::new(255, 255, 255, 255)).is_none());

        let mut dns = VirtualDns::new(true);
        let t0 = Instant::now();
        let (ip1, _) = dns.allocate_for_name("a.test", t0).unwrap();
        assert_eq!(ip1, Ipv4Addr::new(198, 18, 0, 2));
        // Force expiration and advance next_id back to slot 2
        dns.id_to_entry.get_mut(&2).unwrap().expires_at = t0 - Duration::from_secs(1);
        dns.next_id = FIRST_SLOT_ID;
        let (ip2, _) = dns.allocate_for_name("b.test", t0).unwrap();
        assert_eq!(ip2, Ipv4Addr::new(198, 18, 0, 2));
        assert_eq!(dns.resolve_ip(IpAddr::V4(ip2), t0).as_deref(), Some("b.test"));
    }
}
