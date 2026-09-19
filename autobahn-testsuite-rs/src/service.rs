//! Fuzzing client/server, echo/testee, broadcast, and connection-load modes.
use crate::{
    Error, Result,
    catalog::{self, Case},
    codec::{Frame, Reader, Writer, close_code},
    compression::{Deflate, Messages},
    config::{Spec, Target},
    handshake::{self, Connection, Socket},
    report::{self, CaseResult},
    runner,
};
use bytes::Bytes;
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    net::TcpListener,
    sync::{Mutex, Notify, broadcast, watch},
    task::JoinSet,
    time::timeout,
};

/// Supported command-line modes.
#[derive(Clone, Copy, Debug, clap::ValueEnum, PartialEq, Eq)]
pub enum Mode {
    /// Test WebSocket client implementations.
    Fuzzingserver,
    /// Test WebSocket server implementations.
    Fuzzingclient,
    /// Echo received messages.
    Echoserver,
    /// Echo server-originated messages; --message-count enables a roundtrip workload.
    Echoclient,
    /// Echo server for an external conformance runner.
    Testeeserver,
    /// Drive an external Autobahn fuzzing server.
    Testeeclient,
    /// Broadcast incoming messages to all connected clients.
    Broadcastserver,
    /// Send a message and receive broadcasts.
    Broadcastclient,
    /// Open a bounded number of concurrent WebSocket connections.
    Massconnect,
    /// Generate WAMP JSON/MessagePack interoperability vectors.
    Serializer,
}
/// Run a selected service until completion or Ctrl-C. Returns the failure count.
pub async fn run(mode: Mode, spec: Spec, agent: String) -> Result<usize> {
    spec.validate()?;
    match mode {
        Mode::Fuzzingclient => fuzzing_client(spec).await,
        Mode::Fuzzingserver | Mode::Echoserver | Mode::Testeeserver | Mode::Broadcastserver => {
            server(mode, spec).await
        }
        Mode::Testeeclient => testee_client(spec, &agent).await,
        Mode::Echoclient => echo_client(spec).await,
        Mode::Broadcastclient => broadcast_client(spec, &agent).await,
        Mode::Massconnect => mass_connect(spec).await,
        Mode::Serializer => Err(Error::Config("serializer requires --outfile".into())),
    }
}
fn target(url: String, agent: Option<String>) -> Target {
    Target {
        url,
        agent,
        hostname: None,
        headers: BTreeMap::new(),
    }
}
async fn save(spec: &Spec, results: Vec<CaseResult>, cases: Arc<Vec<Case>>) -> Result<()> {
    let directory = spec.outdir.clone();
    tokio::task::spawn_blocking(move || report::write(directory, &results, &cases))
        .await
        .map_err(|e| Error::Config(format!("report task: {e}")))?
}
async fn fuzzing_client(spec: Spec) -> Result<usize> {
    let cases = Arc::new(catalog::load()?);
    let spec = Arc::new(spec);
    let tls = handshake::client_tls(&spec)?;
    let targets = if spec.servers.is_empty() {
        vec![target(spec.url.clone(), None)]
    } else {
        spec.servers.clone()
    };
    let selected = spec.selected(&cases);
    let mut jobs = Vec::new();
    for t in targets {
        for case in &selected {
            let agent = t.agent.as_deref().unwrap_or(&t.url);
            if !spec.excluded(agent, &case.id) {
                jobs.push((t.clone(), (*case).clone()));
            }
        }
    }
    if jobs.is_empty() {
        return Err(Error::Config("no selected cases".into()));
    }
    eprintln!(
        "Running {} cases with concurrency {}",
        jobs.len(),
        spec.concurrency
    );
    let mut jobs = jobs.into_iter();
    let mut tasks = JoinSet::new();
    let mut results = Vec::new();
    let mut interrupted = false;
    loop {
        while tasks.len() < spec.concurrency {
            let Some((target, case)) = jobs.next() else {
                break;
            };
            let spec = spec.clone();
            let tls = tls.clone();
            tasks.spawn(async move {
                let agent = target.agent.as_deref().unwrap_or(&target.url);
                match handshake::connect(
                    &target,
                    &spec,
                    tls,
                    case.compression().then_some(case.parameter),
                )
                .await
                {
                    Ok(connection) => runner::run(connection, &case, agent, &spec).await,
                    Err(e) => CaseResult::failure(agent, &case, &spec, e),
                }
            });
        }
        if tasks.is_empty() {
            break;
        }
        tokio::select! {
            done=tasks.join_next()=>if let Some(done)=done {
                let result=done.map_err(|e|Error::Config(format!("case task: {e}")))?;
                eprintln!("{} {}: {} / {} ({:.1} ms)",result.agent,result.case,result.behavior,result.behavior_close,result.duration);
                results.push(result);
            },
            _=tokio::signal::ctrl_c()=>{interrupted=true;tasks.abort_all();break;}
        }
    }
    let failures = results.iter().filter(|r| r.failed()).count() + usize::from(interrupted);
    save(&spec, results, cases).await?;
    Ok(failures)
}

