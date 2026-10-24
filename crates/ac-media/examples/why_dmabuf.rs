//! Prints why (or whether) VA-API dma-buf export works on this host.
use ac_media::decoder::PROBE_H264;
use ac_media::{Decoder, FrameData, HwDevice, HwOutput};
use ac_scrcpy::VideoCodec;

fn main() {
    let hw = HwDevice::open_vaapi(None).expect("VA-API device");
    let mut d = Decoder::new(VideoCodec::H264, Some(&hw)).unwrap();
    match d.decode(PROBE_H264, 0, true, HwOutput::Dmabuf) {
        Ok(Some(f)) => match &f.data {
            FrameData::Dmabuf(b) => println!("dma-buf ok: fourcc {:#x} modifier {:#x} planes {}", b.fourcc, b.modifier, b.planes.len()),
            FrameData::Memory(m) => println!("software frame {:?}", m.layout),
        },
        Ok(None) => println!("no frame"),
        Err(e) => println!("error: {e}"),
    }
}
