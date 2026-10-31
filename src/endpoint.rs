//! Where MAVLink comes from or goes to, written the way MAVProxy and mavlink-router users already know:
//! `udpin:0.0.0.0:14551`, `udpout:127.0.0.1:14550`, `tcp:192.168.1.5:5760`, `tcpin:0.0.0.0:5760`, `serial:COM5:921600`.

use std::{fmt, net::SocketAddr, str::FromStr};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Endpoint {
    /// Listens on a UDP port; sends back to whoever sent last.
    UdpIn(SocketAddr),
    /// Sends to a fixed UDP address from an ephemeral port, and hears replies there.
    UdpOut(SocketAddr),
    /// Connects to a TCP server, and again whenever the connection drops. A host name is kept as written.
    TcpClient(String),
    /// Accepts any number of TCP clients; everyone gets everything.
    TcpServer(SocketAddr),
    /// A serial port (`COM5`, `/dev/ttyUSB0`) at a baud rate; opened again if it goes away.
    Serial { port: String, baud: u32 },
}

impl Endpoint {
    /// Byte streams need their frames cut out; UDP datagrams are passed as they are.
    pub fn is_stream(&self) -> bool {
        matches!(
            self,
            Endpoint::TcpClient(_) | Endpoint::TcpServer(_) | Endpoint::Serial { .. }
        )
    }

    /// Whether sending to `self` would arrive at `listener` on this machine: an output that does is a loop.
    pub fn reaches(&self, listener: &Endpoint) -> bool {
        let (target, listen) = match (self, listener) {
            (Endpoint::UdpOut(t), Endpoint::UdpIn(l)) => (*t, *l),
            (Endpoint::TcpClient(t), Endpoint::TcpServer(l)) => match t.parse::<SocketAddr>() {
                Ok(t) => (t, *l),
                Err(_) => return false, // a host name: can't tell without resolving it
            },
            (Endpoint::Serial { port: a, .. }, Endpoint::Serial { port: b, .. }) => {
                return a.eq_ignore_ascii_case(b);
            }
            _ => return false,
        };
        target.port() == listen.port()
            && (target.ip().is_loopback()
                || listen.ip().is_unspecified()
                || target.ip() == listen.ip())
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Endpoint::UdpIn(a) => write!(f, "udpin:{a}"),
            Endpoint::UdpOut(a) => write!(f, "udpout:{a}"),
            Endpoint::TcpClient(a) => write!(f, "tcp:{a}"),
            Endpoint::TcpServer(a) => write!(f, "tcpin:{a}"),
            Endpoint::Serial { port, baud } => write!(f, "serial:{port}:{baud}"),
        }
    }
}

impl FromStr for Endpoint {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let s = s.trim();
        let (kind, rest) = s
            .split_once(':')
            .ok_or_else(|| format!("\"{s}\": expected kind:address, e.g. udpin:0.0.0.0:14551"))?;
        let addr = |r: &str| {
            r.parse::<SocketAddr>()
                .map_err(|_| format!("\"{r}\" isn't an ip:port"))
        };
        match kind.to_ascii_lowercase().as_str() {
            "udpin" | "udp" => addr(rest).map(Endpoint::UdpIn),
            "udpout" => addr(rest).map(Endpoint::UdpOut),
            "tcpin" => addr(rest).map(Endpoint::TcpServer),
            "tcp" => match rest.rsplit_once(':') {
                Some((host, port)) if !host.is_empty() && port.parse::<u16>().is_ok() => {
                    Ok(Endpoint::TcpClient(rest.to_string()))
                }
                _ => Err(format!("\"{rest}\" isn't a host:port")),
            },
            "serial" => {
                // COM5:921600, /dev/ttyUSB0:57600, or the port alone at 57600, the autopilots' telemetry default
                let (port, baud) = match rest.rsplit_once(':') {
                    Some((p, b)) if b.bytes().all(|c| c.is_ascii_digit()) && !b.is_empty() => (
                        p,
                        b.parse::<u32>()
                            .map_err(|_| format!("baud \"{b}\" is too high"))?,
                    ),
                    _ => (rest, 57_600),
                };
                if port.is_empty() || baud == 0 {
                    return Err(format!("\"{rest}\" isn't a port:baud"));
                }
                Ok(Endpoint::Serial {
                    port: port.to_string(),
                    baud,
                })
            }
            other => Err(format!(
                "unknown kind \"{other}\": udpin, udpout, tcp, tcpin or serial"
            )),
        }
    }
}

impl TryFrom<String> for Endpoint {
    type Error = String;
    fn try_from(s: String) -> Result<Self, String> {
        s.parse()
    }
}

impl From<Endpoint> for String {
    fn from(e: Endpoint) -> String {
        e.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_kind() {
        for s in [
            "udpin:0.0.0.0:14551",
            "udpout:127.0.0.1:14550",
            "tcp:192.168.1.5:5760",
            "tcp:vehicle.local:5760",
            "tcpin:0.0.0.0:5760",
            "serial:COM5:921600",
            "serial:/dev/ttyUSB0:57600",
            "udpout:[::1]:14550",
        ] {
            assert_eq!(s.parse::<Endpoint>().unwrap().to_string(), s);
        }
    }

    #[test]
    fn serial_baud_defaults_to_57600() {
        assert_eq!(
            "serial:COM3".parse::<Endpoint>().unwrap(),
            Endpoint::Serial {
                port: "COM3".into(),
                baud: 57_600
            }
        );
    }

    #[test]
    fn rejects_what_it_cant_open() {
        for bad in [
            "",
            "14551",
            "udpin:14551",
            "ftp:1.2.3.4:21",
            "tcp:5760",
            "serial:",
            "serial:COM5:0",
            "tcpin:host:1",
        ] {
            assert!(bad.parse::<Endpoint>().is_err(), "{bad}");
        }
    }

    #[test]
    fn spots_outputs_that_loop() {
        let listen: Endpoint = "udpin:0.0.0.0:14551".parse().unwrap();
        let out = |s: &str| s.parse::<Endpoint>().unwrap();
        assert!(out("udpout:127.0.0.1:14551").reaches(&listen));
        assert!(out("udpout:192.168.1.5:14551").reaches(&listen));
        assert!(!out("udpout:127.0.0.1:14550").reaches(&listen));
        assert!(!out("tcp:127.0.0.1:14551").reaches(&listen));
        assert!(out("tcp:127.0.0.1:5760").reaches(&out("tcpin:0.0.0.0:5760")));
        assert!(out("serial:com5:57600").reaches(&out("serial:COM5:921600")));
        let lan: Endpoint = "udpin:192.168.1.9:14551".parse().unwrap();
        assert!(out("udpout:127.0.0.1:14551").reaches(&lan));
        assert!(
            !out("udpout:192.168.1.5:14551").reaches(&lan),
            "another machine's port"
        );
    }

    #[test]
    fn serializes_as_its_string() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct T {
            e: Endpoint,
        }
        let t = T {
            e: "tcpin:0.0.0.0:5760".parse().unwrap(),
        };
        let text = toml::to_string(&t).unwrap();
        assert_eq!(text.trim(), r#"e = "tcpin:0.0.0.0:5760""#);
        assert_eq!(toml::from_str::<T>(&text).unwrap(), t);
    }
}
