//! Compare and sync maintainers from Nixpkgs to maintainers on
//! GitHub Maintainer team, as described in RFC #39:
//! https://github.com/NixOS/rfcs/blob/master/rfcs/0039-unprivileged-maintainer-teams.md

#![warn(missing_docs)]

#[macro_use]
extern crate slog;

#[macro_use]
extern crate serde;

#[macro_use]
extern crate lazy_static;

#[macro_use]
extern crate prometheus;

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use structopt::StructOpt;
mod cli;
use cli::{ExecMode, ExitError, Options};
mod maintainers;
use maintainers::MaintainerList;
mod filemunge;
mod maintainerhistory;
mod nix;
mod op_backfill;
mod op_blame_author;
mod op_check_handles;
mod op_sync_team;
use hubcaps::{Credentials, Github, InstallationTokenGenerator, JWTCredentials};
use std::thread;

/// Github Authentication information for the GitHub app.
/// When creating the application, the only permission it needs
/// is Members: Read and Write.
/// No access to code or other permissions is needed.
// NOTE: DO NOT MAKE "Debug"! This will leak secrets
#[derive(Deserialize)]
pub struct GitHubAuth {
    /// Overall GitHub Application ID, same for all users
    pub app_id: u64,

    /// DER RSA key. Generate with
    /// `openssl rsa -in private_rsa_key.pem -outform DER -out private_rsa_key.der`
    pub private_key_file: PathBuf,

    /// the ID of the installation of this app in to the repo or
    /// organization.
    pub installation_id: u64,
}

fn load_maintainer_file(logger: slog::Logger, src: &Path) -> Result<MaintainerList, ExitError> {
    let maintainers_file = src.canonicalize()?;

    info!(logger, "Loading maintainer information";
          "from" => src.display(),
          "absolute" => maintainers_file.display()
    );

    Ok(MaintainerList::load(logger.clone(), &maintainers_file)?)
}

fn execute_ops(logger: slog::Logger, inputs: Options) -> Result<(), ExitError> {
    // Note: I wanted these in a lazy_static!, but that meant metrics
    // which would report a 0 would never get reported at all, since
    // they aren't accessed.... and lazy_static! is lazy.
    let maintainer_nix_load_failure_counter = register_int_counter!(
        "maintainer_nix_load_failure",
        "Failures to load maintainers.nix"
    )
    .unwrap();

    let maintainers = load_maintainer_file(logger.new(o!()), &inputs.maintainers)
        .map_err(|d| {
            maintainer_nix_load_failure_counter.inc();
            d
        })
        .unwrap();

    let github_auth = nix::nix_instantiate_file_to_struct::<GitHubAuth>(
        logger.new(o!()),
        &inputs.credential_file,
    )
    .expect("Failed to parse the credential file");

    let mut private_key = Vec::new();
    File::open(&github_auth.private_key_file)
        .expect("Opening the private key file")
        .read_to_end(&mut private_key)
        .expect("Reading the private key");

    let github = Github::new(
        String::from("NixOS/rfcs#39 (hubcaps)"),
        Credentials::InstallationToken(InstallationTokenGenerator::new(
            github_auth.installation_id,
            JWTCredentials::new(github_auth.app_id, private_key).unwrap(),
        )),
    )
    .unwrap();

    match inputs.mode {
        ExecMode::CheckHandles => op_check_handles::check_handles(
            logger.new(o!("exec-mode" => "CheckHandles")),
            maintainers,
        ),
        ExecMode::BackfillIDs => op_backfill::backfill_ids(
            logger.new(o!("exec-mode" => "BackfillIDs")),
            github,
            &inputs.maintainers,
            maintainers,
        ),
        ExecMode::BlameAuthor => op_blame_author::report(
            logger.new(o!("exec-mode" => "BlameAuthor")),
            github,
            &inputs.maintainers,
            maintainers,
        ),
        ExecMode::SyncTeam(team_info) => op_sync_team::sync_team(
            logger.new(o!("exec-mode" => "SyncTeam")),
            github,
            maintainers,
            &team_info.organization,
            team_info.team_id,
            team_info.dry_run,
            team_info.limit,
        ),
        ExecMode::ListTeams(team_info) => op_sync_team::list_teams(github, &team_info.organization),
    }
}

fn main() {
    let op_success_counter =
        register_int_counter!("op_suceess_counter", "Execution completed without fault.").unwrap();
    let op_failed_counter =
        register_int_counter!("op_failure_counter", "Execution failed").unwrap();
    let op_panic_counter =
        register_int_counter!("op_panic_counter", "Execution of the operation panicked").unwrap();

    let (logger, _scopes) = rfc39::default_logger();

    let inputs = Options::from_args();

    let dump_metrics = inputs.dump_metrics;

    let handle = {
        let logger = logger.new(o!());
        thread::spawn(move || {
            execute_ops(logger, inputs).map(|ok| {
                op_success_counter.inc();
                ok
            })
        })
    };

    let thread_result: Result<Result<(), ExitError>, _> = handle.join().map_err(|thread_err| {
        warn!(logger, "Op-handling child panicked: {:#?}", thread_err);
        op_failed_counter.inc();
        op_panic_counter.inc();
        thread_err
    });

    if dump_metrics {
        use prometheus::Encoder;
        let mut buffer = Vec::<u8>::new();
        prometheus::default_registry();
        prometheus::TextEncoder::new()
            .encode(&prometheus::default_registry().gather(), &mut buffer)
            .unwrap();
        println!("metrics:\n {}", String::from_utf8(buffer).unwrap());
    }

    thread_result.unwrap().unwrap();
}
