//! Error type for signature verification.

/// Why verification failed. Variants are ordered roughly by how far a signature got through the
/// keyless pipeline; when several signatures are offered and all fail, the error from the one that
/// got furthest is reported.
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum VerifyError {
    /// No policy entry matches the repository and the policy default is reject.
    #[error("no trust policy entry matches `{0}` and the default is reject")]
    NoPolicyMatch(String),
    /// The image has no signatures at all.
    #[error("no signatures found for image")]
    NoSignatures,
    /// The policy document is malformed.
    #[error("invalid policy: {0}")]
    Policy(String),
    /// The signed payload names a different manifest digest than the one being verified.
    #[error("signed payload digest does not match manifest: {0}")]
    DigestMismatch(String),
    /// Certificate chain, trusted root, or certificate shape problem.
    #[error("certificate chain verification failed: {0}")]
    CertChain(String),
    /// The signature over the payload does not verify.
    #[error("signature verification failed: {0}")]
    BadSignature(String),
    /// Rekor transparency log evidence is missing or invalid (SET, entry body).
    #[error("transparency log verification failed: {0}")]
    Tlog(String),
    /// The leaf certificate was not valid at the Rekor integrated time.
    #[error("certificate not valid at integrated time: {0}")]
    CertExpired(String),
    /// Certificate identity (issuer/SAN) does not satisfy the policy.
    #[error("identity does not match policy: {0}")]
    IdentityMismatch(String),
    /// Anything else that makes a signature unusable (malformed annotations, etc.).
    #[error("invalid signature: {0}")]
    Invalid(String),
}

impl VerifyError {
    /// Rank used to pick the "most informative" error among several failed signatures.
    pub(crate) fn rank(&self) -> u8 {
        match self {
            VerifyError::Invalid(_) => 0,
            VerifyError::DigestMismatch(_) => 1,
            VerifyError::CertChain(_) => 2,
            VerifyError::BadSignature(_) => 3,
            VerifyError::Tlog(_) => 4,
            VerifyError::CertExpired(_) => 5,
            VerifyError::IdentityMismatch(_) => 6,
            _ => 0,
        }
    }
}
