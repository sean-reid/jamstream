//! A lively deterministic fake session. Everything animated is a pure
//! function of a frame counter, so snapshot tests freeze the counter and
//! `jamstream-app --demo` lets it run. No randomness, no wall clock.

use std::sync::Mutex;

use crate::runtime::{
    ChatLine, Command, ConnState, CostView, FaderView, LevelsView, MemberId, MemberView,
    MetronomeView, Role, Runtime, Snapshot, StatsView, TokenId,
};

/// The frame snapshot tests freeze at; chosen so meters sit mid-scale.
pub const FROZEN_FRAME: u64 = 1234;

const HOURLY_MICROUSD: u64 = 16_800;
/// Elapsed time the demo session pretends to have before frame zero.
const BASE_ELAPSED_SECS: u64 = 47 * 60 + 12;

struct Member {
    id: u16,
    name: &'static str,
    role: Role,
    fader: FaderView,
}

struct DemoState {
    frame: u64,
    members: Vec<Member>,
    extra_chat: Vec<ChatLine>,
    metronome: MetronomeView,
    revoked: Vec<u16>,
    left: bool,
}

pub struct DemoRuntime {
    state: Mutex<DemoState>,
    is_host: bool,
    frozen: bool,
}

impl DemoRuntime {
    /// The host's view: cost ticker, metronome controls, revoke buttons.
    pub fn host() -> Self {
        Self::build(true, 0, false)
    }

    /// A plain musician's view.
    pub fn musician() -> Self {
        Self::build(false, 0, false)
    }

    /// Frozen at `frame` for snapshot tests; the counter never advances.
    pub fn frozen(frame: u64, is_host: bool) -> Self {
        Self::build(is_host, frame, true)
    }

    fn build(is_host: bool, frame: u64, frozen: bool) -> Self {
        let members = vec![
            Member {
                id: 0,
                name: "Sam",
                role: Role::Musician,
                fader: FaderView {
                    gain_db: 0.0,
                    pan: 0.0,
                    muted: false,
                },
            },
            Member {
                id: 1,
                name: "Ana",
                role: Role::Musician,
                fader: FaderView {
                    gain_db: -3.0,
                    pan: -0.4,
                    muted: false,
                },
            },
            Member {
                id: 2,
                name: "Ben",
                role: Role::Musician,
                fader: FaderView {
                    gain_db: -1.5,
                    pan: 0.3,
                    muted: false,
                },
            },
            Member {
                id: 3,
                name: "Mira",
                role: Role::Musician,
                fader: FaderView {
                    gain_db: -6.0,
                    pan: 0.0,
                    muted: true,
                },
            },
            Member {
                id: 4,
                name: "Lea",
                role: Role::Listener,
                fader: FaderView {
                    gain_db: 0.0,
                    pan: 0.0,
                    muted: false,
                },
            },
        ];
        DemoRuntime {
            state: Mutex::new(DemoState {
                frame,
                members,
                extra_chat: Vec::new(),
                metronome: MetronomeView {
                    bpm: 112,
                    beats_per_bar: 4,
                    enabled: true,
                    you_hear_click: true,
                },
                revoked: Vec::new(),
                left: false,
            }),
            is_host,
            frozen,
        }
    }

    fn scripted_chat() -> Vec<ChatLine> {
        let line = |id: u16, name: &str, text: &str, at_ms: u64| ChatLine {
            from_name: name.to_owned(),
            from_id: MemberId(id),
            text: text.to_owned(),
            at_ms,
        };
        vec![
            line(1, "Ana", "tuning up, one minute", 12_000),
            line(2, "Ben", "click at 112 works for me", 41_000),
            line(0, "Sam", "same, keeping it at 112", 55_000),
            line(
                3,
                "Mira",
                "my monitor mix is a bit bass heavy, fixing",
                93_000,
            ),
            line(4, "Lea", "sounds great from the listener side", 140_000),
            line(1, "Ana", "take it from the bridge?", 171_000),
        ]
    }
}

/// Deterministic 0..1 envelope: two incommensurate sines so meters breathe
/// instead of pulsing.
fn envelope(frame: u64, rate_a: f64, rate_b: f64, base: f64, amp: f64) -> f32 {
    let f = frame as f64;
    let v = base + amp * ((f * rate_a).sin() * 0.6 + (f * rate_b).sin() * 0.4);
    v.clamp(0.0, 1.0) as f32
}

