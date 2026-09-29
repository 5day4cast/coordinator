use bitcoin::{OutPoint, XOnlyPublicKey};
use thiserror::Error;

/// An error from a signer or hook that the caller supplies.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("Arkade server request failed: {0}")]
    Server(#[source] ark_grpc::Error),
    #[error("Arkade server request failed: {0}")]
    Status(#[source] tonic::Status),
    /// arkd's `VTXO_ALREADY_REGISTERED`: a VTXO being spent is held by a batch intent.
    ///
    /// arkd queues an intent until it is confirmed in a batch or deleted, so this lasts until
    /// the intent's owner deletes it; see [`crate::delete_escrow_intent`].
    #[error("a VTXO it spends is held by a registered batch intent: {0}")]
    VtxoAlreadyRegistered(String),
    /// No registered intent spends an input of a delete proof, so there was nothing to delete.
    #[error("no registered intent spends the proof's inputs: {0}")]
    NoMatchingIntent(String),
    #[error("Arkade transaction error: {0}")]
    Ark(#[from] ark_core::Error),
    #[error(transparent)]
    Escrow(#[from] coordinator_ark_escrow::Error),
    #[error("the server's parameters are unusable: {0}")]
    ServerInfo(String),
    #[error("the pool cannot be funded: {0}")]
    InvalidPool(String),
    #[error("signing failed: {0}")]
    Signer(BoxError),
    #[error("a signer returned a bad signature for escrow {escrow} with key {key}")]
    BadSignature {
        escrow: OutPoint,
        key: XOnlyPublicKey,
    },
    #[error("the batch would not fund the pool, so nothing was forfeited: {0}")]
    Unfunded(String),
    #[error("the before-forfeits hook failed, so nothing was forfeited: {0}")]
    Hook(BoxError),
    #[error("batch {id} failed: {reason}")]
    BatchFailed { id: String, reason: String },
    #[error("unexpected batch event: {0}")]
    Protocol(String),
    #[error("timed out {0}")]
    Timeout(&'static str),
}

/// arkd's code for `VTXO_ALREADY_REGISTERED`, the only error it maps to gRPC `AlreadyExists`.
const VTXO_ALREADY_REGISTERED: i32 = 4;
/// arkd's code for `INVALID_INTENT_PROOF`, which a delete proof matching no intent also gets.
const INVALID_INTENT_PROOF: i32 = 23;

impl Error {
    /// Whether arkd refused because a VTXO being spent is held by a registered batch intent.
    pub fn is_vtxo_already_registered(&self) -> bool {
        matches!(self, Error::VtxoAlreadyRegistered(_))
    }

    /// Recognise arkd's own errors in a gRPC status; anything else is `other`.
    fn classify(status: &tonic::Status, other: impl FnOnce() -> Error) -> Error {
        let code = arkd_code(status);
        let message = status.message();
        if status.code() == tonic::Code::AlreadyExists
            && code.is_none_or(|code| code == VTXO_ALREADY_REGISTERED)
        {
            return Error::VtxoAlreadyRegistered(message.into());
        }
        if status.code() == tonic::Code::InvalidArgument
            && code.is_none_or(|code| code == INVALID_INTENT_PROOF)
            && message.contains("no matching intents")
        {
            return Error::NoMatchingIntent(message.into());
        }
        other()
    }
}

impl From<ark_grpc::Error> for Error {
    fn from(error: ark_grpc::Error) -> Self {
        let status = std::error::Error::source(&error)
            .and_then(|source| source.downcast_ref::<tonic::Status>())
            .cloned();
        match status {
            Some(status) => Error::classify(&status, || Error::Server(error)),
            None => Error::Server(error),
        }
    }
}

impl From<tonic::Status> for Error {
    fn from(status: tonic::Status) -> Self {
        Error::classify(&status.clone(), || Error::Status(status))
    }
}

/// The code arkd puts in its `ark.v1.ErrorDetails`, carried in the status's details.
fn arkd_code(status: &tonic::Status) -> Option<i32> {
    use prost::Message;
    let details = RpcStatus::decode(status.details()).ok()?;
    details
        .details
        .iter()
        .find(|any| any.type_url.ends_with("/ark.v1.ErrorDetails"))
        .and_then(|any| ErrorDetails::decode(any.value.as_slice()).ok())
        .map(|details| details.code)
}

/// `google.rpc.Status`, as gRPC carries a status's details.
#[derive(Clone, PartialEq, prost::Message)]
struct RpcStatus {
    #[prost(int32, tag = "1")]
    code: i32,
    #[prost(string, tag = "2")]
    message: String,
    #[prost(message, repeated, tag = "3")]
    details: Vec<Any>,
}

/// `google.protobuf.Any`.
#[derive(Clone, PartialEq, prost::Message)]
struct Any {
    #[prost(string, tag = "1")]
    type_url: String,
    #[prost(bytes = "vec", tag = "2")]
    value: Vec<u8>,
}

/// `ark.v1.ErrorDetails`, without its metadata.
#[derive(Clone, PartialEq, prost::Message)]
struct ErrorDetails {
    #[prost(int32, tag = "1")]
    code: i32,
    #[prost(string, tag = "2")]
    name: String,
    #[prost(string, tag = "3")]
    message: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;

    fn status(code: tonic::Code, message: &str, arkd: Option<i32>) -> tonic::Status {
        let details = arkd.map(|code| {
            RpcStatus {
                code: 0,
                message: message.into(),
                details: vec![Any {
                    type_url: "type.googleapis.com/ark.v1.ErrorDetails".into(),
                    value: ErrorDetails {
                        code,
                        name: String::new(),
                        message: message.into(),
                    }
                    .encode_to_vec(),
                }],
            }
            .encode_to_vec()
        });
        match details {
            Some(details) => tonic::Status::with_details(code, message, details.into()),
            None => tonic::Status::new(code, message),
        }
    }

    #[test]
    fn arkd_vtxo_already_registered_is_recognised() {
        let message = "VTXO_ALREADY_REGISTERED (4): vtxo(s) already registered";
        for arkd in [Some(4), None] {
            let error = Error::from(status(tonic::Code::AlreadyExists, message, arkd));
            assert!(error.is_vtxo_already_registered(), "{error}");
        }
        // Another arkd code under the same gRPC code is not.
        let error = Error::from(status(tonic::Code::AlreadyExists, message, Some(5)));
        assert!(matches!(error, Error::Status(_)));
        let error = Error::from(status(tonic::Code::InvalidArgument, message, None));
        assert!(!error.is_vtxo_already_registered());
    }

    #[test]
    fn a_delete_proof_matching_no_intent_is_recognised() {
        let message = "INVALID_INTENT_PROOF (23): no matching intents found for intent proof";
        let error = Error::from(status(tonic::Code::InvalidArgument, message, Some(23)));
        assert!(matches!(error, Error::NoMatchingIntent(_)), "{error}");
        let invalid = "INVALID_INTENT_PROOF (23): invalid signature";
        let error = Error::from(status(tonic::Code::InvalidArgument, invalid, Some(23)));
        assert!(matches!(error, Error::Status(_)));
    }
}
