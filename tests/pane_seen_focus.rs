//! E2E tests for focused-pane seen semantics: entering a tab must not clear
//! the Done status of unfocused sibling agent panes, neither in the server's
//! authoritative pane state nor in the attached client's projected rendering.
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use serde_json::{json, Value};
pub mod support;
#[path = "support/terminal_screen.rs"]
mod terminal_screen;
use support::{
    cleanup_test_base, register_runtime_dir, register_spawned_herdr_pid,
    stop_spawned_herdr, unregister_spawned_herdr_pid, isolate_herdr_test_process,
};

fn unique_test_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    PathBuf::from(format!(
        "/tmp/herdr-pane-seen-test-{}-{nanos}",
        std::process::id()
    ))
}

struct SpawnedHerdr {
    _master: Option<Box<dyn MasterPty + Send>>,
    child: Box<dyn Child + Send + Sync>,
}

impl SpawnedHerdr {
    fn close_master(&mut self) {
        drop(self._master.take());
    }
}

impl Drop for SpawnedHerdr {
    fn drop(&mut self) {
        let pid = self.child.process_id();
        stop_spawned_herdr(&mut *self.child);
        self.close_master();

        if let Some(pid) = pid {
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                let mut status = 0;
                let result =
                    unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) };
                if result == pid as libc::pid_t || result == -1 {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }

            unregister_spawned_herdr_pid(Some(pid));
        }
    }
}

fn cleanup_spawned_herdr(spawned: SpawnedHerdr, base: PathBuf) {
    drop(spawned);
    cleanup_test_base(&base);
}

fn test_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn wait_for_socket(path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() && UnixStream::connect(path).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("socket did not appear at {}", path.display());
}

fn write_fake_pi(bin_dir: &Path) {
    fs::create_dir_all(bin_dir).unwrap();
    let fake_pi = bin_dir.join("pi");
    fs::write(&fake_pi, "#!/bin/sh\nprintf 'Working...\\n'\nsleep 8\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&fake_pi).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&fake_pi, perms).unwrap();
    }
}

fn spawn_server(
    config_home: &Path,
    runtime_dir: &Path,
    api_socket_path: &Path,
    config_toml: &str,
    path_override: Option<&str>,
) -> SpawnedHerdr {
    fs::create_dir_all(config_home.join("herdr")).unwrap();
    fs::create_dir_all(runtime_dir).unwrap();
    register_runtime_dir(runtime_dir);
    fs::write(config_home.join("herdr/config.toml"), config_toml).unwrap();

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_herdr"));
    isolate_herdr_test_process(&mut cmd);
    cmd.arg("server");
    cmd.env("XDG_STATE_HOME", runtime_dir.join("state"));
    cmd.env("XDG_CONFIG_HOME", config_home);
    cmd.env("XDG_RUNTIME_DIR", runtime_dir);
    cmd.env("HERDR_SOCKET_PATH", api_socket_path);
    cmd.env_remove("HERDR_CLIENT_SOCKET_PATH");
    cmd.env("SHELL", "/bin/sh");
    cmd.env_remove("HERDR_ENV");
    if let Some(path) = path_override {
        cmd.env("PATH", path);
    }

    let child = pair.slave.spawn_command(cmd).unwrap();
    register_spawned_herdr_pid(child.process_id());
    drop(pair.slave);

    SpawnedHerdr {
        _master: Some(pair.master),
        child,
    }
}

fn spawn_client_process(
    config_home: &Path,
    runtime_dir: &Path,
    api_socket_path: &Path,
) -> SpawnedHerdr {
    register_runtime_dir(runtime_dir);
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_herdr"));
    isolate_herdr_test_process(&mut cmd);
    cmd.arg("client");
    cmd.env("HERDR_DISABLE_SOUND", "1");
    cmd.env("XDG_STATE_HOME", runtime_dir.join("state"));
    cmd.env("XDG_CONFIG_HOME", config_home);
    cmd.env("XDG_RUNTIME_DIR", runtime_dir);
    cmd.env("HERDR_SOCKET_PATH", api_socket_path);
    cmd.env_remove("HERDR_CLIENT_SOCKET_PATH");
    cmd.env("SHELL", "/bin/sh");
    cmd.env_remove("HERDR_ENV");

    let child = pair.slave.spawn_command(cmd).unwrap();
    register_spawned_herdr_pid(child.process_id());
    drop(pair.slave);

    SpawnedHerdr {
        _master: Some(pair.master),
        child,
    }
}

