fn main() {
    println!("rio-build v{}", rio_proto::VERSION);
    println!(
        "protocol version: {:#x}",
        rio_nix::protocol::handshake::PROTOCOL_VERSION
    );
}
