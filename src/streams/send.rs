use std::collections::VecDeque;
use std::time::Duration;
use std::time::Instant;

use crate::format::CodecConfig;
use crate::format::PayloadParams;
use crate::io::DATAGRAM_MAX_PACKET_SIZE;
use crate::io::DATAGRAM_MTU_WARN;
use crate::io::MAX_RTP_OVERHEAD;
use crate::media::KeyframeRequestKind;
use crate::media::Media;
use crate::media::MediaKind;
use crate::packet::QueuePriority;
use crate::packet::QueueSnapshot;
use crate::packet::QueueState;
use crate::rtp_::{extend_u16, Descriptions, ReportList, Rtcp};
use crate::rtp_::{ExtensionMap, ReceptionReport, RtpHeader};
use crate::rtp_::{ExtensionValues, MediaTime, Mid, NackEntry};
use crate::rtp_::{Pt, Rid, RtcpFb, SenderInfo, SenderReport, Ssrc};
use crate::rtp_::{Sdes, SdesType, MAX_BLANK_PADDING_PAYLOAD_SIZE};
use crate::rtp_::{SeqNo, SRTP_BLOCK_SIZE};
use crate::session::PacketReceipt;
use crate::stats::MediaEgressStats;
use crate::stats::StatsSnapshot;
use crate::util::value_history::ValueHistory;
use crate::util::InstantExt;
use crate::util::{already_happened, calculate_rtt_ms, not_happening};
use crate::RtcError;

use super::rtx_cache::RtxCache;
use super::send_queue::SendQueue;
use super::{rr_interval, RtpPacket};

/// The smallest size of padding for which we attempt to use a spurious resend. For padding
/// requests smaller than this we use blank packets instead.
const MIN_SPURIOUS_PADDING_SIZE: usize = 50;

pub const DEFAULT_RTX_CACHE_DURATION: Duration = Duration::from_secs(3);

/// Outgoing encoded stream.
///
/// A stream is a primary SSRC + optional RTX SSRC.
///
/// This is RTP level API. For sample level API see [`Rtc::writer`][crate::Rtc::writer].
#[derive(Debug)]
pub struct StreamTx {
    /// Unique identifier of the remote encoded stream.
    ssrc: Ssrc,

    /// Identifier of a resend (RTX) stream. If we are doing resends.
    rtx: Option<Ssrc>,

    /// The Media mid this stream belongs to.
    mid: Mid,

    /// The rid that might be used for this stream.
    rid: Option<Rid>,

    /// Set on first handle_timeout.
    kind: Option<MediaKind>,

    /// Set on first handle_timeout.
    cname: Option<String>,

    /// The last main payload clock rate that was sent.
    clock_rate: Option<i64>,

    /// If we are doing seq_no ourselves (when writing sample mode).
    seq_no: SeqNo,

    /// If we are using RTX, this is the seq no counter.
    seq_no_rtx: SeqNo,

    /// When we last sent something for this encoded stream, packet or RTCP.
    last_used: Instant,

    /// Last written media + wallclock time.
    rtp_and_wallclock: Option<(u32, Instant)>,

    /// Queue of packets to send.
    ///
    /// The packets here do not have correct sequence numbers, header extension values etc.
    /// They must be updated when we are about to send.
    send_queue: SendQueue,

    /// Whether this sender is to be unpaced in BWE situations.
    ///
    /// Audio defaults to not being paced.
    unpaced: Option<bool>,

    /// Scheduled resends due to NACK or spurious padding.
    resends: VecDeque<Resend>,

    /// Requested padding, that has not been turned into packets yet.
    padding: usize,

    /// Dummy packet for resends. Used between poll_packet and poll_packet_padding
    blank_packet: RtpPacket,

    /// Cache of sent packets to be able to answer to NACKs as well as
    /// sending spurious resends as padding.
    rtx_cache: RtxCache,

    /// Last time we produced a SR.
    last_sender_report: Instant,

    /// If we have a pending incoming keyframe request.
    pending_request_keyframe: Option<KeyframeRequestKind>,

    /// Statistics of outgoing data.
    stats: StreamTxStats,

    // downsampled rtx ratio (value, last calculation)
    rtx_ratio: (f32, Instant),
}

