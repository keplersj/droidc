//! OCI registry client (anonymous bearer-token flow) built directly on `reqwest`.
//!
//! Why not `oci-client`: droidc needs (a) rustls with the `ring` provider and webpki-roots as the
//! *only* default trust anchors plus an optional extra CA bundle, (b) a pluggable DNS resolver
//! (Android has no `/etc/resolv.conf`), (c) explicit HTTP proxy handling, (d) hash-while-streaming
//! blob download with resume and progress, and (e) raw manifest bytes so the digest we verify and
//! store is exactly what the registry served. A thin client over `reqwest` gives all of that with
//! about 400 lines and no extra abstraction to fight; the protocol subset needed here (manifest GET,
//! blob GET, token flow) is tiny.

use crate::error::{Context, Error, Result};
use crate::platform::Platform;
use crate::verify_glue::SignatureBlob;
use reqwest::blocking::{Client, Response};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{Read, Write};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const ACCEPT: &str = "application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json, \
application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.docker.distribution.manifest.v2+json";

// ---------------------------------------------------------------- image references

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reference {
    Tag(String),
    Digest(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef {
    pub registry: String,
    pub repository: String,
    pub reference: Reference,
    pub original: String,
}

impl ImageRef {
    pub fn parse(s: &str) -> Result<ImageRef> {
        let original = s.to_string();
        let (name, digest) = match s.split_once('@') {
            Some((n, d)) => (n, Some(d.to_string())),
            None => (s, None),
        };
        if let Some(d) = &digest {
            let ok = d.strip_prefix("sha256:").map(|h| h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit())).unwrap_or(false);
            if !ok {
                return Err(Error::invalid(format!("bad digest in reference '{s}'")));
            }
        }
        let last_slash = name.rfind('/').map(|i| i + 1).unwrap_or(0);
        let (path, tag) = match name[last_slash..].rfind(':') {
            Some(i) => (&name[..last_slash + i], Some(name[last_slash + i + 1..].to_string())),
            None => (name, None),
        };
        if path.is_empty() {
            return Err(Error::invalid(format!("empty image reference '{s}'")));
        }
        let (registry, repository) = match path.split_once('/') {
            Some((first, rest)) if first.contains('.') || first.contains(':') || first == "localhost" => (first.to_string(), rest.to_string()),
            _ => ("docker.io".to_string(), path.to_string()),
        };
        let repository = if registry == "docker.io" && !repository.contains('/') { format!("library/{repository}") } else { repository };
        if repository.is_empty() {
            return Err(Error::invalid(format!("bad image reference '{s}'")));
        }
        let reference = match (digest, tag) {
            (Some(d), _) => Reference::Digest(d),
            (None, Some(t)) => Reference::Tag(t),
            (None, None) => Reference::Tag("latest".into()),
        };
        Ok(ImageRef { registry, repository, reference, original })
    }

    pub fn reference_str(&self) -> &str {
        match &self.reference {
            Reference::Tag(t) => t,
            Reference::Digest(d) => d,
        }
    }

    /// Name used for trust-policy matching, e.g. `ghcr.io/projectbluefin/bluefin-lts`.
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.registry, self.repository)
    }

    fn host(&self) -> &str {
        if self.registry == "docker.io" {
            "registry-1.docker.io"
        } else {
            &self.registry
        }
    }

    fn base_url(&self) -> String {
        let h = self.host();
        let local = h.starts_with("localhost") || h.starts_with("127.") || h.starts_with("[::1]");
        format!("{}://{}", if local { "http" } else { "https" }, h)
    }
}

// ---------------------------------------------------------------- OCI documents

