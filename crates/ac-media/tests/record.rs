//! Records a real scrcpy capture to MP4 and checks the file plays.

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use ac_media::recorder::{self, RecMsg};
use ac_scrcpy::{DemuxError, DemuxItem, Demuxer, VideoCodec};

const CAPTURE: &[u8] = include_bytes!("../../ac-scrcpy/tests/fixtures/scrcpy-4.1-h264-360x720.bin");

fn feed(tx: &mpsc::Sender<RecMsg>, width: u32) -> usize {
    let mut d = Demuxer::new(CAPTURE);
    d.read_device_name().unwrap();
    d.read_codec().unwrap();
    let mut frames = 0;
    loop {
        match d.next_item() {
            Ok(DemuxItem::Session { .. }) => {}
            Ok(DemuxItem::Packet { data, pts_us, key_frame, config_len }) => {
                if config_len > 0 {
                    let config = data[..config_len].to_vec();
                    tx.send(RecMsg::Config { codec: VideoCodec::H264, width, height: 720, config }).unwrap();
                }
                tx.send(RecMsg::Packet { data: data[config_len..].to_vec(), pts_us, key: key_frame }).unwrap();
                frames += 1;
            }
            Err(DemuxError::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => return frames,
            Err(e) => panic!("{e}"),
        }
    }
}

fn ffprobe_frames(path: &std::path::Path) -> Option<usize> {
    let out = std::process::Command::new("ffprobe")
        .args(["-v", "error", "-count_frames", "-select_streams", "v:0", "-show_entries", "stream=nb_read_frames,width,height,codec_name", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    eprintln!("ffprobe {}: {text}", path.display());
    text.trim().rsplit(',').next()?.parse().ok()
}

#[test]
fn records_a_capture_and_starts_a_new_part_on_a_new_size() {
    let dir = std::env::temp_dir().join(format!("ac-record-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (done_tx, done_rx) = mpsc::channel();
    let tx = recorder::spawn(dir.join("Moto 21-50-03"), Box::new(move |r| done_tx.send(r).unwrap())).unwrap();
    let n = feed(&tx, 360);
    // Rotation: a new size and config start a second file.
    feed(&tx, 362);
    tx.send(RecMsg::Stop).unwrap();
    let files: Vec<PathBuf> = done_rx.recv_timeout(Duration::from_secs(10)).unwrap().unwrap();
    assert_eq!(files, vec![dir.join("Moto 21-50-03.mp4"), dir.join("Moto 21-50-03 (2).mp4")]);
    for f in &files {
        assert!(std::fs::metadata(f).unwrap().len() > 20_000);
        if let Some(frames) = ffprobe_frames(f) {
            assert_eq!(frames, n, "every frame is in {}", f.display());
        }
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn nothing_before_a_key_frame() {
    let dir = std::env::temp_dir().join(format!("ac-record-empty-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (done_tx, done_rx) = mpsc::channel();
    let tx = recorder::spawn(dir.join("x"), Box::new(move |r| done_tx.send(r).unwrap())).unwrap();
    tx.send(RecMsg::Packet { data: vec![0, 0, 0, 1, 0x41, 1, 2, 3], pts_us: 0, key: false }).unwrap();
    drop(tx); // the stream went away
    assert_eq!(done_rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap(), Vec::<PathBuf>::new());
    std::fs::remove_dir_all(&dir).unwrap();
}
