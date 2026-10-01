const { invoke } = window.__TAURI__.core;
const $ = (id) => document.getElementById(id);
const els = {
  input: $('input'), output: $('output'), channels: $('channels'), bitrate: $('bitrate'),
  bitrateVal: $('bitrateVal'), service: $('service'), remoteCfg: $('remoteCfg'), autoRc: $('autoRc'), music: $('music'), error: $('error'), status: $('status'), stats: $('stats'), devices: $('devices'),
  myNameRow: $('myNameRow'),
};

let channels = 1;
let effectiveMusic = false; // the link is in Music Mode (either side has it on)
let settingsLoaded = false;
let currentId;
let actionError = '';
let busy = false;
let pairingId = null;   // device whose PIN box is open
let pinDraft = '';
let editAddr = null;    // open "Edit address" box: {id, draft}
let lastDevices = '';
let devices = [];
let remote = null;      // open "Settings on <name>" panel: {id, name, inputs, outputs, draft}
let renaming = null;    // draft name while the "This device" name is being edited, else null
let lastNameSig = '';
let menuFor = null; // device whose ⋯ menu is open
let currentPin = '';
let pinShown = false;
let audioSig = '';      // the engine's audio settings as last loaded into the fields
let forgetting = null;  // device whose "Forget <name>?" confirmation is open

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
  }
  els.bitrate.disabled = on;
  els.bitrateVal.textContent = on ? MUSIC_RATE : els.bitrate.value + ' kbps';
}

// ---- actions ----
async function act(cmd, args) {
  busy = true; actionError = ''; lastDevices = ''; render();
  let ok = true;
  try { await invoke(cmd, args); } catch (e) { actionError = String(e); ok = false; }
  busy = false; lastDevices = '';
  await poll();
  return ok;
}

function saveSettings() {
  act('set_settings', { settings: {
    input: els.input.value || null, output: els.output.value || null,
    bitrate: Number(els.bitrate.value) * 1000, channels, service: els.service.checked,
    remote_config: els.remoteCfg.checked, music_mode: els.music.checked, auto_reconnect: els.autoRc.checked,
  }});
}

