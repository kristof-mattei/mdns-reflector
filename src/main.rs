mod cli;
mod reflector;
mod signal_handlers;
mod sockets;
mod unix;

use std::env::{self, VarError};
use std::fs::{File, remove_file};
use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::process::exit;
use std::sync::Arc;
use std::time::Duration;

use cli::{Config, parse_cli};
use color_eyre::config::HookBuilder;
use color_eyre::eyre;
use libc::{SIG_IGN, SIGCHLD, SIGHUP, chdir, fork, getpid, kill, pid_t, setsid, signal, umask};
use reflector::reflect;
use sockets::{create_recv_sock, create_send_sock};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::{Level, event};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

// TODO this should come from cargo
const PACKAGE: &str = env!("CARGO_PKG_NAME");
const PACKET_SIZE: usize = 65536;
const MDNS_ADDR: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
const MDNS_PORT: u16 = 5353;

const BROADCAST_MDNS: SocketAddr = SocketAddr::V4(SocketAddrV4::new(MDNS_ADDR, MDNS_PORT));

fn build_default_filter() -> EnvFilter {
    EnvFilter::builder()
        .parse(format!("INFO,{}=TRACE", env!("CARGO_CRATE_NAME")))
        .expect("Default filter should always work")
}

fn init_tracing() -> Result<(), eyre::Report> {
    let (filter, filter_parsing_error) = match env::var(EnvFilter::DEFAULT_ENV) {
        Ok(user_directive) => match EnvFilter::builder().parse(user_directive) {
            Ok(filter) => (filter, None),
            Err(error) => (build_default_filter(), Some(eyre::Report::new(error))),
        },
        Err(VarError::NotPresent) => (build_default_filter(), None),
        Err(error @ VarError::NotUnicode(_)) => {
            (build_default_filter(), Some(eyre::Report::new(error)))
        },
    };

    let registry = tracing_subscriber::registry();

    #[cfg(feature = "tokio-console")]
    let registry = registry.with(console_subscriber::ConsoleLayer::builder().spawn());

    registry
        .with(tracing_subscriber::fmt::layer().with_filter(filter))
        .with(tracing_error::ErrorLayer::default())
        .try_init()?;

    filter_parsing_error.map_or(Ok(()), Err)
}

fn main() -> Result<(), eyre::Report> {
    HookBuilder::default()
        .capture_span_trace_by_default(true)
        .display_env_section(false)
        .install()?;

    init_tracing()?;

    // initialize the runtime
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    rt.block_on(start_tasks())
}

