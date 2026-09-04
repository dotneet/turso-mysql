// Copyright 2026 the Turso authors. All rights reserved. MIT license.

//! Standalone, explicit account-store initialization and crash reconciliation.

#[cfg(unix)]
mod secret;

#[cfg(unix)]
use std::{
    collections::HashSet,
    fmt,
    path::PathBuf,
    process::ExitCode,
    time::{Duration, Instant},
};

#[cfg(unix)]
use clap::{error::ErrorKind, ArgAction, ArgGroup, Args, Parser, Subcommand};
#[cfg(unix)]
use turso_mysql::canonicalize_database_name;
#[cfg(unix)]
use turso_mysql_checkpoint_authority::{
    AuthorityId, UnixCheckpointAuthorityClient, UnixCheckpointAuthorityClientConfig,
};
#[cfg(unix)]
use turso_mysql_parser::MySqlTableName;
#[cfg(unix)]
use turso_mysql_server::{
    provision_account, validate_username, CheckpointAuthorityId, CheckpointReadError,
    CrashSafeReconcileOutcome, DatabaseGrant, DatabasePrivileges, GlobalPrivileges,
    OfflineAccountProvisioner, OfflineProvisioningError, PersistentAccountStoreError,
    ProtectedPassword, ProvisionedAccount, TableGrant, TablePrivileges,
};

#[cfg(unix)]
use crate::secret::{read_password, SecretSource};

#[cfg(unix)]
const MAX_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);
#[cfg(unix)]
const MAX_ACCOUNT_TABLE_GRANTS: usize = u16::MAX as usize;

#[cfg(unix)]
#[derive(Debug, Parser)]
#[command(name = "turso-mysql-offline-provision")]
#[command(about = "Initialize or reconcile an offline MySQL account store")]
struct Arguments {
    #[command(flatten)]
    global: GlobalArguments,

    #[command(subcommand)]
    command: Command,
}

#[cfg(unix)]
#[derive(Debug, Args)]
struct GlobalArguments {
    /// Existing private directory holding the account-store snapshot.
    #[arg(long)]
    account_store_root: PathBuf,

    /// Opaque identifier for the external checkpoint authority.
    #[arg(long)]
    authority_id: String,

    /// Absolute Unix socket path for the checkpoint authority.
    #[arg(long)]
    authority_socket: PathBuf,

    /// Effective UID expected for the checkpoint-authority service.
    #[arg(long)]
    authority_service_uid: u32,

    /// Bound for one authority RPC in milliseconds.
    #[arg(long)]
    authority_rpc_timeout_ms: u64,

    /// Bound for provisioning-lock waits and reconcile checkpoint reads in milliseconds.
    #[arg(long)]
    coordination_timeout_ms: u64,
}

#[cfg(unix)]
#[derive(Debug, Subcommand)]
enum Command {
    /// Create the first durable account generation and checkpoint it.
    Initialize(AccountArguments),
    /// Add one account through a durable replacement journal.
    AddAccount(AccountArguments),
    /// Reconcile a retained provisioning journal after an interrupted run.
    Reconcile,
}

#[cfg(unix)]
#[derive(Debug, Args)]
#[command(group(
    ArgGroup::new("password_source")
        .required(true)
        .multiple(false)
        .args(["password_tty", "password_stdin", "password_fd"])
))]
struct AccountArguments {
    /// Account name for the account generation.
    #[arg(long)]
    username: String,

    /// Whether the account can connect before selecting a database.
    #[arg(long, action = ArgAction::Set, required = true)]
    global_connect: bool,

    /// Whether the account can list databases.
    #[arg(long, action = ArgAction::Set, required = true)]
    global_list: bool,

    /// Whether the account starts disabled.
    #[arg(long, action = ArgAction::Set, required = true)]
    disabled: bool,

    /// Grant database permissions as DATABASE:PERMISSION[,PERMISSION...].
    #[arg(long = "database-grant", value_name = "DATABASE:PERMISSIONS")]
    database_grants: Vec<String>,

