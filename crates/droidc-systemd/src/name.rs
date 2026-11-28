//! Unit names (`systemd.unit(5)`, "Description") and the escaping `systemd-escape(1)` describes.
//!
//! A name is `prefix[@instance].type`. `foo@.service` is a template, `foo@bar.service` an instance of
//! it. Names are at most 255 bytes of ASCII letters, digits, `:`, `-`, `_`, `.` and `\`, with one `@`
//! for templates and instances.

use std::fmt;
use std::str::FromStr;

/// The unit types systemd defines. The stand-in loads all of them; which it can run is decided
/// elsewhere ([`crate::options`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum UnitType {
    Service,
    Socket,
    Target,
    Device,
    Mount,
    Automount,
    Swap,
    Timer,
    Path,
    Slice,
    Scope,
}

impl UnitType {
    pub const ALL: [UnitType; 11] = [
        UnitType::Service,
        UnitType::Socket,
        UnitType::Target,
        UnitType::Device,
        UnitType::Mount,
        UnitType::Automount,
        UnitType::Swap,
        UnitType::Timer,
        UnitType::Path,
        UnitType::Slice,
        UnitType::Scope,
    ];

    pub fn suffix(self) -> &'static str {
        match self {
            UnitType::Service => "service",
            UnitType::Socket => "socket",
            UnitType::Target => "target",
            UnitType::Device => "device",
            UnitType::Mount => "mount",
            UnitType::Automount => "automount",
            UnitType::Swap => "swap",
            UnitType::Timer => "timer",
            UnitType::Path => "path",
            UnitType::Slice => "slice",
            UnitType::Scope => "scope",
        }
    }

    /// The type-specific section name, if the type has one (`[Service]`, `[Socket]`, ...).
    pub fn section(self) -> Option<&'static str> {
        match self {
            UnitType::Service => Some("Service"),
            UnitType::Socket => Some("Socket"),
            UnitType::Mount => Some("Mount"),
            UnitType::Automount => Some("Automount"),
            UnitType::Swap => Some("Swap"),
            UnitType::Timer => Some("Timer"),
            UnitType::Path => Some("Path"),
            UnitType::Slice => Some("Slice"),
            UnitType::Scope => Some("Scope"),
            UnitType::Target | UnitType::Device => None,
        }
    }

    fn from_suffix(s: &str) -> Option<UnitType> {
        UnitType::ALL.into_iter().find(|t| t.suffix() == s)
    }
}

impl fmt::Display for UnitType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.suffix())
    }
}

/// A parsed, valid unit name.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct UnitName {
    prefix: String,
    /// `None`: a plain unit. `Some("")`: a template. `Some(x)`: an instance.
    instance: Option<String>,
    ty: UnitType,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NameError {
    TooLong,
    NoType,
    UnknownType(String),
    EmptyPrefix,
    BadCharacter(char),
    TooManyAts,
}

impl fmt::Display for NameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NameError::TooLong => f.write_str("unit name longer than 255 bytes"),
            NameError::NoType => f.write_str("unit name has no type suffix"),
            NameError::UnknownType(t) => write!(f, "unknown unit type '{t}'"),
            NameError::EmptyPrefix => f.write_str("unit name has an empty prefix"),
            NameError::BadCharacter(c) => write!(f, "invalid character {c:?} in unit name"),
            NameError::TooManyAts => f.write_str("more than one '@' in unit name"),
        }
    }
}

impl std::error::Error for NameError {}

pub const MAX_NAME_LEN: usize = 255;

fn valid_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, ':' | '-' | '_' | '.' | '\\')
}

impl UnitName {
    pub fn parse(s: &str) -> Result<UnitName, NameError> {
        if s.len() > MAX_NAME_LEN {
            return Err(NameError::TooLong);
        }
        let (stem, suffix) = s.rsplit_once('.').ok_or(NameError::NoType)?;
        let ty = UnitType::from_suffix(suffix)
            .ok_or_else(|| NameError::UnknownType(suffix.to_string()))?;
        let (prefix, instance) = match stem.split_once('@') {
            Some((p, i)) => {
                if i.contains('@') {
                    return Err(NameError::TooManyAts);
                }
                (p, Some(i))
            }
            None => (stem, None),
        };
        if prefix.is_empty() {
            return Err(NameError::EmptyPrefix);
        }
        for c in prefix.chars().chain(instance.unwrap_or("").chars()) {
            if !valid_name_char(c) {
                return Err(NameError::BadCharacter(c));
            }
        }
        Ok(UnitName {
            prefix: prefix.to_string(),
            instance: instance.map(str::to_string),
            ty,
        })
    }