async fn start_tasks() -> Result<(), eyre::Error> {
    let (config, interfaces) = parse_cli().inspect_err(|error| {
        // this prints the error in color and exits
        // can't do anything else until
        // https://github.com/clap-rs/clap/issues/2914
        // is merged in
        if let Some(clap_error) = error.downcast_ref::<clap::error::Error>() {
            clap_error.exit();
        }
    })?;

    let config = Arc::new(config);

    // unsure what to do here for now
    // openlog(PACKAGE, LOG_PID | LOG_CONS, LOG_DAEMON);

    let cancellation_token = CancellationToken::new();

    if config.foreground {
        // check for pid file when running in foreground
        let running_pid = already_running(&config);

        if let Some(running_pid) = running_pid {
            event!(Level::ERROR, "already running as pid {}", running_pid);
            return Ok(());
        }
    } else {
        daemonize(&config, &cancellation_token);
    }

    // create receiving socket
    let server_socket = create_recv_sock().map_err(|err| {
        event!(Level::ERROR, ?err, "unable to create server socket");

        err
    })?;

    let mut sockets = Vec::with_capacity(interfaces.len());

    // create sending sockets
    for interface in interfaces {
        let send_socket = create_send_sock(&server_socket, interface).map_err(|err| {
            event!(Level::ERROR, ?err, "unable to create socket for interface");

            err
        })?;

        sockets.push(send_socket);
    }

    let task_tracker = TaskTracker::new();

    {
        let cancellation_token = cancellation_token.clone();

        task_tracker.spawn(reflect(
            server_socket,
            sockets,
            config.clone(),
            cancellation_token,
        ));
    }

    // now we wait forever for either
    // * SIGTERM
    // * ctrl + c (SIGINT)
    // * a message on the shutdown channel, sent either by the server task or
    // another task when they complete (which means they failed)
    tokio::select! {
        _ = signal_handlers::wait_for_sigint() => {
            // we completed because ...
            event!(Level::WARN, message = "CTRL+C detected, stopping all tasks");
        },
        _ = signal_handlers::wait_for_sigterm() => {
            // we completed because ...
            event!(Level::WARN, message = "Sigterm detected, stopping all tasks");
        },
        () = cancellation_token.cancelled() => {
            event!(Level::WARN, "Underlying task stopped, stopping all others tasks");
        },
    };

    cancellation_token.cancel();

    task_tracker.close();

    // wait for the task that holds the server to exit gracefully
    // it listens to shutdown_send
    if timeout(Duration::from_millis(10000), task_tracker.wait())
        .await
        .is_err()
    {
        event!(Level::ERROR, "Tasks didn't stop within allotted time!");
    }

    // remove pid file if it belongs to us
    if already_running(&config).is_some_and(|pid| pid == unsafe { getpid() }) {
        if let Err(err) = remove_file(&config.pid_file) {
            event!(
                Level::ERROR,
                ?err,
                pid_file = ?config.pid_file,
                "Failed to remove pid_file, manual deletion required",
            );
        }
    }

    event!(Level::INFO, "Goodbye");

    Ok(())
}

fn daemonize(config: &Config, _cancellation_token: &CancellationToken) {
    // pid_t running_pid;
    let pid: pid_t = unsafe { fork() };

    if pid < 0 {
        let err = std::io::Error::last_os_error();
        event!(Level::ERROR, ?err, "fork()");

        exit(1);
    }

    // exit parent process
    if pid > 0 {
        exit(0);
    }

    // let closure = |signal| {
    //     mdns_reflector_shutdown(signal, &cancellation_token);
    // };

    // signals
    unsafe {
        signal(SIGCHLD, SIG_IGN);
        signal(SIGHUP, SIG_IGN);
        // signal(SIGTERM, closure as usize);

        setsid();
        umask(0o0027);
        chdir(c"/".as_ptr());
    }

    // close all std fd and reopen /dev/null for them
    // int i;
    // for (i = 0; i < 3; i++)
    // {
    //     close(i);
    //     if (open("/dev/null", O_RDWR) != i)
    //     {
    //         log_message(LOG_ERR, "unable to open /dev/null for fd %d", i);
    //         exit(1);
    //     }
    // }

    // check for pid file
    let running_pid = already_running(config);
    if let Some(running_pid) = running_pid {
        event!(Level::ERROR, "already running as pid {}", running_pid);
        exit(1);
    } else if let Err(err) = write_pidfile(config) {
        event!(
            Level::ERROR,
            ?err,
            "unable to write pid file {:?}",
            config.pid_file
        );
        exit(1);
    }
}

fn already_running(config: &Config) -> Option<i32> {
    let file = File::open(&config.pid_file).ok()?;

    let mut reader = BufReader::new(file);

    let mut line = String::with_capacity(5);
    let _r = reader.read_line(&mut line);

    let pid = line.parse::<i32>().ok()?;

    if 0 == unsafe { kill(pid, 0) } {
        return Some(pid);
    }

    None
}

#[expect(unused)]
fn mdns_reflector_shutdown(_signal: i32, cancellation_token: &CancellationToken) {
    cancellation_token.cancel();
}

fn write_pidfile(config: &Config) -> Result<(), eyre::Report> {
    let mut file = File::create(&config.pid_file)?;

    let pid = unsafe { getpid() };

    file.write_fmt(format_args!("{}", pid))?;

    Ok(())
}