    /// Grant table SELECT permission as DATABASE.TABLE:select.
    #[arg(long = "table-grant", value_name = "DATABASE.TABLE:PERMISSION")]
    table_grants: Vec<String>,

    /// Read the password from the controlling terminal without echo.
    #[arg(long, group = "password_source")]
    password_tty: bool,

    /// Read the password from standard input.
    #[arg(long, group = "password_source")]
    password_stdin: bool,

    /// Read the password from this inherited file descriptor.
    #[arg(long, group = "password_source", allow_hyphen_values = true)]
    password_fd: Option<i32>,

    /// Permit an empty password from the selected source.
    #[arg(long)]
    allow_empty_password: bool,

    /// Bound for reading the password from its selected input source in milliseconds.
    #[arg(long)]
    password_input_timeout_ms: u64,
}

#[cfg(unix)]
impl AccountArguments {
    fn password_source(&self) -> Result<SecretSource, CommandError> {
        match (self.password_tty, self.password_stdin, self.password_fd) {
            (true, false, None) => Ok(SecretSource::Tty),
            (false, true, None) => Ok(SecretSource::Stdin),
            (false, false, Some(fd)) if fd >= 0 => Ok(SecretSource::Fd(fd)),
            _ => Err(CommandError::Input),
        }
    }

    fn database_grants(&self) -> Result<Vec<DatabaseGrantSpec>, CommandError> {
        let mut grants = Vec::with_capacity(self.database_grants.len());
        let mut databases = HashSet::with_capacity(self.database_grants.len());
        for value in &self.database_grants {
            let (database, permissions) = value.split_once(':').ok_or(CommandError::Input)?;
            if database.is_empty()
                || canonicalize_database_name(database).ok().as_deref() != Some(database)
                || !databases.insert(database.to_owned())
            {
                return Err(CommandError::Input);
            }
            let privileges = parse_database_privileges(permissions)?;
            grants.push(DatabaseGrantSpec {
                database: database.to_owned(),
                connect: permissions
                    .split(',')
                    .any(|permission| permission == "connect"),
                privileges,
            });
        }
        Ok(grants)
    }

    fn table_grants(&self) -> Result<Vec<TableGrantSpec>, CommandError> {
        if self.table_grants.len() > MAX_ACCOUNT_TABLE_GRANTS {
            return Err(CommandError::Input);
        }
        let mut grants = Vec::with_capacity(self.table_grants.len());
        let mut tables = HashSet::with_capacity(self.table_grants.len());
        for value in &self.table_grants {
            let (qualified_name, permission) = value.split_once(':').ok_or(CommandError::Input)?;
            if permission != "select" {
                return Err(CommandError::Input);
            }
            let (database, table) = qualified_name.split_once('.').ok_or(CommandError::Input)?;
            if database.is_empty()
                || table.is_empty()
                || table.contains('.')
                || canonicalize_database_name(database).ok().as_deref() != Some(database)
            {
                return Err(CommandError::Input);
            }
            let canonical_table = MySqlTableName::parse(table).map_err(|_| CommandError::Input)?;
            if canonical_table.as_str() != table
                || !tables.insert((database.to_owned(), table.to_owned()))
            {
                return Err(CommandError::Input);
            }
            grants.push(TableGrantSpec {
                database: database.to_owned(),
                table: table.to_owned(),
            });
        }
        Ok(grants)
    }
}

#[cfg(unix)]
struct DatabaseGrantSpec {
    database: String,
    connect: bool,
    privileges: DatabasePrivileges,
}

#[cfg(unix)]
struct TableGrantSpec {
    database: String,
    table: String,
}

#[cfg(unix)]
fn parse_database_privileges(value: &str) -> Result<DatabasePrivileges, CommandError> {
    if value.is_empty() {
        return Err(CommandError::Input);
    }
    let mut connect = false;
    let mut query = false;
    let mut create = false;
    let mut drop = false;
    for permission in value.split(',') {
        let slot = match permission {
            "connect" => &mut connect,
            "query" => &mut query,
            "create" => &mut create,
            "drop" => &mut drop,
            _ => return Err(CommandError::Input),
        };
        if *slot {
            return Err(CommandError::Input);
        }
        *slot = true;
    }
    Ok(DatabasePrivileges::new(connect, query, create, drop))
}

