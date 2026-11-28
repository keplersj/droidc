# droidc-verify

Offline verification of [cosign](https://github.com/sigstore/cosign) "simple signing" signatures
for droidc: keyless (Fulcio + Rekor) and key-based, driven by a `policy.json` trust policy
(see `docs/design-mvp.md`, "Signatures and trust policy"). No network access at verify time; pure
Rust, no OpenSSL, builds static for `x86_64`/`aarch64-unknown-linux-musl`.

```rust
let policy = droidc_verify::Policy::default_policy();
let verified = droidc_verify::verify(&policy, "ghcr.io/projectbluefin/bluefin-lts",
                                     "sha256:...", &sig_blobs, std::time::SystemTime::now())?;
```

## Keyless pipeline (a signature is accepted if ANY offered signature passes all steps)

1. payload `critical.image.docker-manifest-digest` == the manifest digest being verified
2. leaf certificate chains (ECDSA P-256/P-384, SHA-256/384) to a Fulcio CA in the trusted root whose
   `validFor` contains the Rekor integrated time (the `chain` annotation is ignored; the chain comes
   from the trusted root only)
3. ECDSA signature over the payload verifies with the leaf key
4. Rekor `SignedEntryTimestamp` verifies (ECDSA P-256) over the canonical JSON (RFC 8785) of
   `{body, integratedTime, logID, logIndex}` with the trusted root's Rekor key (`logID` = hex
   SHA-256 of the key SPKI; key `validFor` must contain the integrated time)
5. leaf validity window contains the integrated time (Fulcio certs live ~10 min, so wall-clock
   `now` is deliberately not compared with the leaf; it is only used to reject integrated times
   in the future, 5 min skew)
6. the Rekor `hashedrekord` body matches SHA-256(payload), the signature, and the leaf certificate
7. the leaf has the code-signing EKU, Fulcio issuer extension (1.3.6.1.4.1.57264.1.8, falling back
   to legacy .1.1) equals `keyless.issuer`, and the SAN (URI or email) equals `keyless.subject` /
   matches `keyless.subject_regex`

Key-based: ECDSA (P-256/P-384) signature over the payload with any PEM key listed in the matching
policy entry; Rekor is not consulted.

Not checked: payload `docker-reference` and `optional` annotations (cosign does not enforce them by
default either; the digest is the binding), CT-log SCTs, Rekor inclusion proofs (the SET is the
offline evidence, as in cosign's default bundle verification), certificate revocation, Rekor
Ed25519 keys (the newer `log2025-1` Rekor v2 shard is not supported; its entries are not in the
legacy annotation bundle form).

## Policy

`match` is an exact repository or a trailing `/*` prefix glob; the most specific entry wins; no
match (or `default: "reject"`, the only supported default) yields `VerifyError::NoPolicyMatch`.
Keyless entries must set `issuer` and `subject` and/or `subject_regex`. An optional top-level
`trusted_root_path` replaces the embedded trusted root. Errors are typed (`DigestMismatch`,
`CertChain`, `BadSignature`, `Tlog`, `CertExpired`, `IdentityMismatch`, ...); when several
signatures all fail, the error from the one that got furthest through the pipeline is returned.

## Embedded trusted root and how to refresh it

`src/data/trusted_root.json` is the Sigstore public-good `trusted_root.json` TUF target (targets
version 14, sha256 `6494e21ea73fa7ee769f85f57d5a3e6a08725eae1e38c755fc3517c9e6bc0b66`, 6787 bytes),
fetched 2026-11-17 from
`https://tuf-repo-cdn.sigstore.dev/targets/6494e21ea73fa7ee769f85f57d5a3e6a08725eae1e38c755fc3517c9e6bc0b66.trusted_root.json`.
It contains the Fulcio root+intermediate (valid to 2031-10-05), Rekor v1 key and Rekor
`log2025-1` key. To refresh:

```sh
B=https://tuf-repo-cdn.sigstore.dev
V=$(curl -s $B/timestamp.json | jq -r '.signed.meta."snapshot.json".version')
T=$(curl -s $B/$V.snapshot.json | jq -r '.signed.meta."targets.json".version')
H=$(curl -s $B/$T.targets.json | jq -r '.signed.targets."trusted_root.json".hashes.sha256')
curl -s $B/targets/$H.trusted_root.json -o crates/droidc-verify/src/data/trusted_root.json
sha256sum crates/droidc-verify/src/data/trusted_root.json   # must equal $H
```

(Preferably verify the TUF metadata chain with `cosign initialize` / `tuf-client` rather than
trusting the CDN; the hash check above only ties the file to the CDN's targets metadata.) Users can
also ship a newer root without rebuilding via `trusted_root_path` in `policy.json`.

`src/data/bluefin-lts-cosign.pub` is
`https://raw.githubusercontent.com/projectbluefin/bluefin-lts/main/cosign.pub` (ECDSA P-256),
identical to `ublue-os/main`'s key, used by the default policy for `ghcr.io/ublue-os/*`.

`src/data/pinniped-os-cosign.pub` is the Pinniped OS image factory's key
(`https://github.com/Pinniped-OS/factory/blob/main/keys/cosign.pub`, ECDSA P-256), used by the
default policy for `ghcr.io/pinniped-os/*`, which publishes the droidc-tuned images Selkie offers.
A `policy.json` that is an unedited copy of an earlier built-in default is upgraded to the current
one when droidc loads it; an edited policy is left as it is.

## Why not `sigstore` (sigstore-rs)?

Not adopted: it drags in a large dependency tree (async runtime, HTTP/TUF and OCI clients, optional native-TLS/OpenSSL
features) aimed at online workflows,
while droidc already has its own OCI client and needs a ~700 line offline check. Its offline
verification path with caller-supplied blobs and a custom trusted root is also awkward to drive
without the registry client. Implementing the pipeline directly on RustCrypto keeps the binary
small, rustls/OpenSSL-free, and statically linkable for musl (verified: `cargo build --release
--target x86_64-unknown-linux-musl -p droidc-verify`, `cargo check --target
aarch64-unknown-linux-musl`). The trade-off is that this crate owns the verification logic, hence the
adversarial tests in `tests/verify.rs`. (Evaluated from sigstore-rs's documented dependency
surface; I did not attempt a musl build of it.)

## Dependencies

- `p256`, `p384` (with `ecdsa`, `pem`): RustCrypto ECDSA verification for leaf/CA/Rekor/user keys.
- `sha2`: SHA-256/384 digests (payload hash, prehash for ECDSA, logID).
- `x509-cert`: RustCrypto DER X.509 parsing (leaf and CA certificates, SAN/EKU, validity).
- `serde`, `serde_json`: policy, trusted root, bundle and payload JSON.
- `serde_json_canonicalizer`: RFC 8785 canonical JSON, which the Rekor SET signs over.
- `base64`, `hex`: annotation/body decoding and logID comparison.
- `regex` (no default features): `subject_regex` policy matching.
- `thiserror`: error type.

## Tests

`cargo test -p droidc-verify`: real Bluefin LTS `stable` arm64/amd64 signatures
(`tests/fixtures/`, fetched anonymously from GHCR 2026-11-17: payload bytes + annotations) verify
with the default policy; tampered digest/payload, wrong manifest, wrong subject/issuer, corrupted
SET, missing bundle, expiry (via a test Rekor key injected through `trusted_root_path`, re-signing
the SET with a shifted integrated time), no-policy-match, and key-based success / wrong key.
