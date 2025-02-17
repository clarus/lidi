use clap::{Arg, Command};
use crossbeam_channel::{unbounded, SendError};
use diode::receive::{decoding, dispatch, reblock};
use diode::{protocol, sock_utils, udp};
use log::{error, info};
use raptorq::EncodingPacket;
use std::sync::Mutex;
use std::{
    env, fmt, io,
    net::{self, SocketAddr, UdpSocket},
    str::FromStr,
    thread,
    time::Duration,
};

struct Config {
    from_udp: SocketAddr,
    from_udp_mtu: u16,

    nb_clients: u16,

    nb_decoding_threads: u8,

    encoding_block_size: u64,
    repair_block_size: u32,
    flush_timeout: Duration,

    to_tcp: SocketAddr,
    abort_timeout: Duration,
    heartbeat: Duration,
}

impl Config {
    fn adjust(&mut self) {
        let oti =
            protocol::object_transmission_information(self.from_udp_mtu, self.encoding_block_size);

        let packet_size = protocol::packet_size(&oti);
        let nb_encoding_packets = protocol::nb_encoding_packets(&oti);
        let nb_repair_packets = protocol::nb_repair_packets(&oti, self.repair_block_size);

        self.encoding_block_size = nb_encoding_packets * packet_size as u64;
        self.repair_block_size = nb_repair_packets * packet_size as u32;
    }
}

fn command_args() -> Config {
    let args = Command::new(env!("CARGO_BIN_NAME"))
        .version(env!("CARGO_PKG_VERSION"))
        .arg(
            Arg::new("from_udp")
                .long("from_udp")
                .value_name("ip:port")
                .default_value("127.0.0.1:6000")
                .help("From where to read data"),
        )
        .arg(
            Arg::new("from_udp_mtu")
                .long("from_udp_mtu")
                .value_name("nb_bytes")
                .default_value("1500") // mtu
                .value_parser(clap::value_parser!(u16))
                .help("MTU of the incoming UDP link"),
        )
        .arg(
            Arg::new("nb_clients")
                .long("nb_clients")
                .value_name("nb")
                .default_value("2")
                .value_parser(clap::value_parser!(u16))
                .help("Number of simultaneous transfers"),
        )
        .arg(
            Arg::new("nb_decoding_threads")
                .long("nb_decoding_threads")
                .value_name("nb")
                .default_value("1")
                .value_parser(clap::value_parser!(u8))
                .help("Number of parallel RaptorQ decoding threads"),
        )
        .arg(
            Arg::new("encoding_block_size")
                .long("encoding_block_size")
                .value_name("nb_bytes")
                .default_value("60000") // (mtu * 40), optimal parameter -- to align with other size !
                .value_parser(clap::value_parser!(u64))
                .help("Size of RaptorQ block"),
        )
        .arg(
            Arg::new("repair_block_size")
                .long("repair_block_size")
                .value_name("ratior")
                .default_value("6000") // mtu * 4
                .value_parser(clap::value_parser!(u32))
                .help("Size of repair data in bytes"),
        )
        .arg(
            Arg::new("flush_timeout")
                .long("flush_timeout")
                .value_name("nb_milliseconds")
                .default_value("500")
                .value_parser(clap::value_parser!(u64))
                .help("Duration in milliseconds after resetting RaptorQ status"),
        )
        .arg(
            Arg::new("to_tcp")
                .long("to_tcp")
                .value_name("ip:port")
                .default_value("127.0.0.1:7000")
                .help("Where to send data"),
        )
        .arg(
            Arg::new("abort_timeout")
                .long("abort_timeout")
                .value_name("nb_seconds")
                .default_value("10")
                .value_parser(clap::value_parser!(u64))
                .help("Duration in seconds after a transfer without incoming data is aborted"),
        )
        .arg(
            Arg::new("heartbeat")
                .long("heartbeat")
                .value_name("nb_secs")
                .default_value("10")
                .value_parser(clap::value_parser!(u16))
                .help("Duration in seconds between heartbeat messages"),
        )
        .get_matches();

    let from_udp = SocketAddr::from_str(args.get_one::<String>("from_udp").expect("default"))
        .expect("invalid from_udp_parameter");
    let from_udp_mtu = *args.get_one::<u16>("from_udp_mtu").expect("default");
    let nb_clients = *args.get_one::<u16>("nb_clients").expect("default");
    let nb_decoding_threads = *args.get_one::<u8>("nb_decoding_threads").expect("default");
    let encoding_block_size = *args.get_one::<u64>("encoding_block_size").expect("default");
    let repair_block_size = *args.get_one::<u32>("repair_block_size").expect("default");
    let flush_timeout =
        Duration::from_millis(*args.get_one::<u64>("flush_timeout").expect("default"));
    let to_tcp = SocketAddr::from_str(args.get_one::<String>("to_tcp").expect("default"))
        .expect("invalid to_tcp parameter");
    let abort_timeout =
        Duration::from_secs(*args.get_one::<u64>("abort_timeout").expect("default"));
    let heartbeat = *args.get_one::<u16>("heartbeat").expect("default");

    Config {
        from_udp,
        from_udp_mtu,
        nb_clients,
        nb_decoding_threads,
        encoding_block_size,
        repair_block_size,
        flush_timeout,
        to_tcp,
        abort_timeout,
        heartbeat: Duration::from_secs(heartbeat as u64),
    }
}

