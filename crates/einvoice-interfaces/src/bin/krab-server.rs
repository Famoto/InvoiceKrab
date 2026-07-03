//! `krab-server` binary — the HTTP transformation service.
//!
//! All decisions live in [`einvoice_interfaces::server`] where they are unit
//! tested; this entry point owns only the untestable I/O boundary: binding
//! the listener, the worker accept loop, reading bodies under the memory
//! gate, and writing responses.
//!
//! `POST /transform?to=<format>[&from=<format>]` with the source XML as the
//! request body. Configuration is environment variables — see
//! [`einvoice_interfaces::server::config`].

use std::io::Read as _;
use std::process::ExitCode;
use std::sync::Arc;

use einvoice_interfaces::server::{self, Config, MemGate, handle};
use tiny_http::{Header, Method, Request, Response, Server};

fn main() -> ExitCode {
    let config = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("krab-server: {e}");
            return ExitCode::from(78); // EX_CONFIG
        }
    };
    // Self-probe mode for container HEALTHCHECKs: the image is `FROM scratch`,
    // so the server binary doubles as its own HTTP client.
    if std::env::args().any(|a| a == "--healthcheck") {
        return healthcheck(&config.addr);
    }
    let server = match listen(&config.addr) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            eprintln!("krab-server: cannot listen on {}: {e}", config.addr);
            return ExitCode::from(74); // EX_IOERR
        }
    };
    let gate = Arc::new(MemGate::new(config.mem_budget_bytes));
    eprintln!(
        "krab-server listening on {} — {} workers, {} bytes memory budget, x{} reservation",
        config.addr, config.workers, config.mem_budget_bytes, config.mem_blowup
    );

    let workers: Vec<_> = (0..config.workers)
        .map(|_| {
            let server = Arc::clone(&server);
            let gate = Arc::clone(&gate);
            let blowup = config.mem_blowup;
            std::thread::spawn(move || {
                // `recv` fails only when the listener is gone; the process is
                // done at that point.
                while let Ok(request) = server.recv() {
                    serve_one(request, &gate, blowup);
                }
            })
        })
        .collect();
    for worker in workers {
        let _ = worker.join();
    }
    ExitCode::SUCCESS
}

/// Serves a single request end to end: route, reserve memory, read the body,
/// delegate to the [`server`](einvoice_interfaces::server) handlers, respond.
/// Failures to write the response are ignored — the client is gone.
fn serve_one(mut request: Request, gate: &MemGate, blowup: u64) {
    // Owned: `url()` borrows the request, which `as_reader()` needs mutably.
    let url = request.url().to_owned();
    let (path, query) = url.split_once('?').unwrap_or((url.as_str(), ""));

    // Body-less capability/health routes: no memory reservation needed.
    match (request.method(), path) {
        (Method::Get, "/health") => return respond(request, 200, "ok".into(), false),
        (Method::Get, "/formats") => {
            return respond_typed(request, 200, server::formats(), "application/json");
        }
        (Method::Get, "/analyze") => {
            return match server::analyze(query) {
                Ok(table) => respond_typed(request, 200, table, "text/plain"),
                Err(problem) => respond(request, 400, problem, false),
            };
        }
        (Method::Post, "/transform") => {}
        _ => {
            return respond(
                request,
                404,
                "POST /transform?to=<format>[&from=<format>] | GET /formats | GET /analyze[?from=<format>] | GET /health"
                    .into(),
                false,
            );
        }
    }

    // The reservation is sized from Content-Length, so a body without one
    // (chunked) cannot be admitted. Standard answer: 411 Length Required.
    let Some(length) = request.body_length() else {
        respond(request, 411, "Content-Length required".into(), true);
        return;
    };

    // Reserve the request's estimated peak memory (body + parse blowup)
    // before reading a byte. Blocks while the budget is exhausted — workers
    // waiting here are the backpressure. Held until the response is written.
    let _reservation = match gate.acquire((length as u64).saturating_mul(blowup)) {
        Ok(guard) => guard,
        Err(never_fits) => {
            respond(request, 413, never_fits.to_string(), true);
            return;
        }
    };

    let mut body = Vec::with_capacity(length);
    // tiny_http already frames the reader by Content-Length; `take` makes the
    // bound local and explicit.
    if let Err(e) = request
        .as_reader()
        .take(length as u64)
        .read_to_end(&mut body)
    {
        respond(
            request,
            400,
            format!("failed reading request body: {e}"),
            true,
        );
        return;
    }

    let reply = handle(query, &body);
    let content_type = if reply.status == 200 {
        "application/xml"
    } else {
        "text/plain"
    };
    let mut response = Response::from_string(reply.body).with_status_code(reply.status);
    if let Ok(h) = Header::from_bytes(&b"Content-Type"[..], content_type.as_bytes()) {
        response.add_header(h);
    }
    if !reply.warnings.is_empty()
        && let Ok(h) = Header::from_bytes(&b"X-Krab-Warnings"[..], reply.warnings.as_bytes())
    {
        response.add_header(h);
    }
    let _ = request.respond(response);
}

