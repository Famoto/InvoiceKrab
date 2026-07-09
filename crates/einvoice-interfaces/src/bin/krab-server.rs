//! `krab-server` binary — the HTTP transformation service.
//!
//! All decisions live in [`einvoice_interfaces::server`] where they are unit
//! tested; this entry point owns only the untestable I/O boundary: binding
//! the listener, building the tokio runtime, and shutdown signals. Routing,
//! admission, and body handling are the tested
//! [`router`](einvoice_interfaces::server::router).
//!
//! `POST /transform?to=<format>[&from=<format>]` with the source XML as the
//! request body. Configuration is environment variables — see
//! [`einvoice_interfaces::server::config`].
//!
//! SIGTERM or SIGINT triggers a graceful shutdown: the listener stops
//! accepting, in-flight requests drain, then the process exits. The
//! container orchestrator's kill grace period is the drain deadline.

use std::io::Read as _;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use einvoice_interfaces::server::{self, Config, MemGate};

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
    let listener = match listen(&config.addr) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("krab-server: cannot listen on {}: {e}", config.addr);
            return ExitCode::from(74); // EX_IOERR
        }
    };
    // `max_blocking_threads` caps the pool running the CPU-bound transforms
    // at the worker count — the same transform parallelism the old
    // thread-per-worker loop had, now separate from connection I/O.
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(config.workers)
        .max_blocking_threads(config.workers)
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("krab-server: cannot start runtime: {e}");
            return ExitCode::from(74); // EX_IOERR
        }
    };
    eprintln!(
        "krab-server listening on {} — {} workers, {} bytes memory budget, x{} reservation, {}s body timeout",
        config.addr,
        config.workers,
        config.mem_budget_bytes,
        config.mem_blowup,
        config.body_timeout_secs
    );
    runtime.block_on(serve(listener, &config))
}

/// Converts the configured listener to tokio and serves until a shutdown
/// signal drains the connections.
async fn serve(listener: std::net::TcpListener, config: &Config) -> ExitCode {
    let listener = match listener
        .set_nonblocking(true)
        .and_then(|()| tokio::net::TcpListener::from_std(listener))
    {
        Ok(l) => l,
        Err(e) => {
            eprintln!("krab-server: cannot register listener: {e}");
            return ExitCode::from(74); // EX_IOERR
        }
    };
    let gate = Arc::new(MemGate::new(config.mem_budget_bytes));
    let app = server::router(
        gate,
        config.mem_blowup,
        Duration::from_secs(config.body_timeout_secs),
    );
    if let Err(e) = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
    {
        eprintln!("krab-server: server error: {e}");
        return ExitCode::from(74); // EX_IOERR
    }
    ExitCode::SUCCESS
}

/// Resolves on SIGTERM (container stop) or SIGINT (^C), starting the drain.
async fn shutdown_signal() {
    let interrupt = async {
        // Errors registering ^C leave SIGTERM as the only trigger.
        let _ = tokio::signal::ctrl_c().await;
    };
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut term) => {
                term.recv().await;
            }
            Err(_) => std::future::pending().await,
        }
    };
    tokio::select! {
        () = interrupt => {},
        () = terminate => {},
    }
    eprintln!("krab-server: shutdown signal received, draining connections");
}

/// Binds `addr` with `TCP_NODELAY` and TCP keepalive set on the listener
/// (accepted sockets inherit both on Linux). Nagle + delayed ACK otherwise
/// adds ~40 ms to every request on a kept-alive connection.
///
/// Keepalive complements the in-process body timeouts: the timeouts bound
/// live-but-silent peers, keepalive reaps *dead* ones (crashed client,
/// dropped NAT mapping) — after ~2 min of unanswered probes the kernel
/// resets the connection and the pending read errors out.
fn listen(addr: &str) -> Result<std::net::TcpListener, Box<dyn std::error::Error + Send + Sync>> {
    let listener = std::net::TcpListener::bind(addr)?;
    let sock = socket2::SockRef::from(&listener);
    sock.set_tcp_nodelay(true)?;
    // Probe count stays at the kernel default (9 on Linux); overriding it
    // needs socket2's `all` feature for one knob that barely matters.
    sock.set_tcp_keepalive(
        &socket2::TcpKeepalive::new()
            .with_time(Duration::from_secs(30))
            .with_interval(Duration::from_secs(10)),
    )?;
    Ok(listener)
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
    let timeout = Duration::from_secs(3);
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
