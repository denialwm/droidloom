//! DHCP for the cell's Wi-Fi link.
//!
//! Android asks for an address on `wlan0` as it would from any access point;
//! this answers with the cell's one fixed lease. It reads and writes frames on
//! the host end of the pair through a packet socket, below netfilter, so the
//! host firewall's input chain stays closed to the cell.

use std::ffi::CString;
use std::io;
use std::net::Ipv4Addr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::thread::JoinHandle;

// libc's constants are C ints; these are the widths the headers carry.
const ETHERTYPE_IPV4: u16 = 0x0800;
const FAMILY_PACKET: u16 = 17;
const PROTOCOL_UDP: u8 = 17;
const SERVER_PORT: u16 = 67;
const CLIENT_PORT: u16 = 68;
const MAGIC_COOKIE: [u8; 4] = [99, 130, 83, 99];
// op .. file, then the magic cookie.
const BOOTP_FIXED_LEN: usize = 236;
// RFC 1542: some clients drop replies shorter than a BOOTP packet.
const BOOTP_MIN_LEN: usize = 300;
const BROADCAST_FLAG: u16 = 0x8000;
const LEASE_SECONDS: u32 = 24 * 60 * 60;

const DISCOVER: u8 = 1;
const OFFER: u8 = 2;
const REQUEST: u8 = 3;
const ACK: u8 = 5;
const NAK: u8 = 6;

const OPTION_PAD: u8 = 0;
const OPTION_SUBNET_MASK: u8 = 1;
const OPTION_ROUTER: u8 = 3;
const OPTION_DNS: u8 = 6;
const OPTION_REQUESTED_ADDRESS: u8 = 50;
const OPTION_LEASE_TIME: u8 = 51;
const OPTION_MESSAGE_TYPE: u8 = 53;
const OPTION_SERVER_ID: u8 = 54;
const OPTION_RENEWAL_TIME: u8 = 58;
const OPTION_REBINDING_TIME: u8 = 59;
const OPTION_END: u8 = 255;

/// The one address the cell gets, and how it reaches the host and resolvers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Lease {
    pub(crate) server: Ipv4Addr,
    pub(crate) client: Ipv4Addr,
    pub(crate) prefix_len: u8,
    pub(crate) dns: [Ipv4Addr; 2],
}

/// Answers DHCP on one host interface until dropped.
pub(crate) struct DhcpResponder {
    stop: OwnedFd,
    thread: Option<JoinHandle<()>>,
}