    pub fn ty(&self) -> UnitType {
        self.ty
    }

    /// The part before `@` (or before the type suffix, for a plain unit). `%p`.
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// The instance of an instantiated unit, still escaped. `%i`. Empty for templates and plain units.
    pub fn instance(&self) -> &str {
        self.instance.as_deref().unwrap_or("")
    }

    pub fn is_template(&self) -> bool {
        self.instance.as_deref() == Some("")
    }

    pub fn is_instance(&self) -> bool {
        self.instance.as_deref().is_some_and(|i| !i.is_empty())
    }

    /// `foo@.service` for `foo@bar.service`; `None` for a unit that isn't an instance.
    pub fn template(&self) -> Option<UnitName> {
        self.is_instance().then(|| UnitName {
            prefix: self.prefix.clone(),
            instance: Some(String::new()),
            ty: self.ty,
        })
    }

    /// `foo@bar.service` from the template `foo@.service`. `None` if this isn't a template.
    pub fn instantiate(&self, instance: &str) -> Option<UnitName> {
        if !self.is_template() || instance.is_empty() || !instance.chars().all(valid_name_char) {
            return None;
        }
        Some(UnitName {
            prefix: self.prefix.clone(),
            instance: Some(instance.to_string()),
            ty: self.ty,
        })
    }

    /// The same prefix and instance with another type: `foo.socket` to `foo.service`.
    pub fn with_type(&self, ty: UnitType) -> UnitName {
        UnitName {
            prefix: self.prefix.clone(),
            instance: self.instance.clone(),
            ty,
        }
    }

    /// The name without the type suffix. `%N`.
    pub fn stem(&self) -> String {
        match &self.instance {
            Some(i) => format!("{}@{i}", self.prefix),
            None => self.prefix.clone(),
        }
    }

    /// Names of the prefix drop-in directories, most specific first: for `a-b-c.service` that is
    /// `a-b-.service` and `a-.service` (`systemd.unit(5)`: "foo-.service.d/ applies to foo-bar.service").
    pub fn dash_prefixes(&self) -> Vec<String> {
        let mut out = Vec::new();
        let p = &self.prefix;
        let mut end = p.len();
        while let Some(i) = p[..end].rfind('-') {
            // `a-.service` is not a prefix of `a-` itself.
            if i + 1 < p.len() {
                out.push(format!("{}.{}", &p[..=i], self.ty.suffix()));
            }
            end = i;
        }
        out
    }
}

impl fmt::Display for UnitName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.stem(), self.ty.suffix())
    }
}

impl FromStr for UnitName {
    type Err = NameError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        UnitName::parse(s)
    }
}

/// `systemd-escape` without `--path`: `/` becomes `-`, a leading `.` and every byte that isn't an
/// ASCII letter, digit, `:`, `_` or `.` becomes `\xNN`.
pub fn escape(s: &str) -> String {
    let mut out = String::new();
    for (i, b) in s.bytes().enumerate() {
        match b {
            b'/' => out.push('-'),
            b'.' if i == 0 => out.push_str("\\x2e"),
            b if b.is_ascii_alphanumeric() || matches!(b, b':' | b'_' | b'.') => {
                out.push(b as char)
            }
            b => out.push_str(&format!("\\x{b:02x}")),
        }
    }
    out
}

/// `systemd-escape --path`: drops leading, trailing and doubled slashes first; `/` alone is `-`.
pub fn escape_path(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return "-".to_string();
    }
    escape(&parts.join("/"))
}

