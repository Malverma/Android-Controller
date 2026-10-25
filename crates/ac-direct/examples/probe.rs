//! Manual check against a phone: `probe <ip> pair <code>` then `probe <ip> link`
//! / `probe <ip> stream` / `probe <ip> awake <seconds>`. Keys go to ./direct-keys-probe.toml.
use std::time::Duration;

use ac_direct::*;
use tokio::io::AsyncReadExt;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let addr = parse_addr(&args[1]).expect("ip[:port]");
    let path = std::path::Path::new("direct-keys-probe.toml");
    let mut ks = KeyStore::load(path);
    match args[2].as_str() {
        "pair" => {
            let p = pair(addr, &ks.pc_id, "probe", &args[3]).await.expect("pair");
            println!("paired with {} ({})", p.model, p.id);
            ks.insert(&p);
            ks.save().unwrap();
        }
        "link" => {
            let mut l = Link::connect(addr, &ks.credentials(), None).await.expect("link");
            println!("{:?}", l.hello);
            for _ in 0..3 {
                let (pong, rtt) = l.ping(Duration::from_secs(3)).await.expect("ping");
                println!("{pong:?} rtt {rtt:?}");
            }
            println!("stay awake now {}", l.stay_awake(None, Duration::from_secs(3)).await.unwrap());
        }
        "awake" => {
            // `probe <ip> awake <seconds>`: keep the screen on while linked.
            let secs: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(30);
            let mut l = Link::connect(addr, &ks.credentials(), None).await.expect("link");
            println!("keep screen on: {}", l.stay_awake(Some(true), Duration::from_secs(3)).await.unwrap());
            for _ in 0..secs / 2 {
                tokio::time::sleep(Duration::from_secs(2)).await;
                l.ping(Duration::from_secs(3)).await.expect("ping");
            }
            println!("releasing");
            println!("keep screen on: {}", l.stay_awake(Some(false), Duration::from_secs(3)).await.unwrap());
        }
        "stream" => {
            let creds = ks.credentials();
            let id = creds.keys.keys().next().unwrap().clone();
            let p = StreamParams { max_size: 1024, bit_rate: 4_000_000, max_fps: 30, power_on: true, codec: "h264" };
            let (mut s, input) = open_stream(addr, &creds, &id, p).await.expect("stream");
            println!("stream open, input {input}");
            let mut codec = [0u8; 4];
            s.read_exact(&mut codec).await.unwrap();
            println!("codec {:?}", String::from_utf8_lossy(&codec));
            let mut total = 0usize;
            let start = std::time::Instant::now();
            let mut packets = 0;
            while start.elapsed() < Duration::from_secs(5) {
                let mut h = [0u8; 12];
                s.read_exact(&mut h).await.unwrap();
                if h[0] & 0x80 != 0 {
                    println!("session {}x{}", u32::from_be_bytes(h[4..8].try_into().unwrap()), u32::from_be_bytes(h[8..12].try_into().unwrap()));
                    continue;
                }
                let flags = u64::from_be_bytes(h[0..8].try_into().unwrap());
                let len = u32::from_be_bytes(h[8..12].try_into().unwrap()) as usize;
                let mut d = vec![0u8; len];
                s.read_exact(&mut d).await.unwrap();
                if packets < 3 {
                    println!("packet len {len} config {} key {}", flags & (1 << 62) != 0, flags & (1 << 61) != 0);
                }
                packets += 1;
                total += len;
            }
            println!("{packets} packets, {} kB in 5 s", total / 1000);
        }
        "push" => {
            let creds = ks.credentials();
            let id = creds.keys.keys().next().unwrap().clone();
            let data = std::fs::read(&args[3]).unwrap();
            let install = args.get(4).is_some_and(|a| a == "install");
            let req = PushRequest { name: "probe-push.bin".into(), dir: String::new(), size: data.len() as u64, job: 43, install, conflict: "overwrite".into() };
            let (mut s, start) = open_push(addr, &creds, &id, &req).await.expect("push");
            let PushStart::From(off) = start else { panic!("skip") };
            println!("offset {off}");
            tokio::io::AsyncWriteExt::write_all(&mut s, &data[off as usize..]).await.unwrap();
            println!("sent");
            println!("{:?}", finish_push(&mut s, Duration::from_secs(60)).await);
        }
        _ => panic!("pair|link|stream|push"),
    }
}
