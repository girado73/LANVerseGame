//! Operator commands: `migrate`, `token`, `mode`.

use anyhow::{Context, Result, bail};

use crate::db::Db;
use crate::migrate::MigrationReport;
use crate::settings::Settings;

fn connect(url: &str) -> impl std::future::Future<Output = Result<Db>> + use<> {
    let url = url.to_owned();
    async move { Db::connect(&url, 4).await }
}

/// `lanverse_game token --email=… [--name=…] [--device=…] [--revoke]`.
pub fn token(args: &[String]) -> Result<()> {
    let settings = Settings::from_env();
    let url = settings
        .database_url
        .clone()
        .context("LANBASH_DATABASE_URL is required (postgres://…)")?;
    let runtime = runtime()?;
    let handle = runtime.handle().clone();
    let result = token_async(args, handle, &url, &settings.public_url);
    runtime.shutdown_background();
    result
}

fn token_async(
    args: &[String],
    runtime: tokio::runtime::Handle,
    url: &str,
    public_url: &str,
) -> Result<()> {
    let db = runtime.block_on(connect(url))?;
    let email = arg(args, "--email").unwrap_or_else(|| "builder@example.test".to_owned());
    let name = arg(args, "--name");
    let device = arg(args, "--device").unwrap_or_else(|| "builder".to_owned());
    let revoke = args.iter().any(|arg| arg == "--revoke");

    if revoke {
        let user = runtime.block_on(db.user_by_email(&email))?;
        let Some(user) = user else {
            bail!("No account with email {email}.");
        };
        let revoked = runtime.block_on(db.revoke_user_tokens(user.id))?;
        println!("Revoked {revoked} token(s) for {email}");
        return Ok(());
    }

    let tokens = crate::auth::TokenService::new(db);
    let (token, user_id, display_name) =
        runtime.block_on(tokens.issue_for_email(&email, name.as_deref(), &device))?;

    println!("Token for {email} (player: {display_name}, user #{user_id})");
    println!();
    println!("{token}");
    println!();
    println!("Use it with the builder:");
    println!("  cd LANVerseBuilder");
    println!("  LAN_BASH_TOKEN='{token}' cargo run -- publish modes/coin_rush");
    println!();
    println!("API endpoint: {public_url}");
    println!("Revoke it with: lanverse_game token --revoke --email={email}");

    Ok(())
}

/// `lanverse_game mode [--list] [--owner=email] [mode]`.
pub fn mode(args: &[String]) -> Result<()> {
    let settings = Settings::from_env();
    let url = settings
        .database_url
        .clone()
        .context("LANBASH_DATABASE_URL is required (postgres://…)")?;
    let runtime = runtime()?;
    let handle = runtime.handle().clone();
    let result = mode_async(args, handle, &url);
    runtime.shutdown_background();
    result
}

fn mode_async(args: &[String], runtime: tokio::runtime::Handle, url: &str) -> Result<()> {
    let db = runtime.block_on(connect(url))?;
    let owner = arg(args, "--owner");
    let list = args.iter().any(|arg| arg == "--list");
    let slug = args
        .iter()
        .find(|arg| !arg.starts_with("--") && Some(arg.as_str()) != owner.as_deref())
        .cloned();

    let modes = runtime.block_on(db.modes(true))?;

    if modes.is_empty() {
        bail!("No uploaded modes.");
    }

    for mode in &modes {
        let owner_email = match mode.created_by {
            Some(id) => runtime
                .block_on(db.user_by_id(id))?
                .map(|user| user.email)
                .unwrap_or_else(|| format!("user #{id}")),
            None => "nobody (no account can update it)".to_owned(),
        };

        println!(
            "{} (\"{}\") – status {} – owner: {}",
            mode.slug, mode.name, mode.status, owner_email
        );
    }

    if list || owner.is_none() {
        return Ok(());
    }

    let email = owner.unwrap();
    let user = runtime
        .block_on(db.user_by_email(&email))?
        .with_context(|| format!("No account with email {email}."))?;

    let targets: Vec<String> = match slug {
        Some(slug) => vec![slug],
        None => modes.iter().map(|mode| mode.slug.clone()).collect(),
    };

    for slug in targets {
        if runtime.block_on(db.transfer_mode_owner(&slug, user.id))? {
            println!("{slug}: owner is now {email}");
        } else {
            println!("{slug}: not found");
        }
    }

    Ok(())
}

/// `lanverse_game migrate [--check]` (MySQL → Postgres).
pub fn migrate(args: &[String]) -> Result<()> {
    let runtime = runtime()?;
    let result = runtime.block_on(migrate_async(args));
    // Dropping a runtime from inside an async context panics; shut it down
    // explicitly once the command finished.
    runtime.shutdown_background();
    result
}

async fn migrate_async(args: &[String]) -> Result<()> {
    let settings = Settings::from_env();
    let target = {
        let url = settings
            .database_url
            .as_deref()
            .context("LANBASH_DATABASE_URL is required (postgres://…)")?;
        Db::connect(url, 4).await?
    };
    // Without a source there is nothing to copy: this is the data import, the
    // schema alone is created by `serve` (or by applying the migrations).
    let Some(mysql_url) = settings.mysql_url.as_deref() else {
        eprintln!("LANBASH_MYSQL_URL is not set: nothing to import.");
        eprintln!("The Postgres schema itself is created on `serve` (or `migrate` with a source).");
        return Ok(());
    };
    crate::migrate::check_url(mysql_url)?;

    let dry_run = args.iter().any(|arg| arg == "--check");
    target.migrate().await?;

    let report: MigrationReport = crate::migrate::migrate(mysql_url, &target, dry_run).await?;

    println!("{}", report.summary());

    if dry_run {
        println!("\ndry run: nothing was written (use without --check to import)");
        return Ok(());
    }

    if !report.is_consistent() {
        bail!("row counts differ between MySQL and Postgres; the import is incomplete");
    }

    println!("\nall tables match");
    Ok(())
}

/// `lanverse_game move` – placeholder for future data moves.
pub fn export(_args: &[String]) -> Result<()> {
    // Parity fixtures live in tests/parity/fixtures and are checked by
    // `cargo test --test parity_test`; nothing to export at runtime.
    println!("parity fixtures: tests/parity/fixtures (verify with cargo test --test parity_test)");
    Ok(())
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("creating the command runtime")
}

fn arg(args: &[String], flag: &str) -> Option<String> {
    let prefix = format!("{flag}=");
    args.iter()
        .find_map(|arg| arg.strip_prefix(&prefix).map(str::to_owned))
}
