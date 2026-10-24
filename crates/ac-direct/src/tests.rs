//! A phone stand-in following DirectService.java, to check the PC side
//! against the protocol end to end.

use super::*;
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;

struct Phone {
    code: Mutex<Option<String>>,
    /// pc_id → key
    pcs: Mutex<BTreeMap<String, [u8; 32]>>,
    id: String,
}

async fn spawn_phone(code: Option<&str>) -> (SocketAddr, Arc<Phone>) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let phone = Arc::new(Phone { code: Mutex::new(code.map(str::to_owned)), pcs: Mutex::default(), id: "direct:abc123".into() });
    let p = phone.clone();
    tokio::spawn(async move {
        loop {
            let (s, _) = l.accept().await.unwrap();
            let p = p.clone();
            tokio::spawn(async move {
                let _ = serve(s, &p).await;
            });
        }
    });
    (addr, phone)
}

async fn serve(mut s: TcpStream, p: &Phone) -> Result<(), Error> {
    let mut magic = [0u8; 5];
    s.read_exact(&mut magic).await?;
    assert_eq!(&magic[..4], MAGIC);
    let purpose = magic[4];
    if purpose == Purpose::Pair as u8 {
        let req = read_json(&mut s).await?;
        let Some(code) = p.code.lock().unwrap().clone() else {
            return write_json(&mut s, &json!({ "error": "Pairing is not open" })).await;
        };
        let theirs = hex_field(&req, "pub")?;
        let secret = random_secret();
        let ours = secret.public_key().to_encoded_point(false).as_bytes().to_vec();
        let shared = p256::ecdh::diffie_hellman(
            secret.to_nonzero_scalar(),
            PublicKey::from_sec1_bytes(&theirs).unwrap().as_affine(),
        );
        let shared = shared.raw_secret_bytes().to_vec();
        write_json(&mut s, &json!({ "id": p.id, "model": "Test", "pub": hex(&ours) })).await?;
        let transcript = sha256(&[&theirs, &ours]);
        let code_key = sha256(&[format!("acd-code:{code}").as_bytes()]);
        let confirm = read_json(&mut s).await?;
        if !same(&hex_field(&confirm, "confirm")?, &hmac(&code_key, &[b"pc", &transcript, &shared])) {
            return write_json(&mut s, &json!({ "error": "Wrong code" })).await;
        }
        let key = hmac(&shared, &[b"acd-key", &transcript, &code_key]);
        p.pcs.lock().unwrap().insert(str_field(&req, "pc_id")?.to_owned(), key);
        let c = hmac(&code_key, &[b"phone", &transcript, &shared]);
        return write_json(&mut s, &json!({ "ok": true, "confirm": hex(&c) })).await;
    }
    let hello = read_json(&mut s).await?;
    let Some(key) = p.pcs.lock().unwrap().get(str_field(&hello, "pc_id")?).copied() else {
        return write_json(&mut s, &json!({ "error": "not paired", "unpaired": true })).await;
    };
    let nc = hex_field(&hello, "nonce")?;
    let ns = random_bytes::<16>();
    let pb = [purpose];
    let proof = hmac(&key, &[b"S", &nc, &ns, &pb]);
    write_json(&mut s, &json!({ "id": p.id, "nonce": hex(&ns), "proof": hex(&proof) })).await?;
    let req = read_json(&mut s).await?;
    if !same(&hex_field(&req, "proof")?, &hmac(&key, &[b"C", &nc, &ns, &pb])) {
        return write_json(&mut s, &json!({ "error": "authentication failed" })).await;
    }
    match purpose {
        2 => {
            write_json(&mut s, &json!({ "ok": true, "model": "Test", "manufacturer": "ACME", "sdk": 28, "release": "9", "input": true })).await?;
            loop {
                let m = read_json(&mut s).await?;
                match m["op"].as_str() {
                    Some("ping") => write_json(&mut s, &json!({ "op": "pong", "battery": 87, "charging": true, "rssi": -60 })).await?,
                    Some("stay_awake") => write_json(&mut s, &json!({ "op": "stay_awake", "on": m["on"].as_bool().unwrap_or(false) })).await?,
                    _ => {}
                }
            }
        }
        4 => {
            let size = req["size"].as_u64().unwrap();
            write_json(&mut s, &json!({ "ok": true, "offset": 3 })).await?;
            let mut rest = vec![0u8; (size - 3) as usize];
            s.read_exact(&mut rest).await?;
            write_json(&mut s, &json!({ "ok": true, "path": "/sdcard/Download/x", "sha256": hex(&sha256(&[&rest])) })).await
        }
        _ => write_json(&mut s, &json!({ "error": "unsupported" })).await,
    }
}

