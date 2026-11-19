use std::collections::HashMap;
use std::env;
use std::net::TcpListener;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};

use ci_node_status::{DEFAULT_MAX_CONCURRENCY, ShutdownHandle, build_service_from_env_map, serve};

static WAKE_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

extern "C" fn handle_signal(_sig: libc::c_int) {
    let fd = WAKE_WRITE_FD.load(Ordering::Acquire);
    ShutdownHandle::wake_fd(fd);
}

fn install_signal_handlers(write_fd: libc::c_int) {
    WAKE_WRITE_FD.store(write_fd, Ordering::Release);
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = handle_signal as *const () as usize;
        sa.sa_flags = 0;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut());
        libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut());
        // Ignore SIGPIPE so disconnected clients never terminate the server.
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
}

fn disarm_signal_handlers() {
    WAKE_WRITE_FD.store(-1, Ordering::Release);
    unsafe {
        libc::signal(libc::SIGTERM, libc::SIG_DFL);
        libc::signal(libc::SIGINT, libc::SIG_DFL);
    }
}

fn main() -> ExitCode {
    let shutdown = match ShutdownHandle::new() {
        Ok(h) => h,
        Err(err) => {
            eprintln!("fatal: failed to create shutdown wake-up pipe: {err}");
            return ExitCode::FAILURE;
        }
    };
    install_signal_handlers(shutdown.write_fd());

    let env_map: HashMap<String, String> = env::vars().collect();
    let host = env_map
        .get("BIND_HOST")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or("0.0.0.0");
    let port: u16 = env_map
        .get("PORT")
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(8080);
    let max_concurrency: usize = env_map
        .get("MAX_CONCURRENCY")
        .and_then(|s| s.trim().parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_MAX_CONCURRENCY);

    let service = match build_service_from_env_map(&env_map) {
        Ok(svc) => Arc::new(svc),
        Err(err) => {
            disarm_signal_handlers();
            eprintln!("fatal: failed to initialize service: {err}");
            return ExitCode::FAILURE;
        }
    };

    let bind_addr = format!("{host}:{port}");
    let listener = match TcpListener::bind(&bind_addr) {
        Ok(l) => l,
        Err(err) => {
            disarm_signal_handlers();
            eprintln!("fatal: failed to bind {bind_addr}: {err}");
            return ExitCode::FAILURE;
        }
    };

    println!(
        "ci-node-status listening on http://{bind_addr} (node={}, namespace={})",
        service.node(),
        service.namespace()
    );

    let result = serve(listener, service, max_concurrency, &shutdown);
    disarm_signal_handlers();

    if let Err(err) = result {
        eprintln!("fatal: server error: {err}");
        return ExitCode::FAILURE;
    }

    println!("ci-node-status shut down cleanly");
    ExitCode::SUCCESS
}
