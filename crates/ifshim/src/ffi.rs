//! The exported C symbols, the ioctls and the C allocation: everything unsafe in the crate.
//! [`crate::list`] decides the order and contents.

use crate::list::{self, Addr, Entry, Link, V4, V6};
use libc::{
    c_char, c_int, c_uint, c_void, ifaddrs, sockaddr, sockaddr_in, sockaddr_in6, sockaddr_ll,
};
use std::io::Write;
use std::mem::{size_of, zeroed};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

// ---- plumbing ------------------------------------------------------------------------------------

/// The next definition of a libc function after this library's, resolved once.
macro_rules! real {
    ($name:literal, $ty:ty) => {{
        static CELL: AtomicUsize = AtomicUsize::new(0);
        let mut p = CELL.load(Ordering::Relaxed);
        if p == 0 {
            // SAFETY: a NUL-terminated name; RTLD_NEXT is always a valid handle.
            p = unsafe { libc::dlsym(libc::RTLD_NEXT, concat!($name, "\0").as_ptr().cast()) }
                as usize;
            CELL.store(p, Ordering::Relaxed);
        }
        // SAFETY: the symbol is the libc function of that name, whose type `$ty` is.
        (p != 0).then(|| unsafe { std::mem::transmute::<usize, $ty>(p) })
    }};
}