els.channels.addEventListener('click', (e) => {
  if (e.target.tagName !== 'BUTTON') return;
  setChannels(Number(e.target.dataset.v));
  saveSettings();
});
els.bitrate.addEventListener('input', showMusic);
els.music.addEventListener('change', () => { showMusic(); saveSettings(); });
els.bitrate.addEventListener('change', saveSettings);
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
    saveSettings();
  });
}
els.service.addEventListener('change', saveSettings);
els.remoteCfg.addEventListener('change', saveSettings);
els.autoRc.addEventListener('change', saveSettings);

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
  input.value = renaming;
  const cancel = () => { renaming = null; lastNameSig = ''; renderName(name); };
  const save = async () => { const n = renaming; if (await act('set_name', { name: n })) cancel(); };
  input.addEventListener('input', () => { renaming = input.value; });
  input.addEventListener('keydown', (e) => {
    if (e.key === 'Enter') save();
    if (e.key === 'Escape') cancel();
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
  try { await invoke('pair_ip', { addr, pin }); els.ipAddr.value = ''; els.ipPin.value = ''; }
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
      top.appendChild(button('⋯', 'more', (e) => { e.stopPropagation(); menuFor = menuFor === d.id ? null : d.id; lastDevices = ''; render(); }));
      if (menuFor === d.id) {
        const m = el('div', 'menu');
        const item = (t, f, cls) => { const b = button(t, cls || '', () => { menuFor = null; lastDevices = ''; f(); }); m.appendChild(b); };
        if (d.reachable) item('Configure', () => openRemote(d.id));
        item('Edit address', () => { editAddr = { id: d.id, draft: d.addr || '' }; render(); });
        item('Forget', () => { forgetting = d.id; render(); }, 'danger');
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
      bottom.appendChild(button('Pair', 'btn', () => { pairingId = pairingId === d.id ? null : d.id; pinDraft = ''; render(); }));
    } else if (d.connected) {
      bottom.appendChild(button('Disconnect', 'btn stop', () => act('disconnect')));
    } else if (d.reachable) {
      bottom.appendChild(button('Connect', 'btn', () => act('connect', { id: d.id })));
    }
    row.appendChild(bottom);
    list.appendChild(row);
    if (pairingId === d.id && !d.paired) {
      const box = el('div', 'pairbox');
      const pin = el('input');
      pin.maxLength = 6; pin.inputMode = 'numeric'; pin.placeholder = '6-digit PIN'; pin.value = pinDraft;
      const confirm = () => { const p = pin.value; pairingId = null; act('pair', { id: d.id, pin: p }); };
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
      input.placeholder = '192.168.1.20'; input.value = editAddr.draft;
      const cancel = () => { editAddr = null; render(); };
      const save = async () => { if (await act('set_peer_addr', { id: d.id, addr: input.value })) cancel(); };
      input.addEventListener('input', () => { editAddr.draft = input.value; });
      input.addEventListener('keydown', (e) => {
        if (e.key === 'Enter') save();
        if (e.key === 'Escape') cancel();
      });
      box.appendChild(input);
      box.appendChild(button('Save', 'btn', save));
      box.appendChild(button('Cancel', 'link', cancel));
      list.appendChild(box);
      setTimeout(() => input.focus());
    }
    if (forgetting === d.id && d.paired) {
      const box = el('div', 'pairbox');
      box.appendChild(el('span', null, 'Forget ' + d.name + '?'));
      box.appendChild(button('Forget', 'btn stop', () => { forgetting = null; act('forget', { id: d.id }); }));
      box.appendChild(button('Cancel', 'link', () => { forgetting = null; render(); }));
      list.appendChild(box);
    }
    if (remote && remote.id === d.id && d.paired) list.appendChild(remotePanel());
  }
}

// ---- remote configuration (edits live in remote.draft, so a rebuild keeps them) ----
async function openRemote(id) {
  if (remote && remote.id === id) { remote = null; lastDevices = ''; render(); return; }
  busy = true; actionError = ''; remote = null; lastDevices = ''; render();
  try { const c = await invoke('remote_get', { id }); remote = { id, ...c, draft: { ...c.settings }, nameDraft: c.name }; }
  catch (e) { actionError = String(e); }
  busy = false; lastDevices = '';
  await poll();
}