/// Holder of stats.
#[derive(Debug, Default)]
pub(crate) struct StreamTxStats {
    /// count of bytes sent, including retransmissions
    /// <https://www.w3.org/TR/webrtc-stats/#dom-rtcsentrtpstreamstats-bytessent>
    bytes: u64,
    /// count of retransmitted bytes alone
    bytes_resent: u64,
    /// count of packets sent, including retransmissions
    /// <https://www.w3.org/TR/webrtc-stats/#summary>
    packets: u64,
    /// count of retransmitted packets alone
    packets_resent: u64,
    /// count of FIR requests received
    firs: u64,
    /// count of PLI requests received
    plis: u64,
    /// count of NACKs received
    nacks: u64,
    /// round trip time (ms)
    /// Can be null in case of missing or bad reports
    rtt: Option<f32>,
    /// losses collecter from RR (known packets, lost ratio)
    losses: Vec<(u64, f32)>,
    bytes_transmitted: ValueHistory<u64>,
    bytes_retransmitted: ValueHistory<u64>,
}

impl StreamTx {
    pub(crate) fn new(ssrc: Ssrc, rtx: Option<Ssrc>, mid: Mid, rid: Option<Rid>) -> Self {
        // https://www.rfc-editor.org/rfc/rfc3550#page-13
        // The initial value of the sequence number SHOULD be random (unpredictable)
        // to make known-plaintext attacks on encryption more difficult
        let seq_no = (rand::random::<u16>() as u64).into();
        let seq_no_rtx = (rand::random::<u16>() as u64).into();

        debug!("Create StreamTx for SSRC: {}", ssrc);

        StreamTx {
            ssrc,
            rtx,
            mid,
            rid,
            kind: None,
            cname: None,
            clock_rate: None,
            seq_no,
            seq_no_rtx,
            last_used: already_happened(),
            rtp_and_wallclock: None,
            send_queue: SendQueue::new(),
            unpaced: None,
            resends: VecDeque::new(),
            padding: 0,
            blank_packet: RtpPacket::blank(),
            rtx_cache: RtxCache::new(2000, DEFAULT_RTX_CACHE_DURATION),
            last_sender_report: already_happened(),
            pending_request_keyframe: None,
            stats: StreamTxStats::default(),
            rtx_ratio: (0.0, already_happened()),
        }
    }

    /// The (primary) SSRC of this encoded stream.
    pub fn ssrc(&self) -> Ssrc {
        self.ssrc
    }

    /// The resend (RTX) SSRC of this encoded stream.
    pub fn rtx(&self) -> Option<Ssrc> {
        self.rtx
    }

    /// Mid for this stream.
    ///
    /// In SDP this corresponds to m-line and "Media".
    pub fn mid(&self) -> Mid {
        self.mid
    }

    /// Rid for this stream.
    ///
    /// This is used to separate streams with the same [`Mid`] when using simulcast.
    pub fn rid(&self) -> Option<Rid> {
        self.rid
    }

    /// Configure the RTX (resend) cache.
    ///
    /// This determines how old incoming NACKs we can reply to.
    ///
    /// The default is 1024 packets over 3 seconds.
    pub fn set_rtx_cache(&mut self, max_packets: usize, max_age: Duration) {
        // Dump old cache to avoid having to deal with resizing logic inside the cache impl.
        self.rtx_cache = RtxCache::new(max_packets, max_age);
    }

    /// Set whether this stream is unpaced or not.
    ///
    /// This is only relevant when BWE (Bandwidth Estimation) is enabled. By default, audio is unpaced
    /// thus not held to a steady send rate by the Pacer.
    ///
    /// This overrides the default behavior.
    pub fn set_unpaced(&mut self, unpaced: bool) {
        self.unpaced = Some(unpaced);
    }

