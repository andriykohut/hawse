use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use socket2::{Domain, Protocol, SockRef, Socket, TcpKeepalive, Type};
use tokio::net::{TcpListener, TcpStream, UdpSocket};

#[derive(Clone, Copy)]
enum Flavor {
    Tcp,
    Udp,
}

impl Flavor {
    fn socket(self, domain: Domain) -> io::Result<Socket> {
        match self {
            Self::Tcp => Socket::new(domain, Type::STREAM, Some(Protocol::TCP)),
            Self::Udp => Socket::new(domain, Type::DGRAM, Some(Protocol::UDP)),
        }
    }
}

/// An unspecified IPv6 `bind` answers on every interface; any other address answers only there.
pub fn bind_tcp(bind: IpAddr, port: u16) -> io::Result<TcpListener> {
    let socket = bound(Flavor::Tcp, bind, port)?;
    socket.listen(1024)?;
    TcpListener::from_std(socket.into())
}

/// Addressed as `bind_tcp` is.
pub fn bind_udp(bind: IpAddr, port: u16) -> io::Result<UdpSocket> {
    UdpSocket::from_std(bound(Flavor::Udp, bind, port)?.into())
}

/// The tunnel's own default quiet time.
const KEEPALIVE_IDLE: Duration = Duration::from_secs(30);

/// For a visitor's socket and the one to a local service. A stream has no idle timeout, so a peer
/// that vanished without a FIN would otherwise hold its pump, and the stream and sockets under it,
/// for as long as the tunnel stayed up.
///
/// Only the quiet time is set, as on the tunnel's TCP connection, and the probe schedule is left to
/// the kernel: another nine or ten minutes on stock Linux and macOS, so a peer is given up on only
/// after going unheard for that long, and one that answers is never closed for being idle.
pub fn keepalive(socket: &TcpStream) -> io::Result<()> {
    let keepalive = TcpKeepalive::new().with_time(KEEPALIVE_IDLE);
    SockRef::from(socket).set_tcp_keepalive(&keepalive)
}

fn bound(flavor: Flavor, bind: IpAddr, port: u16) -> io::Result<Socket> {
    match bind {
        IpAddr::V6(addr) if addr.is_unspecified() => every_interface(flavor, port),
        addr => {
            let domain = if addr.is_ipv4() {
                Domain::IPV4
            } else {
                Domain::IPV6
            };
            let socket = flavor.socket(domain)?;
            prepare(flavor, &socket, SocketAddr::new(addr, port))?;
            Ok(socket)
        }
    }
}

/// One dual-stack IPv6 socket, falling back to IPv4 on a host without IPv6.
fn every_interface(flavor: Flavor, port: u16) -> io::Result<Socket> {
    match dual_stack(flavor, port) {
        Ok(socket) => Ok(socket),
        // A port something already holds must fail the bind: IPv4 alone would answer only part of
        // the visitors while the port is reported as bound.
        Err(v6) if v6.kind() == io::ErrorKind::AddrInUse => Err(v6),
        // A host can hand out an IPv6 socket and still refuse `::`, so the
        // fallback keys off the whole attempt rather than the constructor.
        Err(v6) => ipv4_only(flavor, port).map_err(|_| v6),
    }
}

fn dual_stack(flavor: Flavor, port: u16) -> io::Result<Socket> {
    let socket = flavor.socket(Domain::IPV6)?;
    socket.set_only_v6(false)?;
    prepare(
        flavor,
        &socket,
        SocketAddr::from((Ipv6Addr::UNSPECIFIED, port)),
    )?;
    Ok(socket)
}

fn ipv4_only(flavor: Flavor, port: u16) -> io::Result<Socket> {
    let socket = flavor.socket(Domain::IPV4)?;
    prepare(
        flavor,
        &socket,
        SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)),
    )?;
    Ok(socket)
}

fn prepare(flavor: Flavor, socket: &Socket, addr: SocketAddr) -> io::Result<()> {
    // TCP only. On Linux two UDP sockets that both set it may share one port, so a port another
    // process holds would bind here instead of failing as in use.
    if matches!(flavor, Flavor::Tcp) {
        socket.set_reuse_address(true)?;
    }
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn binds_exactly_the_named_address() {
        let listener = bind_tcp(Ipv4Addr::LOCALHOST.into(), 0).unwrap();
        assert_eq!(listener.local_addr().unwrap().ip(), Ipv4Addr::LOCALHOST);
    }

    #[tokio::test]
    async fn binds_every_interface_for_an_unspecified_address() {
        let listener = bind_tcp(Ipv6Addr::UNSPECIFIED.into(), 0).unwrap();
        assert!(listener.local_addr().unwrap().ip().is_unspecified());
    }

    #[tokio::test]
    async fn keepalive_has_the_kernel_probe_a_socket_once_it_has_been_quiet() {
        let listener = bind_tcp(Ipv4Addr::LOCALHOST.into(), 0).unwrap();
        let socket = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        keepalive(&socket).unwrap();
        let socket = SockRef::from(&socket);
        assert!(socket.keepalive().unwrap());
        assert_eq!(socket.tcp_keepalive_time().unwrap(), KEEPALIVE_IDLE);
    }

    #[tokio::test]
    async fn binds_udp_on_exactly_the_named_address() {
        let socket = bind_udp(Ipv4Addr::LOCALHOST.into(), 0).unwrap();
        assert_eq!(socket.local_addr().unwrap().ip(), Ipv4Addr::LOCALHOST);
    }

    #[tokio::test]
    async fn binds_udp_on_every_interface_for_an_unspecified_address() {
        let socket = bind_udp(Ipv6Addr::UNSPECIFIED.into(), 0).unwrap();
        assert!(socket.local_addr().unwrap().ip().is_unspecified());
    }

    #[tokio::test]
    async fn a_udp_port_somebody_holds_is_refused() {
        let held = bind_udp(Ipv4Addr::LOCALHOST.into(), 0).unwrap();
        let port = held.local_addr().unwrap().port();
        let second = bind_udp(Ipv4Addr::LOCALHOST.into(), port);
        assert_eq!(second.unwrap_err().kind(), io::ErrorKind::AddrInUse);
    }

    /// Falling back to IPv4 here would report the port bound while IPv6 visitors go elsewhere.
    #[tokio::test]
    async fn a_wildcard_udp_bind_is_refused_when_an_ipv6_address_holds_the_port() {
        let held = bind_udp(Ipv6Addr::LOCALHOST.into(), 0).unwrap();
        let port = held.local_addr().unwrap().port();
        let second = bind_udp(Ipv6Addr::UNSPECIFIED.into(), port);
        assert_eq!(second.unwrap_err().kind(), io::ErrorKind::AddrInUse);
    }
}
