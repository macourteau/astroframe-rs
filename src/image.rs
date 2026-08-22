//! A decoded frame: a [`Header`] plus normalized `f32`.

use crate::header::Header;

/// A decoded frame — normalized `f32` in `[0,1]`, row-major and channel-planar.
///
/// `Image` runs the other way from [`Header`]: it exists only for a frame that decoded, so
/// it exposes plain non-`Option` geometry, delegating to the header values it is guaranteed
/// to hold. The common path never pays for the declined one.
///
/// The output invariant is "**finite** samples lie in `[0,1]`", not "all samples lie in
/// `[0,1]`": NaN is preserved rather than folded to a bound, because a NaN pixel is a real
/// signal. Only float sources can produce one.
#[derive(Clone, Debug)]
pub struct Image {
    pub(crate) header: Header,
    pub(crate) data: Vec<f32>,
}

impl Image {
    /// The header this frame was decoded from.
    pub fn header(&self) -> &Header {
        &self.header
    }

    /// Image width in pixels.
    pub fn width(&self) -> u32 {
        self.header
            .width()
            .expect("a decoded image always has geometry")
    }

    /// Image height in pixels.
    pub fn height(&self) -> u32 {
        self.header
            .height()
            .expect("a decoded image always has geometry")
    }

    /// Channel count — `1` after `select_channel`.
    pub fn channels(&self) -> u32 {
        self.header
            .channels()
            .expect("a decoded image always has geometry")
    }

    /// Every sample, channel-planar: channel 0's `width * height` samples, then channel 1's,
    /// and so on.
    pub fn samples(&self) -> &[f32] {
        &self.data
    }

    /// One channel's samples.
    ///
    /// `k` indexes the **image's own** channels, so after `select_channel` the image has one
    /// channel and `k` is 0. [`Header::channel_index`] and a chunk's channel index run the
    /// other way, reporting the *file's* index.
    pub fn channel(&self, k: u32) -> Option<&[f32]> {
        if k >= self.channels() {
            return None;
        }
        let plane = self.width() as usize * self.height() as usize;
        let start = k as usize * plane;
        self.data.get(start..start + plane)
    }

    /// Take the sample buffer, dropping the header.
    pub fn into_samples(self) -> Vec<f32> {
        self.data
    }
}