#[derive(Debug, Clone, Deserialize)]
pub struct PlatformDesc {
    pub architecture: String,
    pub os: String,
    #[serde(default)]
    pub variant: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Descriptor {
    #[serde(rename = "mediaType", default)]
    pub media_type: String,
    pub digest: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub annotations: BTreeMap<String, String>,
    #[serde(default)]
    pub platform: Option<PlatformDesc>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub config: Descriptor,
    pub layers: Vec<Descriptor>,
}

#[derive(Debug, Clone, Deserialize)]
struct Index {
    manifests: Vec<Descriptor>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ImageConfigInner {
    #[serde(rename = "Labels", default)]
    pub labels: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ImageConfig {
    #[serde(default)]
    pub created: Option<String>,
    #[serde(default)]
    pub config: ImageConfigInner,
}

#[derive(Debug, Clone)]
pub struct Resolved {
    pub image: ImageRef,
    pub platform: Platform,
    pub index_digest: Option<String>,
    pub index_bytes: Option<Vec<u8>>,
    pub manifest_digest: String,
    pub manifest_bytes: Vec<u8>,
    pub manifest: Manifest,
    pub config_bytes: Vec<u8>,
    pub config: ImageConfig,
}

pub fn sha256_hex(b: &[u8]) -> String {
    hex::encode(Sha256::digest(b))
}

// ---------------------------------------------------------------- HTTP client construction

#[derive(Debug, Clone, Default)]
pub struct HttpConfig {
    /// Explicit `--dns` / `DROIDC_DNS` value (comma separated IPs).
    pub dns: Option<String>,
    /// Extra PEM CA bundle (`DROIDC_EXTRA_CA_FILE`, then `SSL_CERT_FILE`).
    pub extra_ca: Option<PathBuf>,
}

impl HttpConfig {
    pub fn from_env(dns: Option<String>) -> HttpConfig {
        let dns = dns.or_else(|| std::env::var("DROIDC_DNS").ok().filter(|s| !s.is_empty()));
        let extra_ca = std::env::var_os("DROIDC_EXTRA_CA_FILE")
            .or_else(|| std::env::var_os("SSL_CERT_FILE"))
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty());
        HttpConfig { dns, extra_ca }
    }
}

pub fn parse_dns_list(s: &str) -> Result<Vec<IpAddr>> {
    s.split(',')
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(|x| x.parse::<IpAddr>().map_err(|_| Error::invalid(format!("bad DNS server address '{x}'"))))
        .collect()
}

pub fn parse_resolv_conf(s: &str) -> Vec<IpAddr> {
    s.lines()
        .filter_map(|l| {
            let l = l.trim();
            let rest = l.strip_prefix("nameserver")?;
            let tok = rest.trim().split_whitespace().next()?;
            let tok = tok.split('%').next()?; // drop zone ids
            tok.parse::<IpAddr>().ok()
        })
        .collect()
}

/// `--dns`/`DROIDC_DNS`, then `/etc/resolv.conf`, then 1.1.1.1 and 8.8.8.8.
pub fn nameservers(explicit: Option<&str>) -> Result<Vec<IpAddr>> {
    if let Some(s) = explicit {
        let v = parse_dns_list(s)?;
        if !v.is_empty() {
            return Ok(v);
        }
    }
    if let Ok(s) = fs::read_to_string("/etc/resolv.conf") {
        let v = parse_resolv_conf(&s);
        if !v.is_empty() {
            return Ok(v);
        }
    }
    Ok(vec!["1.1.1.1".parse().unwrap(), "8.8.8.8".parse().unwrap()])
}

struct HickoryDns {
    servers: Vec<IpAddr>,
    resolver: tokio_cell::Cell,
}

mod tokio_cell {
    use hickory_resolver::TokioResolver;
    use std::sync::OnceLock;
    #[derive(Default)]
    pub struct Cell(pub OnceLock<TokioResolver>);
}

impl reqwest::dns::Resolve for HickoryDns {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        use hickory_resolver::config::{NameServerConfigGroup, ResolverConfig};
        use hickory_resolver::name_server::TokioConnectionProvider;
        use hickory_resolver::TokioResolver;
        let servers = self.servers.clone();
        // The resolver is created on first use, inside reqwest's runtime.
        let resolver = self
            .resolver
            .0
            .get_or_init(|| {
                let cfg = ResolverConfig::from_parts(None, vec![], NameServerConfigGroup::from_ips_clear(&servers, 53, true));
                TokioResolver::builder_with_config(cfg, TokioConnectionProvider::default()).build()
            })
            .clone();
        Box::pin(async move {
            let lookup = resolver.lookup_ip(name.as_str()).await?;
            let addrs: Vec<std::net::SocketAddr> = lookup.iter().map(|ip| std::net::SocketAddr::new(ip, 0)).collect();
            let it: reqwest::dns::Addrs = Box::new(addrs.into_iter());
            Ok(it)
        })
    }
}

pub fn build_http_client(cfg: &HttpConfig, total_timeout: Duration) -> Result<Client> {
    use rustls::pki_types::{pem::PemObject, CertificateDer};
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if let Some(p) = &cfg.extra_ca {
        let mut n = 0;
        for cert in CertificateDer::pem_file_iter(p).map_err(|e| Error::invalid(format!("extra CA file {}: {e}", p.display())))? {
            let cert = cert.map_err(|e| Error::invalid(format!("extra CA file {}: {e}", p.display())))?;
            if roots.add(cert).is_ok() {
                n += 1;
            }
        }
        if n == 0 {
            return Err(Error::invalid(format!("extra CA file {} holds no usable certificates", p.display())));
        }
    }
    let mut tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::internal(format!("tls: {e}")))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    tls.alpn_protocols = vec![b"http/1.1".to_vec()];

