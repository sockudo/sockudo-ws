//! Timed case execution with bounded asynchronous reception.
use crate::{
    Error, Result,
    catalog::{Action, Case, CloseExpectation, ExpectedEvent, corpus},
    codec::{Frame, Reader, Writer, close_code},
    compression::{Deflate, Messages},
    config::Spec,
    handshake::Connection,
    report::CaseResult,
};
use bytes::Bytes;
use std::time::Duration;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::mpsc,
    time::{Instant, sleep_until, timeout_at},
};

#[derive(Debug, Clone, PartialEq, Eq)]
struct Event {
    kind: &'static str,
    payload: Bytes,
    binary: bool,
}
impl Event {
    fn from_expected(e: &ExpectedEvent) -> Result<Self> {
        Ok(Self {
            kind: match e.kind.as_str() {
                "message" => "message",
                "pong" => "pong",
                "ping" => "ping",
                "timeout" => "timeout",
                _ => return Err(Error::Config("unknown event".into())),
            },
            payload: e.payload.bytes()?,
            binary: e.binary,
        })
    }
    fn summary(&self) -> String {
        use sha1::{Digest, Sha1};
        let preview = self
            .payload
            .iter()
            .take(32)
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        format!(
            "{} binary={} len={} sha1={:x} prefix={preview}",
            self.kind,
            self.binary,
            self.payload.len(),
            Sha1::digest(&self.payload)
        )
    }
}
enum Incoming {
    Event(Event),
    Close(Bytes, Option<Error>, u64, u64),
    End(u64, u64),
    Error(Error, u64, u64),
}
async fn receive<R: AsyncRead + Unpin>(
    mut reader: Reader<R>,
    mut messages: Messages,
    tx: mpsc::Sender<Incoming>,
) {
    loop {
        let result = match reader.next().await {
            Ok(Some(frame)) => match frame.opcode {
                // Preserve the observed wire code even when it is invalid. The
                // conformance report must distinguish a wrong code from no code.
                8 => {
                    let error = close_code(&frame.payload).err();
                    Ok(Incoming::Close(
                        frame.payload,
                        error,
                        reader.bytes,
                        reader.frames,
                    ))
                }
                9 | 10 => Ok(Incoming::Event(Event {
                    kind: if frame.opcode == 9 { "ping" } else { "pong" },
                    payload: frame.payload,
                    binary: false,
                })),
                _ => match messages.push(frame) {
                    Ok(Some((opcode, payload))) => Ok(Incoming::Event(Event {
                        kind: "message",
                        payload,
                        binary: opcode == 2,
                    })),
                    Ok(None) => continue,
                    Err(e) => Err(e),
                },
            },
            Ok(None) => {
                let _ = tx.send(Incoming::End(reader.bytes, reader.frames)).await;
                break;
            }
            Err(e) => Err(e),
        };
        match result {
            Ok(event) => {
                if tx.send(event).await.is_err() {
                    break;
                }
            }
            Err(e) => {
                let _ = tx
                    .send(Incoming::Error(e, reader.bytes, reader.frames))
                    .await;
                break;
            }
        }
    }
}
struct AbortReader(tokio::task::JoinHandle<()>);
impl Drop for AbortReader {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct PayloadSource {
    fixed: Option<Bytes>,
    corpus: &'static [u8],
    boundaries: Vec<usize>,
    cursor: usize,
    length: usize,
}
impl PayloadSource {
    fn new(case: &Case) -> Result<Self> {
        if !case.compression() {
            return Ok(Self {
                fixed: Some(case.payload.bytes()?),
                corpus: &[],
                boundaries: vec![],
                cursor: 0,
                length: 0,
            });
        }
        let data = corpus(&case.file)?;
        let boundaries = if case.binary {
            vec![]
        } else {
            let text = std::str::from_utf8(data)
                .map_err(|_| Error::Config("invalid UTF8 corpus".into()))?;
            text.char_indices()
                .map(|(i, _)| i)
                .chain(std::iter::once(data.len()))
                .collect()
        };
        Ok(Self {
            fixed: None,
            corpus: data,
            boundaries,
            cursor: 0,
            length: case.length,
        })
    }
    fn next(&mut self) -> Bytes {
        if let Some(data) = &self.fixed {
            return data.clone();
        }
        let units = if self.boundaries.is_empty() {
            self.corpus.len()
        } else {
            self.boundaries.len() - 1
        };
        let end = (self.cursor + self.length) % units;
        let byte_index = |idx| {
            if self.boundaries.is_empty() {
                idx
            } else {
                self.boundaries[idx]
            }
        };
        let start_byte = byte_index(self.cursor);
        let end_byte = byte_index(end);
        let output = if end > self.cursor {
            Bytes::copy_from_slice(&self.corpus[start_byte..end_byte])
        } else {
            let mut data = Vec::with_capacity(self.corpus.len() - start_byte + end_byte);
            data.extend_from_slice(&self.corpus[start_byte..]);
            data.extend_from_slice(&self.corpus[..end_byte]);
            Bytes::from(data)
        };
        self.cursor = end;
        output
    }
}
async fn send_workload<W: AsyncWrite + Unpin>(
    writer: &mut Writer<W>,
    case: &Case,
    payload: Bytes,
    deflate: &mut Option<Deflate>,
) -> Result<()> {
    let opcode = if case.binary { 2 } else { 1 };
    if let Some(deflate) = deflate {
        let compressed = deflate.encode(&payload)?;
        writer.message(opcode, compressed, case.fragment, 4).await?;
    } else if case.chop > 0 {
        writer
            .frame(&Frame::new(opcode, payload), case.chop)
            .await?;
    } else {
        writer.message(opcode, payload, case.fragment, 0).await?;
    }
    writer.flush().await
}
async fn perform<W: AsyncWrite + Unpin>(writer: &mut Writer<W>, action: &Action) -> Result<()> {
    match action.kind.as_str() {
        "frame" => {
            writer
                .frame(
                    &Frame {
                        fin: action.fin,
                        rsv: action.rsv,
                        opcode: action.opcode,
                        payload: action.payload.bytes()?,
                    },
                    action.chop,
                )
                .await?
        }
        "message" => {
            writer
                .message(action.opcode, action.payload.bytes()?, action.fragment, 0)
                .await?
        }
        "close" => {
            writer
                .frame(&Frame::new(8, action.payload.bytes()?), 0)
                .await?
        }
        "header" => {
            writer
                .header(action.fin, 0, action.opcode, action.length)
                .await?
        }
        "data" => writer.data(&action.payload.bytes()?, 0).await?,
        _ => return Err(Error::Config(format!("unknown action {}", action.kind))),
    }
    if action.sync {
        writer.flush().await?;
        tokio::task::yield_now().await;
    }
    Ok(())
}

pub(crate) async fn run(
    connection: Connection,
    case: &Case,
    agent: &str,
    spec: &Spec,
) -> CaseResult {
    let mut report = CaseResult::new(agent, case, spec);
    let start = Instant::now();
    let duration = Duration::from_millis(
        spec.case_timeout_ms
            .unwrap_or(case.timeout_ms)
            .min(case.timeout_ms),
    );
    let result = tokio::time::timeout(duration, execute(connection, case, spec, &mut report))
        .await
        .map_err(|_| Error::Timeout)
        .and_then(|result| result);
    if let Err(e) = result {
        report.behavior = "FAILED".into();
        report.result = e.to_string();
    }
    report.duration = start.elapsed().as_secs_f64() * 1000.0;
    report
}
async fn execute(
    connection: Connection,
    case: &Case,
    spec: &Spec,
    report: &mut CaseResult,
) -> Result<()> {
    let Connection {
        io,
        initial,
        compression,
        server,
    } = connection;
    let (read, write) = tokio::io::split(io);
    let mut writer = Writer::new(write, !server);
    let (tx, mut rx) = mpsc::channel(64);
    let reader = Reader::new(read, initial, server, spec.max_frame_size);
    let guard = AbortReader(tokio::spawn(receive(
        reader,
        Messages::new(spec.max_message_size, compression)?,
        tx,
    )));
    let mut deflate = compression.map(Deflate::new).transpose()?;
    let start = Instant::now();
    let hard_deadline = start
        + Duration::from_millis(
            spec.case_timeout_ms
                .unwrap_or(case.timeout_ms)
                .min(case.timeout_ms),
        );
    let mut deadline = hard_deadline;
    let scripted = case.engine == "script";
    let normal_close = CloseExpectation {
        closed_by_me: true,
        close_code: vec![1000],
        require_clean: true,
        closed_by_wrong_endpoint_is_fatal: false,
    };
    let close = if scripted { &case.close } else { &normal_close };
    let expected = case
        .expected
        .iter()
        .map(|(status, events)| {
            Ok((
                status.as_str(),
                events
                    .iter()
                    .map(Event::from_expected)
                    .collect::<Result<Vec<_>>>()?,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut observed = Vec::new();
    let mut observed_bytes = 0usize;
    let mut sent_close = false;
    let mut received_close = false;
    let mut forced = false;
    let mut eof = false;
    let mut protocol_error = None;
    let mut action_index = 0;
    let mut source = if scripted {
        None
    } else {
        Some(PayloadSource::new(case)?)
    };
    let mut pending = Bytes::new();
    let target_count = spec.message_count.unwrap_or(case.count).min(case.count);
    let unimplemented = case.compression() && compression.is_none();
    if unimplemented {
        writer
            .frame(&Frame::new(8, Bytes::from_static(&[3, 232])), 0)
            .await?;
        writer.flush().await?;
        sent_close = true;
        report.closed_by_me = true;
        deadline = deadline.min(Instant::now() + Duration::from_millis(spec.close_timeout_ms));
    } else if let Some(source) = &mut source {
        pending = source.next();
        timeout_at(
            deadline,
            send_workload(&mut writer, case, pending.clone(), &mut deflate),
        )
        .await
        .map_err(|_| Error::Timeout)??;
    }
    loop {
        // Actions sharing a timestamp are batched. Explicit sync/chop requests
        // still flush at their required boundaries.
        if scripted && !received_close {
            while action_index < case.actions.len()
                && start + Duration::from_millis(case.actions[action_index].at) <= Instant::now()
            {
                let action = &case.actions[action_index];
                action_index += 1;
                if action.kind == "kill" {
                    forced = true;
                    if !received_close {
                        report.closed_by_me = true;
                    }
                    break;
                }
                if action.kind == "mark" {
                    if !sent_close && let Some(event) = &action.event {
                        observed.push(Event::from_expected(event)?);
                    }
                    continue;
                }
                if action.kind == "close" {
                    if sent_close {
                        continue;
                    }
                    sent_close = true;
                    report.closed_by_me = true;
                    deadline =
                        deadline.min(Instant::now() + Duration::from_millis(spec.close_timeout_ms));
                }
                if let Err(e) = timeout_at(deadline, perform(&mut writer, action))
                    .await
                    .map_err(|_| Error::Timeout)
                    .and_then(|r| r)
                {
                    // A peer may drop immediately after detecting an intentionally
                    // malformed frame. Drain its already queued response first.
                    report.result = format!("write ended: {e}");
                    action_index = case.actions.len();
                    break;
                }
            }
            if forced {
                break;
            }
            if let Err(e) = timeout_at(deadline, writer.flush())
                .await
                .map_err(|_| Error::Timeout)
                .and_then(|r| r)
            {
                report.result = e.to_string();
            }
        }
        let next_action = if scripted && !received_close {
            case.actions
                .get(action_index)
                .map(|a| start + Duration::from_millis(a.at))
        } else {
            None
        };
        let wake = next_action.unwrap_or(deadline).min(deadline);
        tokio::select! {
            biased;
            incoming=rx.recv()=>match incoming {
                Some(Incoming::Event(event))=>{
                    if received_close {continue;}
                    if event.kind=="ping" {
                        timeout_at(deadline,writer.frame(&Frame::new(10,event.payload.clone()),0)).await.map_err(|_|Error::Timeout)??;writer.flush().await?;
                    }
                    if scripted {
                        if observed.len()>=1024 {protocol_error=Some("too many unsolicited events".into());break;}
                        observed_bytes = observed_bytes.saturating_add(event.payload.len());
                        if observed_bytes > spec.max_message_size { protocol_error = Some("received event storage limit exceeded".into()); break; }
                        observed.push(event);
                        if !sent_close && close.closed_by_me && !case.suppress_close && !expected.is_empty() && expected.iter().all(|(_,events)|*events==observed) {
                            let code=close.close_code.first().copied().unwrap_or(1000);
                            writer.frame(&Frame::new(8,Bytes::copy_from_slice(&code.to_be_bytes())),0).await?;writer.flush().await?;
                            sent_close=true;report.closed_by_me=true;deadline=deadline.min(Instant::now()+Duration::from_millis(spec.close_timeout_ms));
                        }
                    }else if event.kind=="message" && !sent_close {
                        if event.binary!=case.binary || event.payload!=pending {protocol_error=Some("echo payload or message type differs".into());break;}
                        report.messages+=1;
                        if report.messages==target_count {
                            writer.frame(&Frame::new(8,Bytes::from_static(&[3,232])),0).await?;writer.flush().await?;
                            sent_close=true;report.closed_by_me=true;deadline=deadline.min(Instant::now()+Duration::from_millis(spec.close_timeout_ms));
                        }else if let Some(source)=&mut source {pending=source.next();timeout_at(deadline,send_workload(&mut writer,case,pending.clone(),&mut deflate)).await.map_err(|_|Error::Timeout)??;}
                    }
                },
                Some(Incoming::Close(payload,error,bytes,frames))=>{
                    report.rx_bytes=bytes;report.rx_frames=frames;
                    report.remote_close_code=payload.get(..2).map(|code|u16::from_be_bytes([code[0],code[1]]));
                    if let Some(error)=error { protocol_error=Some(error.to_string()); break; }
                    received_close=true;
                    if !sent_close {report.closed_by_me=false;match async { writer.frame(&Frame::new(8,payload),0).await?;writer.flush().await }.await { Ok(()) => sent_close=true, Err(Error::Io(_)) => {}, Err(e) => return Err(e) }}
                    deadline=deadline.min(Instant::now()+Duration::from_millis(spec.close_timeout_ms));
                    if server { let _ = writer.shutdown().await; }
                },
                Some(Incoming::End(bytes,frames))=>{report.rx_bytes=bytes;report.rx_frames=frames;eof=true;break;},
                Some(Incoming::Error(e,bytes,frames))=>{
                    report.rx_bytes=bytes;report.rx_frames=frames;
                    match e {
                        Error::Io(ref e) if matches!(e.kind(),std::io::ErrorKind::ConnectionReset|std::io::ErrorKind::BrokenPipe|std::io::ErrorKind::UnexpectedEof)=>{},
                        _=>protocol_error=Some(e.to_string()),
                    }
                    break;
                },
                None=>break,
            },
            _=sleep_until(wake)=>{
                if Instant::now()>=deadline {forced=true;if !received_close{report.closed_by_me=true;}break;}
            }
        }
    }
    drop(guard);
    report.tx_bytes = writer.bytes;
    report.tx_frames = writer.frames;
    report.was_clean = sent_close && received_close && eof && !forced;
    if scripted {
        report.messages = observed.iter().filter(|e| e.kind == "message").count();
        report.received = observed.iter().map(Event::summary).collect();
        if let Some((status, _)) = expected.iter().find(|(_, events)| *events == observed) {
            report.behavior = (*status).into();
            report.result = "Events match upstream expectation".into();
        } else {
            report.result = "Received events differ from all expected event sequences".into();
        }
    } else if unimplemented {
        report.behavior = "UNIMPLEMENTED".into();
        report.result = "Peer did not negotiate permessage-deflate".into();
    } else if report.messages == target_count {
        report.behavior = "OK".into();
        report.result = format!("All {target_count} echoes matched byte-for-byte");
    } else {
        report.result = format!(
            "Received {} of {target_count} expected echoes",
            report.messages
        );
    }
    let (behavior, reason) = if close.closed_by_me != report.closed_by_me {
        ("FAILED", "Wrong endpoint initiated closing")
    } else if close.require_clean && !report.was_clean {
        ("UNCLEAN", "Clean closing handshake required")
    } else if report
        .remote_close_code
        .is_some_and(|code| !close.close_code.contains(&code))
    {
        ("WRONG CODE", "Unexpected peer close code")
    } else if !server && forced {
        ("FAILED BY CLIENT", "Client forced TCP shutdown")
    } else {
        ("OK", "Closing behavior matches expectation")
    };
    report.behavior_close = behavior.into();
    report.result_close = reason.into();
    if (close.closed_by_wrong_endpoint_is_fatal && close.closed_by_me != report.closed_by_me)
        || (case.wrong_code_fatal && behavior == "WRONG CODE")
    {
        report.behavior = "FAILED".into();
        report.result = reason.into();
    }
    if let Some(error) = protocol_error {
        report.behavior = "FAILED".into();
        report.result = error;
    }
    // Upstream explicitly leaves these cases informational even when a peer
    // reflects an out-of-range code. Preserve that verdict after diagnostics.
    if case.informational {
        report.behavior = "INFORMATIONAL".into();
        report.behavior_close = "INFORMATIONAL".into();
        report.result = "Upstream informational close behavior".into();
    }
    Ok(())
}
