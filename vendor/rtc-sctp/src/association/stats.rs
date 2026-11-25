/// Association statistics
#[derive(Default, Debug, Copy, Clone)]
pub struct AssociationStats {
    n_datas: u64,
    n_sacks: u64,
    n_t3timeouts: u64,
    n_ack_timeouts: u64,
    n_fast_retrans: u64,
    // Babu Tela: the rest of the counters `Association::congestion` reports.
    n_fast_recoveries: u64,
    n_t3_retrans: u64,
    n_dup_datas: u64,
    n_sacks_sent: u64,
    n_lost: u64,
}

impl AssociationStats {
    /// Counts one DATA chunk sent.
    pub fn inc_datas(&mut self) {
        self.n_datas += 1;
    }

    /// The number of DATA chunks sent.
    pub fn get_num_datas(&mut self) -> u64 {
        self.n_datas
    }

    /// Counts one SACK chunk received.
    pub fn inc_sacks(&mut self) {
        self.n_sacks += 1;
    }

    /// The number of SACK chunks received.
    pub fn get_num_sacks(&mut self) -> u64 {
        self.n_sacks
    }

    /// Counts one T3-rtx retransmission timeout.
    pub fn inc_t3timeouts(&mut self) {
        self.n_t3timeouts += 1;
    }

    /// The number of T3-rtx retransmission timeouts, a signal of loss or a stalled path.
    pub fn get_num_t3timeouts(&mut self) -> u64 {
        self.n_t3timeouts
    }

    /// Counts one delayed-acknowledgement timeout.
    pub fn inc_ack_timeouts(&mut self) {
        self.n_ack_timeouts += 1;
    }

    /// The number of delayed-acknowledgement timeouts.
    pub fn get_num_ack_timeouts(&mut self) -> u64 {
        self.n_ack_timeouts
    }

    /// Counts one fast retransmission.
    pub fn inc_fast_retrans(&mut self) {
        self.n_fast_retrans += 1;
    }

    /// The number of fast retransmissions, triggered by SACK gap reports rather than a timeout.
    pub fn get_num_fast_retrans(&mut self) -> u64 {
        self.n_fast_retrans
    }

    /// Counts one entry into fast recovery (a loss the SACKs revealed).
    pub fn inc_fast_recoveries(&mut self) {
        self.n_fast_recoveries += 1;
    }

    /// The number of times the sender entered fast recovery.
    pub fn get_num_fast_recoveries(&self) -> u64 {
        self.n_fast_recoveries
    }

    /// Counts one DATA chunk sent again outside fast retransmission: after a
    /// T3-rtx timeout, or found lost by RACK (under BBR).
    pub fn inc_t3_retrans(&mut self) {
        self.n_t3_retrans += 1;
    }

    /// The number of DATA chunks sent again outside fast retransmission.
    pub fn get_num_t3_retrans(&self) -> u64 {
        self.n_t3_retrans
    }

    /// Counts one DATA chunk received that was already here (sent again needlessly).
    pub fn inc_dup_datas(&mut self) {
        self.n_dup_datas += 1;
    }

    /// The number of duplicate DATA chunks received.
    pub fn get_num_dup_datas(&self) -> u64 {
        self.n_dup_datas
    }

    /// Counts DATA chunks found lost by RACK (under BBR).
    pub fn add_lost(&mut self, n: u64) {
        self.n_lost += n;
    }

    /// The number of DATA chunks RACK found lost.
    pub fn get_num_lost(&self) -> u64 {
        self.n_lost
    }

    /// Counts one SACK chunk sent.
    pub fn inc_sacks_sent(&mut self) {
        self.n_sacks_sent += 1;
    }

    /// The number of SACK chunks sent.
    pub fn get_num_sacks_sent(&self) -> u64 {
        self.n_sacks_sent
    }

    /// Zeroes every counter.
    pub fn reset(&mut self) {
        *self = AssociationStats::default();
    }
}

/// How an association's sending side stands at one moment, with its counters
/// so far (Babu Tela: what a slow transfer needs to say why it is slow).
#[derive(Default, Debug, Copy, Clone)]
pub struct Congestion {
    /// Congestion window, bytes.
    pub cwnd: u32,
    /// Slow-start threshold, bytes.
    pub ssthresh: u32,
    /// The peer's receive window as this side reckons it (its last a_rwnd
    /// minus what is outstanding), bytes.
    pub rwnd: u32,
    /// The receive window this side advertises, bytes.
    pub my_rwnd: u32,
    /// The largest packet this side sends, bytes.
    pub mtu: u32,
    /// Bytes sent and not acknowledged yet.
    pub inflight: usize,
    /// Bytes handed to the association and not sent yet.
    pub pending: usize,
    /// Smoothed round-trip time, milliseconds (0 before the first measurement).
    pub srtt_ms: u64,
    /// Retransmission timeout, milliseconds.
    pub rto_ms: u64,
    /// In fast recovery from a loss right now.
    pub in_fast_recovery: bool,
    /// Under BBR: its state ("startup", "drain", "probe-bw", "probe-rtt"),
    /// the path's bandwidth it measured (bytes a second), its lowest round
    /// trip (milliseconds) and the pacing rate (bytes a second).
    pub bbr: Option<(&'static str, f64, f64, f64)>,
    /// Payload bytes sent and received so far.
    pub bytes_sent: usize,
    /// See `bytes_sent`.
    pub bytes_received: usize,
    /// The counters.
    pub stats: AssociationStats,
}
