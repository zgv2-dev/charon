//! A serial source, over a pseudo-terminal: what a USB telemetry radio looks like to the router.
//!
//! Linux only. On macOS the serialport crate always sets the baud rate with the IOSSIOSPEED ioctl, which a
//! pseudo-terminal refuses (ENOTTY), so no pty can stand in for a radio there; real USB serial ports take it. Windows
//! has no ptys.
#![cfg(target_os = "linux")]

use std::{
    ffi::CStr,
    fs::File,
    io::{Read, Write},
    os::fd::FromRawFd,
    time::Duration,
};

use charon::{config::Output, endpoint::Endpoint, frame::testutil::heartbeat, router::Router};
use tokio::{net::UdpSocket, time::timeout};

/// The radio's end, and the port name the router opens.
fn pty() -> (File, String) {
    let (mut master, mut slave) = (0, 0);
    let mut name = [0 as libc::c_char; 128];
    let r = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            name.as_mut_ptr(),
            // *mut on macOS, *const on Linux: a null *mut passes as either
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(r, 0, "openpty");
    // raw, so the line discipline passes MAVLink bytes untouched
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        libc::tcgetattr(slave, &mut t);
        libc::cfmakeraw(&mut t);
        libc::tcsetattr(slave, libc::TCSANOW, &t);
    }
    let name = unsafe { CStr::from_ptr(name.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    // the slave stays open in this process for the test's life, so the port doesn't hang up between opens
    std::mem::forget(unsafe { File::from_raw_fd(slave) });
    (unsafe { File::from_raw_fd(master) }, name)
}

#[tokio::test]
async fn a_serial_vehicle_reaches_the_gcs_and_hears_it() {
    let (mut radio, port) = pty();
    let gcs = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let to = gcs.local_addr().unwrap();
    let source = Endpoint::Serial { port, baud: 57_600 };
    let (r, _) = Router::start(source, &[Output::new("GCS", Endpoint::UdpOut(to))])
        .await
        .unwrap();
    let mut state = r.snapshot().source.unwrap().state;
    for _ in 0..100 {
        if state == charon::link::State::Connected {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        state = r.snapshot().source.unwrap().state;
    }
    assert_eq!(state, charon::link::State::Connected, "the port opens");

    let hb = heartbeat(0, 1, 1, 2, 3);
    radio.write_all(&hb).unwrap();
    let mut b = [0u8; 512];
    let (n, router_port) = timeout(Duration::from_secs(2), gcs.recv_from(&mut b))
        .await
        .expect("forwarded")
        .unwrap();
    assert_eq!(&b[..n], hb);

    let reply = heartbeat(0, 255, 190, 6, 8);
    gcs.send_to(&reply, router_port).await.unwrap();
    let got = tokio::task::spawn_blocking(move || {
        let mut got = vec![0u8; reply.len()];
        radio.read_exact(&mut got).unwrap();
        (got, reply)
    });
    let (got, reply) = timeout(Duration::from_secs(2), got)
        .await
        .expect("written")
        .unwrap();
    assert_eq!(got, reply);
}
