use lazy_static::lazy_static;
use reth_network::config::rng_secret_key;
use std::{
    env::var,
    net::{IpAddr, Ipv4Addr},
};

lazy_static! {
    pub static ref PRIVATE_KEY: String = match var("PRIVATE_KEY") {
        Ok(value) => value.parse().unwrap(),
        Err(_) => {
            format!("{}", rng_secret_key().display_secret())
        }
    };
    pub static ref BOOT_NODE_IP: Option<IpAddr> = match var("BOOT_NODE_IP") {
        Ok(value) => Some(value.parse().unwrap()),
        Err(_) => {
            None
        }
    };
    pub static ref BOOT_NODE_TCP_PORT: Option<u16> = match var("BOOT_NODE_TCP_PORT") {
        Ok(value) => Some(value.parse().unwrap()),
        Err(_) => {
            None
        }
    };
    pub static ref BOOT_NODE_UDP_PORT: Option<u16> = match var("BOOT_NODE_UDP_PORT") {
        Ok(value) => Some(value.parse().unwrap()),
        Err(_) => {
            None
        }
    };
    pub static ref BOOT_NODE_ID: Option<String> = match var("BOOT_NODE_ID") {
        Ok(value) => Some(value.parse().unwrap()),
        Err(_) => {
            None
        }
    };
}

pub fn print_config() {
    println!("PRIVATE_KEY: {}", *PRIVATE_KEY);
    println!("BOOT_NODE_IP: {:?}", *BOOT_NODE_IP);
    println!("BOOT_NODE_TCP_PORT: {:?}", *BOOT_NODE_TCP_PORT);
    println!("BOOT_NODE_UDP_PORT: {:?}", *BOOT_NODE_UDP_PORT);
    println!("BOOT_NODE_ID: {:?}", *BOOT_NODE_ID);
}
