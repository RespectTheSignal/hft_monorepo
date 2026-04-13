use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use tracing::{error, info, warn};

/// Per-client state: a channel sender for broadcasting messages.
struct Client {
    id: String,
    symbols: Option<Vec<String>>, // None = all symbols
    tx: mpsc::SyncSender<(Vec<u8>, Vec<u8>)>,
}

/// IPC server that distributes ZMQ data to local Unix socket clients.
pub struct IpcServer {
    clients: Arc<Mutex<Vec<Client>>>,
    socket_path: String,
}

impl IpcServer {
    pub fn new(socket_path: &str) -> Self {
        // Clean up stale socket file
        let _ = std::fs::remove_file(socket_path);

        let clients: Arc<Mutex<Vec<Client>>> = Arc::new(Mutex::new(Vec::new()));
        let path = socket_path.to_string();

        // Listener thread: accept new client connections
        let clients_clone = clients.clone();
        let path_clone = path.clone();
        std::thread::spawn(move || {
            accept_loop(&path_clone, clients_clone);
        });

        Self {
            clients,
            socket_path: path,
        }
    }

    /// Broadcast a message (topic + payload) to all connected clients.
    /// Returns number of clients that received the message.
    pub fn broadcast(&self, topic: &[u8], payload: &[u8]) -> usize {
        let mut clients = self.clients.lock().unwrap();
        let mut sent = 0;
        let mut disconnected = Vec::new();

        for (i, client) in clients.iter().enumerate() {
            // Symbol filter: if client registered specific symbols, check match
            if let Some(ref syms) = client.symbols {
                // Extract symbol from topic: "flipster_bookticker_{SYMBOL}"
                let topic_str = std::str::from_utf8(topic).unwrap_or("");
                let symbol = topic_str.rsplit('_').next().unwrap_or("");
                // Topic has format "flipster_bookticker_BTC-USDT-PERP" but the symbol
                // part after "flipster_bookticker_" may contain underscores...
                // Let's use the full topic suffix after the second underscore.
                let sym = topic_str
                    .find('_')
                    .and_then(|i| topic_str[i + 1..].find('_').map(|j| &topic_str[i + 1 + j + 1..]))
                    .unwrap_or(symbol);
                if !syms.iter().any(|s| s == sym) {
                    continue;
                }
            }

            if client.tx.try_send((topic.to_vec(), payload.to_vec())).is_err() {
                disconnected.push(i);
            } else {
                sent += 1;
            }
        }

        // Remove disconnected clients in reverse order
        for i in disconnected.into_iter().rev() {
            let removed = clients.remove(i);
            warn!("IPC client disconnected: {}", removed.id);
        }

        sent
    }

    pub fn client_count(&self) -> usize {
        self.clients.lock().unwrap().len()
    }

    pub fn socket_path(&self) -> &str {
        &self.socket_path
    }
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

fn accept_loop(path: &str, clients: Arc<Mutex<Vec<Client>>>) {
    let listener = match UnixListener::bind(path) {
        Ok(l) => l,
        Err(e) => {
            error!("IPC bind to {path} failed: {e}");
            return;
        }
    };
    info!("IPC server listening on {path}");

    for stream in listener.incoming() {
        match stream {
            Ok(conn) => {
                let clients = clients.clone();
                std::thread::spawn(move || {
                    handle_client(conn, clients);
                });
            }
            Err(e) => {
                error!("IPC accept error: {e}");
            }
        }
    }
}

fn handle_client(stream: UnixStream, clients: Arc<Mutex<Vec<Client>>>) {
    // Read registration line: "REGISTER:<process_id>:<symbol1>,<symbol2>,...\n"
    let peer = format!("{:?}", stream.peer_addr());
    let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
    let mut reg_line = String::new();

    if reader.read_line(&mut reg_line).is_err() {
        warn!("IPC client {peer}: failed to read registration");
        return;
    }

    let reg_line = reg_line.trim();
    let (client_id, symbols) = parse_registration(reg_line);
    let sym_desc = match &symbols {
        Some(s) => format!("{} symbols", s.len()),
        None => "all symbols".to_string(),
    };
    info!("IPC client registered: {client_id} ({sym_desc})");

    // Create per-client channel and writer thread
    let (tx, rx) = mpsc::sync_channel::<(Vec<u8>, Vec<u8>)>(8192);

    {
        let mut clients = clients.lock().unwrap();
        clients.push(Client {
            id: client_id.clone(),
            symbols,
            tx,
        });
    }

    // Writer loop: send framed messages to client
    // Format: [4B topic_len LE][topic][4B payload_len LE][payload]
    let mut writer = stream;
    for (topic, payload) in rx.iter() {
        let topic_len = (topic.len() as u32).to_le_bytes();
        let payload_len = (payload.len() as u32).to_le_bytes();

        if writer.write_all(&topic_len).is_err()
            || writer.write_all(&topic).is_err()
            || writer.write_all(&payload_len).is_err()
            || writer.write_all(&payload).is_err()
        {
            break;
        }
    }

    info!("IPC client writer exiting: {client_id}");
}

/// Parse "REGISTER:<process_id>:<symbol1>,<symbol2>,..." → (id, Option<Vec<symbols>>)
fn parse_registration(line: &str) -> (String, Option<Vec<String>>) {
    if !line.starts_with("REGISTER:") {
        return (line.to_string(), None);
    }

    let rest = &line["REGISTER:".len()..];
    let mut parts = rest.splitn(2, ':');
    let id = parts.next().unwrap_or("unknown").to_string();
    let symbols = parts.next().and_then(|s| {
        if s.is_empty() {
            None
        } else {
            Some(s.split(',').map(|sym| sym.trim().to_string()).collect())
        }
    });

    (id, symbols)
}
