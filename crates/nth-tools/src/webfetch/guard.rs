use std::{
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    sync::Arc,
};

use reqwest::{
    Url,
    dns::{Addrs, Name, Resolve, Resolving},
};

/// Checks literals before connecting, since HTTP connectors bypass DNS for them.
pub(super) fn check_url(url: &Url) -> Result<(), String> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err("URL must start with http:// or https://".into());
    }
    let host = url.host_str().ok_or("URL has no host")?;
    let unwrapped = host.trim_start_matches('[').trim_end_matches(']');
    match unwrapped.parse::<IpAddr>() {
        Ok(ip) => check_ip(ip),
        Err(_) => check_host(host),
    }
}

fn check_host(host: &str) -> Result<(), String> {
    if host
        .trim_end_matches('.')
        .eq_ignore_ascii_case("metadata.google.internal")
    {
        return Err(format!("blocked destination {host}: metadata hostname"));
    }
    Ok(())
}

fn check_ip(ip: IpAddr) -> Result<(), String> {
    let blocked = match ip {
        IpAddr::V4(ip) => ip.is_link_local() || ip == Ipv4Addr::new(100, 100, 100, 200),
        IpAddr::V6(ip) => {
            ip.to_ipv4_mapped()
                .is_some_and(|v4| check_ip(v4.into()).is_err())
                || ip.is_unicast_link_local()
                || ip == Ipv6Addr::new(0xfd00, 0xec2, 0, 0, 0, 0, 0, 0x254)
        }
    };
    if blocked {
        Err(format!(
            "blocked destination {ip}: link-local or metadata address"
        ))
    } else {
        Ok(())
    }
}

/// The addresses validated here are the very addresses handed to the connector.
pub(super) struct GuardResolver(pub(super) Arc<dyn Resolve>);

impl Resolve for GuardResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        let resolving = self.0.resolve(name);
        Box::pin(async move {
            check_host(&host).map_err(io::Error::other)?;
            let addresses: Vec<_> = resolving.await?.collect();
            for address in &addresses {
                check_ip(address.ip()).map_err(io::Error::other)?;
            }
            Ok(Box::new(addresses.into_iter()) as Addrs)
        })
    }
}

pub(super) struct SystemResolver;

impl Resolve for SystemResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let addresses: Vec<_> = tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            Ok(Box::new(addresses.into_iter()) as Addrs)
        })
    }
}