impl DhcpResponder {
    pub(crate) fn start(interface: &str, lease: Lease) -> io::Result<Self> {
        let name = CString::new(interface).map_err(io::Error::other)?;
        // SAFETY: a NUL-terminated interface name.
        let index = unsafe { libc::if_nametoindex(name.as_ptr()) };
        if index == 0 {
            return Err(io::Error::last_os_error());
        }
        let index = i32::try_from(index).map_err(io::Error::other)?;
        let socket = packet_socket(index)?;
        // SAFETY: eventfd takes scalar arguments and returns a fresh descriptor.
        let stop = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if stop < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a fresh descriptor returned by the kernel, owned from here on.
        let stop = unsafe { OwnedFd::from_raw_fd(stop) };
        let wake = stop.try_clone()?;
        let thread = std::thread::Builder::new()
            .name("droidloom-dhcp".into())
            .spawn(move || serve(&socket, &wake, index, lease))?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for DhcpResponder {
    fn drop(&mut self) {
        let value = 1_u64;
        // SAFETY: the eventfd is owned by self and the buffer is 8 bytes.
        unsafe { libc::write(self.stop.as_raw_fd(), (&raw const value).cast(), 8) };
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A datagram packet socket for IPv4 on one interface: the kernel adds and
/// strips the Ethernet header.
fn packet_socket(index: i32) -> io::Result<OwnedFd> {
    let protocol = ETHERTYPE_IPV4.to_be();
    // SAFETY: socket takes scalar arguments and returns a fresh descriptor.
    let fd = unsafe {
        libc::socket(
            libc::AF_PACKET,
            libc::SOCK_DGRAM | libc::SOCK_CLOEXEC,
            i32::from(protocol),
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a fresh descriptor returned by the kernel, owned from here on.
    let socket = unsafe { OwnedFd::from_raw_fd(fd) };
    let address = link_address(index, [0; 6]);
    // SAFETY: the address is a fully initialized sockaddr_ll of the given size.
    let bound = unsafe {
        libc::bind(
            socket.as_raw_fd(),
            (&raw const address).cast(),
            socklen::<libc::sockaddr_ll>(),
        )
    };
    if bound < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(socket)
}

fn link_address(index: i32, mac: [u8; 6]) -> libc::sockaddr_ll {
    // SAFETY: zero is a valid initial representation for this C struct.
    let mut address: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
    address.sll_family = FAMILY_PACKET;
    address.sll_protocol = ETHERTYPE_IPV4.to_be();
    address.sll_ifindex = index;
    address.sll_halen = 6;
    address.sll_addr[..6].copy_from_slice(&mac);
    address
}

fn socklen<T>() -> libc::socklen_t {
    libc::socklen_t::try_from(size_of::<T>()).expect("socket address size fits socklen_t")
}

fn serve(socket: &OwnedFd, stop: &OwnedFd, index: i32, lease: Lease) {
    let mut buffer = [0_u8; 1500];
    loop {
        let mut fds = [
            libc::pollfd {
                fd: socket.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: stop.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: both pollfd records are initialized and live for the call.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
        if ready < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            eprintln!(
                "droidloom-supervisor: DHCP stopped: {}",
                io::Error::last_os_error()
            );
            return;
        }
        if fds[1].revents != 0 {
            return;
        }
        if fds[0].revents & libc::POLLIN == 0 {
            // The interface went away under the socket.
            return;
        }
        // SAFETY: zero is a valid initial representation for this C struct.
        let mut from: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
        let mut from_len = socklen::<libc::sockaddr_ll>();
        // SAFETY: the buffer and address are writable for their stated sizes.
        let received = unsafe {
            libc::recvfrom(
                socket.as_raw_fd(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                0,
                (&raw mut from).cast(),
                &raw mut from_len,
            )
        };
        let Ok(received) = usize::try_from(received) else {
            continue;
        };
        // The socket also sees the replies it sends.
        if from.sll_pkttype == libc::PACKET_OUTGOING {
            continue;
        }
        let Some(request) = dhcp_payload(&buffer[..received]) else {
            continue;
        };
        let Some(reply) = reply(request, lease) else {
            continue;
        };
        let packet = udp_packet(lease.server, reply.destination, &reply.payload);
        let address = link_address(index, reply.mac);
        // SAFETY: the packet and address are initialized for their stated sizes.
        let sent = unsafe {
            libc::sendto(
                socket.as_raw_fd(),
                packet.as_ptr().cast(),
                packet.len(),
                0,
                (&raw const address).cast(),
                socklen::<libc::sockaddr_ll>(),
            )
        };
        if sent < 0 {
            eprintln!(
                "droidloom-supervisor: DHCP reply not sent: {}",
                io::Error::last_os_error()
            );
        }
    }
}

/// The BOOTP payload of an IPv4 UDP datagram to the DHCP server port.
fn dhcp_payload(packet: &[u8]) -> Option<&[u8]> {
    let header_len = usize::from(packet.first()? & 0x0f) * 4;
    if packet.first()? >> 4 != 4 || header_len < 20 || *packet.get(9)? != PROTOCOL_UDP {
        return None;
    }
    let total_len = usize::from(u16::from_be_bytes([*packet.get(2)?, *packet.get(3)?]));
    let udp = packet.get(header_len..total_len.min(packet.len()))?;
    if udp.len() < 8 || u16::from_be_bytes([udp[2], udp[3]]) != SERVER_PORT {
        return None;
    }
    let udp_len = usize::from(u16::from_be_bytes([udp[4], udp[5]]));
    udp.get(8..udp_len.min(udp.len()))
}

#[derive(Debug, Eq, PartialEq)]
struct Reply {
    destination: Ipv4Addr,
    mac: [u8; 6],
    payload: Vec<u8>,
}

/// The server's answer to one client message, as RFC 2131 describes it for a
/// server with a single address to give.
fn reply(request: &[u8], lease: Lease) -> Option<Reply> {
    if request.len() < BOOTP_FIXED_LEN + 4
        || request[0] != 1
        || request[1] != 1
        || request[2] != 6
        || request[BOOTP_FIXED_LEN..BOOTP_FIXED_LEN + 4] != MAGIC_COOKIE
    {
        return None;
    }
    let options = parse_options(&request[BOOTP_FIXED_LEN + 4..]);
    let message_type = options.message_type?;
    let ciaddr = Ipv4Addr::new(request[12], request[13], request[14], request[15]);
    let response = match message_type {
        DISCOVER => OFFER,
        REQUEST => {
            // A client choosing another server's offer; there is none here,
            // but stay quiet as the protocol asks.
            if options
                .server_id
                .is_some_and(|server| server != lease.server)
            {
                return None;
            }
            let wanted = options.requested_address.unwrap_or(ciaddr);
            if wanted == lease.client { ACK } else { NAK }
        }
        _ => return None,
    };

    let mut payload = vec![0_u8; BOOTP_FIXED_LEN];
    payload[0] = 2;
    payload[1] = 1;
    payload[2] = 6;
    // xid, then flags; secs stays zero.
    payload[4..8].copy_from_slice(&request[4..8]);
    payload[10..12].copy_from_slice(&request[10..12]);
    if response != NAK {
        payload[12..16].copy_from_slice(&request[12..16]);
        payload[16..20].copy_from_slice(&lease.client.octets());
    }
    // giaddr and chaddr.
    payload[24..44].copy_from_slice(&request[24..44]);
    payload.extend_from_slice(&MAGIC_COOKIE);
    payload.extend_from_slice(&[OPTION_MESSAGE_TYPE, 1, response]);
    payload.extend_from_slice(&[OPTION_SERVER_ID, 4]);
    payload.extend_from_slice(&lease.server.octets());
    if response != NAK {
        for (option, seconds) in [
            (OPTION_LEASE_TIME, LEASE_SECONDS),
            (OPTION_RENEWAL_TIME, LEASE_SECONDS / 2),
            (OPTION_REBINDING_TIME, LEASE_SECONDS / 8 * 7),
        ] {
            payload.extend_from_slice(&[option, 4]);
            payload.extend_from_slice(&seconds.to_be_bytes());
        }
        let mask = u32::MAX << (32 - u32::from(lease.prefix_len));
        payload.extend_from_slice(&[OPTION_SUBNET_MASK, 4]);
        payload.extend_from_slice(&mask.to_be_bytes());
        payload.extend_from_slice(&[OPTION_ROUTER, 4]);
        payload.extend_from_slice(&lease.server.octets());
        payload.extend_from_slice(&[OPTION_DNS, 8]);
        for server in lease.dns {
            payload.extend_from_slice(&server.octets());
        }
    }
    payload.push(OPTION_END);
    payload.resize(payload.len().max(BOOTP_MIN_LEN), OPTION_PAD);

    let flags = u16::from_be_bytes([request[10], request[11]]);
    let mut client_mac = [0_u8; 6];
    client_mac.copy_from_slice(&request[28..34]);
    let (destination, mac) = if response == NAK || flags & BROADCAST_FLAG != 0 {
        (Ipv4Addr::BROADCAST, [0xff; 6])
    } else if !ciaddr.is_unspecified() {
        (ciaddr, client_mac)
    } else {
        (lease.client, client_mac)
    };
    Some(Reply {
        destination,
        mac,
        payload,
    })
}

#[derive(Debug, Default)]
struct Options {
    message_type: Option<u8>,
    requested_address: Option<Ipv4Addr>,
    server_id: Option<Ipv4Addr>,
}

fn parse_options(mut bytes: &[u8]) -> Options {
    let mut options = Options::default();
    while let Some((&code, rest)) = bytes.split_first() {
        match code {
            OPTION_PAD => {
                bytes = rest;
                continue;
            }
            OPTION_END => break,
            _ => {}
        }
        let Some((&len, rest)) = rest.split_first() else {
            break;
        };
        let Some(value) = rest.get(..usize::from(len)) else {
            break;
        };
        let address = <[u8; 4]>::try_from(value).ok().map(Ipv4Addr::from);
        match code {
            OPTION_MESSAGE_TYPE => options.message_type = value.first().copied(),
            OPTION_REQUESTED_ADDRESS => options.requested_address = address,
            OPTION_SERVER_ID => options.server_id = address,
            _ => {}
        }
        bytes = &rest[usize::from(len)..];
    }
    options
}

/// An IPv4 UDP datagram from the server port to the client port.
fn udp_packet(source: Ipv4Addr, destination: Ipv4Addr, payload: &[u8]) -> Vec<u8> {
    let udp_len = u16::try_from(8 + payload.len()).expect("DHCP reply fits one datagram");
    let total_len = udp_len + 20;
    let mut packet = Vec::with_capacity(usize::from(total_len));
    packet.extend_from_slice(&[0x45, 0]);
    packet.extend_from_slice(&total_len.to_be_bytes());
    // Identification, flags and fragment offset, TTL, UDP, checksum.
    packet.extend_from_slice(&[0, 0, 0, 0, 64, PROTOCOL_UDP, 0, 0]);
    packet.extend_from_slice(&source.octets());
    packet.extend_from_slice(&destination.octets());
    let header_checksum = checksum(&[&packet[..20]]);
    packet[10..12].copy_from_slice(&header_checksum.to_be_bytes());

    let mut udp = Vec::with_capacity(usize::from(udp_len));
    udp.extend_from_slice(&SERVER_PORT.to_be_bytes());
    udp.extend_from_slice(&CLIENT_PORT.to_be_bytes());
    udp.extend_from_slice(&udp_len.to_be_bytes());
    udp.extend_from_slice(&[0, 0]);
    udp.extend_from_slice(payload);
    let mut pseudo = Vec::with_capacity(12);
    pseudo.extend_from_slice(&source.octets());
    pseudo.extend_from_slice(&destination.octets());
    pseudo.extend_from_slice(&[0, PROTOCOL_UDP]);
    pseudo.extend_from_slice(&udp_len.to_be_bytes());
    let udp_checksum = match checksum(&[&pseudo, &udp]) {
        // Zero means "no checksum" in UDP over IPv4.
        0 => 0xffff,
        sum => sum,
    };
    udp[6..8].copy_from_slice(&udp_checksum.to_be_bytes());
    packet.extend_from_slice(&udp);
    packet
}

/// The Internet checksum over the concatenation of `parts`, each even-sized
/// except possibly the last.
fn checksum(parts: &[&[u8]]) -> u16 {
    let mut sum = 0_u32;
    for part in parts {
        for pair in part.chunks(2) {
            let high = u32::from(pair[0]) << 8;
            sum += high | pair.get(1).copied().map_or(0, u32::from);
        }
    }
    while sum > 0xffff {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !u16::try_from(sum).expect("folded checksum fits 16 bits")
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEASE: Lease = Lease {
        server: Ipv4Addr::new(10, 177, 0, 1),
        client: Ipv4Addr::new(10, 177, 0, 2),
        prefix_len: 30,
        dns: [Ipv4Addr::new(1, 1, 1, 1), Ipv4Addr::new(8, 8, 8, 8)],
    };
    const MAC: [u8; 6] = [0x02, 0x64, 0x6c, 0, 0, 2];

    fn request(message_type: u8, extra: &[u8]) -> Vec<u8> {
        let mut packet = vec![0_u8; BOOTP_FIXED_LEN];
        packet[0] = 1;
        packet[1] = 1;
        packet[2] = 6;
        packet[4..8].copy_from_slice(&[1, 2, 3, 4]);
        packet[28..34].copy_from_slice(&MAC);
        packet.extend_from_slice(&MAGIC_COOKIE);
        packet.extend_from_slice(&[OPTION_MESSAGE_TYPE, 1, message_type]);
        packet.extend_from_slice(extra);
        packet.push(OPTION_END);
        packet
    }

    fn option(reply: &Reply, code: u8) -> Option<Vec<u8>> {
        let mut bytes = &reply.payload[BOOTP_FIXED_LEN + 4..];
        while let [found, len, rest @ ..] = bytes {
            if *found == OPTION_END {
                return None;
            }
            if *found == code {
                return Some(rest[..usize::from(*len)].to_vec());
            }
            bytes = &rest[usize::from(*len)..];
        }
        None
    }

    #[test]
    fn header_constants_match_libc() {
        assert_eq!(i32::from(ETHERTYPE_IPV4), libc::ETH_P_IP);
        assert_eq!(i32::from(FAMILY_PACKET), libc::AF_PACKET);
        assert_eq!(i32::from(PROTOCOL_UDP), libc::IPPROTO_UDP);
    }

    #[test]
    fn discover_gets_the_cell_lease() {
        let offer = reply(&request(DISCOVER, &[]), LEASE).expect("offer");
        assert_eq!(option(&offer, OPTION_MESSAGE_TYPE), Some(vec![OFFER]));
        assert_eq!(offer.payload[4..8], [1, 2, 3, 4]);
        assert_eq!(offer.payload[16..20], [10, 177, 0, 2]);
        assert_eq!(offer.payload[28..34], MAC);
        assert_eq!(
            option(&offer, OPTION_SUBNET_MASK),
            Some(vec![255, 255, 255, 252])
        );
        assert_eq!(option(&offer, OPTION_ROUTER), Some(vec![10, 177, 0, 1]));
        assert_eq!(
            option(&offer, OPTION_DNS),
            Some(vec![1, 1, 1, 1, 8, 8, 8, 8])
        );
        assert_eq!(option(&offer, OPTION_SERVER_ID), Some(vec![10, 177, 0, 1]));
        assert!(offer.payload.len() >= BOOTP_MIN_LEN);
        assert_eq!((offer.destination, offer.mac), (LEASE.client, MAC));
    }

    #[test]
    fn request_for_the_lease_is_acknowledged_and_others_refused() {
        let wanted = [
            OPTION_REQUESTED_ADDRESS,
            4,
            10,
            177,
            0,
            2,
            OPTION_SERVER_ID,
            4,
            10,
            177,
            0,
            1,
        ];
        let ack = reply(&request(REQUEST, &wanted), LEASE).expect("ack");
        assert_eq!(option(&ack, OPTION_MESSAGE_TYPE), Some(vec![ACK]));
        assert_eq!(
            option(&ack, OPTION_LEASE_TIME),
            Some(LEASE_SECONDS.to_be_bytes().to_vec())
        );

        let other = [OPTION_REQUESTED_ADDRESS, 4, 192, 168, 1, 50];
        let nak = reply(&request(REQUEST, &other), LEASE).expect("nak");
        assert_eq!(option(&nak, OPTION_MESSAGE_TYPE), Some(vec![NAK]));
        assert_eq!(nak.payload[16..20], [0, 0, 0, 0]);
        assert_eq!((nak.destination, nak.mac), (Ipv4Addr::BROADCAST, [0xff; 6]));

        let elsewhere = [OPTION_SERVER_ID, 4, 192, 168, 1, 1];
        assert_eq!(reply(&request(REQUEST, &elsewhere), LEASE), None);
    }

    #[test]
    fn renewal_from_a_bound_client_is_unicast() {
        let mut renew = request(REQUEST, &[]);
        renew[12..16].copy_from_slice(&[10, 177, 0, 2]);
        let ack = reply(&renew, LEASE).expect("ack");
        assert_eq!(option(&ack, OPTION_MESSAGE_TYPE), Some(vec![ACK]));
        assert_eq!((ack.destination, ack.mac), (LEASE.client, MAC));
    }

    #[test]
    fn broadcast_flag_is_honoured() {
        let mut discover = request(DISCOVER, &[]);
        discover[10] = 0x80;
        let offer = reply(&discover, LEASE).expect("offer");
        assert_eq!(
            (offer.destination, offer.mac),
            (Ipv4Addr::BROADCAST, [0xff; 6])
        );
        assert_eq!(offer.payload[10..12], [0x80, 0]);
    }

    #[test]
    fn replies_round_trip_through_the_ip_and_udp_headers() {
        let payload = request(DISCOVER, &[]);
        let packet = udp_packet(LEASE.client, LEASE.server, &payload);
        assert_eq!(checksum(&[&packet[..20]]), 0);
        // A reply addressed back to the server port parses as a request again.
        let mut to_server = packet.clone();
        to_server[22..24].copy_from_slice(&SERVER_PORT.to_be_bytes());
        assert_eq!(dhcp_payload(&to_server), Some(&payload[..]));
        assert_eq!(dhcp_payload(&packet), None);
    }

    #[test]
    fn malformed_messages_are_ignored() {
        assert_eq!(reply(&[], LEASE), None);
        let mut wrong_cookie = request(DISCOVER, &[]);
        wrong_cookie[BOOTP_FIXED_LEN] = 0;
        assert_eq!(reply(&wrong_cookie, LEASE), None);
        let mut truncated = request(DISCOVER, &[]);
        truncated.truncate(BOOTP_FIXED_LEN + 6);
        assert_eq!(reply(&truncated, LEASE), None);
        assert_eq!(reply(&request(7, &[]), LEASE), None);
    }
}