    /// Write RTP packet to a send stream.
    ///
    /// The `payload` argument is expected to be only the RTP payload, not the RTP packet header.
    ///
    /// * `pt` Payload type. Declared in the Media this encoded stream belongs to.
    /// * `seq_no` Sequence number to use for this packet.
    /// * `time` Time in whatever the clock rate is for the media in question (normally 90_000 for video
    ///          and 48_000 for audio).
    /// * `wallclock` Real world time that corresponds to the media time in the RTP packet. For an SFU,
    ///               this can be hard to know, since RTP packets typically only contain the media
    ///               time (RTP time). In the simplest SFU setup, the wallclock could simply be the
    ///               arrival time of the incoming RTP data. For better synchronization the SFU
    ///               probably needs to weigh in clock drifts and data provided via the statistics, receiver
    ///               reports etc.
    /// * `marker` Whether to "mark" this packet. This is usually done for the last packet belonging to
    ///            a series of RTP packets constituting the same frame in a video stream.
    /// * `ext_vals` The RTP header extension values to set. The values must be mapped in the session,
    ///              or they will not be set on the RTP packet.
    /// * `nackable` Whether we should respond this packet for incoming NACK from the remote peer. For
    ///              audio this is always false. For temporal encoded video, some packets are discardable
    ///              and this flag should be set accordingly.
    /// * `payload` RTP packet payload, without header.
    #[allow(clippy::too_many_arguments)]
    pub fn write_rtp(
        &mut self,
        pt: Pt,
        seq_no: SeqNo,
        time: u32,
        wallclock: Instant,
        marker: bool,
        ext_vals: ExtensionValues,
        nackable: bool,
        payload: Vec<u8>,
    ) -> Result<(), RtcError> {
        let first_call = self.rtp_and_wallclock.is_none();

        if first_call && seq_no.roc() > 0 {
            // TODO: make it possible to supress this.
            warn!("First SeqNo has non-zero ROC ({}), which needs out-of-band signalling to remote peer", seq_no.roc());
        }

        // This 1 in clock frequency will be fixed in poll_output.
        let media_time = MediaTime::new(time as i64, 1);
        self.rtp_and_wallclock = Some((time, wallclock));

        let header = RtpHeader {
            sequence_number: *seq_no as u16,
            marker,
            payload_type: pt,
            timestamp: time,
            ssrc: self.ssrc,
            ext_vals,
            ..Default::default()
        };

        let packet = RtpPacket {
            seq_no,
            time: media_time,
            header,
            payload,
            nackable,
            // The overall idea for str0m is to only drive time forward from handle_input. If we
            // used a "now" argument to write_rtp(), we effectively get a second point that also need
            // to move time forward _for all of Rtc_ – that's too complicated.
            //
            // Instead we set a future timestamp here. When time moves forward in the "regular way",
            // in handle_timeout() we delegate to self.send_queue.handle_timeout() to mark the enqueued
            // timestamp of all packets that are about to be sent.
            timestamp: not_happening(),

            // This is only relevant for incoming RTP packets.
            last_sender_info: None,
        };

        self.send_queue.push(packet);

        Ok(())
    }

    fn rtx_enabled(&self) -> bool {
        self.rtx.is_some()
    }

