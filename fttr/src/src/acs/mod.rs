//! Optional, bounded CWMP endpoint. It never takes a PON/OMCI lock.
mod actor;
mod dispatch;
mod endpoint;
mod http;
mod listener;
mod policy;
mod resources;
mod retention;
mod session;
mod state;
use endpoint::stop_listener;
#[cfg(test)]
use endpoint::transition;
pub use endpoint::{configuration_matches, reconfigure, retire_previous};
#[cfg(test)]
mod compact_tests;
#[cfg(test)]
mod concurrency_tests;
mod connection_request;
mod connection_request_http;
mod connection_request_proof;
#[cfg(test)]
mod connection_request_tests;
#[cfg(test)]
mod durability_tests;
mod periodic_policy;
#[cfg(test)]
mod periodic_policy_tests;
mod periodic_task;
mod persistence;
mod readback;
mod soap;
mod task;
mod transaction;
use policy::{protect_management, validate};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read};
#[cfg(test)]
use std::io::{BufReader, Write};
use std::net::TcpListener;
#[cfg(test)]
use std::net::TcpStream;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex, OnceLock,
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use task::{error_result, Task, TaskState};

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn error(message: &str) -> Value {
    json!({"error": message})
}
#[derive(Clone, Serialize, Deserialize)]
struct Device {
    serial: String,
    oui: String,
    product_class: String,
    peer: String,
    namespace: String,
    last_inform: u64,
    #[serde(default)]
    endpoint_id: String,
    inform: Value,
}
#[derive(Clone, Default, Serialize, Deserialize)]
struct State {
    #[serde(skip)]
    budget: Arc<resources::Budget>,
    #[serde(skip)]
    dispatch: dispatch::Dispatcher,
    devices: BTreeMap<String, Device>,
    tasks: BTreeMap<u64, Task>,
    #[serde(default)]
    results: BTreeMap<u64, retention::ResultInfo>,
    next_id: u64,
    #[serde(skip)]
    sessions: BTreeMap<String, (String, String, u64)>,
    #[serde(skip)]
    load_failure: Option<String>,
    #[serde(skip)]
    connection_requests: BTreeMap<String, connection_request::Status>,
    #[serde(skip)]
    periodic_inform: BTreeMap<String, periodic_policy::Policy>,
    #[serde(skip)]
    interval_authorizations: BTreeMap<u64, periodic_task::Authorization>,
    #[serde(skip)]
    connection_request_proofs: BTreeMap<String, connection_request_proof::Proof>,
}
#[derive(Serialize)]
struct TaskView<'a> {
    #[serde(flatten)]
    task: &'a Task,
    #[serde(flatten)]
    result_info: Option<&'a retention::ResultInfo>,
}
struct Server {
    state: Arc<Mutex<State>>,
    enabled: bool,
    listen: String,
    path: String,
    state_file: String,
    auth: Option<String>,
    failure: Mutex<Option<String>>,
    requests: Arc<AtomicUsize>,
    endpoint_id: String,
    drain_deadline: Mutex<Option<std::time::Instant>>,
    legacy: Mutex<Vec<Arc<Server>>>,
    stopping: AtomicBool,
    stopped: AtomicBool,
}
static SERVER: OnceLock<Mutex<Arc<Server>>> = OnceLock::new();
static RECONFIGURE: Mutex<()> = Mutex::new(());
fn env(name: &str, fallback: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| fallback.into())
}
fn init() -> Arc<Server> {
    SERVER
        .get_or_init(|| {
            // A configured address is not authorization to activate a listener.
            let enabled = false;
            let state_file = env("FTTR_ACS_STATE", "/var/lib/fttr/acs-state.json");
            let state = State::load_or_block(&state_file);
            let failure = state.load_failure.clone();
            let user = env("FTTR_ACS_USERNAME", "");
            let password = env("FTTR_ACS_PASSWORD", "");
            Mutex::new(Arc::new(Server {
                state: Arc::new(Mutex::new(state)),
                enabled,
                listen: String::new(),
                path: "/cwmp".into(),
                state_file,
                auth: (!user.is_empty()).then(|| http::basic(&user, &password)),
                failure: Mutex::new(failure),
                requests: Arc::new(AtomicUsize::new(0)),
                endpoint_id: String::new(),
                drain_deadline: Mutex::new(None),
                legacy: Mutex::new(Vec::new()),
                stopping: AtomicBool::new(false),
                stopped: AtomicBool::new(true),
            }))
        })
        .lock()
        .unwrap()
        .clone()
}
impl Server {
    fn execute_value(&self, request: Value) -> Value {
        if request["op"] == "task" {
            let mut state = self.state.lock().unwrap();
            if let Some(reason) = &state.load_failure {
                return error(reason);
            }
            state.expire();
            return match request["id"].as_u64().and_then(|id| state.tasks.get(&id)) {
                Some(task) => serde_json::to_value(TaskView {
                    task,
                    result_info: state.results.get(&task.id),
                })
                .unwrap_or_else(|err| error(&format!("task serialization: {err}"))),
                None => error("unknown task"),
            };
        }
        self.execute_small(request)
    }
    fn execute(&self, request: Value) -> String {
        if request["op"] == "task" {
            let mut state = self.state.lock().unwrap();
            if let Some(reason) = &state.load_failure {
                return error(reason).to_string();
            }
            state.expire();
            return match request["id"].as_u64().and_then(|id| state.tasks.get(&id)) {
                Some(task) => serde_json::to_string(&TaskView {
                    task,
                    result_info: state.results.get(&task.id),
                })
                .unwrap_or_else(|err| error(&format!("task serialization: {err}")).to_string()),
                None => error("unknown task").to_string(),
            };
        }
        self.execute_value(request).to_string()
    }
    fn execute_small(&self, request: Value) -> Value {
        match serde_json::from_value(request) {
            Ok(command) => self.execute_command(command, false),
            Err(_) => error("invalid ACS operation"),
        }
    }
    fn execute_command(&self, command: crate::cwmp::transport::Command, internal: bool) -> Value {
        use crate::cwmp::transport::Command;
        let mut state = self.state.lock().unwrap();
        if !matches!(command, Command::Status | Command::Clean) {
            if let Some(reason) = &state.load_failure {
                return error(reason);
            }
        }
        state.expire();
        match command {
            Command::Clean => {
                state.tasks.retain(|_, t| t.state.is_active());
                let ids: std::collections::BTreeSet<_> = state.tasks.keys().copied().collect();
                state.results.retain(|id, _| ids.contains(id));
                state.load_failure = None;
                *self.failure.lock().unwrap() = None;
                if let Err(err) = self.save(&state) {
                    return error(&format!("cannot persist cleaned state: {err}"));
                }
                json!({"cleaned":true, "tasks":state.tasks.len()})
            }
            Command::Status => {
                let resources = state.resources_view();
                json!({"compiled":true, "enabled":self.enabled && !self.stopped.load(Ordering::Acquire), "listen":self.listen, "path":self.path, "endpoint_id":self.endpoint_id,"auth":self.auth.is_some(), "devices":state.devices.len(), "tasks":state.tasks.len(), "response_bytes":state.budget.used(), "response_budget":resources::RESULT_BYTES, "resources":resources, "waiting_capacity":state.tasks.keys().filter(|id| state.waiting_capacity(**id)).count(), "http_requests":self.requests.load(Ordering::Relaxed), "error":self.failure.lock().unwrap().clone()})
            }
            Command::Devices => {
                json!({"devices":state.devices,"connection_requests":state.connection_requests,"periodic_inform":state.periodic_inform})
            }
            Command::ResetDeviceTasks { device } => {
                if device.is_empty() {
                    return error("device required");
                }
                if let Err(err) = self.reset_tasks(&mut state, &device) {
                    return error(&format!("cannot persist device task reset: {err}"));
                }
                json!({"reset":true})
            }
            Command::Busy { device } => {
                json!({"busy":state.tasks.values().any(|t| t.device == device && t.state.is_active())})
            }
            Command::Queue {
                device,
                method,
                mut args,
            } => {
                if !self.enabled {
                    return error("ACS disabled by UCI");
                }
                if !state.devices.contains_key(&device) {
                    return error("unknown device: a real Inform is required before queuing RPCs; use acs devices");
                }
                if let Err(err) = validate(&method, &args) {
                    return error(&err);
                }
                if let Err(err) = protect_management(&state, &device, &method, &args) {
                    return error(&err);
                }
                if matches!(
                    method.as_str(),
                    "SetParameterValues" | "AddObject" | "DeleteObject"
                ) {
                    args["_omci_auth_epoch"] = json!(crate::omci_auth::cwmp_epoch(&device));
                }
                if state.tasks.values().filter(|t| t.state.is_active()).count() >= 128 {
                    return json!({"error":"ACS queue full", "code":"backpressure", "retryable":true});
                }
                let Some(id) = state.next_id.checked_add(1) else {
                    return error("ACS task IDs exhausted");
                };
                let task = Task {
                    id,
                    device,
                    method,
                    args,
                    state: TaskState::Queued,
                    created: now(),
                    sent: None,
                    result: None,
                };
                self.enqueue_owned(&mut state, task, internal)
            }
            Command::Task { id } => match state.tasks.get(&id) {
                Some(task) => serde_json::to_value(TaskView {
                    task,
                    result_info: state.results.get(&id),
                })
                .unwrap_or_else(|e| error(&e.to_string())),
                None => error("unknown task"),
            },
        }
    }
}
pub fn transact(command: crate::cwmp::transport::Command) -> crate::cwmp::transport::Reply {
    crate::runtime::block_on(async move {
        actor::call(move || {
            let server = init();
            if let crate::cwmp::transport::Command::Task { id } = command {
                let mut state = server.state.lock().unwrap();
                if let Some(reason) = &state.load_failure {
                    return error(reason).into();
                }
                state.expire();
                if let Some(task) = state.tasks.get(&id) {
                    let mut snapshot = task.clone();
                    let response = snapshot.result.take();
                    let mut metadata = serde_json::to_value(TaskView {
                        task: &snapshot,
                        result_info: state.results.get(&id),
                    })
                    .unwrap_or_else(|e| error(&e.to_string()));
                    metadata["queue_state"] = json!(if state.waiting_capacity(id) {
                        "waiting_capacity"
                    } else if task.state == TaskState::Queued {
                        "waiting_device"
                    } else {
                        "ready"
                    });
                    if let Some(response) = response {
                        let reply = response.reply(metadata);
                        if state.ack_result(id) {
                            let _ = server.save(&state);
                        }
                        return reply;
                    }
                    return metadata.into();
                }
                return error("unknown task").into();
            }
            server.execute_command(command, true).into()
        })
        .await
        .unwrap_or_else(|_| error("ACS transaction queue unavailable").into())
    })
}
pub fn control(request: &str) -> String {
    if request.len() > 64 * 1024 {
        error("ACS request too large").to_string()
    } else {
        serde_json::from_str(request)
            .map(|v| {
                crate::runtime::block_on(async move {
                    actor::call(move || init().execute(v))
                        .await
                        .unwrap_or_else(|_| error("ACS transaction queue unavailable").to_string())
                })
            })
            .unwrap_or_else(|_| error("invalid ACS JSON").to_string())
    }
}
pub fn start_from_env() {
    let _ = init();
    connection_request::start();
    println!("[ACS] waiting for authorized management network");
}

