//! Kernel errors as wire errors (ARCH-14).
//!
//! Every [`KernelError`] maps to exactly one stable [`ErrorCode`]. The messages are for people and
//! are not a contract. Both error enums are `non_exhaustive`, so an unknown variant becomes
//! `Internal`, never a panic and never an unmapped code.

use reno_kernel::{KernelError, WorkspaceError};
use reno_proto::{ErrorCode, ErrorObject};
use reno_store::StoreError;

use crate::install::InstallError;

/// The code a kernel error crosses the socket with.
pub fn code_of(error: &KernelError) -> ErrorCode {
    match error {
        KernelError::RunNotFound(_)
        | KernelError::ApprovalNotFound(_)
        | KernelError::TriggerNotFound(_)
        | KernelError::ExecutorNotFound(_) => ErrorCode::NotFound,
        KernelError::TriggerDisabled(_) => ErrorCode::Conflict,
        KernelError::NoExecutor(_) => ErrorCode::InvalidParams,
        KernelError::NotCancellable { .. }
        | KernelError::NotResumable { .. }
        | KernelError::RunEnded(_) => ErrorCode::Conflict,
        KernelError::SteerQueueFull(_) | KernelError::ShuttingDown | KernelError::Gap { .. } => {
            ErrorCode::Unavailable
        }
        KernelError::DefinitionNotFound { .. } => ErrorCode::NotFound,
        KernelError::Invalid(_) => ErrorCode::InvalidParams,
        // One answer for every cause: a caller learns nothing about which tokens exist.
        KernelError::AgentDenied => ErrorCode::Unauthorized,
        KernelError::BoardStale { .. } | KernelError::BoardFull { .. } => ErrorCode::Conflict,
        KernelError::BudgetExhausted { .. } => ErrorCode::BudgetExceeded,
        KernelError::Definitions(_) => ErrorCode::Internal,
        KernelError::WorkspaceNotFound(_) => ErrorCode::NotFound,
        KernelError::Workspace(workspace) => workspace_code(workspace),
        KernelError::Store(store) => store_code(store),
    }
}

