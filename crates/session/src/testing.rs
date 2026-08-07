//! One pump pass for a `ClientCore` on a real socket, shared by the
//! integration suites that drive a live jamstreamd over UDP.

use tokio::net::UdpSocket;

use crate::client::{ClientCore, ClientEvent};

/// Flushes whatever `core` wants sent, takes everything the socket is already
/// holding, and returns the events that fell out of the pass.
///
/// The drain is `try_recv` and nothing else, so it ends the moment the queue
/// is empty and can never consume slower than the server produces. A pass
/// that instead takes a fixed count of packets with a receive timeout is
/// correct only while that timeout is shorter than the gap between arrivals:
/// a joined musician's downlink is a mix frame every 2.5 ms, and a timeout
/// above that never fires, so every pass waits for its last packet and costs
/// the count times the gap in wall time. Whatever the caller does between
/// passes is then production the pump never catches up on, and it reads
/// further behind real time on every iteration.
///
/// Send errors are dropped. A pump runs across teardown, where the server has
/// already gone and a refused send is the expected result rather than a
/// fault.
pub async fn pump(socket: &UdpSocket, core: &mut ClientCore, now_ms: u64) -> Vec<ClientEvent> {
    for pkt in core.poll(now_ms) {
        let _ = socket.send(&pkt).await;
    }
    let mut buf = [0u8; 2048];
    while let Ok(len) = socket.try_recv(&mut buf) {
        for pkt in core.handle_datagram(now_ms, &buf[..len]) {
            let _ = socket.send(&pkt).await;
        }
    }
    core.events()
}
