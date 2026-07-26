//! Noise IK handshake and the encrypted transport that follows it. The
//! client learns the server's static public key from its invite, so the
//! server is authenticated on the first flight and there is nothing to
//! trust-on-first-use. The client's own identity is the signed token it
//! carries in the first message, not its (fresh per connection) keypair.

use ed25519_dalek::Signature;
use serde::{Deserialize, Serialize};
use snow::{Builder, HandshakeState, StatelessTransportState};
use zeroize::Zeroizing;

use crate::ids::{MemberId, SessionId};
use crate::invite::{Invite, Token};
use crate::replay::ReplayWindow;
use crate::wire;
use crate::{Error, PROTOCOL_VERSION};

pub const NOISE_PATTERN: &str = "Noise_IK_25519_ChaChaPoly_BLAKE2s";

/// X25519 static keypair for the session server. Generated on the host
/// machine at session create time and injected via provider user-data.
pub struct Keypair {
    pub private: Zeroizing<Vec<u8>>,
    pub public: [u8; 32],
}

pub fn generate_keypair() -> Keypair {
    let kp = Builder::new(NOISE_PATTERN.parse().expect("pattern"))
        .generate_keypair()
        .expect("keygen");
    Keypair {
        private: Zeroizing::new(kp.private),
        public: kp.public.try_into().expect("32-byte x25519 key"),
    }
}

/// Client identity material carried in the first handshake message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandshakePayload {
    pub token: Token,
    pub signature: Signature,
}

/// Server's reply payload in the second handshake message. `sample_clock`
/// is the session sample counter at admission, letting the client align
/// its timeline before the first media frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Welcome {
    pub member_id: MemberId,
    pub sample_clock: u64,
}

fn prologue(version: u16, session_id: &SessionId) -> Vec<u8> {
    let mut p = b"jamstream".to_vec();
    p.extend_from_slice(&version.to_le_bytes());
    p.extend_from_slice(&session_id.0);
    p
}

/// Client side. `new` produces the full wire datagram to send.
pub struct Initiator {
    hs: HandshakeState,
}

impl Initiator {
    pub fn new(invite: &Invite) -> Result<(Self, Vec<u8>), Error> {
        let params = NOISE_PATTERN.parse().expect("pattern");
        let builder = Builder::new(params);
        let local = builder.generate_keypair()?;
        let mut hs = Builder::new(NOISE_PATTERN.parse().expect("pattern"))
            .local_private_key(&local.private)?
            .remote_public_key(&invite.server_pk)?
            .prologue(&prologue(PROTOCOL_VERSION, &invite.session_id))?
            .build_initiator()?;
        let payload = postcard::to_stdvec(&HandshakePayload {
            token: invite.token.clone(),
            signature: invite.signature,
        })?;
        let mut msg = vec![0u8; payload.len() + 160];
        let len = hs.write_message(&payload, &mut msg)?;
        Ok((
            Self { hs },
            wire::build_handshake_init(PROTOCOL_VERSION, &msg[..len]),
        ))
    }

    /// Consumes the server's handshake response and yields the transport.
    pub fn finish(mut self, resp_noise: &[u8]) -> Result<(Session, Welcome), Error> {
        let mut payload = vec![0u8; resp_noise.len()];
        let len = self.hs.read_message(resp_noise, &mut payload)?;
        let welcome: Welcome = postcard::from_bytes(&payload[..len])?;
        let ts = self.hs.into_stateless_transport_mode()?;
        Ok((Session::new(ts), welcome))
    }
}

/// Server side, split so the caller can verify the token (signature,
/// expiry, revocation list) before spending anything on a response.
pub struct Responder {
    hs: HandshakeState,
}

impl Responder {
    pub fn read_init(
        server_private: &[u8],
        session_id: &SessionId,
        version_theirs: u16,
        noise_msg: &[u8],
    ) -> Result<(HandshakePayload, Self), Error> {
        if version_theirs != PROTOCOL_VERSION {
            return Err(Error::VersionMismatch {
                ours: PROTOCOL_VERSION,
                theirs: version_theirs,
            });
        }
        let mut hs = Builder::new(NOISE_PATTERN.parse().expect("pattern"))
            .local_private_key(server_private)?
            .prologue(&prologue(version_theirs, session_id))?
            .build_responder()?;
        let mut payload = vec![0u8; noise_msg.len()];
        let len = hs.read_message(noise_msg, &mut payload)?;
        let hp: HandshakePayload = postcard::from_bytes(&payload[..len])?;
        Ok((hp, Self { hs }))
    }

    /// Called only after the token checks out. Produces the wire datagram
    /// and the server's transport state.
    pub fn respond(mut self, welcome: &Welcome) -> Result<(Session, Vec<u8>), Error> {
        let payload = postcard::to_stdvec(welcome)?;
        let mut msg = vec![0u8; payload.len() + 160];
        let len = self.hs.write_message(&payload, &mut msg)?;
        let packet = wire::build_handshake_resp(&msg[..len]);
        let ts = self.hs.into_stateless_transport_mode()?;
        Ok((Session::new(ts), packet))
    }
}

/// Established encrypted transport: explicit counters on the wire so
/// packets survive reordering, a replay window on receive.
pub struct Session {
    ts: StatelessTransportState,
    send_counter: u64,
    replay: ReplayWindow,
}

impl Session {
    fn new(ts: StatelessTransportState) -> Self {
        Self {
            ts,
            send_counter: 0,
            replay: ReplayWindow::new(),
        }
    }

