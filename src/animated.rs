//! Animated AVIF and HEIC container serialization.
//!
//! Takes pre-encoded frame data and produces a timed-sequence file with a
//! `ftyp + meta + moov + mdat` structure. The frames may be AV1-coded, giving
//! an `avis` file whose track holds `av01` samples, or HEVC-coded, giving an
//! `msf1`/`hevc` file whose track holds `hvc1` samples. Everything between —
//! the track layout, the sample tables, the still item declared for players
//! that only read images — is the same either way, because it is container
//! structure and not codec structure.

use crate::boxes::{Av1CBox, ClapBox, ClliBox, ColrBox, HvcCBox, MdcvBox, MpegBox};
use crate::writer::Writer;

/// A single pre-encoded animation frame.
#[derive(Clone)]
#[non_exhaustive]
pub struct AnimFrame<'a> {
    /// AV1-encoded color data for this frame.
    pub color: &'a [u8],
    /// AV1-encoded alpha data for this frame (if present).
    pub alpha: Option<&'a [u8]>,
    /// Duration in timescale ticks.
    pub duration: u32,
    /// Whether this is a sync (key) frame.
    pub is_sync: bool,
}

impl<'a> AnimFrame<'a> {
    /// Create a frame with color data and duration. Alpha defaults to `None`, sync to `false`.
    pub fn new(color: &'a [u8], duration: u32) -> Self {
        Self { color, alpha: None, duration, is_sync: false }
    }

    /// Set alpha data for this frame.
    pub fn with_alpha(mut self, alpha: &'a [u8]) -> Self {
        self.alpha = Some(alpha);
        self
    }

    /// Mark this frame as a sync (key) frame.
    pub fn with_sync(mut self, is_sync: bool) -> Self {
        self.is_sync = is_sync;
        self
    }
}

/// Builder for animated AVIF container serialization.
///
/// Holds codec configuration and optional metadata. Call [`serialize`](AnimatedImage::serialize)
/// with per-encode data (dimensions, frames, sequence headers) to produce the AVIF file.
pub struct AnimatedImage {
    timescale: u32,
    loop_count: u32,
    color_config: Av1CBox,
    alpha_config: Option<Av1CBox>,
    /// When set, frames are HEVC-coded and the AV1 configurations are unused.
    hevc_config: Option<HvcCBox>,
    alpha_hevc_config: Option<HvcCBox>,
    colr: Option<ColrBox>,
    clli: Option<ClliBox>,
    mdcv: Option<MdcvBox>,
    clap: Option<ClapBox>,
}

/// How one track's samples are configured, and everything that follows from
/// it: the sample entry's type, the item type a still frame gets, and the
/// pixel description the `pixi` property carries.
///
/// The two codecs differ in where the decoder's startup state lives. AV1 keeps
/// the sequence header in the bitstream and `av1C` merely repeats it, so the
/// caller supplies it alongside the configuration. HEVC keeps its parameter
/// sets only in `hvcC`, so there is nothing beside it to pass.
#[derive(Clone, Copy)]
enum CodecConfig<'a> {
    Av1 {
        config: &'a Av1CBox,
        seq_header: &'a [u8],
    },
    Hevc {
        config: &'a HvcCBox,
    },
}

impl CodecConfig<'_> {
    /// The URN naming an alpha auxiliary track for this codec.
    ///
    /// A reader identifies an alpha track by this string, not by the `auxv`
    /// handler alone — libheif and libavif both key on it, and a track without
    /// one is reported as a second picture track rather than as transparency.
    /// HEVC has its own; AV1 uses MIAF's.
    fn alpha_aux_urn(&self) -> &'static str {
        match self {
            Self::Av1 { .. } => "urn:mpeg:mpegB:cicp:systems:auxiliary:alpha",
            Self::Hevc { .. } => "urn:mpeg:hevc:2015:auxid:1",
        }
    }

    /// The `stsd` sample entry type, which is also the still item's type.
    fn entry_type(&self) -> &'static [u8; 4] {
        match self {
            Self::Av1 { .. } => b"av01",
            Self::Hevc { .. } => b"hvc1",
        }
    }

    /// Bits per channel, and how many channels there are.
    fn pixel_description(&self) -> (u8, u8) {
        match self {
            Self::Av1 { config, .. } => {
                let depth = bit_depth_from_av1c(config);
                (if config.monochrome { 1 } else { 3 }, depth)
            }
            Self::Hevc { config } => {
                // chroma_format_idc 0 is monochrome; 1, 2 and 3 are 4:2:0,
                // 4:2:2 and 4:4:4, all of which carry three planes.
                let channels = if config.chroma_format_idc == 0 { 1 } else { 3 };
                (channels, config.bit_depth_luma)
            }
        }
    }

    /// Write the decoder configuration property or sub-box.
    fn write_box(&self, out: &mut Vec<u8>) {
        match self {
            Self::Av1 { config, seq_header } => write_av1c_box(out, config, seq_header),
            Self::Hevc { config } => {
                // Reuse the record the rest of the crate writes rather than
                // open-coding a second one here: the parser has a test holding
                // the exact bytes, and two writers would drift apart silently.
                let mut writer = Writer::new(out);
                let _ = config.write(&mut writer);
            }
        }
    }
}

impl Default for AnimatedImage {
    fn default() -> Self { Self::new() }
}

impl AnimatedImage {
    /// Create with sensible defaults (timescale=1000ms, infinite loop, 8-bit 4:2:0).
    pub fn new() -> Self {
        Self {
            timescale: 1000,
            loop_count: 0,
            color_config: Av1CBox::default(),
            alpha_config: None,
            hevc_config: None,
            alpha_hevc_config: None,
            colr: None,
            clli: None,
            mdcv: None,
            clap: None,
        }
    }

