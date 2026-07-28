//! Noise IK handshake and the encrypted transport that follows it. The
//! client learns the server's static public key from its invite, so the
//! server is authenticated on the first flight and there is nothing to
//! trust-on-first-use. The client's own identity is the signed token it
//! carries in the first message, not its (fresh per connection) keypair.

use blake2::{Blake2s256, Digest};
use serde::{Deserialize, Serialize};
use snow::{Builder, HandshakeState, StatelessTransportState};
use zeroize::Zeroizing;

use crate::ids::{MemberId, SessionId};
use crate::invite::{Invite, SignatureBytes, Token};
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

/// Derives the X25519 public key for a 32-byte private key, e.g. a server
/// recovering its public half from injected user-data.
pub fn derive_public(private: &[u8]) -> Result<[u8; 32], Error> {
    let private: [u8; 32] = private.try_into().map_err(|_| Error::Malformed)?;
    let secret = x25519_dalek::StaticSecret::from(private);
    Ok(x25519_dalek::PublicKey::from(&secret).to_bytes())
}

/// Domain separator for the version reject key. Nothing else derives a key
/// from this shared secret, but naming it costs a hash update.
const REJECT_KEY_DOMAIN: &[u8] = b"jamstream-version-reject-key-v1";

/// Hashes an X25519 shared secret into the reject MAC key. A shared secret is
/// not a uniform key, so it goes through Blake2s rather than being used raw.
/// A non-contributory exchange, which is what a low-order public key
/// produces, yields no key at all: the constant that comes back would be
/// derivable by anyone.
fn reject_key_from_dh(private: &[u8], public: &[u8]) -> Option<wire::RejectKey> {
    let private: [u8; 32] = private.try_into().ok()?;
    let public: [u8; 32] = public.try_into().ok()?;
    let shared = x25519_dalek::StaticSecret::from(private)
        .diffie_hellman(&x25519_dalek::PublicKey::from(public));
    if !shared.was_contributory() {
        return None;
    }
    let mut h = Blake2s256::new();
    h.update(REJECT_KEY_DOMAIN);
    h.update(shared.as_bytes());
    Some(wire::RejectKey::from_bytes(h.finalize().into()))
}

/// Domain separator for the cookie secret, so nothing derived from the static
/// private key can ever be mistaken for anything else derived from it.
const COOKIE_KEY_DOMAIN: &[u8] = b"jamstream-cookie-secret-v1";

/// The cookie secret for one epoch: a hash of the server's static private key
/// and the epoch number.
///
/// No random state and no stored table, which is the whole point of a
/// stateless cookie: the server can recompute what it handed out without
/// remembering that it handed anything out, and the core stays deterministic
/// under the harness. Rotating on an epoch bounds how long a cookie is worth
/// stealing; the caller accepts the previous epoch too so a rotation does not
/// invalidate a cookie in flight.
pub fn cookie_key(server_private: &[u8], epoch: u64) -> wire::CookieKey {
    let mut h = Blake2s256::new();
    h.update(COOKIE_KEY_DOMAIN);
    h.update(server_private);
    h.update(epoch.to_le_bytes());
    wire::CookieKey::from_bytes(h.finalize().into())
}

