//! Decodes the embedded probe frame and the real scrcpy capture. VA-API
//! cases are skipped (with a note) on hosts without a working driver.

use std::sync::atomic::Ordering;

use ac_media::decoder::{PROBE_H264, probe};
use ac_media::{Backend, Decoder, FrameData, HwOutput, Mailbox, MediaContext, PlaneLayout, StreamEvents};
use ac_scrcpy::VideoCodec;

const CAPTURE: &[u8] = include_bytes!("../../ac-scrcpy/tests/fixtures/scrcpy-4.1-h264-360x720.bin");

#[test]
fn software_decodes_probe() {
    let mut d = Decoder::new(VideoCodec::H264, None).unwrap();
    let f = d.decode(PROBE_H264, 0, true, HwOutput::Copy).unwrap().expect("a frame");
    assert_eq!((f.width, f.height), (64, 64));
    let FrameData::Memory(m) = &f.data else { panic!("memory frame") };
    assert_eq!(m.layout, PlaneLayout::I420);
    assert_eq!(m.offsets[0], 0);
    assert!(m.data.len() >= 64 * 64 * 3 / 2);
}

#[test]
fn vaapi_probe() {
    let (hw, result) = probe(None);
    match result.vaapi {
        Ok(()) => {
            assert!(hw.is_some());
            eprintln!("VA-API ok, dma-buf export: {}", result.dmabuf_export);
        }
        Err(e) => eprintln!("skipped: VA-API unavailable here ({e})"),
    }
}

struct Events(u32);
impl StreamEvents for Events {
    fn request_key_frame(&mut self) {
        self.0 += 1;
    }
}

/// Feeds the capture (after its 64-byte device name) through the decode
/// loop, which ends at the truncated last packet.
fn run_capture(media: &MediaContext) -> (Mailbox, u32) {
    let mailbox = Mailbox::default();
    let mut ev = Events(0);
    let end = ac_media::stream::run(&CAPTURE[64..], &mailbox, media, &mut ev);
    assert!(matches!(end, Err(ac_media::StreamEnd::Demux(ac_scrcpy::DemuxError::Io(_)))), "{end:?}");
    (mailbox, ev.0)
}

#[test]
fn capture_decodes_in_software() {
    let media = MediaContext::new("software", "auto");
    let (mailbox, key_requests) = run_capture(&media);
    assert_eq!(mailbox.size(), Some((360, 720)));
    assert_eq!(key_requests, 0, "no decode errors");
    assert!(mailbox.stats.decoded.load(Ordering::Relaxed) >= 10);
    let f = mailbox.take().unwrap();
    assert_eq!((f.width, f.height), (360, 720));
}

#[test]
fn capture_decodes_with_vaapi() {
    for decoder in ["vaapi-copy", "auto"] {
        let media = MediaContext::new(decoder, "auto");
        if media.backend() == Backend::Software {
            eprintln!("skipped: VA-API unavailable here");
            return;
        }
        let (mailbox, key_requests) = run_capture(&media);
        assert_eq!(key_requests, 0, "{decoder}: no decode errors");
        let f = mailbox.take().unwrap();
        assert_eq!((f.width, f.height), (360, 720));
        match (media.backend(), &f.data) {
            (Backend::VaapiCopy, FrameData::Memory(m)) => assert_eq!(m.layout, PlaneLayout::Nv12),
            (Backend::VaapiDmabuf, FrameData::Dmabuf(d)) => {
                assert_eq!(d.fourcc, ac_media::frame::DRM_FORMAT_NV12);
                assert_eq!(d.planes.len(), 2);
                eprintln!("dma-buf modifier {:#x}, strides {} {}", d.modifier, d.planes[0].stride, d.planes[1].stride);
            }
            (b, _) => panic!("{decoder}: backend {b} produced the wrong frame kind"),
        }
    }
}
