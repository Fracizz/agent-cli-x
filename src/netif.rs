//! 本机网卡 IPv4 枚举：为配对命令挑选其他机器最可能访问到的地址。
//! 系统路由选出的出口常被 VPN / 代理的 TUN 网卡（sing-box、Clash、WireGuard 等）接管，
//! 直接用它会给出调用方访问不到的地址，所以按网卡类型和名称打分后再选。

use std::net::Ipv4Addr;

/// 一块网卡上的一个 IPv4 地址。
#[derive(Debug, Clone)]
pub struct Iface {
    pub name: String,
    pub ip: Ipv4Addr,
    /// 有默认网关（真实接入局域网的网卡通常有）
    pub gateway: bool,
    /// 系统报告的隧道 / 虚拟网卡类型（Windows IfType）
    pub tunnel: bool,
}

/// VPN / 代理 / 容器 / 虚拟机内部网卡的常见名称片段（小写比较）。
const VIRTUAL_HINTS: &[&str] = &[
    "tun",
    "tap",
    "wintun",
    "wireguard",
    "wg",
    "singbox",
    "sing-box",
    "clash",
    "mihomo",
    "v2ray",
    "xray",
    "vpn",
    "tailscale",
    "zerotier",
    "zt",
    "docker",
    "veth",
    "br-",
    "virbr",
    "vmnet",
    "default switch",
    "wsl",
    "loopback",
];

fn looks_virtual(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    VIRTUAL_HINTS.iter().any(|h| {
        // 短片段（wg / zt / tun / tap）只匹配名称开头或独立单词，避免误伤 "Ethernet 2" 之类
        if h.len() <= 3 {
            n.starts_with(h)
                || n.split(|c: char| !c.is_ascii_alphanumeric())
                    .any(|w| w.starts_with(h) && w[h.len()..].chars().all(|c| c.is_ascii_digit()))
                || n.ends_with(&format!("_{h}"))
        } else {
            n.contains(h)
        }
    })
}

/// 分数越高越可能被其他机器访问到；负数表示不应作为首选。
fn score(i: &Iface) -> i32 {
    let o = i.ip.octets();
    if i.ip.is_loopback() || i.ip.is_link_local() || i.ip.is_unspecified() {
        return -100;
    }
    let mut s = 0;
    if i.gateway {
        s += 40;
    }
    if i.tunnel || looks_virtual(&i.name) {
        s -= 60;
    }
    // 198.18.0.0/15：常见代理 fake-ip 网段
    if o[0] == 198 && (o[1] == 18 || o[1] == 19) {
        s -= 60;
    }
    if i.ip.is_private() {
        s += 10;
        if o[0] == 192 && o[1] == 168 {
            s += 3;
        } else if o[0] == 10 {
            s += 2;
        }
    }
    s
}

/// 按优先级排好的可用地址（已去掉回环、链路本地地址）；`route` 为系统选路给出的地址，同分时优先。
pub fn ranked(mut list: Vec<Iface>, route: Option<Ipv4Addr>) -> Vec<Iface> {
    list.retain(|i| score(i) > -100);
    list.sort_by_key(|i| {
        let bonus = i32::from(Some(i.ip) == route);
        -(score(i) * 2 + bonus)
    });
    list.dedup_by_key(|i| i.ip);
    list
}

/// 系统路由选出的出口地址（UDP connect 不发包）。
fn route_ip() -> Option<Ipv4Addr> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("8.8.8.8:80").ok()?;
    match s.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(v) if !v.is_unspecified() && !v.is_loopback() => Some(v),
        _ => None,
    }
}

/// 配对用地址：首选 + 其他候选（只含分数不为负的，供人工替换）。
pub fn pair_ips() -> (Option<String>, Vec<String>) {
    let route = route_ip();
    let list = ranked(list(), route);
    if list.is_empty() {
        return (route.map(|r| r.to_string()), Vec::new());
    }
    let best = list[0].ip.to_string();
    let others = list[1..]
        .iter()
        .filter(|i| score(i) >= 0)
        .map(|i| i.ip.to_string())
        .collect();
    (Some(best), others)
}

