//! Plain blocking HTTP with JSON, for the R bridge and the model servers.
//! Called from blocking threads (tokio's spawn_blocking), never the UI loop.

use std::{io::BufRead, time::Duration};

use anyhow::{Result, anyhow};
use serde_json::Value;

fn agent(timeout: Option<Duration>) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(timeout)
        .http_status_as_error(false)
        .build()
        .into()
}

/// Why a request failed, worded for the user.
fn describe(e: ureq::Error, base: &str) -> anyhow::Error {
    match e {
        ureq::Error::Timeout(_) => anyhow!("{base} did not answer in time"),
        ureq::Error::Io(io) if io.kind() == std::io::ErrorKind::ConnectionRefused => {
            anyhow!("cannot reach {base} (connection refused)")
        }
        other => anyhow!("cannot reach {base} ({other})"),
    }
}

/// GET or POST (with a JSON body) and parse the JSON answer. A non-2xx
/// status is an error carrying the start of the body.
pub fn json(
    url: &str,
    headers: &[(&str, &str)],
    body: Option<&Value>,
    timeout: Duration,
) -> Result<Value> {
    let base = url.split('/').take(3).collect::<Vec<_>>().join("/");
    let a = agent(Some(timeout));
    let resp = match body {
        Some(b) => {
            let mut r = a.post(url);
            for (k, v) in headers {
                r = r.header(*k, *v);
            }
            r.send_json(b)
        }
        None => {
            let mut r = a.get(url);
            for (k, v) in headers {
                r = r.header(*k, *v);
            }
            r.call()
        }
    }
    .map_err(|e| describe(e, &base))?;
    let status = resp.status().as_u16();
    let text = resp
        .into_body()
        .read_to_string()
        .map_err(|e| describe(e, &base))?;
    if !(200..300).contains(&status) {
        return Err(anyhow!(
            "HTTP {status}: {}",
            text.chars().take(300).collect::<String>()
        ));
    }
    serde_json::from_str(&text).map_err(|e| anyhow!("{base} sent something that isn't JSON ({e})"))
}

/// POST a JSON body and hand each line of the (streamed) answer to `line`
/// until it returns false. No overall timeout: a model may think for long.
pub fn stream_lines(
    url: &str,
    body: &Value,
    mut line: impl FnMut(&str) -> Result<bool>,
) -> Result<()> {
    let base = url.split('/').take(3).collect::<Vec<_>>().join("/");
    let resp = agent(None)
        .post(url)
        .header("Content-Type", "application/json")
        .send_json(body)
        .map_err(|e| describe(e, &base))?;
    let status = resp.status().as_u16();
    let mut reader = std::io::BufReader::new(resp.into_body().into_reader());
    if !(200..300).contains(&status) {
        let mut text = String::new();
        let _ = std::io::Read::read_to_string(&mut reader, &mut text);
        return Err(anyhow!(
            "HTTP {status}: {}",
            text.chars().take(300).collect::<String>()
        ));
    }
    let mut buf = String::new();
    loop {
        buf.clear();
        if reader.read_line(&mut buf)? == 0 {
            return Ok(());
        }
        if !line(buf.trim_end())? {
            return Ok(());
        }
    }
}
