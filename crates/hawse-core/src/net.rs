use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::{TcpListener, UdpSocket};

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
}