fn errno() -> c_int {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn set_errno(e: c_int) {
    // SAFETY: __errno_location is the calling thread's errno.
    unsafe { *libc::__errno_location() = e };
}

fn env_flag(name: &str, cell: &'static OnceLock<bool>) -> bool {
    *cell.get_or_init(|| std::env::var_os(name).is_some_and(|v| !v.is_empty() && v != "0"))
}

fn tracing() -> bool {
    static T: OnceLock<bool> = OnceLock::new();
    env_flag("IFSHIM_TRACE", &T)
}

/// Skip the real functions and always use the fallback (tests).
fn forced() -> bool {
    static F: OnceLock<bool> = OnceLock::new();
    env_flag("IFSHIM_FORCE", &F)
}

fn trace(msg: std::fmt::Arguments) {
    if tracing() {
        let _ = writeln!(std::io::stderr(), "ifshim: {msg}");
    }
}

// ---- gathering -----------------------------------------------------------------------------------

struct Sock(c_int);
impl Sock {
    fn inet() -> Option<Sock> {
        // SAFETY: plain syscall.
        let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
        (fd >= 0).then_some(Sock(fd))
    }

    /// One `SIOCGIF*` ioctl on the interface `name`; the answer, or `None` when the kernel refuses.
    fn ifreq(&self, req: libc::Ioctl, name: &str) -> Option<libc::ifreq> {
        // SAFETY: ifreq is plain data; all zeroes is valid.
        let mut r: libc::ifreq = unsafe { zeroed() };
        let b = name.as_bytes();
        if b.len() >= r.ifr_name.len() {
            return None;
        }
        for (d, s) in r.ifr_name.iter_mut().zip(b) {
            *d = *s as c_char;
        }
        // SAFETY: a valid fd and a valid, NUL-terminated ifreq.
        (unsafe { libc::ioctl(self.0, req, &mut r) } == 0).then_some(r)
    }

    /// `SIOCGIFCONF`: every interface that has an IPv4 address, with that address.
    fn ifconf(&self) -> Option<Vec<(String, [u8; 4])>> {
        let mut n = 32usize;
        loop {
            // SAFETY: ifreq is plain data; all zeroes is valid.
            let mut buf: Vec<libc::ifreq> = (0..n).map(|_| unsafe { zeroed() }).collect();
            let size = n * size_of::<libc::ifreq>();
            // SAFETY: ifconf is plain data.
            let mut c: libc::ifconf = unsafe { zeroed() };
            c.ifc_len = size as c_int;
            c.ifc_ifcu.ifcu_req = buf.as_mut_ptr();
            // SAFETY: a valid fd; the kernel writes at most ifc_len bytes into buf.
            if unsafe { libc::ioctl(self.0, libc::SIOCGIFCONF, &mut c) } != 0 {
                return None;
            }
            let used = c.ifc_len.max(0) as usize;
            if used + size_of::<libc::ifreq>() > size && n < 4096 {
                n *= 2; // may have been cut short
                continue;
            }
            return Some(
                buf[..used / size_of::<libc::ifreq>()]
                    .iter()
                    .filter_map(|r| {
                        let name = ifname(&r.ifr_name)?;
                        // SAFETY: SIOCGIFCONF fills ifru_addr.
                        let a = unsafe { r.ifr_ifru.ifru_addr };
                        (a.sa_family as c_int == libc::AF_INET).then(|| (name, v4_of(&a)))
                    })
                    .collect(),
            );
        }
    }
}

impl Drop for Sock {
    fn drop(&mut self) {
        // SAFETY: our own fd.
        unsafe { libc::close(self.0) };
    }
}

fn ifname(raw: &[c_char]) -> Option<String> {
    let b: Vec<u8> = raw
        .iter()
        .take_while(|c| **c != 0)
        .map(|c| *c as u8)
        .collect();
    (!b.is_empty()).then(|| String::from_utf8_lossy(&b).into_owned())
}

fn v4_of(a: &sockaddr) -> [u8; 4] {
    // SAFETY: an AF_INET sockaddr is a sockaddr_in, and sockaddr is at least as large.
    let sin: sockaddr_in = unsafe { std::ptr::read_unaligned((a as *const sockaddr).cast()) };
    sin.sin_addr.s_addr.to_ne_bytes()
}

struct Gathered {
    links: Vec<Link>,
    v4: Vec<V4>,
    v6: Vec<V6>,
}

fn gather() -> Option<Gathered> {
    let s = Sock::inet()?;
    let conf = s.ifconf().unwrap_or_default();
    let v6 = std::fs::read_to_string("/proc/net/if_inet6")
        .map(|t| list::parse_if_inet6(&t))
        .unwrap_or_default();
    let dev = std::fs::read_to_string("/proc/net/dev")
        .map(|t| list::parse_proc_net_dev(&t))
        .unwrap_or_default();
    let v4_names: Vec<V4> = conf
        .iter()
        .map(|(name, addr)| V4 {
            name: name.clone(),
            addr: *addr,
            mask: [0; 4],
            other: None,
        })
        .collect();
    let mut links: Vec<Link> = list::names(&dev, &v4_names, &v6)
        .into_iter()
        .filter_map(|name| {
            // SAFETY (all three): SIOCGIFFLAGS fills ifru_flags, SIOCGIFINDEX ifru_ifindex,
            // SIOCGIFHWADDR ifru_hwaddr.
            let flags = unsafe { s.ifreq(libc::SIOCGIFFLAGS, &name)?.ifr_ifru.ifru_flags } as u16;
            let index = s
                .ifreq(libc::SIOCGIFINDEX, &name)
                .map(|r| unsafe { r.ifr_ifru.ifru_ifindex } as u32)
                .or_else(|| v6.iter().find(|a| a.name == name).map(|a| a.index))
                .unwrap_or(0);
            let hw = s.ifreq(libc::SIOCGIFHWADDR, &name).map(|r| {
                let a = unsafe { r.ifr_ifru.ifru_hwaddr };
                let mut mac = [0u8; 6];
                for (d, s) in mac.iter_mut().zip(a.sa_data.iter()) {
                    *d = *s as u8;
                }
                (a.sa_family, mac)
            });
            Some(Link {
                name,
                index,
                flags: flags as u32,
                hw,
            })
        })
        .collect();
    links.sort_by_key(|l| l.index);
    let flags_of = |name: &str| links.iter().find(|l| l.name == name).map(|l| l.flags);
    let v4: Vec<V4> = conf
        .into_iter()
        .map(|(name, addr)| {
            let mask = s
                .ifreq(libc::SIOCGIFNETMASK, &name)
                .map(|r| v4_of(unsafe { &r.ifr_ifru.ifru_netmask }))
                .unwrap_or([0; 4]);
            let f = flags_of(&name).unwrap_or(0) as c_int;
            let other = if f & libc::IFF_BROADCAST != 0 {
                s.ifreq(libc::SIOCGIFBRDADDR, &name)
                    .map(|r| v4_of(unsafe { &r.ifr_ifru.ifru_broadaddr }))
            } else if f & libc::IFF_POINTOPOINT != 0 {
                s.ifreq(libc::SIOCGIFDSTADDR, &name)
                    .map(|r| v4_of(unsafe { &r.ifr_ifru.ifru_dstaddr }))
            } else {
                None
            };
            V4 {
                name,
                addr,
                mask,
                other,
            }
        })
        .collect();
    trace(format_args!(
        "fallback found {} interfaces, {} IPv4 and {} IPv6 addresses",
        links.len(),
        v4.len(),
        v6.len()
    ));
    Some(Gathered { links, v4, v6 })
}

// ---- getifaddrs ----------------------------------------------------------------------------------

/// One entry of the single-block list, as glibc lays its own out: `freeifaddrs` frees the first
/// entry's address, so everything an entry points at lives inside the block.
#[repr(C)]
struct Slot {
    ifa: ifaddrs,
    addr: libc::sockaddr_storage,
    mask: libc::sockaddr_storage,
    other: libc::sockaddr_storage,
    name: [c_char; libc::IFNAMSIZ],
}

/// SAFETY: `p` points to a zeroed sockaddr_storage.
unsafe fn put<T>(p: *mut libc::sockaddr_storage, v: T) -> *mut sockaddr {
    debug_assert!(size_of::<T>() <= size_of::<libc::sockaddr_storage>());
    std::ptr::write(p.cast::<T>(), v);
    p.cast()
}

fn sin(a: [u8; 4]) -> sockaddr_in {
    // SAFETY: plain data.
    let mut s: sockaddr_in = unsafe { zeroed() };
    s.sin_family = libc::AF_INET as libc::sa_family_t;
    s.sin_addr.s_addr = u32::from_ne_bytes(a);
    s
}

fn sin6(a: [u8; 16], scope_id: u32) -> sockaddr_in6 {
    // SAFETY: plain data.
    let mut s: sockaddr_in6 = unsafe { zeroed() };
    s.sin6_family = libc::AF_INET6 as libc::sa_family_t;
    s.sin6_addr.s6_addr = a;
    s.sin6_scope_id = scope_id;
    s
}

/// Builds the list in one `calloc` block. Null on allocation failure, or when there is nothing.
fn build_ifaddrs(entries: &[Entry]) -> *mut ifaddrs {
    if entries.is_empty() {
        return std::ptr::null_mut();
    }
    // SAFETY: calloc of a non-zero count; zeroed Slots are valid (null pointers, empty names).
    let base = unsafe { libc::calloc(entries.len(), size_of::<Slot>()) } as *mut Slot;
    if base.is_null() {
        return base.cast();
    }
    for (i, e) in entries.iter().enumerate() {
        // SAFETY: i < entries.len(), inside the block; each field pointer stays inside its Slot.
        unsafe {
            let slot = base.add(i);
            let s = &mut *slot;
            for (d, c) in s
                .name
                .iter_mut()
                .zip(e.name.bytes().take(libc::IFNAMSIZ - 1))
            {
                *d = c as c_char;
            }
            s.ifa.ifa_name = s.name.as_mut_ptr();
            s.ifa.ifa_flags = e.flags as c_uint;
            if i + 1 < entries.len() {
                s.ifa.ifa_next = &mut (*base.add(i + 1)).ifa;
            }
            let addr = std::ptr::addr_of_mut!(s.addr);
            let mask = std::ptr::addr_of_mut!(s.mask);
            let other = std::ptr::addr_of_mut!(s.other);
            match &e.addr {
                Addr::Packet { index, hw } => {
                    let mut ll: sockaddr_ll = zeroed();
                    ll.sll_family = libc::AF_PACKET as u16;
                    ll.sll_ifindex = *index as c_int;
                    if let Some((hatype, mac)) = hw {
                        ll.sll_hatype = *hatype;
                        ll.sll_halen = 6;
                        ll.sll_addr[..6].copy_from_slice(mac);
                    }
                    s.ifa.ifa_addr = put(addr, ll);
                }
                Addr::Inet(a) => {
                    s.ifa.ifa_addr = put(addr, sin(a.addr));
                    s.ifa.ifa_netmask = put(mask, sin(a.mask));
                    if let Some(o) = a.other {
                        s.ifa.ifa_ifu = put(other, sin(o));
                    }
                }
                Addr::Inet6(a) => {
                    // glibc gives link-local addresses their interface as the scope.
                    let scope = if a.scope == 0x20 { a.index } else { 0 };
                    s.ifa.ifa_addr = put(addr, sin6(a.addr, scope));
                    s.ifa.ifa_netmask = put(mask, sin6(list::mask6(a.prefix), 0));
                }
            }
        }
    }
    base.cast()
}

/// # Safety
/// `ifap` is null or writable, as for glibc's `getifaddrs`.
#[no_mangle]
pub unsafe extern "C" fn getifaddrs(ifap: *mut *mut ifaddrs) -> c_int {
    if ifap.is_null() {
        set_errno(libc::EFAULT);
        return -1;
    }
    let mut err = libc::ENOSYS;
    if !forced() {
        if let Some(f) = real!(
            "getifaddrs",
            unsafe extern "C" fn(*mut *mut ifaddrs) -> c_int
        ) {
            // SAFETY: the caller's pointer, passed on.
            if unsafe { f(ifap) } == 0 {
                return 0;
            }
            err = errno();
            trace(format_args!(
                "getifaddrs failed (errno {err}), using the ioctl fallback"
            ));
        }
    }
    let Some(g) = gather() else {
        set_errno(err);
        return -1;
    };
    let entries = list::entries(&g.links, &g.v4, &g.v6);
    let p = build_ifaddrs(&entries);
    if p.is_null() && !entries.is_empty() {
        set_errno(libc::ENOMEM);
        return -1;
    }
    // SAFETY: checked non-null above.
    unsafe { *ifap = p };
    0
}

// ---- if_nameindex --------------------------------------------------------------------------------

/// Array and names allocated separately, as glibc's `if_freenameindex` frees them.
fn build_nameindex(v: &[(u32, String)]) -> *mut libc::if_nameindex {
    // SAFETY: calloc; the array ends with a zeroed {0, NULL} entry.
    let arr = unsafe { libc::calloc(v.len() + 1, size_of::<libc::if_nameindex>()) }
        as *mut libc::if_nameindex;
    if arr.is_null() {
        return arr;
    }
    for (i, (index, name)) in v.iter().enumerate() {
        let b = name.as_bytes();
        // SAFETY: malloc of len+1; the copy and the NUL fit.
        let s = unsafe { libc::malloc(b.len() + 1) } as *mut u8;
        if s.is_null() {
            for j in 0..i {
                // SAFETY: names allocated above.
                unsafe { libc::free((*arr.add(j)).if_name as *mut c_void) };
            }
            // SAFETY: the array allocated above.
            unsafe { libc::free(arr.cast()) };
            return std::ptr::null_mut();
        }
        // SAFETY: s has b.len()+1 bytes; i < v.len() is inside the array.
        unsafe {
            std::ptr::copy_nonoverlapping(b.as_ptr(), s, b.len());
            *s.add(b.len()) = 0;
            (*arr.add(i)).if_index = *index as c_uint;
            (*arr.add(i)).if_name = s.cast();
        }
    }
    arr
}

/// # Safety
/// As glibc's `if_nameindex`.
#[no_mangle]
pub unsafe extern "C" fn if_nameindex() -> *mut libc::if_nameindex {
    let mut err = libc::ENOSYS;
    if !forced() {
        if let Some(f) = real!(
            "if_nameindex",
            unsafe extern "C" fn() -> *mut libc::if_nameindex
        ) {
            // SAFETY: no arguments.
            let p = unsafe { f() };
            if !p.is_null() {
                return p;
            }
            err = errno();
            trace(format_args!(
                "if_nameindex failed (errno {err}), using the ioctl fallback"
            ));
        }
    }
    let Some(g) = gather() else {
        set_errno(err);
        return std::ptr::null_mut();
    };
    let p = build_nameindex(&list::name_index(&g.links));
    if p.is_null() {
        set_errno(libc::ENOMEM);
    }
    p
}
