#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(non_upper_case_globals)]
#![allow(improper_ctypes)]
#![allow(dead_code)]

include!(concat!(env!("OUT_DIR"), "/aom_ffi.rs"));

use crate::codec::{base_bitrate, codec_thread_num, Quality};
use crate::{codec::EncoderApi, EncodeFrame, STRIDE_ALIGN};
use crate::{
  common::GoogleImage, generate_call_macro, generate_call_ptr_macro, Error,
  Result,
};
use hbb_common::{
  anyhow::{anyhow, Context},
  bytes::Bytes,
  log,
  message_proto::{EncodedVideoFrame, EncodedVideoFrames, Message, VideoFrame},
  ResultType,
};
use std::{ptr, slice};

generate_call_macro!(call_aom, false);
generate_call_macro!(call_aom_allow_err, true);
generate_call_ptr_macro!(call_aom_ptr);

impl Default for aom_codec_enc_cfg_t {
  fn default() -> Self {
    unsafe { std::mem::zeroed() }
  }
}

impl Default for aom_codec_ctx_t {
  fn default() -> Self {
    unsafe { std::mem::zeroed() }
  }
}

impl Default for aom_image_t {
  fn default() -> Self {
    unsafe { std::mem::zeroed() }
  }
}

#[derive(Clone, Copy, Debug)]
pub struct AomEncoderConfig {
  pub width: u32,
  pub height: u32,
  pub quality: Quality,
  pub keyframe_interval: Option<usize>,
}

pub struct AomEncoder {
  ctx: aom_codec_ctx_t,
  width: usize,
  height: usize,
}

// https://webrtc.googlesource.com/src/+/refs/heads/main/modules/video_coding/codecs/av1/libaom_av1_encoder.cc
mod webrtc {
  use super::*;

  const kUsageProfile: u32 = AOM_USAGE_REALTIME;
  const kMinQindex: u32 = 145; // Min qindex threshold for QP scaling.
  const kMaxQindex: u32 = 205; // Max qindex threshold for QP scaling.
  const kBitDepth: u32 = 8;
  const kLagInFrames: u32 = 0; // No look ahead.
  const kRtpTicksPerSecond: i32 = 90000;
  const kMinimumFrameRate: f64 = 1.0;

  pub const DEFAULT_Q_MAX: u32 = 56; // no more than 63
  pub const DEFAULT_Q_MIN: u32 = 12; // no more than 63, litter than q_max

  // Only positive speeds, range for real-time coding currently is: 6 - 8.
  // Lower means slower/better quality, higher means fastest/lower quality.
  fn get_cpu_speed(width: u32, height: u32) -> u32 {
    // aux_config_ = nullptr, kComplexityHigh
    if width * height <= 320 * 180 {
      8
    } else if width * height <= 640 * 360 {
      9
    } else {
      10
    }
  }

  fn get_super_block_size(
    width: u32,
    height: u32,
    threads: u32,
  ) -> aom_superblock_size_t {
    use aom_superblock_size::*;
    let resolution = width * height;
    if threads >= 4 && resolution >= 960 * 540 && resolution < 1920 * 1080 {
      AOM_SUPERBLOCK_SIZE_64X64
    } else {
      AOM_SUPERBLOCK_SIZE_DYNAMIC
    }
  }

