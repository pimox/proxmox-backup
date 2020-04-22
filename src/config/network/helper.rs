use std::collections::HashMap;

use anyhow::{Error, bail, format_err};
use lazy_static::lazy_static;
use nix::sys::socket::{socket, AddressFamily, SockType, SockFlag};
use nix::ioctl_read_bad;
use regex::Regex;

use proxmox::*; // for IP macros

pub static IPV4_REVERSE_MASK: &[&'static str] = &[
    "0.0.0.0",
    "128.0.0.0",
    "192.0.0.0",
    "224.0.0.0",
    "240.0.0.0",
    "248.0.0.0",
    "252.0.0.0",
    "254.0.0.0",
    "255.0.0.0",
    "255.128.0.0",
    "255.192.0.0",
    "255.224.0.0",
    "255.240.0.0",
    "255.248.0.0",
    "255.252.0.0",
    "255.254.0.0",
    "255.255.0.0",
    "255.255.128.0",
    "255.255.192.0",
    "255.255.224.0",
    "255.255.240.0",
    "255.255.248.0",
    "255.255.252.0",
    "255.255.254.0",
    "255.255.255.0",
    "255.255.255.128",
    "255.255.255.192",
    "255.255.255.224",
    "255.255.255.240",
    "255.255.255.248",
    "255.255.255.252",
    "255.255.255.254",
    "255.255.255.255",
];

lazy_static! {
    pub static ref IPV4_MASK_HASH_LOCALNET: HashMap<&'static str, u8> = {
        let mut map = HashMap::new();
        for i in 8..32 {
            map.insert(IPV4_REVERSE_MASK[i], i as u8);
        }
        map
    };
}

pub fn parse_cidr(cidr: &str) -> Result<(String, u8, bool), Error> {

    lazy_static! {
        pub static ref CIDR_V4_REGEX: Regex = Regex::new(
            concat!(r"^(", IPV4RE!(), r")(?:/(\d{1,2}))$")
        ).unwrap();
        pub static ref CIDR_V6_REGEX: Regex = Regex::new(
            concat!(r"^(", IPV6RE!(), r")(?:/(\d{1,3}))$")
        ).unwrap();
    }

    if let Some(caps) = CIDR_V4_REGEX.captures(&cidr) {
        let address = &caps[1];
        let mask = &caps[2];
        let mask = u8::from_str_radix(mask, 10)
            .map(|mask| {
                if !(mask > 0 && mask <= 32) {
                    bail!("IPv4 mask '{}' is out of range (1..32).", mask);
                }
                Ok(mask)
            })?;
        return Ok((address.to_string(), mask.unwrap(), false));
    } else if let Some(caps) = CIDR_V6_REGEX.captures(&cidr) {
        let address = &caps[1];
        let mask = &caps[2];
        let mask = u8::from_str_radix(mask, 10)
            .map(|mask| {
                if !(mask >= 1 && mask <= 128) {
                    bail!("IPv6 mask '{}' is out of range (1..128).", mask);
                }
                Ok(mask)
            })?;
        return Ok((address.to_string(), mask.unwrap(), true));
    } else {
        bail!("invalid address/mask '{}'", cidr);
    }
}

pub fn get_network_interfaces() -> Result<HashMap<String, bool>, Error> {

    const PROC_NET_DEV: &str = "/proc/net/dev";

    #[repr(C)]
    pub struct ifreq {
        ifr_name: [libc::c_uchar; libc::IFNAMSIZ],
        ifru_flags: libc::c_short,
    }

    ioctl_read_bad!(get_interface_flags, libc::SIOCGIFFLAGS, ifreq);

    lazy_static!{
        static ref IFACE_LINE_REGEX: Regex = Regex::new(r"^\s*([^:\s]+):").unwrap();
    }
    let raw = std::fs::read_to_string(PROC_NET_DEV)
        .map_err(|err| format_err!("unable to read {} - {}", PROC_NET_DEV, err))?;

    let lines = raw.lines();

    let sock = socket(AddressFamily::Inet, SockType::Datagram, SockFlag::empty(), None)
        .or_else(|_| socket(AddressFamily::Inet6, SockType::Datagram, SockFlag::empty(), None))?;

    let mut interface_list = HashMap::new();

    for line in lines {
        if let Some(cap) = IFACE_LINE_REGEX.captures(line) {
            let ifname = &cap[1];

            let mut req = ifreq { ifr_name: *b"0000000000000000", ifru_flags: 0 };
            for (i, b) in std::ffi::CString::new(ifname)?.as_bytes_with_nul().iter().enumerate() {
                if i < (libc::IFNAMSIZ-1) { req.ifr_name[i] = *b as libc::c_uchar; }
            }
            let res = unsafe { get_interface_flags(sock, &mut req)? };
            if res != 0 {
                bail!("ioctl get_interface_flags for '{}' failed ({})", ifname, res);
            }
            let is_up = (req.ifru_flags & (libc::IFF_UP as libc::c_short)) != 0;
            interface_list.insert(ifname.to_string(), is_up);
        }
    }

    Ok(interface_list)
}