/// The key that authenticates a version reject for an init this server will
/// not otherwise process, recovered from the init itself.
///
/// The server reads the Noise first message with the prologue the sender's
/// own version implies, which is the only reason a mismatched init is
/// readable at all, and takes the initiator's static key out of it. That key
/// is fresh per connection, so the shared secret with the server's static key
/// belongs to exactly one connection attempt and to no invite holder.
///
/// The read costs two X25519 operations plus one AEAD open, so callers must
/// spend their rate-limit budget before calling: this is the only expensive
/// thing an unauthenticated peer can ask for on the reject path. `None` when
/// the message is not a first flight this server can read, which is every
/// forgery and every future version that changes the Noise pattern or the
/// prologue. Those get silence, which is what they got before.
pub fn reject_key_for_init(
    server_private: &[u8],
    session_id: &SessionId,
    version_theirs: u16,
    noise_msg: &[u8],
) -> Option<wire::RejectKey> {
    let mut hs = Builder::new(NOISE_PATTERN.parse().expect("pattern"))
        .local_private_key(server_private)
        .ok()?
        .prologue(&prologue(version_theirs, session_id))
        .ok()?
        .build_responder()
        .ok()?;
    let mut payload = vec![0u8; noise_msg.len()];
    hs.read_message(noise_msg, &mut payload).ok()?;
    reject_key_from_dh(server_private, hs.get_remote_static()?)
}

/// Client identity material carried in the first handshake message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandshakePayload {
    pub token: Token,
    /// Raw 64-byte Ed25519 signature; see [`SignatureBytes`] for why this is
    /// an array and not `ed25519_dalek::Signature`.
    #[serde(with = "crate::invite::sig_bytes")]
    pub signature: SignatureBytes,
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

/// A failed [`Initiator::finish`], carrying the initiator back to the caller.
///
/// snow checkpoints its symmetric state before a `read_message` and restores
/// it on failure without advancing the handshake pattern, so a response that
/// does not authenticate costs the handshake nothing. Handing the initiator
/// back is what lets a client ignore forged responses: anyone who can see a
/// connecting client's address can spray them, and starting over on each one
/// means the genuine response, computed against the init already sent, no
/// longer fits.
pub struct HandshakeRetry {
    pub error: Error,
    initiator: Option<Initiator>,
}

impl HandshakeRetry {
    /// The handshake state when it is still usable, which is every failure
    /// an attacker can cause. `None` means the response authenticated and
    /// then turned out to be unusable, which spends the state.
    pub fn into_initiator(self) -> Option<Initiator> {
        self.initiator
    }
}

impl std::fmt::Debug for HandshakeRetry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HandshakeRetry")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

/// Client side. `new` produces the full wire datagram to send. The handshake
/// state is boxed because it is most of a kilobyte and travels back through
/// [`HandshakeRetry`] on every response that does not verify, which an
/// attacker sets the rate of.
pub struct Initiator {
    hs: Box<HandshakeState>,
    /// Derived once at construction: a reject sprayed at a connecting client
    /// must cost it a MAC, not a Diffie-Hellman.
    reject_key: Option<wire::RejectKey>,
}

impl Initiator {
    pub fn new(invite: &Invite) -> Result<(Self, Vec<u8>), Error> {
        Self::claiming_version(invite, PROTOCOL_VERSION)
    }

    /// A first flight claiming some other protocol version, prologue and
    /// framing included. Nothing ships this: a client speaks one version.
    /// Tests need it because a version reject can only be drawn by an init
    /// the server can read and will not accept.
    #[cfg(any(test, feature = "testing"))]
    pub fn new_claiming_version(invite: &Invite, version: u16) -> Result<(Self, Vec<u8>), Error> {
        Self::claiming_version(invite, version)
    }

    fn claiming_version(invite: &Invite, version: u16) -> Result<(Self, Vec<u8>), Error> {
        let params = NOISE_PATTERN.parse().expect("pattern");
        let builder = Builder::new(params);
        let local = builder.generate_keypair()?;
        let reject_key = reject_key_from_dh(&local.private, &invite.server_pk);
        let mut hs = Builder::new(NOISE_PATTERN.parse().expect("pattern"))
            .local_private_key(&local.private)?
            .remote_public_key(&invite.server_pk)?
            .prologue(&prologue(version, &invite.session_id))?
            .build_initiator()?;
        let payload = postcard::to_stdvec(&HandshakePayload {
            token: invite.token.clone(),
            signature: invite.signature,
        })?;
        let mut msg = vec![0u8; payload.len() + 160];
        let len = hs.write_message(&payload, &mut msg)?;
        Ok((
            Self {
                hs: Box::new(hs),
                reject_key,
            },
            wire::build_handshake_init(version, &msg[..len]),
        ))
    }

