//! Which paths are usbfs device nodes.

/// Where the kernel's usbfs nodes are, and where the host writes its placeholders.
pub const USBFS: &[u8] = b"/dev/bus/usb/";

/// The bus and device number of a usbfs node path, `/dev/bus/usb/BBB/DDD`, or `None` for any other
/// path. Both numbers are 1 to 3 decimal digits, as libusb, adb and fastboot write them (`%03d`).
pub fn parse(path: &[u8]) -> Option<(u16, u16)> {
    parse_under(USBFS, path)
}

/// [`parse`] with the usbfs directory at `usbfs` (ending in `/`), for tests.
pub fn parse_under(usbfs: &[u8], path: &[u8]) -> Option<(u16, u16)> {
    let rest = path.strip_prefix(usbfs)?;
    let mut parts = rest.split(|b| *b == b'/');
    let bus = number(parts.next()?)?;
    let dev = number(parts.next()?)?;
    parts.next().is_none().then_some((bus, dev))
}

fn number(s: &[u8]) -> Option<u16> {
    if s.is_empty() || s.len() > 3 || !s.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(s).ok()?.parse().ok()
}

/// `name` joined to the directory `dir` (both as the guest sees them), for an `openat` with a
/// relative path. An absolute `name` stands alone.
pub fn join(dir: &[u8], name: &[u8]) -> Vec<u8> {
    if name.starts_with(b"/") {
        return name.to_vec();
    }
    let mut p = dir.to_vec();
    if !p.ends_with(b"/") {
        p.push(b'/');
    }
    p.extend_from_slice(name);
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_paths() {
        assert_eq!(parse(b"/dev/bus/usb/001/005"), Some((1, 5)));
        assert_eq!(parse(b"/dev/bus/usb/2/17"), Some((2, 17)));
        assert_eq!(parse(b"/dev/bus/usb/003/127"), Some((3, 127)));
    }

    #[test]
    fn other_paths() {
        for p in [
            &b"/dev/bus/usb"[..],
            b"/dev/bus/usb/",
            b"/dev/bus/usb/001",
            b"/dev/bus/usb/001/",
            b"/dev/bus/usb/001/005/x",
            b"/dev/bus/usb/001/0005",
            b"/dev/bus/usb/0a1/005",
            b"/dev/bus/usb/001/-05",
            b"/dev/bus/usb/../usb/001/005",
            b"dev/bus/usb/001/005",
            b"/sys/bus/usb/devices/1-5/busnum",
            b"/dev/null",
        ] {
            assert_eq!(parse(p), None, "{}", String::from_utf8_lossy(p));
        }
    }

    #[test]
    fn elsewhere() {
        assert_eq!(
            parse_under(b"/tmp/t/usb/", b"/tmp/t/usb/001/005"),
            Some((1, 5))
        );
        assert_eq!(parse_under(b"/tmp/t/usb/", b"/dev/bus/usb/001/005"), None);
    }

    #[test]
    fn relative_names() {
        assert_eq!(join(b"/dev/bus/usb/001", b"005"), b"/dev/bus/usb/001/005");
        assert_eq!(join(b"/dev/bus/usb/001/", b"005"), b"/dev/bus/usb/001/005");
        assert_eq!(
            join(b"/tmp", b"/dev/bus/usb/001/005"),
            b"/dev/bus/usb/001/005"
        );
    }
}