impl Runtime for DemoRuntime {
    fn snapshot(&self) -> Snapshot {
        let mut s = self.state.lock().expect("demo state");
        if !self.frozen {
            s.frame += 1;
        }
        let f = s.frame;

        let input_peak = envelope(f, 0.11, 0.043, 0.55, 0.33);
        let output_peak = envelope(f, 0.083, 0.027, 0.62, 0.3);
        let levels = LevelsView {
            input_peak,
            input_rms: input_peak * 0.55,
            output_peak,
            output_rms: output_peak * 0.6,
        };

        let rtt = 14.0 + 1.6 * ((f as f64) * 0.027).sin() as f32;
        let elapsed_secs = BASE_ELAPSED_SECS + f / 60;
        let stats = StatsView {
            state: if s.left {
                ConnState::Idle
            } else {
                ConnState::Joined
            },
            rtt_ms: Some(rtt),
            jitter_depth: 3 + ((f / 240) % 2) as usize,
            jitter_target: 4,
            loss_pct: (0.2 + 0.15 * ((f as f64) * 0.011).sin() as f32).max(0.0),
            mouth_to_ear_ms: Some(8.4 + 0.5 * ((f as f64) * 0.019).sin() as f32),
        };

        let members = s
            .members
            .iter()
            .map(|m| MemberView {
                id: MemberId(m.id),
                name: m.name.to_owned(),
                role: m.role,
                connected: !s.revoked.contains(&m.id),
                is_you: m.id == 0,
                fader: m.fader,
                token: self.is_host.then_some(TokenId([m.id as u8; 16])),
            })
            .collect();

        let mut chat = Self::scripted_chat();
        chat.extend(s.extra_chat.iter().cloned());

        Snapshot {
            stats,
            members,
            chat,
            levels,
            metronome: s.metronome,
            cost: self.is_host.then_some(CostView {
                hourly_microusd: HOURLY_MICROUSD,
                accrued_microusd: HOURLY_MICROUSD * elapsed_secs / 3600,
                elapsed_secs,
            }),
            session_short: "deadbeef".to_owned(),
            server_addr: "203.0.113.10:43210".to_owned(),
            is_host: self.is_host,
        }
    }

    fn send(&self, cmd: Command) {
        let mut s = self.state.lock().expect("demo state");
        match cmd {
            Command::SetFader {
                member,
                gain_db,
                pan,
                muted,
            } => {
                if let Some(m) = s.members.iter_mut().find(|m| m.id == member.0) {
                    m.fader = FaderView {
                        gain_db,
                        pan,
                        muted,
                    };
                }
            }
            Command::SetClick(on) => s.metronome.you_hear_click = on,
            Command::SetMetronome {
                bpm,
                beats_per_bar,
                enabled,
            } => {
                s.metronome.bpm = bpm;
                s.metronome.beats_per_bar = beats_per_bar;
                s.metronome.enabled = enabled;
            }
            Command::SendChat(text) => {
                let at_ms = (BASE_ELAPSED_SECS + s.frame / 60) * 1000;
                s.extra_chat.push(ChatLine {
                    from_name: "Sam".to_owned(),
                    from_id: MemberId(0),
                    text,
                    at_ms,
                });
            }
            Command::Leave => s.left = true,
            Command::Revoke(jti) => {
                // The demo token is the member id repeated; reverse it.
                let id = jti.0[0] as u16;
                if !s.revoked.contains(&id) {
                    s.revoked.push(id);
                }
            }
        }
    }
}

/// Wraps any runtime and logs every command; interaction tests assert on
/// the log while the inner runtime keeps behaving.
pub struct RecordingRuntime<R: Runtime> {
    inner: R,
    log: Mutex<Vec<Command>>,
}

impl<R: Runtime> RecordingRuntime<R> {
    pub fn new(inner: R) -> Self {
        RecordingRuntime {
            inner,
            log: Mutex::new(Vec::new()),
        }
    }

    pub fn commands(&self) -> Vec<Command> {
        self.log.lock().expect("command log").clone()
    }
}

impl<R: Runtime> Runtime for RecordingRuntime<R> {
    fn snapshot(&self) -> Snapshot {
        self.inner.snapshot()
    }

    fn send(&self, cmd: Command) {
        self.log.lock().expect("command log").push(cmd.clone());
        self.inner.send(cmd);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frozen_snapshots_are_identical() {
        let rt = DemoRuntime::frozen(FROZEN_FRAME, true);
        assert_eq!(rt.snapshot(), rt.snapshot());
    }

    #[test]
    fn running_demo_advances() {
        let rt = DemoRuntime::host();
        let a = rt.snapshot();
        let b = rt.snapshot();
        assert_ne!(a.levels, b.levels);
        assert!(
            b.cost.expect("host cost").accrued_microusd
                >= a.cost.expect("host cost").accrued_microusd
        );
    }

    #[test]
    fn commands_are_reflected_in_the_next_snapshot() {
        let rt = DemoRuntime::host();
        rt.send(Command::SetFader {
            member: MemberId(1),
            gain_db: 2.5,
            pan: 0.1,
            muted: true,
        });
        rt.send(Command::SendChat("hello".to_owned()));
        let snap = rt.snapshot();
        let ana = snap.members.iter().find(|m| m.id == MemberId(1)).unwrap();
        assert_eq!(ana.fader.gain_db, 2.5);
        assert!(ana.fader.muted);
        assert_eq!(snap.chat.last().unwrap().text, "hello");
    }

    #[test]
    fn recording_runtime_logs_and_forwards() {
        let rt = RecordingRuntime::new(DemoRuntime::frozen(0, false));
        rt.send(Command::SetClick(false));
        assert_eq!(rt.commands(), vec![Command::SetClick(false)]);
        assert!(!rt.snapshot().metronome.you_hear_click);
    }

    #[test]
    fn musician_snapshot_has_no_host_data() {
        let snap = DemoRuntime::musician().snapshot();
        assert!(snap.cost.is_none());
        assert!(snap.members.iter().all(|m| m.token.is_none()));
        assert!(!snap.is_host);
    }
}
