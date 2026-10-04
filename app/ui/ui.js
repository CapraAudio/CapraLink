const { invoke } = window.__TAURI__.core;
const $ = (id) => document.getElementById(id);
const els = {
  input: $('input'), output: $('output'), channels: $('channels'), bitrate: $('bitrate'),
  bitrateVal: $('bitrateVal'), service: $('service'), remoteCfg: $('remoteCfg'), autoRc: $('autoRc'), music: $('music'), hifi: $('hifi'), error: $('error'), status: $('status'), stats: $('stats'), quality: $('quality'), devices: $('devices'),
  myNameRow: $('myNameRow'),
};

let channels = 1;
let effectiveMusic = false;
let effectiveHifi = false, hifiFallback = false;
let pttMode = 'off', pttWaiting = false, pttSig = '';
let liveRate = null; // kbps actually being sent, while streaming // the link is in Music Mode (either side has it on)
let settingsLoaded = false;
let currentId;
let actionError = '';
let busy = false;
let pairingId = null;   // device whose PIN box is open
let pinDraft = '';
let editAddr = null;    // open "Edit address" box: {id, draft}
let lastDevices = '';
let devices = [];
let remote = null;      // open "Settings on <name>" panel: {id, name, inputs, outputs, input_devices, output_devices, draft}
let renaming = null;    // draft name while the "This device" name is being edited, else null
let lastNameSig = '';
let menuFor = null; // device whose ⋯ menu is open
let currentPin = '';
let pinShown = false;
let audioSig = '';      // the engine's audio settings as last loaded into the fields
let forgetting = null;  // device whose "Forget <name>?" confirmation is open
let lastPeer;           // connected device id at the last poll (undefined before the first: no announcement)
let lastErr = '';

// Polite screen-reader announcement (the #live region). Callers announce only on a change.
const announce = (msg) => { $('live').textContent = msg; };
function setError(msg) {
  els.error.textContent = msg;
  if (msg !== lastErr && msg) announce(msg);
  lastErr = msg;
}
// Moves focus back to the control that opened a popover/panel for device `id` (its ⋯ or Pair button)
const focusOpener = (id) => { const b = [...els.devices.querySelectorAll('[data-opener]')].find((n) => n.dataset.opener === id); if (b) b.focus(); };

function setChannels(v) {
  channels = v;
  showMusic();
}

const MUSIC_RATE = 'up to 160 kbps (Music Mode)';
// Music Mode forces stereo and its own bitrate ceiling: show those instead of the user's choice
function showMusic() {
  const on = els.music.checked || effectiveMusic;
  for (const b of els.channels.children) {
    b.disabled = on;
    b.classList.toggle('active', Number(b.dataset.v) === (on ? 2 : channels));
    b.setAttribute('aria-pressed', b.classList.contains('active'));
  }
  els.bitrate.disabled = on;
  // the track fills to the bitrate actually being sent; the target is the knob, or in Music Mode
  // its fixed 160 kbps ceiling (the knob is hidden then: the user's target doesn't apply)
  const target = on ? 160 : Number(els.bitrate.value);
  els.bitrate.classList.toggle('music', on);
  const lossless = !!liveRate && effectiveHifi;
  els.hifi.disabled = !on; // Hi-Fi rides on Music Mode
  els.bitrate.style.setProperty('--live', lossless ? 1 : liveRate ? Math.min(1, Math.max(0, (liveRate - 8) / ((on ? 160 : 96) - 8))) : 0);
  // current/target, e.g. "48kbps/64kbps" (0 when nothing is being sent)
  els.bitrateVal.textContent = `${liveRate || 0}kbps/${lossless ? 'lossless' : target + 'kbps'}`;
  $('musicHint').textContent = hifiFallback ? "Hi-Fi paused: the network couldn't keep up — using Music Mode, retrying shortly."
    : els.hifi.checked ? "Lossless 24-bit, about 2.3 Mbit/s each way, up to 1 s of delay. Falls back to Music Mode if the network can't keep up."
    : 'Stereo, higher quality, a little more delay — applies to both directions of the link.';
  els.bitrate.setAttribute('aria-valuetext', `${target} kbps ${on ? 'ceiling (Music Mode)' : 'target'}, sending ${liveRate || 0} kbps`);
}

// ---- actions ----
async function act(cmd, args) {
  busy = true; actionError = ''; lastDevices = ''; render();
  let ok = true;
  try { await invoke(cmd, typeof args === 'function' ? await args() : args); } catch (e) { actionError = String(e); ok = false; }
  busy = false; lastDevices = '';
  await poll();
  return ok;
}

// Save only the field(s) the user changed: the engine merges them onto its current settings, so a
// remote change to another field (e.g. the microphone) made meanwhile isn't undone.
function saveSetting(patch) {
  act('patch_settings', () => ({ patch: patch() }));
}