struct State {
    spec: Arc<Spec>,
    cases: Arc<Vec<Case>>,
    selected: Vec<usize>,
    results: Mutex<BTreeMap<(String, String), CaseResult>>,
    changed: Notify,
    reporting: Mutex<()>,
    active_cases: AtomicUsize,
    stop: watch::Sender<bool>,
    broadcasts: broadcast::Sender<(u8, Bytes)>,
}
impl State {
    async fn reports(&self) -> Result<()> {
        let _guard = self.reporting.lock().await;
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.active_cases.load(Ordering::Acquire) == 0 {
                break;
            }
            changed.await;
        }
        let snapshot = self.results.lock().await.values().cloned().collect();
        save(&self.spec, snapshot, self.cases.clone()).await
    }
}
async fn server(mode: Mode, spec: Spec) -> Result<usize> {
    let url = url::Url::parse(&spec.url).map_err(|e| Error::Config(e.to_string()))?;
    let host = url
        .host_str()
        .ok_or_else(|| Error::Config("missing bind host".into()))?
        .trim_matches(['[', ']']);
    let port = url
        .port_or_known_default()
        .ok_or_else(|| Error::Config("missing port".into()))?;
    let listener = TcpListener::bind((host, port)).await?;
    let web = if spec.webport > 0 {
        Some(TcpListener::bind((host, spec.webport)).await?)
    } else {
        None
    };
    let tls = handshake::server_tls(&spec)?;
    let cases = Arc::new(catalog::load()?);
    let selected = cases
        .iter()
        .enumerate()
        .filter(|(_, c)| spec.selected(std::slice::from_ref(c)).len() == 1)
        .map(|(i, _)| i)
        .collect::<Vec<_>>();
    let (stop, mut stopped) = watch::channel(false);
    let (broadcasts, _) = broadcast::channel(256);
    let state = Arc::new(State {
        spec: Arc::new(spec),
        cases,
        selected,
        results: Mutex::new(BTreeMap::new()),
        changed: Notify::new(),
        reporting: Mutex::new(()),
        active_cases: AtomicUsize::new(0),
        stop,
        broadcasts,
    });
    eprintln!(
        "{mode:?} listening on {} ({} selected cases)",
        listener.local_addr()?,
        state.selected.len()
    );
    let mut tasks = JoinSet::new();
    if let Some(web) = web {
        let state = state.clone();
        tasks.spawn(async move {
            if let Err(e) = web_server(web, state).await {
                eprintln!("HTTP server: {e}");
            }
        });
    }
    let mut ticks = tokio::time::interval(Duration::from_secs(1));
    let mut tick_count = 0u64;
    loop {
        tokio::select! {
            _=ticks.tick(), if mode == Mode::Broadcastserver => { tick_count += 1; let _ = state.broadcasts.send((1, Bytes::from(format!("tick {tick_count}")))); },
            _=tokio::signal::ctrl_c()=>break,
            _=stopped.changed()=>break,
            result=tasks.join_next(),if !tasks.is_empty()=>if let Some(Err(e))=result {eprintln!("connection task: {e}");},
            accepted=listener.accept(),if tasks.len()<state.spec.max_connections=>{
                let (tcp,_)=accepted?;tcp.set_nodelay(true)?;let state=state.clone();let tls=tls.clone();
                tasks.spawn(async move {
                    let outcome=async {
                        let socket:Socket=if let Some(tls)=tls {Box::new(timeout(Duration::from_millis(state.spec.handshake_timeout_ms),tls.accept(tcp)).await.map_err(|_|Error::Timeout)??)}else{Box::new(tcp)};
                        handle(socket,mode,&state).await
                    }.await;
                    if let Err(e)=outcome {eprintln!("connection: {e}");}
                });
            }
        }
    }
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    if mode == Mode::Fuzzingserver {
        state.reports().await?;
    }
    Ok(0)
}
fn one_param<'a>(params: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    params.get(key).map(String::as_str)
}
async fn handle(socket: Socket, mode: Mode, state: &State) -> Result<()> {
    let request = handshake::request(socket, state.spec.handshake_timeout_ms).await?;
    if mode != Mode::Fuzzingserver {
        let connection = request.upgrade(Some(1), &state.spec.protocols).await?;
        return echo(
            connection,
            &state.spec,
            if mode == Mode::Broadcastserver {
                Some(&state.broadcasts)
            } else {
                None
            },
        )
        .await;
    }
    let url = url::Url::parse(&format!("http://localhost{}", request.path))
        .map_err(|e| Error::Handshake(e.to_string()))?;
    let mut params = BTreeMap::new();
    for (key, value) in url.query_pairs() {
        if params
            .insert(key.into_owned(), value.into_owned())
            .is_some()
        {
            return request.reject("duplicate query parameter").await;
        }
    }
    let agent = one_param(&params, "agent")
        .or_else(|| request.headers.get("user-agent").map(String::as_str))
        .unwrap_or("UnknownClient")
        .to_owned();
    if agent.len() > 256 {
        return request.reject("agent too long").await;
    }
    let case = if let Some(id) =
        one_param(&params, "casetuple").or_else(|| one_param(&params, "caseId"))
    {
        state
            .selected
            .iter()
            .filter_map(|i| state.cases.get(*i))
            .find(|c| c.id == id)
    } else {
        one_param(&params, "case")
            .and_then(|s| s.parse::<usize>().ok())
            .and_then(|n| n.checked_sub(1))
            .and_then(|n| state.selected.get(n))
            .and_then(|i| state.cases.get(*i))
    };
    match url.path() {
        "/runCase" => {
            let Some(case) = case else {
                return request.reject("unknown case").await;
            };
            let full = {
                let results = state.results.lock().await;
                results
                    .len()
                    .saturating_add(state.active_cases.load(Ordering::Acquire))
                    >= state.spec.max_results
                    && !results.contains_key(&(agent.clone(), case.id.clone()))
            };
            if full {
                return request
                    .reject("result capacity reached; save reports and restart")
                    .await;
            }
            let connection = request
                .upgrade(
                    case.compression().then_some(case.parameter),
                    &state.spec.protocols,
                )
                .await?;
            if state.spec.excluded(&agent, &case.id) {
                return control(connection, None, &state.spec).await.map(|_| ());
            }
            state.active_cases.fetch_add(1, Ordering::AcqRel);
            let lease = CaseLease(state);
            let result = runner::run(connection, case, &agent, &state.spec).await;
            eprintln!(
                "{} {}: {} / {}",
                agent, case.id, result.behavior, result.behavior_close
            );
            state
                .results
                .lock()
                .await
                .insert((agent, case.id.clone()), result);
            drop(lease);
            Ok(())
        }
        "/getCaseCount" => {
            let conn = request.upgrade(None, &state.spec.protocols).await?;
            control(conn, Some(state.selected.len().to_string()), &state.spec)
                .await
                .map(|_| ())
        }
        "/getCaseInfo" => {
            let Some(case) = case else {
                return request.reject("unknown case").await;
            };
            let data = serde_json::json!({"id":case.id,"description":case.description}).to_string();
            let conn = request.upgrade(None, &state.spec.protocols).await?;
            control(conn, Some(data), &state.spec).await.map(|_| ())
        }
        "/getCaseStatus" => {
            let Some(case) = case else {
                return request.reject("unknown case").await;
            };
            let conn = request.upgrade(None, &state.spec.protocols).await?;
            let key = (agent, case.id.clone());
            let data = timeout(Duration::from_secs(30), async {
                loop {
                    let notified = state.changed.notified();
                    tokio::pin!(notified);
                    notified.as_mut().enable();
                    if let Some(result) = state.results.lock().await.get(&key) {
                        break serde_json::json!({"behavior":result.behavior}).to_string();
                    }
                    notified.await;
                }
            })
            .await
            .map_err(|_| Error::Timeout)?;
            control(conn, Some(data), &state.spec).await.map(|_| ())
        }
        "/updateReports" => {
            let conn = request.upgrade(None, &state.spec.protocols).await?;
            state.reports().await?;
            control(conn, None, &state.spec).await?;
            if one_param(&params, "shutdownOnComplete").is_some_and(|v| {
                v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes") || v == "1"
            }) {
                let _ = state.stop.send(true);
            }
            Ok(())
        }
        "/stopServer" => {
            let conn = request.upgrade(None, &state.spec.protocols).await?;
            control(conn, None, &state.spec).await?;
            let _ = state.stop.send(true);
            Ok(())
        }
        _ => request.reject("unknown endpoint").await,
    }
}
async fn control(
    connection: Connection,
    message: Option<String>,
    spec: &Spec,
) -> Result<Option<Bytes>> {
    let Connection {
        io,
        initial,
        server,
        ..
    } = connection;
    let (read, write) = tokio::io::split(io);
    let mut reader = Reader::new(read, initial, server, spec.max_frame_size);
    let mut writer = Writer::new(write, !server);
    if let Some(message) = message {
        writer
            .frame(&Frame::new(1, Bytes::from(message)), 0)
            .await?;
    }
    if server {
        writer
            .frame(&Frame::new(8, Bytes::from_static(&[3, 232])), 0)
            .await?;
    }
    writer.flush().await?;
    let mut response = None;
    timeout(
        Duration::from_millis(spec.close_timeout_ms.max(5000)),
        async {
            while let Some(frame) = reader.next().await? {
                match frame.opcode {
                    1 | 2 => response = Some(frame.payload),
                    9 => {
                        writer.frame(&Frame::new(10, frame.payload), 0).await?;
                        writer.flush().await?;
                    }
                    8 => {
                        if !server {
                            writer.frame(&Frame::new(8, frame.payload), 0).await?;
                        }
                        writer.flush().await?;
                        writer.shutdown().await?;
                        break;
                    }
                    _ => {}
                }
            }
            Ok::<_, Error>(())
        },
    )
    .await
    .map_err(|_| Error::Timeout)??;
    Ok(response)
}
pub(crate) async fn echo(
    connection: Connection,
    spec: &Spec,
    broadcaster: Option<&broadcast::Sender<(u8, Bytes)>>,
) -> Result<()> {
    let Connection {
        io,
        initial,
        compression,
        server,
    } = connection;
    let (read, write) = tokio::io::split(io);
    let mut reader = Reader::new(read, initial, server, spec.max_frame_size);
    let mut writer = Writer::new(write, !server);
    let mut messages = Messages::new(spec.max_message_size, compression)?;
    let mut deflate = compression.map(Deflate::new).transpose()?;
    let mut subscription = broadcaster.map(|b| b.subscribe());
    loop {
        let incoming = if let Some(subscription) = &mut subscription {
            tokio::select! {
                frame=reader.next()=>frame,
                message=subscription.recv()=>{
                    let (opcode,payload)=message.map_err(|e|Error::Protocol(format!("broadcast lag: {e}")))?;
                    send_message(&mut writer,opcode,payload,&mut deflate).await?;continue;
                }
            }
        } else {
            reader.next().await
        };
        let frame = match incoming {
            Ok(Some(frame)) => frame,
            Ok(None) => return Ok(()),
            Err(error) => {
                send_error(&mut writer, &error).await?;
                return Ok(());
            }
        };
        match frame.opcode {
            8 => {
                if let Err(error) = close_code(&frame.payload) {
                    send_error(&mut writer, &error).await?;
                    return Ok(());
                }
                writer.frame(&Frame::new(8, frame.payload), 0).await?;
                writer.flush().await?;
                if server {
                    writer.shutdown().await?;
                } else {
                    let _ =
                        timeout(Duration::from_millis(spec.close_timeout_ms), reader.next()).await;
                }
                return Ok(());
            }
            9 => {
                writer.frame(&Frame::new(10, frame.payload), 0).await?;
                writer.flush().await?;
            }
            10 => {}
            _ => match messages.push(frame) {
                Ok(Some((opcode, payload))) => {
                    if let Some(broadcaster) = broadcaster {
                        let _ = broadcaster.send((opcode, payload));
                    } else {
                        send_message(&mut writer, opcode, payload, &mut deflate).await?;
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    send_error(&mut writer, &error).await?;
                    return Ok(());
                }
            },
        }
    }
}
async fn send_message<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut Writer<W>,
    opcode: u8,
    payload: Bytes,
    deflate: &mut Option<Deflate>,
) -> Result<()> {
    if let Some(deflate) = deflate {
        writer
            .message(opcode, deflate.encode(&payload)?, 0, 4)
            .await?;
    } else {
        writer.message(opcode, payload, 0, 0).await?;
    }
    writer.flush().await
}
async fn send_error<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut Writer<W>,
    error: &Error,
) -> Result<()> {
    let code: u16 = match error {
        Error::Limit(_) => 1009,
        Error::Protocol(text) if text.contains("UTF-8") => 1007,
        _ => 1002,
    };
    writer
        .frame(
            &Frame::new(8, Bytes::copy_from_slice(&code.to_be_bytes())),
            0,
        )
        .await?;
    writer.flush().await?;
    writer.shutdown().await
}
fn endpoint(base: &str, path: &str, agent: &str, case: Option<usize>) -> Result<String> {
    let mut url = url::Url::parse(base).map_err(|e| Error::Config(e.to_string()))?;
    url.set_path(path);
    url.set_query(None);
    url.query_pairs_mut().append_pair("agent", agent);
    if let Some(case) = case {
        url.query_pairs_mut().append_pair("case", &case.to_string());
    }
    Ok(url.into())
}
async fn testee_client(spec: Spec, agent: &str) -> Result<usize> {
    let tls = handshake::client_tls(&spec)?;
    let target_count = target(endpoint(&spec.url, "/getCaseCount", agent, None)?, None);
    let conn = handshake::connect(&target_count, &spec, tls.clone(), None).await?;
    let count = control(conn, None, &spec)
        .await?
        .ok_or_else(|| Error::Protocol("missing case count".into()))?;
    let count: usize =
        serde_json::from_slice(&count).map_err(|e| Error::Protocol(e.to_string()))?;
    for i in 1..=count {
        let target = target(endpoint(&spec.url, "/runCase", agent, Some(i))?, None);
        eprintln!("Running case {i}/{count}");
        let conn = handshake::connect(&target, &spec, tls.clone(), Some(1)).await?;
        if let Err(e) = timeout(
            Duration::from_millis(spec.case_timeout_ms.unwrap_or(1_100_000)),
            echo(conn, &spec, None),
        )
        .await
        .map_err(|_| Error::Timeout)
        .and_then(|r| r)
        {
            eprintln!("case {i}: {e}");
        }
    }
    let target = target(endpoint(&spec.url, "/updateReports", agent, None)?, None);
    let conn = handshake::connect(&target, &spec, tls, None).await?;
    control(conn, None, &spec).await?;
    Ok(0)
}
async fn echo_client(spec: Spec) -> Result<usize> {
    let cases = catalog::load()?;
    let case = cases
        .iter()
        .find(|c| c.id == "9.7.3")
        .ok_or_else(|| Error::Config("missing echo workload".into()))?;
    let tls = handshake::client_tls(&spec)?;
    let conn = handshake::connect(&target(spec.url.clone(), None), &spec, tls, None).await?;
    if spec.message_count.is_none() {
        tokio::select! { result=echo(conn,&spec,None)=>result?, _=tokio::signal::ctrl_c()=>{} }
        return Ok(0);
    }
    let result = runner::run(conn, case, "echo", &spec).await;
    println!(
        "{}",
        serde_json::to_string_pretty(&result).map_err(|e| Error::Config(e.to_string()))?
    );
    Ok(usize::from(result.failed()))
}
async fn mass_connect(spec: Spec) -> Result<usize> {
    let targets = if spec.servers.is_empty() {
        vec![target(spec.url.clone(), None)]
    } else {
        spec.servers.clone()
    };
    let tls = handshake::client_tls(&spec)?;
    let spec = Arc::new(spec);
    let mut failures = 0;
    for target in targets {
        let mut connecting = JoinSet::new();
        let mut live = JoinSet::new();
        let (stop, _) = watch::channel(false);
        let mut next = 0;
        let mut connected = 0;
        let started = tokio::time::Instant::now();
        while next < spec.connections || !connecting.is_empty() {
            let mut launched = false;
            while next < spec.connections && connecting.len() < spec.concurrency {
                let tls = tls.clone();
                let spec = spec.clone();
                let target = target.clone();
                next += 1;
                launched = true;
                connecting.spawn(async move {
                    let mut retries = 0usize;
                    loop {
                        match handshake::connect(&target, &spec, tls.clone(), None).await {
                            Ok(connection) => break Ok(connection),
                            Err(error) => {
                                if spec.connect_retries.is_some_and(|max| retries >= max) {
                                    break Err(error);
                                }
                                retries = retries.saturating_add(1);
                                tokio::time::sleep(Duration::from_millis(spec.retry_delay_ms))
                                    .await;
                            }
                        }
                    }
                });
            }
            if launched && spec.batch_delay_ms > 0 {
                tokio::select! { _=tokio::time::sleep(Duration::from_millis(spec.batch_delay_ms))=>{}, _=tokio::signal::ctrl_c()=>return Ok(failures+1) }
            }
            tokio::select! {
                _=tokio::signal::ctrl_c()=>return Ok(failures+1),
                done=live.join_next(), if !live.is_empty()=> { if done.is_some() { failures += 1; eprintln!("connection lost during mass-connect ramp"); } },
                done=connecting.join_next()=>if let Some(done)=done {
                    match done.map_err(|e|Error::Config(e.to_string()))? {
                        Ok(connection)=>{ connected+=1; let stopped=stop.subscribe(); let spec=spec.clone(); live.spawn(async move { idle_connection(connection,&spec,stopped).await }); },
                        Err(error)=>{failures+=1;eprintln!("connect: {error}");}
                    }
                }
            }
        }
        println!(
            "{}: {connected} connected in {:.3}s; {} live",
            target.agent.as_deref().unwrap_or(&target.url),
            started.elapsed().as_secs_f64(),
            live.len()
        );
        tokio::select! { _=tokio::time::sleep(Duration::from_millis(spec.hold_ms))=>{}, _=tokio::signal::ctrl_c()=>{} }
        let _ = stop.send(true);
        while let Some(done) = live.join_next().await {
            if !matches!(done, Ok(Ok(true))) {
                failures += 1;
            }
        }
    }
    Ok(failures)
}
async fn idle_connection(
    connection: Connection,
    spec: &Spec,
    mut stop: watch::Receiver<bool>,
) -> Result<bool> {
    let Connection { io, initial, .. } = connection;
    let (read, write) = tokio::io::split(io);
    let mut reader = Reader::new(read, initial, false, spec.max_frame_size);
    let mut writer = Writer::new(write, true);
    loop {
        tokio::select! {
            _=stop.changed()=>break,
            frame=reader.next()=>match frame? {
                Some(frame) if frame.opcode==9=>{writer.frame(&Frame::new(10,frame.payload),0).await?;writer.flush().await?;},
                Some(frame) if frame.opcode==8=>{writer.frame(&Frame::new(8,frame.payload),0).await?;writer.flush().await?;return Ok(false);},
                None=>return Ok(false),
                _=>{},
            }
        }
    }
    timeout(Duration::from_millis(spec.close_timeout_ms), async {
        writer
            .frame(&Frame::new(8, Bytes::from_static(&[3, 232])), 0)
            .await?;
        writer.flush().await?;
        while let Some(frame) = reader.next().await? {
            if frame.opcode == 8 {
                break;
            }
        }
        Ok::<_, Error>(())
    })
    .await
    .map_err(|_| Error::Timeout)??;
    Ok(true)
}

