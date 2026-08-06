use std::collections::BTreeMap;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

const LC_TIMEOUT: Duration = Duration::from_secs(3);
const LC_RETRIES: u32 = 3;

static NEXT_ID: AtomicU32 = AtomicU32::new(1);

#[derive(Debug, PartialEq)]
pub struct CheckResult {
    pub ok: bool,
    pub diagnostics: String,
}

#[derive(Debug, PartialEq)]
pub struct ReplyChunk {
    pub id: u32,
    pub seq: u32,
    pub total: u32,
    pub ok: bool,
    pub payload: String,
}

/// Build a request datagram for one file.
pub fn encode_request(id: u32, user: &str, password: &str, path: &str) -> Vec<u8> {
    format!("LCOMPILE\t{id}\tREQ\tcheck\t{user}\t{password}\t{path}").into_bytes()
}

/// Parse one reply datagram; None if not a well-formed LCOMPILE reply.
pub fn parse_reply(datagram: &[u8]) -> Option<ReplyChunk> {
    let text = std::str::from_utf8(datagram).ok()?;
    let parts: Vec<&str> = text.splitn(7, '\t').collect();
    if parts.len() < 7 || parts[0] != "LCOMPILE" || parts[2] != "RPLY" {
        return None;
    }
    let ok = match parts[5] {
        "OK" => true,
        "FAIL" => false,
        _ => return None,
    };
    Some(ReplyChunk {
        id: parts[1].parse().ok()?,
        seq: parts[3].parse().ok()?,
        total: parts[4].parse().ok()?,
        ok,
        payload: parts[6].to_string(),
    })
}

/// Concatenate chunks 1..=total in order; None if any are missing.
pub fn reassemble(chunks: &BTreeMap<u32, ReplyChunk>, total: u32) -> Option<CheckResult> {
    if total == 0 || (chunks.len() as u32) < total {
        return None;
    }
    let mut diagnostics = String::new();
    let mut ok = true;
    for seq in 1..=total {
        let c = chunks.get(&seq)?;
        ok = c.ok;
        diagnostics.push_str(&c.payload);
    }
    Some(CheckResult { ok, diagnostics })
}

pub struct CompileClient {
    addr: SocketAddr,
    timeout: Duration,
    retries: u32,
}

impl CompileClient {
    pub fn new(host: &str, udp_port: u16) -> Result<Self> {
        let addr = (host, udp_port)
            .to_socket_addrs()
            .with_context(|| format!("resolving {host}:{udp_port}"))?
            .next()
            .ok_or_else(|| anyhow!("no address for {host}:{udp_port}"))?;
        Ok(Self {
            addr,
            timeout: LC_TIMEOUT,
            retries: LC_RETRIES,
        })
    }

    pub fn with_addr(addr: SocketAddr) -> Self {
        Self {
            addr,
            timeout: LC_TIMEOUT,
            retries: LC_RETRIES,
        }
    }

    pub fn check(&self, user: &str, password: &str, path: &str) -> Result<CheckResult> {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let sock = UdpSocket::bind(("0.0.0.0", 0)).context("binding UDP socket")?;
        sock.set_read_timeout(Some(self.timeout))
            .context("set_read_timeout")?;
        let req = encode_request(id, user, password, path);

        for _attempt in 0..self.retries {
            sock.send_to(&req, self.addr)
                .with_context(|| format!("sending to {}", self.addr))?;
            let mut chunks: BTreeMap<u32, ReplyChunk> = BTreeMap::new();
            let mut buf = [0u8; 2048];
            loop {
                match sock.recv_from(&mut buf) {
                    Ok((n, _)) => {
                        if let Some(chunk) = parse_reply(&buf[..n]) {
                            if chunk.id != id {
                                continue;
                            }
                            let total = chunk.total;
                            chunks.insert(chunk.seq, chunk);
                            if let Some(res) = reassemble(&chunks, total) {
                                return Ok(res);
                            }
                        }
                    }
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::TimedOut =>
                    {
                        break; // timeout -> retry the whole request
                    }
                    Err(e) => return Err(e).context("recv_from"),
                }
            }
        }
        bail!(
            "no complete reply from {} after {} attempts",
            self.addr,
            self.retries
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn encodes_request_with_seven_fields() {
        let out = String::from_utf8(encode_request(7, "simon", "pw", "cmds/secure/cc.c")).unwrap();
        assert_eq!(out, "LCOMPILE\t7\tREQ\tcheck\tsimon\tpw\tcmds/secure/cc.c");
    }

    #[test]
    fn parses_valid_reply_and_keeps_tabs_in_payload() {
        let d = b"LCOMPILE\t7\tRPLY\t2\t3\tFAIL\tfile.c:1: error: a\tb";
        let c = parse_reply(d).unwrap();
        assert_eq!(
            c,
            ReplyChunk {
                id: 7,
                seq: 2,
                total: 3,
                ok: false,
                payload: "file.c:1: error: a\tb".to_string()
            }
        );
    }

    #[test]
    fn rejects_non_lcompile_or_short_datagrams() {
        assert!(parse_reply(b"NOPE\t1\tRPLY\t1\t1\tOK\t").is_none());
        assert!(parse_reply(b"LCOMPILE\t1\tREQ\tcheck").is_none());
    }

    #[test]
    fn reassembles_only_when_all_chunks_present() {
        let mut m: BTreeMap<u32, ReplyChunk> = BTreeMap::new();
        m.insert(
            1,
            ReplyChunk {
                id: 1,
                seq: 1,
                total: 2,
                ok: false,
                payload: "a\n".into(),
            },
        );
        assert_eq!(reassemble(&m, 2), None);
        m.insert(
            2,
            ReplyChunk {
                id: 1,
                seq: 2,
                total: 2,
                ok: false,
                payload: "b\n".into(),
            },
        );
        assert_eq!(
            reassemble(&m, 2),
            Some(CheckResult {
                ok: false,
                diagnostics: "a\nb\n".into()
            })
        );
    }

    #[test]
    fn check_reassembles_chunked_reply_over_loopback() {
        use std::net::{SocketAddr, UdpSocket};
        use std::thread;

        // Fake server: receives one request, replies with 2 chunks.
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        let server_addr: SocketAddr = server.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let mut buf = [0u8; 2048];
            let (n, from) = server.recv_from(&mut buf).unwrap();
            let req = std::str::from_utf8(&buf[..n]).unwrap();
            let id: u32 = req.split('\t').nth(1).unwrap().parse().unwrap();
            server
                .send_to(
                    format!("LCOMPILE\t{id}\tRPLY\t1\t2\tFAIL\tx.c:1: error: bad\n").as_bytes(),
                    from,
                )
                .unwrap();
            server
                .send_to(
                    format!("LCOMPILE\t{id}\tRPLY\t2\t2\tFAIL\tx.c:2: error: worse\n").as_bytes(),
                    from,
                )
                .unwrap();
        });

        let client = CompileClient::with_addr(server_addr);
        let res = client.check("simon", "pw", "x.c").unwrap();
        handle.join().unwrap();
        assert_eq!(
            res,
            CheckResult {
                ok: false,
                diagnostics: "x.c:1: error: bad\nx.c:2: error: worse\n".into(),
            }
        );
    }
}
