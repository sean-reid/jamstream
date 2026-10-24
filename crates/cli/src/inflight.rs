//! Whether a session has a take in flight, asked of the server itself before
//! `jamstream end` destroys the machine it runs on.
//!
//! Destroying the server kills a take that is recording or uploading, and the
//! dead man's switch on the machine only defers the machine's own exits. The
//! only thing that knows the recorder's state is the server, so this joins
//! through the session's host invite, reads the status the server reports on
//! admission, and leaves. The server sends that status only when it is not
//! idle, so a join that settles with no report is an idle recorder.
//!
//! The host's seat is one seat. While the desktop app holds it this cannot
//! join, which is [`InFlight::HostConnected`]: the app has its own guard, so
//! the answer is to end from there.

use std::time::{Duration, Instant};

use jamstream_protocol::control::{MAX_DATAGRAM_BYTES, RecordingState};
use jamstream_protocol::invite::Invite;
use jamstream_session::client::{ClientCore, ClientEvent, ClientState, ServerCandidates};

use crate::state::SessionState;

/// What the probe learned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InFlight {
    /// The recorder is idle, failed, or the session never armed recording.
    Nothing,
    Recording,
    Uploading,
    /// The host's seat is taken, so the state could not be read from here.
    HostConnected,
    /// No server answered in time, or the record carries no host invite. The
    /// string says which.
    Unknown(String),
}

/// How long the whole probe may take. A join on a live server settles in
/// well under a second; a machine that is already gone never answers.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(4);

/// How long after admission to wait for a status report before calling the
/// recorder idle. The report rides the ordered control link beside the
/// admission itself, so this is a bound on reordering, not on loss.
const SETTLE_AFTER_JOIN: Duration = Duration::from_millis(500);

const TICK: Duration = Duration::from_millis(20);

/// Asks the session's server whether a take is in flight.
pub async fn probe(session: &SessionState) -> InFlight {
    let Some(record) = session.invites.iter().find(|i| i.role == "host") else {
        return InFlight::Unknown("the session record has no host invite".to_owned());
    };
    let invite = match Invite::decode(&record.invite) {
        Ok(invite) => invite,
        Err(err) => return InFlight::Unknown(format!("the host invite does not decode: {err}")),
    };
    match tokio::time::timeout(PROBE_TIMEOUT, ask(&invite)).await {
        Ok(answer) => answer,
        Err(_) => InFlight::Unknown("no answer from the session server".to_owned()),
    }
}

async fn ask(invite: &Invite) -> InFlight {
    let mut candidates = match ServerCandidates::new(invite) {
        Ok(c) => c,
        Err(err) => return InFlight::Unknown(err.to_string()),
    };
    let mut socket = match crate::launch::connected_socket(candidates.current()).await {
        Ok(s) => s,
        Err(err) => return InFlight::Unknown(err.to_string()),
    };
    let start = Instant::now();
    let now = || start.elapsed().as_millis() as u64;
    let (mut core, init) = match ClientCore::connect(invite, now()) {
        Ok(c) => c,
        Err(err) => return InFlight::Unknown(err.to_string()),
    };
    if socket.send(&init).await.is_err() {
        return InFlight::Unknown("cannot send to the session server".to_owned());
    }

    let mut tick = tokio::time::interval(TICK);
    let mut buf = [0u8; MAX_DATAGRAM_BYTES];
    let mut joined_at: Option<Instant> = None;
    let mut reported: Option<RecordingState> = None;
    let answer = loop {
        tokio::select! {
            _ = tick.tick() => {
                for pkt in core.poll(now()) {
                    let _ = socket.send(&pkt).await;
                }
            }
            result = socket.recv(&mut buf) => {
                if let Ok(len) = result {
                    for pkt in core.handle_datagram(now(), &buf[..len]) {
                        let _ = socket.send(&pkt).await;
                    }
                }
            }
        }
        for event in core.events() {
            if let ClientEvent::RecordStatus { state, .. } = event {
                reported = Some(state);
            }
        }
        match core.state() {
            ClientState::Joined => {
                let t0 = *joined_at.get_or_insert_with(Instant::now);
                match &reported {
                    Some(RecordingState::Recording) => break InFlight::Recording,
                    Some(RecordingState::Uploading) => break InFlight::Uploading,
                    Some(_) => break InFlight::Nothing,
                    None if t0.elapsed() >= SETTLE_AFTER_JOIN => break InFlight::Nothing,
                    None => {}
                }
            }
            ClientState::Connecting if core.session_full() => break InFlight::HostConnected,
            ClientState::TimedOut if candidates.has_alternatives() => {
                let next = candidates.advance();
                match crate::launch::connected_socket(next).await {
                    Ok(s) => socket = s,
                    Err(err) => break InFlight::Unknown(err.to_string()),
                }
                match core.reconnect(now()) {
                    Ok(init) => {
                        let _ = socket.send(&init).await;
                    }
                    Err(err) => break InFlight::Unknown(err.to_string()),
                }
            }
            ClientState::TimedOut => {
                break InFlight::Unknown("no answer from the session server".to_owned());
            }
            ClientState::Rejected { .. } | ClientState::Ejected { .. } => {
                break InFlight::Unknown("the session server refused the probe".to_owned());
            }
            ClientState::Connecting => {}
        }
    };
    if joined_at.is_some() {
        let _ = core.leave("end probe");
        for pkt in core.poll(now()) {
            let _ = socket.send(&pkt).await;
        }
    }
    answer
}
