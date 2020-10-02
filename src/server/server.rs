use crossbeam_channel::{Receiver, Sender, unbounded};
use igd::{search_gateway, PortMappingProtocol};
use local_ipaddress;
use serde_json::{Value};
use thread::sleep;
use std::{io::{Read, Write}, net::IpAddr, net::TcpStream, thread, time::Duration};
use std::net::{TcpListener, Ipv4Addr, SocketAddrV4};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering::SeqCst};
use std::{str::FromStr};
use super::{process_message, util::{ReceiveData, TransferClient}};

pub enum PortForwardResult {
    GatewayNotFound,
    LocalAddrNotFound,
    AddPortError
}

pub struct PartialReader {
    buffer: Vec<u8>,
}

impl PartialReader {
    pub fn new() -> Self {
        Self {
            buffer: Vec::new()
        }
    }

    pub fn try_read_string(&mut self, buf: &[u8]) -> Option<String> {
        self.buffer.extend_from_slice(&buf);

        if let Some(index) = self.buffer.iter().position(|&x| x == 0x0a) {
            let result_string = String::from_utf8(self.buffer[0..index].to_vec()).unwrap();
            self.buffer.drain(0..index + 1);
            return Some(result_string);
        } else {
            return None
        }
    }
}

struct Client {
    stream: TcpStream,
    reader: PartialReader,
    address: IpAddr
}

struct TransferStruct {
    // Internal array of receivers to send receive data from clients
    clients: Vec<Client>,
    // Internally receive data to send to clients
    client_rx: Receiver<Value>,
    // Send data to app to receive client data
    server_tx: Sender<ReceiveData>,
}

pub struct Server {
    number_connections: Arc<AtomicU16>,
    port_error: Option<PortForwardResult>,
    should_stop: Arc<AtomicBool>,

    transfer: Option<Arc<Mutex<TransferStruct>>>,

    // Send data to clients
    client_tx: Sender<Value>,
    // Internally receive data to send to clients
    client_rx: Receiver<Value>,

    // Send data to app to receive client data
    server_tx: Sender<ReceiveData>,
    // Recieve data from clients
    server_rx: Receiver<ReceiveData>,
}

impl Server {
    pub fn new() -> Self  {
        let (client_tx, client_rx) = unbounded();
        let (server_tx, server_rx) = unbounded();

        return Self {
            number_connections: Arc::new(AtomicU16::new(0)),
            should_stop: Arc::new(AtomicBool::new(false)),
            port_error: None,
            transfer: None,
            client_rx, client_tx, server_rx, server_tx
        }
    }

    fn port_forward(&self, port: u16) -> Result<(), PortForwardResult> {
        let gateway = search_gateway(Default::default());
        if !gateway.is_ok() {return Err(PortForwardResult::GatewayNotFound)}

        let local_addr = local_ipaddress::get();
        if !local_addr.is_some() {return Err(PortForwardResult::LocalAddrNotFound)}
        let local_addr = Ipv4Addr::from_str(local_addr.unwrap().as_str()).unwrap();

        let result = gateway.unwrap().add_port(PortMappingProtocol::TCP, port, SocketAddrV4::new(local_addr, port), 86400, "YourControl");
        if result.is_err() {return Err(PortForwardResult::AddPortError)}

        Ok(())
    }