els.channels.addEventListener('click', (e) => {
  if (e.target.tagName !== 'BUTTON') return;
  setChannels(Number(e.target.dataset.v));
  saveSetting(() => ({ channels }));
});
els.bitrate.addEventListener('input', showMusic);
els.music.addEventListener('change', () => { showMusic(); saveSetting(() => ({ music_mode: els.music.checked })); });
els.hifi.addEventListener('change', () => { showMusic(); saveSetting(() => ({ hifi: els.hifi.checked })); });
// volumes: the percentage follows the slider live, the engine is told when it is let go
for (const [id, key] of [['sendVol', 'send_volume'], ['recvVol', 'recv_volume']]) {
  const showVol = () => { $(id + 'V').textContent = $(id).value + '%'; $(id).setAttribute('aria-valuetext', $(id).value + ' percent'); };
  $(id).addEventListener('input', showVol);
  $(id).addEventListener('change', () => saveSetting(() => ({ [key]: Number($(id).value) })));
  $(id).showVol = showVol;
}
$('mute').addEventListener('click', () => saveSetting(() => ({ mute: $('mute').getAttribute('aria-pressed') !== 'true' })));
$('pttMode').addEventListener('change', () => saveSetting(() => ({ ptt: $('pttMode').value })));
$('pttSet').addEventListener('click', async () => {
  if (pttWaiting) return;
  pttWaiting = true; showPtt(); announce('Press a key or button');
  try {
    const k = await invoke('ptt_capture');
    pttWaiting = false;
    saveSetting(() => ({ ptt_key: k, ptt: pttMode === 'off' ? 'hold' : pttMode }));
  } catch (e) { pttWaiting = false; actionError = String(e); poll(); }
});
// Push-to-talk controls: the select needs a button first; the hint says what to do next
let pttKey = null, pttError = null;
function showPtt() {
  $('pttSet').textContent = pttWaiting ? 'Press a key or button…' : pttKey ? 'Change button…' : 'Set button…';
  $('pttMode').disabled = !pttKey;
  $('pttMode').value = pttMode;
  const sig = JSON.stringify([pttKey, pttMode, pttError]);
  if (sig === pttSig) return;
  pttSig = sig;
  const h = $('pttHint');
  h.classList.toggle('err', !!pttError);
  if (pttError) { h.textContent = pttError; return; }
  if (!pttKey) { h.textContent = "Set a button first. It works while games have focus, and you'll hear a chirp when you start and stop talking."; return; }
  h.replaceChildren('Button: ', el('b', 'keycap', pttKey.label), '. ' + (pttMode === 'hold' ? 'Hold it to talk.' : pttMode === 'toggle' ? 'Press once to start, again to stop.' : 'Choose Hold or Toggle above.'));
}
els.bitrate.addEventListener('change', () => saveSetting(() => ({ bitrate: Number(els.bitrate.value) * 1000 })));
// "Refresh devices…" re-reads this computer's devices and keeps both selections
async function refreshDevices() {
  const keep = [els.input.dataset.prev, els.output.dataset.prev];
  try { await loadDevices(); } catch (e) { actionError = String(e); }
  setSel(els.input, keep[0]);
  setSel(els.output, keep[1]);
}
for (const sel of [els.input, els.output]) {
  sel.addEventListener('change', () => {
    if (isRefresh(sel)) return refreshDevices();
    sel.dataset.prev = sel.value;
    saveSetting(() => ({ [sel === els.input ? 'input' : 'output']: sel.value || null }));
  });
}
els.service.addEventListener('change', () => saveSetting(() => ({ service: els.service.checked })));
els.remoteCfg.addEventListener('change', () => saveSetting(() => ({ remote_config: els.remoteCfg.checked })));
els.autoRc.addEventListener('change', () => saveSetting(() => ({ auto_reconnect: els.autoRc.checked })));

// ---- this device's name ----
function renderName(name) {
  // stays 'edit' while typing, so the 1 s poll can't clobber the draft or steal focus
  const sig = renaming === null ? 'view:' + name : 'edit';
  if (sig === lastNameSig) return;
  lastNameSig = sig;
  const box = els.myNameRow;
  box.innerHTML = '';
  if (renaming === null) {
    box.appendChild(el('span', null, name));
    box.appendChild(button('Rename', 'link', () => { renaming = name; lastNameSig = ''; renderName(name); }));
    return;
  }
  const input = el('input');
  input.setAttribute('aria-label', 'Device name');
  input.value = renaming;
  const cancel = () => { renaming = null; lastNameSig = ''; renderName(name); box.querySelector('button').focus(); };
  const save = async () => { const n = renaming; if (await act('set_name', { name: n })) cancel(); };
  input.addEventListener('input', () => { renaming = input.value; });
  input.addEventListener('keydown', (e) => {
    if (e.key === 'Enter') save();
    if (e.key === 'Escape') { e.stopPropagation(); cancel(); }
  });
  box.appendChild(input);
  box.appendChild(button('Save', 'btn', save));
  box.appendChild(button('Cancel', 'link', cancel));
  setTimeout(() => { input.focus(); input.select(); });
}