    pub(crate) fn poll_packet(
        &mut self,
        now: Instant,
        exts: &ExtensionMap,
        twcc: &mut u64,
        params: &[PayloadParams],
        buf: &mut Vec<u8>,
    ) -> Option<PacketReceipt> {
        let mid = self.mid;
        let rid = self.rid;
        let ssrc_rtx = self.rtx;

        let (next, is_padding) = if let Some(next) = self.poll_packet_resend(now) {
            (next, false)
        } else if let Some(next) = self.poll_packet_regular(now) {
            (next, false)
        } else if let Some(next) = self.poll_packet_padding(now) {
            (next, true)
        } else {
            return None;
        };

        let pop_send_queue = next.kind == NextPacketKind::Regular;

        // Need the header for the receipt and modifications
        // TODO: Can we remove this?
        let header_ref = &mut next.pkt.header;

        // This is true also for RTX.
        header_ref.ext_vals.mid = Some(mid);

        let pt_main = header_ref.payload_type;

        // The pt in next.pkt is the "main" pt.
        let Some(param) = params.iter().find(|p| p.pt() == pt_main) else {
            // PT does not exist in the connected media.
            warn!("Media is missing PT ({}) used in RTP packet", pt_main);

            // Get rid of this packet we can't send.
            if pop_send_queue {
                self.send_queue.pop(now);
            }

            return None;
        };

        let mut set_pt = None;
        let mut set_cr = None;

        let mut header = match next.kind {
            NextPacketKind::Regular => {
                // Remember PT We want to set these directly on `self` here, but can't
                // because we already have a mutable borrow.
                set_pt = Some(param.pt());

                let clock_rate = param.spec().clock_rate as i64;
                set_cr = Some(clock_rate);

                // Modify the cached packet time. This is so write_rtp can use u32 media time without
                // worrying about lengthening or the clock rate.
                let time = MediaTime::new(next.pkt.time.numer(), clock_rate);
                next.pkt.time = time;

                // Modify the original (and also cached) header value.
                header_ref.ext_vals.rid = rid;
                header_ref.ext_vals.rid_repair = None;

                header_ref.clone()
            }
            NextPacketKind::Resend(_) | NextPacketKind::Blank(_) => {
                let pt_rtx = param.resend().expect("pt_rtx resend/blank");

                // Clone header to not change the original (cached) header.
                let mut header = header_ref.clone();

                // Update clone of header (to not change the cached value).
                header.payload_type = pt_rtx;
                header.ssrc = ssrc_rtx.expect("Should have RTX SSRC for resends");
                header.sequence_number = *next.seq_no as u16;

                header.ext_vals.rid = None;
                header.ext_vals.rid_repair = rid;

                header
            }
        };

        // These need to match `Extension::is_supported()` so we are sending what we are
        // declaring we support.
        header.ext_vals.abs_send_time = Some(MediaTime::new_ntp_time(now));
        header.ext_vals.transport_cc = Some(*twcc as u16);
        *twcc += 1;

        buf.resize(DATAGRAM_MAX_PACKET_SIZE, 0);

        let header_len = header.write_to(buf, exts);
        assert!(header_len % 4 == 0, "RTP header must be multiple of 4");
        header.header_len = header_len;

        let mut body_out = &mut buf[header_len..];

        // For resends, the original seq_no is inserted before the payload.
        let mut original_seq_len = 0;
        if let NextPacketKind::Resend(orig_seq_no) = next.kind {
            original_seq_len = RtpHeader::write_original_sequence_number(body_out, orig_seq_no);
            body_out = &mut body_out[original_seq_len..];
        }

        let pkt = &next.pkt;

        let body_len = match next.kind {
            NextPacketKind::Regular | NextPacketKind::Resend(_) => {
                let body_len = pkt.payload.len();
                body_out[..body_len].copy_from_slice(&pkt.payload);

                // pad for SRTP
                let pad_len = RtpHeader::pad_packet(
                    &mut buf[..],
                    header_len,
                    body_len + original_seq_len,
                    SRTP_BLOCK_SIZE,
                );

                body_len + original_seq_len + pad_len
            }
            NextPacketKind::Blank(len) => {
                let len = RtpHeader::create_padding_packet(
                    &mut buf[..],
                    header_len,
                    len,
                    SRTP_BLOCK_SIZE,
                );

                if len == 0 {
                    return None;
                }

                len
            }
        };

        buf.truncate(header_len + body_len);

        #[cfg(feature = "_internal_dont_use_log_stats")]
        {
            let queued_at = match next.kind {
                NextPacketKind::Regular => Some(pkt.timestamp),
                _ => {
                    // TODO: We don't have queued at stats for Resends or blank padding.
                    None
                }
            };

            if let Some(delay) = queued_at.map(|i| now.duration_since(i)) {
                crate::log_stat!("QUEUE_DELAY", header.ssrc, delay.as_secs_f64() * 1000.0);
            }
        }

        let seq_no = next.seq_no;
        self.last_used = now;

        if pop_send_queue {
            // poll_packet_regular leaves the packet in the head of the send_queue
            let pkt = self
                .send_queue
                .pop(now)
                .expect("head of send_queue to be there");
            if self.rtx_enabled() {
                self.rtx_cache.cache_sent_packet(pkt, now);
            }
        }

        // This is set here due to borrow checker.
        if let Some(pt) = set_pt {
            self.blank_packet.header.payload_type = pt;
        }
        if set_cr.is_some() && self.clock_rate != set_cr {
            self.clock_rate = set_cr;
        }

        Some(PacketReceipt {
            header,
            seq_no,
            is_padding,
            payload_size: body_len,
        })
    }

