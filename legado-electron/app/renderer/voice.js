'use strict';
// Voice, camera and screen sharing over a WebRTC mesh. The gateway only relays
// signalling; audio and video flow directly between participants.

// Bitrates are per viewer in kbps: in a mesh every viewer gets its own copy,
// so the sender's upload is roughly bitrate x number of people watching.
const QUALITY = {
  '720p30': { label: '720p · 30 fps', width: 1280, height: 720, fps: 30, screen: 2500, camera: 1500 },
  '1080p30': { label: '1080p · 30 fps', width: 1920, height: 1080, fps: 30, screen: 4500, camera: 2500 },
  '1080p60': { label: '1080p · 60 fps', width: 1920, height: 1080, fps: 60, screen: 8000, camera: 4000 },
  source: { label: 'Original · 60 fps', width: 3840, height: 2160, fps: 60, screen: 12000, camera: 4000 },
};
const VOICE_BITRATE = 96000;

// Short synthesized cues, one per event, so the app ships no audio files.
// Each cue is a few notes [frequency Hz, start offset s]; rising shapes mean
// "on / arrived", falling shapes mean "off / gone". They use their own audio
// context because the call's context is closed on hang-up, right when the
// leave cue needs to play.
const N = { G3: 196, C4: 261.63, D4: 293.66, E4: 329.63, G4: 392, A4: 440, B4: 493.88, C5: 523.25, D5: 587.33, E5: 659.25, G5: 783.99, A5: 880, C6: 1046.5, E6: 1318.5 };
const SOUNDS = {
  join: { label: 'Alguém entrou na chamada', notes: [[N.D5, 0], [N.A5, 0.085]] },
  leave: { label: 'Alguém saiu da chamada', notes: [[N.A5, 0], [N.D5, 0.085]] },
  disconnect: { label: 'Chamada desconectada', notes: [[N.E5, 0], [N.B4, 0.11], [N.E4, 0.22]], decay: 0.45 },
  mute: { label: 'Microfone desligado', notes: [[N.E5, 0], [N.B4, 0.06]], decay: 0.16, gain: 0.8 },
  unmute: { label: 'Microfone ligado', notes: [[N.B4, 0], [N.E5, 0.06]], decay: 0.16, gain: 0.8 },
  deafen: { label: 'Áudio desligado', notes: [[N.G4, 0], [N.D4, 0.07], [N.G3, 0.14]], decay: 0.22, wave: 'triangle' },
  undeafen: { label: 'Áudio ligado', notes: [[N.G3, 0], [N.D4, 0.07], [N.G4, 0.14]], decay: 0.22, wave: 'triangle' },
  stream_start: { label: 'Alguém começou a transmitir a tela', notes: [[N.C5, 0], [N.E5, 0.07], [N.G5, 0.14], [N.C6, 0.21]], decay: 0.3 },
  stream_stop: { label: 'Alguém parou de transmitir a tela', notes: [[N.C6, 0], [N.G5, 0.07], [N.E5, 0.14], [N.C5, 0.21]], decay: 0.3 },
  message: { label: 'Mensagem nova (direta ou menção)', notes: [[N.A5, 0], [N.E6, 0.045]], decay: 0.4, gain: 0.7 },
};

