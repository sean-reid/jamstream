//! A lively deterministic fake session. Everything animated is a pure
//! function of a frame counter, so snapshot tests freeze the counter and
//! `jamstream-app --demo` lets it run. No randomness, no wall clock.

use std::sync::{Arc, Mutex};

use crate::avatar::disc_color;
use crate::runtime::{
    AudioFaultView, AvatarHandle, BroadcastReadiness, BroadcastView, ChatLine, Command, ConnState,
    CostView, DestinationId, DestinationState, DestinationView, DeviceModeView, FaderView,
    LevelsView, MemberId, MemberView, MetronomeView, RateOutcomesView, RecordState, RecordView,
    Role, Runtime, Snapshot, StatsView, StreamPlatform, StreamView, TokenId,
};
use crate::theme;

/// The frame snapshot tests freeze at; chosen so meters sit mid-scale.
pub const FROZEN_FRAME: u64 = 1234;

/// What the encoder is configured for, from the same catalog the server
/// reads, so the demo's readout is the number a real session shows.
pub fn demo_bitrate_kbps() -> u32 {
    let catalog = jamstream_stream::PlatformCatalog::bundled();
    catalog.video().kbps + catalog.audio().kbps
}

const HOURLY_MICROUSD: u64 = 16_800;
/// Elapsed time the demo session pretends to have before frame zero.
const BASE_ELAPSED_SECS: u64 = 47 * 60 + 12;

/// Unity gain, centered, unmuted: the state every fader starts from.
const FLAT: FaderView = FaderView {
    gain_db: 0.0,
    pan: 0.0,
    muted: false,
};

fn fv(gain_db: f32, pan: f32, muted: bool) -> FaderView {
    FaderView {
        gain_db,
        pan,
        muted,
    }
}

struct Member {
    id: u16,
    name: &'static str,
    role: Role,
    fader: FaderView,
    /// The member's fader in the broadcast mix; host snapshots only.
    bcast: FaderView,
    /// Decoded pixels, as the live runtime would hand them over. Two demo
    /// members carry one so snapshots exercise both the picture and the
    /// initials fallback.
    avatar: Option<AvatarHandle>,
}

/// A procedural stand-in portrait: a tinted field with a head-and-shoulders
/// silhouette in the member's own hashed hue. Deterministic, no asset files,
/// and deliberately not a photograph. One demo avatar is wide and one is
/// square, so the cover crop is visible in the snapshots rather than assumed.
///
/// The hash is a synthetic key: nothing transfers in the demo, and the UI
/// only needs it to be stable per member for the texture cache.
fn demo_avatar(name: &str, w: u32, h: u32) -> AvatarHandle {
    let base = disc_color(name);
    let top = theme::blend(base, theme::DARK.well, 0.35);
    let bottom = theme::blend(base, theme::DARK.text_primary, 0.18);
    let head = theme::blend(base, theme::DARK.text_primary, 0.62);
    let shoulders = theme::blend(base, theme::DARK.well, 0.15);
    let short = w.min(h) as f32 / 2.0;
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            // Centered on the short side, so the square crop keeps the
            // whole silhouette whatever the aspect is.
            let cx = (x as f32 + 0.5 - w as f32 / 2.0) / short;
            let cy = (y as f32 + 0.5 - h as f32 / 2.0) / short;
            let field = theme::blend(top, bottom, (cy + 1.0) / 2.0);
            let color = if cx * cx + (cy + 0.28) * (cy + 0.28) < 0.40 * 0.40 {
                head
            } else if cx * cx + (cy - 1.05) * (cy - 1.05) < 0.80 * 0.80 {
                shoulders
            } else {
                field
            };
            rgba.extend_from_slice(&[color.r(), color.g(), color.b(), 255]);
        }
    }
    AvatarHandle {
        hash: format!("demo-{name}-{w}x{h}"),
        width: w,
        height: h,
        rgba: Arc::from(rgba.into_boxed_slice()),
    }
}