  pub fn enc_cfg(
    i: *const aom_codec_iface,
    cfg: AomEncoderConfig,
  ) -> ResultType<aom_codec_enc_cfg> {
    let mut c = unsafe { std::mem::MaybeUninit::zeroed().assume_init() };
    call_aom!(aom_codec_enc_config_default(i, &mut c, kUsageProfile));

    // Overwrite default config with input encoder settings & RTC-relevant values.
    c.g_w = cfg.width;
    c.g_h = cfg.height;
    c.g_threads = codec_thread_num() as _;
    c.g_timebase.num = 1;
    c.g_timebase.den = kRtpTicksPerSecond;
    c.g_input_bit_depth = kBitDepth;
    if let Some(keyframe_interval) = cfg.keyframe_interval {
      c.kf_min_dist = 0;
      c.kf_max_dist = keyframe_interval as _;
    } else {
      c.kf_mode = aom_kf_mode::AOM_KF_DISABLED;
    }
    let (q_min, q_max, b) = AomEncoder::convert_quality(cfg.quality);
    if q_min > 0 && q_min < q_max && q_max < 64 {
      c.rc_min_quantizer = q_min;
      c.rc_max_quantizer = q_max;
    } else {
      c.rc_min_quantizer = DEFAULT_Q_MIN;
      c.rc_max_quantizer = DEFAULT_Q_MAX;
    }
    let base_bitrate = base_bitrate(cfg.width as _, cfg.height as _);
    let bitrate = base_bitrate * b / 100;
    if bitrate > 0 {
      c.rc_target_bitrate = bitrate;
    } else {
      c.rc_target_bitrate = base_bitrate;
    }
    c.rc_undershoot_pct = 50;
    c.rc_overshoot_pct = 50;
    c.rc_buf_initial_sz = 600;
    c.rc_buf_optimal_sz = 600;
    c.rc_buf_sz = 1000;
    c.g_usage = kUsageProfile;
    c.g_error_resilient = 0;
    // Low-latency settings.
    c.rc_end_usage = aom_rc_mode::AOM_CBR; // Constant Bit Rate (CBR) mode
    c.g_pass = aom_enc_pass::AOM_RC_ONE_PASS; // One-pass rate control
    c.g_lag_in_frames = kLagInFrames; // No look ahead when lag equals 0.

    Ok(c)
  }