    /// Timescale in ticks per second. Default: 1000 (milliseconds).
    pub fn set_timescale(&mut self, timescale: u32) -> &mut Self { self.timescale = timescale; self }
    /// How many times the sequence plays. Zero means forever. Default: 0.
    ///
    /// ISO 23008-12 §9.6.1 counts plays as the track's duration divided by the
    /// edit list's segment, so a finite count is written by stating a track
    /// duration that many times the media's, and forever by leaving the track
    /// duration indefinite. Both libheif and libavif read it that way.
    pub fn set_loop_count(&mut self, loop_count: u32) -> &mut Self { self.loop_count = loop_count; self }
    /// AV1 codec configuration for the color track.
    pub fn set_color_config(&mut self, config: Av1CBox) -> &mut Self { self.color_config = config; self }
    /// AV1 codec configuration for the alpha track.
    pub fn set_alpha_config(&mut self, config: Av1CBox) -> &mut Self { self.alpha_config = Some(config); self }
    /// HEVC codec configuration for the color track.
    ///
    /// Setting it makes the whole file HEVC: `hvc1` samples, an `msf1` brand,
    /// and the sequence-header arguments to [`serialize`](Self::serialize)
    /// ignored, since HEVC's parameter sets live in this record.
    pub fn set_hevc_config(&mut self, config: HvcCBox) -> &mut Self { self.hevc_config = Some(config); self }
    /// HEVC codec configuration for the alpha track.
    ///
    /// Only consulted when the color track is HEVC too. A file mixing codecs
    /// between its colour and alpha tracks is not something any reader expects.
    pub fn set_alpha_hevc_config(&mut self, config: HvcCBox) -> &mut Self { self.alpha_hevc_config = Some(config); self }
    /// CICP color info (nclx).
    pub fn set_colr(&mut self, colr: ColrBox) -> &mut Self { self.colr = Some(colr); self }
    /// Content Light Level Information (HDR).
    pub fn set_clli(&mut self, clli: ClliBox) -> &mut Self { self.clli = Some(clli); self }
    /// Mastering Display Colour Volume (HDR).
    pub fn set_mdcv(&mut self, mdcv: MdcvBox) -> &mut Self { self.mdcv = Some(mdcv); self }
    /// The region of each coded frame that is the picture.
    ///
    /// Needed whenever the coded size is not the size to show, which for HEVC
    /// is any picture smaller than a coding tree unit: the codec cannot code
    /// one, so the encoder pads and the container says where the real picture
    /// is. Written into the sample entry, and the track header then states the
    /// cropped size rather than the coded one — a player takes its dimensions
    /// from there.
    pub fn set_clean_aperture(&mut self, clap: ClapBox) -> &mut Self { self.clap = Some(clap); self }

    /// Serialize an animated file from pre-encoded frame data.
    ///
    /// The sequence-header arguments describe AV1 frames and are ignored when
    /// [`set_hevc_config`](Self::set_hevc_config) has been called.
    pub fn serialize(&self, width: u32, height: u32, frames: &[AnimFrame<'_>],
                     color_seq_header: &[u8], alpha_seq_header: Option<&[u8]>) -> Vec<u8> {
    let color_codec = match self.hevc_config.as_ref() {
        Some(config) => CodecConfig::Hevc { config },
        None => CodecConfig::Av1 { config: &self.color_config, seq_header: color_seq_header },
    };
    // An alpha track needs alpha data on the frames and a configuration to
    // decode it against, in whichever codec the colour track chose.
    let alpha_codec = match self.hevc_config.as_ref() {
        Some(_) => self.alpha_hevc_config.as_ref().map(|config| CodecConfig::Hevc { config }),
        None => match (self.alpha_config.as_ref(), alpha_seq_header) {
            (Some(config), Some(seq_header)) => Some(CodecConfig::Av1 { config, seq_header }),
            _ => None,
        },
    };
    let has_alpha = frames.iter().any(|f| f.alpha.is_some()) && alpha_codec.is_some();

    let total_duration: u64 = frames.iter().map(|f| u64::from(f.duration)).sum();
    let durations: Vec<u32> = frames.iter().map(|f| f.duration).collect();
    let color_frames: Vec<&[u8]> = frames.iter().map(|f| f.color).collect();
    let alpha_frames: Vec<&[u8]> = if has_alpha {
        frames.iter().map(|f| f.alpha.unwrap_or(&[])).collect()
    } else {
        Vec::new()
    };
    let sync_indices: Vec<u32> = frames.iter().enumerate()
        .filter(|(_, f)| f.is_sync)
        .map(|(i, _)| (i + 1) as u32) // 1-indexed
        .collect();

    let next_track_id = if has_alpha { 3 } else { 2 };

    let mut out = Vec::new();

    // ftyp
    write_ftyp(&mut out, color_codec);

    // meta — declares primary item for still-frame interop. Returns the byte position
    // of the iloc extent_offset placeholder so we can patch it without scanning.
    let iloc_offset_pos = write_meta(
        &mut out,
        width,
        height,
        color_codec,
        color_frames.first().map(|f| f.len() as u32).unwrap_or(0),
        self.colr.as_ref(),
        self.clli.as_ref(),
        self.mdcv.as_ref(),
        self.clap.as_ref(),
    );

    // moov — each write_track returns the byte position of its stco placeholder.
    //
    // The presented duration is what says how often the sequence plays: a
    // reader divides it by the edit list's segment. One play needs no edit
    // list at all, which is also what libheif writes for that case.
    let presented = Presentation::of(self.loop_count, total_duration);
    let moov_pos = begin_box(&mut out, b"moov");
    write_mvhd(&mut out, self.timescale, total_duration, next_track_id, presented);
    let color_stco_pos = write_track(
        &mut out, 1, width, height,
        self.timescale, total_duration,
        &color_frames, &durations, &sync_indices,
        color_codec, self.clap.as_ref(),
        false, presented,
    );
    let alpha_stco_pos = if has_alpha {
        Some(write_track(
            &mut out, 2, width, height,
            self.timescale, total_duration,
            &alpha_frames, &durations, &sync_indices,
            alpha_codec.expect("has_alpha implies an alpha configuration"),
            self.clap.as_ref(),
            true, presented,
        ))
    } else {
        None
    };
    end_box(&mut out, moov_pos);

    // mdat
    let mdat_pos = begin_box(&mut out, b"mdat");
    let mdat_data_start = out.len();
    for frame in &color_frames {
        out.extend_from_slice(frame);
    }
    let alpha_data_start = out.len();
    for frame in &alpha_frames {
        out.extend_from_slice(frame);
    }
    end_box(&mut out, mdat_pos);

    // Patch placeholder offsets at exact recorded positions. We never scan the buffer
    // for sentinel byte patterns: AV1 frame payloads can legitimately contain those
    // bytes (and an attacker could deliberately seed them), so a scan-and-replace
    // approach would silently corrupt user data.
    write_u32_at(&mut out, iloc_offset_pos, mdat_data_start as u32);
    write_u32_at(&mut out, color_stco_pos, mdat_data_start as u32);
    if let Some(pos) = alpha_stco_pos {
        write_u32_at(&mut out, pos, alpha_data_start as u32);
    }

    out
    }
}

// ─── Low-level helpers ───────────────────────────────────────────────

fn write_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn write_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn write_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_be_bytes());
}

/// Start a box, return position for later size patching.
fn begin_box(out: &mut Vec<u8>, box_type: &[u8; 4]) -> usize {
    let pos = out.len();
    write_u32(out, 0); // placeholder
    out.extend_from_slice(box_type);
    pos
}

/// Patch box size at the given position.
fn end_box(out: &mut [u8], pos: usize) {
    let size = (out.len() - pos) as u32;
    out[pos..pos + 4].copy_from_slice(&size.to_be_bytes());
}

fn write_fullbox(out: &mut Vec<u8>, version: u8, flags: u32) {
    out.push(version);
    out.push((flags >> 16) as u8);
    out.push((flags >> 8) as u8);
    out.push(flags as u8);
}