    pub fn start(&mut self, is_ipv6: bool, port: u16) -> Result<(), std::io::Error> {
        // Attempt to port forward
        if let Err(e) = self.port_forward(port) {
            self.port_error = Some(e);
        }

        // Start listening for connections
        let bind_ip = if is_ipv6 {"::"} else {"0.0.0.0"};
        let listener = match TcpListener::bind(format!("{}:{}", bind_ip, port)) {
            Ok(listener) => listener,
            Err(n) => {return Err(n)}
        };

        // Needed to stop the server
        listener.set_nonblocking(true).ok();

        // to be used in run()
        self.transfer = Some(Arc::new(Mutex::new(
            TransferStruct {
                clients: Vec::new(),
                client_rx: self.client_rx.clone(), 
                server_tx: self.server_tx.clone()
            }
        )));

        let transfer = self.transfer.as_ref().unwrap().clone();
        let number_connections = self.number_connections.clone();
        let should_stop = self.should_stop.clone();

        // Listen for new connections
        thread::spawn(move || {
            loop {
                // Accept connection
                if let Ok((stream, addr)) = listener.accept() {
                    // Do not block as we need to iterate over all streams
                    stream.set_nonblocking(true).unwrap();

                    let mut transfer = transfer.lock().unwrap();
                    // Append client transfers into vector
                    transfer.clients.push(
                        Client {
                            stream,
                            reader: PartialReader::new(),
                            address: addr.ip()
                        }
                    );
                    // Increment number of connections and tell app
                    number_connections.fetch_add(1, SeqCst);
                    transfer.server_tx.send(ReceiveData::NewConnection(addr.ip())).ok();
                }
                // Break the loop if the server's stopped
                if should_stop.load(SeqCst) {break}
                sleep(Duration::from_millis(100));
            }
        });
        

        return Ok(());
    }

    pub fn run(&mut self) {
        let transfer = self.transfer.as_ref().unwrap().clone();
        let number_connections = self.number_connections.clone();
        let should_stop = self.should_stop.clone();

        thread::spawn(move || {
            loop {
                let transfer = &mut transfer.lock().unwrap();
                let mut to_write = Vec::new();
    
                // Read incoming stream data
                for client in transfer.clients.iter_mut() {
                    // Read buffs
                    let mut buf = [0; 1024];
                    client.stream.read(&mut buf).ok();
    
                    // Append bytes to reader
                    if let Some(data) = client.reader.try_read_string(&buf) {
                        // Parse payload
                        if let Ok(data) = process_message(&data) {
                            to_write.push(data);
                        }
                    }
                }
    
                // Send resulting incoming stream data to app
                for data in to_write {
                    transfer.server_tx.send(data).ok();
                }
    
                // Send data from app to clients
                let mut to_drop = Vec::new();
                if let Ok(data) = transfer.client_rx.try_recv() {
                    // Broadcast data to all clients
                    for (index, client) in transfer.clients.iter_mut().enumerate() {
                        match client.stream.write_all((data.to_string() + "\n").as_bytes()) {
                            Ok(_) => {}
                            Err(_) => {
                                // Connection dropped
                                to_drop.push(index);
                            }
                        };
                    }
                }
    
                // Remove any connections that got dropped and tell app
                for dropping in to_drop {
                    let removed_client = transfer.clients.remove(dropping);
                    number_connections.fetch_sub(1, SeqCst);
                    transfer.server_tx.send(ReceiveData::ConnectionLost(removed_client.address)).ok();
                }

                if should_stop.load(SeqCst) {break}
            }
        });
    }

    pub fn get_last_port_forward_error(&self) -> &Option<PortForwardResult> {
        return &self.port_error
    }
}

impl TransferClient for Server {
    fn get_connected_count(&self) -> u16 {
        return self.number_connections.load(SeqCst);
    }

    fn stop(&self) {
        self.should_stop.store(true, SeqCst);
    }

    fn is_server(&self) -> bool {
        true
    }

    fn stopped(&self) -> bool {
        self.should_stop.load(SeqCst)
    }

    fn get_transmitter(&self) -> &Sender<Value> {
        return &self.client_tx;
    }

    fn get_receiver(&self) ->& Receiver<ReceiveData> {
        return &self.server_rx;
    }
}

#[cfg(test)]
mod test {
    use super::*;
    #[test]
    fn test_partial_reader() {
        let mut pr = PartialReader::new();
        assert_eq!(pr.try_read_string("Hello".as_bytes()), None);
        assert_eq!(pr.try_read_string("\nYes".as_bytes()).unwrap(), "Hello");
        assert_eq!(pr.try_read_string("\nYes\n".as_bytes()).unwrap(), "Yes");
    }
}