  pub fn set_controls(
    ctx: *mut aom_codec_ctx_t,
    cfg: &aom_codec_enc_cfg,
  ) -> ResultType<()> {
    use aom_tune_content::*;
    use aome_enc_control_id::*;
    macro_rules! call_ctl {
      ($ctx:expr, $av1e:expr, $arg:expr) => {{
        call_aom_allow_err!(aom_codec_control($ctx, $av1e as i32, $arg));
      }};
    }

    call_ctl!(ctx, AOME_SET_CPUUSED, get_cpu_speed(cfg.g_w, cfg.g_h));
    call_ctl!(ctx, AV1E_SET_ENABLE_CDEF, 1);
    call_ctl!(ctx, AV1E_SET_ENABLE_TPL_MODEL, 0);
    call_ctl!(ctx, AV1E_SET_DELTAQ_MODE, 0);
    call_ctl!(ctx, AV1E_SET_ENABLE_ORDER_HINT, 0);
    call_ctl!(ctx, AV1E_SET_AQ_MODE, 3);
    call_ctl!(ctx, AOME_SET_MAX_INTRA_BITRATE_PCT, 300);
    call_ctl!(ctx, AV1E_SET_COEFF_COST_UPD_FREQ, 3);
    call_ctl!(ctx, AV1E_SET_MODE_COST_UPD_FREQ, 3);
    call_ctl!(ctx, AV1E_SET_MV_COST_UPD_FREQ, 3);
    // kScreensharing
    call_ctl!(ctx, AV1E_SET_TUNE_CONTENT, AOM_CONTENT_SCREEN);
    call_ctl!(ctx, AV1E_SET_ENABLE_PALETTE, 1);
    let tile_set = if cfg.g_threads == 4
      && cfg.g_w == 640
      && (cfg.g_h == 360 || cfg.g_h == 480)
    {
      AV1E_SET_TILE_ROWS
    } else {
      AV1E_SET_TILE_COLUMNS
    };
    // Failed on android
    call_ctl!(ctx, tile_set, (cfg.g_threads as f64 * 1.0f64).log2().ceil());
    call_ctl!(ctx, AV1E_SET_ROW_MT, 1);
    call_ctl!(ctx, AV1E_SET_ENABLE_OBMC, 0);
    call_ctl!(ctx, AV1E_SET_NOISE_SENSITIVITY, 0);
    call_ctl!(ctx, AV1E_SET_ENABLE_WARPED_MOTION, 0);
    call_ctl!(ctx, AV1E_SET_ENABLE_GLOBAL_MOTION, 0);
    call_ctl!(ctx, AV1E_SET_ENABLE_REF_FRAME_MVS, 0);
    call_ctl!(
      ctx,
      AV1E_SET_SUPERBLOCK_SIZE,
      get_super_block_size(cfg.g_w, cfg.g_h, cfg.g_threads)
    );
    call_ctl!(ctx, AV1E_SET_ENABLE_CFL_INTRA, 0);
    call_ctl!(ctx, AV1E_SET_ENABLE_SMOOTH_INTRA, 0);
    call_ctl!(ctx, AV1E_SET_ENABLE_ANGLE_DELTA, 0);
    call_ctl!(ctx, AV1E_SET_ENABLE_FILTER_INTRA, 0);
    call_ctl!(ctx, AV1E_SET_INTRA_DEFAULT_TX_ONLY, 1);
    call_ctl!(ctx, AV1E_SET_DISABLE_TRELLIS_QUANT, 1);
    call_ctl!(ctx, AV1E_SET_ENABLE_DIST_WTD_COMP, 0);
    call_ctl!(ctx, AV1E_SET_ENABLE_DIFF_WTD_COMP, 0);
    call_ctl!(ctx, AV1E_SET_ENABLE_DUAL_FILTER, 0);
    call_ctl!(ctx, AV1E_SET_ENABLE_INTERINTRA_COMP, 0);
    call_ctl!(ctx, AV1E_SET_ENABLE_INTERINTRA_WEDGE, 0);
    call_ctl!(ctx, AV1E_SET_ENABLE_INTRA_EDGE_FILTER, 0);
    call_ctl!(ctx, AV1E_SET_ENABLE_INTRABC, 0);
    call_ctl!(ctx, AV1E_SET_ENABLE_MASKED_COMP, 0);
    call_ctl!(ctx, AV1E_SET_ENABLE_PAETH_INTRA, 0);
    call_ctl!(ctx, AV1E_SET_ENABLE_QM, 0);
    call_ctl!(ctx, AV1E_SET_ENABLE_RECT_PARTITIONS, 0);
    call_ctl!(ctx, AV1E_SET_ENABLE_RESTORATION, 0);
    call_ctl!(ctx, AV1E_SET_ENABLE_SMOOTH_INTERINTRA, 0);
    call_ctl!(ctx, AV1E_SET_ENABLE_TX64, 0);
    call_ctl!(ctx, AV1E_SET_MAX_REFERENCE_FRAMES, 3);

    Ok(())
  }
}

impl EncoderApi for AomEncoder {
  fn new(cfg: crate::codec::EncoderCfg) -> ResultType<Self>
  where
    Self: Sized,
  {
    match cfg {
      crate::codec::EncoderCfg::AOM(config) => {
        let i = call_aom_ptr!(aom_codec_av1_cx());
        let c = webrtc::enc_cfg(i, config)?;

        let mut ctx = Default::default();
        // Flag options: AOM_CODEC_USE_PSNR and AOM_CODEC_USE_HIGHBITDEPTH
        let flags: aom_codec_flags_t = 0;
        call_aom!(aom_codec_enc_init_ver(
          &mut ctx,
          i,
          &c,
          flags,
          AOM_ENCODER_ABI_VERSION as _
        ));
        webrtc::set_controls(&mut ctx, &c)?;
        Ok(Self {
          ctx,
          width: config.width as _,
          height: config.height as _,
        })
      }
      _ => Err(anyhow!("encoder type mismatch")),
    }
  }

  fn encode_to_message(
    &mut self,
    frame: &[u8],
    ms: i64,
  ) -> ResultType<Message> {
    let mut frames = Vec::new();
    for ref frame in self
      .encode(ms, frame, STRIDE_ALIGN)
      .with_context(|| "Failed to encode")?
    {
      frames.push(Self::create_frame(frame));
    }
    if frames.len() > 0 {
      Ok(Self::create_msg(frames))
    } else {
      Err(anyhow!("no valid frame"))
    }
  }

