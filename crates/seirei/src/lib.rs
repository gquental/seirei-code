pub mod config;
pub mod coordinator;
pub mod project;
pub mod server;
pub mod t3;

use config::Config;
use std::{
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::Path,
};
use t3::T3Api;

pub enum Started {
    Served,
    AlreadyRunning,
}

pub async fn run_daemon(config: Config) -> anyhow::Result<Started> {
    let socket = seirei_proto::home::socket()?;
    create_private_dir(&config.state_dir)?;
    let lock = std::fs::File::create(config.state_dir.join("daemon.lock"))?;
    if lock.try_lock().is_err() {
        return Ok(Started::AlreadyRunning);
    }
    if let Some(dir) = socket.parent() {
        create_private_dir(dir)?;
    }
    let _ = std::fs::remove_file(&socket);
    let listener = tokio::net::UnixListener::bind(&socket)?;
    let machines = config.machines.into_iter().map(|machine| {
        if machine.token.is_none() {
            let login = seirei_proto::model::login_command(&machine.id);
            eprintln!("seirei: no usable credential for {}; run `{login}`", machine.id);
        }
        let api = t3::McpT3::connect(machine.endpoint, machine.token);
        (machine.id, server::MachineApi::new(api, machine.coordinator_model))
    });
    let daemon = server::Daemon::new(machines, config.dir, config.state_dir)?;
    daemon.warm_start()?;
    daemon.serve(listener).await?;
    let _ = std::fs::remove_file(&socket);
    Ok(Started::Served)
}

/// Creates `dir` and any missing parent readable by this user alone.
pub fn create_private_dir(dir: &Path) -> io::Result<()> {
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)
}

pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        create_private_dir(dir)?;
    }
    let temp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&temp);
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(&temp, path)
}
