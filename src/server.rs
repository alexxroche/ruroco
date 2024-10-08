use openssl::error::ErrorStack;
use openssl::pkey::Public;
use openssl::rsa::Rsa;
use openssl::version::version;
use std::fs::ReadDir;
use std::io::Write;
use std::net::{SocketAddr, UdpSocket};
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::{env, fs};

use crate::blocklist::Blocklist;
use crate::commander_data::CommanderData;
use crate::common::{error, get_socket_path, info, resolve_path, time, RSA_PADDING};
use crate::config_server::ConfigServer;

#[derive(Debug)]
pub struct Server {
    rsa: Rsa<Public>,
    socket: UdpSocket,
    address: String,
    encrypted_data: Vec<u8>,
    decrypted_data: Vec<u8>,
    socket_path: PathBuf,
    blocklist: Blocklist,
}

impl PartialEq for Server {
    fn eq(&self, other: &Self) -> bool {
        let self_address_split = self.address.split(':').next().unwrap_or("");
        let other_address_split = other.address.split(':').next().unwrap_or("");
        self_address_split == other_address_split
            && self.encrypted_data == other.encrypted_data
            && self.decrypted_data == other.decrypted_data
            && self.socket_path == other.socket_path
            && self.blocklist == other.blocklist
    }
}

struct DecodedData {
    deadline_ns: u128,
    now_ns: u128,
    command_name: String,
}

impl Server {
    pub fn create_from_path(path: PathBuf) -> Result<Server, String> {
        match fs::read_to_string(&path) {
            Err(e) => Err(format!("Could not read {path:?}: {e}")),
            Ok(config) => match toml::from_str::<ConfigServer>(&config) {
                Err(e) => Err(format!("Could not create TOML from {path:?}: {e}")),
                Ok(config) => Server::create(config),
            },
        }
    }

    pub fn create(config: ConfigServer) -> Result<Server, String> {
        let address = config.address;
        let config_dir = resolve_path(&config.config_dir);

        let pem_path = Self::get_pem_path(&config_dir)?;
        info(format!(
            "Creating server, loading public PEM from {pem_path:?}, using {} ...",
            version()
        ));

        let pem_data =
            fs::read(&pem_path).map_err(|e| format!("Could not read {pem_path:?}: {e}"))?;
        let rsa = Rsa::public_key_from_pem(&pem_data)
            .map_err(|e| format!("Could not load public key from {pem_path:?}: {e}"))?;

        let pid = std::process::id().to_string();
        let socket = match env::var("LISTEN_PID") {
            Ok(listen_pid) if listen_pid == pid => {
                info(String::from(
                    "env var LISTEN_PID was set to our PID, creating socket from raw fd ...",
                ));
                let fd: RawFd = 3;
                unsafe { UdpSocket::from_raw_fd(fd) }
            }
            Ok(_) => {
                info(format!(
                    "env var LISTEN_PID was set, but not to our PID, binding to {address}"
                ));
                UdpSocket::bind(&address)
                    .map_err(|e| format!("Could not UdpSocket bind {address:?}: {e}"))?
            }
            Err(_) => {
                info(format!("env var LISTEN_PID was not set, binding to {address}"));
                UdpSocket::bind(&address)
                    .map_err(|e| format!("Could not UdpSocket bind {address:?}: {e}"))?
            }
        };

        let rsa_size = rsa.size() as usize;
        let decrypted_data = vec![0; rsa_size];
        let encrypted_data = vec![0; rsa_size];

        Ok(Server {
            rsa,
            address,
            socket,
            decrypted_data,
            encrypted_data,
            socket_path: get_socket_path(&config_dir),
            blocklist: Blocklist::create(&config_dir),
        })
    }

    fn get_pem_path(config_dir: &PathBuf) -> Result<PathBuf, String> {
        let pem_files = Self::get_pem_files(config_dir);

        match pem_files.len() {
            0 => Err(format!("Could not find any .pem files in {config_dir:?}")),
            1 => Ok(pem_files.first().unwrap().clone()),
            other => Err(format!("Only one public PEM is supported, found {other}")),
        }
    }