pub(crate) fn shutdown() {
    let server = init();
    stop_listener(&server);
    for old in server.legacy.lock().unwrap().iter() {
        stop_listener(old);
    }
}

fn token() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
#[cfg(test)]
fn serve(server: &Arc<Server>, stream: TcpStream) -> io::Result<()> {
    stream.set_nonblocking(true)?;
    crate::runtime::block_on(async {
        http::serve(server.clone(), tokio::net::TcpStream::from_std(stream)?).await
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn save_preserves_completed_results_and_private_mode_when_replacing_state() {
        use std::os::unix::fs::PermissionsExt;
        let server = listener_test_server();
        fs::write(&server.state_file, b"previous state").unwrap();
        let mut state = State::default();
        state.next_id = 7;
        state.tasks.insert(
            7,
            Task {
                id: 7,
                device: "save-test".into(),
                method: "GetParameterValues".into(),
                args: json!({"names":["InternetGatewayDevice."]}),
                state: TaskState::Completed,
                created: 1,
                sent: Some(2),
                result: Some(resources::Response::uncharged(
                    json!({"raw":"\"\\\n".repeat(100_000)}),
                )),
            },
        );
        let result = server.save(&state);
        result.unwrap();
        let saved = fs::read(&server.state_file).unwrap();
        assert_eq!(saved, serde_json::to_vec(&state).unwrap());
        assert_eq!(
            fs::metadata(&server.state_file)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert!(!std::path::Path::new(&format!("{}.tmp", server.state_file)).exists());
        fs::remove_file(&server.state_file).unwrap();
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn save_preserves_previous_state_when_buffer_flush_fails() {
        let server = listener_test_server();
        fs::write(&server.state_file, b"previous state").unwrap();
        let temp = format!("{}.tmp", server.state_file);
        std::os::unix::fs::symlink("/dev/full", &temp).unwrap();
        let result = server.save(&State::default());
        assert!(result.is_err());
        assert_eq!(fs::read(&server.state_file).unwrap(), b"previous state");
        assert!(server
            .failure
            .lock()
            .unwrap()
            .as_deref()
            .unwrap()
            .starts_with("state persistence:"));
        fs::remove_file(temp).unwrap();
        fs::remove_file(&server.state_file).unwrap();
    }
    #[test]
    fn management_writes_and_unowned_wan_are_blocked_even_when_queued() {
        let mut state = State::default();
        for name in ["InternetGatewayDevice.ManagementServer.URL", "InternetGatewayDevice.WANDevice.1.WANConnectionDevice.3.WANIPConnection.1.X_CU_ServiceList"] {
            let args=json!({"parameters":[{"name":name,"value":"TR069"}]});
            assert!(protect_management(&state,"guard-test","SetParameterValues",&args).is_err());
            state.tasks.insert(1,Task{id:1,device:"guard-test".into(),method:"SetParameterValues".into(),args,state:TaskState::Queued,created:now(),sent:None,result:None});
            assert!(state.next("guard-test","urn:dslforum-org:cwmp-1-0").unwrap().is_none());
            assert_eq!(state.tasks[&1].state,TaskState::Fault);
        }
        assert!(protect_management(
            &state,
            "guard-test",
            "DeleteObject",
            &json!({"object":"InternetGatewayDevice.WANDevice.1.WANConnectionDevice.3."})
        )
        .is_err());
        assert!(protect_management(
            &state,
            "guard-test",
            "AddObject",
            &json!({"object":"InternetGatewayDevice.WANDevice.1.WANConnectionDevice."})
        )
        .is_ok());
        assert!(protect_management(
            &state,
            "guard-test",
            "GetParameterValues",
            &json!({"names":["InternetGatewayDevice.ManagementServer.URL"]})
        )
        .is_ok());
    }
    #[test]
    fn task_identity_fault_and_no_write_replay() {
        let mut s = State::default();
        s.tasks.insert(
            4,
            Task {
                id: 4,
                device: "a".into(),
                method: "SetParameterValues".into(),
                args: json!({"parameters":[{"name":"x","value":"y"}]}),
                state: TaskState::Queued,
                created: now(),
                sent: None,
                result: None,
            },
        );
        assert!(s
            .next("a", "urn:dslforum-org:cwmp-1-0")
            .unwrap()
            .unwrap()
            .xml
            .contains("fttr-4"));
        assert!(s.next("a", "urn:dslforum-org:cwmp-1-0").unwrap().is_none());
        let msg = soap::parse(&soap::envelope(
            "urn:dslforum-org:cwmp-1-0",
            "fttr-4",
            "<cwmp:SetParameterValuesResponse><Status>1</Status></cwmp:SetParameterValuesResponse>",
        ))
        .unwrap();
        assert!(s
            .response(
                "b",
                soap::parse(&soap::envelope(
                    "urn:dslforum-org:cwmp-1-0",
                    "fttr-1",
                    "<cwmp:GetParameterValuesResponse/>"
                ))
                .unwrap()
            )
            .is_err());
        s.response("a", msg).unwrap();
        assert_eq!(s.tasks[&4].state, TaskState::AcceptedPendingApply);
        s.tasks.get_mut(&4).unwrap().state = TaskState::Sent;
        s.tasks.get_mut(&4).unwrap().sent = Some(now() - 100);
        s.expire();
        assert_eq!(s.tasks[&4].state, TaskState::Unknown);
        assert!(s.next("a", "urn:dslforum-org:cwmp-1-0").unwrap().is_none());
        assert!(validate("SetParameterValues", &json!({"parameters":[]})).is_err());
    }
    #[test]
    fn reset_discards_device_tasks_and_sessions_without_replaying_writes() {
        let mut state = State::default();
        for (id, device, status) in [
            (1, "a", TaskState::Queued),
            (2, "a", TaskState::Sent),
            (3, "b", TaskState::Queued),
        ] {
            state.tasks.insert(
                id,
                Task {
                    id,
                    device: device.into(),
                    method: "SetParameterValues".into(),
                    args: json!({}),
                    state: status,
                    created: now(),
                    sent: None,
                    result: None,
                },
            );
        }
        state.next_id = 3;
        state
            .sessions
            .insert("a-token".into(), ("a".into(), "peer".into(), now()));
        state
            .sessions
            .insert("b-token".into(), ("b".into(), "peer".into(), now()));
        state.forget_tasks("a");
        assert_eq!(state.tasks.keys().copied().collect::<Vec<_>>(), vec![3]);
        assert!(!state.sessions.contains_key("a-token"));
        assert!(state.sessions.contains_key("b-token"));
        assert_eq!(state.next_id, 3);
        assert!(state
            .next("a", "urn:dslforum-org:cwmp-1-0")
            .unwrap()
            .is_none());
        let reply = soap::parse(&soap::envelope(
            "urn:dslforum-org:cwmp-1-0",
            "fttr-2",
            "<cwmp:SetParameterValuesResponse><Status>0</Status></cwmp:SetParameterValuesResponse>",
        ))
        .unwrap();
        assert!(state.response("a", reply).is_err());
    }
    #[test]
    fn listener_bind_failure_preserves_the_existing_listener() {
        let server = listener_test_server();
        let old = transition(server, true, "127.0.0.1:0", "/old", "", "", false).unwrap();
        let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
        assert!(transition(
            old.clone(),
            true,
            &occupied.local_addr().unwrap().to_string(),
            "/new",
            "",
            "",
            true
        )
        .is_err());
        assert!(!old.stopping.load(Ordering::Acquire));
        assert!(TcpStream::connect(&old.listen).is_ok());
        stop_listener(&old);
    }
    pub(super) fn listener_test_server() -> Arc<Server> {
        Arc::new(Server {
            state: Arc::new(Mutex::new(State::default())),
            enabled: false,
            listen: String::new(),
            path: "/cwmp".into(),
            state_file: format!(
                "/tmp/fttr-listener-test-{}-{}.json",
                std::process::id(),
                token().unwrap()
            ),
            auth: None,
            failure: Mutex::new(None),
            requests: Arc::new(AtomicUsize::new(0)),
            endpoint_id: String::new(),
            drain_deadline: Mutex::new(None),
            legacy: Mutex::new(Vec::new()),
            stopping: AtomicBool::new(false),
            stopped: AtomicBool::new(true),
        })
    }
    #[test]
    fn listener_migration_shares_tasks_and_marks_inform_with_the_actual_endpoint() {
        use std::io::BufRead;
        let old = transition(
            listener_test_server(),
            true,
            "127.0.0.1:0",
            "/old",
            "old",
            "secret",
            false,
        )
        .unwrap();
        let new = transition(
            old.clone(),
            true,
            "127.0.0.1:0",
            "/new",
            "new",
            "secret",
            true,
        )
        .unwrap();
        assert!(Arc::ptr_eq(&old.state, &new.state));
        assert!(!old.stopping.load(Ordering::Acquire));
        let inform=soap::envelope("urn:dslforum-org:cwmp-1-0","cpe","<cwmp:Inform><DeviceId><OUI>ABCDEF</OUI><ProductClass>test</ProductClass><SerialNumber>migration</SerialNumber></DeviceId></cwmp:Inform>");
        let send = |address: &str, path: &str, user: &str| {
            let mut stream = TcpStream::connect(address).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            write!(stream,"POST {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: {}\r\nContent-Length: {}\r\n\r\n{inform}",http::basic(user,"secret"),inform.len()).unwrap();
            stream.flush().unwrap();
            let mut reader = BufReader::new(stream);
            let mut headers = String::new();
            loop {
                let mut line = String::new();
                assert_ne!(reader.read_line(&mut line).unwrap(), 0);
                headers.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
            assert!(headers.starts_with("HTTP/1.1 200"));
            let length: usize = headers
                .lines()
                .find_map(|l| l.strip_prefix("Content-Length: "))
                .unwrap()
                .parse()
                .unwrap();
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            assert!(String::from_utf8(body).unwrap().contains("InformResponse"));
        };
        send(&old.listen, "/old", "old");
        assert_eq!(
            new.state.lock().unwrap().devices["ABCDEF:test:migration"].endpoint_id,
            old.endpoint_id
        );
        send(&new.listen, "/new", "new");
        assert_eq!(
            new.state.lock().unwrap().devices["ABCDEF:test:migration"].endpoint_id,
            new.endpoint_id
        );
        let same_address = transition(
            new.clone(),
            true,
            &new.listen,
            "/third",
            "third",
            "secret",
            true,
        )
        .unwrap();
        send(&same_address.listen, "/new", "new");
        assert_eq!(
            same_address.state.lock().unwrap().devices["ABCDEF:test:migration"].endpoint_id,
            new.endpoint_id
        );
        send(&same_address.listen, "/third", "third");
        assert_eq!(
            same_address.state.lock().unwrap().devices["ABCDEF:test:migration"].endpoint_id,
            same_address.endpoint_id
        );
        for previous in same_address.legacy.lock().unwrap().iter() {
            stop_listener(previous);
        }
        stop_listener(&same_address);
        fs::remove_file(&same_address.state_file).unwrap();
    }
    #[test]
    fn actual_http_inform_cookie_rpc_and_termination() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let state_file = format!("/tmp/fttr-acs-wire-test-{}.json", std::process::id());
        let server = Arc::new(Server {
            state: Arc::new(Mutex::new(State::default())),
            enabled: true,
            listen: address.to_string(),
            path: "/cwmp".into(),
            state_file: state_file.clone(),
            auth: Some(http::basic("test", "secret")),
            failure: Mutex::new(None),
            requests: Arc::new(AtomicUsize::new(0)),
            endpoint_id: "wire-test".into(),
            drain_deadline: Mutex::new(None),
            legacy: Mutex::new(Vec::new()),
            stopping: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
        });
        let handler = server.clone();
        let thread = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            serve(&handler, stream).unwrap();
        });
        let mut client = TcpStream::connect(address).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut reader = BufReader::new(client.try_clone().unwrap());
        fn exchange(
            client: &mut TcpStream,
            reader: &mut BufReader<TcpStream>,
            body: &str,
        ) -> (String, String) {
            use std::io::BufRead;
            write!(client, "POST /cwmp HTTP/1.1\r\nHost: localhost\r\nAuthorization: Basic dGVzdDpzZWNyZXQ=\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
            client.flush().unwrap();
            let mut headers = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                headers.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
            let length: usize = headers
                .lines()
                .find_map(|l| l.strip_prefix("Content-Length: "))
                .unwrap_or_else(|| {
                    assert!(headers.starts_with("HTTP/1.1 204"));
                    "0"
                })
                .parse()
                .unwrap();
            let mut data = vec![0; length];
            reader.read_exact(&mut data).unwrap();
            (headers, String::from_utf8(data).unwrap())
        }
        let inform = soap::envelope("urn:dslforum-org:cwmp-1-0", "cpe1", "<cwmp:Inform><DeviceId><OUI>ABCDEF</OUI><ProductClass>test</ProductClass><SerialNumber>mock</SerialNumber></DeviceId></cwmp:Inform>");
        let (headers, body) = exchange(&mut client, &mut reader, &inform);
        assert!(headers.contains("Set-Cookie: fttr_session="));
        assert!(body.contains("InformResponse"));
        let task: Value=serde_json::from_str(&server.execute(json!({"op":"queue","device":"ABCDEF:test:mock","method":"GetParameterValues","args":{"names":["InternetGatewayDevice.DeviceInfo.ModelName"]}}))).unwrap();
        assert_eq!(task["id"], 1);
        let (_, rpc) = exchange(&mut client, &mut reader, "");
        assert!(rpc.contains("GetParameterValues"));
        let response=soap::envelope("urn:dslforum-org:cwmp-1-0", "fttr-1", "<cwmp:GetParameterValuesResponse><ParameterList><ParameterValueStruct><Name>InternetGatewayDevice.DeviceInfo.ModelName</Name><Value xsi:type=\"xsd:string\">mock</Value></ParameterValueStruct></ParameterList></cwmp:GetParameterValuesResponse>");
        let (headers, body) = exchange(&mut client, &mut reader, &response);
        assert!(headers.starts_with("HTTP/1.1 204"));
        assert!(body.is_empty());
        thread.join().unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&server.execute(json!({"op":"task","id":1}))).unwrap()
                ["result"]["parameters"][0]["value"],
            "mock"
        );
        fs::remove_file(state_file).unwrap();
    }
}