#[cfg(unix)]
fn validate_table_grant_scope(
    global_connect: bool,
    database_grants: &[DatabaseGrantSpec],
    table_grants: &[TableGrantSpec],
) -> Result<(), CommandError> {
    if table_grants.is_empty() {
        return Ok(());
    }
    if !global_connect {
        return Err(CommandError::Input);
    }
    let connected_databases = database_grants
        .iter()
        .filter(|grant| grant.connect)
        .map(|grant| grant.database.as_str())
        .collect::<HashSet<_>>();
    if table_grants
        .iter()
        .any(|table| !connected_databases.contains(table.database.as_str()))
    {
        return Err(CommandError::Input);
    }
    Ok(())
}

#[cfg(unix)]
fn main() -> ExitCode {
    if !cfg!(any(target_os = "linux", target_os = "macos")) {
        eprintln!("turso-mysql-offline-provision is unsupported on this platform");
        return ExitCode::from(CommandError::LocalState.exit_code());
    }

    let arguments = match Arguments::try_parse() {
        Ok(arguments) => arguments,
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) =>
        {
            let _ = error.print();
            return ExitCode::SUCCESS;
        }
        Err(_) => {
            eprintln!("offline provisioning input is invalid");
            return ExitCode::from(CommandError::Input.exit_code());
        }
    };

    match run(arguments) {
        Ok(()) => {
            println!("offline provisioning completed");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(error.exit_code())
        }
    }
}

#[cfg(not(unix))]
fn main() -> std::process::ExitCode {
    eprintln!("turso-mysql-offline-provision is unsupported on this platform");
    std::process::ExitCode::from(3)
}

#[cfg(unix)]
fn run(arguments: Arguments) -> Result<(), CommandError> {
    let configuration = Configuration::from_global(arguments.global)?;
    let mut authority = UnixCheckpointAuthorityClient::new(configuration.authority_client.clone())
        .map_err(|_| CommandError::Authority)?;

    match arguments.command {
        Command::Initialize(arguments) => initialize(arguments, configuration, &mut authority),
        Command::AddAccount(arguments) => add_account(arguments, configuration, &mut authority),
        Command::Reconcile => {
            let deadline = Instant::now()
                .checked_add(configuration.coordination_timeout)
                .ok_or(CommandError::Input)?;
            reconcile(configuration, &mut authority, deadline)
        }
    }
}

#[cfg(unix)]
fn initialize(
    arguments: AccountArguments,
    configuration: Configuration,
    authority: &mut UnixCheckpointAuthorityClient,
) -> Result<(), CommandError> {
    let (account, database_grants, table_grants) = provisioned_account(arguments)?;
    let mut builder = account.into_builder();
    for grant in database_grants {
        builder.add_grant(grant);
    }
    for grant in table_grants {
        builder.add_table_grant(grant);
    }
    let deadline = Instant::now()
        .checked_add(configuration.coordination_timeout)
        .ok_or(CommandError::Input)?;
    OfflineAccountProvisioner::initialize_crash_safe(
        configuration.account_store_root,
        configuration.checkpoint_authority,
        builder,
        authority,
        deadline,
    )
    .map(|_| ())
    .map_err(map_provisioning_error)
}

#[cfg(unix)]
fn add_account(
    arguments: AccountArguments,
    configuration: Configuration,
    authority: &mut UnixCheckpointAuthorityClient,
) -> Result<(), CommandError> {
    let (account, database_grants, table_grants) = provisioned_account(arguments)?;
    let deadline = Instant::now()
        .checked_add(configuration.coordination_timeout)
        .ok_or(CommandError::Input)?;
    OfflineAccountProvisioner::add_account_with_grants_crash_safe(
        configuration.account_store_root,
        configuration.checkpoint_authority,
        account,
        database_grants,
        table_grants,
        authority,
        deadline,
    )
    .map(|_| ())
    .map_err(map_provisioning_error)
}