    /// The key that authenticates a version reject answering this handshake.
    /// `None` when the invite names a server key no shared secret can be had
    /// with, in which case no reject is ever believable.
    pub fn reject_key(&self) -> Option<&wire::RejectKey> {
        self.reject_key.as_ref()
    }

    /// Consumes the server's handshake response and yields the transport.
    /// On failure the initiator comes back in the error so the caller can
    /// wait for a response that does verify; see [`HandshakeRetry`].
    pub fn finish(mut self, resp_noise: &[u8]) -> Result<(Session, Welcome), HandshakeRetry> {
        let mut payload = vec![0u8; resp_noise.len()];
        let len = match self.hs.read_message(resp_noise, &mut payload) {
            Ok(len) => len,
            Err(e) => {
                return Err(HandshakeRetry {
                    error: e.into(),
                    initiator: Some(self),
                });
            }
        };
        // Past here the response authenticated, so it came from the server
        // and the handshake state has advanced: nothing else can be read
        // with it, and the caller has no retry to make.
        let welcome: Welcome =
            postcard::from_bytes(&payload[..len]).map_err(|e| HandshakeRetry {
                error: e.into(),
                initiator: None,
            })?;
        let ts = self
            .hs
            .into_stateless_transport_mode()
            .map_err(|e| HandshakeRetry {
                error: e.into(),
                initiator: None,
            })?;
        Ok((Session::new(ts), welcome))
    }
}