fn workspace_code(error: &WorkspaceError) -> ErrorCode {
    match error {
        WorkspaceError::NotAGitRepo { .. }
        | WorkspaceError::NoCommits { .. }
        | WorkspaceError::BareRepository { .. }
        | WorkspaceError::Unsafe(_) => ErrorCode::InvalidParams,
        WorkspaceError::PathTaken { .. } | WorkspaceError::InUse(_) => ErrorCode::Conflict,
        WorkspaceError::Failed(_) => ErrorCode::Internal,
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
    let object = ErrorObject::new(code, error.to_string());
    match error {
        // The entry as it stands, so a writer can merge and write again without another call.
        KernelError::BoardStale { current } => ErrorObject {
            data: serde_json::to_value(reno_proto::BoardConflict {
                current: current.as_deref().cloned(),
            })
            .ok(),
            ..object
        },
        // The limit and the use, typed, so a client never parses the message.
        KernelError::BudgetExhausted { bot, stop } => ErrorObject {
            data: serde_json::to_value(reno_proto::BudgetRefusal {
                bot: bot.clone(),
                stop: *stop,
            })
            .ok(),
            ..object
        },
        // Why the run cannot be resumed, typed, so a client never parses the message.
        KernelError::NotResumable { run, why } => ErrorObject {
            data: serde_json::to_value(reno_proto::NotResumable {
                run_id: run.clone(),
                why: why.clone(),
            })
            .ok(),
            ..object
        },
        _ => object,
    }
}

/// The code an install error crosses the socket with.
pub fn install_code_of(error: &InstallError) -> ErrorCode {
    match error {
        InstallError::NotFound(_) => ErrorCode::NotFound,
        InstallError::NothingToInstall { .. } | InstallError::InProgress(_) => ErrorCode::Conflict,
        InstallError::Failed { .. } | InstallError::ShuttingDown => ErrorCode::Unavailable,
        InstallError::Broken { .. } => ErrorCode::Internal,
        InstallError::Kernel(kernel) => code_of(kernel),
    }
}

/// The error object for an install error. Internal failures are logged with their detail.
pub fn install_error(error: &InstallError) -> ErrorObject {
    let code = install_code_of(error);
    if code == ErrorCode::Internal {
        tracing::error!(%error, "internal error while installing an executor");
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
            (
                KernelError::NotResumable {
                    run: run(),
                    why: reno_proto::NotResumed::NoSession,
                },
                ErrorCode::Conflict,
            ),
            (KernelError::SteerQueueFull(run()), ErrorCode::Unavailable),
            (KernelError::ShuttingDown, ErrorCode::Unavailable),
            (
                KernelError::Gap {
                    resume_from: Seq(3),
                },
                ErrorCode::Unavailable,
            ),
            (KernelError::Invalid("x".into()), ErrorCode::InvalidParams),
            (KernelError::AgentDenied, ErrorCode::Unauthorized),
            (
                KernelError::BoardStale { current: None },
                ErrorCode::Conflict,
            ),
            (
                KernelError::BoardFull {
                    scope: "bot:b".into(),
                },
                ErrorCode::Conflict,
            ),
            (
                KernelError::BudgetExhausted {
                    bot: "keeper".into(),
                    stop: reno_proto::BudgetStop {
                        kind: reno_proto::BudgetKind::Tokens,
                        limit: 10,
                        used: 12,
                    },
                },
                ErrorCode::BudgetExceeded,
            ),
            (
                KernelError::NoExecutor("b".into()),
                ErrorCode::InvalidParams,
            ),
            (
                KernelError::TriggerNotFound("t".into()),
                ErrorCode::NotFound,
            ),
            (
                KernelError::ExecutorNotFound("codex".parse().expect("id")),
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
            (KernelError::WorkspaceNotFound(run()), ErrorCode::NotFound),
            (
                KernelError::Workspace(WorkspaceError::NotAGitRepo { dir: "/x".into() }),
                ErrorCode::InvalidParams,
            ),
            (
                KernelError::Workspace(WorkspaceError::NoCommits { repo: "/x".into() }),
                ErrorCode::InvalidParams,
            ),
            (
                KernelError::Workspace(WorkspaceError::BareRepository { repo: "/x".into() }),
                ErrorCode::InvalidParams,
            ),
            (
                KernelError::Workspace(WorkspaceError::Unsafe("x".into())),
                ErrorCode::InvalidParams,
            ),
            (
                KernelError::Workspace(WorkspaceError::PathTaken { path: "/x".into() }),
                ErrorCode::Conflict,
            ),
            (
                KernelError::Workspace(WorkspaceError::InUse(run())),
                ErrorCode::Conflict,
            ),
            (
                KernelError::Workspace(WorkspaceError::Failed("x".into())),
                ErrorCode::Internal,
            ),
        ];
        for (error, code) in cases {
            assert_eq!(code_of(&error), code, "{error}");
            assert_eq!(kernel_error(&error).code, code, "{error}");
        }
    }

    #[test]
    fn a_stale_board_write_carries_the_entry_as_it_stands() {
        let entry = reno_proto::BoardEntry {
            key: "plan".into(),
            value: Some("v1".into()),
            version: 3,
            size: 2,
            updated_at_ms: 9,
        };
        let object = kernel_error(&KernelError::BoardStale {
            current: Some(Box::new(entry.clone())),
        });
        assert_eq!(object.code, ErrorCode::Conflict);
        let data: reno_proto::BoardConflict =
            serde_json::from_value(object.data.expect("data")).expect("typed");
        assert_eq!(data.current, Some(entry));
        // A key that does not exist yet says so.
        let object = kernel_error(&KernelError::BoardStale { current: None });
        let data: reno_proto::BoardConflict =
            serde_json::from_value(object.data.expect("data")).expect("typed");
        assert_eq!(data.current, None);
    }

    #[test]
    fn every_refused_token_reads_the_same() {
        let object = kernel_error(&KernelError::AgentDenied);
        assert_eq!(object.code, ErrorCode::Unauthorized);
        assert!(object.data.is_none());
    }

    #[test]
    fn a_budget_refusal_carries_its_typed_payload() {
        let stop = reno_proto::BudgetStop {
            kind: reno_proto::BudgetKind::Tokens,
            limit: 10,
            used: 12,
        };
        let object = kernel_error(&KernelError::BudgetExhausted {
            bot: "keeper".into(),
            stop,
        });
        let data: reno_proto::BudgetRefusal =
            serde_json::from_value(object.data.expect("data")).expect("typed");
        assert_eq!((data.bot.as_str(), data.stop), ("keeper", stop));
    }

    #[test]
    fn every_install_error_has_its_code() {
        let id = || -> reno_proto::ExecutorId { "acp.hermes".parse().expect("id") };
        let cases: Vec<(InstallError, ErrorCode)> = vec![
            (InstallError::NotFound(id()), ErrorCode::NotFound),
            (
                InstallError::NothingToInstall {
                    id: id(),
                    state: "it is installed".into(),
                },
                ErrorCode::Conflict,
            ),
            (InstallError::InProgress(id()), ErrorCode::Conflict),
            (
                InstallError::Failed {
                    id: id(),
                    detail: "registry unreachable".into(),
                },
                ErrorCode::Unavailable,
            ),
            (
                InstallError::Broken {
                    id: id(),
                    detail: "x".into(),
                },
                ErrorCode::Internal,
            ),
            (InstallError::ShuttingDown, ErrorCode::Unavailable),
            (
                InstallError::Kernel(KernelError::ShuttingDown),
                ErrorCode::Unavailable,
            ),
            (
                InstallError::Kernel(KernelError::ExecutorNotFound(id())),
                ErrorCode::NotFound,
            ),
        ];
        for (error, code) in cases {
            assert_eq!(install_code_of(&error), code, "{error}");
            assert_eq!(install_error(&error).code, code, "{error}");
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