    fn poll_packet_resend(&mut self, now: Instant) -> Option<NextPacket<'_>> {
        let ratio = self.rtx_ratio_downsampled(now);

        // If we hit the cap, stop doing resends by clearing those we have queued.
        if ratio > 0.15_f32 {
            self.resends.clear();
            return None;
        }

        self.do_poll_packet_resend(now)
    }

    fn rtx_ratio_downsampled(&mut self, now: Instant) -> f32 {
        let (value, ts) = self.rtx_ratio;
        if now - ts < Duration::from_millis(50) {
            // not worth re-evaluating, return the old value
            return value;
        }

        // bytes stats refer to the last second by default
        let bytes_transmitted = self.stats.bytes_transmitted.sum();
        let bytes_retransmitted = self.stats.bytes_retransmitted.sum();
        let ratio = bytes_retransmitted as f32 / (bytes_retransmitted + bytes_transmitted) as f32;
        let ratio = if ratio.is_finite() { ratio } else { 0_f32 };
        self.rtx_ratio = (ratio, now);
        ratio
    }

    fn do_poll_packet_resend(&mut self, now: Instant) -> Option<NextPacket<'_>> {
        if !self.rtx_enabled() {
            // We're not doing resends for non-RTX.
            return None;
        }

        let seq_no = loop {
            let resend = self.resends.pop_front()?;

            let pkt = self.rtx_cache.get_cached_packet_by_seq_no(resend.seq_no);

            // The seq_no could simply be too old to exist in the buffer, in which
            // case we will not do a resend.
            let Some(pkt) = pkt else {
                continue;
            };

            if !pkt.nackable {
                trace!("SSRC {} resend {} not nackable", self.ssrc, pkt.seq_no);
            }

            break pkt.seq_no;
        };

        // Borrow checker gymnastics.
        let pkt = self.rtx_cache.get_cached_packet_by_seq_no(seq_no).unwrap();

        let len = pkt.payload.len() as u64;
        self.stats.update_packet_counts(len, true);
        self.stats.bytes_retransmitted.push(now, len);

        let seq_no = self.seq_no_rtx.inc();

        let orig_seq_no = pkt.seq_no;

        Some(NextPacket {
            kind: NextPacketKind::Resend(orig_seq_no),
            seq_no,
            pkt,
        })
    }

    fn poll_packet_regular(&mut self, now: Instant) -> Option<NextPacket<'_>> {
        // exit via ? here is ok since that means there is nothing to send.
        // The packet remains in the head of the send queue until we
        // finish poll_packet, at which point we move it to the cache.
        let pkt = self.send_queue.peek()?;

        pkt.timestamp = now;

        let len = pkt.payload.len() as u64;
        self.stats.update_packet_counts(len, false);
        self.stats.bytes_transmitted.push(now, len);

        let seq_no = pkt.seq_no;

        Some(NextPacket {
            kind: NextPacketKind::Regular,
            seq_no,
            pkt,
        })
    }

    fn poll_packet_padding(&mut self, _now: Instant) -> Option<NextPacket> {
        if self.padding == 0 {
            return None;
        }

        #[allow(clippy::unnecessary_operation)]
        'outer: {
            if self.padding > MIN_SPURIOUS_PADDING_SIZE {
                // Find a historic packet that is smaller than this max size. The max size
                // is a headroom since we can accept slightly larger padding than asked for.
                let max_size = (self.padding * 2).min(DATAGRAM_MTU_WARN - MAX_RTP_OVERHEAD);

                let Some(pkt) = self.rtx_cache.get_cached_packet_smaller_than(max_size) else {
                    // Couldn't find spurious packet, try a blank packet instead.
                    break 'outer;
                };

                let orig_seq_no = pkt.seq_no;
                let seq_no = self.seq_no_rtx.inc();

                self.padding = self.padding.saturating_sub(pkt.payload.len());

                return Some(NextPacket {
                    kind: NextPacketKind::Resend(orig_seq_no),
                    seq_no,
                    pkt,
                });
            }
        };

        let seq_no = self.seq_no_rtx.inc();

        let pkt = &mut self.blank_packet;
        pkt.seq_no = seq_no;

        let len = self
            .padding
            .clamp(SRTP_BLOCK_SIZE, MAX_BLANK_PADDING_PAYLOAD_SIZE);
        assert!(len <= 255); // should fit in a byte

        self.padding = self.padding.saturating_sub(len);

        Some(NextPacket {
            kind: NextPacketKind::Blank(len as u8),
            seq_no,
            pkt,
        })
    }

    pub(crate) fn sender_report_at(&self) -> Instant {
        let Some(kind) = self.kind else {
            // First handle_timeout sets the kind. No sender report until then.
            return not_happening();
        };
        self.last_sender_report + rr_interval(kind.is_audio())
    }

    pub(crate) fn poll_keyframe_request(&mut self) -> Option<KeyframeRequestKind> {
        self.pending_request_keyframe.take()
    }

    pub(crate) fn handle_rtcp(&mut self, now: Instant, fb: RtcpFb) {
        use RtcpFb::*;
        match fb {
            ReceptionReport(r) => self.stats.update_with_rr(now, r),
            Nack(_, list) => {
                self.stats.increase_nacks();
                let entries = list.into_iter();
                self.handle_nack(entries, now);
            }
            Pli(_) => {
                self.stats.increase_plis();
                self.pending_request_keyframe = Some(KeyframeRequestKind::Pli);
            }
            Fir(_) => {
                self.stats.increase_firs();
                self.pending_request_keyframe = Some(KeyframeRequestKind::Fir);
            }
            Twcc(_) => unreachable!("TWCC should be handled on session level"),
            _ => {}
        }
    }

    pub(crate) fn handle_nack(
        &mut self,
        entries: impl Iterator<Item = NackEntry>,
        now: Instant,
    ) -> Option<()> {
        // Turning NackEntry into SeqNo we need to know a SeqNo "close by" to lengthen the 16 bit
        // sequence number into the 64 bit we have in SeqNo.
        let seq_no = self.rtx_cache.last_cached_seq_no()?;
        let iter = entries.flat_map(|n| n.into_iter(seq_no));

        // Schedule all resends. They will be handled on next poll_packet
        for seq_no in iter {
            let Some(packet) = self.rtx_cache.get_cached_packet_by_seq_no(seq_no) else {
                // Packet was not available in RTX cache, it has probably expired.
                continue;
            };

            let resend = Resend {
                seq_no,
                queued_at: now,
                payload_size: packet.payload.len(),
            };
            self.resends.push_back(resend);
        }

        Some(())
    }

    pub(crate) fn need_sr(&self, now: Instant) -> bool {
        now >= self.sender_report_at()
    }

    pub(crate) fn create_sr_and_update(&mut self, now: Instant, feedback: &mut VecDeque<Rtcp>) {
        let sr = self.create_sender_report(now);

        debug!("Created feedback SR: {:?}", sr);
        feedback.push_back(Rtcp::SenderReport(sr));

        if let Some(ds) = self.create_sdes() {
            feedback.push_back(Rtcp::SourceDescription(ds));
        }

        // Update timestamp to move time when next is created.
        self.last_sender_report = now;
    }

    fn create_sender_report(&self, now: Instant) -> SenderReport {
        SenderReport {
            sender_info: self.sender_info(now),
            reports: ReportList::new(),
        }
    }

    fn create_sdes(&self) -> Option<Descriptions> {
        // CNAME is set on first handle_timeout. No SDES before that.
        let cname = self.cname.as_ref()?;
        let mut s = Sdes {
            ssrc: self.ssrc,
            values: ReportList::new(),
        };
        s.values.push((SdesType::CNAME, cname.to_string()));

        let mut d = Descriptions {
            reports: Box::new(ReportList::new()),
        };
        d.reports.push(s);

        Some(d)
    }

    fn sender_info(&self, now: Instant) -> SenderInfo {
        let rtp_time = self.current_rtp_time(now).unwrap_or(MediaTime::ZERO);

        SenderInfo {
            ssrc: self.ssrc,
            ntp_time: now,
            rtp_time,
            sender_packet_count: self.stats.packets as u32,
            sender_octet_count: self.stats.bytes as u32,
        }
    }

    fn current_rtp_time(&self, now: Instant) -> Option<MediaTime> {
        // This is the RTP time and the wallclock from the last written media.
        // We use that as an offset to current time (now), to calculate the
        // current RTP time.
        let (t_u32, w) = self.rtp_and_wallclock?;

        let clock_rate = self.clock_rate?;
        let t = MediaTime::new(t_u32 as i64, clock_rate);

        // Wallclock needs to be in the past.
        if w > now {
            let delta = w - now;
            debug!("write_rtp wallclock is in the future: {:?}", delta);
            return None;
        }
        let offset = now - w;

        // This might be in the wrong base.
        let rtp_time = t + offset.into();

        Some(rtp_time.rebase(clock_rate))
    }

    pub(crate) fn next_seq_no(&mut self) -> SeqNo {
        self.seq_no.inc()
    }

    pub(crate) fn last_packet(&self) -> Option<&[u8]> {
        if self.send_queue.is_empty() {
            self.rtx_cache.last_packet()
        } else {
            self.send_queue.last().map(|q| q.payload.as_ref())
        }
    }

    pub(crate) fn visit_stats(&mut self, snapshot: &mut StatsSnapshot, now: Instant) {
        self.stats.fill(snapshot, self.mid, self.rid, now);
    }

    pub(crate) fn queue_state(&mut self, now: Instant) -> QueueState {
        // The unpaced flag is set to a default value on first handle_timeout. The
        // default is to not pace audio. We unwrap default to "true" here to not
        // apply any pacing until we know what kind of content we are sending.
        let unpaced = self.unpaced.unwrap_or(true);

        // It's only possible to use this sender for padding if RTX is enabled and
        // we know the previous main PT.
        let use_for_padding = self.rtx_enabled() && self.blank_packet.is_pt_set();

        let mut snapshot = self.send_queue.snapshot(now);

        if let Some(snapshot_resend) = self.queue_state_resend(now) {
            snapshot.merge(&snapshot_resend);
        }

        if let Some(snapshot_padding) = self.queue_state_padding(now) {
            snapshot.merge(&snapshot_padding);
        }

        QueueState {
            mid: self.mid,
            unpaced,
            use_for_padding,
            snapshot,
        }
    }

    fn queue_state_resend(&self, now: Instant) -> Option<QueueSnapshot> {
        if self.resends.is_empty() {
            return None;
        }

        // Outstanding resends
        let mut snapshot = self
            .resends
            .iter()
            .fold(QueueSnapshot::default(), |mut snapshot, r| {
                snapshot.total_queue_time_origin += now.duration_since(r.queued_at);
                snapshot.size += r.payload_size;
                snapshot.packet_count += 1;
                snapshot.first_unsent = snapshot
                    .first_unsent
                    .map(|i| i.min(r.queued_at))
                    .or(Some(r.queued_at));

                snapshot
            });
        snapshot.created_at = now;
        snapshot.update_priority(QueuePriority::Media);

        Some(snapshot)
    }

    fn queue_state_padding(&self, now: Instant) -> Option<QueueSnapshot> {
        if self.padding == 0 {
            return None;
        }

        // TODO: Be more scientific about these factors.
        const AVERAGE_PADDING_PACKET_SIZE: usize = 800;
        const FAKE_PADDING_DURATION_MILLIS: usize = 5;

        let fake_packets = self.padding / AVERAGE_PADDING_PACKET_SIZE;
        let fake_millis = fake_packets * FAKE_PADDING_DURATION_MILLIS;
        let fake_duration = Duration::from_millis(fake_millis as u64);

        Some(QueueSnapshot {
            created_at: now,
            size: self.padding,
            packet_count: fake_packets as u32,
            total_queue_time_origin: fake_duration,
            priority: QueuePriority::Padding,
            ..Default::default()
        })
    }

    pub(crate) fn generate_padding(&mut self, padding: usize) {
        if !self.rtx_enabled() {
            return;
        }
        self.padding += padding;
    }

    pub(crate) fn need_timeout(&self) -> bool {
        self.send_queue.need_timeout()
    }

    pub(crate) fn handle_timeout<'a>(
        &mut self,
        now: Instant,
        get_media: impl FnOnce() -> (&'a Media, &'a CodecConfig),
    ) {
        // If kind is None, this is the first time we ever get a handle_timeout.
        if self.kind.is_none() {
            let (media, config) = get_media();
            self.on_first_timeout(media, config);
        }

        self.send_queue.handle_timeout(now);
    }

    fn on_first_timeout(&mut self, media: &Media, config: &CodecConfig) {
        // Always set on first timeout.
        self.kind = Some(media.kind());
        self.cname = Some(media.cname().to_string());

        // Set on first timeout, if not set already by configuration.
        if self.unpaced.is_none() {
            // Default audio to be unpaced.
            self.unpaced = Some(media.kind().is_audio());
        }

        // To allow for sending padding on a newly created StreamTx, before any regular
        // packet has been sent, we need an PT that has RTX for any main PT. This is
        // later be overwritten when we send the first regular packet.
        if self.rtx_enabled() && !self.blank_packet.is_pt_set() {
            if let Some(pt) = media.first_pt_with_rtx(config) {
                trace!(
                    "StreamTx Mid {} blank packet PT {} before first regular packet",
                    self.mid,
                    pt
                );
                self.blank_packet.header.payload_type = pt;
            }
        }
    }

    pub(crate) fn reset_buffers(&mut self) {
        self.send_queue.clear();
        self.rtx_cache.clear();
        self.resends.clear();
        self.padding = 0;
    }
}