function remotePanel() {
  const r = remote, s = r.draft;
  const box = el('div', 'remote');
  box.appendChild(el('h2', null, 'Settings on ' + r.name));
  box.appendChild(el('label', null, 'Name'));
  const nameInput = el('input');
  nameInput.value = r.nameDraft;
  nameInput.addEventListener('input', () => { r.nameDraft = nameInput.value; });
  box.appendChild(nameInput);
  const pick = (label, list, special, hint, key) => {
    box.appendChild(el('label', null, label));
    const sel = el('select');
    fill(sel, list, special, hint);
    setSel(sel, s[key]);
    sel.addEventListener('change', async () => {
      if (!isRefresh(sel)) { s[key] = sel.value || null; return; }
      try { const c = await invoke('remote_get', { id: r.id }); r.inputs = c.inputs; r.outputs = c.outputs; }
      catch (e) { actionError = String(e); }
      lastDevices = ''; render(); // rebuilds the panel from the draft, so choices stay
    });
    box.appendChild(sel);
  };
  box.appendChild(el('h2', 'sub', 'Sending'));
  pick('Send from', r.inputs, 'CapraLink Output', 'apps on that computer play into it', 'input');
  box.appendChild(el('label', null, 'Channels'));
  const seg = el('div', 'seg');
  const rate = el('input');
  const val = el('div', 'bitrate-val');
  const sync = () => {
    const on = !!s.music_mode;
    for (const c of seg.children) { c.disabled = on; c.classList.toggle('active', Number(c.dataset.v) === (on ? 2 : s.channels)); }
    rate.disabled = on;
    val.textContent = on ? MUSIC_RATE : rate.value + ' kbps';
  };
  for (const [v, t] of [[1, 'Mono'], [2, 'Stereo']]) {
    const b = el('button', null, t);
    b.dataset.v = v;
    b.addEventListener('click', () => { s.channels = v; sync(); });
    seg.appendChild(b);
  }
  box.appendChild(seg);
  box.appendChild(el('label', null, 'Bitrate'));
  rate.type = 'range'; rate.min = 8; rate.max = 96; rate.step = 8; rate.value = Math.round(s.bitrate / 1000);
  rate.addEventListener('input', () => { s.bitrate = Number(rate.value) * 1000; sync(); });
  box.appendChild(rate);
  box.appendChild(val);
  box.appendChild(el('h2', 'sub', 'Receiving'));
  pick('Play to', r.outputs, 'CapraLink Input', 'apps on that computer record from it', 'output');
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

// keeps a saved device selectable even while it is unplugged
function setSel(sel, v) {
  if (v && ![...sel.options].some((o) => o.value === v && !o.dataset.refresh)) {
    const o = document.createElement('option');
    o.value = v; o.textContent = v + ' (not found)';
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
function setBars(inP, outP) {
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
  try { st = await invoke('state'); } catch (e) { els.error.textContent = String(e); return; }
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
  effectiveMusic = !!(st.stats && st.stats.music);
  showMusic();
  devices = st.devices;
  render();
  const peer = devices.find((d) => d.connected);
  els.status.textContent = peer ? 'Streaming' : 'Idle';
  els.status.classList.toggle('on', !!peer);
  const s = st.stats;
  if (s) {
    els.stats.textContent =
      `sent ${s.sent} · recv ${s.received} · lost ${s.lost} · fec ${s.fec_recovered} · underruns ${s.underruns} · buf ${s.buffer_ms.toFixed(0)}/${s.target_ms.toFixed(0)}ms · gap tx ${s.tx_gap_ms.toFixed(0)} rx ${s.rx_gap_ms.toFixed(0)}ms · ${Math.round(s.bitrate / 1000)} kbps · cx ${s.complexity}`;
  } else {
    els.stats.textContent = '';
  }
  els.error.textContent = [actionError || st.error, st.virtual_error].filter(Boolean).join(' · ');
}

// the CapraLink virtual device (if present) goes first, right after System default
function fill(sel, list, special, hint) {
  sel.innerHTML = '';
  const def = document.createElement('option');
  def.value = ''; def.textContent = 'System default';
  sel.appendChild(def);
  const none = document.createElement('option'); // turns this direction off
  none.value = 'none'; none.textContent = 'None';
  sel.appendChild(none);
  const names = list.includes(special) ? [special, ...list.filter((n) => n !== special)] : list;
  for (const name of names) {
    const o = document.createElement('option');
    o.value = name; o.textContent = name === special ? name + ' — ' + hint : name;
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
  $('cableNote').hidden = !win || d.outputs.includes('CapraLink Input');
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

$('optsBtn').addEventListener('click', (e) => { e.stopPropagation(); $('opts').hidden = !$('opts').hidden; });
$('opts').addEventListener('click', (e) => e.stopPropagation()); // clicks inside keep it open
document.addEventListener('click', () => { $('opts').hidden = true; });

(async function init() {
  // options must exist before saved selections can apply; the engine may still be starting
  for (;;) {
    try { await loadDevices(); break; } catch (e) { els.error.textContent = String(e); }
    await new Promise((r) => setTimeout(r, 2000));
  }
  await poll();
  setInterval(() => { if (!busy) poll(); }, 1000);
})();