    /// Encrypts `plaintext` into a complete wire datagram. `member` tells
    /// the receiving end which session member this connection belongs to.
    pub fn seal(&mut self, member: MemberId, plaintext: &[u8]) -> Result<Vec<u8>, Error> {
        let counter = self.send_counter;
        self.send_counter += 1;
        let mut ct = vec![0u8; plaintext.len() + 16];
        let len = self.ts.write_message(counter, plaintext, &mut ct)?;
        Ok(wire::build_transport(member, counter, &ct[..len]))
    }

    /// Decrypts a transport packet body. Replay-checked: a counter is
    /// accepted at most once and only within the reorder window.
    pub fn open(&mut self, counter: u64, ciphertext: &[u8]) -> Result<Vec<u8>, Error> {
        let mut plain = vec![0u8; ciphertext.len()];
        let len = self
            .ts
            .read_message(counter, ciphertext, &mut plain)
            .map_err(|_| Error::Decrypt)?;
        // Window update happens after authentication so garbage cannot
        // poison the window state.
        if !self.replay.accept(counter) {
            return Err(Error::Replay);
        }
        plain.truncate(len);
        Ok(plain)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{Role, TokenId};
    use crate::invite::Issuer;
    use crate::invite::verify_token;

    fn setup() -> (Issuer, Keypair, Invite) {
        let issuer = Issuer::generate();
        let server = generate_keypair();
        let session = SessionId::generate();
        let token = Token {
            member_id: MemberId(2),
            role: Role::Musician,
            name_hint: None,
            expires_unix: 4_000_000_000,
            jti: TokenId::generate(),
        };
        let invite = issuer.mint(
            session,
            vec!["192.0.2.4:43210".parse().unwrap()],
            server.public,
            token,
        );
        (issuer, server, invite)
    }

    fn handshake() -> (Session, Session, Welcome) {
        let (issuer, server, invite) = setup();
        let (initiator, init_packet) = Initiator::new(&invite).unwrap();

        let wire::Packet::HandshakeInit { version, noise } = wire::parse(&init_packet).unwrap()
        else {
            panic!("expected init");
        };
        let (hp, responder) =
            Responder::read_init(&server.private, &invite.session_id, version, noise).unwrap();
        verify_token(
            &issuer.public_key(),
            &invite.session_id,
            &server.public,
            &hp.token,
            &hp.signature,
            1_000,
        )
        .unwrap();

        let welcome = Welcome {
            member_id: hp.token.member_id,
            sample_clock: 480_000,
        };
        let (server_session, resp_packet) = responder.respond(&welcome).unwrap();
        let wire::Packet::HandshakeResp { noise } = wire::parse(&resp_packet).unwrap() else {
            panic!("expected resp");
        };
        let (client_session, got_welcome) = initiator.finish(noise).unwrap();
        (client_session, server_session, got_welcome)
    }

    #[test]
    fn full_handshake_and_transport() {
        let (mut client, mut server, welcome) = handshake();
        assert_eq!(
            welcome,
            Welcome {
                member_id: MemberId(2),
                sample_clock: 480_000
            }
        );

        // Client to server.
        let pkt = client.seal(MemberId(2), b"hello from the client").unwrap();
        let wire::Packet::Transport {
            member,
            counter,
            ciphertext,
        } = wire::parse(&pkt).unwrap()
        else {
            panic!("expected transport");
        };
        assert_eq!(member, MemberId(2));
        assert_eq!(
            server.open(counter, ciphertext).unwrap(),
            b"hello from the client"
        );

        // Server to client.
        let pkt = server.seal(MemberId(2), b"and back").unwrap();
        let wire::Packet::Transport {
            counter,
            ciphertext,
            ..
        } = wire::parse(&pkt).unwrap()
        else {
            panic!("expected transport");
        };
        assert_eq!(client.open(counter, ciphertext).unwrap(), b"and back");
    }

    #[test]
    fn replayed_packet_is_rejected() {
        let (mut client, mut server, _) = handshake();
        let pkt = client.seal(MemberId(2), b"once only").unwrap();
        let wire::Packet::Transport {
            counter,
            ciphertext,
            ..
        } = wire::parse(&pkt).unwrap()
        else {
            panic!("expected transport");
        };
        assert!(server.open(counter, ciphertext).is_ok());
        assert!(matches!(
            server.open(counter, ciphertext),
            Err(Error::Replay)
        ));
    }

    #[test]
    fn tampered_ciphertext_is_rejected() {
        let (mut client, mut server, _) = handshake();
        let pkt = client.seal(MemberId(2), b"intact").unwrap();
        let wire::Packet::Transport {
            counter,
            ciphertext,
            ..
        } = wire::parse(&pkt).unwrap()
        else {
            panic!("expected transport");
        };
        let mut bad = ciphertext.to_vec();
        bad[0] ^= 0x40;
        assert!(matches!(server.open(counter, &bad), Err(Error::Decrypt)));
        // The failed decrypt must not have burned the counter.
        assert!(server.open(counter, ciphertext).is_ok());
    }

    #[test]
    fn wrong_server_key_cannot_complete() {
        let (_, _, invite) = setup();
        let imposter = generate_keypair();
        let (_, init_packet) = Initiator::new(&invite).unwrap();
        let wire::Packet::HandshakeInit { version, noise } = wire::parse(&init_packet).unwrap()
        else {
            panic!("expected init");
        };
        // An imposter without the real static private key cannot even read
        // the first message.
        assert!(
            Responder::read_init(&imposter.private, &invite.session_id, version, noise).is_err()
        );
    }

    #[test]
    fn version_mismatch_is_explicit() {
        let (_, server, invite) = setup();
        let err = Responder::read_init(&server.private, &invite.session_id, 2, b"junk");
        assert!(matches!(
            err,
            Err(Error::VersionMismatch { ours: 1, theirs: 2 })
        ));
    }
}