fn creds(pc_id: &str, paired: &Paired) -> Credentials {
    Credentials { pc_id: pc_id.into(), pc_name: "test-pc".into(), keys: [(paired.id.clone(), paired.key)].into() }
}

#[tokio::test]
async fn pairs_and_authenticates() {
    let (addr, phone) = spawn_phone(Some("482913")).await;
    let paired = pair(addr, "pc1", "test-pc", "482913").await.unwrap();
    assert_eq!(paired.id, "direct:abc123");
    assert_eq!(phone.pcs.lock().unwrap()["pc1"], paired.key, "both sides derive the same key");

    let c = creds("pc1", &paired);
    let mut link = Link::connect(addr, &c, Some("direct:abc123")).await.unwrap();
    assert_eq!((link.hello.model.as_str(), link.hello.sdk, link.hello.input), ("Test", 28, true));
    let (pong, _) = link.ping(Duration::from_secs(2)).await.unwrap();
    assert_eq!((pong.battery, pong.charging, pong.rssi), (Some(87), true, Some(-60)));
    assert!(link.stay_awake(Some(true), Duration::from_secs(2)).await.unwrap());

    // A remembered address now answered by another phone.
    assert!(matches!(Link::connect(addr, &c, Some("direct:other")).await, Err(Error::WrongDevice { .. })));
    // A PC the phone does not know.
    let stranger = Credentials { pc_id: "pc2".into(), ..c.clone() };
    assert!(Link::connect(addr, &stranger, None).await.unwrap_err().is_unpaired());
    // A key that does not match.
    let mut wrong = c.clone();
    wrong.keys.insert(paired.id.clone(), [7u8; 32]);
    assert!(matches!(Link::connect(addr, &wrong, None).await, Err(Error::Protocol(_))));
}

#[tokio::test]
async fn wrong_code_and_closed_pairing_are_refused() {
    let (addr, phone) = spawn_phone(Some("111111")).await;
    match pair(addr, "pc1", "pc", "222222").await {
        Err(Error::Refused { message, .. }) => assert!(message.contains("Wrong code")),
        other => panic!("{other:?}"),
    }
    assert!(phone.pcs.lock().unwrap().is_empty());
    *phone.code.lock().unwrap() = None;
    assert!(matches!(pair(addr, "pc1", "pc", "111111").await, Err(Error::Refused { .. })));
}

#[tokio::test]
async fn push_resumes_from_the_phones_offset() {
    let (addr, _phone) = spawn_phone(Some("000000")).await;
    let paired = pair(addr, "pc1", "pc", "000000").await.unwrap();
    let c = creds("pc1", &paired);
    let req = PushRequest { name: "a.txt".into(), dir: String::new(), size: 8, job: 1, install: false, conflict: "rename".into() };
    let (mut s, start) = open_push(addr, &c, &paired.id, &req).await.unwrap();
    let PushStart::From(off) = start else { panic!("skip") };
    assert_eq!(off, 3);
    s.write_all(&b"abcdefgh"[off as usize..]).await.unwrap();
    let done = finish_push(&mut s, Duration::from_secs(2)).await.unwrap();
    assert_eq!(done.sha256, Some(hex(&sha256(&[b"defgh"]))));
}

#[test]
fn key_store_round_trip() {
    let dir = std::env::temp_dir().join(format!("ac-direct-test-{}", std::process::id()));
    let path = dir.join("direct-keys.toml");
    let _ = std::fs::remove_file(&path);
    let mut ks = KeyStore::load(&path);
    assert_eq!(ks.pc_id.len(), 32);
    ks.insert(&Paired { id: "direct:x".into(), model: "LG".into(), key: [9u8; 32] });
    ks.save().unwrap();
    let again = KeyStore::load(&path);
    assert_eq!(again.pc_id, ks.pc_id);
    assert_eq!(again.key("direct:x"), Some([9u8; 32]));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn addresses_and_hex() {
    assert_eq!(parse_addr("192.168.0.50"), Some("192.168.0.50:27190".parse().unwrap()));
    assert_eq!(parse_addr(" 192.168.0.50:1234 "), Some("192.168.0.50:1234".parse().unwrap()));
    assert_eq!(parse_addr("phone"), None);
    assert_eq!(unhex(&hex(&[0, 255, 16])), Some(vec![0, 255, 16]));
    assert_eq!(unhex("abc"), None);
    assert_eq!(unhex("zz"), None);
}
