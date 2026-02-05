mod build_env;
mod cli;
mod reflector;
mod signal_handlers;
mod sockets;
mod unix;

use std::env::{self, VarError};
use std::ffi::CString;
use std::fs::{File, remove_file};
use std::io::{BufRead as _, BufReader, Write as _};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::process;
use std::sync::Arc;
use std::time::Duration;

use cli::{Config, parse_cli};
use color_eyre::config::HookBuilder;
use color_eyre::eyre;
use libc::{SIG_IGN, SIGCHLD, SIGHUP, chdir, fork, getpid, kill, pid_t, setsid, signal, umask};
use reflector::reflect;
use sockets::{create_recv_sock, create_send_sock};
use syslog_tracing::{Facility, Options};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::{Level, event};
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::{EnvFilter, Layer as _, Registry, reload};

use crate::build_env::get_build_env;

// TODO this should come from cargo
const PACKAGE: &str = env!("CARGO_PKG_NAME");
#[expect(
    clippy::decimal_literal_representation,
    reason = "This is the number used everywhere"
)]
const PACKET_SIZE: usize = 65536;
const MDNS_ADDR: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
const MDNS_PORT: u16 = 5353;

const BROADCAST_MDNS: SocketAddr = SocketAddr::V4(SocketAddrV4::new(MDNS_ADDR, MDNS_PORT));

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn build_default_filter() -> EnvFilter {
    EnvFilter::builder()
        .parse(format!("INFO,{}=TRACE", env!("CARGO_CRATE_NAME")))
        .expect("Default filter should always work")
}

fn init_tracing() -> std::result::Result<
    tracing_subscriber::reload::Handle<
        std::boxed::Box<
            dyn tracing_subscriber::Layer<tracing_subscriber::Registry>
                + std::marker::Send
                + std::marker::Sync,
        >,
        Registry,
    >,
    color_eyre::Report,
> {
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

    let (stdout_layer, handle) = reload::Layer::new(tracing_subscriber::fmt::layer().boxed());

    let layers = vec![
        #[cfg(feature = "tokio-console")]
        console_subscriber::ConsoleLayer::builder().spawn().boxed(),
        stdout_layer.with_filter(filter).boxed(),
        tracing_error::ErrorLayer::default().boxed(),
    ];

    tracing_subscriber::registry().with(layers).try_init()?;

    filter_parsing_error.map_or(Ok(handle), Err)
}

fn print_header() {
    const NAME: &str = env!("CARGO_PKG_NAME");
    const VERSION: &str = env!("CARGO_PKG_VERSION");

    let build_env = get_build_env();

    println!(
        "{} v{} - built for {} ({})",
        NAME,
        VERSION,
        build_env.get_target(),
        build_env.get_target_cpu().unwrap_or("base cpu variant"),
    );
}

fn main() -> Result<(), eyre::Report> {
    HookBuilder::default()
        .capture_span_trace_by_default(true)
        .display_location_section(true)
        .display_env_section(false)
        .install()?;

    let handle = init_tracing()?;

    let (config, interfaces) = parse_cli().inspect_err(|error| {
        // this prints the error in color and exits
        // can't do anything else until
        // https://github.com/clap-rs/clap/issues/2914
        // is merged in
        if let Some(clap_error) = error.downcast_ref::<clap::error::Error>() {
            clap_error.exit();
        }
    })?;

    print_header();

    let cancellation_token = CancellationToken::new();

    if config.foreground {
        // check for pid file when running in foreground
        let running_pid = already_running(&config);

        if let Some(running_pid) = running_pid {
            event!(Level::ERROR, "already running as pid {}", running_pid);
            return Ok(());
        }
    } else {
        daemonize(&config);

        let identity = CString::new(PACKAGE).unwrap();

        let syslog = syslog_tracing::Syslog::new(
            identity,
            Options::LOG_PID | Options::LOG_CONS,
            Facility::Daemon,
        )
        .unwrap();

        // switch to syslog logging
        handle.reload(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .without_time()
                .with_level(false)
                .with_writer(syslog)
                .boxed(),
        )?;
    }

    // initialize the runtime
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    if let Err(error) = rt.block_on(start_tasks(
        cancellation_token,
        Arc::new(config),
        interfaces,
    )) {
        event!(Level::ERROR, ?error);
    }

    Ok(())
}