    let servers = nameservers(cfg.dns.as_deref())?;
    let mut b = Client::builder()
        .use_preconfigured_tls(tls)
        .dns_resolver(Arc::new(HickoryDns { servers, resolver: Default::default() }))
        .connect_timeout(Duration::from_secs(20))
        .timeout(total_timeout)
        .user_agent(concat!("droidc/", env!("CARGO_PKG_VERSION")))
        .no_proxy();
    let no_proxy = reqwest::NoProxy::from_env();
    for var in ["HTTPS_PROXY", "https_proxy"] {
        if let Ok(u) = std::env::var(var) {
            if !u.is_empty() {
                let p = reqwest::Proxy::https(&u).map_err(|e| Error::invalid(format!("{var}: {e}")))?.no_proxy(no_proxy.clone());
                b = b.proxy(p);
                break;
            }
        }
    }
    for var in ["HTTP_PROXY", "http_proxy"] {
        if let Ok(u) = std::env::var(var) {
            if !u.is_empty() {
                let p = reqwest::Proxy::http(&u).map_err(|e| Error::invalid(format!("{var}: {e}")))?.no_proxy(no_proxy.clone());
                b = b.proxy(p);
                break;
            }
        }
    }
    b.build().map_err(|e| Error::internal(format!("http client: {e}")))
}

// ---------------------------------------------------------------- registry client

pub struct Registry {
    /// Short total timeout: manifests, tokens, small blobs.
    http: Client,
    /// Long total timeout (1h): layer blobs (the blocking client has no per-read timeout).
    bulk: Client,
    tokens: Mutex<HashMap<String, String>>,
}

fn net(e: reqwest::Error) -> Error {
    Error::network(format!("{}", describe(&e)))
}

fn describe(e: &reqwest::Error) -> String {
    let mut s = e.to_string();
    let mut src = std::error::Error::source(e);
    while let Some(x) = src {
        s.push_str(": ");
        s.push_str(&x.to_string());
        src = x.source();
    }
    s
}

fn parse_challenge(h: &str) -> Option<HashMap<String, String>> {
    let rest = h.trim().strip_prefix("Bearer").or_else(|| h.trim().strip_prefix("bearer"))?;
    let mut out = HashMap::new();
    for part in rest.trim().split(',') {
        if let Some((k, v)) = part.split_once('=') {
            out.insert(k.trim().to_ascii_lowercase(), v.trim().trim_matches('"').to_string());
        }
    }
    Some(out)
}

