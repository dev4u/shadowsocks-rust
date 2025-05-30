//! Server Manager launchers

use std::{future::Future, net::IpAddr, path::PathBuf, process::ExitCode, time::Duration};

use clap::{Arg, ArgAction, ArgGroup, ArgMatches, Command, ValueHint, builder::PossibleValuesParser};
use futures::future::{self, Either};
use log::{info, trace};
use tokio::{
    self,
    runtime::{Builder, Runtime},
};

#[cfg(unix)]
use shadowsocks_service::config::ManagerServerMode;
use shadowsocks_service::{
    acl::AccessControl,
    config::{Config, ConfigType, ManagerConfig, ManagerServerHost},
    run_manager,
    shadowsocks::{
        config::{ManagerAddr, Mode},
        crypto::{CipherKind, available_ciphers},
        plugin::PluginConfig,
    },
};

#[cfg(feature = "logging")]
use crate::logging;
use crate::{
    config::{Config as ServiceConfig, RuntimeMode},
    error::{ShadowsocksError, ShadowsocksResult},
    monitor, vparser,
};

/// Defines command line options
pub fn define_command_line_options(mut app: Command) -> Command {
    app = app
        .arg(
            Arg::new("CONFIG")
                .short('c')
                .long("config")
                .num_args(1)
                .action(ArgAction::Set)
                .value_parser(clap::value_parser!(PathBuf))
                .value_hint(ValueHint::FilePath)
                .help("Shadowsocks configuration file (https://shadowsocks.org/doc/configs.html), the only required fields are \"manager_address\" and \"manager_port\". Servers defined will be created when process is started."),
        )
        .arg(
            Arg::new("UDP_ONLY")
                .short('u')
                .action(ArgAction::SetTrue)
                .conflicts_with("TCP_AND_UDP")
                .help("Server mode UDP_ONLY"),
        )
        .arg(
            Arg::new("TCP_AND_UDP")
                .short('U')
                .action(ArgAction::SetTrue)
                .help("Server mode TCP_AND_UDP"),
        )
        .arg(
            Arg::new("OUTBOUND_BIND_ADDR")
                .short('b')
                .long("outbound-bind-addr")
                .num_args(1)
                .action(ArgAction::Set)
                .alias("bind-addr")
                .value_parser(vparser::parse_ip_addr)
                .help("Bind address, outbound socket will bind this address"),
        )
        .arg(
            Arg::new("OUTBOUND_BIND_INTERFACE")
                .long("outbound-bind-interface")
                .num_args(1)
                .action(ArgAction::Set)
                .help("Set SO_BINDTODEVICE / IP_BOUND_IF / IP_UNICAST_IF option for outbound socket"),
        )
        .arg(Arg::new("SERVER_HOST").short('s').long("server-host").num_args(1).action(ArgAction::Set).value_parser(vparser::parse_manager_server_host).help("Host name or IP address of your remote server"))
        .arg(
            Arg::new("MANAGER_ADDR")
                .long("manager-addr")
                .num_args(1)
                .action(ArgAction::Set)
                .alias("manager-address")
                .value_parser(vparser::parse_manager_addr)
                .help("ShadowSocks Manager (ssmgr) address, could be ip:port, domain:port or /path/to/unix.sock"),
        )
        .group(ArgGroup::new("SERVER_CONFIG").arg("MANAGER_ADDR"))
        .arg(Arg::new("ENCRYPT_METHOD").short('m').long("encrypt-method").num_args(1).action(ArgAction::Set).value_parser(PossibleValuesParser::new(available_ciphers())).help("Default encryption method"))
        .arg(Arg::new("TIMEOUT").long("timeout").num_args(1).action(ArgAction::Set).value_parser(clap::value_parser!(u64)).help("Default timeout seconds for TCP relay"))
        .arg(
            Arg::new("PLUGIN")
                .long("plugin")
                .num_args(1)
                .action(ArgAction::Set)
                .value_hint(ValueHint::CommandName)
                .help("Default SIP003 (https://shadowsocks.org/doc/sip003.html) plugin"),
        )
        .arg(
            Arg::new("PLUGIN_MODE")
                .long("plugin-mode")
                .num_args(1)
                .action(ArgAction::Set)
                .requires("PLUGIN")
                .help("SIP003/SIP003u plugin mode, must be one of `tcp_only` (default), `udp_only` and `tcp_and_udp`"),
        )
        .arg(
            Arg::new("PLUGIN_OPT")
                .long("plugin-opts")
                .num_args(1)
                .action(ArgAction::Set)
                .requires("PLUGIN")
                .help("Default SIP003 plugin options"),
        ).arg(Arg::new("ACL").long("acl").num_args(1).action(ArgAction::Set).value_hint(ValueHint::FilePath).help("Path to ACL (Access Control List)"))
        .arg(Arg::new("DNS").long("dns").num_args(1).action(ArgAction::Set).help("DNS nameservers, formatted like [(tcp|udp)://]host[:port][,host[:port]]..., or unix:///path/to/dns, or predefined keys like \"google\", \"cloudflare\""))
        .arg(Arg::new("DNS_CACHE_SIZE").long("dns-cache-size").num_args(1).action(ArgAction::Set).value_parser(clap::value_parser!(usize)).help("DNS cache size in number of records. Works when trust-dns DNS backend is used."))
        .arg(Arg::new("TCP_NO_DELAY").long("tcp-no-delay").alias("no-delay").action(ArgAction::SetTrue).help("Set TCP_NODELAY option for sockets"))
        .arg(Arg::new("TCP_FAST_OPEN").long("tcp-fast-open").alias("fast-open").action(ArgAction::SetTrue).help("Enable TCP Fast Open (TFO)"))
        .arg(Arg::new("TCP_KEEP_ALIVE").long("tcp-keep-alive").num_args(1).action(ArgAction::Set).value_parser(clap::value_parser!(u64)).help("Set TCP keep alive timeout seconds"))
        .arg(Arg::new("TCP_MULTIPATH").long("tcp-multipath").alias("mptcp").action(ArgAction::SetTrue).help("Enable Multipath-TCP (MPTCP)"))
        .arg(Arg::new("UDP_TIMEOUT").long("udp-timeout").num_args(1).action(ArgAction::Set).value_parser(clap::value_parser!(u64)).help("Timeout seconds for UDP relay"))
        .arg(Arg::new("UDP_MAX_ASSOCIATIONS").long("udp-max-associations").num_args(1).action(ArgAction::Set).value_parser(clap::value_parser!(usize)).help("Maximum associations to be kept simultaneously for UDP relay"))
        .arg(Arg::new("INBOUND_SEND_BUFFER_SIZE").long("inbound-send-buffer-size").num_args(1).action(ArgAction::Set).value_parser(clap::value_parser!(u32)).help("Set inbound sockets' SO_SNDBUF option"))
        .arg(Arg::new("INBOUND_RECV_BUFFER_SIZE").long("inbound-recv-buffer-size").num_args(1).action(ArgAction::Set).value_parser(clap::value_parser!(u32)).help("Set inbound sockets' SO_RCVBUF option"))
        .arg(Arg::new("OUTBOUND_SEND_BUFFER_SIZE").long("outbound-send-buffer-size").num_args(1).action(ArgAction::Set).value_parser(clap::value_parser!(u32)).help("Set outbound sockets' SO_SNDBUF option"))
        .arg(Arg::new("OUTBOUND_RECV_BUFFER_SIZE").long("outbound-recv-buffer-size").num_args(1).action(ArgAction::Set).value_parser(clap::value_parser!(u32)).help("Set outbound sockets' SO_RCVBUF option"))
        .arg(
            Arg::new("IPV6_FIRST")
                .short('6')
                .action(ArgAction::SetTrue)
                .help("Resolve hostname to IPv6 address first"),
        );

    #[cfg(feature = "logging")]
    {
        app = app
            .arg(
                Arg::new("VERBOSE")
                    .short('v')
                    .action(ArgAction::Count)
                    .help("Set log level"),
            )
            .arg(
                Arg::new("LOG_WITHOUT_TIME")
                    .long("log-without-time")
                    .action(ArgAction::SetTrue)
                    .help("Log without datetime prefix"),
            )
            .arg(
                Arg::new("LOG_CONFIG")
                    .long("log-config")
                    .num_args(1)
                    .action(ArgAction::Set)
                    .value_parser(clap::value_parser!(PathBuf))
                    .value_hint(ValueHint::FilePath)
                    .help("log4rs configuration file"),
            );
    }

    #[cfg(unix)]
    {
        app = app
            .arg(
                Arg::new("DAEMONIZE")
                    .short('d')
                    .long("daemonize")
                    .action(ArgAction::SetTrue)
                    .help("Daemonize"),
            )
            .arg(
                Arg::new("DAEMONIZE_PID_PATH")
                    .long("daemonize-pid")
                    .num_args(1)
                    .action(ArgAction::Set)
                    .value_parser(clap::value_parser!(PathBuf))
                    .value_hint(ValueHint::FilePath)
                    .help("File path to store daemonized process's PID"),
            )
            .arg(
                Arg::new("MANAGER_SERVER_MODE")
                    .long("manager-server-mode")
                    .num_args(1)
                    .action(ArgAction::Set)
                    .value_parser(vparser::parse_manager_server_mode)
                    // .possible_values(["builtin", "standalone"])
                    .help("Servers mode: builtin (default) or standalone"),
            )
            .arg(
                Arg::new("MANAGER_SERVER_WORKING_DIRECTORY")
                    .long("manager-server-working-directory")
                    .num_args(1)
                    .action(ArgAction::Set)
                    .value_parser(clap::value_parser!(PathBuf))
                    .value_hint(ValueHint::DirPath)
                    .help("Folder for putting servers' configuration and pid files, default is current directory"),
            );
    }

    #[cfg(all(unix, not(target_os = "android")))]
    {
        app = app.arg(
            Arg::new("NOFILE")
                .short('n')
                .long("nofile")
                .num_args(1)
                .action(ArgAction::Set)
                .value_parser(clap::value_parser!(u64))
                .help("Set RLIMIT_NOFILE with both soft and hard limit"),
        );
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        app = app.arg(
            Arg::new("OUTBOUND_FWMARK")
                .long("outbound-fwmark")
                .num_args(1)
                .action(ArgAction::Set)
                .value_parser(clap::value_parser!(u32))
                .help("Set SO_MARK option for outbound sockets"),
        );
    }

    #[cfg(target_os = "freebsd")]
    {
        app = app.arg(
            Arg::new("OUTBOUND_USER_COOKIE")
                .long("outbound-user-cookie")
                .num_args(1)
                .action(ArgAction::Set)
                .value_parser(clap::value_parser!(u32))
                .help("Set SO_USER_COOKIE option for outbound sockets"),
        );
    }

    #[cfg(feature = "multi-threaded")]
    {
        app = app
            .arg(
                Arg::new("SINGLE_THREADED")
                    .long("single-threaded")
                    .action(ArgAction::SetTrue)
                    .help("Run the program all in one thread"),
            )
            .arg(
                Arg::new("WORKER_THREADS")
                    .long("worker-threads")
                    .num_args(1)
                    .action(ArgAction::Set)
                    .value_parser(clap::value_parser!(usize))
                    .help("Sets the number of worker threads the `Runtime` will use"),
            );
    }

    #[cfg(unix)]
    {
        app = app.arg(
            Arg::new("USER")
                .long("user")
                .short('a')
                .num_args(1)
                .action(ArgAction::Set)
                .value_hint(ValueHint::Username)
                .help("Run as another user"),
        );
    }

    app
}

