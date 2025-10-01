use std::net::SocketAddr;

use crate::config::Config;

pub fn healthcheck(config: Config, addr: Option<SocketAddr>, timeout: u64) -> anyhow::Result<()> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout))
        .build()?;

    let addr = match addr {
        Some(addr) => addr,
        None => config.bind.parse()?,
    };

    let url = format!("http://{addr}/healthcheck");
    tracing::debug!("Sending request to: {url}");

    let response = client.get(url).send();

    match response {
        Ok(response) if response.status().is_success() => {
            println!("OK");
            Ok(())
        }
        Ok(response) => {
            println!("ERROR");
            Err(anyhow::anyhow!(
                "Symbolicator ({addr}) is unhealthy. Status: {}",
                response.status()
            ))
        }
        Err(error) => {
            println!("ERROR");
            Err(anyhow::anyhow!(
                "Failed to check Symbolicator ({addr}) health: {error}"
            ))
        }
    }
}
