use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::TcpListener;

/// An unspecified IPv6 `bind` answers on every interface; any other address answers only there.
pub fn bind_tcp(bind: IpAddr, port: u16) -> io::Result<TcpListener> {
    let socket = match bind {
        IpAddr::V6(addr) if addr.is_unspecified() => every_interface(port)?,
        addr => {
            let domain = if addr.is_ipv4() {
                Domain::IPV4
            } else {
                Domain::IPV6
            };
            let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
            prepare(&socket, SocketAddr::new(addr, port))?;
            socket
        }
    };
    socket.listen(1024)?;
    TcpListener::from_std(socket.into())
}

/// One dual-stack IPv6 socket, falling back to IPv4 on a host without IPv6.
fn every_interface(port: u16) -> io::Result<Socket> {
    match dual_stack(port) {
        Ok(socket) => Ok(socket),
        // A host can hand out an IPv6 socket and still refuse `::`, so the
        // fallback keys off the whole attempt rather than the constructor.
        Err(v6) => ipv4_only(port).map_err(|_| v6),
    }
}

fn dual_stack(port: u16) -> io::Result<Socket> {
    let socket = Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP))?;
    socket.set_only_v6(false)?;
    prepare(&socket, SocketAddr::from((Ipv6Addr::UNSPECIFIED, port)))?;
    Ok(socket)
}

fn ipv4_only(port: u16) -> io::Result<Socket> {
    let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
    prepare(&socket, SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)))?;
    Ok(socket)
}

fn prepare(socket: &Socket, addr: SocketAddr) -> io::Result<()> {
    socket.set_reuse_address(true)?;
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
}