const Sounds = {
  ctx: null,
  // The old single on/off switch (before per-sound settings) still mutes everything.
  off() {
    if (store.get('sounds') === '0') return new Set(Object.keys(SOUNDS));
    try {
      return new Set(JSON.parse(store.get('sounds.off') || '[]'));
    } catch {
      return new Set();
    }
  },
  enabled: kind => !Sounds.off().has(kind),
  setEnabled(kind, on) {
    const off = this.off();
    store.set('sounds', null);
    if (on) off.delete(kind);
    else off.add(kind);
    store.set('sounds.off', off.size ? JSON.stringify([...off]) : null);
  },
  play(kind, force) {
    const cue = SOUNDS[kind];
    if (!cue || (!force && !this.enabled(kind))) return;
    try {
      const ctx = (this.ctx ||= new AudioContext());
      if (ctx.state === 'suspended') ctx.resume();
      const out = ctx.createGain();
      out.gain.value = 0.35 * (cue.gain ?? 1);
      out.connect(ctx.destination);
      const t0 = ctx.currentTime + 0.02;
      const decay = cue.decay ?? 0.32;
      for (const [freq, delay] of cue.notes) {
        const t = t0 + delay;
        const env = ctx.createGain();
        env.gain.setValueAtTime(0.0001, t);
        env.gain.exponentialRampToValueAtTime(1, t + 0.012);
        env.gain.exponentialRampToValueAtTime(0.0001, t + decay);
        env.connect(out);
        // A base tone with a quiet octave on top gives a soft, bell-like sound.
        for (const [type, mult, level] of [[cue.wave || 'sine', 1, 1], ['triangle', 2, 0.12]]) {
          const osc = ctx.createOscillator();
          const g = ctx.createGain();
          osc.type = type;
          osc.frequency.value = freq * mult;
          g.gain.value = level;
          osc.connect(g).connect(env);
          osc.start(t);
          osc.stop(t + decay + 0.03);
        }
      }
    } catch (e) {
      console.warn('sound', e);
    }
  },
};

