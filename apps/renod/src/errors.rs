//! Kernel errors as wire errors (ARCH-14).
//!
//! Every [`KernelError`] maps to exactly one stable [`ErrorCode`]. The messages are for people and
//! are not a contract. Both error enums are `non_exhaustive`, so an unknown variant becomes
//! `Internal`, never a panic and never an unmapped code.

use reno_kernel::KernelError;
use reno_proto::{ErrorCode, ErrorObject};
use reno_store::StoreError;

/// The code a kernel error crosses the socket with.
pub fn code_of(error: &KernelError) -> ErrorCode {
    match error {
        KernelError::RunNotFound(_)
        | KernelError::ApprovalNotFound(_)
        | KernelError::TriggerNotFound(_) => ErrorCode::NotFound,
        KernelError::TriggerDisabled(_) => ErrorCode::Conflict,
        KernelError::NoExecutor(_) => ErrorCode::InvalidParams,
        KernelError::NotCancellable { .. } | KernelError::RunEnded(_) => ErrorCode::Conflict,
        KernelError::SteerQueueFull(_) | KernelError::ShuttingDown | KernelError::Gap { .. } => {
            ErrorCode::Unavailable
        }
        KernelError::DefinitionNotFound { .. } => ErrorCode::NotFound,
        KernelError::Invalid(_) => ErrorCode::InvalidParams,
        KernelError::Definitions(_) => ErrorCode::Internal,
        KernelError::Store(store) => store_code(store),
    }
}

fn store_code(error: &StoreError) -> ErrorCode {
    match error {
        StoreError::NotFound { .. } => ErrorCode::NotFound,
        StoreError::AlreadyExists { .. }
        | StoreError::IllegalTransition { .. }
        | StoreError::NotLeased { .. } => ErrorCode::Conflict,
        StoreError::StaleFence { .. } => ErrorCode::StaleFence,
        StoreError::InvalidArgument(_) => ErrorCode::InvalidParams,
        StoreError::Closed => ErrorCode::Unavailable,
        StoreError::Database(_)
        | StoreError::Migration(_)
        | StoreError::Corrupt(_)
        | StoreError::Io(_)
        | StoreError::Json(_) => ErrorCode::Internal,
    }
}

/// The error object for `error`. Internal failures are logged with their detail.
pub fn kernel_error(error: &KernelError) -> ErrorObject {
    let code = code_of(error);
    if code == ErrorCode::Internal {
        tracing::error!(%error, "internal error while serving a request");
    }
    ErrorObject::new(code, error.to_string())
}

/// A caller error that is not the kernel's: bad params the protocol types cannot rule out.
pub fn invalid(message: impl Into<String>) -> ErrorObject {
    ErrorObject::new(ErrorCode::InvalidParams, message)
}

#[cfg(test)]
mod tests {
    use reno_proto::defs::DefKind;
    use reno_proto::{ApprovalId, RunId, RunStatus, Seq};

    use super::*;

    fn run() -> RunId {
        RunId::from_suffix("a").expect("id")
    }

    #[test]
    fn every_kernel_error_has_its_code() {
        let approval = ApprovalId::from_suffix("a").expect("id");
        let cases: Vec<(KernelError, ErrorCode)> = vec![
            (KernelError::RunNotFound(run()), ErrorCode::NotFound),
            (KernelError::ApprovalNotFound(approval), ErrorCode::NotFound),
            (
                KernelError::NotCancellable {
                    run: run(),
                    status: RunStatus::Succeeded,
                },
                ErrorCode::Conflict,
            ),
            (KernelError::RunEnded(run()), ErrorCode::Conflict),
            (KernelError::SteerQueueFull(run()), ErrorCode::Unavailable),
            (KernelError::ShuttingDown, ErrorCode::Unavailable),
            (
                KernelError::Gap {
                    resume_from: Seq(3),
                },
                ErrorCode::Unavailable,
            ),
            (KernelError::Invalid("x".into()), ErrorCode::InvalidParams),
            (
                KernelError::NoExecutor("b".into()),
                ErrorCode::InvalidParams,
            ),
            (
                KernelError::TriggerNotFound("t".into()),
                ErrorCode::NotFound,
            ),
            (
                KernelError::TriggerDisabled("t".into()),
                ErrorCode::Conflict,
            ),
            (
                KernelError::DefinitionNotFound {
                    kind: DefKind::Bot,
                    name: "keeper".into(),
                    version: Some(2),
                },
                ErrorCode::NotFound,
            ),
            (KernelError::Definitions("x".into()), ErrorCode::Internal),
        ];
        for (error, code) in cases {
            assert_eq!(code_of(&error), code, "{error}");
            assert_eq!(kernel_error(&error).code, code, "{error}");
        }
    }

    #[test]
    fn every_store_error_has_its_code() {
        let cases: Vec<(StoreError, ErrorCode)> = vec![
            (
                StoreError::NotFound {
                    kind: "run",
                    id: "x".into(),
                },
                ErrorCode::NotFound,
            ),
            (
                StoreError::AlreadyExists {
                    kind: "run",
                    id: "x".into(),
                },
                ErrorCode::Conflict,
            ),
            (
                StoreError::StaleFence {
                    run: run(),
                    supplied: 1,
                    current: 2,
                },
                ErrorCode::StaleFence,
            ),
            (
                StoreError::IllegalTransition {
                    run: run(),
                    from: RunStatus::Succeeded,
                    to: RunStatus::Running,
                },
                ErrorCode::Conflict,
            ),
            (
                StoreError::NotLeased {
                    run: run(),
                    status: RunStatus::Queued,
                },
                ErrorCode::Conflict,
            ),
            (
                StoreError::InvalidArgument("x".into()),
                ErrorCode::InvalidParams,
            ),
            (StoreError::Closed, ErrorCode::Unavailable),
            (StoreError::Corrupt("x".into()), ErrorCode::Internal),
            (StoreError::Migration("x".into()), ErrorCode::Internal),
            (
                StoreError::Io(std::io::Error::other("x")),
                ErrorCode::Internal,
            ),
            (
                StoreError::Database("x".to_owned().into()),
                ErrorCode::Internal,
            ),
        ];
        for (error, code) in cases {
            assert_eq!(code_of(&KernelError::Store(error)), code);
        }
    }
}
