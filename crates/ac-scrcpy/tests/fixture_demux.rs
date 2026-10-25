//! Golden demux of a real scrcpy 4.1 capture (Schok Volt SV55, Android 11,
//! Qualcomm H.264 encoder, `max_size=720`), taken with the dummy byte
//! already consumed.

use ac_scrcpy::{DemuxError, DemuxItem, Demuxer, VideoCodec};

const CAPTURE: &[u8] = include_bytes!("fixtures/scrcpy-4.1-h264-360x720.bin");

#[test]
fn real_capture_demuxes() {
    let mut d = Demuxer::new(CAPTURE);
    assert_eq!(d.read_device_name().unwrap(), "Schok Volt SV55");
    assert_eq!(d.read_codec().unwrap(), VideoCodec::H264);
    assert_eq!(d.next_item().unwrap(), DemuxItem::Session { width: 360, height: 720, client_resized: false });

    // First packet: SPS/PPS config (32 bytes) prepended to the IDR frame.
    let DemuxItem::Packet { data, pts_us, key_frame, .. } = d.next_item().unwrap() else { panic!("expected a packet") };
    assert!(key_frame);
    assert_eq!(data.len(), 32 + 22240);
    assert_eq!(&data[..5], &[0, 0, 0, 1, 0x67], "Annex B SPS first");
    assert_eq!(&data[32..37], &[0, 0, 0, 1, 0x65], "then the IDR slice");

    let mut last = pts_us;
    let mut frames = 1;
    loop {
        match d.next_item() {
            Ok(DemuxItem::Packet { data, pts_us, key_frame, .. }) => {
                assert!(!key_frame);
                assert_eq!(data[4] & 0x1f, 1, "non-IDR slice");
                assert!(pts_us > last, "presentation timestamps increase");
                last = pts_us;
                frames += 1;
            }
            Ok(other) => panic!("unexpected {other:?}"),
            // The capture was cut mid-packet.
            Err(DemuxError::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => panic!("{e}"),
        }
    }
    assert!(frames >= 10, "{frames} frames");
}
