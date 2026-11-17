//! Single seam to the signature verifier (`droidc-verify`). Everything else in droidc goes through
//! here, and every `VerifyError` maps to error kind `signature`.

pub use droidc_verify::{verify, Policy, SignatureBlob, Verified, VerifyError};

impl From<VerifyError> for crate::error::Error {
    fn from(e: VerifyError) -> Self {
        crate::error::Error::signature(e.to_string())
    }
}