// ---- pair by IP (manual fallback when mDNS discovery can't see the other computer) ----
els.ipAddr = $('ipAddr'); els.ipPin = $('ipPin'); els.ipPairBtn = $('ipPairBtn');
els.ipPin.addEventListener('input', () => { els.ipPin.value = els.ipPin.value.replace(/\D/g, ''); });
async function pairIp() {
  const addr = els.ipAddr.value, pin = els.ipPin.value;
  busy = true; actionError = ''; els.ipPairBtn.disabled = true; render();
  try { await invoke('pair_ip', { addr, pin }); els.ipAddr.value = ''; els.ipPin.value = ''; announce('Paired'); }
  catch (e) { actionError = String(e); }
  busy = false; els.ipPairBtn.disabled = false;
  await poll();
}
els.ipPairBtn.addEventListener('click', pairIp);
els.ipAddr.addEventListener('keydown', (e) => { if (e.key === 'Enter') pairIp(); });
els.ipPin.addEventListener('keydown', (e) => { if (e.key === 'Enter') pairIp(); });

// ---- devices list ----
function el(tag, cls, text) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text !== undefined) e.textContent = text;
  return e;
}

function button(text, cls, onclick) {
  const b = el('button', cls, text);
  b.disabled = busy;
  b.addEventListener('click', onclick);
  return b;
}

function render() {
  // rebuild only when something changed, so a half-typed PIN survives the 1 s poll
  const sig = JSON.stringify([devices, pairingId, busy, remote && remote.id, editAddr && editAddr.id, menuFor, forgetting]);
  if (sig === lastDevices) return;
  lastDevices = sig;
  const list = els.devices;
  // a rebuild (from the poll or any action) keeps keyboard focus on the same control, if it still exists
  const ctl = () => [...list.querySelectorAll('button,input,select')];
  const key = (n) => n.tagName + '|' + (n.textContent || n.placeholder || n.getAttribute('aria-label'));
  const at = list.contains(document.activeElement) ? ctl().indexOf(document.activeElement) : -1;
  const was = at >= 0 && key(document.activeElement);
  list.innerHTML = '';
  if (!devices.length) {
    list.appendChild(el('div', 'empty', 'Looking for other CapraLink computers on this network…'));
    return;
  }
  for (const d of devices) {
    const row = el('div', 'dev');
    const top = el('div', 'top');
    top.appendChild(el('div', 'name', d.name));
    if (d.paired) {
      const more = button('⋯', 'more', (e) => {
        e.stopPropagation();
        const open = menuFor !== d.id;
        menuFor = open ? d.id : null; lastDevices = ''; render();
        if (open) els.devices.querySelector('.menu button').focus(); else focusOpener(d.id);
      });
      more.dataset.opener = d.id;
      more.setAttribute('aria-label', 'More actions for ' + d.name);
      more.setAttribute('aria-haspopup', 'menu');
      more.setAttribute('aria-expanded', menuFor === d.id);
      top.appendChild(more);
      if (menuFor === d.id) {
        const m = el('div', 'menu');
        m.setAttribute('role', 'menu');
        m.addEventListener('keydown', (e) => { // arrow keys move between the items
          const items = [...m.children], i = items.indexOf(document.activeElement);
          if (e.key === 'ArrowDown') items[(i + 1) % items.length].focus();
          else if (e.key === 'ArrowUp') items[(i + items.length - 1) % items.length].focus();
          else return;
          e.preventDefault();
        });
        const item = (t, f, cls) => { const b = button(t, cls || '', () => { menuFor = null; lastDevices = ''; f(); }); b.setAttribute('role', 'menuitem'); m.appendChild(b); };
        if (d.reachable) item('Configure', () => openRemote(d.id));
        item('Edit address', () => { editAddr = { id: d.id, draft: d.addr || '' }; render(); });
        item('Forget', () => { forgetting = d.id; render(); els.devices.querySelector('.forget .link').focus(); }, 'danger');
        top.appendChild(m);
      }
    }
    row.appendChild(top);
    const status = d.connected ? 'Paired · Connected'
      : d.reconnecting ? 'Paired · Reconnecting…'
      : !d.paired ? 'Not paired'
      : d.online ? 'Paired · Online'
      : d.reachable ? 'Paired · Offline (last seen at ' + d.addr + ')'
      : 'Paired · Offline';
    const bottom = el('div', 'bottom');
    bottom.appendChild(el('div', 'st', status));
    if (!d.paired) {
      const b = button('Pair', 'btn', () => { pairingId = pairingId === d.id ? null : d.id; pinDraft = ''; render(); });
      b.dataset.opener = d.id; b.setAttribute('aria-label', 'Pair with ' + d.name);
      bottom.appendChild(b);
    } else if (d.connected) {
      bottom.appendChild(button('Disconnect', 'btn stop', () => act('disconnect'))).setAttribute('aria-label', 'Disconnect from ' + d.name);
    } else if (d.reachable) {
      bottom.appendChild(button('Connect', 'btn', () => act('connect', { id: d.id }))).setAttribute('aria-label', 'Connect to ' + d.name);
    }
    row.appendChild(bottom);
    list.appendChild(row);
    if (pairingId === d.id && !d.paired) {
      const box = el('div', 'pairbox');
      const pin = el('input');
      pin.setAttribute('aria-label', 'PIN shown on ' + d.name); pin.maxLength = 6; pin.inputMode = 'numeric'; pin.placeholder = '6-digit PIN'; pin.value = pinDraft;
      const confirm = async () => { const p = pin.value; pairingId = null; if (await act('pair', { id: d.id, pin: p })) announce('Paired with ' + d.name); };
      pin.addEventListener('input', () => { pin.value = pin.value.replace(/\D/g, ''); pinDraft = pin.value; });
      pin.addEventListener('keydown', (e) => { if (e.key === 'Enter') confirm(); });
      box.appendChild(pin);
      box.appendChild(button('Confirm', 'btn', confirm));
      list.appendChild(box);
      setTimeout(() => pin.focus());
    }
    if (editAddr && editAddr.id === d.id && d.paired) {
      const box = el('div', 'pairbox');
      const input = el('input', 'ip');
      input.setAttribute('aria-label', 'IP address of ' + d.name);
      input.placeholder = '192.168.1.20'; input.value = editAddr.draft;
      const cancel = () => { editAddr = null; render(); focusOpener(d.id); };
      const save = async () => { if (await act('set_peer_addr', { id: d.id, addr: input.value })) cancel(); };
      input.addEventListener('input', () => { editAddr.draft = input.value; });
      input.addEventListener('keydown', (e) => { if (e.key === 'Enter') save(); }); // Escape: see below
      box.appendChild(input);
      box.appendChild(button('Save', 'btn', save));
      box.appendChild(button('Cancel', 'link', cancel));
      list.appendChild(box);
      setTimeout(() => input.focus());
    }
    if (forgetting === d.id && d.paired) {
      const box = el('div', 'pairbox forget');
      box.setAttribute('role', 'group'); box.setAttribute('aria-label', 'Forget ' + d.name + '?');
      box.appendChild(el('span', null, 'Forget ' + d.name + '?'));
      box.appendChild(button('Forget', 'btn stop', () => { forgetting = null; act('forget', { id: d.id }); }));
      box.appendChild(button('Cancel', 'link', () => { forgetting = null; render(); }));
      list.appendChild(box);
    }
    if (remote && remote.id === d.id && d.paired) list.appendChild(remotePanel());
  }
  if (at >= 0) { const n = ctl()[at]; if (n && key(n) === was) n.focus(); }
}

