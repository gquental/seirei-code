//! Where seirei keeps its files. The daemon and its clients both resolve the root here, so they
//! agree on it.

use std::{ffi::OsString, io, path::PathBuf};

/// The longest path a Unix socket takes, `sun_path` less its terminating NUL.
#[cfg(target_os = "linux")]
const MAX_SOCKET_PATH: usize = 107;
#[cfg(not(target_os = "linux"))]
const MAX_SOCKET_PATH: usize = 103;

/// `$SEIREI_HOME`, else `~/.seirei/code`.
pub fn root() -> io::Result<PathBuf> {
    root_in(&|name| std::env::var_os(name), std::env::home_dir())
}

/// `$SEIREI_SOCKET`, else `daemon.sock` in the root.
pub fn socket() -> io::Result<PathBuf> {
    socket_in(&|name| std::env::var_os(name), std::env::home_dir())
}

fn root_in(var: &impl Fn(&str) -> Option<OsString>, home: Option<PathBuf>) -> io::Result<PathBuf> {
    match set(var, "SEIREI_HOME") {
        Some(root) => absolute("SEIREI_HOME", root),
        None => Ok(home.ok_or_else(|| io::Error::other("no home directory; set SEIREI_HOME"))?.join(".seirei/code")),
    }
}

fn socket_in(var: &impl Fn(&str) -> Option<OsString>, home: Option<PathBuf>) -> io::Result<PathBuf> {
    let path = match set(var, "SEIREI_SOCKET") {
        Some(path) => absolute("SEIREI_SOCKET", path)?,
        None => root_in(var, home)?.join("daemon.sock"),
    };
    if path.as_os_str().len() > MAX_SOCKET_PATH {
        return Err(io::Error::other(format!(
            "the daemon socket {} is longer than the {MAX_SOCKET_PATH} bytes a Unix socket path may have; set \
             SEIREI_SOCKET to a shorter path, or set a shorter SEIREI_HOME",
            path.display()
        )));
    }
    Ok(path)
}

/// A relative path would resolve against each process's own working directory, so two `seirei`
/// commands started in different folders would look for different daemons.
fn absolute(name: &str, value: OsString) -> io::Result<PathBuf> {
    let path = PathBuf::from(value);
    if path.is_relative() {
        return Err(io::Error::other(format!("{name} must be an absolute path, not {}", path.display())));
    }
    Ok(path)
}

/// An empty variable counts as unset.
fn set(var: &impl Fn(&str) -> Option<OsString>, name: &str) -> Option<OsString> {
    var(name).filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let vars: Vec<(String, OsString)> = vars.iter().map(|(name, value)| (name.to_string(), value.into())).collect();
        move |name| vars.iter().find(|(set, _)| set == name).map(|(_, value)| value.clone())
    }

    fn home() -> Option<PathBuf> {
        Some("/home/gui".into())
    }

    #[test]
    fn the_root_is_seirei_home_else_dot_seirei_code() {
        assert_eq!(root_in(&env(&[("SEIREI_HOME", "/srv/seirei")]), home()).unwrap(), PathBuf::from("/srv/seirei"));
        assert_eq!(root_in(&env(&[("SEIREI_HOME", "")]), home()).unwrap(), PathBuf::from("/home/gui/.seirei/code"));
        assert_eq!(root_in(&env(&[]), home()).unwrap(), PathBuf::from("/home/gui/.seirei/code"));
        let homeless = root_in(&env(&[]), None).unwrap_err().to_string();
        assert!(homeless.contains("SEIREI_HOME"), "{homeless}");
    }

    #[test]
    fn the_socket_is_seirei_socket_else_in_the_root() {
        let both = env(&[("SEIREI_SOCKET", "/tmp/s.sock"), ("SEIREI_HOME", "/srv/seirei")]);
        assert_eq!(socket_in(&both, home()).unwrap(), PathBuf::from("/tmp/s.sock"));
        assert_eq!(socket_in(&env(&[("SEIREI_SOCKET", "/tmp/s.sock")]), None).unwrap(), PathBuf::from("/tmp/s.sock"));
        let rooted = env(&[("SEIREI_SOCKET", ""), ("SEIREI_HOME", "/srv/seirei")]);
        assert_eq!(socket_in(&rooted, home()).unwrap(), PathBuf::from("/srv/seirei/daemon.sock"));
        assert_eq!(socket_in(&env(&[]), home()).unwrap(), PathBuf::from("/home/gui/.seirei/code/daemon.sock"));
    }

    #[test]
    fn relative_paths_are_refused() {
        let relative_home = root_in(&env(&[("SEIREI_HOME", "seirei")]), home()).unwrap_err().to_string();
        assert!(relative_home.contains("SEIREI_HOME must be an absolute path"), "{relative_home}");
        let relative_socket = socket_in(&env(&[("SEIREI_SOCKET", "s.sock")]), home()).unwrap_err().to_string();
        assert!(relative_socket.contains("SEIREI_SOCKET must be an absolute path"), "{relative_socket}");
    }

    #[test]
    fn a_socket_path_too_long_for_unix_is_refused_by_name() {
        let deep = format!("/{}", "d".repeat(MAX_SOCKET_PATH));
        let refused = socket_in(&env(&[("SEIREI_HOME", &deep)]), home()).unwrap_err().to_string();
        assert!(refused.contains(&format!("{deep}/daemon.sock")), "{refused}");
        assert!(refused.contains("set SEIREI_SOCKET") && refused.contains("shorter SEIREI_HOME"), "{refused}");
        let longest = format!("/{}", "s".repeat(MAX_SOCKET_PATH - 1));
        assert_eq!(socket_in(&env(&[("SEIREI_SOCKET", &longest)]), home()).unwrap(), PathBuf::from(&longest));
        let over = format!("{longest}s");
        assert!(socket_in(&env(&[("SEIREI_SOCKET", &over)]), home()).is_err(), "one byte over");
    }
}
