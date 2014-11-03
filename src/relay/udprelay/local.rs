// The MIT License (MIT)

// Copyright (c) 2014 Y. T. CHUNG

// Permission is hereby granted, free of charge, to any person obtaining a copy of
// this software and associated documentation files (the "Software"), to deal in
// the Software without restriction, including without limitation the rights to
// use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies of
// the Software, and to permit persons to whom the Software is furnished to do so,
// subject to the following conditions:

// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.

// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS
// FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR
// COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER
// IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN
// CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.

// SOCKS5 UDP Request
// +----+------+------+----------+----------+----------+
// |RSV | FRAG | ATYP | DST.ADDR | DST.PORT |   DATA   |
// +----+------+------+----------+----------+----------+
// | 2  |  1   |  1   | Variable |    2     | Variable |
// +----+------+------+----------+----------+----------+

// SOCKS5 UDP Response
// +----+------+------+----------+----------+----------+
// |RSV | FRAG | ATYP | DST.ADDR | DST.PORT |   DATA   |
// +----+------+------+----------+----------+----------+
// | 2  |  1   |  1   | Variable |    2     | Variable |
// +----+------+------+----------+----------+----------+

// shadowsocks UDP Request (before encrypted)
// +------+----------+----------+----------+
// | ATYP | DST.ADDR | DST.PORT |   DATA   |
// +------+----------+----------+----------+
// |  1   | Variable |    2     | Variable |
// +------+----------+----------+----------+

// shadowsocks UDP Response (before encrypted)
// +------+----------+----------+----------+
// | ATYP | DST.ADDR | DST.PORT |   DATA   |
// +------+----------+----------+----------+
// |  1   | Variable |    2     | Variable |
// +------+----------+----------+----------+

// shadowsocks UDP Request and Response (after encrypted)
// +-------+--------------+
// |   IV  |    PAYLOAD   |
// +-------+--------------+
// | Fixed |   Variable   |
// +-------+--------------+

#[phase(plugin, link)]
extern crate log;

use std::sync::{Arc, Mutex};
use std::io::net::udp::UdpSocket;
use std::io::net::ip::SocketAddr;
use std::collections::{LruCache, HashMap};
use std::io::BufReader;

use crypto::cipher;
use crypto::cipher::Cipher;
use config::{Config, ServerConfig, SingleServer, MultipleServer};
use relay::Relay;
use relay::socks5::{AddressType, parse_request_header};
use relay::loadbalancing::server::{LoadBalancer, RoundRobin};
use relay::udprelay::UDP_RELAY_LOCAL_LRU_CACHE_CAPACITY;

#[deriving(Clone)]
pub struct UdpRelayLocal {
    config: Config,
}

impl UdpRelayLocal {
    pub fn new(config: Config) -> UdpRelayLocal {
        UdpRelayLocal {
            config: config,
        }
    }
}

impl Relay for UdpRelayLocal {
    fn run(&self) {
        let addr = self.config.local.expect("Local configuration should not be None");

        let mut server_load_balancer = RoundRobin::new(
                                        self.config.server.clone().expect("`server` should not be None"));

        let server_set = {
            let mut server_set = HashMap::new();
            match self.config.server.clone().unwrap() {
                SingleServer(s) => {
                    server_set.insert(s.addr, s);
                },
                MultipleServer(ref slist) => {
                    for s in slist.iter() {
                        server_set.insert(s.addr, s.clone());
                    }
                }
            }
            server_set
        };

        let client_map_arc = Arc::new(Mutex::new(
                    LruCache::<AddressType, SocketAddr>::new(UDP_RELAY_LOCAL_LRU_CACHE_CAPACITY)));

        let mut socket = UdpSocket::bind(addr).ok().expect("Failed to bind udp socket");

        let mut buf = [0u8, .. 0xffff];
        loop {
            match socket.recv_from(buf) {
                Ok((len, source_addr)) => {
                    if len < 4 {
                        error!("UDP request is too short");
                        continue;
                    }

                    let request_message = buf.slice_to(len).to_vec();
                    let move_socket = socket.clone();
                    let client_map = client_map_arc.clone();

                    match server_set.find(&source_addr) {
                        Some(sref) => {
                            let s = sref.clone();
                            spawn(proc()
                                handle_response(move_socket,
                                               request_message.as_slice(),
                                               source_addr,
                                               &s,
                                               client_map));
                        }
                        None => {
                            let s = server_load_balancer.pick_server().clone();

                            spawn(proc()
                                handle_request(move_socket,
                                              request_message.as_slice(),
                                              source_addr,
                                              &s,
                                              client_map));
                        }
                    }
                },
                Err(err) => {
                    error!("Failed in UDP recv_from: {}", err);
                    break
                }
            }
        }
    }
}

fn handle_request(mut socket: UdpSocket,
                  request_message: &[u8],
                  from_addr: SocketAddr,
                  config: &ServerConfig,
                  client_map: Arc<Mutex<LruCache<AddressType, SocketAddr>>>) {
    // According to RFC 1928
    //
    // Implementation of fragmentation is optional; an implementation that
    // does not support fragmentation MUST drop any datagram whose FRAG
    // field is other than X'00'.
    if request_message[2] != 0x00u8 {
        // Drop it
        warn!("Does not support fragmentation");
        return;
    }

    let data = request_message.slice_from(3);
    let mut bufr = BufReader::new(data);

    let (_, addr) = {
        let (header_len, addr) = match parse_request_header(&mut bufr) {
            Ok(result) => result,
            Err(..) => {
                error!("Error while parsing request header");
                return;
            }
        };
        (data.slice_to(header_len), addr)
    };

    info!("UDP ASSOCIATE {}", addr);
    debug!("UDP associate {} <-> {}", addr, from_addr);

    client_map.lock().put(addr, from_addr);

    let mut cipher = cipher::with_name(config.method.as_slice(), config.password.as_slice().as_bytes())
                        .expect(format!("Unsupported cipher {}", config.method.as_slice()).as_slice());
    let encrypted_data = cipher.encrypt(data);

    socket.send_to(encrypted_data.as_slice(), config.addr)
        .ok().expect("Error occurs while sending to remote");
}

fn handle_response(mut socket: UdpSocket,
                   response_messge: &[u8],
                   from_addr: SocketAddr,
                   config: &ServerConfig,
                   client_map: Arc<Mutex<LruCache<AddressType, SocketAddr>>>) {
    let mut cipher = cipher::with_name(config.method.as_slice(), config.password.as_slice().as_bytes())
                        .expect(format!("Unsupported cipher {}", config.method.as_slice()).as_slice());
    let decrypted_data = cipher.decrypt(response_messge);

    let mut bufr = BufReader::new(decrypted_data.as_slice());

    let (_, addr) = {
        let (header_len, addr) = match parse_request_header(&mut bufr) {
            Ok(result) => result,
            Err(..) => {
                error!("Error while parsing request header");
                return;
            }
        };
        (decrypted_data.as_slice().slice_from(header_len), addr)
    };

    let client_addr = {
        let mut cmap = client_map.lock();
        match cmap.get(&addr) {
            Some(a) => a.clone(),
            None => return
        }
    };

    debug!("UDP response {} -> {}", from_addr, client_addr);

    let mut response = vec![0x00, 0x00, 0x00];
    response.push_all(decrypted_data.as_slice());

    socket.send_to(response.as_slice(), client_addr)
        .ok().expect("Error occurs while sending to local");
}