/// The reverse of [`escape`]: `-` becomes `/`, `\xNN` its byte. `%I`, `%P`, `%J`.
/// Invalid escapes are kept as written.
pub fn unescape(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'-' => {
                out.push(b'/');
                i += 1;
            }
            b'\\' if bytes.get(i + 1) == Some(&b'x') => {
                match s
                    .get(i + 2..i + 4)
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
                {
                    Some(b) => {
                        out.push(b);
                        i += 4;
                    }
                    None => {
                        out.push(b'\\');
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The reverse of [`escape_path`]: always absolute. `%f` for an instance that is a path.
pub fn unescape_path(s: &str) -> String {
    if s == "-" {
        return "/".to_string();
    }
    format!("/{}", unescape(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_template_instance() {
        let n = UnitName::parse("dbus.service").unwrap();
        assert_eq!(
            (n.prefix(), n.instance(), n.ty()),
            ("dbus", "", UnitType::Service)
        );
        assert!(!n.is_template() && !n.is_instance() && n.template().is_none());

        let t = UnitName::parse("getty@.service").unwrap();
        assert!(t.is_template());
        let i = t.instantiate("tty1").unwrap();
        assert_eq!(i.to_string(), "getty@tty1.service");
        assert!(i.is_instance());
        assert_eq!(i.template().unwrap(), t);
        assert_eq!(i.stem(), "getty@tty1");
        assert!(i.instantiate("x").is_none());
        assert!(t.instantiate("").is_none());
        assert!(t.instantiate("a/b").is_none());
    }

    #[test]
    fn invalid_names() {
        assert_eq!(UnitName::parse("foo"), Err(NameError::NoType));
        assert_eq!(
            UnitName::parse("foo.bar"),
            Err(NameError::UnknownType("bar".into()))
        );
        assert_eq!(UnitName::parse(".service"), Err(NameError::EmptyPrefix));
        assert_eq!(UnitName::parse("@x.service"), Err(NameError::EmptyPrefix));
        assert_eq!(UnitName::parse("a@b@c.service"), Err(NameError::TooManyAts));
        assert_eq!(
            UnitName::parse("a b.service"),
            Err(NameError::BadCharacter(' '))
        );
        assert_eq!(
            UnitName::parse(&format!("{}.service", "a".repeat(250))),
            Err(NameError::TooLong)
        );
        assert!(UnitName::parse("dev-disk-by\\x2duuid-1234.device").is_ok());
    }

    #[test]
    fn types_and_sections() {
        for t in UnitType::ALL {
            let n = UnitName::parse(&format!("x.{t}")).unwrap();
            assert_eq!(n.ty(), t);
        }
        assert_eq!(UnitType::Service.section(), Some("Service"));
        assert_eq!(UnitType::Target.section(), None);
        let s = UnitName::parse("foo@a.socket").unwrap();
        assert_eq!(s.with_type(UnitType::Service).to_string(), "foo@a.service");
    }

    #[test]
    fn dash_prefixes() {
        let n = UnitName::parse("a-b-c.service").unwrap();
        assert_eq!(n.dash_prefixes(), vec!["a-b-.service", "a-.service"]);
        assert!(UnitName::parse("abc.service")
            .unwrap()
            .dash_prefixes()
            .is_empty());
        let t = UnitName::parse("user-runtime-dir@1000.service").unwrap();
        assert_eq!(
            t.dash_prefixes(),
            vec!["user-runtime-.service", "user-.service"]
        );
        assert_eq!(
            UnitName::parse("a-.service").unwrap().dash_prefixes(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn escaping() {
        assert_eq!(
            escape("Hallöchen, Meister"),
            "Hall\\xc3\\xb6chen\\x2c\\x20Meister"
        );
        assert_eq!(escape("a/b-c"), "a-b\\x2dc");
        assert_eq!(escape(".hidden"), "\\x2ehidden");
        assert_eq!(escape_path("/home/user/"), "home-user");
        assert_eq!(escape_path("//var//lib"), "var-lib");
        assert_eq!(escape_path("/"), "-");
        assert_eq!(unescape("home-user"), "home/user");
        assert_eq!(unescape("a-b\\x2dc"), "a/b-c");
        assert_eq!(unescape("Hall\\xc3\\xb6chen"), "Hallöchen");
        assert_eq!(unescape("bad\\xzz"), "bad\\xzz");
        assert_eq!(unescape("end\\x2"), "end\\x2");
        assert_eq!(unescape_path("home-user"), "/home/user");
        assert_eq!(unescape_path("-"), "/");
        for s in ["/run/user/1000", "/a b/ç"] {
            assert_eq!(unescape_path(&escape_path(s)), s);
        }
    }
}