// Escape closes the topmost open popover/panel and returns focus to the control that opened it.
// To make another popover close this way, add a line: [is it open, close it].
document.addEventListener('keydown', (e) => {
  if (e.key !== 'Escape') return;
  const closeFor = (get, set) => () => { const id = get(); set(null); lastDevices = ''; render(); focusOpener(id); };
  const layers = [
    [!$('opts').hidden, () => setOpts(false, true)],
    [menuFor, closeFor(() => menuFor, (v) => { menuFor = v; })],
    [forgetting, closeFor(() => forgetting, (v) => { forgetting = v; })],
    [editAddr, closeFor(() => editAddr.id, (v) => { editAddr = v; })],
    [pairingId, closeFor(() => pairingId, (v) => { pairingId = v; })],
    [remote, closeFor(() => remote.id, (v) => { remote = v; })],
    [!$('trouble').hidden, () => { $('trouble').hidden = true; $('optsBtn').focus(); }],
  ];
  const open = layers.find((l) => l[0]);
  if (open) { open[1](); e.preventDefault(); }
});

// ---- remote configuration (edits live in remote.draft, so a rebuild keeps them) ----
async function openRemote(id) {
  if (remote && remote.id === id) { remote = null; lastDevices = ''; render(); return; }
  busy = true; actionError = ''; remote = null; lastDevices = ''; render();
  try { const c = await invoke('remote_get', { id }); remote = { id, ...c, draft: { ...c.settings }, nameDraft: c.name }; }
  catch (e) { actionError = String(e); }
  busy = false; lastDevices = '';
  await poll();
  const first = els.devices.querySelector('.remote input'); // keyboard users land in the panel
  if (first) first.focus();
}