// ---------------------------------------------------------------------------
// API helpers (newline-delimited JSON over the API socket)
// ---------------------------------------------------------------------------

fn send_json_request(socket_path: &Path, id: &str, method: &str, params: Value) -> Value {
    let mut stream = UnixStream::connect(socket_path).expect("should connect to API socket");
    let request = json!({
        "id": id,
        "method": method,
        "params": params
    });
    writeln!(stream, "{}", request).unwrap();

    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    reader.read_line(&mut response).unwrap();
    serde_json::from_str(&response).expect("response should be valid JSON")
}

fn pane_send_input(socket_path: &Path, pane_id: &str, text: &str) {
    let response = send_json_request(
        socket_path,
        "pane_send_input",
        "pane.send_input",
        json!({
            "pane_id": pane_id,
            "text": text,
            "keys": ["Enter"]
        }),
    );
    assert!(
        response.get("error").is_none(),
        "pane.send_input should succeed: {response}"
    );
}

fn pane_send_text(socket_path: &Path, pane_id: &str, text: &str) {
    let response = send_json_request(
        socket_path,
        "pane_send_text",
        "pane.send_text",
        json!({
            "pane_id": pane_id,
            "text": text
        }),
    );
    assert!(
        response.get("error").is_none(),
        "pane.send_text should succeed: {response}"
    );
}

fn pane_report_agent(socket_path: &Path, pane_id: &str, agent: &str, state: &str) {
    let response = send_json_request(
        socket_path,
        "pane_report_agent",
        "pane.report_agent",
        json!({
            "pane_id": pane_id,
            "agent": agent,
            "state": state,
            "source": "pane-seen-e2e",
        }),
    );
    assert!(
        response.get("error").is_none(),
        "pane.report_agent should succeed: {response}"
    );
}

fn pane_agent_status(socket_path: &Path, pane_id: &str) -> Option<String> {
    let response = send_json_request(
        socket_path,
        "pane_get",
        "pane.get",
        json!({ "pane_id": pane_id }),
    );
    response["result"]["pane"]["agent_status"]
        .as_str()
        .map(|status| status.to_string())
}

fn pane_agent(socket_path: &Path, pane_id: &str) -> Option<String> {
    let response = send_json_request(
        socket_path,
        "pane_get",
        "pane.get",
        json!({ "pane_id": pane_id }),
    );
    response["result"]["pane"]["agent"]
        .as_str()
        .map(|agent| agent.to_string())
}

fn wait_for(
    timeout: Duration,
    poll: Duration,
    mut predicate: impl FnMut() -> bool,
) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        thread::sleep(poll);
    }
    false
}

fn wait_for_agent_status(socket_path: &Path, pane_id: &str, expected: &str) -> bool {
    wait_for(Duration::from_secs(5), Duration::from_millis(50), || {
        pane_agent_status(socket_path, pane_id).as_deref() == Some(expected)
    })
}

fn wait_for_agent_detected(socket_path: &Path, pane_id: &str, expected: &str) -> bool {
    wait_for(Duration::from_secs(8), Duration::from_millis(60), || {
        pane_agent(socket_path, pane_id).as_deref() == Some(expected)
    })
}