/// Create `Runtime` and `main` entry
pub fn create(matches: &ArgMatches) -> ShadowsocksResult<(Runtime, impl Future<Output = ShadowsocksResult> + use<>)> {
    let (config, runtime) = {
        let config_path_opt = matches.get_one::<PathBuf>("CONFIG").cloned().or_else(|| {
            if !matches.contains_id("SERVER_CONFIG") {
                match crate::config::get_default_config_path("manager.json") {
                    None => None,
                    Some(p) => {
                        println!("loading default config {p:?}");
                        Some(p)
                    }
                }
            } else {
                None
            }
        });

        let mut service_config = match config_path_opt {
            Some(ref config_path) => ServiceConfig::load_from_file(config_path)
                .map_err(|err| ShadowsocksError::LoadConfigFailure(format!("loading config {config_path:?}, {err}")))?,
            None => ServiceConfig::default(),
        };
        service_config.set_options(matches);

        #[cfg(feature = "logging")]
        match service_config.log.config_path {
            Some(ref path) => {
                logging::init_with_file(path);
            }
            None => {
                logging::init_with_config("sslocal", &service_config.log);
            }
        }

        trace!("{:?}", service_config);

        let mut config = match config_path_opt {
            Some(cpath) => Config::load_from_file(&cpath, ConfigType::Manager)
                .map_err(|err| ShadowsocksError::LoadConfigFailure(format!("loading config {cpath:?}, {err}")))?,
            None => Config::new(ConfigType::Manager),
        };

        if matches.get_flag("TCP_NO_DELAY") {
            config.no_delay = true;
        }

        if matches.get_flag("TCP_FAST_OPEN") {
            config.fast_open = true;
        }

        if let Some(keep_alive) = matches.get_one::<u64>("TCP_KEEP_ALIVE") {
            config.keep_alive = Some(Duration::from_secs(*keep_alive));
        }

        if matches.get_flag("TCP_MULTIPATH") {
            config.mptcp = true;
        }

        #[cfg(any(target_os = "linux", target_os = "android"))]
        if let Some(mark) = matches.get_one::<u32>("OUTBOUND_FWMARK") {
            config.outbound_fwmark = Some(*mark);
        }

        #[cfg(target_os = "freebsd")]
        if let Some(user_cookie) = matches.get_one::<u32>("OUTBOUND_USER_COOKIE") {
            config.outbound_user_cookie = Some(*user_cookie);
        }

        if let Some(iface) = matches.get_one::<String>("OUTBOUND_BIND_INTERFACE").cloned() {
            config.outbound_bind_interface = Some(iface);
        }

        if let Some(addr) = matches.get_one::<ManagerAddr>("MANAGER_ADDR").cloned() {
            match config.manager {
                Some(ref mut manager_config) => {
                    manager_config.addr = addr;
                }
                _ => {
                    config.manager = Some(ManagerConfig::new(addr));
                }
            }
        }

        #[cfg(all(unix, not(target_os = "android")))]
        match matches.get_one::<u64>("NOFILE") {
            Some(nofile) => config.nofile = Some(*nofile),
            None => {
                if config.nofile.is_none() {
                    crate::sys::adjust_nofile();
                }
            }
        }

        if let Some(ref mut manager_config) = config.manager {
            if let Some(m) = matches.get_one::<String>("ENCRYPT_METHOD").cloned() {
                manager_config.method = Some(m.parse::<CipherKind>().expect("method"));
            }

            if let Some(t) = matches.get_one::<u64>("TIMEOUT") {
                manager_config.timeout = Some(Duration::from_secs(*t));
            }

            if let Some(sh) = matches.get_one::<ManagerServerHost>("SERVER_HOST").cloned() {
                manager_config.server_host = sh;
            }

            if let Some(p) = matches.get_one::<String>("PLUGIN").cloned() {
                manager_config.plugin = Some(PluginConfig {
                    plugin: p,
                    plugin_opts: matches.get_one::<String>("PLUGIN_OPT").cloned(),
                    plugin_args: Vec::new(),
                    plugin_mode: matches
                        .get_one::<String>("PLUGIN_MODE")
                        .map(|x| {
                            x.parse::<Mode>()
                                .expect("plugin-mode must be one of `tcp_only` (default), `udp_only` and `tcp_and_udp`")
                        })
                        .unwrap_or(Mode::TcpOnly),
                });
            }

            #[cfg(unix)]
            if let Some(server_mode) = matches.get_one::<ManagerServerMode>("MANAGER_SERVER_MODE").cloned() {
                manager_config.server_mode = server_mode;
            }

            #[cfg(unix)]
            if let Some(server_working_directory) =
                matches.get_one::<PathBuf>("MANAGER_SERVER_WORKING_DIRECTORY").cloned()
            {
                manager_config.server_working_directory = server_working_directory;
            }
        }

        // Overrides
        if matches.get_flag("UDP_ONLY") {
            if let Some(ref mut m) = config.manager {
                m.mode = Mode::UdpOnly;
            }
        }

        if matches.get_flag("TCP_AND_UDP") {
            if let Some(ref mut m) = config.manager {
                m.mode = Mode::TcpAndUdp;
            }
        }

        if let Some(acl_file) = matches.get_one::<String>("ACL") {
            let acl = AccessControl::load_from_file(acl_file)
                .map_err(|err| ShadowsocksError::LoadAclFailure(format!("loading ACL \"{acl_file}\", {err}")))?;
            config.acl = Some(acl);
        }

        if let Some(dns) = matches.get_one::<String>("DNS") {
            config.set_dns_formatted(dns).expect("dns");
        }

        if let Some(dns_cache_size) = matches.get_one::<usize>("DNS_CACHE_SIZE") {
            config.dns_cache_size = Some(*dns_cache_size);
        }

        if matches.get_flag("IPV6_FIRST") {
            config.ipv6_first = true;
        }

        if let Some(udp_timeout) = matches.get_one::<u64>("UDP_TIMEOUT") {
            config.udp_timeout = Some(Duration::from_secs(*udp_timeout));
        }

        if let Some(udp_max_assoc) = matches.get_one::<usize>("UDP_MAX_ASSOCIATIONS") {
            config.udp_max_associations = Some(*udp_max_assoc);
        }

        if let Some(bs) = matches.get_one::<u32>("INBOUND_SEND_BUFFER_SIZE") {
            config.inbound_send_buffer_size = Some(*bs);
        }
        if let Some(bs) = matches.get_one::<u32>("INBOUND_RECV_BUFFER_SIZE") {
            config.inbound_recv_buffer_size = Some(*bs);
        }
        if let Some(bs) = matches.get_one::<u32>("OUTBOUND_SEND_BUFFER_SIZE") {
            config.outbound_send_buffer_size = Some(*bs);
        }
        if let Some(bs) = matches.get_one::<u32>("OUTBOUND_RECV_BUFFER_SIZE") {
            config.outbound_recv_buffer_size = Some(*bs);
        }

        if let Some(bind_addr) = matches.get_one::<IpAddr>("OUTBOUND_BIND_ADDR") {
            config.outbound_bind_addr = Some(*bind_addr);
        }

        // DONE reading options

        config.manager.as_ref().ok_or_else(|| {
            ShadowsocksError::InsufficientParams(
                "missing `manager_address`, consider specifying it by --manager-address command line option, \
                    or \"manager_address\" and \"manager_port\" keys in configuration file"
                    .to_string(),
            )
        })?;

        config
            .check_integrity()
            .map_err(|err| ShadowsocksError::LoadConfigFailure(format!("config integrity check failed, {err}")))?;

        #[cfg(unix)]
        if matches.get_flag("DAEMONIZE") || matches.get_raw("DAEMONIZE_PID_PATH").is_some() {
            use crate::daemonize;
            daemonize::daemonize(matches.get_one::<PathBuf>("DAEMONIZE_PID_PATH"));
        }

        #[cfg(unix)]
        if let Some(uname) = matches.get_one::<String>("USER") {
            crate::sys::run_as_user(uname).map_err(|err| {
                ShadowsocksError::InsufficientParams(format!("failed to change as user, error: {err}"))
            })?;
        }

        info!("shadowsocks manager {} build {}", crate::VERSION, crate::BUILD_TIME);

        let mut builder = match service_config.runtime.mode {
            RuntimeMode::SingleThread => Builder::new_current_thread(),
            #[cfg(feature = "multi-threaded")]
            RuntimeMode::MultiThread => {
                let mut builder = Builder::new_multi_thread();
                if let Some(worker_threads) = service_config.runtime.worker_count {
                    builder.worker_threads(worker_threads);
                }

                builder
            }
        };

        let runtime = builder.enable_all().build().expect("create tokio Runtime");

        (config, runtime)
    };

    let main_fut = async move {
        let abort_signal = monitor::create_signal_monitor();
        let server = run_manager(config);

        tokio::pin!(abort_signal);
        tokio::pin!(server);

        match future::select(server, abort_signal).await {
            // Server future resolved without an error. This should never happen.
            Either::Left((Ok(..), ..)) => Err(ShadowsocksError::ServerExitUnexpectedly(
                "server exited unexpectedly".to_owned(),
            )),
            // Server future resolved with error, which are listener errors in most cases
            Either::Left((Err(err), ..)) => Err(ShadowsocksError::ServerAborted(format!("server aborted with {err}"))),
            // The abort signal future resolved. Means we should just exit.
            Either::Right(_) => Ok(()),
        }
    };

    Ok((runtime, main_fut))
}

/// Program entrance `main`
#[inline]
pub fn main(matches: &ArgMatches) -> ExitCode {
    match create(matches).and_then(|(runtime, main_fut)| runtime.block_on(main_fut)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{err}");
            err.exit_code().into()
        }
    }
}

#[cfg(test)]
mod test {
    use clap::Command;

    #[test]
    fn verify_manager_command() {
        let mut app = Command::new("shadowsocks")
            .version(crate::VERSION)
            .about("A fast tunnel proxy that helps you bypass firewalls. (https://shadowsocks.org)");
        app = super::define_command_line_options(app);
        app.debug_assert();
    }
}
