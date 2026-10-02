use super::Completion;
use crate::{CompletionError, LimboError, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnSyncFailure {
    Panic,
    ReturnError,
}

impl OnSyncFailure {
    pub fn for_data_sync_retry(data_sync_retry: bool) -> Self {
        if data_sync_retry {
            Self::ReturnError
        } else {
            Self::Panic
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum DurableFile {
    Wal,
    LogicalLog,
    Database,
}

impl DurableFile {
    fn name(self) -> &'static str {
        match self {
            Self::Wal => "WAL",
            Self::LogicalLog => "logical log",
            Self::Database => "database file",
        }
    }
}

pub(crate) fn sync_durable_file<F>(
    file: DurableFile,
    on_failure: OnSyncFailure,
    on_result: F,
    submit: impl FnOnce(Completion) -> Result<Completion>,
) -> Result<Completion>
where
    F: Fn(&std::result::Result<i32, CompletionError>) + Send + Sync + 'static,
{
    let completion = Completion::new_sync_with_after_failure(
        move |result| on_result(&result),
        move |err| sync_failed(file, on_failure, &err),
    );
    match submit(completion.clone()) {
        Ok(submitted) => Ok(submitted),
        Err(err) if completion.finished() => {
            if completion.succeeded() {
                sync_failed(file, on_failure, &err);
            }
            Err(err)
        }
        Err(err) => {
            completion.error(refused_sync_error(err));
            Ok(completion)
        }
    }
}

fn sync_failed(file: DurableFile, on_failure: OnSyncFailure, err: &dyn std::fmt::Display) {
    tracing::error!("fsync of the {} failed: {err}", file.name());
    if on_failure == OnSyncFailure::Panic {
        panic!(
            "fsync of the {} failed ({err}); what the file holds on disk is no longer known, \
             and a retried fsync could report success for data that never reached the disk",
            file.name()
        );
    }
}

fn refused_sync_error(err: LimboError) -> CompletionError {
    match err {
        LimboError::CompletionError(err) => err,
        other => {
            tracing::error!("fsync was refused: {other}");
            CompletionError::IOError(std::io::ErrorKind::Other, "sync")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{sync_durable_file, DurableFile, OnSyncFailure};
    use crate::{Completion, CompletionError, LimboError};
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::sync::{Arc, Mutex};

    #[test]
    fn a_sync_refused_when_submitted_fails_its_completion() {
        let results = Arc::new(Mutex::new(Vec::new()));
        let seen = results.clone();
        let completion = sync_durable_file(
            DurableFile::Wal,
            OnSyncFailure::ReturnError,
            move |result| seen.lock().unwrap().push(result.is_ok()),
            |_| Err(io_error()),
        )
        .unwrap();

        assert!(completion.failed());
        assert_eq!(*results.lock().unwrap(), vec![false]);
    }

    #[test]
    fn a_sync_refused_after_its_completion_reported_success_is_still_a_failure() {
        let refused = sync_durable_file(
            DurableFile::Database,
            OnSyncFailure::ReturnError,
            |_| {},
            |completion| {
                completion.complete(-1);
                Err(LimboError::ExtensionError("VFS is null".to_string()))
            },
        );
        assert!(refused.is_err());

        let panicked = catch_unwind(AssertUnwindSafe(|| {
            sync_durable_file(
                DurableFile::Database,
                OnSyncFailure::Panic,
                |_| {},
                |completion| {
                    completion.complete(-1);
                    Err(LimboError::ExtensionError("VFS is null".to_string()))
                },
            )
        }));
        assert!(panicked.is_err());
    }

    #[test]
    fn a_sync_that_fails_later_panics_only_after_its_completion_finished() {
        let submitted = Arc::new(Mutex::new(None));
        let pending = submitted.clone();
        let completion = sync_durable_file(
            DurableFile::LogicalLog,
            OnSyncFailure::Panic,
            |_| {},
            move |completion: Completion| {
                *pending.lock().unwrap() = Some(completion.clone());
                Ok(completion)
            },
        )
        .unwrap();
        assert!(!completion.finished());

        let failing = submitted.lock().unwrap().take().unwrap();
        let panicked = catch_unwind(AssertUnwindSafe(|| {
            failing.error(CompletionError::IOError(std::io::ErrorKind::Other, "sync"))
        }));

        assert!(panicked.is_err());
        assert!(completion.finished());
        assert!(completion.failed());
    }

    fn io_error() -> LimboError {
        LimboError::CompletionError(CompletionError::IOError(std::io::ErrorKind::Other, "sync"))
    }
}