    fn get_pem_files(config_dir: &PathBuf) -> Vec<PathBuf> {
        let entries: ReadDir = match fs::read_dir(config_dir) {
            Ok(entries) => entries,
            Err(e) => {
                error(format!("Error reading directory: {e}"));
                return vec![];
            }
        };

        entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.is_file() && path.extension().is_some() && path.extension().unwrap() == "pem"
            })
            .collect()
    }

    pub fn run(&mut self) -> Result<(), String> {
        info(format!("Running server on udp://{}", self.address));
        let rsa_size = self.rsa.size() as usize;
        loop {
            match self.receive() {
                Ok((count, src)) if count != rsa_size => {
                    error(format!("Invalid read count {count}, expected {rsa_size} from {src}"))
                }
                Ok((count, src)) => {
                    info(format!("Successfully received {count} bytes from {src}"));
                    match self.decrypt() {
                        Ok(count) => self.validate(count, src.ip().to_string()),
                        Err(e) => {
                            error(format!("Could not decrypt {:X?}: {e}", self.encrypted_data))
                        }
                    }
                }
                Err(e) => {
                    error(format!("Could not recv_from socket from udp://{}: {e}", self.address))
                }
            }

            self.encrypted_data = vec![0; rsa_size];
            self.decrypted_data = vec![0; rsa_size];
        }
    }

    fn receive(&mut self) -> std::io::Result<(usize, SocketAddr)> {
        self.socket.recv_from(&mut self.encrypted_data)
    }

    fn decrypt(&mut self) -> Result<usize, ErrorStack> {
        self.rsa.public_decrypt(&self.encrypted_data, &mut self.decrypted_data, RSA_PADDING)
    }

    fn validate(&mut self, count: usize, ip_str: String) {
        self.decrypted_data.truncate(count);
        match self.decode() {
            Ok(data) if data.now_ns > data.deadline_ns => error(format!(
                "Invalid data - now {} is after deadline {}",
                data.now_ns, data.deadline_ns
            )),
            Ok(data) if self.blocklist.is_blocked(data.deadline_ns) => {
                error(format!("Invalid data - deadline {} is on blocklist", data.deadline_ns))
            }
            Ok(data) => {
                info(format!(
                    "Successfully validated data - now {} is before deadline {}",
                    data.now_ns, data.deadline_ns
                ));

                self.send_command(CommanderData {
                    command_name: String::from(&data.command_name),
                    ip: ip_str,
                });
                self.update_block_list(&data);
            }
            Err(e) => error(format!("Could not decode data: {e}")),
        }
    }

    fn update_block_list(&mut self, data: &DecodedData) {
        self.blocklist.clean(data.now_ns);
        self.blocklist.add(data.deadline_ns);
        self.blocklist.save();
    }

    fn send_command(&self, data: CommanderData) {
        match self.write_to_socket(data) {
            Ok(_) => info("Successfully sent data to commander".to_string()),
            Err(e) => error(format!(
                "Could not send data to commander via socket {:?}: {e}",
                &self.socket_path
            )),
        }
    }

    fn write_to_socket(&self, data: CommanderData) -> Result<(), String> {
        let mut stream = UnixStream::connect(&self.socket_path)
            .map_err(|e| format!("Could not connect to socket {:?}: {e}", self.socket_path))?;

        let data_to_send = toml::to_string(&data).map_err(|e| {
            format!("Could not serialize commander data to socket {:?}: {e}", &data)
        })?;

        stream.write_all(data_to_send.as_bytes()).map_err(|e| {
            format!("Could not write {data_to_send} to socket {:?}: {e}", self.socket_path)
        })?;

        stream
            .flush()
            .map_err(|e| format!("Could not flush stream for {:?}: {e}", self.socket_path))?;
        Ok(())
    }

    fn decode(&self) -> Result<DecodedData, String> {
        let mut buffer = [0u8; 16];
        buffer.copy_from_slice(&self.decrypted_data[..16]);

        Ok(DecodedData {
            deadline_ns: u128::from_le_bytes(buffer),
            now_ns: time()?,
            command_name: String::from_utf8_lossy(&self.decrypted_data[16..]).to_string(),
        })
    }
}