/// Binds `addr` with `TCP_NODELAY` set on the listener (accepted sockets
/// inherit it on Linux). tiny_http never sets it, and Nagle + delayed ACK
/// otherwise adds ~40 ms to every request on a kept-alive connection.
fn listen(addr: &str) -> Result<Server, Box<dyn std::error::Error + Send + Sync>> {
    let listener = std::net::TcpListener::bind(addr)?;
    socket2::SockRef::from(&listener).set_nodelay(true)?;
    Server::from_listener(listener, None)
}

/// Writes a plain-text response. `close` adds `Connection: close`, used when
/// the request body was not (fully) read — the connection cannot be reused,
/// and closing it stops an in-flight upload at the TCP level.
fn respond(request: Request, status: u16, text: String, close: bool) {
    let mut response = Response::from_string(text).with_status_code(status);
    if close && let Ok(h) = Header::from_bytes(&b"Connection"[..], &b"close"[..]) {
        response.add_header(h);
    }
    let _ = request.respond(response);
}

/// Writes a response with an explicit Content-Type.
fn respond_typed(request: Request, status: u16, body: String, content_type: &str) {
    let mut response = Response::from_string(body).with_status_code(status);
    if let Ok(h) = Header::from_bytes(&b"Content-Type"[..], content_type.as_bytes()) {
        response.add_header(h);
    }
    let _ = request.respond(response);
}

/// `--healthcheck`: connect to the configured port on loopback, `GET /health`,
/// exit 0 on HTTP 200. The listen host (e.g. `0.0.0.0`) is not a connectable
/// address, so only the port is taken from `addr`.
fn healthcheck(addr: &str) -> ExitCode {
    let port = addr.rsplit_once(':').map_or(addr, |(_, p)| p);
    let ok = probe(&format!("127.0.0.1:{port}")).is_some();
    if !ok {
        eprintln!("krab-server: healthcheck failed for 127.0.0.1:{port}");
    }
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Minimal HTTP/1.0 GET over a raw socket; `Some(())` when the status is 200.
fn probe(target: &str) -> Option<()> {
    use std::io::Write as _;
    let timeout = std::time::Duration::from_secs(3);
    let addr = target.parse().ok()?;
    let mut stream = std::net::TcpStream::connect_timeout(&addr, timeout).ok()?;
    stream.set_read_timeout(Some(timeout)).ok()?;
    stream
        .write_all(b"GET /health HTTP/1.0\r\nHost: localhost\r\n\r\n")
        .ok()?;
    let mut status_line = [0u8; 16];
    stream.read_exact(&mut status_line).ok()?;
    // "HTTP/1.x 200 ..." — the status code sits at bytes 9..12.
    (&status_line[9..12] == b"200").then_some(())
}