  fn use_yuv(&self) -> bool {
    true
  }

  fn set_quality(&mut self, quality: Quality) -> ResultType<()> {
    let mut c = unsafe { *self.ctx.config.enc.to_owned() };
    let (q_min, q_max, b) = Self::convert_quality(quality);
    if q_min > 0 && q_min < q_max && q_max < 64 {
      c.rc_min_quantizer = q_min;
      c.rc_max_quantizer = q_max;
    }
    let bitrate = base_bitrate(self.width as _, self.height as _) * b / 100;
    if bitrate > 0 {
      c.rc_target_bitrate = bitrate;
    }
    call_aom!(aom_codec_enc_config_set(&mut self.ctx, &c));
    Ok(())
  }

  fn bitrate(&self) -> u32 {
    let c = unsafe { *self.ctx.config.enc.to_owned() };
    c.rc_target_bitrate
  }
}

impl AomEncoder {
  pub fn encode(
    &mut self,
    pts: i64,
    data: &[u8],
    stride_align: usize,
  ) -> Result<EncodeFrames> {
    if 2 * data.len() < 3 * self.width * self.height {
      return Err(Error::FailedCall("len not enough".to_string()));
    }

    let mut image = Default::default();
    call_aom_ptr!(aom_img_wrap(
      &mut image,
      aom_img_fmt::AOM_IMG_FMT_I420,
      self.width as _,
      self.height as _,
      stride_align as _,
      data.as_ptr() as _,
    ));

    call_aom!(aom_codec_encode(
      &mut self.ctx,
      &image,
      pts as _,
      1, // Duration
      0, // Flags
    ));

    Ok(EncodeFrames {
      ctx: &mut self.ctx,
      iter: ptr::null(),
    })
  }

  #[inline]
  pub fn create_msg(frames: Vec<EncodedVideoFrame>) -> Message {
    let mut msg_out = Message::new();
    let mut vf = VideoFrame::new();
    let av1s = EncodedVideoFrames {
      frames: frames.into(),
      ..Default::default()
    };
    vf.set_av1s(av1s);
    msg_out.set_video_frame(vf);
    msg_out
  }

  #[inline]
  fn create_frame(frame: &EncodeFrame) -> EncodedVideoFrame {
    EncodedVideoFrame {
      data: Bytes::from(frame.data.to_vec()),
      key: frame.key,
      pts: frame.pts,
      ..Default::default()
    }
  }

  pub fn convert_quality(quality: Quality) -> (u32, u32, u32) {
    // we can use lower bitrate for av1
    match quality {
      Quality::Best => (12, 25, 100),
      Quality::Balanced => (12, 35, 100 * 2 / 3),
      Quality::Low => (18, 45, 50),
      Quality::Custom(b) => {
        let (q_min, q_max) = Self::calc_q_values(b);
        (q_min, q_max, b)
      }
    }
  }

  #[inline]
  fn calc_q_values(b: u32) -> (u32, u32) {
    let b = std::cmp::min(b, 200);
    let q_min1: i32 = 24;
    let q_min2 = 5;
    let q_max1 = 45;
    let q_max2 = 25;

    let t = b as f32 / 200.0;

    let q_min: u32 =
      ((1.0 - t) * q_min1 as f32 + t * q_min2 as f32).round() as u32;
    let q_max = ((1.0 - t) * q_max1 as f32 + t * q_max2 as f32).round() as u32;

    (q_min, q_max)
  }
}

impl Drop for AomEncoder {
  fn drop(&mut self) {
    unsafe {
      let result = aom_codec_destroy(&mut self.ctx);
      if result != aom_codec_err_t::AOM_CODEC_OK {
        panic!("failed to destroy aom codec");
      }
    }
  }
}

pub struct EncodeFrames<'a> {
  ctx: &'a mut aom_codec_ctx_t,
  iter: aom_codec_iter_t,
}