impl StreamTxStats {
    fn update_packet_counts(&mut self, bytes: u64, is_resend: bool) {
        self.packets += 1;
        self.bytes += bytes;
        if is_resend {
            self.bytes_resent += bytes;
            self.packets_resent += 1;
        }
    }

    fn increase_nacks(&mut self) {
        self.nacks += 1;
    }

    fn increase_plis(&mut self) {
        self.plis += 1;
    }

    fn increase_firs(&mut self) {
        self.firs += 1;
    }

    fn update_with_rr(&mut self, now: Instant, r: ReceptionReport) {
        let ntp_time = now.to_ntp_duration();
        let rtt = calculate_rtt_ms(ntp_time, r.last_sr_delay, r.last_sr_time);
        self.rtt = rtt;

        let ext_seq = {
            let prev = self.losses.last().map(|s| s.0).unwrap_or(r.max_seq as u64);
            let next = (r.max_seq & 0xffff) as u16;
            extend_u16(Some(prev), next)
        };

        self.losses
            .push((ext_seq, r.fraction_lost as f32 / u8::MAX as f32));
    }

    pub(crate) fn fill(
        &mut self,
        snapshot: &mut StatsSnapshot,
        mid: Mid,
        rid: Option<Rid>,
        now: Instant,
    ) {
        if self.bytes == 0 {
            return;
        }

        let key = (mid, rid);

        let loss = {
            let mut value = 0_f32;
            let mut total_weight = 0_u64;

            // just in case we received RRs out of order
            self.losses.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());

            // average known RR losses weighted by their number of packets
            for it in self.losses.windows(2) {
                let [prev, next] = it else { continue };
                let weight = next.0.saturating_sub(prev.0);
                value += next.1 * weight as f32;
                total_weight += weight;
            }

            let result = value / total_weight as f32;
            result.is_finite().then_some(result)
        };

        self.losses.drain(..self.losses.len().saturating_sub(1));

        snapshot.egress.insert(
            key,
            MediaEgressStats {
                mid,
                rid,
                bytes: self.bytes,
                packets: self.packets,
                firs: self.firs,
                plis: self.plis,
                nacks: self.nacks,
                rtt: self.rtt,
                loss,
                timestamp: now,
            },
        );
    }
}

struct NextPacket<'a> {
    kind: NextPacketKind,
    seq_no: SeqNo,
    pkt: &'a mut RtpPacket,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NextPacketKind {
    Regular,
    Resend(SeqNo),
    Blank(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Resend {
    seq_no: SeqNo,
    queued_at: Instant,
    payload_size: usize,
}