function remotePanel() {
  const r = remote, s = r.draft;
  const box = el('div', 'remote');
  box.setAttribute('role', 'group'); box.setAttribute('aria-label', 'Settings on ' + r.name);
  box.appendChild(el('h2', null, 'Settings on ' + r.name));
  let n = 0; // a label tied to its control
  const lab = (text, ctl) => { ctl.id = 'remote-' + n++; const l = el('label', null, text); l.htmlFor = ctl.id; box.append(l, ctl); };
  const nameInput = el('input');
  nameInput.value = r.nameDraft;
  nameInput.addEventListener('input', () => { r.nameDraft = nameInput.value; });
  lab('Name', nameInput);
  const pick = (label, special, hint, key) => {
    const sel = el('select');
    const devs = r[key + '_devices']; // absent from an older computer: its lists are names
    fill(sel, devs && devs.length ? devs : r[key + 's'], special, hint);
    setSel(sel, s[key]);
    sel.addEventListener('change', async () => {
      if (!isRefresh(sel)) { s[key] = sel.value || null; return; }
      try { const c = await invoke('remote_get', { id: r.id }); for (const k of ['inputs', 'outputs', 'input_devices', 'output_devices']) r[k] = c[k]; }
      catch (e) { actionError = String(e); }
      lastDevices = ''; render(); // rebuilds the panel from the draft, so choices stay
    });
    lab(label, sel);
  };
  box.appendChild(el('h2', 'sub', 'Sending'));
  pick('Send from', 'CapraLink Output', 'apps on that computer play into it', 'input');
  box.appendChild(el('label', null, 'Channels'));
  const seg = el('div', 'seg');
  seg.setAttribute('role', 'group'); seg.setAttribute('aria-label', 'Channels');
  const rate = el('input');
  const val = el('div', 'bitrate-val');
  const sync = () => {
    const on = !!s.music_mode;
    for (const c of seg.children) { c.disabled = on; c.classList.toggle('active', Number(c.dataset.v) === (on ? 2 : s.channels)); c.setAttribute('aria-pressed', c.classList.contains('active')); }
    rate.disabled = on;
    val.textContent = on ? MUSIC_RATE : rate.value + ' kbps';
    rate.setAttribute('aria-valuetext', on ? MUSIC_RATE : rate.value + ' kbps target');
  };
  for (const [v, t] of [[1, 'Mono'], [2, 'Stereo']]) {
    const b = el('button', null, t);
    b.dataset.v = v;
    b.addEventListener('click', () => { s.channels = v; sync(); });
    seg.appendChild(b);
  }
  box.appendChild(seg);
  rate.type = 'range'; rate.min = 8; rate.max = 96; rate.step = 8; rate.value = Math.round(s.bitrate / 1000);
  rate.addEventListener('input', () => { s.bitrate = Number(rate.value) * 1000; sync(); });
  lab('Bitrate Target', rate);
  box.appendChild(val);
  const vol = (text, key) => {
    const r = el('input'), out = el('div', 'bitrate-val');
    r.type = 'range'; r.min = 0; r.max = 150; r.value = s[key] ?? 100;
    const sh = () => { out.textContent = r.value + '%'; r.setAttribute('aria-valuetext', r.value + ' percent'); };
    r.addEventListener('input', () => { s[key] = Number(r.value); sh(); });
    sh(); lab(text, r); box.appendChild(out);
  };
  vol('Send volume', 'send_volume');
  const mute = el('label', 'check'), mcb = el('input');
  mcb.type = 'checkbox'; mcb.checked = !!s.mute;
  mcb.addEventListener('change', () => { s.mute = mcb.checked; });
  mute.append(mcb, ' Mute');
  box.appendChild(mute);
  box.appendChild(el('h2', 'sub', 'Receiving'));
  pick('Play to', 'CapraLink Input', 'apps on that computer record from it', 'output');
  vol('Receive volume', 'recv_volume');
  const music = el('label', 'check');
  const cb = el('input');
  cb.type = 'checkbox'; cb.checked = !!s.music_mode;
  cb.addEventListener('change', () => { s.music_mode = cb.checked; sync(); });
  music.append(cb, ' Music Mode');
  box.appendChild(music);
  sync();
  const btns = el('div', 'btns');
  btns.appendChild(button('Close', 'link', () => { remote = null; lastDevices = ''; render(); }));
  btns.appendChild(button('Save', 'btn', async () => {
    const name = r.nameDraft !== r.name ? r.nameDraft : null;
    if (await act('remote_set', { id: r.id, settings: s, name })) { remote = null; lastDevices = ''; render(); }
  }));
  box.appendChild(btns);
  return box;
}

// keeps a saved device selectable even while it is unplugged. Settings saved before 0.2 hold
// the device's name: that selects its entry (the next save stores the id).
function setSel(sel, v) {
  const opts = [...sel.options].filter((o) => !o.dataset.refresh);
  const named = v && !opts.some((o) => o.value === v) && opts.find((o) => o.dataset.name === v);
  if (named) v = named.value;
  else if (v && !opts.some((o) => o.value === v)) {
    const o = document.createElement('option');
    // an id ("coreaudio:…", "wasapi:…") means nothing to people
    o.value = v; o.textContent = /^[a-z]+:/.test(v) ? 'Saved device (not connected)' : v + ' (not found)';
    sel.insertBefore(o, sel.querySelector('option[data-refresh]')); // Refresh stays last
  }
  sel.value = v || '';
  sel.dataset.prev = v || '';
}

const isRefresh = (sel) => !!(sel.selectedOptions[0] && sel.selectedOptions[0].dataset.refresh);