#[cfg(unix)]
fn provisioned_account(
    arguments: AccountArguments,
) -> Result<(ProvisionedAccount, Vec<DatabaseGrant>, Vec<TableGrant>), CommandError> {
    validate_username(&arguments.username).map_err(|_| CommandError::Input)?;
    let database_specs = arguments.database_grants()?;
    let table_specs = arguments.table_grants()?;
    validate_table_grant_scope(arguments.global_connect, &database_specs, &table_specs)?;
    let source = arguments.password_source()?;
    let password_input_timeout = duration_from_millis(arguments.password_input_timeout_ms)?;
    let password_deadline = Instant::now()
        .checked_add(password_input_timeout)
        .ok_or(CommandError::Input)?;
    let mut password = read_password(source, arguments.allow_empty_password, password_deadline)
        .map_err(|_| CommandError::Input)?;
    let account = provision_account(
        arguments.username,
        ProtectedPassword::new(password.as_mut_slice()),
        !arguments.disabled,
        GlobalPrivileges::new(arguments.global_connect, arguments.global_list),
    )
    .map_err(map_provisioning_error)?;
    let database_grants = database_specs
        .into_iter()
        .map(|grant| account.grant(grant.database, grant.privileges))
        .collect();
    let table_grants = table_specs
        .into_iter()
        .map(|grant| account.table_grant(grant.database, grant.table, TablePrivileges::new(true)))
        .collect();
    Ok((account, database_grants, table_grants))
}

#[cfg(unix)]
fn reconcile(
    configuration: Configuration,
    authority: &mut UnixCheckpointAuthorityClient,
    deadline: Instant,
) -> Result<(), CommandError> {
    match OfflineAccountProvisioner::reconcile_crash_safe(
        configuration.account_store_root,
        &configuration.checkpoint_authority,
        authority,
        deadline,
    )
    .map_err(map_provisioning_error)?
    {
        CrashSafeReconcileOutcome::NoPendingUpdate
        | CrashSafeReconcileOutcome::AbortedBeforeSnapshot
        | CrashSafeReconcileOutcome::Reconciled { .. } => Ok(()),
    }
}

#[cfg(unix)]
struct Configuration {
    account_store_root: PathBuf,
    checkpoint_authority: CheckpointAuthorityId,
    authority_client: UnixCheckpointAuthorityClientConfig,
    coordination_timeout: Duration,
}

#[cfg(unix)]
impl Configuration {
    fn from_global(arguments: GlobalArguments) -> Result<Self, CommandError> {
        let checkpoint_authority = CheckpointAuthorityId::new(arguments.authority_id.clone())
            .map_err(|_| CommandError::Input)?;
        let authority =
            AuthorityId::new(arguments.authority_id).map_err(|_| CommandError::Input)?;
        let authority_rpc_timeout = duration_from_millis(arguments.authority_rpc_timeout_ms)?;
        let coordination_timeout = duration_from_millis(arguments.coordination_timeout_ms)?;
        let authority_client = UnixCheckpointAuthorityClientConfig::new(
            arguments.authority_socket,
            authority,
            arguments.authority_service_uid,
            authority_rpc_timeout,
        )
        .map_err(|_| CommandError::Input)?;
        Ok(Self {
            account_store_root: arguments.account_store_root,
            checkpoint_authority,
            authority_client,
            coordination_timeout,
        })
    }
}

#[cfg(unix)]
fn duration_from_millis(milliseconds: u64) -> Result<Duration, CommandError> {
    let duration = Duration::from_millis(milliseconds);
    if duration.is_zero() || duration > MAX_TIMEOUT {
        return Err(CommandError::Input);
    }
    Ok(duration)
}