const STCO_PLACEHOLDER: u32 = 0xDEAD_BEEF;
const ILOC_PLACEHOLDER: u32 = 0xDEAD_BEE0;

// ─── Top-level boxes ─────────────────────────────────────────────────

fn write_ftyp(out: &mut Vec<u8>, codec: CodecConfig<'_>) {
    let pos = begin_box(out, b"ftyp");
    match codec {
        CodecConfig::Av1 { .. } => {
            out.extend_from_slice(b"avis"); // major brand
            write_u32(out, 0); // minor version
            out.extend_from_slice(b"avis"); // compatible brands
            out.extend_from_slice(b"avif");
            out.extend_from_slice(b"mif1");
            out.extend_from_slice(b"miaf");
            out.extend_from_slice(b"iso8");
        }
        CodecConfig::Hevc { .. } => {
            // The brand set libheif writes for a HEIC sequence, in its order.
            // `hevc` leads because the file is HEVC-coded throughout; `msf1`
            // says there is an image sequence and `heic` that the still item
            // inside it is readable on its own.
            out.extend_from_slice(b"hevc"); // major brand
            write_u32(out, 0); // minor version
            out.extend_from_slice(b"mif1"); // compatible brands
            out.extend_from_slice(b"heic");
            out.extend_from_slice(b"miaf");
            out.extend_from_slice(b"msf1");
            out.extend_from_slice(b"iso8");
            out.extend_from_slice(b"hevc");
        }
    }
    end_box(out, pos);
}

#[allow(clippy::too_many_arguments)]
fn write_meta(
    out: &mut Vec<u8>,
    width: u32,
    height: u32,
    codec: CodecConfig<'_>,
    first_frame_len: u32,
    colr: Option<&ColrBox>,
    clli: Option<&ClliBox>,
    mdcv: Option<&MdcvBox>,
    clap: Option<&ClapBox>,
) -> usize {
    // Records the byte position of the iloc extent_offset placeholder.
    let iloc_offset_pos: usize;
    let meta_pos = begin_box(out, b"meta");
    write_fullbox(out, 0, 0);

    // hdlr
    {
        let pos = begin_box(out, b"hdlr");
        write_fullbox(out, 0, 0);
        write_u32(out, 0); // pre_defined
        out.extend_from_slice(b"pict");
        out.extend_from_slice(&[0u8; 12]); // reserved
        out.push(0); // name (null-terminated empty)
        end_box(out, pos);
    }

    // pitm
    {
        let pos = begin_box(out, b"pitm");
        write_fullbox(out, 0, 0);
        write_u16(out, 1); // item_id
        end_box(out, pos);
    }

    // iloc
    {
        let pos = begin_box(out, b"iloc");
        write_fullbox(out, 0, 0);
        out.push(0x44); // offset_size=4, length_size=4
        out.push(0x00); // base_offset_size=0, reserved=0
        write_u16(out, 1); // item_count
        write_u16(out, 1); // item_id
        write_u16(out, 0); // data_reference_index
        write_u16(out, 1); // extent_count
        iloc_offset_pos = out.len();
        write_u32(out, ILOC_PLACEHOLDER); // extent_offset (patched later)
        write_u32(out, first_frame_len); // extent_length
        end_box(out, pos);
    }

    // iinf
    {
        let iinf_pos = begin_box(out, b"iinf");
        write_fullbox(out, 0, 0);
        write_u16(out, 1); // entry_count

        let infe_pos = begin_box(out, b"infe");
        write_fullbox(out, 2, 0);
        write_u16(out, 1); // item_id
        write_u16(out, 0); // protection_index
        out.extend_from_slice(codec.entry_type());
        out.push(0); // name
        end_box(out, infe_pos);

        end_box(out, iinf_pos);
    }

    // iprp (ipco + ipma)
    {
        let iprp_pos = begin_box(out, b"iprp");

        // ipco
        {
            let ipco_pos = begin_box(out, b"ipco");

            // Property 1: ispe
            {
                let pos = begin_box(out, b"ispe");
                write_fullbox(out, 0, 0);
                write_u32(out, width);
                write_u32(out, height);
                end_box(out, pos);
            }

            // Property 2: the decoder configuration, av1C or hvcC
            codec.write_box(out);

            // Property 3: pixi
            {
                let pos = begin_box(out, b"pixi");
                write_fullbox(out, 0, 0);
                let (channels, depth) = codec.pixel_description();
                out.push(channels);
                for _ in 0..channels {
                    out.push(depth);
                }
                end_box(out, pos);
            }

            // Property 4: colr (optional)
            if let Some(colr) = colr
                && *colr != ColrBox::default() {
                    write_colr_nclx(out, colr);
                }

            // Property 5: clli (optional)
            if let Some(clli) = clli {
                write_clli(out, clli);
            }

            // Property 6: mdcv (optional)
            if let Some(mdcv) = mdcv {
                write_mdcv(out, mdcv);
            }

            // Property 7: clap (optional). The still item is coded at the
            // padded size like every frame, so it needs the same crop.
            if let Some(clap) = clap {
                let mut writer = Writer::new(out);
                let _ = clap.write(&mut writer);
            }

            end_box(out, ipco_pos);
        }

        // ipma
        {
            let pos = begin_box(out, b"ipma");
            write_fullbox(out, 0, 0);
            write_u32(out, 1); // entry_count
            write_u16(out, 1); // item_id
            // Count associations: ispe + config(essential) + pixi + optional colr/clli/mdcv
            let mut assoc_count: u8 = 3;
            let has_colr = colr.is_some_and(|c| *c != ColrBox::default());
            if has_colr { assoc_count += 1; }
            if clli.is_some() { assoc_count += 1; }
            if mdcv.is_some() { assoc_count += 1; }
            if clap.is_some() { assoc_count += 1; }
            out.push(assoc_count);
            out.push(0x01); // property 1 (ispe), not essential
            out.push(0x82); // property 2 (av1C or hvcC), essential
            out.push(0x03); // property 3 (pixi), not essential
            let mut next_prop = 4u8;
            if has_colr {
                out.push(next_prop);
                next_prop += 1;
            }
            if clli.is_some() {
                out.push(next_prop);
                next_prop += 1;
            }
            if mdcv.is_some() {
                out.push(next_prop);
                next_prop += 1;
            }
            if clap.is_some() {
                // Essential: a reader that ignores it shows the padding.
                out.push(0x80 | next_prop);
                let _ = next_prop;
            }
            end_box(out, pos);
        }

        end_box(out, iprp_pos);
    }

    end_box(out, meta_pos);
    iloc_offset_pos
}

/// All-ones, which ISO 14496-12 reserves to mean a duration that is not known
/// in advance. Combined with an edit list that repeats, ISO 23008-12 §9.6.1
/// makes it the way a sequence says it plays forever, and it is what both
/// libheif and libavif read: the repeat flag alone gets counted against a
/// finite duration and comes out as exactly one play.
const INDEFINITE_DURATION: u64 = u64::MAX;

