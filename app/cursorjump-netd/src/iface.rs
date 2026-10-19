//! Reading and setting a network interface's up/down state with the
//! `SIOCGIFFLAGS`/`SIOCSIFFLAGS` ioctls (no `ifconfig` process).

use std::ffi::c_short;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

/// An interface that can be taken down and brought back up.
pub trait Iface {
    fn is_up(&mut self) -> io::Result<bool>;
    fn set_up(&mut self, up: bool) -> io::Result<()>;
}

/// `flags` with `IFF_UP` set (`up`) or cleared, every other flag kept.
pub fn flags_with_up(flags: c_short, up: bool) -> c_short {
    if up { flags | IFF_UP } else { flags & !IFF_UP }
}

/// Whether `flags` has `IFF_UP`.
pub fn flags_are_up(flags: c_short) -> bool {
    flags & IFF_UP != 0
}

/// `IFF_UP` from `<net/if.h>`, the width of `ifr_flags`.
const IFF_UP: c_short = libc::IFF_UP as c_short;

/// `_IOC(inout, group, num, len)` from `<sys/ioccom.h>`.
const fn ioc(inout: u32, group: u8, num: u8, len: usize) -> libc::c_ulong {
    (inout | ((len as u32 & 0x1fff) << 16) | ((group as u32) << 8) | num as u32) as libc::c_ulong
}
const IOC_IN: u32 = 0x8000_0000;
const IOC_OUT: u32 = 0x4000_0000;
const IFREQ_LEN: usize = std::mem::size_of::<libc::ifreq>();
/// `_IOWR('i', 17, struct ifreq)`.
const SIOCGIFFLAGS: libc::c_ulong = ioc(IOC_IN | IOC_OUT, b'i', 17, IFREQ_LEN);
/// `_IOW('i', 16, struct ifreq)`.
const SIOCSIFFLAGS: libc::c_ulong = ioc(IOC_IN, b'i', 16, IFREQ_LEN);

/// A real interface, by name (`awdl0`). Setting its flags needs root.
pub struct Interface {
    name: [libc::c_char; libc::IFNAMSIZ],
    sock: OwnedFd,
}

impl Interface {
    pub fn open(name: &str) -> io::Result<Interface> {
        if name.is_empty() || name.len() >= libc::IFNAMSIZ || name.contains('\0') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("bad interface name {name:?}"),
            ));
        }
        let mut ifname = [0 as libc::c_char; libc::IFNAMSIZ];
        for (dst, src) in ifname.iter_mut().zip(name.bytes()) {
            *dst = src as libc::c_char;
        }
        // SAFETY: plain socket(2) call; the result is checked before use.
        let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` is a fresh descriptor owned by nothing else.
        let sock = unsafe { OwnedFd::from_raw_fd(fd) };
        Ok(Interface { name: ifname, sock })
    }

    fn request(&self) -> libc::ifreq {
        // SAFETY: ifreq is plain data; all zeroes is a valid value.
        let mut ifr: libc::ifreq = unsafe { std::mem::zeroed() };
        ifr.ifr_name = self.name;
        ifr
    }

    fn flags(&self) -> io::Result<c_short> {
        let mut ifr = self.request();
        // SAFETY: SIOCGIFFLAGS reads the name from and writes the flags into
        // `ifr`, a live ifreq of the size encoded in the request.
        if unsafe { libc::ioctl(self.sock.as_raw_fd(), SIOCGIFFLAGS, &mut ifr) } < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: SIOCGIFFLAGS filled in the flags member.
        Ok(unsafe { ifr.ifr_ifru.ifru_flags })
    }
}

impl Iface for Interface {
    fn is_up(&mut self) -> io::Result<bool> {
        self.flags().map(flags_are_up)
    }

    fn set_up(&mut self, up: bool) -> io::Result<()> {
        let mut ifr = self.request();
        ifr.ifr_ifru.ifru_flags = flags_with_up(self.flags()?, up);
        // SAFETY: SIOCSIFFLAGS reads the name and flags from `ifr`, a live
        // ifreq of the size encoded in the request.
        if unsafe { libc::ioctl(self.sock.as_raw_fd(), SIOCSIFFLAGS, &mut ifr) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IFF_RUNNING: c_short = libc::IFF_RUNNING as c_short;
    const IFF_BROADCAST: c_short = libc::IFF_BROADCAST as c_short;

    #[test]
    fn up_flag_math_touches_only_iff_up() {
        let others = IFF_RUNNING | IFF_BROADCAST;
        assert_eq!(flags_with_up(others | IFF_UP, false), others);
        assert_eq!(flags_with_up(others, true), others | IFF_UP);
        assert_eq!(flags_with_up(others | IFF_UP, true), others | IFF_UP);
        assert_eq!(flags_with_up(others, false), others);
        // The sign bit (IFF_* fill all 16 bits) survives both ways.
        let high = c_short::MIN;
        assert_eq!(flags_with_up(high, true), high | IFF_UP);
        assert_eq!(flags_with_up(high | IFF_UP, false), high);
    }

    #[test]
    fn up_is_read_from_iff_up_alone() {
        assert!(flags_are_up(IFF_UP));
        assert!(flags_are_up(IFF_UP | IFF_RUNNING));
        assert!(!flags_are_up(IFF_RUNNING | IFF_BROADCAST));
        assert!(!flags_are_up(0));
    }

    #[test]
    fn ioctl_numbers_match_the_sdk() {
        // <sys/sockio.h> on macOS: struct ifreq is 32 bytes.
        assert_eq!(IFREQ_LEN, 32);
        assert_eq!(SIOCGIFFLAGS, 0xc020_6911);
        assert_eq!(SIOCSIFFLAGS, 0x8020_6910);
    }

    #[test]
    fn bad_names_are_refused() {
        assert!(Interface::open("").is_err());
        assert!(Interface::open("a-name-that-is-too-long").is_err());
    }

    /// Read-only and needs no root: the loopback interface is up.
    #[test]
    fn reads_the_loopback_flags() {
        let mut lo = Interface::open("lo0").unwrap();
        assert!(lo.is_up().unwrap());
    }

    /// Really takes awdl0 down and back up: needs root, and disturbs AirDrop
    /// for a moment. Run by hand only:
    /// `sudo cargo test -p cursorjump-netd -- --ignored toggles_awdl0`.
    #[test]
    #[ignore = "needs root and toggles the real awdl0"]
    fn toggles_awdl0() {
        let mut awdl = Interface::open("awdl0").unwrap();
        let was_up = awdl.is_up().unwrap();
        awdl.set_up(false).unwrap();
        assert!(!awdl.is_up().unwrap());
        awdl.set_up(true).unwrap();
        assert!(awdl.is_up().unwrap());
        awdl.set_up(was_up).unwrap();
    }
}