impl Registry {
    pub fn new(cfg: &HttpConfig) -> Result<Registry> {
        Ok(Registry { http: build_http_client(cfg, Duration::from_secs(60))?, bulk: build_http_client(cfg, Duration::from_secs(3600))?, tokens: Mutex::new(HashMap::new()) })
    }

    fn token_key(r: &ImageRef) -> String {
        format!("{}/{}", r.registry, r.repository)
    }

    fn fetch_token(&self, r: &ImageRef, challenge: &str) -> Result<()> {
        let ch = parse_challenge(challenge).ok_or_else(|| Error::network(format!("unsupported auth challenge from {}: {challenge}", r.registry)))?;
        let realm = ch.get("realm").ok_or_else(|| Error::network("auth challenge without realm"))?;
        let mut url = reqwest::Url::parse(realm).map_err(|e| Error::network(format!("bad auth realm: {e}")))?;
        {
            let mut q = url.query_pairs_mut();
            if let Some(s) = ch.get("service") {
                q.append_pair("service", s);
            }
            let scope = ch.get("scope").cloned().unwrap_or_else(|| format!("repository:{}:pull", r.repository));
            q.append_pair("scope", &scope);
        }
        let resp = self.http.get(url).send().map_err(net)?;
        if !resp.status().is_success() {
            return Err(Error::not_found(format!("token request for {} failed: HTTP {}", r.full_name(), resp.status())));
        }
        let v: serde_json::Value = serde_json::from_slice(&resp.bytes().map_err(net)?).map_err(|e| Error::network(format!("bad token response: {e}")))?;
        let tok = v.get("token").or_else(|| v.get("access_token")).and_then(|t| t.as_str()).ok_or_else(|| Error::network("token response without token"))?;
        self.tokens.lock().unwrap().insert(Self::token_key(r), tok.to_string());
        Ok(())
    }

    /// GET with auth handling and retries on transient failures. Returns the response for any
    /// status other than 401-after-auth and retryable ones.
    fn get(&self, r: &ImageRef, url: &str, accept: Option<&str>, range_from: Option<u64>) -> Result<Response> {
        self.get_with(&self.http, r, url, accept, range_from)
    }

    fn get_with(&self, client: &Client, r: &ImageRef, url: &str, accept: Option<&str>, range_from: Option<u64>) -> Result<Response> {
        let mut authed_once = false;
        let mut attempt = 0;
        loop {
            let mut req = client.get(url);
            if let Some(t) = self.tokens.lock().unwrap().get(&Self::token_key(r)) {
                req = req.bearer_auth(t);
            }
            if let Some(a) = accept {
                req = req.header("Accept", a);
            }
            if let Some(from) = range_from {
                req = req.header("Range", format!("bytes={from}-"));
            }
            match req.send() {
                Err(e) => {
                    attempt += 1;
                    if attempt >= 4 {
                        return Err(net(e));
                    }
                    std::thread::sleep(Duration::from_millis(400 << attempt));
                }
                Ok(resp) => {
                    let st = resp.status();
                    if st.as_u16() == 401 && !authed_once {
                        authed_once = true;
                        let ch = resp.headers().get("www-authenticate").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
                        self.fetch_token(r, &ch)?;
                        continue;
                    }
                    if (st.as_u16() == 429 || st.is_server_error()) && attempt < 3 {
                        attempt += 1;
                        std::thread::sleep(Duration::from_millis(500 << attempt));
                        continue;
                    }
                    return Ok(resp);
                }
            }
        }
    }

    fn manifest_url(r: &ImageRef, reference: &str) -> String {
        format!("{}/v2/{}/manifests/{}", r.base_url(), r.repository, reference)
    }