async fn broadcast_client(spec: Spec, agent: &str) -> Result<usize> {
    let tls = handshake::client_tls(&spec)?;
    let conn = handshake::connect(&target(spec.url.clone(), None), &spec, tls, Some(1)).await?;
    let Connection {
        io,
        initial,
        compression,
        ..
    } = conn;
    let (read, write) = tokio::io::split(io);
    let mut reader = Reader::new(read, initial, false, spec.max_frame_size);
    let mut writer = Writer::new(write, true);
    let mut messages = Messages::new(spec.max_message_size, compression)?;
    let mut deflate = compression.map(Deflate::new).transpose()?;
    let mut ticks = tokio::time::interval(Duration::from_secs(2));
    loop {
        tokio::select! {
            _=tokio::signal::ctrl_c()=>{writer.frame(&Frame::new(8,Bytes::from_static(&[3,232])),0).await?;writer.flush().await?;return Ok(0);},
            _=ticks.tick()=>send_message(&mut writer,1,Bytes::from(format!("hello from {agent}")),&mut deflate).await?,
            frame=reader.next()=>{
                let Some(frame)=frame? else{return Ok(0);};
                match frame.opcode {
                    8=>{writer.frame(&Frame::new(8,frame.payload),0).await?;writer.flush().await?;return Ok(0);},
                    9=>{writer.frame(&Frame::new(10,frame.payload),0).await?;writer.flush().await?;},
                    10=>{},
                    _=>if let Some((opcode,data))=messages.push(frame)? {
                        if opcode==1{println!("{}",String::from_utf8_lossy(&data));}else{println!("binary: {}",data.iter().map(|b|format!("{b:02x}")).collect::<String>());}
                    }
                }
            }
        }
    }
}