/// How long the sequence is presented for, which is how often it plays.
#[derive(Clone, Copy)]
enum Presentation {
    /// One play. No edit list; the track lasts exactly as long as its media.
    Once,
    /// `n` plays, stated as a track duration `n` times the media's.
    Repeats(u64),
    /// Forever, stated by leaving the duration indefinite.
    Forever,
}

impl Presentation {
    fn of(loop_count: u32, media_duration: u64) -> Self {
        match loop_count {
            0 => Self::Forever,
            1 => Self::Once,
            n => match media_duration.checked_mul(u64::from(n)) {
                // A count so large the duration overflows is indistinguishable
                // from forever at any rate a reader would play it.
                Some(total) => Self::Repeats(total),
                None => Self::Forever,
            },
        }
    }

    /// Whether an edit list is written at all.
    const fn repeats(self) -> bool {
        !matches!(self, Self::Once)
    }

    /// The duration to state for the movie and the track.
    const fn duration(self, media_duration: u64) -> u64 {
        match self {
            Self::Once => media_duration,
            Self::Repeats(total) => total,
            Self::Forever => INDEFINITE_DURATION,
        }
    }
}

fn write_mvhd(
    out: &mut Vec<u8>,
    timescale: u32,
    duration: u64,
    next_track_id: u32,
    presented: Presentation,
) {
    let pos = begin_box(out, b"mvhd");
    write_fullbox(out, 1, 0);
    write_u64(out, 0); // creation_time
    write_u64(out, 0); // modification_time
    write_u32(out, timescale);
    write_u64(out, presented.duration(duration));
    write_u32(out, 0x0001_0000); // rate 1.0
    write_u16(out, 0x0100); // volume 1.0
    out.extend_from_slice(&[0u8; 10]); // reserved
    // Identity matrix (3×3 fixed point)
    for &v in &[0x0001_0000u32, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000] {
        write_u32(out, v);
    }
    out.extend_from_slice(&[0u8; 24]); // pre_defined
    write_u32(out, next_track_id);
    end_box(out, pos);
}