async fn start_tasks(
    cancellation_token: CancellationToken,
    config: Arc<Config>,
    interfaces: Vec<String>,
) -> Result<(), eyre::Report> {
    // create receiving socket
    let server_socket = match create_recv_sock() {
        Ok(server_socket) => server_socket,
        Err(error) => {
            event!(
                Level::ERROR,
                ?error,
                "Unable to create send socket on interface"
            );

            return Ok(());
        },
    };

    let mut sockets = Vec::with_capacity(interfaces.len());

    // create sending sockets
    for interface in interfaces {
        let send_socket = match create_send_sock(&server_socket, interface) {
            Ok(send_socket) => send_socket,
            Err(error) => {
                event!(
                    Level::ERROR,
                    ?error,
                    "Unable to create send socket on interface"
                );

                return Ok(());
            },
        };

        sockets.push(send_socket);
    }

    let task_tracker = TaskTracker::new();

    {
        let cancellation_token = cancellation_token.clone();

        task_tracker.spawn(reflect(
            server_socket,
            sockets,
            Arc::clone(&config),
            cancellation_token,
        ));
    }

    // now we wait forever for either
    // * SIGTERM
    // * ctrl + c (SIGINT)
    // * a message on the shutdown channel, sent either by the server task or
    // another task when they complete (which means they failed)
    tokio::select! {
        result = signal_handlers::wait_for_sigterm() => {
            if let Err(error) = result {
                event!(Level::ERROR, ?error, "Failed to register SIGERM handler, aborting");
            } else {
                // we completed because ...
                event!(Level::WARN, "Sigterm detected, stopping all tasks");
            }
        },
        result = signal_handlers::wait_for_sigint() => {
            if let Err(error) = result {
                event!(Level::ERROR, ?error, "Failed to register CTRL+C handler, aborting");
            } else {
                // we completed because ...
                event!(Level::WARN, "CTRL+C detected, stopping all tasks");
            }
        },
        () = cancellation_token.cancelled() => {
            event!(Level::WARN, "Underlying task stopped, stopping all others tasks");
        },
    }

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
    if let Some(pid) = already_running(&config)
        && {
            // SAFETY: libc call
            pid == unsafe { getpid() }
        }
    {
        if let Err(error) = remove_file(&config.pid_file) {
            event!(
                Level::ERROR,
                ?error,
                pid_file = %config.pid_file.display(),
                "Failed to remove pid_file, manual deletion required"
            );
        } else {
            event!(Level::INFO, pid_file = %config.pid_file.display(), "pid_file cleaned up");
        }
    }

    event!(Level::INFO, "Goodbye");

    Ok(())
}

#[expect(clippy::exit, reason = "Daemonize failed, cleanup unneeded")]
fn daemonize(config: &Config) {
    // SAFETY: libc call
    let pid: pid_t = unsafe { fork() };

    if pid < 0 {
        let error = std::io::Error::last_os_error();
        event!(Level::ERROR, ?error, "fork()");

        process::exit(1);
    }

    // exit parent process
    if pid > 0 {
        process::exit(0);
    }

    // signals

    // SAFETY: libc call
    unsafe {
        signal(SIGCHLD, SIG_IGN);
    }

    // SAFETY: libc call
    unsafe {
        signal(SIGHUP, SIG_IGN);
    }

    // SAFETY: libc call
    unsafe {
        setsid();
    }

    // SAFETY: libc call
    unsafe {
        umask(0o0027);
    }

    // SAFETY: libc call
    unsafe {
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

        #[expect(clippy::exit, reason = "Daemonize failed, cleanup unneeded")]
        process::exit(1);
    } else if let Err(error) = write_pidfile(config) {
        event!(
            Level::ERROR,
            ?error,
            "unable to write pid file {:?}",
            config.pid_file
        );

        #[expect(clippy::exit, reason = "Daemonize failed, cleanup unneeded")]
        process::exit(1);
    } else {
        // Daemonize succesful
    }
}

fn already_running(config: &Config) -> Option<i32> {
    let file = File::open(&config.pid_file).ok()?;

    let mut reader = BufReader::new(file);

    let mut line = String::with_capacity(5);
    let _r = reader.read_line(&mut line);

    let pid = line.parse::<i32>().ok()?;

    // SAFETY: libc call
    if 0 == unsafe { kill(pid, 0) } {
        return Some(pid);
    }

    None
}

fn write_pidfile(config: &Config) -> Result<(), eyre::Report> {
    let mut file = File::create(&config.pid_file)?;

    // SAFETY: libc call
    let pid = unsafe { getpid() };

    file.write_fmt(format_args!("{}", pid))?;

    Ok(())
}