/// Creates a workspace whose tab holds two panes: `root_pane` (layout focus)
/// and an unfocused sibling that runs a detectable fake `pi` agent.
fn setup_workspace_with_sibling_agent(
    api_socket: &Path,
) -> (String, String, String, String) {
    let created = send_json_request(
        api_socket,
        "workspace_create",
        "workspace.create",
        json!({ "label": "agents", "focus": true }),
    );
    assert!(
        created.get("error").is_none(),
        "workspace.create should succeed: {created}"
    );
    let workspace_id = created["result"]["workspace"]["workspace_id"]
        .as_str()
        .expect("workspace id")
        .to_string();
    let root_pane = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .expect("root pane id")
        .to_string();

    let agents_tab = send_json_request(
        api_socket,
        "tab_list",
        "tab.list",
        json!({ "workspace_id": workspace_id }),
    );
    let agents_tab_id = agents_tab["result"]["tabs"][0]["tab_id"]
        .as_str()
        .expect("agents tab id")
        .to_string();

    let split = send_json_request(
        api_socket,
        "pane_split",
        "pane.split",
        json!({
            "workspace_id": workspace_id,
            "target_pane_id": root_pane,
            "direction": "right",
            "focus": false,
        }),
    );
    assert!(
        split.get("error").is_none(),
        "pane.split should succeed: {split}"
    );
    let sibling = split["result"]["pane"]["pane_id"]
        .as_str()
        .expect("sibling pane id")
        .to_string();

    // Start the fake pi in the sibling so the pane is a detectable agent.
    pane_send_text(api_socket, &sibling, "pi");
    pane_send_input(api_socket, &sibling, "");

    assert!(
        wait_for_agent_detected(api_socket, &sibling, "pi"),
        "fake pi should be detected in the sibling pane"
    );

    (workspace_id, root_pane, agents_tab_id, sibling)
}

/// Shared prefix: workspace with root + sibling agent pane, plus a focused
/// "other" tab so the agents tab is inactive while the agent completes.
fn setup_with_inactive_agents_tab(
    base: &Path,
    config_toml: &str,
) -> (SpawnedHerdr, PathBuf, PathBuf, PathBuf, String, String, String) {
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");

    let bin_dir = base.join("bin");
    write_fake_pi(&bin_dir);
    let inherited_path = std::env::var("PATH").unwrap_or_default();
    let path_override = format!("{}:{}", bin_dir.display(), inherited_path);

    let server = spawn_server(
        &config_home,
        &runtime_dir,
        &api_socket,
        config_toml,
        Some(&path_override),
    );
    wait_for_socket(&api_socket, Duration::from_secs(10));
    wait_for_socket(&client_socket, Duration::from_secs(10));

    let (workspace_id, root_pane, agents_tab_id, sibling) =
        setup_workspace_with_sibling_agent(&api_socket);

    let other_tab = send_json_request(
        &api_socket,
        "tab_create",
        "tab.create",
        json!({
            "workspace_id": workspace_id,
            "label": "other",
            "focus": true,
        }),
    );
    assert!(
        other_tab.get("error").is_none(),
        "tab.create should succeed: {other_tab}"
    );

    (
        server,
        config_home,
        runtime_dir,
        api_socket,
        root_pane,
        agents_tab_id,
        sibling,
    )
}

fn complete_sibling_as_done(api_socket: &Path, sibling: &str) {
    pane_report_agent(api_socket, sibling, "pi", "working");
    assert!(
        wait_for_agent_status(api_socket, sibling, "working"),
        "sibling agent should report working before completing"
    );
    pane_report_agent(api_socket, sibling, "pi", "idle");
    assert!(
        wait_for_agent_status(api_socket, sibling, "done"),
        "sibling completion while its tab is inactive should read as done"
    );
}

// ---------------------------------------------------------------------------
// Server contract
// ---------------------------------------------------------------------------

#[test]
fn entering_tab_keeps_unfocused_sibling_agent_done() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let (server, _config_home, _runtime_dir, api_socket, root_pane, agents_tab_id, sibling) =
        setup_with_inactive_agents_tab(&base, "onboarding = false\n");

    complete_sibling_as_done(&api_socket, &sibling);

    // Enter the agents tab. The unfocused sibling must keep its done status;
    // only the pane the user lands on (the focused root pane) is acknowledged.
    let focused = send_json_request(
        &api_socket,
        "tab_focus",
        "tab.focus",
        json!({ "tab_id": agents_tab_id }),
    );
    assert!(
        focused.get("error").is_none(),
        "tab.focus should succeed: {focused}"
    );

    assert!(
        wait_for(Duration::from_millis(500), Duration::from_millis(50), || {
            pane_agent_status(&api_socket, &sibling).as_deref() == Some("done")
        }),
        "entering the tab must keep the unfocused sibling agent done (status: {:?})",
        pane_agent_status(&api_socket, &sibling)
    );

    // Focusing the sibling is what acknowledges it.
    let focus_pane = send_json_request(
        &api_socket,
        "pane_focus",
        "pane.focus",
        json!({ "pane_id": sibling }),
    );
    assert!(
        focus_pane.get("error").is_none(),
        "pane.focus should succeed: {focus_pane}"
    );
    assert!(
        wait_for_agent_status(&api_socket, &sibling, "idle"),
        "focusing the sibling should acknowledge its completion"
    );
    let _ = root_pane;

    cleanup_spawned_herdr(server, base);
}

