//! The interface list the fallback builds, in plain Rust: what the ioctls and `/proc/net/if_inet6`
//! said, in glibc's order.

/// One interface, from `SIOCGIFFLAGS`, `SIOCGIFINDEX` and `SIOCGIFHWADDR`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub name: String,
    pub index: u32,
    pub flags: u32,
    /// ARPHRD_* type and address, when `SIOCGIFHWADDR` answered.
    pub hw: Option<(u16, [u8; 6])>,
}

/// An IPv4 address, from `SIOCGIFCONF` with `SIOCGIFNETMASK` and `SIOCGIFBRDADDR`/`SIOCGIFDSTADDR`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct V4 {
    pub name: String,
    pub addr: [u8; 4],
    pub mask: [u8; 4],
    /// Broadcast address (`IFF_BROADCAST`) or peer (`IFF_POINTOPOINT`), per the link's flags.
    pub other: Option<[u8; 4]>,
}

/// An IPv6 address, from one line of `/proc/net/if_inet6`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct V6 {
    pub name: String,
    pub index: u32,
    pub addr: [u8; 16],
    pub prefix: u8,
    pub scope: u8,
}

/// What one `ifaddrs` entry holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Addr {
    Packet {
        index: u32,
        hw: Option<(u16, [u8; 6])>,
    },
    Inet(V4),
    Inet6(V6),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub flags: u32,
    pub addr: Addr,
}

/// Parses `/proc/net/if_inet6`: `<32 hex> <ifindex hex> <prefix hex> <scope hex> <flags hex> <name>`.
/// Lines that don't parse are skipped.
pub fn parse_if_inet6(text: &str) -> Vec<V6> {
    text.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 6 || f[0].len() != 32 {
                return None;
            }
            let mut addr = [0u8; 16];
            for (i, b) in addr.iter_mut().enumerate() {
                *b = u8::from_str_radix(f[0].get(2 * i..2 * i + 2)?, 16).ok()?;
            }
            Some(V6 {
                name: f[5].to_string(),
                index: u32::from_str_radix(f[1], 16).ok()?,
                addr,
                prefix: u8::from_str_radix(f[2], 16).ok().filter(|p| *p <= 128)?,
                scope: u8::from_str_radix(f[3], 16).ok()?,
            })
        })
        .collect()
}

/// The netmask of an IPv6 prefix length.
pub fn mask6(prefix: u8) -> [u8; 16] {
    let mut m = [0u8; 16];
    for (i, b) in m.iter_mut().enumerate() {
        let bits = (prefix as usize).saturating_sub(8 * i).min(8);
        *b = if bits == 0 { 0 } else { 0xffu8 << (8 - bits) };
    }
    m
}

/// Parses `/proc/net/dev` for interface names: every line after the two headers is `name: counters`.
/// It names interfaces that have no address too, which `SIOCGIFCONF` leaves out.
pub fn parse_proc_net_dev(text: &str) -> Vec<String> {
    text.lines()
        .skip(2)
        .filter_map(|line| {
            let (name, _) = line.split_once(':')?;
            let name = name.trim();
            (!name.is_empty() && name.len() < 16).then(|| name.to_string())
        })
        .collect()
}

/// Interface names in first-seen order, without duplicates: `/proc/net/dev`, then the IPv4 list,
/// then the IPv6 list.
pub fn names(dev: &[String], v4: &[V4], v6: &[V6]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for n in dev
        .iter()
        .chain(v4.iter().map(|a| &a.name))
        .chain(v6.iter().map(|a| &a.name))
    {
        if !out.contains(n) {
            out.push(n.clone());
        }
    }
    out
}

/// glibc's order: one `AF_PACKET` entry per link, then the IPv4 addresses, then the IPv6 ones.
/// Addresses of an interface that isn't in `links` (it vanished between calls) are dropped.
pub fn entries(links: &[Link], v4: &[V4], v6: &[V6]) -> Vec<Entry> {
    let flags = |name: &str| links.iter().find(|l| l.name == name).map(|l| l.flags);
    let mut out: Vec<Entry> = links
        .iter()
        .map(|l| Entry {
            name: l.name.clone(),
            flags: l.flags,
            addr: Addr::Packet {
                index: l.index,
                hw: l.hw,
            },
        })
        .collect();
    out.extend(v4.iter().filter_map(|a| {
        Some(Entry {
            name: a.name.clone(),
            flags: flags(&a.name)?,
            addr: Addr::Inet(a.clone()),
        })
    }));
    out.extend(v6.iter().filter_map(|a| {
        Some(Entry {
            name: a.name.clone(),
            flags: flags(&a.name)?,
            addr: Addr::Inet6(a.clone()),
        })
    }));
    out
}