#[cfg(unix)]
fn map_provisioning_error(error: OfflineProvisioningError) -> CommandError {
    match error {
        OfflineProvisioningError::InvalidUsername(_)
        | OfflineProvisioningError::GrantOwnerMismatch => CommandError::Input,
        OfflineProvisioningError::Store(PersistentAccountStoreError::InvalidGeneration) => {
            CommandError::Input
        }
        OfflineProvisioningError::RandomUnavailable
        | OfflineProvisioningError::Store(
            PersistentAccountStoreError::Unavailable
            | PersistentAccountStoreError::MissingSnapshot
            | PersistentAccountStoreError::InvalidSnapshot
            | PersistentAccountStoreError::ProvisioningBusy,
        )
        | OfflineProvisioningError::ProvisioningBusy => CommandError::LocalState,
        OfflineProvisioningError::CheckpointRead(
            CheckpointReadError::Missing
            | CheckpointReadError::Unavailable
            | CheckpointReadError::TimedOut
            | CheckpointReadError::Invalid,
        ) => CommandError::Authority,
        OfflineProvisioningError::Store(
            PersistentAccountStoreError::AlreadyInitialized
            | PersistentAccountStoreError::Conflict
            | PersistentAccountStoreError::CheckpointMismatch,
        )
        | OfflineProvisioningError::CheckpointMismatch
        | OfflineProvisioningError::ReconciliationRequired(_)
        | OfflineProvisioningError::CheckpointConflict(_)
        | OfflineProvisioningError::CheckpointFailed(_)
        | OfflineProvisioningError::CheckpointAmbiguous(_)
        | OfflineProvisioningError::PendingJournalInvalid
        | OfflineProvisioningError::PendingAuthorityMismatch => CommandError::Reconciliation,
    }
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CommandError {
    Input,
    LocalState,
    Authority,
    Reconciliation,
}

#[cfg(unix)]
impl CommandError {
    const fn exit_code(self) -> u8 {
        match self {
            Self::Input => 2,
            Self::LocalState => 3,
            Self::Authority => 4,
            Self::Reconciliation => 5,
        }
    }
}

#[cfg(unix)]
impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Input => f.write_str("offline provisioning input is invalid"),
            Self::LocalState => f.write_str("offline provisioning local state is invalid"),
            Self::Authority => f.write_str("checkpoint authority is unavailable"),
            Self::Reconciliation => f.write_str("offline provisioning requires reconciliation"),
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use clap::Parser;
    use turso_mysql_server::{
        CheckpointReadError, OfflineProvisioningError, PersistentAccountStoreError,
    };

    use super::{
        duration_from_millis, map_provisioning_error, parse_database_privileges,
        provisioned_account, validate_table_grant_scope, AccountArguments, Arguments, Command,
        CommandError, MAX_ACCOUNT_TABLE_GRANTS,
    };

    const GLOBAL: [&str; 12] = [
        "--account-store-root",
        "/var/lib/turso/accounts",
        "--authority-id",
        "account-store",
        "--authority-socket",
        "/run/turso/checkpoint.sock",
        "--authority-service-uid",
        "1001",
        "--authority-rpc-timeout-ms",
        "250",
        "--coordination-timeout-ms",
        "500",
    ];

    const ACCOUNT: [&str; 12] = [
        "--username",
        "admin",
        "--global-connect",
        "true",
        "--global-list",
        "false",
        "--disabled",
        "false",
        "--password-stdin",
        "--password-input-timeout-ms",
        "100",
        "--allow-empty-password",
    ];

    fn account_arguments(command: &'static str, extra: &[&'static str]) -> AccountArguments {
        let mut arguments = vec!["turso-mysql-offline-provision"];
        arguments.extend(GLOBAL);
        arguments.push(command);
        arguments.extend(ACCOUNT);
        arguments.extend(extra);
        match Arguments::try_parse_from(arguments)
            .expect("account input should parse")
            .command
        {
            Command::Initialize(arguments) | Command::AddAccount(arguments) => arguments,
            Command::Reconcile => panic!("account command parsed as reconcile"),
        }
    }

    #[test]
    fn initialize_requires_every_explicit_global_and_account_option() {
        let mut arguments = vec!["turso-mysql-offline-provision"];
        arguments.extend(GLOBAL);
        arguments.extend([
            "initialize",
            "--username",
            "admin",
            "--global-connect",
            "true",
            "--global-list",
            "false",
            "--disabled",
            "false",
            "--password-stdin",
            "--password-input-timeout-ms",
            "100",
        ]);
        let parsed = Arguments::try_parse_from(arguments).expect("explicit input should parse");
        assert!(matches!(parsed.command, Command::Initialize(_)));

        assert!(Arguments::try_parse_from([
            "turso-mysql-offline-provision",
            "initialize",
            "--username",
            "admin",
            "--global-connect",
            "true",
            "--global-list",
            "false",
            "--disabled",
            "false",
            "--password-stdin",
            "--password-input-timeout-ms",
            "100",
        ])
        .is_err());

        let mut missing_password_timeout = vec!["turso-mysql-offline-provision"];
        missing_password_timeout.extend(GLOBAL);
        missing_password_timeout.extend([
            "initialize",
            "--username",
            "admin",
            "--global-connect",
            "true",
            "--global-list",
            "false",
            "--disabled",
            "false",
            "--password-stdin",
        ]);
        assert!(Arguments::try_parse_from(missing_password_timeout).is_err());
    }

    #[test]
    fn initialize_rejects_implicit_or_multiple_password_sources() {
        let mut missing = vec!["turso-mysql-offline-provision"];
        missing.extend(GLOBAL);
        missing.extend([
            "initialize",
            "--username",
            "admin",
            "--global-connect",
            "true",
            "--global-list",
            "false",
            "--disabled",
            "false",
            "--password-input-timeout-ms",
            "100",
        ]);
        assert!(Arguments::try_parse_from(missing).is_err());

        let mut multiple = vec!["turso-mysql-offline-provision"];
        multiple.extend(GLOBAL);
        multiple.extend([
            "initialize",
            "--username",
            "admin",
            "--global-connect",
            "true",
            "--global-list",
            "false",
            "--disabled",
            "false",
            "--password-stdin",
            "--password-tty",
            "--password-input-timeout-ms",
            "100",
        ]);
        assert!(Arguments::try_parse_from(multiple).is_err());
    }

    #[test]
    fn add_account_requires_every_explicit_global_and_account_option() {
        let arguments = account_arguments("add-account", &[]);
        assert_eq!(arguments.username, "admin");

        assert!(Arguments::try_parse_from([
            "turso-mysql-offline-provision",
            "add-account",
            "--username",
            "admin",
            "--global-connect",
            "true",
            "--global-list",
            "false",
            "--disabled",
            "false",
            "--password-stdin",
            "--password-input-timeout-ms",
            "100",
        ])
        .is_err());
    }

    #[test]
    fn initialize_and_add_account_share_database_grant_arguments() {
        for command in ["initialize", "add-account"] {
            let arguments = account_arguments(
                command,
                &[
                    "--database-grant",
                    "reports:connect,query",
                    "--database-grant",
                    "archive:create,drop",
                    "--table-grant",
                    "reports.records:select",
                ],
            );
            assert_eq!(arguments.database_grants().unwrap().len(), 2);
            assert_eq!(arguments.table_grants().unwrap().len(), 1);
        }
    }

    #[test]
    fn invalid_username_is_rejected_before_password_read() {
        let mut arguments = account_arguments("initialize", &[]);
        arguments.username.clear();
        assert!(matches!(
            provisioned_account(arguments),
            Err(CommandError::Input)
        ));
    }

    #[test]
    fn add_account_rejects_implicit_or_multiple_password_sources() {
        let mut missing = vec!["turso-mysql-offline-provision"];
        missing.extend(GLOBAL);
        missing.extend([
            "add-account",
            "--username",
            "admin",
            "--global-connect",
            "true",
            "--global-list",
            "false",
            "--disabled",
            "false",
            "--password-input-timeout-ms",
            "100",
        ]);
        assert!(Arguments::try_parse_from(missing).is_err());

        let mut multiple = vec!["turso-mysql-offline-provision"];
        multiple.extend(GLOBAL);
        multiple.extend([
            "add-account",
            "--username",
            "admin",
            "--global-connect",
            "true",
            "--global-list",
            "false",
            "--disabled",
            "false",
            "--password-stdin",
            "--password-fd",
            "7",
            "--password-input-timeout-ms",
            "100",
        ]);
        assert!(Arguments::try_parse_from(multiple).is_err());
    }

    #[test]
    fn account_commands_require_an_explicit_password_timeout() {
        for command in ["initialize", "add-account"] {
            let mut arguments = vec!["turso-mysql-offline-provision"];
            arguments.extend(GLOBAL);
            arguments.extend([
                command,
                "--username",
                "admin",
                "--global-connect",
                "true",
                "--global-list",
                "false",
                "--disabled",
                "false",
                "--password-stdin",
            ]);
            assert!(
                Arguments::try_parse_from(arguments).is_err(),
                "{command} should fail"
            );
        }
    }

    #[test]
    fn account_commands_require_explicit_global_privilege_values() {
        for command in ["initialize", "add-account"] {
            let mut arguments = vec!["turso-mysql-offline-provision"];
            arguments.extend(GLOBAL);
            arguments.extend([
                command,
                "--username",
                "admin",
                "--global-connect",
                "--global-list",
                "false",
                "--disabled",
                "false",
                "--password-stdin",
                "--password-input-timeout-ms",
                "100",
            ]);
            assert!(
                Arguments::try_parse_from(arguments).is_err(),
                "{command} should fail"
            );
        }
    }

    #[test]
    fn grants_require_canonical_lowercase_database_names() {
        for grant in ["Reports:query", "mysql:query", "bad/name:query", ":query"] {
            let arguments = account_arguments("initialize", &["--database-grant", grant]);
            assert!(arguments.database_grants().is_err(), "{grant} should fail");
        }
    }

    #[test]
    fn grants_require_database_and_permission_separator() {
        for grant in ["reports", "reports:", "reports:query:", "reports:,query"] {
            let arguments = account_arguments("add-account", &["--database-grant", grant]);
            assert!(arguments.database_grants().is_err(), "{grant} should fail");
        }
    }

    #[test]
    fn grants_require_exact_lowercase_permissions() {
        for grant in ["reports:Query", "reports:select", "reports:query,unknown"] {
            let arguments = account_arguments("initialize", &["--database-grant", grant]);
            assert!(arguments.database_grants().is_err(), "{grant} should fail");
        }
    }

    #[test]
    fn grants_reject_duplicate_permissions() {
        let arguments =
            account_arguments("add-account", &["--database-grant", "reports:query,query"]);
        assert!(arguments.database_grants().is_err());
    }

    #[test]
    fn grants_reject_duplicate_databases_before_password_read() {
        let arguments = account_arguments(
            "initialize",
            &[
                "--database-grant",
                "reports:query",
                "--database-grant",
                "reports:create",
            ],
        );
        assert!(arguments.database_grants().is_err());
    }

    #[test]
    fn table_grants_require_canonical_names_and_select_permission() {
        for grant in [
            "Reports.records:select",
            "reports.Records:select",
            "reports.records:SELECT",
            "reports.records:query",
            "reports.records",
            "reports.records:select:extra",
            "reports.records.other:select",
            ".records:select",
            "reports.:select",
        ] {
            let arguments = account_arguments("initialize", &["--table-grant", grant]);
            assert!(arguments.table_grants().is_err(), "{grant} should fail");
        }
    }

    #[test]
    fn table_grants_reject_duplicates_and_excessive_counts() {
        let duplicate = account_arguments(
            "add-account",
            &[
                "--table-grant",
                "reports.records:select",
                "--table-grant",
                "reports.records:select",
            ],
        );
        assert!(duplicate.table_grants().is_err());

        let mut excessive = account_arguments("initialize", &[]);
        excessive.table_grants = (0..=MAX_ACCOUNT_TABLE_GRANTS)
            .map(|index| format!("reports.table{index}:select"))
            .collect();
        assert!(excessive.table_grants().is_err());
    }

    #[test]
    fn table_grants_require_global_and_matching_database_connect_without_implicit_grants() {
        let table_only =
            account_arguments("initialize", &["--table-grant", "reports.records:select"]);
        let tables = table_only.table_grants().unwrap();
        assert!(validate_table_grant_scope(true, &[], &tables).is_err());

        let query_only = account_arguments(
            "initialize",
            &[
                "--database-grant",
                "reports:query",
                "--table-grant",
                "reports.records:select",
            ],
        );
        let databases = query_only.database_grants().unwrap();
        let tables = query_only.table_grants().unwrap();
        assert!(validate_table_grant_scope(true, &databases, &tables).is_err());

        let not_global = account_arguments(
            "initialize",
            &[
                "--database-grant",
                "reports:connect",
                "--table-grant",
                "reports.records:select",
            ],
        );
        let databases = not_global.database_grants().unwrap();
        let tables = not_global.table_grants().unwrap();
        assert!(validate_table_grant_scope(false, &databases, &tables).is_err());

        let valid = account_arguments(
            "initialize",
            &[
                "--database-grant",
                "reports:connect",
                "--table-grant",
                "reports.records:select",
            ],
        );
        let databases = valid.database_grants().unwrap();
        let tables = valid.table_grants().unwrap();
        assert!(validate_table_grant_scope(true, &databases, &tables).is_ok());
    }

    #[test]
    fn password_fd_must_be_nonnegative() {
        let mut arguments = vec!["turso-mysql-offline-provision"];
        arguments.extend(GLOBAL);
        arguments.extend([
            "add-account",
            "--username",
            "admin",
            "--global-connect",
            "true",
            "--global-list",
            "false",
            "--disabled",
            "false",
            "--password-fd",
            "-1",
            "--password-input-timeout-ms",
            "100",
        ]);
        let parsed =
            Arguments::try_parse_from(arguments).expect("input should parse before validation");
        let Command::AddAccount(arguments) = parsed.command else {
            panic!("add-account should parse as add-account");
        };
        assert!(arguments.password_source().is_err());
    }

    #[test]
    fn reconcile_rejects_account_arguments() {
        let mut arguments = vec!["turso-mysql-offline-provision"];
        arguments.extend(GLOBAL);
        arguments.extend(["reconcile", "--username", "admin"]);
        assert!(Arguments::try_parse_from(arguments).is_err());
    }

    #[test]
    fn reconcile_accepts_only_global_arguments() {
        let mut arguments = vec!["turso-mysql-offline-provision"];
        arguments.extend(GLOBAL);
        arguments.push("reconcile");
        let parsed = Arguments::try_parse_from(arguments).expect("reconcile input should parse");
        assert!(matches!(parsed.command, Command::Reconcile));
    }

    #[test]
    fn each_database_permission_is_accepted_once() {
        assert!(parse_database_privileges("connect,query,create,drop").is_ok());
    }

    #[test]
    fn provisioning_errors_have_fixed_exit_categories() {
        assert_eq!(
            map_provisioning_error(OfflineProvisioningError::CheckpointRead(
                CheckpointReadError::TimedOut
            )),
            CommandError::Authority
        );
        assert_eq!(
            map_provisioning_error(OfflineProvisioningError::PendingJournalInvalid),
            CommandError::Reconciliation
        );
        assert_eq!(
            map_provisioning_error(OfflineProvisioningError::CheckpointMismatch),
            CommandError::Reconciliation
        );
        assert_eq!(
            map_provisioning_error(OfflineProvisioningError::Store(
                PersistentAccountStoreError::InvalidGeneration
            )),
            CommandError::Input
        );
        assert_eq!(CommandError::Input.exit_code(), 2);
        assert_eq!(CommandError::LocalState.exit_code(), 3);
        assert_eq!(CommandError::Authority.exit_code(), 4);
        assert_eq!(CommandError::Reconciliation.exit_code(), 5);
    }

    #[test]
    fn password_timeout_must_be_nonzero_and_bounded() {
        assert!(duration_from_millis(0).is_err());
        assert!(duration_from_millis(24 * 60 * 60 * 1000 + 1).is_err());
        assert!(duration_from_millis(1).is_ok());
    }
}