const Voice = {
  channelId: null,
  local: null,
  peers: new Map(),
  mute: false,
  deaf: false,
  video: null, // null | 'camera' | 'screen'
  videoTrack: null,
  meters: new Map(),
  ctx: null,
  timer: null,

  get quality() {
    return QUALITY[store.get('quality')] || QUALITY['1080p30'];
  },

  videoConstraints() {
    const q = this.quality;
    return { width: { ideal: q.width }, height: { ideal: q.height }, frameRate: { ideal: q.fps, max: q.fps } };
  },

  // Text on a shared screen stays sharp with 'detail'; 60 fps sharing is
  // usually a game or video, where smooth motion matters more.
  hint(kind) {
    return kind === 'screen' && this.quality.fps < 60 ? 'detail' : 'motion';
  },

  flags() {
    return { mute: this.mute || this.deaf, deaf: this.deaf, video: this.video };
  },

  async join(channelId) {
    if (this.channelId === channelId) return;
    if (this.channelId) this.leave();
    this.mute = this.deaf = false;
    try {
      this.local = await navigator.mediaDevices.getUserMedia({
        audio: { echoCancellation: true, noiseSuppression: true, autoGainControl: true, channelCount: 1, sampleRate: 48000 },
      });
    } catch {
      this.local = new MediaStream();
      this.mute = true;
      toast('Sem acesso ao microfone: você entrou só para ouvir.');
    }
    this.ice = await api('GET', '/ice').catch(() => null);
    this.channelId = channelId;
    this.watch(S.me.id, this.local);
    this.timer = setInterval(() => this.tick(), 150);
    sendOp({ op: 'voice_join', channelId, ...this.flags() });
  },

  // `cue` is 'disconnect' when the call ended without the user asking.
  leave(silent, cue = 'leave') {
    if (!this.channelId) return;
    if (!silent) sendOp({ op: 'voice_leave' });
    Sounds.play(cue);
    this.present = null;
    this.streaming = null;
    for (const uid of [...this.peers.keys()]) this.drop(uid);
    this.local?.getTracks().forEach(t => t.stop());
    this.videoTrack?.stop();
    clearInterval(this.timer);
    this.meters.clear();
    this.ctx?.close().catch(() => {});
    Object.assign(this, { channelId: null, local: null, videoTrack: null, video: null, ctx: null });
  },

  // Called with the authoritative participant list whenever it changes.
  sync(users) {
    const ids = new Set(users.map(u => u.userId));
    if (!ids.has(S.me.id)) return;
    const streaming = new Set(users.filter(u => u.video === 'screen').map(u => u.userId));
    // One cue per update: arrivals and departures win over stream changes.
    if (!this.present) Sounds.play('join');
    else if ([...ids].some(id => !this.present.has(id))) Sounds.play('join');
    else if ([...this.present].some(id => !ids.has(id))) Sounds.play('leave');
    else if ([...streaming].some(id => !this.streaming?.has(id))) Sounds.play('stream_start');
    else if ([...(this.streaming || [])].some(id => ids.has(id) && !streaming.has(id))) Sounds.play('stream_stop');
    this.present = ids;
    this.streaming = streaming;
    for (const uid of ids) if (uid !== S.me.id && !this.peers.has(uid)) this.peer(uid);
    for (const uid of [...this.peers.keys()]) if (!ids.has(uid)) this.drop(uid);
  },

  peer(uid) {
    const pc = new RTCPeerConnection({
      iceServers: this.ice?.iceServers || [{ urls: 'stun:stun.l.google.com:19302' }],
      iceTransportPolicy: this.ice?.relayOnly ? 'relay' : 'all',
    });
    const audio = new Audio();
    audio.autoplay = true;
    audio.muted = this.deaf;
    const p = { pc, audio, polite: S.me.id < uid, makingOffer: false, ignoreOffer: false, videoSender: null, videoStream: null, queue: Promise.resolve() };
    this.peers.set(uid, p);
    this.local.getTracks().forEach(t => pc.addTrack(t, this.local));
    if (this.videoTrack) p.videoSender = this.addVideo(pc);
    const signal = data => sendOp({ op: 'signal', to: uid, data });

    pc.onnegotiationneeded = async () => {
      try {
        p.makingOffer = true;
        await pc.setLocalDescription();
        signal({ description: pc.localDescription });
      } catch (e) {
        console.error(e);
      } finally {
        p.makingOffer = false;
      }
    };
    pc.onicecandidate = ({ candidate }) => candidate && signal({ candidate });
    // Encoder limits only stick once a negotiation has finished.
    pc.onsignalingstatechange = () => pc.signalingState === 'stable' && this.tune(p);
    pc.ontrack = ({ track }) => {
      if (track.kind === 'audio') {
        const stream = new MediaStream([track]);
        audio.srcObject = stream;
        audio.play().catch(() => {});
        this.watch(uid, stream);
      } else {
        p.videoStream = new MediaStream([track]);
        renderStage();
      }
    };
    return p;
  },

  drop(uid) {
    const p = this.peers.get(uid);
    if (!p) return;
    p.pc.close();
    p.audio.srcObject = null;
    this.peers.delete(uid);
    this.meters.delete(uid);
  },

  // Perfect negotiation: both sides may offer at once; the polite one yields.
  onSignal(from, data) {
    if (!this.channelId) return;
    const p = this.peers.get(from) || this.peer(from);
    p.queue = p.queue.then(async () => {
      const { pc } = p;
      if (data.description) {
        const collision = data.description.type === 'offer' && (p.makingOffer || pc.signalingState !== 'stable');
        p.ignoreOffer = !p.polite && collision;
        if (p.ignoreOffer) return;
        await pc.setRemoteDescription(data.description);
        if (data.description.type === 'offer') {
          await pc.setLocalDescription();
          sendOp({ op: 'signal', to: from, data: { description: pc.localDescription } });
        }
      } else if (data.candidate) {
        try {
          await pc.addIceCandidate(data.candidate);
        } catch (e) {
          if (!p.ignoreOffer) throw e;
        }
      }
    }).catch(e => console.error('voice signal', e));
  },

  push() {
    sendOp({ op: 'voice_state', ...this.flags() });
  },

  toggleMute() {
    if (!this.channelId) return;
    if (this.deaf) return this.toggleDeaf();
    if (!this.local.getAudioTracks().length) return toast('Nenhum microfone disponível.');
    this.mute = !this.mute;
    Sounds.play(this.mute ? 'mute' : 'unmute');
    this.applyAudio();
  },

  toggleDeaf() {
    if (!this.channelId) return;
    this.deaf = !this.deaf;
    Sounds.play(this.deaf ? 'deafen' : 'undeafen');
    this.applyAudio();
  },

  applyAudio() {
    this.local.getAudioTracks().forEach(t => (t.enabled = !this.mute && !this.deaf));
    for (const p of this.peers.values()) p.audio.muted = this.deaf;
    this.push();
  },

  async setVideo(kind) {
    if (!this.channelId) return;
    if (this.videoTrack) {
      this.videoTrack.onended = null;
      this.videoTrack.stop();
      for (const p of this.peers.values()) {
        if (p.videoSender) p.pc.removeTrack(p.videoSender);
        p.videoSender = null;
      }
      this.videoTrack = null;
    }
    if (kind) {
      try {
        const video = this.videoConstraints();
        const stream = kind === 'camera'
          ? await navigator.mediaDevices.getUserMedia({ video })
          : await navigator.mediaDevices.getDisplayMedia({ video, audio: false });
        this.videoTrack = stream.getVideoTracks()[0];
        this.videoTrack.contentHint = this.hint(kind);
        // Some capture paths ignore the request constraints; ask again on the track.
        await this.videoTrack.applyConstraints(video).catch(() => {});
      } catch {
        kind = null;
        toast('Não foi possível iniciar o vídeo.');
      }
    }
    this.video = kind;
    if (this.videoTrack) {
      this.videoTrack.onended = () => this.setVideo(null);
      for (const p of this.peers.values()) p.videoSender = this.addVideo(p.pc);
    }
    this.video = kind;
    this.push();
  },

  addVideo(pc) {
    const sender = pc.addTrack(this.videoTrack, this.local);
    const transceiver = pc.getTransceivers().find(t => t.sender === sender);
    const caps = window.RTCRtpReceiver?.getCapabilities?.('video');
    if (transceiver?.setCodecPreferences && caps) {
      // VP9 keeps screen text crisp at a given bitrate; H.264 is usually
      // hardware-encoded, which matters for camera and 60 fps.
      const order = this.video === 'screen' && this.quality.fps < 60 ? ['VP9', 'H264', 'VP8'] : ['H264', 'VP9', 'VP8'];
      const rank = c => {
        const i = order.findIndex(name => c.mimeType.toUpperCase().endsWith('/' + name));
        return i < 0 ? order.length : i;
      };
      try {
        transceiver.setCodecPreferences([...caps.codecs].sort((a, b) => rank(a) - rank(b)));
      } catch (e) {
        console.warn('codec preferences', e);
      }
    }
    return sender;
  },

  async tune(p) {
    const q = this.quality;
    for (const sender of p.pc.getSenders()) {
      if (!sender.track) continue;
      const params = sender.getParameters();
      if (!params.encodings?.length) continue;
      const enc = params.encodings[0];
      if (sender.track.kind === 'audio') {
        enc.maxBitrate = VOICE_BITRATE;
        enc.priority = enc.networkPriority = 'high';
      } else {
        enc.maxBitrate = (this.video === 'screen' ? q.screen : q.camera) * 1000;
        enc.maxFramerate = q.fps;
        enc.scaleResolutionDownBy = 1;
        params.degradationPreference = this.video === 'screen' && q.fps < 60 ? 'maintain-resolution' : 'balanced';
      }
      try {
        await sender.setParameters(params);
      } catch (e) {
        console.warn('encoder settings', e);
      }
    }
  },

  // Called when the quality setting changes mid-call.
  async applyQuality() {
    if (this.videoTrack) {
      this.videoTrack.contentHint = this.hint(this.video);
      await this.videoTrack.applyConstraints(this.videoConstraints()).catch(() => {});
    }
    for (const p of this.peers.values()) this.tune(p);
  },

  streamFor(uid) {
    if (uid === S.me.id) return this.videoTrack ? new MediaStream([this.videoTrack]) : null;
    return this.peers.get(uid)?.videoStream || null;
  },

  watch(uid, stream) {
    if (!stream.getAudioTracks().length) return;
    try {
      this.ctx ||= new AudioContext();
      const analyser = this.ctx.createAnalyser();
      analyser.fftSize = 512;
      this.ctx.createMediaStreamSource(stream).connect(analyser);
      this.meters.set(uid, { analyser, buf: new Uint8Array(analyser.frequencyBinCount) });
    } catch (e) {
      console.warn('speaking meter', e);
    }
  },

  tick() {
    for (const [uid, m] of this.meters) {
      m.analyser.getByteFrequencyData(m.buf);
      let sum = 0;
      for (const v of m.buf) sum += v;
      const speaking = sum / m.buf.length > 14 && !(uid === S.me.id && (this.mute || this.deaf));
      for (const el of document.querySelectorAll(`[data-speak="${uid}"]`)) el.classList.toggle('speaking', speaking);
    }
  },
};