/// A configured destination, as the demo's stand-in server tracks it. There
/// is deliberately no key field: the demo takes the key the panel sends and
/// drops it on the spot, which is what the real client does too.
struct Destination {
    id: DestinationId,
    platform: StreamPlatform,
    state: DestinationState,
    dropped_frames: u64,
    repeated_frames: u64,
}

struct DemoState {
    frame: u64,
    members: Vec<Member>,
    extra_chat: Vec<ChatLine>,
    metronome: MetronomeView,
    revoked: Vec<u16>,
    /// Members the roster no longer counts as present. Nothing else in the
    /// demo produces one, so the strip's disconnected note and its presence
    /// dot had no fixture at all.
    away: Vec<u16>,
    /// Members the roster reports as quiet: connected, seat held, nothing
    /// heard from them for `MEMBER_QUIET_AFTER_MS`. A real session passes
    /// through this window in seconds, so only a fixture can hold it still.
    quiet: Vec<u16>,
    left: bool,
    audition: bool,
    hear_self: bool,
    destinations: Vec<Destination>,
    /// Whether the session can broadcast at all. None is a session that has
    /// not been asked, which is every demo one; a fixture pins the answer.
    readiness: Option<BroadcastReadiness>,
    record: RecordView,
    /// Why this computer has no audio stream, when it has none. The real
    /// runtime fills this from the device that refused; a fixture pins it so
    /// the sentence a silent musician reads can be looked at.
    device_error: Option<String>,
    /// The sharing mode and rate-outcome report a fixture pins; the real
    /// runtime reads them off the audio backend. None by default, which is
    /// what every platform without the split reports and what a session with
    /// no stream shows.
    device_mode: Option<DeviceModeView>,
    rate: Option<RateOutcomesView>,
    /// Whether the playout ring is pinned as crackling: the real runtime
    /// derives this from the ring's own counters, so a fixture pins the
    /// answer instead.
    crackling: bool,
    /// Each direction's loss rate, pinned per direction because that is the
    /// whole point of them: a fixture holds one losing while the other is
    /// clean, which no single figure could express.
    uplink_loss_pct: f32,
    downlink_loss_pct: f32,
    /// What the audio stream is doing wrong: the real runtime derives it from
    /// the reopen cadence, which a fixture has no way to run, so it pins the
    /// answer instead.
    audio_fault: Option<AudioFaultView>,
    /// Your own display name, as [`Command::SetOwnName`] set it: the demo
    /// stands in for the roster fanout the real server answers with.
    own_name: Option<String>,
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

    /// The design maximum: 10 musicians and 10 listeners. `frozen` freezes
    /// the frame counter for snapshots; a running instance animates.
    pub fn full(frame: u64, is_host: bool, frozen: bool) -> Self {
        let rt = Self::build(is_host, frame, frozen);
        let musicians = [
            ("Theo", -2.0_f32, 0.2_f32, false),
            ("Ivy", -4.5, -0.6, false),
            ("Noor", 1.5, 0.0, false),
            ("Kai", -9.0, 0.5, true),
            ("Zoe", -0.5, -0.2, false),
            ("Raul", -12.0, 0.0, false),
        ];
        let listeners = [
            "Omar", "Pia", "Finn", "Nia", "Eli", "Rosa", "Jun", "Ada", "Max",
        ];
        let mut s = rt.state.lock().expect("demo state");
        let mut id = s.members.len() as u16;
        for (name, gain_db, pan, muted) in musicians {
            s.members.insert(
                (id - 1) as usize,
                Member {
                    id,
                    name,
                    role: Role::Musician,
                    fader: fv(gain_db, pan, muted),
                    bcast: FLAT,
                    avatar: None,
                },
            );
            id += 1;
        }
        for name in listeners {
            s.members.push(Member {
                id,
                name,
                role: Role::Listener,
                fader: FLAT,
                bcast: FLAT,
                avatar: None,
            });
            id += 1;
        }
        drop(s);
        rt
    }

