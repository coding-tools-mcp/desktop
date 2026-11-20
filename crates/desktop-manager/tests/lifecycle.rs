use desktop_manager::{core, model::*, Manager};
use serde_json::json;
use std::{
    net::TcpListener,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

fn lifecycle_fixture_lock() -> MutexGuard<'static, ()> {
    // These process-launch fixtures share OS descriptor/port state. Keep
    // independent test cases from probing ports while another case forks;
    // dedicated concurrency tests and the two-workspace case remain intact.
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|error| error.into_inner())
}
fn python() -> String {
    std::env::var("PYTHON")
        .unwrap_or_else(|_| if cfg!(windows) { "python" } else { "python3" }.into())
}
fn fixture() -> Vec<String> {
    vec![
        python(),
        format!("{}/tests/fixtures/fake_core.py", env!("CARGO_MANIFEST_DIR")),
    ]
}

#[cfg(unix)]
fn unused_port(port: u16) -> std::io::Result<()> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    // TcpListener enables SO_REUSEADDR on Unix, so its successful bind can
    // select a port with connections left by an earlier test invocation.
    // Probe without reuse: fixture allocation should skip all such TCP state.
    #[cfg(target_os = "linux")]
    let kind = libc::SOCK_STREAM | libc::SOCK_CLOEXEC;
    #[cfg(not(target_os = "linux"))]
    let kind = libc::SOCK_STREAM;
    let fd = unsafe { libc::socket(libc::AF_INET, kind, 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let socket = unsafe { OwnedFd::from_raw_fd(fd) };
    if unsafe { libc::fcntl(socket.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    let mut address: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    address.sin_family = libc::AF_INET as libc::sa_family_t;
    address.sin_port = port.to_be();
    address.sin_addr.s_addr = u32::from_ne_bytes([127, 0, 0, 1]);
    #[cfg(target_os = "macos")]
    {
        address.sin_len = std::mem::size_of_val(&address) as u8;
    }
    let result = unsafe {
        libc::bind(
            socket.as_raw_fd(),
            (&address as *const libc::sockaddr_in).cast(),
            std::mem::size_of_val(&address) as libc::socklen_t,
        )
    };
    if result == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(unix))]
fn unused_port(port: u16) -> std::io::Result<()> {
    TcpListener::bind(("127.0.0.1", port)).map(|_| ())
}

fn test_port() -> u16 {
    // Keep fixture listeners out of the OS outbound ephemeral range. Otherwise
    // readiness HTTP requests can allocate a just-released candidate as their
    // source port before its server starts, leaving a TIME_WAIT collision.
    // Independent/repeated invocations must not all probe the same first ports
    // while the previous invocation's sockets are still settling. Each run
    // starts elsewhere, and its monotonic index never repeats a candidate.
    const PORT_COUNT: u16 = 4000;
    static OFFSET: std::sync::OnceLock<u16> = std::sync::OnceLock::new();
    static NEXT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);
    let offset =
        *OFFSET.get_or_init(|| (uuid::Uuid::new_v4().as_u128() % PORT_COUNT as u128) as u16);
    for _ in 0..PORT_COUNT {
        let index = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        assert!(
            index < PORT_COUNT,
            "Lifecycle fixture port range is exhausted"
        );
        let port = 20000 + (offset + index) % PORT_COUNT;
        match unused_port(port) {
            Ok(()) => return port,
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => continue,
            Err(error) => panic!("Could not probe lifecycle fixture port {port}: {error}"),
        }
    }
    panic!("No unused lifecycle fixture port is available");
}

#[cfg(unix)]
#[test]
fn fixture_port_probe_rejects_prior_tcp_state() {
    use std::io::Read;
    use std::net::{Shutdown, TcpStream};

    let _test_guard = lifecycle_fixture_lock();
    // This in-process fixture keeps its listener bound while obtaining the
    // port, so it does not itself introduce the bind/drop allocation window.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let (mut server, _) = listener.accept().unwrap();
    drop(listener);
    client
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    server
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();

    // Make the server the active closer, putting its local port in TIME_WAIT.
    server.shutdown(Shutdown::Write).unwrap();
    let mut byte = [0u8; 1];
    assert_eq!(client.read(&mut byte).unwrap(), 0);
    client.shutdown(Shutdown::Write).unwrap();
    assert_eq!(server.read(&mut byte).unwrap(), 0);
    drop(client);
    drop(server);

    assert_eq!(
        unused_port(port).unwrap_err().kind(),
        std::io::ErrorKind::AddrInUse
    );
}
fn workspace(path: &std::path::Path) -> Workspace {
    serde_json::from_value(json!({"id":"","name":"Sample project","path":path,"port":test_port(),"access":"local","auth":"noauth","permissionMode":"safe","coreCommand":fixture()})).unwrap()
}
fn manager() -> (MutexGuard<'static, ()>, tempfile::TempDir, Arc<Manager>) {
    manager_in(tempfile::tempdir().unwrap())
}

fn manager_in(
    dir: tempfile::TempDir,
) -> (MutexGuard<'static, ()>, tempfile::TempDir, Arc<Manager>) {
    let guard = lifecycle_fixture_lock();
    #[cfg(unix)]
    let m = Manager::open_supervised(
        dir.path().join("home"),
        std::env::var_os("DESKTOP_SUPERVISOR_BIN")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::path::PathBuf::from(env!("CARGO_BIN_EXE_desktop-process-supervisor"))
            }),
    )
    .unwrap();
    #[cfg(not(unix))]
    let m = Manager::open(dir.path().join("home")).unwrap();
    (guard, dir, m)
}
#[test]
fn lifecycle_two_workspaces_and_configuration_lock() {
    let (_test_guard, d, m) = manager();
    let w = m.save_workspace(workspace(d.path()), None).unwrap();
    let mut second = workspace(d.path());
    second.name = "Second".into();
    let w2 = m.save_workspace(second, None).unwrap();
    assert_ne!(w.port, w2.port);
    let s = m.start(&w.id).unwrap();
    assert_eq!(s.local_state, "ready");
    assert_eq!(m.start(&w.id).unwrap().pid, s.pid);
    assert!(m.save_workspace(w.clone(), None).is_err());
    assert!(m.delete_workspace(&w.id).is_err());
    let s2 = m.start(&w2.id).unwrap();
    assert_ne!(s.pid, s2.pid);
    assert_eq!(m.stop(&w.id).unwrap().state, "stopped");
    assert_eq!(m.snapshot().statuses[1].state, "running");
    assert_eq!(m.restart(&w2.id).unwrap().state, "running");
    m.stop_all().unwrap();
    assert!(TcpListener::bind(("127.0.0.1", w.port)).is_ok());
}
#[test]
fn occupied_port_never_kills_unrelated_listener() {
    let (_test_guard, d, m) = manager();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut w = workspace(d.path());
    w.port = listener.local_addr().unwrap().port();
    let w = m.save_workspace(w, None).unwrap();
    assert!(m
        .start(&w.id)
        .unwrap_err()
        .to_string()
        .contains("Port is already in use"));
    assert!(listener.local_addr().is_ok());
    assert_eq!(m.snapshot().statuses[0].state, "error");
}
#[test]
fn crash_is_reported_and_can_restart() {
    let (_test_guard, d, m) = manager();
    let w = m.save_workspace(workspace(d.path()), None).unwrap();
    let s = m.start(&w.id).unwrap();
    let mut sys = sysinfo::System::new_all();
    sys.refresh_all();
    let pid = sysinfo::Pid::from_u32(s.pid.unwrap());
    let process = sys.process(pid).unwrap();
    let birth = process.start_time();
    assert!(process.kill(), "Fixture crash signal was not accepted");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut exited;
    let mut status;
    loop {
        sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
        exited = !sys.process(pid).is_some_and(|process| {
            process.start_time() == birth && process.status() != sysinfo::ProcessStatus::Zombie
        });
        // refresh intentionally skips a busy lifecycle lock. Wait for both
        // real process exit and the resulting manager state, rather than
        // assuming a single observation 100ms after kill must see the crash.
        m.refresh();
        status = m.snapshot().statuses.remove(0);
        if (exited && status.state == "error" && !status.cleanup_pending)
            || std::time::Instant::now() >= deadline
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        exited,
        "Fixture process did not exit after its crash signal"
    );
    assert_eq!(
        status.state, "error",
        "Manager never observed the fixture crash"
    );
    assert!(!status.cleanup_pending, "Crash cleanup did not finish");
    assert_eq!(m.start(&w.id).unwrap().state, "running");
    m.stop_all().unwrap();
}
#[test]
fn remote_requires_authentication_and_safe_url() {
    let (_test_guard, d, m) = manager();
    let mut w = workspace(d.path());
    w.access = "quick".into();
    assert!(m.save_workspace(w.clone(), None).is_err());
    w.auth = "oauth".into();
    assert!(m.save_workspace(w.clone(), None).is_err());
    w.access = "named".into();
    w.public_url = "https://user:password@example.com".into();
    assert!(m.save_workspace(w, None).is_err());
}
#[test]
fn tunnel_failure_does_not_stop_local_core() {
    if core::find_program("cloudflared").is_some() {
        return;
    }
    let (_test_guard, d, m) = manager();
    let mut w = workspace(d.path());
    w.access = "quick".into();
    w.auth = "bearer".into();
    let w = m.save_workspace(w, None).unwrap();
    let s = m.start(&w.id).unwrap();
    assert_eq!(s.state, "running");
    assert_eq!(s.local_state, "ready");
    assert_eq!(s.public_state, "error");
    assert!(m.connection_config(&w.id, true).is_err());
    assert!(m
        .connection_config(&w.id, false)
        .unwrap()
        .contains("Bearer"));
    m.stop_all().unwrap();
}
#[test]
fn oauth_requires_client_authorization_but_has_verified_core() {
    let (_test_guard, d, m) = manager();
    let mut w = workspace(d.path());
    w.auth = "oauth".into();
    let w = m.save_workspace(w, None).unwrap();
    let s = m.start(&w.id).unwrap();
    assert!(s.local_message.contains("OAuth authorization"));
    assert!(
        m.auth_details(&w.id).unwrap()["oauthPassword"]
            .as_str()
            .unwrap()
            .len()
            > 32
    );
    m.stop_all().unwrap();
}
#[test]
fn diagnostics_do_not_export_paths_credentials_or_urls() {
    let (_test_guard, d, m) = manager();
    let w = m
        .save_workspace(
            workspace(d.path()),
            Some(Secrets {
                bearer_token: "private-secret-value".into(),
                ..Default::default()
            }),
        )
        .unwrap();
    m.start(&w.id).unwrap();
    let report = m.export_diagnostics(&w.id).unwrap();
    assert!(!report.contains("private-secret-value"));
    assert!(!report.contains(&w.path));
    assert!(!report.contains("127.0.0.1"));
    m.stop_all().unwrap();
}

