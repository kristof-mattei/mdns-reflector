use std::ffi::CString;
use std::mem::MaybeUninit;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::os::fd::AsRawFd as _;
use std::ptr::addr_of;

use color_eyre::Section as _;
use color_eyre::eyre::{self, Context as _};
use libc::{IFNAMSIZ, IP_PKTINFO, SIOCGIFADDR, SIOCGIFNETMASK, SOL_IP, ifreq, ioctl, sockaddr_in};
use socket2::{Domain, Type};
use tokio::net::UdpSocket;
use tracing::{Level, event, instrument};

use crate::{MDNS_ADDR, MDNS_PORT, unix};

#[derive(Debug)]
pub struct InterfaceSocket {
    /// interface name.
    pub name: String,
    /// socket.
    pub socket: UdpSocket,
    /// interface address.
    pub address: Ipv4Addr,
    /// interface mask.
    pub mask: Ipv4Addr,
    /// interface network (computed).
    pub network: Ipv4Addr,
}

#[instrument]
pub fn create_recv_sock() -> Result<UdpSocket, eyre::Error> {
    let socket = socket2::Socket::new(Domain::IPV4, Type::DGRAM, None /* IPPROTO_IP */)
        .wrap_err("recv socket()")?;

    socket
        .set_nonblocking(true)
        .wrap_err("recv setsockopt(SO_NONBLOCK)")?;

    socket
        .set_reuse_address(true)
        .wrap_err("recv setsockopt(SO_REUSEADDR)")?;

    // bind to an address
    let server_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, MDNS_PORT);
    socket.bind(&server_addr.into()).wrap_err("recv bind()")?;

    // enable loopback in case someone else needs the data
    socket
        .set_multicast_loop_v4(true)
        .wrap_err("recv setsockopt(IP_MULTICAST_LOOP)")?;

    // do we support any OS that doesn't have `IP_PKTINFO?`
    // SAFETY: libc call
    unsafe {
        unix::setsockopt(&socket, SOL_IP, IP_PKTINFO, true)
            .wrap_err("recv setsockopt(IP_PKTINFO)")?;
    }

    Ok(UdpSocket::from_std(socket.into())?)
}

#[instrument(skip(recv_sock), fields(recv_sock = %recv_sock.local_addr().unwrap(), ifname = %ifname))]
pub fn create_send_sock(
    recv_sock: &UdpSocket,
    ifname: String,
) -> Result<InterfaceSocket, eyre::Error> {
    let (interface_address, interface_mask) = get_interface_details(&ifname)?;

    let socket = socket2::Socket::new(
        Domain::IPV4,
        Type::DGRAM,
        // IPPROTO_IP
        None,
    )
    .wrap_err_with(|| format!("send socket() failed on {}", &ifname))?;

    // compute network (address & mask)
    let interface_network = interface_address & interface_mask;

    socket
        .set_nonblocking(true)
        .wrap_err_with(|| format!("send setsockopt(SO_NONBLOCK) failed on {}", &ifname))?;

    socket
        .set_reuse_address(true)
        .wrap_err_with(|| format!("send setsockopt(SO_REUSEADDR) failed on {}", &ifname))?;

    // do we support any OS that doesn't have `SO_BINDTODEVICE?`
    socket
        .bind_device(Some(ifname.as_bytes()))
        .wrap_err_with(|| format!("send setsockopt(SO_BINDTODEVICE) failed on {}", &ifname))?;

    // bind to an address
    let server_addr = SocketAddrV4::new(interface_address, MDNS_PORT);

    socket
        .bind(&server_addr.into())
        .wrap_err_with(|| format!("send bind() failed on {}", &ifname))?;

    // add membership to receiving socket
    recv_sock
        .join_multicast_v4(MDNS_ADDR, interface_address)
        .wrap_err_with(|| format!("recv setsockopt(IP_ADD_MEMBERSHIP) failed on {}", &ifname))?;

    // enable loopback in case someone else needs the data
    socket
        .set_multicast_loop_v4(true)
        .wrap_err_with(|| format!("send setsockopt()SO_BINDTODEVICE failed on {}", &ifname))?;

    let interface_socket = InterfaceSocket {
        name: ifname,
        socket: UdpSocket::from_std(socket.into())?,
        address: interface_address,
        mask: interface_mask,
        network: interface_network,
    };

    event!(
        Level::INFO,
        dev = %interface_socket.name,
        addr = %interface_socket.address,
        mask = %interface_socket.mask,
        net = %interface_socket.network
    );

    Ok(interface_socket)
}

fn get_interface_details(ifname: &str) -> Result<(Ipv4Addr, Ipv4Addr), eyre::Report> {
    let socket = socket2::Socket::new(
        Domain::IPV4,
        Type::DGRAM,
        // IPPROTO_IP
        None,
    )
    .wrap_err("send socket()")?;

    // SAFETY: all zeroes is valid for `ifreq`
    let mut ifr = unsafe { MaybeUninit::<ifreq>::zeroed().assume_init() };

    let c_ifname = CString::new(ifname)
        .map_err(|err| eyre::Error::new(err).with_note(|| "Failed to convert ifname to CString"))?;

    let len = std::cmp::min(
        c_ifname.as_bytes_with_nul().len(),
        IFNAMSIZ - 1, // leave one to ensure there's a null terminator
    );

    // SAFETY: We can write up to IFNAMSIZ in `ifr.name`.
    // SAFETY: We don't write the last byte to ensure `ifr.ifr_name` is '\0' terminated.
    unsafe {
        std::ptr::copy_nonoverlapping(c_ifname.as_ptr(), ifr.ifr_name.as_mut_ptr(), len);
    };

    let sockaddr_in = addr_of!(ifr.ifr_ifru).cast::<sockaddr_in>();

    #[cfg(target_env = "musl")]
    let siocgifnetmask: i32 = SIOCGIFNETMASK.try_into().unwrap();
    #[cfg(not(target_env = "musl"))]
    let siocgifnetmask = SIOCGIFNETMASK;

    // get netmask
    let interface_mask = {
        // SAFETY: libc call
        if 0 == unsafe { ioctl(socket.as_raw_fd(), siocgifnetmask, &ifr) } {
            // SAFETY: we tested ioctl's return value
            let mask_in_network_order = unsafe { (*sockaddr_in).sin_addr.s_addr };

            Ipv4Addr::from(u32::from_be(mask_in_network_order))
        } else {
            Ipv4Addr::UNSPECIFIED
        }
    };

    #[cfg(target_env = "musl")]
    let siocgifaddr: i32 = SIOCGIFADDR.try_into().unwrap();
    #[cfg(not(target_env = "musl"))]
    let siocgifaddr = SIOCGIFADDR;

    // .. and interface address
    let interface_address = {
        // SAFETY: libc call
        if 0 == unsafe { ioctl(socket.as_raw_fd(), siocgifaddr, &ifr) } {
            // SAFETY: we tested ioctl's return value
            let addr_in_network_order = unsafe { (*sockaddr_in).sin_addr.s_addr };

            Ipv4Addr::from(u32::from_be(addr_in_network_order))
        } else {
            Ipv4Addr::UNSPECIFIED
        }
    };

    Ok((interface_address, interface_mask))
}
