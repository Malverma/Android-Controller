//! Decoded frame → `GdkTexture` (spec §2.4 step 4).

use ac_media::{ColorInfo, Frame, FrameData, PlaneLayout};
use gtk::gdk;
use gtk::glib::{self, translate::FromGlib};

/// Builds a texture for `frame`. Memory frames always succeed; a dma-buf
/// import can fail (unsupported modifier), in which case the error is
/// returned and the caller switches the decoder to `vaapi-copy`.
pub fn build(display: &gdk::Display, frame: Frame) -> Result<gdk::Texture, glib::Error> {
    let color = color_state(&frame.color);
    let (w, h) = (frame.width, frame.height);
    if let FrameData::Dmabuf(d) = &frame.data {
        let mut b = gdk::DmabufTextureBuilder::new()
            .set_display(display)
            .set_width(w)
            .set_height(h)
            // NVIDIA's R8 + RG88 layers and Intel/AMD's composed export are both NV12 in memory.
            .set_fourcc(ac_media::frame::DRM_FORMAT_NV12)
            .set_modifier(d.modifier)
            .set_n_planes(d.planes.len() as u32)
            .set_color_state(Some(&color));
        for (i, p) in d.planes.iter().enumerate() {
            // SAFETY: the fds stay open while `frame` lives, and the release
            // func keeps it alive until GTK drops the texture.
            b = unsafe { b.set_fd(i as u32, p.fd) }.set_offset(i as u32, p.offset).set_stride(i as u32, p.stride);
        }
        // SAFETY: see above.
        return unsafe { b.build_with_release_func(move || drop(frame)) };
    }
    let FrameData::Memory(m) = frame.data else { unreachable!("handled above") };
    {
        {
            // SAFETY: GDK_MEMORY_G8_B8R8_420 (NV12) and G8_B8_R8_420 (I420)
            // exist from GTK 4.20; the gtk4-rs enum predates them.
            let format = unsafe {
                gdk::MemoryFormat::from_glib(match m.layout {
                    PlaneLayout::Nv12 => gdk::ffi::GDK_MEMORY_G8_B8R8_420,
                    PlaneLayout::I420 => gdk::ffi::GDK_MEMORY_G8_B8_R8_420,
                })
            };
            let planes = match m.layout {
                PlaneLayout::Nv12 => 2,
                PlaneLayout::I420 => 3,
            };
            let mut b = gdk::MemoryTextureBuilder::new()
                .set_bytes(Some(&glib::Bytes::from_owned(m.data)))
                .set_format(format)
                .set_width(w as i32)
                .set_height(h as i32)
                .set_color_state(&color);
            for p in 0..planes {
                b = b.set_offset(p as u32, m.offsets[p]).set_stride_for_plane(p as u32, m.strides[p]);
            }
            Ok(b.build())
        }
    }
}

fn color_state(c: &ColorInfo) -> gdk::ColorState {
    let p = gdk::CicpParams::new();
    p.set_color_primaries(c.primaries);
    p.set_transfer_function(c.transfer);
    p.set_matrix_coefficients(c.matrix);
    p.set_range(if c.full_range { gdk::CicpRange::Full } else { gdk::CicpRange::Narrow });
    p.build_color_state().unwrap_or_else(|e| {
        tracing::debug!("colour {c:?} not supported by GTK ({e}); using BT.709");
        let p = gdk::CicpParams::new();
        p.set_color_primaries(1);
        p.set_transfer_function(1);
        p.set_matrix_coefficients(1);
        p.set_range(gdk::CicpRange::Narrow);
        p.build_color_state().unwrap_or_else(|_| gdk::ColorState::srgb())
    })
}