#[allow(clippy::too_many_arguments)]
fn write_track(
    out: &mut Vec<u8>,
    track_id: u32,
    width: u32,
    height: u32,
    timescale: u32,
    duration: u64,
    frames: &[&[u8]],
    durations: &[u32],
    sync_indices: &[u32],
    codec: CodecConfig<'_>,
    clap: Option<&ClapBox>,
    is_alpha: bool,
    presented: Presentation,
) -> usize {
    // Records the byte position of the stco chunk_offset placeholder.
    let stco_offset_pos: usize;
    let trak_pos = begin_box(out, b"trak");

    // tkhd
    {
        let pos = begin_box(out, b"tkhd");
        let flags = if is_alpha { 1 } else { 3 }; // enabled | in_movie
        write_fullbox(out, 1, flags);
        write_u64(out, 0); // creation_time
        write_u64(out, 0); // modification_time
        write_u32(out, track_id);
        write_u32(out, 0); // reserved
        // The TRACK duration is the one a reader divides the edit list's
        // segment into to get the number of plays, so it states the whole
        // presentation rather than the media. The media duration in `mdhd`
        // stays finite — the samples really are that long, and libheif checks
        // the edit list's segment against it before believing the repeat.
        write_u64(out, presented.duration(duration));
        out.extend_from_slice(&[0u8; 8]); // reserved
        write_u16(out, 0); // layer
        write_u16(out, 0); // alternate_group
        write_u16(out, 0); // volume
        write_u16(out, 0); // reserved
        for &v in &[0x0001_0000u32, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000] {
            write_u32(out, v);
        }
        // tkhd width/height are 16.16 fixed-point u32s. Widening to u64 prevents
        // a debug-build panic / release-build wrap when width or height >= 65536;
        // saturate to u32::MAX so very large dimensions still produce a well-formed
        // box (the integer part is clamped to 0xFFFF, which is the spec maximum).
        // The DISPLAYED size, which is not the coded size when the frames
        // were padded to reach one coding tree unit. A player reads its
        // dimensions here, so stating the padded size shows the padding.
        let (display_width, display_height) = display_size(width, height, clap);
        write_u32(out, fixed_16_16_saturating(display_width));
        write_u32(out, fixed_16_16_saturating(display_height));
        end_box(out, pos);
    }

    // edts/elst, one segment covering the media exactly once.
    //
    // ISO 14496-12 gives the edit list a flags field whose bit 0 says the
    // segment repeats. How often is not in this box: a reader divides the
    // track's duration, written above, by this segment's. So the segment is
    // always one pass over the media and the count lives in the duration.
    if presented.repeats() {
        let edts_pos = begin_box(out, b"edts");
        {
            let pos = begin_box(out, b"elst");
            write_fullbox(out, 0, 1); // version 0, flags bit 0 = repeat
            write_u32(out, 1); // entry_count
            write_u32(out, duration as u32); // segment_duration
            write_u32(out, 0); // media_time, from the start
            write_u16(out, 1); // media_rate_integer
            write_u16(out, 0); // media_rate_fraction
            end_box(out, pos);
        }
        end_box(out, edts_pos);
    }

    // mdia
    {
        let mdia_pos = begin_box(out, b"mdia");

        // mdhd
        {
            let pos = begin_box(out, b"mdhd");
            write_fullbox(out, 1, 0);
            write_u64(out, 0); // creation_time
            write_u64(out, 0); // modification_time
            write_u32(out, timescale);
            write_u64(out, duration);
            write_u16(out, 0x55C4); // language = "und"
            write_u16(out, 0);
            end_box(out, pos);
        }

        // hdlr
        {
            let pos = begin_box(out, b"hdlr");
            write_fullbox(out, 0, 0);
            write_u32(out, 0);
            if is_alpha {
                out.extend_from_slice(b"auxv");
            } else {
                out.extend_from_slice(b"pict");
            }
            out.extend_from_slice(&[0u8; 12]);
            out.extend_from_slice(if is_alpha { b"Alpha\0" } else { b"Color\0" });
            end_box(out, pos);
        }


        // minf
        {
            let minf_pos = begin_box(out, b"minf");

            // vmhd
            {
                let pos = begin_box(out, b"vmhd");
                write_fullbox(out, 0, 1);
                out.extend_from_slice(&[0u8; 8]); // graphicsmode + opcolor
                end_box(out, pos);
            }

            // dinf + dref
            {
                let dinf_pos = begin_box(out, b"dinf");
                let dref_pos = begin_box(out, b"dref");
                write_fullbox(out, 0, 0);
                write_u32(out, 1);
                let url_pos = begin_box(out, b"url ");
                write_fullbox(out, 0, 1); // self-contained
                end_box(out, url_pos);
                end_box(out, dref_pos);
                end_box(out, dinf_pos);
            }

            // stbl
            {
                let stbl_pos = begin_box(out, b"stbl");

                // stsd with the sample entry and its decoder configuration
                {
                    let pos = begin_box(out, b"stsd");
                    write_fullbox(out, 0, 0);
                    write_u32(out, 1); // entry_count

                    let entry_pos = begin_box(out, codec.entry_type());
                    out.extend_from_slice(&[0u8; 6]); // reserved
                    write_u16(out, 1); // data_reference_index
                    write_u16(out, 0); // pre_defined
                    write_u16(out, 0); // reserved
                    out.extend_from_slice(&[0u8; 12]); // pre_defined
                    // VisualSampleEntry width/height are u16. Saturate rather than
                    // silently wrap: `70000 as u16 = 4464` would emit a corrupted box.
                    write_u16(out, width.min(0xFFFF) as u16);
                    write_u16(out, height.min(0xFFFF) as u16);
                    write_u32(out, 0x0048_0000); // horiz resolution 72dpi
                    write_u32(out, 0x0048_0000); // vert resolution 72dpi
                    write_u32(out, 0); // reserved
                    write_u16(out, 1); // frame_count
                    out.extend_from_slice(&[0u8; 32]); // compressorname
                    write_u16(out, 0x0018); // depth = 24
                    out.extend_from_slice(&0xFFFFu16.to_be_bytes()); // pre_defined = -1

                    codec.write_box(out);

                    // auxi, naming what an auxiliary track carries. It lives
                    // in the sample entry, which is where libheif looks — a
                    // reader that does not find it sees a second picture
                    // track and shows it as one, rather than as transparency.
                    if is_alpha {
                        let pos = begin_box(out, b"auxi");
                        write_fullbox(out, 0, 0);
                        out.extend_from_slice(codec.alpha_aux_urn().as_bytes());
                        out.push(0);
                        end_box(out, pos);
                    }

                    // clap, when the coded frame is larger than the picture.
                    if let Some(clap) = clap {
                        let mut writer = Writer::new(out);
                        let _ = clap.write(&mut writer);
                    }

                    // ccst — MIAF requires the coding constraints of an image
                    // sequence to be stated in its sample entry, and both
                    // libheif and libavif write it. The values are derived
                    // rather than fixed: a track whose every sample is a sync
                    // sample really is all-intra, and one that is not must not
                    // claim to be, or a reader may skip building the reference
                    // machinery the later frames need.
                    {
                        let ccst_pos = begin_box(out, b"ccst");
                        write_fullbox(out, 0, 0);
                        let all_intra = sync_indices.len() == frames.len();
                        let mut byte = 1u8 << 6; // intra_pred_used
                        if all_intra {
                            byte |= 1 << 7; // all_ref_pics_intra
                        } else {
                            // max_ref_per_pic_used, four bits; 15 states that
                            // no bound is being claimed.
                            byte |= 0b1111 << 2;
                        }
                        out.push(byte);
                        out.extend_from_slice(&[0u8; 3]); // reserved
                        end_box(out, ccst_pos);
                    }

                    end_box(out, entry_pos);
                    end_box(out, pos);
                }

                // stts (time-to-sample): run-length encode durations
                {
                    let pos = begin_box(out, b"stts");
                    write_fullbox(out, 0, 0);
                    let mut entries: Vec<(u32, u32)> = Vec::new();
                    for &d in durations {
                        if let Some(last) = entries.last_mut()
                            && last.1 == d {
                                last.0 += 1;
                                continue;
                            }
                        entries.push((1, d));
                    }
                    write_u32(out, entries.len() as u32);
                    for (count, delta) in &entries {
                        write_u32(out, *count);
                        write_u32(out, *delta);
                    }
                    end_box(out, pos);
                }

                // stsc (sample-to-chunk: all samples in one chunk)
                {
                    let pos = begin_box(out, b"stsc");
                    write_fullbox(out, 0, 0);
                    write_u32(out, 1);
                    write_u32(out, 1); // first_chunk
                    write_u32(out, frames.len() as u32); // samples_per_chunk
                    write_u32(out, 1); // sample_description_index
                    end_box(out, pos);
                }

                // stsz (sample sizes)
                {
                    let pos = begin_box(out, b"stsz");
                    write_fullbox(out, 0, 0);
                    write_u32(out, 0); // sample_size = 0 (variable)
                    write_u32(out, frames.len() as u32);
                    for frame in frames {
                        write_u32(out, frame.len() as u32);
                    }
                    end_box(out, pos);
                }

                // stco (chunk offset — placeholder, patched later via stco_offset_pos)
                stco_offset_pos = {
                    let pos = begin_box(out, b"stco");
                    write_fullbox(out, 0, 0);
                    write_u32(out, 1); // entry_count
                    let p = out.len();
                    write_u32(out, STCO_PLACEHOLDER);
                    end_box(out, pos);
                    p
                };

                // stss (sync samples)
                {
                    let pos = begin_box(out, b"stss");
                    write_fullbox(out, 0, 0);
                    write_u32(out, sync_indices.len() as u32);
                    for &idx in sync_indices {
                        write_u32(out, idx);
                    }
                    end_box(out, pos);
                }

                end_box(out, stbl_pos);
            }

            end_box(out, minf_pos);
        }

        end_box(out, mdia_pos);
    }

    // tref for alpha track
    if is_alpha {
        let tref_pos = begin_box(out, b"tref");
        let auxl_pos = begin_box(out, b"auxl");
        write_u32(out, 1); // references track 1 (color)
        end_box(out, auxl_pos);
        end_box(out, tref_pos);
    }

    end_box(out, trak_pos);
    stco_offset_pos
}

// ─── Shared utilities ────────────────────────────────────────────────

fn write_av1c_box(out: &mut Vec<u8>, av1c: &Av1CBox, seq_header: &[u8]) {
    let pos = begin_box(out, b"av1C");
    out.push(0x81); // marker=1, version=1

    let byte1 = (av1c.seq_profile << 5) | av1c.seq_level_idx_0;
    let byte2 =
        u8::from(av1c.seq_tier_0) << 7
        | u8::from(av1c.high_bitdepth) << 6
        | u8::from(av1c.twelve_bit) << 5
        | u8::from(av1c.monochrome) << 4
        | u8::from(av1c.chroma_subsampling_x) << 3
        | u8::from(av1c.chroma_subsampling_y) << 2
        | av1c.chroma_sample_position;

    out.push(byte1);
    out.push(byte2);
    out.push(0x00); // no initial_presentation_delay
    out.extend_from_slice(seq_header);
    end_box(out, pos);
}

/// The size to show, given the coded size and any clean aperture.
///
/// The aperture's width and height are rationals, and a crop that is not a
/// whole number of pixels is not something a track header can state, so it
/// rounds down to the pixels wholly inside.
fn display_size(width: u32, height: u32, clap: Option<&ClapBox>) -> (u32, u32) {
    let Some(clap) = clap else {
        return (width, height);
    };
    let resolve = |n: u32, d: u32, coded: u32| -> u32 {
        if d == 0 {
            return coded;
        }
        (n / d).clamp(1, coded)
    };
    (
        resolve(clap.width_n, clap.width_d, width),
        resolve(clap.height_n, clap.height_d, height),
    )
}