    /// Fetch raw manifest bytes (digest-verified when `reference` is a digest). Returns None on 404.
    fn get_manifest_bytes(&self, r: &ImageRef, reference: &str) -> Result<Option<(Vec<u8>, String)>> {
        let resp = self.get(r, &Self::manifest_url(r, reference), Some(ACCEPT), None)?;
        let st = resp.status();
        if st.as_u16() == 404 {
            return Ok(None);
        }
        if st.as_u16() == 401 || st.as_u16() == 403 {
            return Err(Error::not_found(format!("{} not found or not accessible anonymously (HTTP {st})", r.original)));
        }
        if !st.is_success() {
            return Err(Error::network(format!("manifest {reference}: HTTP {st}")));
        }
        let bytes = resp.bytes().map_err(net)?.to_vec();
        let digest = format!("sha256:{}", sha256_hex(&bytes));
        if reference.starts_with("sha256:") && reference != digest {
            return Err(Error::network(format!("manifest digest mismatch: wanted {reference}, got {digest}")));
        }
        Ok(Some((bytes, digest)))
    }

    /// Resolve a tag or digest to a single-platform manifest plus config.
    pub fn resolve(&self, r: &ImageRef, plat: &Platform) -> Result<Resolved> {
        let (bytes, digest) = self
            .get_manifest_bytes(r, r.reference_str())?
            .ok_or_else(|| Error::not_found(format!("manifest unknown: {}", r.original)))?;
        let v: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| Error::network(format!("bad manifest json: {e}")))?;
        let (index_digest, index_bytes, m_digest, m_bytes) = if v.get("manifests").is_some() {
            let idx: Index = serde_json::from_value(v).map_err(|e| Error::network(format!("bad index: {e}")))?;
            let want = &plat.arch;
            let pick = idx
                .manifests
                .iter()
                .find(|d| {
                    d.platform.as_ref().map(|p| {
                        p.os == plat.os && p.architecture == *want && (want != "arm64" || matches!(p.variant.as_deref(), None | Some("") | Some("v8")))
                    }) == Some(true)
                })
                .ok_or_else(|| Error::not_found(format!("{} has no manifest for platform {plat}", r.original)))?;
            let (mb, md) = self
                .get_manifest_bytes(r, &pick.digest)?
                .ok_or_else(|| Error::not_found(format!("platform manifest {} missing", pick.digest)))?;
            (Some(digest), Some(bytes), md, mb)
        } else {
            (None, None, digest, bytes)
        };
        let manifest: Manifest = serde_json::from_slice(&m_bytes).map_err(|e| Error::network(format!("bad image manifest: {e}")))?;
        let config_bytes = self.get_blob_bytes(r, &manifest.config.digest, 8 << 20)?;
        let config: ImageConfig = serde_json::from_slice(&config_bytes).map_err(|e| Error::network(format!("bad image config: {e}")))?;
        Ok(Resolved {
            image: r.clone(),
            platform: plat.clone(),
            index_digest,
            index_bytes,
            manifest_digest: m_digest,
            manifest_bytes: m_bytes,
            manifest,
            config_bytes,
            config,
        })
    }

    fn blob_url(r: &ImageRef, digest: &str) -> String {
        format!("{}/v2/{}/blobs/{}", r.base_url(), r.repository, digest)
    }

    /// Small blob into memory, digest-verified.
    pub fn get_blob_bytes(&self, r: &ImageRef, digest: &str, max: u64) -> Result<Vec<u8>> {
        let resp = self.get(r, &Self::blob_url(r, digest), None, None)?;
        if !resp.status().is_success() {
            return Err(Error::network(format!("blob {digest}: HTTP {}", resp.status())));
        }
        let mut buf = Vec::new();
        resp.take(max + 1).read_to_end(&mut buf).map_err(|e| Error::network(format!("blob {digest}: {e}")))?;
        if buf.len() as u64 > max {
            return Err(Error::network(format!("blob {digest} larger than {max} bytes")));
        }
        let got = format!("sha256:{}", sha256_hex(&buf));
        if got != digest {
            return Err(Error::network(format!("blob digest mismatch: wanted {digest}, got {got}")));
        }
        Ok(buf)
    }

    /// Download a blob into `blob_path` (content-addressed), hashing while streaming. Skips if the
    /// blob is already present with the right size. Resumes a partial download from `tmp_dir`.
    /// `on_bytes` is called with the number of bytes transferred by each chunk. Returns bytes
    /// downloaded over the network (0 when cached).
    pub fn fetch_blob(&self, r: &ImageRef, desc: &Descriptor, blob_path: &Path, tmp_dir: &Path, on_bytes: &dyn Fn(u64)) -> Result<u64> {
        if let Ok(md) = fs::metadata(blob_path) {
            if desc.size == 0 || md.len() == desc.size {
                return Ok(0);
            }
            let _ = fs::remove_file(blob_path);
        }
        fs::create_dir_all(tmp_dir).context(tmp_dir.display())?;
        let hexpart = desc.digest.strip_prefix("sha256:").ok_or_else(|| Error::invalid(format!("unsupported digest {}", desc.digest)))?;
        let part = tmp_dir.join(format!("{hexpart}.part"));
        let mut hasher = Sha256::new();
        let mut have = 0u64;
        if let Ok(mut f) = fs::File::open(&part) {
            let mut buf = vec![0u8; 256 * 1024];
            loop {
                let n = f.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                have += n as u64;
            }
        }
        let url = Self::blob_url(r, &desc.digest);
        let mut out = fs::OpenOptions::new().create(true).append(true).open(&part).context(part.display())?;
        let mut buf = vec![0u8; 256 * 1024];
        let mut got = 0u64;
        let mut failures = 0;
        on_bytes(have);
        'attempt: loop {
            let mut resp = self.get_with(&self.bulk, r, &url, None, if have > 0 { Some(have) } else { None })?;
            let st = resp.status();
            if have > 0 && st.as_u16() != 206 && st.is_success() {
                // Server ignored the range: start over from zero.
                on_bytes(0);
                out.set_len(0)?;
                use std::io::Seek;
                out.seek(std::io::SeekFrom::Start(0))?;
                hasher = Sha256::new();
                have = 0;
                got = 0;
            }
            if !st.is_success() {
                return Err(if st.as_u16() == 404 {
                    Error::not_found(format!("blob {} not found", desc.digest))
                } else {
                    Error::network(format!("blob {}: HTTP {st}", desc.digest))
                });
            }
            loop {
                match resp.read(&mut buf) {
                    Ok(0) if desc.size == 0 || have >= desc.size => break 'attempt,
                    Ok(0) => {
                        failures += 1;
                        if failures > 6 {
                            return Err(Error::network(format!("blob {}: connection closed early at {have}/{} bytes", desc.digest, desc.size)));
                        }
                        continue 'attempt;
                    }
                    Ok(n) => {
                        hasher.update(&buf[..n]);
                        out.write_all(&buf[..n]).context(part.display())?;
                        have += n as u64;
                        got += n as u64;
                        on_bytes(n as u64);
                    }
                    Err(e) => {
                        failures += 1;
                        if failures > 6 {
                            return Err(Error::network(format!("blob {}: {e}", desc.digest)));
                        }
                        std::thread::sleep(Duration::from_millis(500 * failures));
                        continue 'attempt; // resume from `have` with a Range request
                    }
                }
            }
        }
        out.sync_all()?;
        drop(out);
        let digest = hex::encode(hasher.finalize());
        if digest != hexpart || (desc.size != 0 && have != desc.size) {
            let _ = fs::remove_file(&part);
            return Err(Error::network(format!("blob {} failed verification (got sha256:{digest}, {} bytes)", desc.digest, have)));
        }
        if let Some(p) = blob_path.parent() {
            fs::create_dir_all(p)?;
        }
        fs::rename(&part, blob_path).context(blob_path.display())?;
        Ok(got)
    }

    /// Cosign signatures for `digest`: the layers of the `sha256-<hex>.sig` manifest. Empty if none.
    pub fn fetch_signatures(&self, r: &ImageRef, digest: &str) -> Result<Vec<SignatureBlob>> {
        let hexpart = digest.strip_prefix("sha256:").unwrap_or(digest);
        let tag = format!("sha256-{hexpart}.sig");
        let Some((bytes, _)) = self.get_manifest_bytes(r, &tag).or_else(|e| {
            // anonymous access to a missing signature tag may be reported as 401/403 on some registries
            if e.kind == crate::error::ErrorKind::NotFound { Ok(None) } else { Err(e) }
        })?
        else {
            return Ok(vec![]);
        };
        let m: Manifest = serde_json::from_slice(&bytes).map_err(|e| Error::network(format!("bad signature manifest: {e}")))?;
        let mut out = Vec::new();
        for l in &m.layers {
            let payload = self.get_blob_bytes(r, &l.digest, 1 << 20)?;
            out.push(SignatureBlob { payload, annotations: l.annotations.clone() });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_refs() {
        let r = ImageRef::parse("quay.io/centos-bootc/centos-bootc:stream10").unwrap();
        assert_eq!(r.registry, "quay.io");
        assert_eq!(r.repository, "centos-bootc/centos-bootc");
        assert_eq!(r.reference, Reference::Tag("stream10".into()));
        assert_eq!(r.full_name(), "quay.io/centos-bootc/centos-bootc");
        let d = format!("ghcr.io/projectbluefin/bluefin-lts@sha256:{}", "a".repeat(64));
        let r = ImageRef::parse(&d).unwrap();
        assert_eq!(r.reference, Reference::Digest(format!("sha256:{}", "a".repeat(64))));
        let r = ImageRef::parse("ghcr.io/x/y").unwrap();
        assert_eq!(r.reference, Reference::Tag("latest".into()));
        let r = ImageRef::parse("localhost:5000/foo:1").unwrap();
        assert_eq!(r.registry, "localhost:5000");
        assert_eq!(r.base_url(), "http://localhost:5000");
        let r = ImageRef::parse("fedora:40").unwrap();
        assert_eq!((r.registry.as_str(), r.repository.as_str()), ("docker.io", "library/fedora"));
        assert!(ImageRef::parse("ghcr.io/x/y@sha256:zz").is_err());
    }

    #[test]
    fn dns_parsing_and_fallback_order() {
        assert_eq!(parse_dns_list("1.1.1.1, 2606:4700::1111").unwrap().len(), 2);
        assert!(parse_dns_list("nope").is_err());
        let rc = "# c\nnameserver 192.168.1.1\nnameserver fe80::1%eth0\nsearch lan\nnameserver bogus\n";
        assert_eq!(parse_resolv_conf(rc).len(), 2);
        assert_eq!(nameservers(Some("9.9.9.9")).unwrap(), vec!["9.9.9.9".parse::<IpAddr>().unwrap()]);
        assert!(!nameservers(None).unwrap().is_empty());
    }

    #[test]
    fn challenge_parsing() {
        let c = parse_challenge(r#"Bearer realm="https://ghcr.io/token",service="ghcr.io",scope="repository:a/b:pull""#).unwrap();
        assert_eq!(c["realm"], "https://ghcr.io/token");
        assert_eq!(c["scope"], "repository:a/b:pull");
        assert!(parse_challenge("Basic realm=x").is_none());
    }

    #[test]
    fn client_builds_with_extra_ca() {
        build_http_client(&HttpConfig::default(), Duration::from_secs(5)).unwrap();
        let bad = tempfile::NamedTempFile::new().unwrap();
        assert!(build_http_client(&HttpConfig { dns: None, extra_ca: Some(bad.path().into()) }, Duration::from_secs(5)).is_err());
    }
}
