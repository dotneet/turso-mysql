//! Authorization ports for authenticated classic MySQL sessions.
//!
//! Authentication proves who connected. These ports let a server owner make
//! the separate authorization decision before a command executor is installed.

use std::{error::Error, fmt};

use crate::{
    AuthenticatedPrincipal, CommandExecutionOptions, CommandExecutor, FrontendErrorKind,
    InitialDatabaseSelector,
};

/// A database action that a policy may allow or reject.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatabaseAction<'a> {
    /// Establish a session, optionally selecting the requested database.
    Connect { database: Option<&'a str> },
    /// Execute a query against the selected database.
    Query { database: &'a str },
    /// Create the named logical database.
    Create { database: &'a str },
    /// Drop the named logical database.
    Drop { database: &'a str },
    /// List logical databases.
    List,
}

/// A table action that a policy may allow or reject.
///
/// Database query permission remains the wildcard for this API: an
/// authorizer with database `Query` permission may allow any table action in
/// that database. A table action is used when a policy grants access to a
/// narrower table scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableAction<'a> {
    /// Read rows from one table in the selected database.
    Select {
        /// Canonical logical database name.
        database: &'a str,
        /// Canonical table name.
        table: &'a str,
    },
}

/// A policy could not authorize a database action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorizationError {
    /// The principal is not allowed to perform the action.
    Denied,
    /// The policy backend could not make a decision.
    Unavailable,
}

impl fmt::Display for AuthorizationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Denied => f.write_str("database action denied"),
            Self::Unavailable => f.write_str("database authorization is unavailable"),
        }
    }
}

impl Error for AuthorizationError {}

/// Makes authorization decisions for an authenticated principal.
pub trait DatabaseAuthorizer {
    /// Authorizes one database action without exposing credential material.
    fn authorize(
        &self,
        principal: &AuthenticatedPrincipal,
        action: DatabaseAction<'_>,
    ) -> Result<(), AuthorizationError>;

    /// Authorizes one table action without exposing credential material.
    ///
    /// Implementations that do not support table-scoped grants fail closed.
    /// Existing authorizers that only use database-wide query permission stay
    /// source-compatible because callers can continue using [`Self::authorize`].
    fn authorize_table(
        &self,
        _principal: &AuthenticatedPrincipal,
        _action: TableAction<'_>,
    ) -> Result<(), AuthorizationError> {
        Err(AuthorizationError::Denied)
    }
}

/// A fail-closed policy for deployments that have not configured authorization.
#[derive(Debug, Default, Clone, Copy)]
pub struct DenyAllDatabaseAuthorizer;

impl DatabaseAuthorizer for DenyAllDatabaseAuthorizer {
    fn authorize(
        &self,
        _principal: &AuthenticatedPrincipal,
        _action: DatabaseAction<'_>,
    ) -> Result<(), AuthorizationError> {
        Err(AuthorizationError::Denied)
    }
}

/// A command executor that is safe to install only after authentication and
/// connection authorization have both succeeded.
pub trait AuthenticatedCommandExecutor: CommandExecutor + InitialDatabaseSelector {
    /// Performs the connection-level authorization check.
    fn authorize_connection(&mut self) -> Result<(), AuthorizationError>;
}

/// Creates the one executor owned by an authenticated connection.
pub trait AuthenticatedExecutorFactory {
    /// Executor type installed after a principal has been authenticated.
    type Executor: AuthenticatedCommandExecutor;

    /// Consumes this factory so it cannot be reused for another connection.
    fn build(self, principal: AuthenticatedPrincipal)
        -> Result<Self::Executor, AuthorizationError>;

    /// Builds an executor with the immutable options selected by negotiation.
    ///
    /// The default keeps existing factory implementations source-compatible;
    /// factories that implement affected-row semantics can override this
    /// method to retain the `CLIENT_FOUND_ROWS` choice in their executor.
    fn build_with_options(
        self,
        principal: AuthenticatedPrincipal,
        _options: CommandExecutionOptions,
    ) -> Result<Self::Executor, AuthorizationError>
    where
        Self: Sized,
    {
        self.build(principal)
    }
}

/// Converts authorization failures to the one client-safe frontend category.
pub(crate) const fn authorization_frontend_error(_: AuthorizationError) -> FrontendErrorKind {
    FrontendErrorKind::AccessDenied
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AccountId;

    #[test]
    fn deny_all_policy_rejects_every_database_action() {
        let principal =
            AuthenticatedPrincipal::from_account_id_for_testing(AccountId::from_bytes([0x2a; 32]));
        let authorizer = DenyAllDatabaseAuthorizer;
        let actions = [
            DatabaseAction::Connect { database: None },
            DatabaseAction::Connect {
                database: Some("tenant"),
            },
            DatabaseAction::Query { database: "tenant" },
            DatabaseAction::Create { database: "tenant" },
            DatabaseAction::Drop { database: "tenant" },
            DatabaseAction::List,
        ];
        for action in actions {
            assert_eq!(
                authorizer.authorize(&principal, action),
                Err(AuthorizationError::Denied)
            );
        }
        assert_eq!(
            authorizer.authorize_table(
                &principal,
                TableAction::Select {
                    database: "tenant",
                    table: "reports",
                },
            ),
            Err(AuthorizationError::Denied)
        );
    }

    #[test]
    fn authorization_failures_share_the_client_safe_frontend_category() {
        assert_eq!(
            authorization_frontend_error(AuthorizationError::Denied),
            FrontendErrorKind::AccessDenied
        );
        assert_eq!(
            authorization_frontend_error(AuthorizationError::Unavailable),
            FrontendErrorKind::AccessDenied
        );
    }
}