// ---------------------------------------------------------------------------
// Attached client projection (real TUI in a PTY)
// ---------------------------------------------------------------------------

type SharedOutput = std::sync::Arc<Mutex<PtyOutput>>;

#[derive(Default)]
struct PtyOutput {
    bytes: Vec<u8>,
    text: String,
}

fn spawn_pty_drain(mut reader: Box<dyn Read + Send>) -> SharedOutput {
    let output: SharedOutput = std::sync::Arc::new(Mutex::new(PtyOutput::default()));
    let thread_output = output.clone();
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let mut captured = thread_output.lock().unwrap_or_else(|p| p.into_inner());
                    captured.bytes.extend_from_slice(&buf[..n]);
                    captured.text.push_str(&String::from_utf8_lossy(&buf[..n]));
                }
                Err(_) => break,
            }
        }
    });
    output
}

fn read_output(output: &SharedOutput) -> String {
    output
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .text
        .clone()
}

/// Renders the client's current visible screen from the accumulated PTY
/// bytes. The raw byte stream is cumulative, so symbols from earlier frames
/// never disappear from it; assertions must run against the live screen.
fn current_screen(output: &SharedOutput) -> String {
    let bytes = output
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .bytes
        .clone();
    terminal_screen::text(&bytes, 100, 30)
}

#[test]
fn client_rendering_keeps_sibling_done_when_entering_tab() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let (server, config_home, runtime_dir, api_socket, _root_pane, agents_tab_id, sibling) =
        setup_with_inactive_agents_tab(
            &base,
            "onboarding = false\n[theme]\nname = \"terminal\"\n[ui]\nstatus_indicators = \"symbols\"\n",
        );

    // Attach a real client TUI while the "other" tab is active. The client's
    // first-snapshot baseline acknowledges pre-existing states, so the
    // completion below happens while the client is already attached.
    let client = spawn_client_process(&config_home, &runtime_dir, &api_socket);
    let master = client._master.as_ref().expect("client PTY");
    let output = spawn_pty_drain(master.try_clone_reader().unwrap());
    assert!(
        wait_for(Duration::from_secs(10), Duration::from_millis(50), || {
            read_output(&output).contains("other")
        }),
        "client should render the active other tab"
    );

    // The sibling agent completes while its tab is not the active one.
    complete_sibling_as_done(&api_socket, &sibling);
    assert!(
        wait_for(Duration::from_secs(5), Duration::from_millis(50), || {
            current_screen(&output).contains('✓')
        }),
        "client sidebar should show the done symbol for the completed sibling"
    );

    // Enter the agents tab via the API; the client follows the server's
    // active tab and renders the agents surface. Rendering the sibling pane
    // must not acknowledge it: the done symbol has to survive.
    let focused = send_json_request(
        &api_socket,
        "tab_focus",
        "tab.focus",
        json!({ "tab_id": agents_tab_id }),
    );
    assert!(
        focused.get("error").is_none(),
        "tab.focus should succeed: {focused}"
    );

    assert!(
        wait_for(Duration::from_secs(3), Duration::from_millis(50), || {
            current_screen(&output).contains("Working...")
        }),
        "client should render the agents tab surface (fake pi output visible)"
    );
    thread::sleep(Duration::from_millis(800));
    let screen = current_screen(&output);
    assert!(
        screen.contains('✓'),
        "entering the tab must not clear the sibling's done symbol in the client sidebar; screen:\n{screen}"
    );

    drop(client);
    cleanup_spawned_herdr(server, base);
}
