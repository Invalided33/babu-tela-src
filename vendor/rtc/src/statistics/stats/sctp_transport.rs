//! SCTP association statistics (Babu Tela).
//!
//! The association the data channels run on, as its congestion control
//! stands: what a slow data channel needs to say why it is slow. Not a
//! current W3C dictionary: an early draft had an `RTCSctpTransportStats` with
//! some of these (the congestion window, the receiver window, the smoothed
//! round trip), which browsers have since dropped. vendor/rtc-sctp
//! (`Association::congestion`) supplies the numbers.

use super::{RTCStats, RTCStatsType};
use serde::{Deserialize, Serialize};
use shared::time::SystemInstant;
use std::time::Instant;

/// The SCTP association under the data channels, at one moment.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RTCSctpTransportStats {
    /// Base statistics fields (timestamp, type, id).
    #[serde(flatten)]
    pub stats: RTCStats,
    /// Congestion window, bytes.
    pub congestion_window: u32,
    /// Slow-start threshold, bytes.
    pub slow_start_threshold: u32,
    /// The peer's receive window as this side reckons it, bytes.
    pub receiver_window: u32,
    /// The receive window this side advertises, bytes.
    pub local_receiver_window: u32,
    /// The largest packet this side sends, bytes.
    pub mtu: u32,
    /// Bytes sent and not acknowledged yet.
    pub unack_data: u64,
    /// Bytes handed to the association and not sent yet.
    pub pending_data: u64,
    /// Smoothed round-trip time, seconds (0 before the first measurement).
    pub smoothed_round_trip_time: f64,
    /// Retransmission timeout, seconds.
    pub retransmission_timeout: f64,
    /// In fast recovery from a loss right now.
    pub in_fast_recovery: bool,
    /// Payload bytes sent and received.
    pub bytes_sent: u64,
    /// See `bytes_sent`.
    pub bytes_received: u64,
    /// SACK chunks received and sent.
    pub sacks_received: u64,
    /// See `sacks_received`.
    pub sacks_sent: u64,
    /// DATA chunks received, and those of them that were already here.
    pub data_chunks_received: u64,
    /// See `data_chunks_received`.
    pub duplicate_data_chunks_received: u64,
    /// Times a loss the SACKs revealed started a fast recovery.
    pub fast_recoveries: u64,
    /// DATA chunks sent again by fast retransmission.
    pub fast_retransmissions: u64,
    /// T3-rtx timeouts.
    pub retransmission_timeouts: u64,
    /// DATA chunks sent again outside fast retransmission: found lost by
    /// RACK (under BBR) or after a T3-rtx timeout.
    pub timeout_retransmissions: u64,
    /// DATA chunks RACK found lost (under BBR).
    pub lost_data_chunks: u64,
    /// The congestion control: "reno", "cubic", or BBR's state ("startup",
    /// "drain", "probe-bw", "probe-rtt").
    pub congestion_control: String,
    /// Under BBR: the bandwidth it measured and the rate it paces at,
    /// bytes a second, and the lowest round trip, seconds.
    pub delivery_rate: f64,
    /// See `delivery_rate`.
    pub pacing_rate: f64,
    /// See `delivery_rate`.
    pub min_round_trip_time: f64,
}

impl RTCSctpTransportStats {
    pub(crate) fn new(now: Instant, c: sctp::Congestion) -> Self {
        let mut s = c.stats;
        RTCSctpTransportStats {
            stats: RTCStats {
                timestamp: SystemInstant::now(now),
                typ: RTCStatsType::SctpTransport,
                id: "RTCSctpTransport".to_owned(),
            },
            congestion_window: c.cwnd,
            slow_start_threshold: c.ssthresh,
            receiver_window: c.rwnd,
            local_receiver_window: c.my_rwnd,
            mtu: c.mtu,
            unack_data: c.inflight as u64,
            pending_data: c.pending as u64,
            smoothed_round_trip_time: c.srtt_ms as f64 / 1000.0,
            retransmission_timeout: c.rto_ms as f64 / 1000.0,
            in_fast_recovery: c.in_fast_recovery,
            bytes_sent: c.bytes_sent as u64,
            bytes_received: c.bytes_received as u64,
            sacks_received: s.get_num_sacks(),
            sacks_sent: s.get_num_sacks_sent(),
            data_chunks_received: s.get_num_datas(),
            duplicate_data_chunks_received: s.get_num_dup_datas(),
            fast_recoveries: s.get_num_fast_recoveries(),
            fast_retransmissions: s.get_num_fast_retrans(),
            retransmission_timeouts: s.get_num_t3timeouts(),
            timeout_retransmissions: s.get_num_t3_retrans(),
            lost_data_chunks: s.get_num_lost(),
            congestion_control: c
                .bbr
                .map_or(if c.cubic { "cubic" } else { "reno" }, |b| b.0)
                .to_owned(),
            delivery_rate: c.bbr.map_or(0.0, |b| b.1),
            pacing_rate: c.bbr.map_or(0.0, |b| b.3),
            min_round_trip_time: c.bbr.map_or(0.0, |b| b.2 / 1000.0),
        }
    }
}