impl<'a> Iterator for EncodeFrames<'a> {
  type Item = EncodeFrame<'a>;
  fn next(&mut self) -> Option<Self::Item> {
    loop {
      unsafe {
        let pkt = aom_codec_get_cx_data(self.ctx, &mut self.iter);
        if pkt.is_null() {
          return None;
        } else if (*pkt).kind == aom_codec_cx_pkt_kind::AOM_CODEC_CX_FRAME_PKT {
          let f = &(*pkt).data.frame;
          return Some(Self::Item {
            data: slice::from_raw_parts(f.buf as _, f.sz as _),
            key: (f.flags & AOM_FRAME_IS_KEY) != 0,
            pts: f.pts,
          });
        } else {
          // Ignore the packet.
        }
      }
    }
  }
}

pub struct AomDecoder {
  ctx: aom_codec_ctx_t,
}

impl AomDecoder {
  pub fn new() -> Result<Self> {
    let i = call_aom_ptr!(aom_codec_av1_dx());
    let mut ctx = Default::default();
    let cfg = aom_codec_dec_cfg_t {
      threads: codec_thread_num() as _,
      w: 0,
      h: 0,
      allow_lowbitdepth: 1,
    };
    call_aom!(aom_codec_dec_init_ver(
      &mut ctx,
      i,
      &cfg,
      0,
      AOM_DECODER_ABI_VERSION as _,
    ));
    Ok(Self { ctx })
  }

  pub fn decode(&mut self, data: &[u8]) -> Result<DecodeFrames> {
    call_aom!(aom_codec_decode(
      &mut self.ctx,
      data.as_ptr(),
      data.len() as _,
      ptr::null_mut(),
    ));

    Ok(DecodeFrames {
      ctx: &mut self.ctx,
      iter: ptr::null(),
    })
  }

  /// Notify the decoder to return any pending frame
  pub fn flush(&mut self) -> Result<DecodeFrames> {
    call_aom!(aom_codec_decode(
      &mut self.ctx,
      ptr::null(),
      0,
      ptr::null_mut(),
    ));
    Ok(DecodeFrames {
      ctx: &mut self.ctx,
      iter: ptr::null(),
    })
  }
}

impl Drop for AomDecoder {
  fn drop(&mut self) {
    unsafe {
      let result = aom_codec_destroy(&mut self.ctx);
      if result != aom_codec_err_t::AOM_CODEC_OK {
        panic!("failed to destroy aom codec");
      }
    }
  }
}

pub struct DecodeFrames<'a> {
  ctx: &'a mut aom_codec_ctx_t,
  iter: aom_codec_iter_t,
}

impl<'a> Iterator for DecodeFrames<'a> {
  type Item = Image;
  fn next(&mut self) -> Option<Self::Item> {
    let img = unsafe { aom_codec_get_frame(self.ctx, &mut self.iter) };
    if img.is_null() {
      return None;
    } else {
      return Some(Image(img));
    }
  }
}

pub struct Image(*mut aom_image_t);
impl Image {
  #[inline]
  pub fn new() -> Self {
    Self(std::ptr::null_mut())
  }

  #[inline]
  pub fn is_null(&self) -> bool {
    self.0.is_null()
  }

  #[inline]
  pub fn format(&self) -> aom_img_fmt_t {
    self.inner().fmt
  }

  #[inline]
  pub fn inner(&self) -> &aom_image_t {
    unsafe { &*self.0 }
  }
}

impl GoogleImage for Image {
  #[inline]
  fn width(&self) -> usize {
    self.inner().d_w as _
  }

  #[inline]
  fn height(&self) -> usize {
    self.inner().d_h as _
  }

  #[inline]
  fn stride(&self) -> Vec<i32> {
    self.inner().stride.iter().map(|x| *x as i32).collect()
  }

  #[inline]
  fn planes(&self) -> Vec<*mut u8> {
    self.inner().planes.iter().map(|p| *p as *mut u8).collect()
  }
}

impl Drop for Image {
  fn drop(&mut self) {
    if !self.0.is_null() {
      unsafe { aom_img_free(self.0) };
    }
  }
}

unsafe impl Send for aom_codec_ctx_t {}