// ---- PIN: hidden until asked for; Show opens the engine's 2-minute pairing window ----
let pairSecs = 0, pairLocked = 0;
function showPin(on) {
  pinShown = on;
  $('pin').textContent = on && currentPin ? currentPin : '••• •••';
  $('pinToggle').textContent = on ? 'Hide' : 'Show';
  $('pinToggle').setAttribute('aria-label', on ? 'Hide PIN' : 'Show PIN');
  $('pinToggle').disabled = pairLocked > 0;
  $('pinHint').textContent = pairLocked > 0
    ? 'Pairing locked for ' + (pairLocked >= 60 ? Math.ceil(pairLocked / 60) + ' min' : pairLocked + ' s') + ' after a wrong PIN'
    : pairSecs > 0
      ? 'Enter this PIN on the other computer · ' + Math.floor(pairSecs / 60) + ':' + String(pairSecs % 60).padStart(2, '0') + ' left'
      : 'Press Show to pair a new computer — the PIN works for 2 minutes';
}
$('pinToggle').addEventListener('click', async () => {
  if (!pinShown && pairSecs <= 0) {
    try { await invoke('open_pairing'); } catch (e) { actionError = String(e); return poll(); }
  }
  showPin(!pinShown);
  poll();
});

// ---- VU meters: ~15 updates/s from the engine's VU levels; CSS animates between them ----
const pct = (p) => (Math.max(-60, 20 * Math.log10(p || 1e-9)) + 60) / 60 * 100;
let meterAt = 0; // screen readers get the level about once a second, not at the animation rate
function setBars(inP, outP) {
  if (Date.now() - meterAt >= 1000) {
    meterAt = Date.now();
    $('inBar').setAttribute('aria-valuenow', Math.round(pct(inP)));
    $('outBar').setAttribute('aria-valuenow', Math.round(pct(outP)));
  }
  $('inLevel').style.width = (100 - pct(inP)) + '%';   // the cover shrinks as the level rises
  $('outLevel').style.width = (100 - pct(outP)) + '%';
}
let levelsBusy = false;
setInterval(async () => {
  // only while streaming: each poll is a local connection, so idle windows make none
  if (levelsBusy || document.hidden || !els.status.classList.contains('on')) { if (!levelsBusy) setBars(0, 0); return; }
  levelsBusy = true;
  try { const l = await invoke('levels'); l ? setBars(l[0], l[1]) : setBars(0, 0); }
  catch (_) { setBars(0, 0); }
  finally { levelsBusy = false; }
}, 66);

// ---- polling ----

async function poll() {
  let st;
  try { st = await invoke('state'); } catch (e) { setError(String(e)); return; }
  renderName(st.name);
  currentPin = st.pin.slice(0, 3) + ' ' + st.pin.slice(3);
  pairSecs = st.pairing_secs; pairLocked = st.pairing_locked_secs;
  showPin(pinShown && pairSecs > 0);
  // a different current connection brings its own saved audio settings
  if (st.current !== currentId) { currentId = st.current; settingsLoaded = false; }
  const cur = st.devices.find((d) => d.id === st.current);
  $('sendPeer').textContent = cur ? ' to ' + cur.name : '';
  $('recvPeer').textContent = cur ? ' from ' + cur.name : '';
  // reload the fields when the engine's values differ (first load, another connection, a remote edit),
  // unless the user is mid-edit: the next poll picks it up
  const cfg = st.settings;
  const sig = JSON.stringify([cfg.input, cfg.output, cfg.bitrate, cfg.channels]);
  if (!settingsLoaded || (sig !== audioSig && ![els.input, els.output, els.bitrate].includes(document.activeElement))) {
    settingsLoaded = true;
    audioSig = sig;
    setSel(els.input, cfg.input);
    setSel(els.output, cfg.output);
    els.bitrate.value = Math.round(cfg.bitrate / 1000);
    channels = cfg.channels;
  }
  els.service.checked = st.settings.service; // also undoes a failed toggle
  els.remoteCfg.checked = st.settings.remote_config;
  els.autoRc.checked = st.settings.auto_reconnect;
  els.music.checked = !!st.settings.music_mode; // also follows the tray menu's toggle
  els.hifi.checked = !!cfg.hifi;
  effectiveMusic = !!(st.stats && st.stats.music);
  effectiveHifi = !!(st.stats && st.stats.hifi);
  hifiFallback = !!(st.stats && st.stats.hifi_fallback);
  for (const [id, v] of [['sendVol', cfg.send_volume], ['recvVol', cfg.recv_volume]]) {
    if (document.activeElement !== $(id)) $(id).value = v ?? 100; // not while it is being dragged
    $(id).showVol();
  }
  $('mute').setAttribute('aria-pressed', !!cfg.mute);
  $('mute').textContent = cfg.mute ? 'Muted' : 'Mute';
  pttMode = cfg.ptt || 'off'; pttKey = cfg.ptt_key || null; pttError = st.ptt_error || null;
  showPtt();
  // with push-to-talk on, the sending meter dims while you are not talking
  const quiet = pttMode !== 'off' && !!pttKey && !st.talking;
  $('inBar').classList.toggle('idle', quiet);
  $('inBar').setAttribute('aria-label', quiet ? 'Sending level (not talking)' : 'Sending level');
  showMusic();
  devices = st.devices;
  render();
  const peer = devices.find((d) => d.connected);
  els.status.textContent = peer ? 'Streaming' : 'Idle';
  const peerId = peer ? peer.id : null;
  if (lastPeer !== undefined && peerId !== lastPeer) announce(peer ? 'Streaming: connected to ' + peer.name : 'Idle: disconnected');
  lastPeer = peerId;
  els.status.classList.toggle('on', !!peer);
  const s = st.stats;
  liveRate = s ? Math.round(s.bitrate / 1000) : null;
  showMusic();
  if (s) {
    els.stats.textContent =
      `sent ${s.sent} · recv ${s.received} · lost ${s.lost} · fec ${s.fec_recovered} · underruns ${s.underruns} · buf ${s.buffer_ms.toFixed(0)}/${s.target_ms.toFixed(0)}ms · gap tx ${s.tx_gap_ms.toFixed(0)} rx ${s.rx_gap_ms.toFixed(0)}ms · ${Math.round(s.bitrate / 1000)} kbps · cx ${s.complexity}`;
  } else {
    els.stats.textContent = '';
  }
  showQuality(st.quality, cur);
  const dly = [['You hear them', s && s.delay_in_ms], ['They hear you', s && s.delay_out_ms]].filter((d) => d[1] != null).map((d) => d[0] + ': ' + d[1] + ' ms');
  $('delay').textContent = dly.join(' · ');
  $('delay').hidden = !peer || !dly.length;
  showPeerLog(cur);
  setError([actionError || st.error, st.virtual_error].filter(Boolean).join(' · '));
}

