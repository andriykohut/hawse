use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};

use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::TcpListener;

/// Listens on every interface: one dual-stack IPv6 socket where the host has IPv6, otherwise IPv4 only.
pub fn bind_tcp(port: u16) -> io::Result<TcpListener> {
    let socket = if let Ok(socket) = Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP)) {
        socket.set_only_v6(false)?;
        prepare(&socket, SocketAddr::from((Ipv6Addr::UNSPECIFIED, port)))?;
        socket
    } else {
        let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
        prepare(&socket, SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)))?;
        socket
    };
    socket.listen(1024)?;
    TcpListener::from_std(socket.into())
}

fn prepare(socket: &Socket, addr: SocketAddr) -> io::Result<()> {
    socket.set_reuse_address(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())
}
