use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{anyhow, Context, Result};

#[derive(Debug, Clone)]
pub struct CmdOut {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

pub fn which(bin: &str, path_env: &str) -> Option<PathBuf> {
    if bin.contains('/') {
        let path = PathBuf::from(bin);
        return is_exec(&path).then_some(path);
    }
    path_env.split(':').find_map(|dir| {
        if dir.is_empty() {
            return None;
        }
        let path = Path::new(dir).join(bin);
        is_exec(&path).then_some(path)
    })
}

fn is_exec(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

pub fn run(
    program: &str,
    args: &[String],
    dir: Option<&Path>,
    stdin_data: Option<&[u8]>,
) -> Result<CmdOut> {
    let path_env = std::env::var("PATH").unwrap_or_default();
    if which(program, &path_env).is_none() {
        anyhow::bail!("`{program}` is not on PATH");
    }
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(if stdin_data.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = dir {
        cmd.current_dir(dir);
    }
    cmd.env("GH_PROMPT_DISABLED", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GH_NO_UPDATE_NOTIFIER", "1");
    let mut child = cmd.spawn().with_context(|| format!("spawn {program}"))?;
    if let Some(data) = stdin_data {
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(data).ok();
        }
    }
    let output = child.wait_with_output().context("wait for command")?;
    Ok(CmdOut {
        code: output.status.code().unwrap_or(1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

pub fn gh(args: &[String], dir: Option<&Path>, repo: Option<&str>) -> Result<CmdOut> {
    let path_env = std::env::var("PATH").unwrap_or_default();
    if which("gh", &path_env).is_none() {
        anyhow::bail!("`gh` is not on PATH");
    }
    let mut cmd = Command::new("gh");
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GH_PROMPT_DISABLED", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GH_NO_UPDATE_NOTIFIER", "1");
    if let Some(repo) = repo {
        cmd.env("GH_REPO", repo);
    }
    if let Some(dir) = dir {
        cmd.current_dir(dir);
    }
    let output = cmd.output().context("run gh")?;
    Ok(CmdOut {
        code: output.status.code().unwrap_or(1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

pub fn git(dir: &Path, args: &[String]) -> Result<CmdOut> {
    run("git", args, Some(dir), None).map_err(|e| anyhow!(e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn which_finds_an_executable() {
        let dir = std::env::temp_dir().join(format!(
            "ghost-which-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("fake-gh");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&bin).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&bin, perms).unwrap();
        }
        assert!(which("fake-gh", dir.to_str().unwrap()).is_some());
        assert!(which("missing-ghost-bin", dir.to_str().unwrap()).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
