//! Server-side request forgery protection for remote fetches.
//!
//! Remote sources are user-supplied URLs, so without a guard anyone can make
//! the server request internal addresses: cloud metadata endpoints
//! (`169.254.169.254` hands out service-account credentials on GCP/AWS),
//! localhost admin ports, or other hosts on the private network.
//!
//! Two layers, because reqwest only consults the DNS resolver for hostnames:
//! - [`GuardedResolver`] drops non-public addresses from every DNS answer.
//!   Every connection reqwest makes (including after redirects) goes through
//!   it, and the connection uses the addresses it vetted, so DNS rebinding
//!   doesn't get around it.
//! - [`check_url`] rejects URLs whose host is a non-public IP literal. It runs
//!   on the initial URL and, via [`redirect_policy`], on every redirect hop.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::redirect;
use url::{Host, Url};

const MAX_REDIRECTS: usize = 10;

/// A fetch the guard refused. Surfaced through reqwest's error chain so
/// callers can tell it apart from ordinary network failures.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct Refused(pub String);

/// Find a [`Refused`] anywhere in an error's source chain.
pub fn find_refusal<'a>(mut err: &'a (dyn std::error::Error + 'static)) -> Option<&'a Refused> {
    loop {
        if let Some(refused) = err.downcast_ref::<Refused>() {
            return Some(refused);
        }
        err = err.source()?;
    }
}

/// Whether `ip` is a publicly routable unicast address.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => is_public_v6(v6),
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_documentation()
        || a == 0 // "this network"
        || (a == 100 && (64..128).contains(&b)) // carrier-grade NAT, 100.64.0.0/10
        || (a == 192 && b == 0 && ip.octets()[2] == 0) // IETF protocol assignments
        || (a == 198 && (b == 18 || b == 19)) // benchmarking, 198.18.0.0/15
        || a >= 240) // reserved
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    // IPv4 addresses embedded in IPv6 are judged by their IPv4 value.
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_public_v4(v4);
    }
    let segments = ip.segments();
    if segments[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
        // NAT64 well-known prefix, 64:ff9b::/96
        let [.., a, b] = segments;
        return is_public_v4(Ipv4Addr::new(
            (a >> 8) as u8,
            a as u8,
            (b >> 8) as u8,
            b as u8,
        ));
    }
    !(ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || ip.is_unique_local()
        || ip.is_unicast_link_local()
        || segments[0] == 0x2001 && segments[1] == 0x0db8) // documentation
}

/// Reject URLs that aren't http(s) or whose host is a non-public IP literal.
/// Hostnames are checked at connect time by [`GuardedResolver`].
pub fn check_url(url: &Url) -> Result<(), Refused> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(Refused(format!(
            "unsupported URL scheme '{}'",
            url.scheme()
        )));
    }
    let ip = match url.host() {
        Some(Host::Ipv4(ip)) => IpAddr::V4(ip),
        Some(Host::Ipv6(ip)) => IpAddr::V6(ip),
        Some(Host::Domain(_)) => return Ok(()),
        None => return Err(Refused("URL has no host".to_string())),
    };
    if is_public(ip) {
        Ok(())
    } else {
        Err(Refused(format!(
            "refusing to fetch from non-public address {ip}"
        )))
    }
}

/// Follow redirects (up to a limit), checking every hop with [`check_url`].
pub fn redirect_policy() -> redirect::Policy {
    redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= MAX_REDIRECTS {
            return attempt.error("too many redirects");
        }
        match check_url(attempt.url()) {
            Ok(()) => attempt.follow(),
            Err(e) => attempt.error(e),
        }
    })
}

/// DNS resolver that only returns public addresses.
pub struct GuardedResolver;

impl Resolve for GuardedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        Box::pin(async move {
            let host = name.as_str().to_owned();
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .filter(|addr| is_public(addr.ip()))
                .collect();
            if addrs.is_empty() {
                return Err(Box::new(Refused(format!(
                    "refusing to fetch from '{host}': it does not resolve to a public address"
                )))
                    as Box<dyn std::error::Error + Send + Sync>);
            }
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn blocks_non_public_addresses() {
        for addr in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "0.0.0.0",
            "100.64.0.1",
            "255.255.255.255",
            "224.0.0.1",
            "::1",
            "::",
            "fc00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            "64:ff9b::a9fe:a9fe", // NAT64 of 169.254.169.254
        ] {
            assert!(!is_public(ip(addr)), "{addr} should be blocked");
        }
    }

    #[test]
    fn allows_public_addresses() {
        for addr in [
            "8.8.8.8",
            "1.1.1.1",
            "2606:4700:4700::1111",
            "::ffff:8.8.8.8",
        ] {
            assert!(is_public(ip(addr)), "{addr} should be allowed");
        }
    }

    #[test]
    fn check_url_rejects_ip_literals_and_odd_schemes() {
        for url in [
            "http://169.254.169.254/latest/meta-data",
            "http://127.0.0.1:8080/",
            "http://[::1]/",
            "file:///etc/passwd",
            "ftp://example.com/a.mp3",
        ] {
            assert!(check_url(&Url::parse(url).unwrap()).is_err(), "{url}");
        }
        assert!(check_url(&Url::parse("https://example.com/a.mp3").unwrap()).is_ok());
        assert!(check_url(&Url::parse("http://8.8.8.8/a.mp3").unwrap()).is_ok());
    }

    #[tokio::test]
    async fn resolver_drops_localhost() {
        let name: Name = "localhost".parse().unwrap();
        let err = match GuardedResolver.resolve(name).await {
            Ok(_) => panic!("localhost should not resolve"),
            Err(e) => e,
        };
        assert!(err.downcast_ref::<Refused>().is_some(), "{err}");
    }
}