struct CaseLease<'a>(&'a State);
impl Drop for CaseLease<'_> {
    fn drop(&mut self) {
        self.0.active_cases.fetch_sub(1, Ordering::AcqRel);
        self.0.changed.notify_waiters();
    }
}

async fn web_server(listener: TcpListener, state: Arc<State>) -> Result<()> {
    let mut tasks = JoinSet::new();
    let mut stop = state.stop.subscribe();
    eprintln!("Browser test page: http://{}", listener.local_addr()?);
    loop {
        tokio::select! {
            _=stop.changed()=>break,
            _=tasks.join_next(),if !tasks.is_empty()=>{},
            accepted=listener.accept(),if tasks.len()<32=>{
                let (socket,_)=accepted?;let state=state.clone();
                tasks.spawn(async move{let _=timeout(Duration::from_secs(10),http_request(socket,&state)).await;});
            }
        }
    }
    Ok(())
}
async fn http_request(mut socket: tokio::net::TcpStream, state: &State) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut buf = Vec::new();
    let mut scratch = [0; 1024];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = socket.read(&mut scratch).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&scratch[..n]);
        if buf.len() > 8192 {
            return Err(Error::Limit("HTTP request"));
        }
    }
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut request = httparse::Request::new(&mut headers);
    request
        .parse(&buf)
        .map_err(|e| Error::Handshake(e.to_string()))?;
    let path = request.path.unwrap_or("");
    let (status, content_type, data) = if request.method != Some("GET") {
        (
            "405 Method Not Allowed",
            "text/plain",
            b"GET required".to_vec(),
        )
    } else if path == "/" {
        let url = state
            .spec
            .url
            .replace('&', "&amp;")
            .replace('"', "&quot;")
            .replace('<', "&lt;");
        (
            "200 OK",
            "text/html; charset=utf-8",
            include_str!("web.html")
                .replace("__WS_URL__", &url)
                .into_bytes(),
        )
    } else if let Some(file) = path.strip_prefix("/reports/") {
        let safe = !file.starts_with('.')
            && file
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
            && (file.ends_with(".json") || file.ends_with(".html"));
        if safe {
            match tokio::fs::read(std::path::Path::new(&state.spec.outdir).join(file)).await {
                Ok(bytes) => (
                    "200 OK",
                    if file.ends_with(".json") {
                        "application/json"
                    } else {
                        "text/html; charset=utf-8"
                    },
                    bytes,
                ),
                Err(_) => (
                    "404 Not Found",
                    "text/plain",
                    b"Run tests and update reports first".to_vec(),
                ),
            }
        } else {
            ("404 Not Found", "text/plain", b"Not found".to_vec())
        }
    } else {
        ("404 Not Found", "text/plain", b"Not found".to_vec())
    };
    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
        data.len()
    );
    socket.write_all(header.as_bytes()).await?;
    socket.write_all(&data).await?;
    socket.shutdown().await?;
    Ok(())
}