/// `if_nameindex` order: by index, without duplicates.
pub fn name_index(links: &[Link]) -> Vec<(u32, String)> {
    let mut v: Vec<(u32, String)> = links.iter().map(|l| (l.index, l.name.clone())).collect();
    v.sort();
    v.dedup_by_key(|e| e.0);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    const IF_INET6: &str = "\
00000000000000000000000000000001 01 80 10 80       lo
fe800000000000000a1b2cfffe3d4e5f 1e 40 20 80    wlan0
2001db8000000000000000000000abcd 1e 40 00 00    wlan0
garbage line
";

    #[test]
    fn parses_if_inet6() {
        let v = parse_if_inet6(IF_INET6);
        assert_eq!(v.len(), 3);
        assert_eq!(v[0].name, "lo");
        assert_eq!(v[0].index, 1);
        assert_eq!(v[0].prefix, 128);
        assert_eq!(v[0].scope, 0x10);
        assert_eq!(v[0].addr[15], 1);
        assert_eq!(v[1].index, 0x1e);
        assert_eq!(v[1].prefix, 64);
        assert_eq!(&v[1].addr[..2], &[0xfe, 0x80]);
        assert_eq!(&v[2].addr[14..], &[0xab, 0xcd]);
    }

    #[test]
    fn parses_proc_net_dev() {
        let text = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo: 101281056   13808    0    0    0     0          0         0 101281056   13808    0    0    0     0       0          0
  eth0:106301866   27370    0    0    0     0          0         0 38751386   24141    0    0    0     0       0          0
";
        assert_eq!(parse_proc_net_dev(text), ["lo", "eth0"]);
    }

    #[test]
    fn masks() {
        assert_eq!(mask6(0), [0; 16]);
        assert_eq!(mask6(128), [0xff; 16]);
        let m = mask6(64);
        assert_eq!(&m[..8], &[0xff; 8]);
        assert_eq!(&m[8..], &[0; 8]);
        let m = mask6(57);
        assert_eq!(&m[..7], &[0xff; 7]);
        assert_eq!(m[7], 0x80);
        assert_eq!(m[8], 0);
    }

    fn link(name: &str, index: u32) -> Link {
        Link {
            name: name.into(),
            index,
            flags: 0x1043,
            hw: None,
        }
    }

    #[test]
    fn glibc_order_and_unknown_interfaces_dropped() {
        let v4 = vec![
            V4 {
                name: "lo".into(),
                addr: [127, 0, 0, 1],
                mask: [255, 0, 0, 0],
                other: None,
            },
            V4 {
                name: "gone".into(),
                addr: [10, 0, 0, 1],
                mask: [255; 4],
                other: None,
            },
        ];
        let v6 = parse_if_inet6(IF_INET6);
        assert_eq!(names(&[], &v4, &v6), ["lo", "gone", "wlan0"]);
        let dev = vec!["wlan0".to_string(), "dummy0".to_string()];
        assert_eq!(names(&dev, &v4, &v6), ["wlan0", "dummy0", "lo", "gone"]);
        let links = vec![link("lo", 1), link("wlan0", 30)];
        let e = entries(&links, &v4, &v6);
        let kinds: Vec<(&str, &str)> = e
            .iter()
            .map(|e| {
                let k = match e.addr {
                    Addr::Packet { .. } => "packet",
                    Addr::Inet(_) => "inet",
                    Addr::Inet6(_) => "inet6",
                };
                (e.name.as_str(), k)
            })
            .collect();
        assert_eq!(
            kinds,
            [
                ("lo", "packet"),
                ("wlan0", "packet"),
                ("lo", "inet"),
                ("lo", "inet6"),
                ("wlan0", "inet6"),
                ("wlan0", "inet6"),
            ]
        );
        assert!(e.iter().all(|e| e.flags == 0x1043));
    }

    #[test]
    fn name_index_sorted_by_index() {
        let links = vec![link("wlan0", 30), link("lo", 1), link("wlan0", 30)];
        assert_eq!(
            name_index(&links),
            [(1, "lo".to_string()), (30, "wlan0".to_string())]
        );
    }
}