fn initialize(client: &reqwest::blocking::Client, url: &str) -> reqwest::blocking::RequestBuilder {
    client
        .post(url)
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}))
}
fn entry(m: &Manager, w: &Workspace) -> serde_json::Value {
    let config: serde_json::Value =
        serde_json::from_str(&m.connection_config(&w.id, false).unwrap()).unwrap();
    config["mcpServers"][&w.name].clone()
}
fn challenge(response: &reqwest::blocking::Response) -> &str {
    response.headers()["www-authenticate"].to_str().unwrap()
}
#[test]
fn url_token_is_enforced_before_the_core_and_never_forwarded() {
    let (_test_guard, d, m) = manager();
    let mut w = workspace(d.path());
    w.auth = "url-token".into();
    // Saved as typed, including what a bearer header could not carry.
    let token = " url+tökén/=&x ";
    let w = m
        .save_workspace(
            w,
            Some(Secrets {
                bearer_token: token.into(),
                ..Default::default()
            }),
        )
        .unwrap();
    let status = m.start(&w.id).unwrap();
    assert_eq!(status.local_state, "ready", "{}", status.local_message);
    let base = format!("http://127.0.0.1:{}", w.port);
    let client = core::client().unwrap();

    for url in [
        format!("{base}/mcp"),
        format!("{base}/mcp?token="),
        format!("{base}/mcp?token=url%2Bt%C3%B6k%C3%A9n%2F%3D%26x"),
        format!("{base}/mcp?Token=+url%2Bt%C3%B6k%C3%A9n%2F%3D%26x+"),
    ] {
        let denied = initialize(&client, &url).send().unwrap();
        assert_eq!(denied.status(), 401, "{url}");
        assert!(challenge(&denied).starts_with("Bearer "));
        assert!(denied.headers().get("x-fixture-target").is_none());
    }
    assert_eq!(
        client.get(format!("{base}/mcp")).send().unwrap().status(),
        401
    );
    let basic = initialize(&client, &format!("{base}/mcp"))
        .basic_auth("ctc", Some(token))
        .send()
        .unwrap();
    assert_eq!(basic.status(), 401);

    let entry = entry(&m, &w);
    let url = entry["url"].as_str().unwrap();
    assert_eq!(
        url,
        format!("{base}/mcp?token=+url%2Bt%C3%B6k%C3%A9n%2F%3D%26x+")
    );
    assert!(entry["headers"].is_null());
    let accepted = initialize(&client, url).send().unwrap();
    assert_eq!(accepted.status(), 200);
    assert_eq!(accepted.headers()["x-fixture-target"], "/mcp");
    let mixed = initialize(&client, &format!("{url}&cursor=a%20b"))
        .send()
        .unwrap();
    assert_eq!(mixed.headers()["x-fixture-target"], "/mcp?cursor=a%20b");

    // Discovery stays public, and is forwarded without the core's credential.
    let discovery = client
        .get(format!("{base}/.well-known/mcp.json"))
        .send()
        .unwrap();
    assert_eq!(discovery.status(), 200);
    assert_eq!(discovery.headers()["x-fixture-credential"], "0");
    let spoofed = client
        .get(format!("{base}/.well-known/mcp.json"))
        .bearer_auth("guess")
        .send()
        .unwrap();
    assert_eq!(spoofed.headers()["x-fixture-credential"], "0");

    assert_eq!(m.auth_details(&w.id).unwrap()["urlToken"], token);
    assert_eq!(m.restart(&w.id).unwrap().local_state, "ready");
    assert_eq!(initialize(&client, url).send().unwrap().status(), 200);
    m.stop_all().unwrap();
    assert!(TcpListener::bind(("127.0.0.1", w.port)).is_ok());
}
#[test]
fn basic_is_enforced_before_the_core() {
    let (_test_guard, d, m) = manager();
    let mut w = workspace(d.path());
    // The bearer token was handed to clients before the switch to Basic.
    w.auth = "bearer".into();
    let shared = "token-shared-before-the-switch";
    let secrets = Secrets {
        bearer_token: shared.into(),
        ..Default::default()
    };
    let mut w = m.save_workspace(w, Some(secrets)).unwrap();
    w.auth = "basic".into();
    w.basic_username = "  ".into();
    let w = m.save_workspace(w, None).unwrap();
    assert_eq!(w.basic_username, "ctc");
    let details = m.auth_details(&w.id).unwrap();
    assert_eq!(details["basicUsername"], "ctc");
    let password = details["basicPassword"].as_str().unwrap().to_owned();
    assert!(password.len() >= 32);

    let status = m.start(&w.id).unwrap();
    assert_eq!(status.local_state, "ready", "{}", status.local_message);
    let url = format!("http://127.0.0.1:{}/mcp", w.port);
    let client = core::client().unwrap();
    let attempts: [(&str, Option<&str>); 4] = [
        ("ctc", Some("wrong")),
        ("other", Some(&password)),
        ("ctc", None),
        ("", Some(&password)),
    ];
    for (user, pass) in attempts {
        let denied = initialize(&client, &url)
            .basic_auth(user, pass)
            .send()
            .unwrap();
        assert_eq!(denied.status(), 401, "{user}");
        assert!(challenge(&denied).starts_with("Basic "));
    }
    let anonymous = initialize(&client, &url).send().unwrap();
    assert!(challenge(&anonymous).starts_with("Basic "));
    // The core's own token is not a way around the password.
    let bearer = initialize(&client, &url)
        .bearer_auth(&password)
        .send()
        .unwrap();
    assert_eq!(bearer.status(), 401);
    let query = initialize(&client, &format!("{url}?token={password}"))
        .send()
        .unwrap();
    assert_eq!(query.status(), 401);

    let accepted = initialize(&client, &url)
        .basic_auth("ctc", Some(&password))
        .send()
        .unwrap();
    assert_eq!(accepted.status(), 200);
    // Neither that old token nor the password opens the core's own port.
    let core_port = accepted.headers()["x-fixture-port"].to_str().unwrap();
    assert_ne!(core_port, w.port.to_string());
    for stale in [shared, password.as_str()] {
        let direct = initialize(&client, &format!("http://127.0.0.1:{core_port}/mcp"))
            .bearer_auth(stale)
            .send()
            .unwrap();
        assert_eq!(direct.status(), 401);
    }
    let entry = entry(&m, &w);
    assert_eq!(entry["url"], url);
    let configured = initialize(&client, &url)
        .header(
            "Authorization",
            entry["headers"]["Authorization"].as_str().unwrap(),
        )
        .send()
        .unwrap();
    assert_eq!(configured.status(), 200);
    assert_eq!(
        client
            .get(format!("http://127.0.0.1:{}/.well-known/mcp.json", w.port))
            .send()
            .unwrap()
            .status(),
        200
    );
    assert!(!m.export_diagnostics(&w.id).unwrap().contains(&password));

    m.stop(&w.id).unwrap();
    let mut edited = w.clone();
    edited.basic_username = "team:one".into();
    assert!(m.save_workspace(edited.clone(), None).is_err());
    edited.basic_username = "team".into();
    let custom = "pä:ss wörd";
    let w = m
        .save_workspace(
            edited,
            Some(Secrets {
                basic_password: custom.into(),
                ..Default::default()
            }),
        )
        .unwrap();
    m.start(&w.id).unwrap();
    let old = initialize(&client, &url)
        .basic_auth("ctc", Some(&password))
        .send()
        .unwrap();
    assert_eq!(old.status(), 401);
    let new = initialize(&client, &url)
        .basic_auth("team", Some(custom))
        .send()
        .unwrap();
    assert_eq!(new.status(), 200);
    m.stop_all().unwrap();
}
#[test]
fn gateway_port_is_released_when_the_core_crashes_or_fails_to_start() {
    let (_test_guard, d, m) = manager();
    let mut w = workspace(d.path());
    w.auth = "basic".into();
    let w = m.save_workspace(w, None).unwrap();
    let status = m.start(&w.id).unwrap();
    let mut sys = sysinfo::System::new_all();
    sys.refresh_all();
    assert!(sys
        .process(sysinfo::Pid::from_u32(status.pid.unwrap()))
        .unwrap()
        .kill());
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        m.refresh();
        let status = m.snapshot().statuses.remove(0);
        if status.state == "error" && !status.cleanup_pending {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "Crash was not observed"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(TcpListener::bind(("127.0.0.1", w.port)).is_ok());
    assert_eq!(m.start(&w.id).unwrap().state, "running");
    m.stop_all().unwrap();

    let mut broken = w.clone();
    broken.core_command = vec![python(), "-c".into(), "raise SystemExit(3)".into()];
    let w = m.save_workspace(broken, None).unwrap();
    assert!(m.start(&w.id).is_err());
    assert!(TcpListener::bind(("127.0.0.1", w.port)).is_ok());
}
#[test]
fn profiles_saved_before_the_new_methods_keep_bearer_behaviour() {
    let old: Workspace = serde_json::from_value(
        json!({"id":"a","name":"Old","path":"/tmp","port":1,"access":"quick","auth":"bearer"}),
    )
    .unwrap();
    assert_eq!(
        (old.auth.as_str(), old.basic_username.as_str()),
        ("bearer", "ctc")
    );
    let secrets: Secrets = serde_json::from_value(json!({"bearerToken":"kept"})).unwrap();
    assert!(secrets.basic_password.is_empty());

    let (_test_guard, d, m) = manager();
    let mut w = workspace(d.path());
    w.auth = "bearer".into();
    let w = m
        .save_workspace(
            w,
            Some(Secrets {
                bearer_token: "kept".into(),
                ..Default::default()
            }),
        )
        .unwrap();
    m.start(&w.id).unwrap();
    let url = format!("http://127.0.0.1:{}/mcp", w.port);
    let entry = entry(&m, &w);
    assert_eq!(entry["url"], url);
    assert_eq!(entry["headers"]["Authorization"], "Bearer kept");
    let client = core::client().unwrap();
    // No gateway: the token in a URL means nothing to the core.
    let query = initialize(&client, &format!("{url}?token=kept"))
        .send()
        .unwrap();
    assert_eq!(query.status(), 401);
    let header = initialize(&client, &url)
        .bearer_auth("kept")
        .send()
        .unwrap();
    assert_eq!(header.status(), 200);
    m.stop_all().unwrap();

    // Quick Tunnel now accepts the header-free methods; OAuth still needs a fixed address.
    for (auth, allowed) in [("url-token", true), ("basic", true), ("oauth", false)] {
        let mut quick = w.clone();
        quick.access = "quick".into();
        quick.auth = auth.into();
        assert_eq!(m.save_workspace(quick, None).is_ok(), allowed, "{auth}");
    }
}

// Run explicitly against an installed, unmodified official core, never import its internals.
#[test]
#[ignore = "Set DESKTOP_CORE_COMMAND_JSON to the official external core command"]
fn official_core_mcp_acceptance() {
    let command: Vec<String> = serde_json::from_str(
        &std::env::var("DESKTOP_CORE_COMMAND_JSON").expect("set DESKTOP_CORE_COMMAND_JSON"),
    )
    .unwrap();
    eprintln!("[core acceptance] prepare isolated fixture");
    let log_dir = std::env::var_os("DESKTOP_ACCEPTANCE_LOG_DIR");
    let (_test_guard, d, m) = if let Some(path) = log_dir {
        std::fs::create_dir_all(&path).unwrap();
        // CI collects only redacted runtime logs, not the whole fixture.
        manager_in(
            tempfile::Builder::new()
                .disable_cleanup(true)
                .tempdir_in(path)
                .unwrap(),
        )
    } else {
        manager()
    };
    let project = d.path().join("项目 空间 (MCP)");
    std::fs::create_dir(&project).unwrap();
    std::fs::write(project.join("hello.txt"), "desktop acceptance\n").unwrap();
    let mut w = workspace(&project);
    w.core_command = command;
    let w = m.save_workspace(w, None).unwrap();
    eprintln!("[core acceptance] start core");
    let status = m.start(&w.id).unwrap();
    eprintln!("[core acceptance] start returned {}", status.local_state);
    assert_eq!(status.local_state, "ready");
    let c = core::client().unwrap();
    let endpoint = format!("http://127.0.0.1:{}/mcp", w.port);
    let rpc = |id: u32, method: &str, params: serde_json::Value| -> serde_json::Value {
        eprintln!("[core acceptance] RPC {id} {method}");
        let response = c
            .post(&endpoint)
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", "2025-11-25")
            .json(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .send()
            .unwrap();
        let status = response.status();
        let text = response.text().unwrap();
        assert!(status.is_success(), "{method}: {status}: {text}");
        let payload = serde_json::from_str(&text).unwrap();
        eprintln!("[core acceptance] RPC {id} returned HTTP {status}");
        payload
    };
    let init = rpc(
        1,
        "initialize",
        json!({"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"desktop-acceptance","version":"1"}}),
    );
    assert_eq!(init["result"]["serverInfo"]["name"], "coding-tools-mcp");
    let list = rpc(2, "tools/list", json!({}));
    assert!(list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["name"] == "read_file"));
    let ok = rpc(
        3,
        "tools/call",
        json!({"name":"read_file","arguments":{"path":"hello.txt"}}),
    );
    assert_ne!(ok["result"]["isError"], true);
    assert!(ok.to_string().contains("desktop acceptance"));
    let fail = rpc(
        4,
        "tools/call",
        json!({"name":"read_file","arguments":{"path":"missing-file.txt"}}),
    );
    assert_eq!(fail["result"]["isError"], true);
    let expect_activity = std::env::var("DESKTOP_EXPECT_ACTIVITY").as_deref() != Ok("false");
    eprintln!("[core acceptance] read activity and run diagnostics");
    let activity = m.activity(&w.id).unwrap();
    if expect_activity {
        assert_eq!(m.snapshot().statuses[0].activity_state, "available");
        assert_eq!(activity.len(), 2);
        assert!(activity
            .iter()
            .any(|a| a.outcome == "success" && a.duration_ms.is_some()));
        assert!(activity.iter().any(|a| a.outcome == "tool_error"));
        assert!(m
            .diagnose(&w.id)
            .unwrap()
            .iter()
            .any(|d| d.name == "Tool activity" && d.level == "ok"));
    } else {
        assert_eq!(m.snapshot().statuses[0].activity_state, "unavailable");
        assert!(activity.is_empty());
        let diagnostics = m.diagnose(&w.id).unwrap();
        let history = diagnostics
            .iter()
            .find(|check| check.name == "Tool activity")
            .expect("diagnostics explain why tool history is unavailable");
        assert_eq!(history.level, "warning");
        assert!(history
            .message
            .contains("does not record tool-call history"));
        assert!(history.message.contains("You can still connect and use it"));
    }
    // A real restart must release the prior journal/process, preserve history,
    // and expose only the capabilities of the newly launched core.
    eprintln!("[core acceptance] restart core");
    let restarted = m.restart(&w.id).unwrap();
    eprintln!(
        "[core acceptance] restart returned {}",
        restarted.local_state
    );
    assert_eq!(restarted.local_state, "ready");
    assert_eq!(
        restarted.activity_state,
        if expect_activity {
            "available"
        } else {
            "unavailable"
        }
    );
    let restored = m.activity(&w.id).unwrap();
    assert_eq!(restored.len(), activity.len());
    let init = rpc(
        5,
        "initialize",
        json!({"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"desktop-restart-acceptance","version":"1"}}),
    );
    assert_eq!(init["result"]["serverInfo"]["name"], "coding-tools-mcp");
    let after_restart = rpc(
        6,
        "tools/call",
        json!({"name":"read_file","arguments":{"path":"hello.txt"}}),
    );
    assert_ne!(after_restart["result"]["isError"], true);
    assert!(after_restart.to_string().contains("desktop acceptance"));
    let combined = m.activity(&w.id).unwrap();
    if expect_activity {
        assert_eq!(combined.len(), 3);
        assert_eq!(
            combined
                .iter()
                .map(|row| &row.runtime_id)
                .collect::<std::collections::HashSet<_>>()
                .len(),
            2
        );
    } else {
        assert!(combined.is_empty());
    }
    eprintln!("[core acceptance] stop and verify port release");
    m.stop_all().unwrap();
    assert!(TcpListener::bind(("127.0.0.1", w.port)).is_ok());
    eprintln!("[core acceptance] assertions complete");
}

// The gateway-enforced methods against the same unmodified official core.
#[test]
#[ignore = "Set DESKTOP_CORE_COMMAND_JSON to the official external core command"]
fn official_core_mcp_acceptance_behind_the_gateway() {
    let command: Vec<String> = serde_json::from_str(
        &std::env::var("DESKTOP_CORE_COMMAND_JSON").expect("set DESKTOP_CORE_COMMAND_JSON"),
    )
    .unwrap();
    let (_test_guard, d, m) = manager();
    let project = d.path().join("gateway project");
    std::fs::create_dir(&project).unwrap();
    std::fs::write(project.join("hello.txt"), "gateway acceptance\n").unwrap();
    let c = core::client().unwrap();
    for auth in ["url-token", "basic"] {
        eprintln!("[core acceptance] {auth}: start core behind the gateway");
        let mut w = workspace(&project);
        w.name = auth.into();
        w.auth = auth.into();
        w.core_command = command.clone();
        let w = m.save_workspace(w, None).unwrap();
        let status = m.start(&w.id).unwrap();
        assert_eq!(status.local_state, "ready", "{}", status.local_message);
        let base = format!("http://127.0.0.1:{}", w.port);
        let rpc = |url: &str, authorization: Option<&str>, method: &str, params| {
            let mut request = c
                .post(url)
                .header("Accept", "application/json, text/event-stream")
                .header("MCP-Protocol-Version", "2025-11-25")
                .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}));
            if let Some(value) = authorization {
                request = request.header("Authorization", value);
            }
            request.send().unwrap()
        };
        let init = json!({"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"desktop-gateway-acceptance","version":"1"}});
        assert_eq!(
            rpc(&format!("{base}/mcp"), None, "initialize", init.clone()).status(),
            401
        );
        assert!(c
            .get(format!("{base}/.well-known/mcp.json"))
            .send()
            .unwrap()
            .status()
            .is_success());
        // Connect exactly as the copied client configuration says to.
        let entry = entry(&m, &w);
        let url = entry["url"].as_str().unwrap();
        let authorization = entry["headers"]["Authorization"].as_str();
        assert_eq!(authorization.is_some(), auth == "basic");
        let response = rpc(url, authorization, "initialize", init);
        assert!(response.status().is_success(), "{}", response.status());
        let payload: serde_json::Value = response.json().unwrap();
        assert_eq!(payload["result"]["serverInfo"]["name"], "coding-tools-mcp");
        let read = rpc(
            url,
            authorization,
            "tools/call",
            json!({"name":"read_file","arguments":{"path":"hello.txt"}}),
        );
        assert!(read.status().is_success(), "{}", read.status());
        assert!(read.text().unwrap().contains("gateway acceptance"));
        m.stop(&w.id).unwrap();
        assert!(TcpListener::bind(("127.0.0.1", w.port)).is_ok());
    }
    eprintln!("[core acceptance] gateway assertions complete");
}

#[test]
fn deleted_workspace_cannot_be_resurrected_by_a_stale_save() {
    let (_test_guard, dir, manager) = manager();
    let workspace = manager.save_workspace(workspace(dir.path()), None).unwrap();
    manager.delete_workspace(&workspace.id).unwrap();
    let backup = std::fs::read(manager.storage.home.join("desktop-before-delete.json")).unwrap();
    assert!(manager
        .save_workspace(workspace.clone(), None)
        .unwrap_err()
        .to_string()
        .contains("removed"));
    assert!(manager.start(&workspace.id).is_err());
    assert!(manager.delete_workspace(&workspace.id).is_err());
    assert!(manager.snapshot().workspaces.is_empty());
    assert_eq!(
        backup,
        std::fs::read(manager.storage.home.join("desktop-before-delete.json")).unwrap()
    );
}

#[test]
fn successful_shutdown_closes_admission_but_stop_all_does_not() {
    let (_test_guard, dir, manager) = manager();
    let workspace = manager.save_workspace(workspace(dir.path()), None).unwrap();
    manager.start(&workspace.id).unwrap();
    manager.stop_all().unwrap();
    manager.start(&workspace.id).unwrap();
    manager.shutdown().unwrap();
    assert!(manager
        .start(&workspace.id)
        .unwrap_err()
        .to_string()
        .contains("shutting down"));
    assert!(manager.save_workspace(workspace.clone(), None).is_err());
    assert!(manager.delete_workspace(&workspace.id).is_err());
    assert!(manager
        .install_core("0.5.0")
        .unwrap_err()
        .to_string()
        .contains("shutting down"));
    assert!(manager
        .cloudflare_login()
        .unwrap_err()
        .to_string()
        .contains("shutting down"));
    assert_eq!(manager.snapshot().statuses[0].state, "stopped");
    assert!(TcpListener::bind(("127.0.0.1", workspace.port)).is_ok());
}

#[test]
fn workspace_name_limit_counts_unicode_characters_not_utf8_bytes() {
    let (_test_guard, dir, manager) = manager();
    let mut value = workspace(dir.path());
    value.name = "项目".repeat(40);
    let saved = manager.save_workspace(value, None).unwrap();
    assert_eq!(saved.name.chars().count(), 80);
    let mut too_long = saved.clone();
    too_long.name = "项".repeat(121);
    assert!(manager.save_workspace(too_long, None).is_err());
    assert_eq!(manager.snapshot().workspaces[0].name, saved.name);
}
