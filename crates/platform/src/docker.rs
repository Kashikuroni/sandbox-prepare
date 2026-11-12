//! Small, cancellable Docker CLI boundary. No user text is interpreted by a host shell.
use anyhow::{Context, Result, bail, ensure};
use std::{
    collections::VecDeque,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Clone)]
pub struct Docker {
    pub binary: PathBuf,
}
impl Docker {
    pub fn discover() -> Self {
        let binary = [
            "/usr/local/bin/docker",
            "/opt/homebrew/bin/docker",
            "/Applications/Docker.app/Contents/Resources/bin/docker",
        ]
        .into_iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
        .unwrap_or_else(|| "docker".into());
        Self { binary }
    }
    pub fn command(&self, args: &[String]) -> Command {
        let mut command = Command::new(&self.binary);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }
    pub fn output(&self, args: &[String], timeout: Duration) -> Result<String> {
        self.run(args, None, &AtomicBool::new(false), timeout, |_| {})
    }
    pub fn run(
        &self,
        args: &[String],
        input: Option<Box<dyn Read + Send>>,
        cancel: &AtomicBool,
        timeout: Duration,
        log: impl Fn(String) + Send + Sync,
    ) -> Result<String> {
        self.run_with_env(
            args,
            input,
            cancel,
            timeout,
            &std::collections::BTreeMap::new(),
            log,
        )
    }
    pub fn run_with_env(
        &self,
        args: &[String],
        input: Option<Box<dyn Read + Send>>,
        cancel: &AtomicBool,
        timeout: Duration,
        environment: &std::collections::BTreeMap<String, String>,
        log: impl Fn(String) + Send + Sync,
    ) -> Result<String> {
        ensure!(!cancel.load(Ordering::Acquire), "Operation cancelled");
        let mut command = self.command(args);
        command.envs(environment);
        if input.is_some() {
            command.stdin(Stdio::piped());
        }
        let mut child = command
            .spawn()
            .context("Could not run Docker. Install Docker Desktop and start its engine.")?;
        let writer = input.map(|mut input| {
            let mut stdin = child.stdin.take().expect("piped stdin");
            thread::spawn(move || -> std::io::Result<()> {
                std::io::copy(&mut input, &mut stdin)?;
                stdin.flush()
            })
        });
        let tail = Mutex::new(VecDeque::<String>::new());
        let started = Instant::now();
        thread::scope(|scope| -> Result<String> {
            let log = &log;
            let tail = &tail;
            let mut readers = Vec::new();
            for pipe in [
                Box::new(child.stdout.take().unwrap()) as Box<dyn Read + Send>,
                Box::new(child.stderr.take().unwrap()),
            ] {
                readers.push(scope.spawn(move || {
                    let mut reader = BufReader::new(pipe);
                    let mut bytes = Vec::new();
                    // Preserve full logical lines for container and restore logs.
                    while let Ok(n) = reader.read_until(b'\n', &mut bytes) {
                        if n == 0 {
                            break;
                        }
                        let line = String::from_utf8_lossy(&bytes).trim_end().to_string();
                        bytes.clear();
                        log(line.clone());
                        let mut lines = tail.lock().unwrap();
                        lines.push_back(line);
                        while lines.len() > 80 {
                            lines.pop_front();
                        }
                    }
                }));
            }
            let status = loop {
                if cancel.load(Ordering::Acquire) || started.elapsed() > timeout {
                    kill_group(&mut child);
                    let _ = child.wait();
                    break None;
                }
                match child.try_wait() {
                    Ok(Some(status)) => break Some(status),
                    Ok(None) => thread::sleep(Duration::from_millis(75)),
                    Err(error) => {
                        kill_group(&mut child);
                        let _ = child.wait();
                        return Err(error.into());
                    }
                }
            };
            let writer_result = writer.map(|writer| {
                writer
                    .join()
                    .map_err(|_| std::io::Error::other("Input writer panicked"))
                    .and_then(|result| result)
            });
            for reader in readers {
                let _ = reader.join();
            }
            let output = tail
                .lock()
                .unwrap()
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join("\n");
            match status {
                None if cancel.load(Ordering::Acquire) => bail!("Operation cancelled"),
                None => bail!(
                    "Docker command timed out after {} seconds",
                    timeout.as_secs()
                ),
                Some(status) if !status.success() => bail!("Docker command failed: {output}"),
                _ => {
                    if let Some(result) = writer_result {
                        result.context("Could not stream backup into Docker")?;
                    }
                    Ok(output)
                }
            }
        })
    }
    /// Capture structured CLI output without mixing stderr or truncating JSON.
    pub fn capture(
        &self,
        args: &[String],
        environment: &std::collections::BTreeMap<String, String>,
        cancel: &AtomicBool,
        timeout: Duration,
    ) -> Result<String> {
        ensure!(!cancel.load(Ordering::Acquire), "Operation cancelled");
        let mut child = self
            .command(args)
            .envs(environment)
            .spawn()
            .with_context(|| format!("Could not start {}", self.binary.display()))?;
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let started = Instant::now();
        thread::scope(|scope| -> Result<String> {
            let read = |pipe: Box<dyn Read + Send>| -> std::io::Result<Vec<u8>> {
                let mut bytes = Vec::new();
                pipe.take(16 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
                Ok(bytes)
            };
            let out = scope.spawn(move || read(Box::new(stdout)));
            let err = scope.spawn(move || read(Box::new(stderr)));
            let status = loop {
                if cancel.load(Ordering::Acquire) || started.elapsed() > timeout {
                    kill_group(&mut child);
                    break child.wait()?;
                }
                match child.try_wait() {
                    Ok(Some(status)) => break status,
                    Ok(None) => thread::sleep(Duration::from_millis(50)),
                    Err(error) => {
                        kill_group(&mut child);
                        let _ = child.wait();
                        return Err(error.into());
                    }
                }
            };
            let stdout = out
                .join()
                .map_err(|_| anyhow::anyhow!("Output reader failed"))??;
            let stderr = err
                .join()
                .map_err(|_| anyhow::anyhow!("Error reader failed"))??;
            ensure!(!cancel.load(Ordering::Acquire), "Operation cancelled");
            ensure!(started.elapsed() <= timeout, "Command timed out");
            ensure!(
                stdout.len() <= 16 * 1024 * 1024 && stderr.len() <= 16 * 1024 * 1024,
                "Command output exceeds 16 MiB"
            );
            ensure!(status.success(), "{}", String::from_utf8_lossy(&stderr));
            Ok(String::from_utf8(stdout)?)
        })
    }
    pub fn check(&self) -> Result<()> {
        self.output(
            &strings(&["version", "--format", "{{.Server.Version}}"]),
            Duration::from_secs(10),
        )?;
        self.output(&strings(&["compose", "version"]), Duration::from_secs(10))?;
        Ok(())
    }
}
fn kill_group(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        // Every command starts its own process group; never signal the caller's group.
        unsafe {
            libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
        }
    }
    let _ = child.kill();
}
pub fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|s| (*s).into()).collect()
}
pub fn private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::fs::OpenOptions;
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}
pub fn nonce() -> Result<String> {
    let mut random = [0u8; 16];
    #[cfg(unix)]
    std::fs::File::open("/dev/urandom")?.read_exact(&mut random)?;
    #[cfg(not(unix))]
    bail!("Secure random source is not implemented on this platform");
    Ok(random.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    #[cfg(unix)]
    #[test]
    fn cancellation_reaps_descendants_holding_output_pipes() {
        let cli = Docker {
            binary: "/bin/sh".into(),
        };
        let flag = Arc::new(AtomicBool::new(false));
        let cancel = flag.clone();
        let trigger = thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            cancel.store(true, Ordering::Release);
        });
        let started = Instant::now();
        assert!(
            cli.capture(
                &strings(&["-c", "sleep 3 & wait"]),
                &std::collections::BTreeMap::new(),
                &flag,
                Duration::from_secs(5)
            )
            .is_err()
        );
        trigger.join().unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));
    }
    #[test]
    fn cancellation_reaps_the_cli_child() {
        let docker = Docker {
            binary: "/bin/sleep".into(),
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let trigger = cancel.clone();
        let task = thread::spawn(move || {
            thread::sleep(Duration::from_millis(80));
            trigger.store(true, Ordering::Release);
        });
        let started = Instant::now();
        assert!(
            docker
                .run(
                    &strings(&["10"]),
                    None,
                    &cancel,
                    Duration::from_secs(20),
                    |_| {}
                )
                .is_err()
        );
        task.join().unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}

#[cfg(test)]
mod stream_tests {
    use super::*;
    struct BrokenInput;
    impl Read for BrokenInput {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("damaged compressed input"))
        }
    }
    #[test]
    fn input_errors_are_not_hidden_by_a_successful_consumer() {
        let docker = Docker {
            binary: "/bin/cat".into(),
        };
        let result = docker.run(
            &[],
            Some(Box::new(BrokenInput)),
            &AtomicBool::new(false),
            Duration::from_secs(2),
            |_| {},
        );
        assert!(result.unwrap_err().to_string().contains("stream backup"));
    }
}