// ---- connection quality: the bottom line (click for the detailed stats) and the Troubleshooting panel ----
let statsOpen = false;
function gradeLine(box, q, text) {
  box.replaceChildren(el('b', q.grade, text), el('span', 'qhint', q.hint || ''));
  box.lastChild.hidden = !q.hint;
}
function showQuality(q, cur) {
  const word = q ? q.grade[0].toUpperCase() + q.grade.slice(1) : '';
  els.quality.hidden = !q;
  els.stats.hidden = !q || !statsOpen;
  if (q) gradeLine(els.quality, q, 'Connection: ' + word);
  if (q) gradeLine($('tConn'), q, word); else $('tConn').textContent = 'Not connected';
  if (q && cur) $('tConn').firstChild.after(' to ' + cur.name);
}
els.quality.addEventListener('click', () => { statsOpen = !statsOpen; els.stats.hidden = !statsOpen; });

// ---- Troubleshooting panel: a static section the poll only updates in place ----
let peerForLog = null; // current device id while its log can be included, else null
function showPeerLog(cur) {
  peerForLog = cur && cur.paired && cur.reachable ? cur.id : null;
  $('peerLogRow').hidden = !peerForLog;
  if (cur) $('peerLogText').textContent = "Include " + cur.name + "'s log";
}
async function runChecks() {
  const ul = $('checks');
  ul.replaceChildren(el('li', null, 'Checking…'));
  try {
    const rows = await invoke('checks');
    ul.replaceChildren(...rows.map((c) => {
      const li = el('li');
      const txt = el('span', null, c.title);
      if (!c.ok && c.fix) txt.append(el('br'), el('span', 'fix', c.fix));
      li.append(el('span', 'ic ' + (c.ok ? 'ok' : 'warn'), c.ok ? '✓' : '⚠'), txt);
      return li;
    }));
  } catch (e) { ul.replaceChildren(); $('audioResult').textContent = String(e); }
}
$('troubleBtn').addEventListener('click', () => {
  setOpts(false);
  $('trouble').hidden = !$('trouble').hidden;
  if (!$('trouble').hidden) { runChecks(); $('trouble').focus(); }
});
$('troubleClose').addEventListener('click', () => { $('trouble').hidden = true; $('optsBtn').focus(); });
// runs one audio test with its button disabled; `go` returns the result line
async function audioTest(btn, go) {
  btn.disabled = true;
  try { $('audioResult').textContent = await go(); } catch (e) { $('audioResult').textContent = String(e); }
  btn.disabled = false;
}
$('tone').addEventListener('click', () => audioTest($('tone'), async () => {
  await invoke('test_tone');
  return 'Played a test sound on ' + (els.output.value || 'System default') + '.';
}));
$('micCheck').addEventListener('click', () => audioTest($('micCheck'), async () => {
  $('audioResult').textContent = 'Listening for 3 s…';
  return (await invoke('mic_check')).message;
}));
$('export').addEventListener('click', async () => {
  const out = $('exportResult');
  $('export').disabled = true; out.textContent = '';
  try {
    const path = await invoke('export_diagnostics', { redact: $('redact').checked, peer: peerForLog && $('peerLog').checked ? peerForLog : null });
    if (path) out.textContent = 'Saved ' + path.split(/[\\/]/).pop();
  } catch (e) { out.textContent = String(e); }
  $('export').disabled = false;
});