    /// Names at the 64-char protocol cap plus long chat lines; frozen.
    pub fn long_names(frame: u64, is_host: bool) -> Self {
        const LONG_A: &str = "Bartholomew Alexander Montgomery Fitzgerald Oyelaran-Wieczorek III";
        const LONG_B: &str = "Anastasia Wilhelmina Barrington-Smythe of the Greater Hebrides Isle";
        let rt = Self::build(is_host, frame, true);
        {
            let mut s = rt.state.lock().expect("demo state");
            s.members[1].name = &LONG_A[..64.min(LONG_A.len())];
            s.members[2].name = &LONG_B[..64.min(LONG_B.len())];
            s.extra_chat.push(ChatLine {
                from_name: LONG_A[..64.min(LONG_A.len())].to_owned(),
                from_id: MemberId(1),
                text: "the monitor mix on my end could use a little less low end \
                       between 80 and 120 Hz, and maybe a touch more of the click \
                       track, if anyone has a hand free before the next take"
                    .to_owned(),
                at_ms: 200_000,
            });
        }
        rt
    }

    fn build(is_host: bool, frame: u64, frozen: bool) -> Self {
        let members = vec![
            Member {
                id: 0,
                name: "Sam",
                role: Role::Musician,
                fader: FLAT,
                bcast: fv(-1.0, 0.0, false),
                avatar: None,
            },
            Member {
                id: 1,
                name: "Ana",
                role: Role::Musician,
                fader: fv(-3.0, -0.4, false),
                bcast: fv(-2.0, -0.3, false),
                // Wide: the cover crop takes a centered square of it.
                avatar: Some(demo_avatar("Ana", 96, 48)),
            },
            Member {
                id: 2,
                name: "Ben",
                role: Role::Musician,
                fader: fv(-1.5, 0.3, false),
                bcast: fv(-4.5, 0.35, false),
                avatar: Some(demo_avatar("Ben", 64, 64)),
            },
            Member {
                id: 3,
                name: "Mira",
                role: Role::Musician,
                fader: fv(-6.0, 0.0, true),
                bcast: fv(-12.0, 0.0, true),
                avatar: None,
            },
            Member {
                id: 4,
                name: "Lea",
                role: Role::Listener,
                fader: FLAT,
                bcast: FLAT,
                avatar: None,
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
                away: Vec::new(),
                quiet: Vec::new(),
                left: false,
                audition: false,
                hear_self: false,
                destinations: Vec::new(),
                readiness: None,
                record: RecordView::default(),
                device_error: None,
                device_mode: None,
                rate: None,
                crackling: false,
                uplink_loss_pct: 0.0,
                downlink_loss_pct: 0.2,
                audio_fault: None,
                own_name: None,
            }),
            is_host,
            frozen,
        }
    }

    /// Pins the broadcast state, ids assigned in the given order. Lets a
    /// snapshot hold a state a real pipeline passes through in seconds; no
    /// key is involved, which is the point.
    pub fn set_destinations(&self, entries: &[(StreamPlatform, DestinationState)]) {
        // One encode feeds every destination, so both frame counters are one
        // counter each and the pipeline reports the same pair on every row
        // (jamstream_stream::pipeline, `status`). Per-destination figures that
        // disagree are a state the product cannot reach, and this snapshot
        // reaches the docs site. Nonzero whenever anything has gone wrong at
        // all, so both readouts are still exercised.
        //
        // Repeats far outnumber losses, which is the shape a struggling machine
        // really has: it runs out of time to draw long before the encoder's
        // queue starts refusing frames.
        let anything_wrong = entries
            .iter()
            .any(|(_, state)| matches!(state, DestinationState::Failed { .. }));
        let (repeats, losses) = if anything_wrong { (41, 3) } else { (0, 0) };
        let mut s = self.state.lock().expect("demo state");
        s.destinations = entries
            .iter()
            .enumerate()
            .map(|(i, (platform, state))| Destination {
                id: DestinationId(i as u16),
                platform: *platform,
                state: state.clone(),
                dropped_frames: losses,
                repeated_frames: repeats,
            })
            .collect();
    }

    /// Marks one member as no longer connected, the way the roster does when
    /// somebody's client goes quiet without leaving.
    pub fn set_away(&self, member: u16, away: bool) {
        let mut s = self.state.lock().expect("demo state");
        s.away.retain(|id| *id != member);
        if away {
            s.away.push(member);
        }
    }

    /// Marks one member quiet, the way the server's roster does two seconds
    /// after it last heard from them.
    ///
    /// Away wins in the snapshot rather than here, because that is where the
    /// server's own rule lives: it clears the flag when it drops a member, so a
    /// fixture that set both would be showing a roster the wire cannot carry.
    pub fn set_quiet(&self, member: u16, quiet: bool) {
        let mut s = self.state.lock().expect("demo state");
        s.quiet.retain(|id| *id != member);
        if quiet {
            s.quiet.push(member);
        }
    }

    /// Pins the reason this computer has no audio stream, the way the real
    /// runtime publishes the one the device gave it.
    pub fn set_device_error(&self, reason: Option<&str>) {
        let mut s = self.state.lock().expect("demo state");
        s.device_error = reason.map(str::to_owned);
    }

    /// Pins what the audio stream is doing wrong, as the real runtime derives
    /// it from the reopen cadence.
    pub fn set_audio_fault(&self, fault: Option<AudioFaultView>) {
        let mut s = self.state.lock().expect("demo state");
        s.audio_fault = fault;
    }

    /// Pins the sharing mode, as the real runtime reads it off the audio
    /// backend after an open.
    pub fn set_device_mode(&self, mode: Option<DeviceModeView>) {
        let mut s = self.state.lock().expect("demo state");
        s.device_mode = mode;
    }

    /// Pins the rate outcomes, as the real runtime publishes them after an
    /// open: which rung each direction landed on and what it costs.
    pub fn set_rate(&self, rate: Option<RateOutcomesView>) {
        let mut s = self.state.lock().expect("demo state");
        s.rate = rate;
    }

    /// Pins whether the session can broadcast at all, as the server's relay
    /// probe reports it. A fixture holds the answer a real session gets from a
    /// VM whose relay never came up.
    pub fn set_broadcast_readiness(&self, readiness: Option<BroadcastReadiness>) {
        let mut s = self.state.lock().expect("demo state");
        s.readiness = readiness;
    }

    /// Pins the recorder's reported state, the way [`Self::set_destinations`]
    /// pins the broadcast: a fixture can hold a state a real take only
    /// passes through in time, uploading included.
    pub fn set_record(&self, state: RecordState, stems: bool) {
        let mut s = self.state.lock().expect("demo state");
        s.record = RecordView { state, stems };
    }

    /// Pins each direction's loss rate: the uplink as the server's Stats
    /// report gives it, the downlink as the local jitter buffer's own window
    /// closes on it.
    pub fn set_loss(&self, uplink_pct: f32, downlink_pct: f32) {
        let mut s = self.state.lock().expect("demo state");
        s.uplink_loss_pct = uplink_pct;
        s.downlink_loss_pct = downlink_pct;
    }

    /// Pins whether the playout ring reads as crackling, as the real runtime
    /// derives it from the ring's own underrun counters.
    pub fn set_crackling(&self, crackling: bool) {
        let mut s = self.state.lock().expect("demo state");
        s.crackling = crackling;
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
            uplink_loss_pct: Some(s.uplink_loss_pct),
            downlink_loss_pct: Some(s.downlink_loss_pct),
            mouth_to_ear_ms: Some(8.4 + 0.5 * ((f as f64) * 0.019).sin() as f32),
            device_mode: s.device_mode,
            rate: s.rate,
            crackling: s.crackling,
            playout_low_frames: None,
            // Nothing fills a ring here, so there is no thread to time.
            wake: None,
        };

        let members = s
            .members
            .iter()
            // Revoking ejects: `ServerCore` drops the member and sends a
            // roster without them, so the demo cannot leave a strip behind
            // or the mixer would contradict the invites panel beside it.
            .filter(|m| !s.revoked.contains(&m.id))
            .map(|m| MemberView {
                id: MemberId(m.id),
                // Your strip carries the name you set, the way the roster
                // fanout would answer a SetName.
                name: match (&s.own_name, m.id) {
                    (Some(name), 0) => name.clone(),
                    _ => m.name.to_owned(),
                },
                role: m.role,
                connected: !s.away.contains(&m.id),
                // Never both: the server clears quiet when it gives up on a
                // member, so a snapshot claiming gone and quiet at once would
                // be a fixture inventing a roster.
                quiet: !s.away.contains(&m.id) && s.quiet.contains(&m.id),
                is_you: m.id == 0,
                fader: m.fader,
                token: self.is_host.then_some(TokenId([m.id as u8; 16])),
                avatar: m.avatar.clone(),
            })
            .collect();

        let mut chat = Self::scripted_chat();
        chat.extend(s.extra_chat.iter().cloned());

        let broadcast = self.is_host.then(|| BroadcastView {
            faders: s
                .members
                .iter()
                .filter(|m| m.role == Role::Musician)
                .map(|m| (MemberId(m.id), m.bcast))
                .collect(),
            audition: s.audition,
        });

        let bitrate_kbps = demo_bitrate_kbps();
        let stream = StreamView {
            destinations: s
                .destinations
                .iter()
                .map(|d| DestinationView {
                    id: d.id,
                    platform: d.platform,
                    state: d.state.clone(),
                    bitrate_kbps,
                    dropped_frames: d.dropped_frames,
                    repeated_frames: d.repeated_frames,
                })
                .collect(),
            readiness: s.readiness.clone(),
        };

        Snapshot {
            stats,
            members,
            chat,
            levels,
            metronome: s.metronome,
            broadcast,
            stream,
            record: s.record.clone(),
            cost: self.is_host.then_some(CostView {
                hourly_microusd: HOURLY_MICROUSD,
                accrued_microusd: HOURLY_MICROUSD * elapsed_secs / 3600,
                elapsed_secs,
            }),
            hear_self: s.hear_self,
            session_short: "a3f29c41".to_owned(),
            server_addr: "203.0.113.10:43210".to_owned(),
            is_host: self.is_host,
            device_error: s.device_error.clone(),
            audio_fault: s.audio_fault,
        }
    }

    /// The state on its own, from the one flag that decides it. The demo's
    /// snapshot copies a roster and a chat buffer like the live one does,
    /// and the frame loop asks this every frame.
    fn conn_state(&self) -> ConnState {
        if self.state.lock().expect("demo state").left {
            ConnState::Idle
        } else {
            ConnState::Joined
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
            Command::SetBroadcastFader {
                member,
                gain_db,
                pan,
                muted,
            } => {
                if let Some(m) = s.members.iter_mut().find(|m| m.id == member.0) {
                    m.bcast = fv(gain_db, pan, muted);
                }
            }
            Command::SetBroadcastAudition(on) => s.audition = on,
            Command::SetHearSelf(on) => s.hear_self = on,
            // The demo stands in for the runtime's decode step: raw file
            // bytes in, pixels on your own strip out, or the initials disc
            // back when they are dropped.
            Command::SetOwnAvatar(bytes) => {
                let decoded = bytes.and_then(|bytes| {
                    crate::avatar::decode(format!("own-{}", bytes.len()), &bytes)
                        .inspect_err(|err| tracing::warn!(%err, "demo avatar did not decode"))
                        .ok()
                });
                if let Some(me) = s.members.iter_mut().find(|m| m.id == 0) {
                    me.avatar = decoded;
                }
            }
            // The key is taken by value and dropped here, unstored: the
            // demo's stand-in server keeps what the wire status carries and
            // nothing more.
            Command::AddDestination { id, platform, .. } => {
                s.destinations.retain(|d| d.id != id);
                let streaming = s
                    .destinations
                    .iter()
                    .any(|d| d.state != DestinationState::Idle);
                s.destinations.push(Destination {
                    id,
                    platform,
                    // Added mid-broadcast, a destination joins the running
                    // encode; added before one, it waits.
                    state: if streaming {
                        DestinationState::Live
                    } else {
                        DestinationState::Idle
                    },
                    dropped_frames: 0,
                    repeated_frames: 0,
                });
            }
            Command::RemoveDestination(id) => s.destinations.retain(|d| d.id != id),
            Command::StartStream => {
                for d in &mut s.destinations {
                    d.state = DestinationState::Live;
                }
            }
            Command::StopStream => {
                for d in &mut s.destinations {
                    d.state = DestinationState::Idle;
                    d.dropped_frames = 0;
                    d.repeated_frames = 0;
                }
            }
            // The demo recorder transitions instantly; the state a fixture
            // needs is the one the button just asked for.
            Command::StartRecord => s.record.state = RecordState::Recording,
            Command::StopRecord => s.record.state = RecordState::Idle,
            Command::SetOwnName(name) => {
                let name = name.trim();
                if !name.is_empty() {
                    s.own_name = Some(name.to_owned());
                }
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

    /// The runtime underneath, so a test can move the session on mid-frame the
    /// way the server does: a relay that comes up, a take that starts.
    pub fn inner(&self) -> &R {
        &self.inner
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

/// So a test can hold the recorder and hand the same instance to
/// [`crate::app::JamApp`] as its boxed runtime, the way `Arc<LiveRuntime>`
/// serves the real app.
impl<R: Runtime + Sync> Runtime for Arc<RecordingRuntime<R>> {
    fn snapshot(&self) -> Snapshot {
        (**self).snapshot()
    }

    fn send(&self, cmd: Command) {
        (**self).send(cmd);
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

    /// The cheap accessor and the snapshot must never disagree, in either
    /// state: the frame loop leaves a session on what this says.
    #[test]
    fn the_state_accessor_agrees_with_the_snapshot() {
        let rt = DemoRuntime::host();
        assert_eq!(rt.conn_state(), rt.snapshot().stats.state);
        rt.send(Command::Leave);
        assert_eq!(rt.conn_state(), ConnState::Idle);
        assert_eq!(rt.conn_state(), rt.snapshot().stats.state);
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

    /// The demo answers a SetOwnName the way the server's roster fanout
    /// does, so the join screen's field is exercised in fixtures too.
    #[test]
    fn set_own_name_renames_your_own_strip() {
        let rt = DemoRuntime::frozen(FROZEN_FRAME, false);
        rt.send(Command::SetOwnName("  Ana Lucia  ".to_owned()));
        let snap = rt.snapshot();
        let me = snap.members.iter().find(|m| m.is_you).expect("you");
        assert_eq!(me.name, "Ana Lucia", "trimmed, like the wire's copy");
        // Whitespace alone is not a name; nothing changes.
        rt.send(Command::SetOwnName("   ".to_owned()));
        let me_id = me.id;
        let snap = rt.snapshot();
        let me = snap.members.iter().find(|m| m.id == me_id).expect("you");
        assert_eq!(me.name, "Ana Lucia");
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
        assert!(snap.broadcast.is_none());
        assert!(!snap.is_host);
    }

    #[test]
    fn broadcast_commands_are_reflected_in_the_next_snapshot() {
        let rt = DemoRuntime::host();
        rt.send(Command::SetBroadcastFader {
            member: MemberId(2),
            gain_db: -7.5,
            pan: -0.25,
            muted: true,
        });
        rt.send(Command::SetBroadcastAudition(true));
        let broadcast = rt.snapshot().broadcast.expect("host broadcast view");
        assert!(broadcast.audition);
        let (_, ben) = broadcast
            .faders
            .iter()
            .find(|(id, _)| *id == MemberId(2))
            .expect("Ben in broadcast faders");
        assert_eq!((ben.gain_db, ben.pan, ben.muted), (-7.5, -0.25, true));
        // Only musicians have broadcast faders; Lea the listener does not.
        assert!(broadcast.faders.iter().all(|(id, _)| *id != MemberId(4)));
    }
}