fn bit_depth_from_av1c(av1c: &Av1CBox) -> u8 {
    if av1c.twelve_bit { 12 } else if av1c.high_bitdepth { 10 } else { 8 }
}

fn write_colr_nclx(out: &mut Vec<u8>, colr: &ColrBox) {
    let pos = begin_box(out, b"colr");
    out.extend_from_slice(b"nclx");
    write_u16(out, colr.color_primaries as u16);
    write_u16(out, colr.transfer_characteristics as u16);
    write_u16(out, colr.matrix_coefficients as u16);
    out.push(if colr.full_range_flag { 1 << 7 } else { 0 });
    end_box(out, pos);
}

fn write_clli(out: &mut Vec<u8>, clli: &ClliBox) {
    let pos = begin_box(out, b"clli");
    write_u16(out, clli.max_content_light_level);
    write_u16(out, clli.max_pic_average_light_level);
    end_box(out, pos);
}

fn write_mdcv(out: &mut Vec<u8>, mdcv: &MdcvBox) {
    let pos = begin_box(out, b"mdcv");
    for &(x, y) in &mdcv.primaries {
        write_u16(out, x);
        write_u16(out, y);
    }
    write_u16(out, mdcv.white_point.0);
    write_u16(out, mdcv.white_point.1);
    write_u32(out, mdcv.max_luminance);
    write_u32(out, mdcv.min_luminance);
    end_box(out, pos);
}

/// Write a big-endian u32 at an exact byte position.
///
/// Used to patch iloc/stco offset placeholders at the positions recorded when
/// they were emitted. This avoids any buffer-wide scanning for sentinel byte
/// patterns, which would risk corrupting AV1 frame payloads that happen to
/// contain those bytes (a real possibility, and one an attacker could trigger
/// deliberately by seeding the sentinel into encoded data).
fn write_u32_at(out: &mut [u8], pos: usize, value: u32) {
    debug_assert!(pos + 4 <= out.len());
    if pos + 4 <= out.len() {
        out[pos..pos + 4].copy_from_slice(&value.to_be_bytes());
    }
}