#[cfg(windows)]
fn list() -> Vec<Iface> {
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetAdaptersAddresses, GAA_FLAG_INCLUDE_GATEWAYS, GAA_FLAG_SKIP_ANYCAST,
        GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST, IP_ADAPTER_ADDRESSES_LH,
    };
    use windows_sys::Win32::Networking::WinSock::{AF_INET, SOCKADDR_IN};

    // IF_TYPE_PROP_VIRTUAL（Wintun 等）与 IF_TYPE_TUNNEL
    const IF_TYPE_PROP_VIRTUAL: u32 = 53;
    const IF_TYPE_TUNNEL: u32 = 131;
    const IF_OPER_STATUS_UP: i32 = 1;

    let flags = GAA_FLAG_INCLUDE_GATEWAYS
        | GAA_FLAG_SKIP_ANYCAST
        | GAA_FLAG_SKIP_MULTICAST
        | GAA_FLAG_SKIP_DNS_SERVER;
    let mut size: u32 = 32 * 1024;
    let mut buf: Vec<u64> = Vec::new();
    for _ in 0..3 {
        buf = vec![0u64; (size as usize).div_ceil(8)];
        // SAFETY: 缓冲区按 8 字节对齐、长度为 size 字节；系统在其中写入链表。
        let rc = unsafe {
            GetAdaptersAddresses(
                AF_INET as u32,
                flags,
                std::ptr::null(),
                buf.as_mut_ptr().cast(),
                &mut size,
            )
        };
        match rc {
            0 => break,
            111 => continue, // ERROR_BUFFER_OVERFLOW：size 已更新为所需大小
            _ => return Vec::new(),
        }
    }
    let mut out = Vec::new();
    let mut p = buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
    // SAFETY: 遍历系统返回的链表，所有指针都指向 buf 内部或为空。
    unsafe {
        while !p.is_null() {
            let a = &*p;
            p = a.Next;
            if a.OperStatus != IF_OPER_STATUS_UP {
                continue;
            }
            let name = wide_to_string(a.FriendlyName);
            let gateway = !a.FirstGatewayAddress.is_null();
            let tunnel = matches!(a.IfType, IF_TYPE_PROP_VIRTUAL | IF_TYPE_TUNNEL);
            let mut u = a.FirstUnicastAddress;
            while !u.is_null() {
                let ua = &*u;
                u = ua.Next;
                let sa = ua.Address.lpSockaddr;
                if sa.is_null() || (*sa).sa_family != AF_INET {
                    continue;
                }
                let sin = &*(sa as *const SOCKADDR_IN);
                let ip = Ipv4Addr::from(u32::from_be(sin.sin_addr.S_un.S_addr));
                out.push(Iface {
                    name: name.clone(),
                    ip,
                    gateway,
                    tunnel,
                });
            }
        }
    }
    out
}

#[cfg(windows)]
unsafe fn wide_to_string(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut n = 0;
    while *p.add(n) != 0 {
        n += 1;
    }
    String::from_utf16_lossy(std::slice::from_raw_parts(p, n))
}

#[cfg(unix)]
fn list() -> Vec<Iface> {
    // /proc/net/route 中目标为 0 的行即默认路由所在网卡（非 Linux 读不到，按无网关处理）
    let gw_ifaces: Vec<String> = std::fs::read_to_string("/proc/net/route")
        .unwrap_or_default()
        .lines()
        .skip(1)
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let name = f.next()?;
            (f.next()? == "00000000").then(|| name.to_string())
        })
        .collect();
    let mut out = Vec::new();
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs 成功后遍历链表，最后 freeifaddrs 释放。
    unsafe {
        if libc::getifaddrs(&mut head) != 0 {
            return out;
        }
        let mut p = head;
        while !p.is_null() {
            let a = &*p;
            p = a.ifa_next;
            let up = a.ifa_flags & (libc::IFF_UP as u32) != 0;
            if !up || a.ifa_addr.is_null() || i32::from((*a.ifa_addr).sa_family) != libc::AF_INET {
                continue;
            }
            let sin = &*(a.ifa_addr as *const libc::sockaddr_in);
            let ip = Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
            let name = std::ffi::CStr::from_ptr(a.ifa_name)
                .to_string_lossy()
                .to_string();
            out.push(Iface {
                gateway: gw_ifaces.contains(&name),
                tunnel: a.ifa_flags & (libc::IFF_POINTOPOINT as u32) != 0,
                name,
                ip,
            });
        }
        libc::freeifaddrs(head);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn i(name: &str, ip: &str, gateway: bool, tunnel: bool) -> Iface {
        Iface {
            name: name.into(),
            ip: ip.parse().unwrap(),
            gateway,
            tunnel,
        }
    }

    #[test]
    fn tun_route_is_not_preferred() {
        // sing-box TUN 接管默认路由，真实网卡是 Hyper-V 外部交换机
        let list = vec![
            i("singbox_tun", "172.18.0.1", true, true),
            i("vEthernet (VM-Bridge)", "192.168.10.87", true, false),
            i("vEthernet (Default Switch)", "172.20.64.1", false, false),
            i(
                "vEthernet (WSL (Hyper-V firewall))",
                "172.23.128.1",
                false,
                false,
            ),
        ];
        let r = ranked(list, Some("172.18.0.1".parse().unwrap()));
        assert_eq!(r[0].ip.to_string(), "192.168.10.87");
    }

    #[test]
    fn route_wins_on_tie_and_names() {
        let list = vec![
            i("eth0", "10.0.0.5", true, false),
            i("eth1", "10.0.1.5", true, false),
        ];
        let r = ranked(list, Some("10.0.1.5".parse().unwrap()));
        assert_eq!(r[0].ip.to_string(), "10.0.1.5");
        assert!(looks_virtual("tun0") && looks_virtual("wg0") && looks_virtual("Clash"));
        assert!(looks_virtual("docker0") && looks_virtual("singbox_tun"));
        assert!(!looks_virtual("Ethernet 2") && !looks_virtual("eth0"));
        assert!(!looks_virtual("以太网") && !looks_virtual("Wi-Fi") && !looks_virtual("enp3s0"));
    }

    #[test]
    fn loopback_and_link_local_dropped() {
        let list = vec![
            i("lo", "127.0.0.1", false, false),
            i("eth0", "169.254.3.4", false, false),
        ];
        assert!(ranked(list, None).is_empty());
    }
}