enum Error {
    Io(io::Error),
    AddrParseError(net::AddrParseError),
    Crossbeam(SendError<Vec<EncodingPacket>>),
}

impl fmt::Display for Error {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        match self {
            Self::Io(e) => write!(fmt, "I/O error: {e}"),
            Self::AddrParseError(e) => write!(fmt, "address parse error: {e}"),
            Self::Crossbeam(e) => write!(fmt, "crossbeam send error: {e}"),
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<net::AddrParseError> for Error {
    fn from(e: net::AddrParseError) -> Self {
        Self::AddrParseError(e)
    }
}

impl From<SendError<Vec<EncodingPacket>>> for Error {
    fn from(e: SendError<Vec<EncodingPacket>>) -> Self {
        Self::Crossbeam(e)
    }
}

fn main_loop(config: Config) -> Result<(), Error> {
    let (decoding_sendq, decoding_recvq) = unbounded::<protocol::Message>();
    let (reblock_sendq, reblock_recvq) = unbounded::<(u8, Vec<EncodingPacket>)>();
    let (udp_sendq, udp_recvq) = unbounded::<Vec<EncodingPacket>>();

    let dispatch_config = dispatch::Config {
        nb_multiplex: config.nb_clients,
        to_tcp: config.to_tcp,
        to_tcp_buffer_size: config.encoding_block_size as usize
            - protocol::Message::serialize_overhead(),
        abort_timeout: config.abort_timeout,
        heartbeat: config.heartbeat,
    };

    thread::Builder::new()
        .name("diode-dispatch".to_string())
        .spawn(move || dispatch::new(dispatch_config, decoding_recvq))
        .expect("thread spawn");

    let object_transmission_info =
        protocol::object_transmission_information(config.from_udp_mtu, config.encoding_block_size);

    let decoding_config = decoding::Config {
        object_transmission_info,
    };

    let reblock_config = reblock::Config {
        object_transmission_info,
        repair_block_size: config.repair_block_size,
        flush_timeout: config.flush_timeout,
    };

    info!(
        "sending TCP traffic to {} with abort timeout of {} second(s) and {} simultaneous transfers",
        config.to_tcp,
        config.abort_timeout.as_secs(),
        config.nb_clients,
    );

    let max_messages = protocol::nb_encoding_packets(&object_transmission_info) as u16
        + protocol::nb_repair_packets(&object_transmission_info, config.repair_block_size) as u16;

    info!("listening for UDP packets at {}", config.from_udp);
    let socket = UdpSocket::bind(config.from_udp)?;
    sock_utils::set_socket_recv_buffer_size(&socket, i32::MAX)?;
    let sock_buffer_size = sock_utils::get_socket_recv_buffer_size(&socket)?;
    log::info!("UDP socket receive buffer size set to {sock_buffer_size}");
    if (sock_buffer_size as u64)
        < 2 * (config.encoding_block_size + config.repair_block_size as u64)
    {
        log::warn!("UDP socket recv buffer may be too small to achieve optimal performances");
        log::warn!("Please review the kernel parameters using sysctl");
    }

    let mut udp_messages = udp::UdpMessages::new_receiver(
        socket,
        usize::from(max_messages),
        usize::from(config.from_udp_mtu),
    );

    let block_to_receive = Mutex::new(0);

    thread::scope(|scope| {
        thread::Builder::new()
            .name("diode-reblock".to_string())
            .spawn_scoped(scope, || {
                reblock::new(
                    &reblock_config,
                    &block_to_receive,
                    &udp_recvq,
                    &reblock_sendq,
                )
            })
            .expect("thread spawn");

        for i in 0..config.nb_decoding_threads {
            thread::Builder::new()
                .name(format!("diode-decoding_{i}"))
                .spawn_scoped(scope, || {
                    decoding::new(
                        &decoding_config,
                        &block_to_receive,
                        &reblock_recvq,
                        &decoding_sendq,
                    )
                })
                .expect("thread spawn");
        }

        loop {
            let packets = udp_messages.recv_mmsg()?.map(EncodingPacket::deserialize);
            udp_sendq.send(packets.collect())?;
        }
    })
}

fn main() {
    let mut config = command_args();

    init_logger();

    config.adjust();

    if let Err(e) = main_loop(config) {
        error!("failed to launch main_loop: {e}");
    }
}

fn init_logger() {
    if env::var("RUST_LOG").is_ok() {
        simple_logger::init_with_env()
    } else {
        simple_logger::init_with_level(log::Level::Info)
    }
    .expect("logger initialization")
}