/// Convert an unsigned integer dimension to ISO/IEC 14496-12 16.16 fixed-point,
/// saturating at u32::MAX (i.e. integer part clamped to 0xFFFF).
///
/// `value << 16` panics in debug builds and silently wraps in release builds for
/// any `value >= 0x10000`. The tkhd width/height fields are u32 16.16 — values
/// representing > 65535 px integer width can't be encoded exactly, so saturate
/// rather than crash or emit a corrupted box.
fn fixed_16_16_saturating(value: u32) -> u32 {
    let widened = (value as u64) << 16;
    if widened > u32::MAX as u64 { u32::MAX } else { widened as u32 }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::boxes::HvcCParameterSet;

    fn basic_av1c() -> Av1CBox {
        Av1CBox {
            seq_profile: 0,
            seq_level_idx_0: 4,
            seq_tier_0: false,
            high_bitdepth: false,
            twelve_bit: false,
            monochrome: false,
            chroma_subsampling_x: true,
            chroma_subsampling_y: true,
            chroma_sample_position: 0,
        }
    }

    fn mono_av1c() -> Av1CBox {
        Av1CBox {
            seq_profile: 0,
            seq_level_idx_0: 4,
            seq_tier_0: false,
            high_bitdepth: false,
            twelve_bit: false,
            monochrome: true,
            chroma_subsampling_x: true,
            chroma_subsampling_y: true,
            chroma_sample_position: 0,
        }
    }

    #[test]
    fn serialize_color_only() {
        let frames = [
            AnimFrame::new(b"frame1color", 100).with_sync(true),
            AnimFrame::new(b"frame2color", 200),
        ];
        let mut image = AnimatedImage::new();
        image.set_color_config(basic_av1c());
        let avif = image.serialize(64, 64, &frames, b"seqhdr", None);

        // Should start with ftyp avis
        assert_eq!(&avif[4..8], b"ftyp");
        assert_eq!(&avif[8..12], b"avis");

        // Should contain mdat with frame data
        let mdat_str = b"mdat";
        assert!(avif.windows(4).any(|w| w == mdat_str));

        // Frame data should be present
        assert!(avif.windows(b"frame1color".len()).any(|w| w == b"frame1color"));
        assert!(avif.windows(b"frame2color".len()).any(|w| w == b"frame2color"));

        // Parse with zenavif-parse to verify structure
        let parser = zenavif_parse::AvifParser::from_bytes(&avif).unwrap();
        let info = parser.animation_info().expect("should have animation info");
        assert_eq!(info.timescale, 1000);
        assert_eq!(info.frame_count, 2);
    }

    #[test]
    fn serialize_with_alpha() {
        let frames = [
            AnimFrame::new(b"c1", 500).with_alpha(b"a1").with_sync(true),
            AnimFrame::new(b"c2", 500).with_alpha(b"a2"),
        ];
        let mut image = AnimatedImage::new();
        image.set_color_config(basic_av1c());
        image.set_alpha_config(mono_av1c());
        let avif = image.serialize(32, 32, &frames, b"colseq", Some(b"alphaseq"));

        assert_eq!(&avif[4..8], b"ftyp");
        assert!(avif.windows(2).any(|w| w == b"c1"));
        assert!(avif.windows(2).any(|w| w == b"a1"));
        assert!(avif.windows(2).any(|w| w == b"c2"));
        assert!(avif.windows(2).any(|w| w == b"a2"));

        let parser = zenavif_parse::AvifParser::from_bytes(&avif).unwrap();
        let info = parser.animation_info().expect("should have animation info");
        assert_eq!(info.frame_count, 2);
    }

    #[test]
    fn frame_payload_containing_placeholder_sentinels_is_not_corrupted() {
        // Regression: the old patcher walked the output buffer searching for
        // 0xDEADBEEF / 0xDEADBEE0 and overwrote any 4-byte match. Animation frame
        // payloads can legitimately contain those bytes (and an attacker could
        // deliberately seed them). This test puts both sentinels into multiple
        // frames at varied alignments and asserts the bytes survive serialization
        // intact.
        let stco = STCO_PLACEHOLDER.to_be_bytes();
        let iloc = ILOC_PLACEHOLDER.to_be_bytes();

        let mut frame1 = vec![0xAAu8; 33]; // odd-aligned sentinel
        frame1.extend_from_slice(&stco);
        frame1.extend_from_slice(&[0xBB; 16]);
        frame1.extend_from_slice(&iloc);
        frame1.extend_from_slice(&[0xCC; 32]);

        let mut frame2 = vec![0u8; 4];     // sentinels at start (after offset 0)
        frame2.extend_from_slice(&stco);
        frame2.extend_from_slice(&iloc);
        frame2.extend_from_slice(&[0xEE; 100]);

        let mut alpha1 = vec![0x44u8; 8];
        alpha1.extend_from_slice(&stco);
        alpha1.extend_from_slice(&[0x55; 16]);
        let mut alpha2 = vec![0x66u8; 16];
        alpha2.extend_from_slice(&iloc);
        alpha2.extend_from_slice(&[0x77; 8]);

        let frames = [
            AnimFrame::new(frame1.as_slice(), 100).with_alpha(alpha1.as_slice()).with_sync(true),
            AnimFrame::new(frame2.as_slice(), 200).with_alpha(alpha2.as_slice()),
        ];
        let mut image = AnimatedImage::new();
        image.set_color_config(basic_av1c());
        image.set_alpha_config(mono_av1c());
        let avif = image.serialize(64, 64, &frames, b"colseq", Some(b"alphaseq"));

        // Each frame's bytes must appear verbatim somewhere in the file.
        // (We used to fail this when sentinels in payload were overwritten.)
        assert!(avif.windows(frame1.len()).any(|w| w == frame1.as_slice()),
            "frame1 (with both sentinels) corrupted by placeholder scan");
        assert!(avif.windows(frame2.len()).any(|w| w == frame2.as_slice()),
            "frame2 (with both sentinels) corrupted by placeholder scan");
        assert!(avif.windows(alpha1.len()).any(|w| w == alpha1.as_slice()),
            "alpha1 corrupted by placeholder scan");
        assert!(avif.windows(alpha2.len()).any(|w| w == alpha2.as_slice()),
            "alpha2 corrupted by placeholder scan");

        // Parser still resolves animation structure correctly.
        let parser = zenavif_parse::AvifParser::from_bytes(&avif).unwrap();
        let info = parser.animation_info().expect("animation info");
        assert_eq!(info.frame_count, 2);
    }

    #[test]
    fn very_large_width_does_not_panic() {
        // Regression: tkhd encoded width as `width << 16`, which panics in debug
        // builds for any width >= 65536. Saturation via fixed_16_16_saturating
        // produces a well-formed (clamped) box and never panics.
        assert_eq!(fixed_16_16_saturating(0), 0);
        assert_eq!(fixed_16_16_saturating(1), 0x0001_0000);
        assert_eq!(fixed_16_16_saturating(0xFFFF), 0xFFFF_0000);
        assert_eq!(fixed_16_16_saturating(0x1_0000), u32::MAX);
        assert_eq!(fixed_16_16_saturating(70_000), u32::MAX);
        assert_eq!(fixed_16_16_saturating(u32::MAX), u32::MAX);

        let frames = [AnimFrame::new(b"f", 100).with_sync(true)];
        let mut image = AnimatedImage::new();
        image.set_color_config(basic_av1c());
        // 70000 used to panic at write_u32(out, width << 16).
        let avif = image.serialize(70_000, 70_000, &frames, b"seq", None);
        // File is structurally valid.
        assert_eq!(&avif[4..8], b"ftyp");
        // 70000 squared is 4.9 gigapixels, well past the parser's default
        // ceiling, and the point here is the writer's arithmetic rather than
        // whether anything would agree to decode a picture that size. Raise
        // the limit for the read-back so the structure is what is checked.
        let config = zenavif_parse::DecodeConfig {
            total_megapixels_limit: None,
            ..zenavif_parse::DecodeConfig::default()
        };
        let parser = zenavif_parse::AvifParser::from_bytes_with_config(
            &avif,
            &config,
            &zenavif_parse::Unstoppable,
        )
        .unwrap();
        let info = parser.animation_info().expect("animation info");
        assert_eq!(info.frame_count, 1);
    }

    #[test]
    fn frame_durations_roundtrip() {
        let frames = [
            AnimFrame::new(b"f1", 100).with_sync(true),
            AnimFrame::new(b"f2", 200),
            AnimFrame::new(b"f3", 300),
        ];
        let mut image = AnimatedImage::new();
        image.set_color_config(basic_av1c());
        let avif = image.serialize(16, 16, &frames, b"seq", None);
        let parser = zenavif_parse::AvifParser::from_bytes(&avif).unwrap();
        let info = parser.animation_info().expect("animation info");
        assert_eq!(info.frame_count, 3);
        assert_eq!(info.timescale, 1000);
    }

    fn basic_hvcc() -> HvcCBox {
        HvcCBox {
            general_profile_idc: 1, // Main
            general_level_idc: 60,
            chroma_format_idc: 1, // 4:2:0
            bit_depth_luma: 8,
            bit_depth_chroma: 8,
            nal_length_size: 4,
            parameter_sets: vec![
                HvcCParameterSet {
                    nal_unit_type: 32, // VPS
                    array_completeness: true,
                    data: vec![0x40, 0x01, 0x0c],
                },
                HvcCParameterSet {
                    nal_unit_type: 33, // SPS
                    array_completeness: true,
                    data: vec![0x42, 0x01, 0x01],
                },
                HvcCParameterSet {
                    nal_unit_type: 34, // PPS
                    array_completeness: true,
                    data: vec![0x44, 0x01, 0xc0],
                },
            ],
            ..HvcCBox::default()
        }
    }

    #[test]
    fn hevc_sequence_is_a_heic_sequence_the_parser_reads_back() {
        let frames = [
            AnimFrame::new(b"hevcframe1", 40).with_sync(true),
            AnimFrame::new(b"hevcframe2", 40),
            AnimFrame::new(b"hevcframe3", 40),
        ];
        let mut image = AnimatedImage::new();
        image.set_hevc_config(basic_hvcc());
        // The AV1 sequence header argument is meaningless for HEVC and must
        // not leak into the file: HEVC's parameter sets live in hvcC alone.
        let heic = image.serialize(96, 64, &frames, b"AV1SEQHEADER", None);

        assert_eq!(&heic[4..8], b"ftyp");
        assert_eq!(&heic[8..12], b"hevc");
        assert!(
            heic.windows(4).any(|w| w == b"msf1"),
            "an image sequence must declare the msf1 brand"
        );
        assert!(
            !heic.windows(4).any(|w| w == b"av01") && !heic.windows(4).any(|w| w == b"av1C"),
            "no AV1 structures belong in an HEVC file"
        );
        assert!(
            !heic.windows(12).any(|w| w == b"AV1SEQHEADER"),
            "the AV1 sequence header argument must be ignored, not written"
        );
        for frame in &frames {
            assert!(heic.windows(frame.color.len()).any(|w| w == frame.color));
        }

        let parser = zenavif_parse::AvifParser::from_bytes(&heic).unwrap();
        let info = parser.animation_info().expect("animation info");
        assert_eq!(info.frame_count, 3);
        assert_eq!(info.timescale, 1000);

        // The track's own sample entry must carry the configuration. Reading
        // the still item's instead is the bug that let one frame decode and
        // every later one fail, so assert on the track's specifically.
        let config = parser.track_hevc_config().expect("track hvcC");
        assert_eq!(config.nal_length_size, 4);
        assert_eq!(
            config
                .parameter_sets
                .iter()
                .map(|ps| ps.nal_unit_type)
                .collect::<Vec<_>>(),
            vec![32, 33, 34]
        );

        for index in 0..info.frame_count {
            let frame = parser.frame(index).expect("frame");
            assert_eq!(&frame.data[..], frames[index].color);
            assert_eq!(frame.duration_ticks, 40);
        }
    }

    #[test]
    fn hevc_sequence_carries_alpha_in_its_own_track() {
        let frames = [
            AnimFrame::new(b"c1", 100).with_alpha(b"a1").with_sync(true),
            AnimFrame::new(b"c2", 100).with_alpha(b"a2"),
        ];
        let mut image = AnimatedImage::new();
        image.set_hevc_config(basic_hvcc());
        let mut alpha = basic_hvcc();
        alpha.chroma_format_idc = 0; // monochrome
        image.set_alpha_hevc_config(alpha);
        let heic = image.serialize(32, 32, &frames, b"", None);

        let parser = zenavif_parse::AvifParser::from_bytes(&heic).unwrap();
        let info = parser.animation_info().expect("animation info");
        assert_eq!(info.frame_count, 2);
        assert!(info.has_alpha, "the alpha track should be found");
        for name in [&b"a1"[..], &b"a2"[..]] {
            assert!(heic.windows(2).any(|w| w == name));
        }
    }

    #[test]
    fn an_hevc_colour_track_without_an_alpha_config_writes_no_alpha_track() {
        // Alpha data with nothing to decode it against is not an alpha track;
        // writing one would produce a track whose samples cannot be read.
        let frames = [AnimFrame::new(b"c1", 100).with_alpha(b"a1").with_sync(true)];
        let mut image = AnimatedImage::new();
        image.set_hevc_config(basic_hvcc());
        let heic = image.serialize(32, 32, &frames, b"", None);

        let parser = zenavif_parse::AvifParser::from_bytes(&heic).unwrap();
        let info = parser.animation_info().expect("animation info");
        assert!(!info.has_alpha);
        assert!(!heic.windows(2).any(|w| w == b"a1"), "alpha data was written anyway");
    }

    /// The duration field of the first box of a given type, as a u64.
    fn duration_of(file: &[u8], fourcc: &[u8; 4], version_1_offset: usize) -> u64 {
        let at = file
            .windows(4)
            .position(|w| w == fourcc)
            .unwrap_or_else(|| panic!("no {} box", std::str::from_utf8(fourcc).unwrap()));
        let payload = at + 4 + 4; // past the fourcc and the version/flags word
        let start = payload + version_1_offset;
        u64::from_be_bytes(file[start..start + 8].try_into().unwrap())
    }

    /// Loop-forever is signalled by an indefinite duration, not by the edit
    /// list's flag alone.
    ///
    /// The flag on its own reads as one play: libheif divides the movie
    /// duration by the track's, and libavif divides the track duration by the
    /// edit segment's, and with everything finite and equal both come out at
    /// one. Files written before this was measured said "repeat" and were
    /// played once by both.
    #[test]
    fn repeating_forever_leaves_the_duration_indefinite() {
        let frames = [
            AnimFrame::new(b"f1", 40).with_sync(true),
            AnimFrame::new(b"f2", 40),
        ];
        let mut image = AnimatedImage::new();
        image.set_color_config(basic_av1c());

        image.set_loop_count(0); // forever
        let forever = image.serialize(16, 16, &frames, b"seq", None);
        assert!(
            forever.windows(4).any(|w| w == b"elst"),
            "a repeating track needs an edit list"
        );
        // mvhd is version 1: creation and modification are 8 bytes each, then
        // a 4-byte timescale, then the duration.
        assert_eq!(duration_of(&forever, b"mvhd", 8 + 8 + 4), u64::MAX);
        // tkhd is version 1: creation, modification, a 4-byte track id and a
        // 4-byte reserved word come before the duration.
        assert_eq!(duration_of(&forever, b"tkhd", 8 + 8 + 4 + 4), u64::MAX);
        // The media itself is still exactly as long as its samples.
        assert_eq!(duration_of(&forever, b"mdhd", 8 + 8 + 4), 80);

        image.set_loop_count(1); // once
        let once = image.serialize(16, 16, &frames, b"seq", None);
        assert!(
            !once.windows(4).any(|w| w == b"elst"),
            "no edit list means play once, which is what libheif writes"
        );
        assert_eq!(duration_of(&once, b"mvhd", 8 + 8 + 4), 80);
        assert_eq!(duration_of(&once, b"tkhd", 8 + 8 + 4 + 4), 80);

        // A finite count is not "forever or once": the reader divides the
        // track's duration by the edit segment's, so three plays is three
        // times the media. This was written as one play until libheif read a
        // file back and reported one where three had been asked for.
        image.set_loop_count(3);
        let thrice = image.serialize(16, 16, &frames, b"seq", None);
        assert!(thrice.windows(4).any(|w| w == b"elst"));
        assert_eq!(duration_of(&thrice, b"mvhd", 8 + 8 + 4), 240);
        assert_eq!(duration_of(&thrice, b"tkhd", 8 + 8 + 4 + 4), 240);
        // The media is still exactly as long as its samples, and the edit
        // segment covers it once — libheif checks that before believing any
        // of the above.
        assert_eq!(duration_of(&thrice, b"mdhd", 8 + 8 + 4), 80);
        let elst = thrice
            .windows(4)
            .position(|w| w == b"elst")
            .expect("an edit list");
        let segment = u32::from_be_bytes(thrice[elst + 12..elst + 16].try_into().unwrap());
        assert_eq!(segment, 80, "the segment is one pass over the media");
    }

    #[test]
    fn coding_constraints_state_whether_the_track_is_all_intra() {
        // MIAF wants ccst in the sample entry, and a track that is not
        // all-intra must not say it is.
        let ccst_payload = |heic: &[u8]| -> u8 {
            let at = heic.windows(4).position(|w| w == b"ccst").expect("ccst");
            heic[at + 8] // past the fourcc and the version/flags word
        };

        let mut image = AnimatedImage::new();
        image.set_hevc_config(basic_hvcc());

        let all_intra = [
            AnimFrame::new(b"k1", 10).with_sync(true),
            AnimFrame::new(b"k2", 10).with_sync(true),
        ];
        assert_eq!(
            ccst_payload(&image.serialize(16, 16, &all_intra, b"", None)) >> 7,
            1,
            "every sample is a sync sample, so all_ref_pics_intra should be set"
        );

        let inter = [
            AnimFrame::new(b"k1", 10).with_sync(true),
            AnimFrame::new(b"p2", 10),
        ];
        assert_eq!(
            ccst_payload(&image.serialize(16, 16, &inter, b"", None)) >> 7,
            0,
            "a track with inter frames must not claim to be all-intra"
        );
    }
}
