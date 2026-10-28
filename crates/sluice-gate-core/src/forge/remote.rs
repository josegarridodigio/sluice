//! Remote URLs: which host and repository path a push URL names, and the
//! real host behind an SSH config alias (`ssh -G`).

use std::path::Path;
use std::time::Duration;

use super::exec::{self, Budget};
use crate::git::EnvMap;

/// A parsed remote URL (userinfo dropped).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteUrl {
    /// Lowercase host (or SSH alias), without port.
    pub host: String,
    /// Port of an `ssh://` or `https://` URL.
    pub port: Option<u16>,
    /// Repository path without leading `/` and trailing `.git`.
    pub path: String,
    /// True for `ssh://` and scp-like URLs: the host may be an alias.
    pub ssh: bool,
}

/// Parses `https://`, `http://`, `ssh://`, `git://` and scp-like
/// (`[user@]host:path`) URLs; `None` for a local path or anything else.
#[must_use]
pub fn parse(url: &str) -> Option<RemoteUrl> {
    let url = url.trim();
    if let Some((scheme, rest)) = url.split_once("://") {
        let ssh = matches!(scheme, "ssh" | "git+ssh" | "ssh+git");
        if !ssh && !matches!(scheme, "https" | "http" | "git") {
            return None;
        }
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        let hostport = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
        let (host, port) = split_port(hostport)?;
        return build(host, port, path, ssh);
    }
    if url.starts_with(['/', '.', '~']) {
        return None;
    }
    let (left, path) = url.split_once(':')?;
    if left.contains('/') || path.starts_with("//") {
        return None;
    }
    let host = left.rsplit_once('@').map_or(left, |(_, h)| h);
    build(host, None, path, true)
}

fn split_port(hostport: &str) -> Option<(&str, Option<u16>)> {
    if hostport.starts_with('[') {
        return None;
    }
    match hostport.rsplit_once(':') {
        Some((host, port)) => Some((host, Some(port.parse().ok()?))),
        None => Some((hostport, None)),
    }
}

fn build(host: &str, port: Option<u16>, path: &str, ssh: bool) -> Option<RemoteUrl> {
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    if host.is_empty() || path.is_empty() {
        return None;
    }
    Some(RemoteUrl {
        host: host.to_ascii_lowercase(),
        port,
        path: path.to_owned(),
        ssh,
    })
}

/// The `hostname` line of `ssh -G` output.
#[must_use]
pub fn hostname_of(ssh_g: &str) -> Option<String> {
    ssh_g.lines().find_map(|l| {
        let (key, value) = l.trim().split_once(char::is_whitespace)?;
        (key.eq_ignore_ascii_case("hostname") && !value.trim().is_empty())
            .then(|| value.trim().to_ascii_lowercase())
    })
}

/// The host an SSH URL really reaches: the `hostname` of `ssh -G <host>`
/// (config aliases), falling back to the URL's own host when `ssh` is
/// missing, slow or answers nothing useful. `ssh.github.com` (the port 443
/// endpoint) is `github.com`.
#[must_use]
pub fn real_host(url: &RemoteUrl, env: &EnvMap, cwd: &Path, budget: &Budget) -> String {
    let mut args = vec!["-G".to_owned()];
    if let Some(p) = url.port {
        args.push("-p".to_owned());
        args.push(p.to_string());
    }
    args.push("--".to_owned());
    args.push(url.host.clone());
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let resolved = exec::run("ssh", &refs, env, cwd, budget, Duration::from_secs(2))
        .ok()
        .filter(|c| c.code == Some(0))
        .and_then(|c| hostname_of(&c.stdout))
        .unwrap_or_else(|| url.host.clone());
    if resolved == "ssh.github.com" {
        "github.com".to_owned()
    } else {
        resolved
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn url(s: &str) -> RemoteUrl {
        parse(s).unwrap_or_else(|| panic!("{s} should parse"))
    }

    #[test]
    fn https_urls_drop_userinfo_and_the_git_suffix() {
        let u = url("https://user:tok@GitHub.com/acme/widgets.git");
        assert_eq!(
            (u.host.as_str(), u.path.as_str(), u.port, u.ssh),
            ("github.com", "acme/widgets", None, false)
        );
        assert_eq!(
            url("https://ghe.example.com:8443/acme/widgets/").port,
            Some(8443)
        );
    }

    #[test]
    fn ssh_and_scp_like_urls_are_marked_as_ssh() {
        let scp = url("git@gh-alias:acme/widgets.git");
        assert_eq!(
            (scp.host.as_str(), scp.path.as_str(), scp.ssh),
            ("gh-alias", "acme/widgets", true)
        );
        let full = url("ssh://git@ssh.github.com:443/acme/widgets.git");
        assert_eq!(
            (full.host.as_str(), full.port, full.ssh),
            ("ssh.github.com", Some(443), true)
        );
        assert!(url("example.com:acme/widgets").ssh);
    }

    #[test]
    fn local_paths_and_odd_urls_are_not_remotes() {
        for u in [
            "/srv/git/r.git",
            "./r",
            "../r.git",
            "~/r",
            "file:///srv/r.git",
            "",
            "https://host",
        ] {
            assert_eq!(parse(u), None, "{u}");
        }
    }

    #[test]
    fn hostname_is_read_from_ssh_g_output() {
        let out = "user git\nhostname Github.COM\nport 22\n";
        assert_eq!(hostname_of(out).as_deref(), Some("github.com"));
        assert_eq!(hostname_of("user git\nport 22\n"), None);
    }

    #[test]
    fn an_alias_is_resolved_with_ssh_g_and_the_port_443_endpoint_is_github() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("ssh");
        std::fs::write(
            &fake,
            "#!/bin/sh\nfor a; do last=$a; done\n\
             case \"$last\" in alias) echo 'hostname ssh.github.com';; *) exit 255;; esac\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut env = EnvMap::new();
        env.insert("PATH".into(), dir.path().display().to_string());
        let budget = Budget::new(Duration::from_secs(5));
        let host = |u: &str| real_host(&url(u), &env, dir.path(), &budget);
        assert_eq!(host("git@alias:acme/widgets.git"), "github.com");
        // `ssh -G` failing keeps the URL's own host.
        assert_eq!(
            host("git@other.example.com:acme/widgets.git"),
            "other.example.com"
        );
    }
}