// the CapraLink virtual device (if present) goes first, right after System default.
// `list` holds {id, name} entries, or names (an older computer's lists), where id = name.
function fill(sel, list, special, hint) {
  sel.innerHTML = '';
  const def = document.createElement('option');
  def.value = ''; def.textContent = 'System default';
  sel.appendChild(def);
  const none = document.createElement('option'); // turns this direction off
  none.value = 'none'; none.textContent = 'None';
  sel.appendChild(none);
  const devs = list.map((d) => (typeof d === 'string' ? { id: d, name: d } : d));
  for (const d of [...devs.filter((d) => d.id === special), ...devs.filter((d) => d.id !== special)]) {
    const o = document.createElement('option');
    o.value = d.id; o.dataset.name = d.name; o.textContent = d.id === special ? d.name + ' — ' + hint : d.name;
    sel.appendChild(o);
  }
  const refresh = document.createElement('option'); // re-checks for plugged/unplugged devices
  refresh.value = ''; refresh.dataset.refresh = '1'; refresh.textContent = 'Refresh devices…';
  sel.appendChild(refresh);
}

async function loadDevices() {
  const d = await invoke('devices');
  fill(els.input, d.inputs, 'CapraLink Output', 'apps on this computer play into it');
  // Windows: CapraLink Input is VB-Cable's "CABLE Input"; apps record from its other end
  const win = navigator.userAgent.includes('Windows');
  fill(els.output, d.outputs, 'CapraLink Input', win ? 'apps on this computer record from it — pick "CABLE Output" as the microphone in Discord' : 'apps on this computer record from it');
  $('cableNote').hidden = !win || d.outputs.some((o) => o.id === 'CapraLink Input');
}

// Size the window to the content, so it never needs a scroll bar; re-fit whenever the
// content grows or shrinks (devices appearing, Configure panel, errors).
// `chrome` is whatever the platform eats from the requested size (title bar, rounding):
// measured once after the first fit, then added to every request.
let fittedHeight = 0, chrome = 0;
async function fitWindow() {
  const h = Math.ceil(document.body.getBoundingClientRect().height) + 1;
  if (h === fittedHeight) return;
  fittedHeight = h;
  try {
    await invoke('fit', { height: h + chrome });
    await new Promise((r) => setTimeout(r, 100)); // let the resize land
    const short = h - window.innerHeight;
    if (short > 0) { chrome += short; await invoke('fit', { height: h + chrome }); }
  } catch (_) {}
}
new ResizeObserver(fitWindow).observe(document.body);

document.addEventListener('click', () => { if (menuFor) { menuFor = null; lastDevices = ''; render(); } });

// opens/closes the ⚙ popover; opening moves focus into it, closing with `focus` returns it to the gear
function setOpts(open, focus) {
  $('opts').hidden = !open;
  $('optsBtn').setAttribute('aria-expanded', open);
  if (open) $('service').focus(); else if (focus) $('optsBtn').focus();
}
$('optsBtn').addEventListener('click', (e) => { e.stopPropagation(); setOpts($('opts').hidden); });
$('opts').addEventListener('click', (e) => e.stopPropagation()); // clicks inside keep it open
document.addEventListener('click', () => setOpts(false));

// Shows this version in the corner; if there is a newer release, offers it instead. An install
// that can update itself (update_check) also gets an "Update now" button; otherwise (deb/rpm,
// macOS drivers changed, updater unavailable) GitHub's latest release tag is asked, once per window.
async function checkUpdate(current) {
  $('version').textContent = 'v' + current;
  let tag = '', inApp = false;
  try {
    const u = await invoke('update_check');
    if (u) { tag = u.version; inApp = true; }
  } catch (_) {}
  try {
    if (!tag) {
      const r = await fetch('https://api.github.com/repos/CapraAudio/CapraLink/releases/latest');
      tag = r.ok ? (await r.json()).tag_name : '';
    }
    if (!tag || !newer(tag, current)) return;
    // "New version available! [Update now] v0.2.0": the notice (opens the release page) left of this version
    const link = button('New version available!', 'link update', () => invoke('open_releases'));
    link.title = tag + ' is out: open the download page';
    const parts = [link];
    if (inApp) {
      const now = button('Update now', 'btn', async () => {
        now.disabled = true;
        now.textContent = 'Updating…';
        try { await invoke('update_install'); } catch (e) {
          actionError = String(e); // shown (and kept) by the next poll
          render();
          now.disabled = false;
          now.textContent = 'Update now';
        }
      });
      now.title = 'Install ' + tag + ' and restart CapraLink';
      parts.push(now);
    }
    $('version').replaceChildren(...parts, ' v' + current);
  } catch (_) {} // offline: just the version
}

// "v1.2.10" > "1.2.9"
function newer(a, b) {
  const p = (v) => v.replace(/^v/, '').split('.').map(Number);
  const [x, y] = [p(a), p(b)];
  for (let i = 0; i < 3; i++) if ((x[i] || 0) !== (y[i] || 0)) return (x[i] || 0) > (y[i] || 0);
  return false;
}

(async function init() {
  invoke('version').then(checkUpdate, () => {});
  // options must exist before saved selections can apply; the engine may still be starting
  for (;;) {
    try { await loadDevices(); break; } catch (e) { setError(String(e)); }
    await new Promise((r) => setTimeout(r, 2000));
  }
  await poll();
  setInterval(() => { if (!busy) poll(); }, 1000);
})();
