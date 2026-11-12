use super::*;
use sandbox_projects::compose::Plan;
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
};

/// A running container without a health check is not a verified-ready application.
fn service_state(state: &Value, completed_job: bool) -> Result<(bool, bool)> {
    match state["Status"].as_str().unwrap_or("unknown") {
        "exited" if completed_job && state["ExitCode"] == 0 => Ok((true, true)),
        "dead" | "exited" => anyhow::bail!("Service exited. See Processes for its log."),
        "running" => match state["Health"]["Status"].as_str() {
            Some("healthy") => Ok((true, true)),
            Some("unhealthy") => anyhow::bail!("Service is unhealthy. See Processes."),
            Some(_) => Ok((false, false)),
            None => Ok((true, false)),
        },
        _ => Ok((false, false)),
    }
}
fn http_ok(address: SocketAddr, path: &str) -> Result<bool> {
    ensure!(
        path.starts_with('/') && !path.contains(['\r', '\n']),
        "Invalid readiness path"
    );
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(2))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"
    )?;
    let mut line = Vec::new();
    let mut byte = [0; 1];
    while line.len() < 1024 && stream.read(&mut byte)? == 1 {
        line.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
    }
    let status = std::str::from_utf8(&line)?
        .split_whitespace()
        .nth(1)
        .and_then(|n| n.parse::<u16>().ok());
    Ok(status.is_some_and(|code| (200..300).contains(&code)))
}
impl Manager {
    pub(super) fn validate_connection_databases(
        &self,
        instance: &Instance,
        plan: &Plan,
    ) -> Result<()> {
        self.stage("Checking restored database names…");
        for (role, names) in &plan.connection_databases {
            for name in names {
                let hex = name
                    .as_bytes()
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>();
                let mysql = role == "mysql";
                let query = if mysql {
                    format!(
                        "SELECT count(*) FROM information_schema.schemata WHERE schema_name=CONVERT(X'{hex}' USING utf8mb4)"
                    )
                } else {
                    format!("SELECT count() FROM system.databases WHERE name=unhex('{hex}')")
                };
                ensure!(
                    self.sql(instance, mysql, &query)?.trim().parse::<u64>()? > 0,
                    "Database {name} is missing from the restored {role} backup; correct the project profile"
                );
            }
        }
        Ok(())
    }
    pub(super) fn wait_for_application(&self, instance: &Instance, plan: &Plan) -> Result<()> {
        self.stage("Checking application readiness…");
        let started = Instant::now();
        loop {
            ensure!(!self.cancel.load(Ordering::Acquire), "Cancelled");
            let mut started_all = true;
            let mut checked_all = true;
            let mut endpoints = Vec::new();
            let mut probe_address = None;
            for service in &plan.application_services {
                let name = format!("{}-app-{service}", instance.project);
                let output = self.docker.capture(
                    &strings(&["inspect", &name]),
                    &BTreeMap::new(),
                    &self.cancel,
                    SHORT,
                )?;
                let inspect: Value = serde_json::from_str(&output)?;
                let container = &inspect[0];
                ensure!(
                    container["Config"]["Labels"][OWNER_LABEL] == self.store.owner
                        && container["Config"]["Labels"][INSTANCE_LABEL] == instance.id,
                    "Refusing a foreign application container"
                );
                let completed_job = plan.document["services"]
                    .as_object()
                    .into_iter()
                    .flat_map(|s| s.values())
                    .any(|s| {
                        s["depends_on"][service]["condition"] == "service_completed_successfully"
                    });
                // The protected document encodes strings; decode the condition via its environment.
                let completed_job = completed_job
                    || plan.document["services"]
                        .as_object()
                        .into_iter()
                        .flat_map(|s| s.values())
                        .any(|s| {
                            s["depends_on"][service]["condition"]
                                .as_str()
                                .and_then(|v| v.strip_prefix("${"))
                                .and_then(|v| v.strip_suffix('}'))
                                .and_then(|key| plan.environment.get(key))
                                .is_some_and(|v| v == "service_completed_successfully")
                        });
                let (running, checked) = service_state(&container["State"], completed_job)
                    .with_context(|| format!("Service {service}"))?;
                started_all &= running;
                checked_all &= checked;
                if let Some(ports) = container["NetworkSettings"]["Ports"].as_object() {
                    for (port, bindings) in ports {
                        for binding in bindings.as_array().into_iter().flatten() {
                            let Some(host_port) = binding["HostPort"]
                                .as_str()
                                .and_then(|v| v.parse::<u16>().ok())
                            else {
                                continue;
                            };
                            endpoints.push(format!("{service}: 127.0.0.1:{host_port} → {port}"));
                            if plan.readiness_probe.as_ref().is_some_and(|p| {
                                p.service == *service && port == &format!("{}/tcp", p.port)
                            }) {
                                probe_address = Some(SocketAddr::from(([127, 0, 0, 1], host_port)));
                            }
                        }
                    }
                }
            }
            let probe_ready = if let Some(probe) = &plan.readiness_probe {
                let address=probe_address.context("Publish the HTTP readiness container port in Compose, or use a Compose healthcheck")?;
                if http_ok(address, &probe.path).unwrap_or(false) {
                    endpoints.push(format!("http://{address}{}", probe.path));
                    true
                } else {
                    false
                }
            } else {
                true
            };
            if started_all && probe_ready {
                let verified = checked_all || plan.readiness_probe.is_some();
                self.change(|s| {
                    s.application_running = true;
                    s.application_ready = verified;
                    s.endpoints = endpoints;
                });
                return Ok(());
            }
            ensure!(
                started.elapsed() < Duration::from_secs(180),
                "Application did not become ready. See Processes."
            );
            thread::sleep(Duration::from_millis(500));
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn http_probe_requires_a_successful_response_and_rejects_header_injection() -> Result<()> {
        for (code, expected) in [(200, true), (204, true), (503, false), (302, false)] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
            let address = listener.local_addr()?;
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 1024];
                while !request.ends_with(b"\r\n\r\n") && request.len() < 8192 {
                    let count = stream.read(&mut buffer).unwrap();
                    assert!(count > 0);
                    request.extend_from_slice(&buffer[..count]);
                }
                assert!(String::from_utf8_lossy(&request).starts_with("GET /health HTTP/1.1"));
                write!(
                    stream,
                    "HTTP/1.1 {code} Result\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .unwrap();
            });
            assert_eq!(http_ok(address, "/health")?, expected);
            server.join().unwrap();
        }
        assert!(http_ok("127.0.0.1:1".parse()?, "/health\r\nInjected: true").is_err());
        Ok(())
    }
    #[test]
    fn readiness_distinguishes_running_unchecked_and_successful_jobs() {
        assert_eq!(
            service_state(&serde_json::json!({"Status":"running"}), false).unwrap(),
            (true, false)
        );
        assert_eq!(
            service_state(
                &serde_json::json!({"Status":"running","Health":{"Status":"healthy"}}),
                false
            )
            .unwrap(),
            (true, true)
        );
        assert_eq!(
            service_state(
                &serde_json::json!({"Status":"running","Health":{"Status":"starting"}}),
                false
            )
            .unwrap(),
            (false, false)
        );
        assert!(
            service_state(&serde_json::json!({"Status":"exited","ExitCode":0}), false).is_err()
        );
        assert_eq!(
            service_state(&serde_json::json!({"Status":"exited","ExitCode":0}), true).unwrap(),
            (true, true)
        );
    }
}