/// Server side, split so the caller can verify the token (signature,
/// expiry, revocation list) before spending anything on a response.
pub struct Responder {
    hs: HandshakeState,
    /// The initiator's per-connection static key, taken out of the first
    /// message. Kept so a reject can be authenticated without reading that
    /// message a second time.
    remote_static: [u8; 32],
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
        // IK carries the initiator's static in the first message, so a
        // successful read always has one.
        let remote_static: [u8; 32] = hs
            .get_remote_static()
            .ok_or(Error::Malformed)?
            .try_into()
            .map_err(|_| Error::Malformed)?;
        Ok((hp, Self { hs, remote_static }))
    }

    /// The key that authenticates a reject answering this handshake, for a
    /// caller that read the init and then found it cannot admit the peer.
    ///
    /// One X25519 rather than the whole of [`reject_key_for_init`], since the
    /// initiator's static key is already out of the message. Deliberately not
    /// derived at construction: the happy path never wants it, and this runs
    /// on the task that also owns the 2.5 ms mix tick.
    pub fn reject_key(&self, server_private: &[u8]) -> Option<wire::RejectKey> {
        reject_key_from_dh(server_private, &self.remote_static)
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
        let mut out = Vec::new();
        self.seal_into(member, plaintext, &mut out)?;
        Ok(out)
    }

    /// `seal` into a caller-owned buffer (cleared first), encrypting in
    /// place: no intermediate ciphertext allocation. Contents are
    /// unspecified on error. The counter is burned either way.
    pub fn seal_into(
        &mut self,
        member: MemberId,
        plaintext: &[u8],
        out: &mut Vec<u8>,
    ) -> Result<(), Error> {
        let counter = self.send_counter;
        self.send_counter += 1;
        out.clear();
        wire::append_transport_header(member, counter, out);
        let header = out.len();
        out.resize(header + plaintext.len() + 16, 0);
        let len = self
            .ts
            .write_message(counter, plaintext, &mut out[header..])?;
        out.truncate(header + len);
        Ok(())
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
    fn derive_public_matches_generated_keypair() {
        let kp = generate_keypair();
        assert_eq!(derive_public(&kp.private).unwrap(), kp.public);
        assert!(matches!(derive_public(&[0u8; 31]), Err(Error::Malformed)));
    }

    #[test]
    fn seal_into_reuses_buffer_and_matches_seal() {
        let (mut client, mut server, _) = handshake();
        let mut buf = vec![0xFFu8; 512];
        for msg in [&b"first"[..], &b"second, longer message"[..]] {
            client.seal_into(MemberId(2), msg, &mut buf).unwrap();
            let wire::Packet::Transport {
                counter,
                ciphertext,
                ..
            } = wire::parse(&buf).unwrap()
            else {
                panic!("expected transport");
            };
            assert_eq!(server.open(counter, ciphertext).unwrap(), msg);
        }
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

    /// Pins the `HandshakePayload` encoding against the published beta. This
    /// payload rides inside the encrypted Noise IK first message, so a change
    /// here breaks the handshake itself, not just invite strings: a peer would
    /// decrypt successfully and then fail to parse. Same fence as
    /// `invite_wire_encoding_is_pinned`; fix the encoding, not the vector.
    #[test]
    fn handshake_payload_wire_encoding_is_pinned() {
        // The exact 92 bytes produced by 0.1.1-beta (ed25519 2.2.3): a
        // 28-byte token followed by 64 bare signature bytes, no length prefix.
        #[rustfmt::skip]
        const V2_HANDSHAKE_HEX: &str = "03000103616e6180d0acf30e09090909090909090909090909090909719eaeb59a4eac1698d844f7f649100f996eca2dffd9aa11d350ffaa08a26bf49f071be5d2902e70293c393fc28f96b1582321f19e8beaaf3e7970c0482d7801";

        let issuer = Issuer::from_bytes(&[7u8; 32]);
        let invite = issuer.mint(
            SessionId([5u8; 16]),
            vec!["203.0.113.10:43210".parse().unwrap()],
            [7u8; 32],
            Token {
                member_id: MemberId(3),
                role: Role::Musician,
                name_hint: Some("ana".into()),
                expires_unix: 4_000_000_000,
                jti: TokenId([9u8; 16]),
            },
        );
        let hp = HandshakePayload {
            token: invite.token.clone(),
            signature: invite.signature,
        };
        let bytes = postcard::to_stdvec(&hp).expect("serialize");
        assert_eq!(
            data_encoding::HEXLOWER.encode(&bytes),
            V2_HANDSHAKE_HEX,
            "handshake payload encoding drifted from the 0.1.1-beta bytes"
        );
        assert_eq!(bytes.len(), 92);
        // Beta-encoded bytes still parse back.
        let back: HandshakePayload = postcard::from_bytes(&bytes).expect("decode");
        assert_eq!(back.signature, hp.signature);
        assert_eq!(back.token, hp.token);
    }

    /// The reject key is a shared secret between the server's static key and
    /// the client key behind one init. The server derives it from the init
    /// itself, the client from the handshake it started, and an invite,
    /// which carries only the server's public key, is no help to anyone else.
    #[test]
    fn only_the_server_and_the_client_that_sent_an_init_derive_its_reject_key() {
        let (_, server, invite) = setup();
        let (initiator, init_packet) = Initiator::new_claiming_version(&invite, 9).unwrap();
        let wire::Packet::HandshakeInit { version, noise } = wire::parse(&init_packet).unwrap()
        else {
            panic!("expected init");
        };
        assert_eq!(version, 9);

        let server_key = reject_key_for_init(&server.private, &invite.session_id, version, noise)
            .expect("server derives the key from the init");
        let reject =
            wire::build_version_reject(&server_key, PROTOCOL_VERSION, version, &init_packet);
        let wire::Packet::VersionReject { ours, theirs, mac } = wire::parse(&reject).unwrap()
        else {
            panic!("expected reject");
        };
        assert!(wire::verify_version_reject(
            initiator.reject_key().expect("client derives the key"),
            ours,
            theirs,
            &mac,
            &init_packet
        ));

        // Another handshake with the same invite, so another client key: the
        // reject that answers one connection attempt does not answer another.
        let (other, _) = Initiator::new_claiming_version(&invite, 9).unwrap();
        assert!(!wire::verify_version_reject(
            other.reject_key().unwrap(),
            ours,
            theirs,
            &mac,
            &init_packet
        ));

        // Holding the invite is holding the server's public key. Without the
        // private half the init is unreadable and no key comes out of it.
        let imposter = generate_keypair();
        assert!(
            reject_key_for_init(&imposter.private, &invite.session_id, version, noise).is_none()
        );
        // Nor does an init this server cannot read, whatever its length.
        assert!(
            reject_key_for_init(&server.private, &invite.session_id, version, &[0x5A; 96])
                .is_none()
        );
        // Nor one whose claimed version does not match the prologue it was
        // written under, which is what a replayed init relabelled looks like.
        assert!(reject_key_for_init(&server.private, &invite.session_id, 8, noise).is_none());
    }

    /// A capacity reject is produced after the init has been read, so the
    /// server takes the key out of the handshake it already has rather than
    /// reading the message again. It has to be the same key the client
    /// derived, or a full session would look like silence to it.
    #[test]
    fn a_responder_recovers_the_same_reject_key_the_initiator_holds() {
        let (_, server, invite) = setup();
        let (initiator, init_packet) = Initiator::new(&invite).unwrap();
        let wire::Packet::HandshakeInit { version, noise } = wire::parse(&init_packet).unwrap()
        else {
            panic!("expected init");
        };
        let (_, responder) =
            Responder::read_init(&server.private, &invite.session_id, version, noise).unwrap();

        let key = responder
            .reject_key(&server.private)
            .expect("the responder holds the initiator static");
        let reject = wire::build_capacity_reject(&key, &init_packet);
        let wire::Packet::CapacityReject { mac } = wire::parse(&reject).unwrap() else {
            panic!("expected a capacity reject");
        };
        assert!(wire::verify_capacity_reject(
            initiator.reject_key().expect("client derives the key"),
            &mac,
            &init_packet
        ));

        // Another connection attempt with the same invite derives another
        // key, so one session's reject is not another's.
        let (other, _) = Initiator::new(&invite).unwrap();
        assert!(!wire::verify_capacity_reject(
            other.reject_key().unwrap(),
            &mac,
            &init_packet
        ));
        // And a server without the static private key gets no key at all,
        // which is every invite holder.
        let imposter = generate_keypair();
        let imposter_key = responder
            .reject_key(&imposter.private)
            .expect("a well-formed key still produces a secret");
        assert!(!wire::verify_capacity_reject(
            &imposter_key,
            &mac,
            &init_packet
        ));
    }

    /// The cookie secret is a hash of the static private key and the epoch,
    /// which is what makes the cookie stateless: the server recomputes what it
    /// handed out without having remembered handing anything out, and the core
    /// stays deterministic under the harness because there is no RNG in it.
    #[test]
    fn the_cookie_secret_is_a_pure_function_of_the_key_and_the_epoch() {
        let server = generate_keypair();
        let other = generate_keypair();
        let src: std::net::IpAddr = "203.0.113.7".parse().unwrap();

        let cookie =
            |private: &[u8], epoch: u64| wire::cookie_for(&cookie_key(private, epoch), src);
        // Same inputs, same cookie, every time and in any process.
        assert_eq!(cookie(&server.private, 7), cookie(&server.private, 7));
        // A rotation changes it, and so does a different server.
        assert_ne!(cookie(&server.private, 7), cookie(&server.private, 8));
        assert_ne!(cookie(&server.private, 7), cookie(&other.private, 7));
        // Nothing an invite carries produces one: the private key is the only
        // input, so no client can mint a cookie for itself.
        assert_ne!(cookie(&server.private, 7), cookie(&server.public, 7));
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
